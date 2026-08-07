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
use pumpfun_amm::{PumpfunAmmPool, PUMPFUN_AMM_PROGRAM};
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

/// Proxy de liquidez embutido no pool account: dois dataSlice GPAs baratos
/// (um para os mints, um para o campo de liquidez/reservas) permitem rankear
/// todos os pools de um DEX e manter os top-N mais líquidos em USD sem pagar
/// o custo de um full load (sem fetch dos vaults).
struct LiquidityProxy {
    /// dataSlice dos 2 mints (64 bytes: mint_0 .. mint_1).
    mints: (usize, usize),
    /// dataSlice do campo de score: CLMM/Whirlpool = L u128 + sqrt_price u128
    /// (32B); DAMM V2 = token_a_amount u64 + token_b_amount u64 (16B).
    data: (usize, usize),
    /// Extrai o score (USD estimado) de (mints_bytes, data_bytes).
    score: fn(&[u8], &[u8]) -> f64,
}

// Offsets dos campos nos pool accounts, verificados por teste contra o layout
// borsh real + accounts de mainnet (ver `crates/aggregator/tests/offset_check.rs`).
// Descontam o discriminator de 8 bytes (o slice é relativo ao account data).
const CLMM_MINTS_OFFSET: usize = 73; // token_mint_0..1 (struct 65 + 8)
const CLMM_LIQUIDITY_OFFSET: usize = 237; // liquidity u128 (struct 229 + 8)
const WHIRLPOOL_MINTS_OFFSET: usize = 101; // token_mint_a (struct 93 + 8)
const WHIRLPOOL_LIQUIDITY_OFFSET: usize = 49; // liquidity u128 (struct 41 + 8)
const DAMMV2_MINTS_OFFSET: usize = 168; // token_a_mint..b_mint (struct 160 + 8)
const DAMMV2_AMOUNTS_OFFSET: usize = 680; // token_a_amount u64 + token_b_amount u64

/// Preço aproximado em USD + decimals de tokens "hub" (constantes — servem só
/// para ranking relativo, não são oráculos).
fn hub_usd(mint: &[u8]) -> Option<(f64, u8)> {
    use std::str::FromStr;
    const HUBS: &[(&str, f64, u8)] = &[
        ("So11111111111111111111111111111111111111112", 150.0, 9), // WSOL
        ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 1.0, 6),  // USDC
        ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", 1.0, 6),  // USDT
        ("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", 175.0, 9), // JitoSOL
        ("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So", 165.0, 9), // mSOL
        ("bSo13r4TkiE4KumL71LsHTPpL2euBYLPxVMh7aCVueG", 160.0, 9), // bSOL
        ("jupSoLaHXQiZZTSpEWRVWmKVpsaQoptjT5z6q1ULjxa", 150.0, 9), // jupSOL
        ("DSRnp2rGZgCkCw7jMYZ7bZkBYdK7JZaFgzYg5MbcGayL", 1.0, 9),  // USDe
        ("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZdbHsDbw2Gf", 1.0, 6),  // PYUSD
        ("zebeczgi5fSEtbpfQ5Zbr7njtGVfbwF1BGuTk1UTpU", 1.1, 6),    // EURC
    ];
    HUBS.iter()
        .find(|(s, _, _)| Pubkey::from_str(s).ok().as_ref().map(|p| p.as_ref()) == Some(mint))
        .map(|(_, p, d)| (*p, *d))
}

/// Score USD estimado para pools de liquidez concentrada (CLMM, Whirlpool):
/// TVL virtual = L × price_hub × (1/sqrt(P) + sqrt(P)), com P da convenção
/// sqrt_price embutida (direção-agnóstico — ver teste). Sem hub: fallback L.
fn score_liquidity_usd(mints: &[u8], data: &[u8]) -> f64 {
    if mints.len() < 64 || data.len() < 32 {
        return 0.0;
    }
    let l = u128::from_le_bytes(data[0..16].try_into().unwrap());
    let sq_raw = u128::from_le_bytes(data[16..32].try_into().unwrap());
    let price_hub = hub_usd(&mints[0..32]).or_else(|| hub_usd(&mints[32..64]));
    match price_hub {
        Some((p, _)) => {
            let sq = sq_raw as f64 / (1u128 << 64) as f64;
            if sq > 0.0 {
                l as f64 * p * (1.0 / sq + sq)
            } else {
                l as f64 * p
            }
        }
        None => l as f64,
    }
}

/// Score USD para DAMM V2: token_a_amount/token_b_amount são as reservas
/// cacheadas reais embutidas no pool account; com mints + preço hub vira USD.
fn score_amounts_usd(mints: &[u8], data: &[u8]) -> f64 {
    if mints.len() < 64 || data.len() < 16 {
        return 0.0;
    }
    let a = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let b = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let usd = |amt: u64, m: &[u8]| {
        hub_usd(m)
            .map(|(p, d)| amt as f64 * p / 10f64.powi(d as i32))
            .unwrap_or(0.0)
    };
    usd(a, &mints[0..32]) + usd(b, &mints[32..64])
}

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
    /// Proxy de liquidez embutido no pool account. None = sem proxy (DEXs de
    /// balanço em vaults externos: Raydium V4, DAMM V1, DLMM, Pumpfun) — nesses,
    /// o modo top-N cai no early-stop arbitrário.
    liquidity_proxy: Option<LiquidityProxy>,
}

const DESCRIPTORS: [DexDescriptor; 7] = [
    DexDescriptor {
        name: "Raydium AMM V4",
        program_id: RAYDIUM_LIQUIDITY_POOL_V4,
        data_sizes: &[752],
        discriminator: None,
        liquidity_proxy: None, // reservas nos vaults (sem campo embutido)
    },
    DexDescriptor {
        name: "Raydium CLMM",
        program_id: RAYDIUM_CLMM,
        data_sizes: &[1544],
        discriminator: Some(DISC_POOL_STATE),
        liquidity_proxy: Some(LiquidityProxy {
            mints: (CLMM_MINTS_OFFSET, 64),
            data: (CLMM_LIQUIDITY_OFFSET, 32), // L u128 + sqrt_price_x64 u128
            score: score_liquidity_usd,
        }),
    },
    DexDescriptor {
        name: "Meteora DAMM V1",
        program_id: METEORA_DYNAMIC_AMM,
        data_sizes: &[944],
        discriminator: Some(DISC_POOL),
        liquidity_proxy: None, // sem campo de liquidez no pool account
    },
    DexDescriptor {
        name: "Meteora DAMM V2",
        program_id: METEORA_DYNAMIC_AMM_V2,
        data_sizes: &[1112],
        discriminator: Some(DISC_POOL),
        liquidity_proxy: Some(LiquidityProxy {
            mints: (DAMMV2_MINTS_OFFSET, 64),
            data: (DAMMV2_AMOUNTS_OFFSET, 16), // token_a_amount + token_b_amount u64
            score: score_amounts_usd,
        }),
    },
    DexDescriptor {
        name: "Meteora DLMM",
        program_id: METEORA_DYNAMIC_LMM,
        data_sizes: &[904],
        discriminator: Some(DISC_LB_PAIR),
        liquidity_proxy: None, // liquidez distribuída nos bin arrays
    },
    DexDescriptor {
        name: "Pumpfun AMM",
        program_id: PUMPFUN_AMM_PROGRAM,
        data_sizes: &[],
        discriminator: Some(DISC_POOL),
        liquidity_proxy: None, // bonding curve em conta separada
    },
    DexDescriptor {
        name: "Orca Whirlpool",
        program_id: ORCA_WHIRLPOOL_PROGRAM,
        data_sizes: &[WHIRLPOOL_LEN],
        discriminator: Some(DISC_WHIRLPOOL),
        liquidity_proxy: Some(LiquidityProxy {
            mints: (WHIRLPOOL_MINTS_OFFSET, 112), // token_mint_a + token_mint_b
            data: (WHIRLPOOL_LIQUIDITY_OFFSET, 32), // L u128 + sqrt_price u128
            score: score_liquidity_usd,
        }),
    },
];

pub struct PoolLoader {
    rpc: Arc<RpcClient>,
    max_pools_per_dex: usize,
    top_by_liquidity: bool,
}

impl PoolLoader {
    pub fn new(rpc_url: &str) -> Self {
        let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
            rpc_url.to_string(),
            Duration::from_secs(300),
            CommitmentConfig::confirmed(),
        ));
        Self {
            rpc,
            max_pools_per_dex: DEFAULT_MAX_POOLS_PER_DEX,
            top_by_liquidity: false,
        }
    }

    /// Override the per-DEX pool cap. Set to `usize::MAX` for no limit.
    pub fn with_max_pools(mut self, max: usize) -> Self {
        self.max_pools_per_dex = max;
        self
    }

    /// Mantém os top-N pools por liquidez (proxy embutido no pool account)
    /// em vez dos primeiros N arbitrários da paginação.
    pub fn with_top_by_liquidity(mut self, top: bool) -> Self {
        self.top_by_liquidity = top;
        self
    }

    pub async fn load_all(
        &self,
        progress_cb: &ProgressCallback,
    ) -> Result<PoolIndex, GenericError> {
        let mut index = PoolIndex::new();

        let (r0, r1, r2, r3, r4, r5, r6) = tokio::join!(
            self.load_dex(&DESCRIPTORS[0], progress_cb),
            self.load_dex(&DESCRIPTORS[1], progress_cb),
            self.load_dex(&DESCRIPTORS[2], progress_cb),
            self.load_dex(&DESCRIPTORS[3], progress_cb),
            self.load_dex(&DESCRIPTORS[4], progress_cb),
            self.load_dex(&DESCRIPTORS[5], progress_cb),
            self.load_dex(&DESCRIPTORS[6], progress_cb),
        );

        for result in [r0, r1, r2, r3, r4, r5, r6] {
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

        // Top-N por liquidez (proxy embutido no pool account) quando o loader
        // está em modo top-by-liquidity com cap finito.
        if self.top_by_liquidity && self.max_pools_per_dex != usize::MAX {
            return self.load_dex_top(desc, cb).await;
        }

        let raw_accounts = self.fetch_dex_raw(desc, cb).await;
        self.finish_dex(desc, cb, raw_accounts).await
    }

    /// Loop de fetch bruto por data_size (early-stop quando capado; GPA único
    /// com fallback two-phase/paginated quando sem cap).
    async fn fetch_dex_raw(
        &self,
        desc: &DexDescriptor,
        cb: &ProgressCallback,
    ) -> Vec<(Pubkey, Account)> {
        let dex = desc.name;
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

            // Capped loads (`PoolLoader::with_max_pools`, ex.: `--max-per-dex
            // 50`) não devem pagar o custo do programa inteiro: um único GPA
            // (`fetch_filtered`) do DAMM V2 na NLN demora demais mesmo quando o
            // RPC serve tudo de uma vez. Para cap finito, vai direto pro
            // two-phase com early-stop (dataSlice só das chaves + batches até o
            // cap); sem cap, mantém o GPA único com fallback two-phase/paginated.
            let stop_after =
                (self.max_pools_per_dex != usize::MAX).then_some(self.max_pools_per_dex);
            let fetched = if stop_after.is_some() {
                match self.two_phase_fetch(&program, filters.clone(), dex, cb, stop_after).await {
                    Ok(accounts) => accounts,
                    Err(_) => match self
                        .fetch_paginated_v2(&program, filters.clone(), dex, cb, stop_after)
                        .await
                    {
                        Ok(accounts) => accounts,
                        Err(_) => match self.fetch_filtered(&program, filters.clone()).await {
                            Ok(accounts) => accounts,
                            Err(_) => continue,
                        },
                    },
                }
            } else {
                match self.fetch_filtered(&program, filters.clone()).await {
                    Ok(accounts) => accounts,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("excluded from account secondary indexes") {
                            cb(progress(dex, LoadPhase::Error(
                                "Program excluded from RPC indexes".into(),
                            )));
                            return Vec::new();
                        }
                        match self.two_phase_fetch(&program, filters, dex, cb, None).await {
                            Ok(accounts) => accounts,
                            Err(_) => continue,
                        }
                    }
                }
            };

            raw_accounts.extend(fetched);
        }

        raw_accounts
    }

    /// Cauda comum do load: cap, fases Deserializing/build_entries/Complete.
    async fn finish_dex(
        &self,
        desc: &DexDescriptor,
        cb: &ProgressCallback,
        raw_accounts: Vec<(Pubkey, Account)>,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = desc.name;
        if raw_accounts.is_empty() {
            cb(progress(dex, LoadPhase::Complete { pool_count: 0 }));
            return Ok(vec![]);
        }

        let mut raw_accounts = raw_accounts;
        if raw_accounts.len() > self.max_pools_per_dex {
            raw_accounts.truncate(self.max_pools_per_dex);
        }

        let total = raw_accounts.len();
        cb(progress(dex, LoadPhase::Deserializing { done: 0, total }));

        let entries = self.build_entries(desc, raw_accounts, cb).await?;
        cb(progress(dex, LoadPhase::Complete { pool_count: entries.len() }));
        Ok(entries)
    }

    /// Carrega apenas os top-N pools por liquidez sem pagar um full load:
    /// para DEXs com proxy embutido, dois dataSlice GPAs baratos (mints +
    /// campo de liquidez/reservas) enumeram todos os pools, rankeia por USD
    /// estimado (hub tokens) e busca full data + vaults só dos N escolhidos.
    /// DEXs sem proxy (V4, DAMM V1, DLMM, Pumpfun — liquidez nos vaults) caem
    /// no early-stop arbitrário.
    async fn load_dex_top(
        &self,
        desc: &DexDescriptor,
        cb: &ProgressCallback,
    ) -> Result<Vec<(String, PoolEntry)>, GenericError> {
        let dex = desc.name;
        let Some(proxy) = desc.liquidity_proxy.as_ref() else {
            println!("[cache] {dex}: sem proxy de liquidez embutido — top-N arbitrário (early-stop)");
            let raw = self.fetch_dex_raw(desc, cb).await;
            return self.finish_dex(desc, cb, raw).await;
        };

        // Dois dataSlice GPAs: mints + campo de score. Respostas pequenas —
        // enumeram todos os pools sem baixar os dados completos.
        let program = Pubkey::from_str_const(desc.program_id);
        let mut filters = Vec::new();
        if let Some(&size) = desc.data_sizes.first() {
            if size > 0 {
                filters.push(RpcFilterType::DataSize(size));
            }
        }
        if let Some(disc) = &desc.discriminator {
            filters.push(RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, disc.to_vec())));
        }
        let gpa = |offset: usize, length: usize| {
            let config = RpcProgramAccountsConfig {
                filters: Some(filters.clone()),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    data_slice: Some(solana_account_decoder_client_types::UiDataSliceConfig {
                        offset,
                        length,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            self.rpc.get_program_accounts_with_config(&program, config)
        };

        let (mints_gpa, data_gpa) = tokio::join!(
            gpa(proxy.mints.0, proxy.mints.1),
            gpa(proxy.data.0, proxy.data.1),
        );
        let (mints_gpa, data_gpa) = match (mints_gpa, data_gpa) {
            (Ok(m), Ok(d)) => (m, d),
            (Err(e), _) | (_, Err(e)) => {
                println!("[cache] {dex}: dataSlice GPA falhou ({e}) — top-N arbitrário (early-stop)");
                let raw = self.fetch_dex_raw(desc, cb).await;
                return self.finish_dex(desc, cb, raw).await;
            }
        };

        // Score por pool: mints de um GPA, campo de score do outro.
        let mints_map: std::collections::HashMap<Pubkey, Vec<u8>> =
            mints_gpa.into_iter().map(|(pk, acc)| (pk, acc.data)).collect();
        let total_pools = data_gpa.len();
        let mut scored: Vec<(f64, Pubkey)> = Vec::with_capacity(total_pools);
        for (pk, acc) in data_gpa {
            if let Some(mints) = mints_map.get(&pk) {
                if mints.len() >= proxy.mints.1 && acc.data.len() >= proxy.data.1 {
                    scored.push(((proxy.score)(mints, &acc.data), pk));
                }
            }
        }
        scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        scored.truncate(self.max_pools_per_dex);
        println!(
            "[cache] {dex}: top-{} por liquidez USD de {} pools",
            scored.len(),
            total_pools
        );

        // Full data dos top-N (getMultipleAccounts em batches).
        let mut full: Vec<(Pubkey, Account)> = Vec::with_capacity(scored.len());
        for chunk in scored.chunks(BALANCE_BATCH_SIZE) {
            let addrs: Vec<Pubkey> = chunk.iter().map(|(_, pk)| *pk).collect();
            let accounts = self.rpc.get_multiple_accounts(&addrs).await?;
            for (pk, acc) in chunk.iter().map(|(_, pk)| pk).zip(accounts.into_iter().flatten()) {
                full.push((*pk, acc));
            }
        }

        self.finish_dex(desc, cb, full).await
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
        stop_after: Option<usize>,
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
            if stop_after.is_some_and(|cap| results.len() >= cap) {
                break;
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
        stop_after: Option<usize>,
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
            // Com cap definido, pagina apenas o suficiente (uma página) em vez
            // de enumerar o programa inteiro.
            let remaining = stop_after.map(|cap| cap.saturating_sub(results.len()));
            let page_limit = remaining.map(|r| r.min(1000)).unwrap_or(1000);
            let mut cfg = serde_json::json!({
                "encoding": "base64",
                "commitment": "confirmed",
                "filters": filters_json,
                "limit": page_limit,
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

            if stop_after.is_some_and(|cap| results.len() >= cap) {
                break;
            }

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
