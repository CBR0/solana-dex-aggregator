#![allow(deprecated)] // get_program_accounts_with_config — successor returns UI-encoded data

//! Async pool loading from Solana RPC for all supported DEXs.
//!
//! Strategy: for each DEX, query pools paired with hub mints (WSOL, USDC, USDT)
//! using memcmp filters. If a full fetch fails (response too large), fall back
//! to two-phase: discover addresses with dataSlice, then batch-fetch full data.

use std::sync::Arc;
use std::time::Duration;

use borsh::BorshDeserialize;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_sdk::account::Account;

use meteora_damm::{
    derive_token_vault_address, MeteoraDAMMPool,
    MeteoraDAMMV2Pool, METEORA_DYNAMIC_AMM, METEORA_DYNAMIC_AMM_V2,
};
use meteora_dlmm::{MeteoraDLMMPool, METEORA_DYNAMIC_LMM};
use bonk::{parse_pool_state as parse_bonk_pool, BONK_LAUNCHPAD_PROGRAM};
use meteora_dbc::{
    parse_pool_config as parse_dbc_config, parse_virtual_pool, quote::is_tradeable as dbc_tradeable,
    DISC_VIRTUAL_POOL, METEORA_DBC_PROGRAM,
};
use pumpfun_amm::{
    derive_bonding_curve_pda, parse_bonding_curve, PumpfunAmmPool, PumpfunBondingCurvePool,
    PUMPFUN_AMM_PROGRAM,
};
use raydium_amm_v4::{RaydiumAMMV4, RAYDIUM_LIQUIDITY_POOL_V4};
use orca_whirlpool::{WhirlpoolPool, DISC_WHIRLPOOL, ORCA_WHIRLPOOL_PROGRAM, WHIRLPOOL_LEN};
use raydium_clmm::{RaydiumCLMMPool, RAYDIUM_CLMM};
use solroute_core::GenericError;

use crate::cache::CachedPool;
use crate::pool_index::PoolIndex;
use crate::types::{LoadPhase, LoadProgress, PoolEntry};

pub type ProgressCallback = Box<dyn Fn(LoadProgress) + Send + Sync>;

const BALANCE_BATCH_SIZE: usize = 100;
const BALANCE_CONCURRENCY: usize = 20;
const DEFAULT_MAX_POOLS_PER_DEX: usize = usize::MAX;

// Anchor discriminators (SHA256("account:<Name>")[0..8])
const DISC_POOL_STATE: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70]; // Raydium CLMM
const DISC_POOL: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188]; // DAMM V1/V2, Pumpfun
const DISC_LB_PAIR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13]; // Meteora DLMM

struct DexDescriptor {
    name: &'static str,
    program_id: &'static str,
    /// Account sizes to query. Empty = no dataSize filter (discriminator only).
    data_sizes: &'static [u64],
    /// Anchor discriminator (first 8 bytes). None for non-Anchor (Raydium V4).
    discriminator: Option<[u8; 8]>,
}

const DESCRIPTORS: [DexDescriptor; 9] = [
    DexDescriptor {
        name: "Raydium AMM V4",
        program_id: RAYDIUM_LIQUIDITY_POOL_V4,
        data_sizes: &[752],
        discriminator: None,
    },
    DexDescriptor {
        name: "Raydium CLMM",
        program_id: RAYDIUM_CLMM,
        data_sizes: &[1544],
        discriminator: Some(DISC_POOL_STATE),
    },
    DexDescriptor {
        name: "Meteora DAMM V1",
        program_id: METEORA_DYNAMIC_AMM,
        data_sizes: &[944],
        discriminator: Some(DISC_POOL),
    },
    DexDescriptor {
        name: "Meteora DAMM V2",
        program_id: METEORA_DYNAMIC_AMM_V2,
        data_sizes: &[1112],
        discriminator: Some(DISC_POOL),
    },
    DexDescriptor {
        name: "Meteora DLMM",
        program_id: METEORA_DYNAMIC_LMM,
        data_sizes: &[904],
        discriminator: Some(DISC_LB_PAIR),
    },
    DexDescriptor {
        name: "Pumpfun AMM",
        program_id: PUMPFUN_AMM_PROGRAM,
        data_sizes: &[],
        discriminator: Some(DISC_POOL),
    },
    DexDescriptor {
        name: "Orca Whirlpool",
        program_id: ORCA_WHIRLPOOL_PROGRAM,
        data_sizes: &[WHIRLPOOL_LEN],
        discriminator: Some(DISC_WHIRLPOOL),
    },
    // bonk.fun / Raydium LaunchLab. PoolState shares CLMM's discriminator but
    // lives under a different program — disc-only (sizes vary by curve type).
    DexDescriptor {
        name: "Bonk",
        program_id: BONK_LAUNCHPAD_PROGRAM,
        data_sizes: &[],
        discriminator: Some(DISC_POOL_STATE),
    },
    // Meteora Dynamic Bonding Curve. VirtualPool is 424 bytes; disc-only filter
    // (a transfer-hook pool variant shares the size but not the discriminator).
    DexDescriptor {
        name: "Meteora DBC",
        program_id: METEORA_DBC_PROGRAM,
        data_sizes: &[],
        discriminator: Some(DISC_VIRTUAL_POOL),
    },
];

pub struct PoolLoader {
    rpc: Arc<RpcClient>,
    max_pools_per_dex: usize,
}

impl PoolLoader {
    pub fn new(rpc_url: &str) -> Self {
        let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
            rpc_url.to_string(),
            Duration::from_secs(300),
            CommitmentConfig::confirmed(),
        ));
        Self { rpc, max_pools_per_dex: DEFAULT_MAX_POOLS_PER_DEX }
    }

    /// Override the per-DEX pool cap. Set to `usize::MAX` for no limit.
    pub fn with_max_pools(mut self, max: usize) -> Self {
        self.max_pools_per_dex = max;
        self
    }

    pub async fn load_all(
        &self,
        progress_cb: &ProgressCallback,
    ) -> Result<PoolIndex, GenericError> {
        let mut index = PoolIndex::new();

        let (r0, r1, r2, r3, r4, r5, r6, r7, r8) = tokio::join!(
            self.load_dex(&DESCRIPTORS[0], progress_cb),
            self.load_dex(&DESCRIPTORS[1], progress_cb),
            self.load_dex(&DESCRIPTORS[2], progress_cb),
            self.load_dex(&DESCRIPTORS[3], progress_cb),
            self.load_dex(&DESCRIPTORS[4], progress_cb),
            self.load_dex(&DESCRIPTORS[5], progress_cb),
            self.load_dex(&DESCRIPTORS[6], progress_cb),
            self.load_dex(&DESCRIPTORS[7], progress_cb),
            self.load_dex(&DESCRIPTORS[8], progress_cb),
        );

        for result in [r0, r1, r2, r3, r4, r5, r6, r7, r8] {
            if let Ok(pools) = result {
                for (addr, entry) in pools {
                    let _ = index.add_pool(addr, entry);
                }
            }
        }

        Ok(index)
    }

    async fn load_dex(
        &self,
        desc: &DexDescriptor,
        cb: &ProgressCallback,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = desc.name;
        cb(progress(dex, LoadPhase::FetchingPools));

        let program = Pubkey::from_str_const(desc.program_id);
        let mut raw_accounts: Vec<(Pubkey, Account)> = Vec::new();

        // One query per data_size. Filters: discriminator + dataSize only (no mint filter).
        for &data_size in desc.data_sizes.iter().chain(
            if desc.data_sizes.is_empty() { [0u64].iter() } else { [].iter() },
        ) {
            let mut filters = Vec::new();
            if data_size > 0 {
                filters.push(RpcFilterType::DataSize(data_size));
            }
            if let Some(disc) = &desc.discriminator {
                filters.push(RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, disc.to_vec())));
            }

            let fetched = match self.fetch_filtered(&program, filters.clone()).await {
                Ok(accounts) => accounts,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("excluded from account secondary indexes") {
                        cb(progress(dex, LoadPhase::Error(
                            "Program excluded from RPC indexes".into(),
                        )));
                        return Ok(vec![]);
                    }
                    // Response too large — try two-phase (dataSlice discovery
                    // + getMultipleAccounts). If phase-1 also fails because the
                    // RPC demands pagination (Helius on cpamd…), page through
                    // getProgramAccountsV2 as a last resort.
                    match self.two_phase_fetch(&program, filters.clone(), dex, cb).await {
                        Ok(accounts) => accounts,
                        Err(_) => match self.fetch_paginated_v2(&program, filters, dex, cb).await {
                            Ok(accounts) => accounts,
                            Err(_) => continue,
                        },
                    }
                }
            };

            raw_accounts.extend(fetched);
        }

        if raw_accounts.is_empty() {
            cb(progress(dex, LoadPhase::Complete { pool_count: 0 }));
            return Ok(vec![]);
        }

        if raw_accounts.len() > self.max_pools_per_dex {
            raw_accounts.truncate(self.max_pools_per_dex);
        }

        let total = raw_accounts.len();
        cb(progress(dex, LoadPhase::Deserializing { done: 0, total }));

        let entries = self.build_entries(desc, raw_accounts, cb).await?;
        cb(progress(dex, LoadPhase::Complete { pool_count: entries.len() }));
        Ok(entries)
    }

    /// Index of a DEX descriptor by its display name (e.g. "Raydium CLMM").
    pub fn descriptor_index(name: &str) -> Option<usize> {
        DESCRIPTORS.iter().position(|d| d.name == name)
    }

    /// Build pool entries for a bounded, explicit set of pool addresses instead
    /// of scanning the whole program. Fetches each pool's full account via
    /// `getMultipleAccounts`, then reuses the per-DEX deserialization and
    /// vault-balance fetch. Intended for cheap test sampling.
    pub async fn build_sample_from_addresses(
        &self,
        dex_index: usize,
        addresses: &[Pubkey],
        cb: &ProgressCallback,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let desc = &DESCRIPTORS[dex_index];
        let mut raw: Vec<(Pubkey, Account)> = Vec::with_capacity(addresses.len());

        let chunks: Vec<&[Pubkey]> = addresses.chunks(BALANCE_BATCH_SIZE).collect();
        for window in chunks.chunks(BALANCE_CONCURRENCY) {
            let futures: Vec<_> = window
                .iter()
                .map(|chunk| self.rpc.get_multiple_accounts(chunk))
                .collect();
            let results = futures::future::join_all(futures).await;
            for (chunk, result) in window.iter().zip(results) {
                if let Ok(accounts) = result {
                    for (pubkey, maybe_account) in chunk.iter().zip(accounts) {
                        if let Some(account) = maybe_account {
                            raw.push((*pubkey, account));
                        }
                    }
                }
            }
        }

        self.build_entries(desc, raw, cb).await
    }

    async fn build_entries(
        &self,
        desc: &DexDescriptor,
        raw_accounts: Vec<(Pubkey, Account)>,
        cb: &ProgressCallback,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        match desc.name {
            "Raydium AMM V4" => self.build_raydium_v4(raw_accounts, cb).await,
            "Raydium CLMM" => self.build_raydium_clmm(raw_accounts, cb).await,
            "Meteora DAMM V1" => self.build_meteora_damm_v1(raw_accounts, cb).await,
            "Meteora DAMM V2" => self.build_meteora_damm_v2(raw_accounts, cb).await,
            "Meteora DLMM" => self.build_meteora_dlmm(raw_accounts, cb).await,
            "Pumpfun AMM" => self.build_pumpfun(raw_accounts, cb).await,
            "Orca Whirlpool" => self.build_orca_whirlpool(raw_accounts, cb).await,
            "Bonk" => self.build_bonk(raw_accounts, cb).await,
            "Meteora DBC" => self.build_dbc(raw_accounts, cb).await,
            _ => Err(format!("Unknown DEX: {}", desc.name).into()),
        }
    }

    // =========================================================================
    // Per-DEX builders (identical pattern: deserialize, fetch vaults, wrap)
    // =========================================================================

    async fn build_raydium_v4(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Raydium AMM V4";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = RaydiumAMMV4::try_from_slice(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| [p.base_vault, p.quote_vault]).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::RaydiumV4 { addr, pool, quote_bal: balances[i*2+1], base_bal: balances[i*2] }.into_pool_entry()
        }).collect())
    }

    async fn build_raydium_clmm(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Raydium CLMM";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = deser_anchor::<RaydiumCLMMPool>(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| [p.token_vault_0, p.token_vault_1]).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::RaydiumClmm { addr, pool, v0_bal: balances[i*2], v1_bal: balances[i*2+1] }.into_pool_entry()
        }).collect())
    }

    async fn build_meteora_damm_v1(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Meteora DAMM V1";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = deser_anchor::<MeteoraDAMMPool>(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| {
            [derive_token_vault_address(p.a_vault).0, derive_token_vault_address(p.b_vault).0]
        }).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::MeteoraDAMMV1 { addr, pool, a_bal: balances[i*2], b_bal: balances[i*2+1] }.into_pool_entry()
        }).collect())
    }

    async fn build_meteora_damm_v2(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Meteora DAMM V2";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = deser_anchor::<MeteoraDAMMV2Pool>(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| [p.token_a_vault, p.token_b_vault]).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::MeteoraDAMMV2 { addr, pool, a_bal: balances[i*2], b_bal: balances[i*2+1] }.into_pool_entry()
        }).collect())
    }

    async fn build_meteora_dlmm(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Meteora DLMM";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = deser_anchor::<MeteoraDLMMPool>(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| [p.reserve_x, p.reserve_y]).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::MeteoraDLMM { addr, pool, rx_bal: balances[i*2], ry_bal: balances[i*2+1] }.into_pool_entry()
        }).collect())
    }

    async fn build_orca_whirlpool(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Orca Whirlpool";
        let mut pools = Vec::new();
        for (pubkey, account) in &accounts {
            if let Ok(pool) = deser_anchor::<WhirlpoolPool>(&account.data) {
                pools.push((pubkey.to_string(), pool));
            }
        }
        let vault_keys: Vec<Pubkey> = pools.iter().flat_map(|(_, p)| [p.token_vault_a, p.token_vault_b]).collect();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: pools.len() }));
        let balances = self.batch_fetch_balances(&vault_keys, dex, cb).await?;
        Ok(pools.into_iter().enumerate().map(|(i, (addr, pool))| {
            CachedPool::OrcaWhirlpool { addr, pool, a_bal: balances[i*2], b_bal: balances[i*2+1] }.into_pool_entry()
        }).collect())
    }

    async fn build_pumpfun(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Pumpfun AMM";
        let total = accounts.len();
        let mut entries = Vec::new();
        for (i, (pubkey, account)) in accounts.iter().enumerate() {
            if let Ok(pool) = deser_anchor::<PumpfunAmmPool>(&account.data) {
                entries.push(CachedPool::PumpfunAmm { addr: pubkey.to_string(), pool }.into_pool_entry());
            }
            if (i + 1) % 5000 == 0 || i + 1 == total {
                cb(progress(dex, LoadPhase::BuildingMarkets { done: i + 1, total }));
            }
        }
        Ok(entries)
    }

    async fn build_bonk(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Bonk";
        let total = accounts.len();
        let mut entries = Vec::new();
        for (i, (pubkey, account)) in accounts.iter().enumerate() {
            if let Some(pool) = parse_bonk_pool(&account.data) {
                // Only fundraising (on-curve) pools are tradeable here; migrated
                // ones live on the AMM.
                if pool.status == bonk::POOL_STATUS_FUND {
                    entries.push(
                        CachedPool::Bonk { addr: pubkey.to_string(), pool }.into_pool_entry(),
                    );
                }
            }
            if (i + 1) % 5000 == 0 || i + 1 == total {
                cb(progress(dex, LoadPhase::BuildingMarkets { done: i + 1, total }));
            }
        }
        Ok(entries)
    }

    async fn build_dbc(&self, accounts: Vec<(Pubkey, Account)>, cb: &ProgressCallback) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Meteora DBC";
        // Parse pools, pre-filter migrated (full tradeable check needs the config).
        let mut pools: Vec<(String, meteora_dbc::VirtualPool)> = Vec::new();
        for (pubkey, account) in &accounts {
            if let Some(p) = parse_virtual_pool(&account.data) {
                if p.is_migrated == 0 && p.migration_progress == 0 {
                    pools.push((pubkey.to_string(), p));
                }
            }
        }
        // Configs are shared across many pools — fetch the unique set once.
        let mut cfg_keys: Vec<Pubkey> = pools.iter().map(|(_, p)| p.config).collect();
        cfg_keys.sort_unstable_by_key(|k| k.to_bytes());
        cfg_keys.dedup();
        cb(progress(dex, LoadPhase::FetchingBalances { done: 0, total: cfg_keys.len() }));
        let mut configs: std::collections::HashMap<Pubkey, meteora_dbc::PoolConfig> =
            std::collections::HashMap::new();
        for chunk in cfg_keys.chunks(BALANCE_BATCH_SIZE) {
            if let Ok(accs) = self.rpc.get_multiple_accounts(chunk).await {
                for (k, maybe) in chunk.iter().zip(accs) {
                    if let Some(a) = maybe.as_ref() {
                        if let Some(c) = parse_dbc_config(&a.data) {
                            configs.insert(*k, c);
                        }
                    }
                }
            }
        }
        // Clock for the fee scheduler: slot for activation_type Slot(0), unix
        // time for Timestamp(1).
        let current_slot = self.rpc.get_slot().await.unwrap_or(0);
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Keep only on-curve tradeable pools (needs the config's migration threshold).
        let mut entries = Vec::new();
        for (addr, pool) in pools {
            if let Some(config) = configs.get(&pool.config) {
                if dbc_tradeable(&pool, config) {
                    let current_point = if config.activation_type == 1 { now_unix } else { current_slot };
                    entries.push(crate::cache::dbc_pool_entry(addr, pool, config.clone(), current_point));
                }
            }
        }
        cb(progress(dex, LoadPhase::Complete { pool_count: entries.len() }));
        Ok(entries)
    }

    /// Load pump.fun bonding curves for a bounded, explicit set of token mints.
    ///
    /// The BondingCurve account carries no base mint and its `["bonding-curve",
    /// mint]` PDA can't be reversed, so curves cannot be enumerated program-wide
    /// (and `getProgramAccounts` on the pump program is blocked on stock RPCs).
    /// This resolves each supplied mint's curve PDA, fetches it via
    /// `getMultipleAccounts`, parses, and keeps only still-trading curves
    /// (`complete == false`, non-empty SOL reserves). Production discovery of new
    /// curves is via pump `create`/trade event streaming.
    pub async fn load_bonding_curves_for_mints(
        &self,
        mints: &[Pubkey],
        cb: &ProgressCallback,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = "Pumpfun BC";
        let curves: Vec<Pubkey> = mints.iter().map(derive_bonding_curve_pda).collect();
        let total = mints.len();
        cb(progress(dex, LoadPhase::FetchingPools));

        let mut entries = Vec::new();
        let mut done = 0usize;
        for (mchunk, cchunk) in mints.chunks(BALANCE_BATCH_SIZE).zip(curves.chunks(BALANCE_BATCH_SIZE)) {
            let accounts = self.rpc.get_multiple_accounts(cchunk).await.unwrap_or_default();
            for ((mint, curve), maybe) in mchunk.iter().zip(cchunk).zip(accounts) {
                if let Some(account) = maybe {
                    if let Some(bc) = parse_bonding_curve(&account.data) {
                        // Skip migrated (complete) and dead (empty) curves.
                        if bc.complete || bc.real_sol_reserves == 0 {
                            continue;
                        }
                        let pool = PumpfunBondingCurvePool {
                            mint: *mint,
                            bonding_curve: *curve,
                            curve: bc,
                        };
                        entries.push(
                            CachedPool::PumpfunBondingCurve { addr: curve.to_string(), pool }
                                .into_pool_entry(),
                        );
                    }
                }
            }
            done += mchunk.len();
            cb(progress(dex, LoadPhase::BuildingMarkets { done, total }));
        }
        cb(progress(dex, LoadPhase::Complete { pool_count: entries.len() }));
        Ok(entries)
    }

    // =========================================================================
    // RPC helpers
    // =========================================================================

    async fn fetch_filtered(
        &self,
        program_id: &Pubkey,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>, GenericError> {
        let config = RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(self.rpc.get_program_accounts_with_config(program_id, config).await?)
    }

    /// Fallback: discover addresses via dataSlice, then batch-fetch full data.
    async fn two_phase_fetch(
        &self,
        program_id: &Pubkey,
        filters: Vec<RpcFilterType>,
        dex: &str,
        cb: &ProgressCallback,
    ) -> Result<Vec<(Pubkey, Account)>, GenericError> {
        let config = RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                data_slice: Some(solana_account_decoder_client_types::UiDataSliceConfig {
                    offset: 0,
                    length: 8,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let addresses: Vec<Pubkey> = self.rpc
            .get_program_accounts_with_config(program_id, config)
            .await?
            .into_iter()
            .map(|(pubkey, _)| pubkey)
            .collect();

        let mut results: Vec<(Pubkey, Account)> = Vec::with_capacity(addresses.len());
        let chunks: Vec<&[Pubkey]> = addresses.chunks(BALANCE_BATCH_SIZE).collect();

        for window in chunks.chunks(BALANCE_CONCURRENCY) {
            let futures: Vec<_> = window.iter()
                .map(|chunk| self.rpc.get_multiple_accounts(chunk))
                .collect();
            let batch_results = futures::future::join_all(futures).await;
            for (chunk, result) in window.iter().zip(batch_results) {
                if let Ok(accounts) = result {
                    for (pubkey, maybe_account) in chunk.iter().zip(accounts) {
                        if let Some(account) = maybe_account {
                            results.push((*pubkey, account));
                        }
                    }
                }
            }
            cb(progress(dex, LoadPhase::FetchingPools));
        }

        Ok(results)
    }

    /// Fallback for programs whose account index the RPC won't serve via a
    /// single `getProgramAccounts` (Helius returns "account index service
    /// overloaded ... use getProgramAccountsV2 with pagination"). Pages through
    /// `getProgramAccountsV2` until the cursor is exhausted. Meteora DAMM V2
    /// (`cpamd…`) hits this on Helius while the other DEX programs do not.
    async fn fetch_paginated_v2(
        &self,
        program_id: &Pubkey,
        filters: Vec<RpcFilterType>,
        dex: &str,
        cb: &ProgressCallback,
    ) -> Result<Vec<(Pubkey, Account)>, GenericError> {
        use solana_rpc_client_api::request::RpcRequest;

        #[derive(serde::Deserialize)]
        struct V2Page {
            accounts: Vec<solana_rpc_client_api::response::RpcKeyedAccount>,
            #[serde(rename = "paginationKey")]
            pagination_key: Option<String>,
        }

        let filters_json = serde_json::to_value(&filters)?;
        let mut results: Vec<(Pubkey, Account)> = Vec::new();
        let mut pagination_key: Option<String> = None;

        loop {
            let mut cfg = serde_json::json!({
                "encoding": "base64",
                "commitment": "confirmed",
                "filters": filters_json,
                "limit": 1000,
            });
            if let Some(key) = &pagination_key {
                cfg["paginationKey"] = serde_json::Value::String(key.clone());
            }
            let params = serde_json::json!([program_id.to_string(), cfg]);

            let page: V2Page = self
                .rpc
                .send(RpcRequest::Custom { method: "getProgramAccountsV2" }, params)
                .await?;

            for keyed in &page.accounts {
                let pubkey = match keyed.pubkey.parse::<Pubkey>() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if let Some(account) = keyed.account.decode::<Account>() {
                    results.push((pubkey, account));
                }
            }
            cb(progress(dex, LoadPhase::FetchingPools));

            match page.pagination_key {
                Some(key) => pagination_key = Some(key),
                None => break,
            }
        }

        Ok(results)
    }

    async fn batch_fetch_balances(
        &self,
        keys: &[Pubkey],
        dex: &str,
        cb: &ProgressCallback,
    ) -> Result<Vec<u64>, GenericError> {
        let mut balances = vec![0u64; keys.len()];
        let total_vaults = keys.len();
        let chunks: Vec<(usize, &[Pubkey])> = keys.chunks(BALANCE_BATCH_SIZE).enumerate().collect();
        let done_counter = std::sync::atomic::AtomicUsize::new(0);

        for window in chunks.chunks(BALANCE_CONCURRENCY) {
            let futures: Vec<_> = window.iter()
                .map(|(_, chunk)| self.rpc.get_multiple_accounts(chunk))
                .collect();
            let results = futures::future::join_all(futures).await;

            for ((chunk_idx, _), result) in window.iter().zip(results) {
                if let Ok(accounts) = result {
                    let base = chunk_idx * BALANCE_BATCH_SIZE;
                    for (j, maybe_account) in accounts.into_iter().enumerate() {
                        if let Some(account) = maybe_account {
                            balances[base + j] = read_token_balance(&account.data);
                        }
                    }
                }
            }

            let done = done_counter.fetch_add(window.len(), std::sync::atomic::Ordering::Relaxed) + window.len();
            let pools_done = (done * BALANCE_BATCH_SIZE).min(total_vaults) / 2;
            cb(progress(dex, LoadPhase::FetchingBalances { done: pools_done, total: total_vaults / 2 }));
        }

        Ok(balances)
    }
}

fn read_token_balance(data: &[u8]) -> u64 {
    if data.len() < 72 { return 0; }
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}

/// Deserialize an Anchor account, skipping 8-byte discriminator.
/// Tolerates trailing bytes (common on Solana).
fn deser_anchor<T: BorshDeserialize>(data: &[u8]) -> Result<T, GenericError> {
    if data.len() < 8 { return Err("Account data too short".into()); }
    let mut slice = &data[8..];
    T::deserialize(&mut slice).map_err(|e| e.into())
}

fn progress(dex: &str, phase: LoadPhase) -> LoadProgress {
    LoadProgress { dex_name: dex.to_string(), phase }
}
