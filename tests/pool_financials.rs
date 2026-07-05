//! Integration test: stream live pool account updates across all DEXs via Yellowstone gRPC.
//!
//! For each pool update, extracts mint addresses and fetches their decimals from
//! a local cache (populated by subscribing to mint accounts on-demand). This ensures
//! correct price calculations for ALL pairs regardless of token decimals.
//!
//! Run: cargo test --test pool_financials -- --nocapture

mod helpers;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use borsh::BorshDeserialize;
use futures::StreamExt;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::*;

use thunder_core::Market;

// ---------------------------------------------------------------------------
// DEX config
// ---------------------------------------------------------------------------

struct DexConfig {
    dex: &'static str,
    program_id: &'static str,
    data_sizes: &'static [usize],
    disc_skip: usize,
}

const DEXES: &[DexConfig] = &[
    DexConfig { dex: "Raydium V4",      program_id: raydium_amm_v4::RAYDIUM_LIQUIDITY_POOL_V4, data_sizes: &[752],       disc_skip: 0 },
    DexConfig { dex: "Raydium CLMM",    program_id: raydium_clmm::RAYDIUM_CLMM,               data_sizes: &[1544],      disc_skip: 8 },
    DexConfig { dex: "Meteora DAMM",    program_id: meteora_damm::METEORA_DYNAMIC_AMM,         data_sizes: &[944, 952],  disc_skip: 8 },
    DexConfig { dex: "Meteora DAMM V2", program_id: meteora_damm::METEORA_DYNAMIC_AMM_V2,      data_sizes: &[1112],      disc_skip: 8 },
    DexConfig { dex: "Meteora DLMM",    program_id: meteora_dlmm::METEORA_DYNAMIC_LMM,         data_sizes: &[904],       disc_skip: 8 },
    DexConfig { dex: "Pumpfun AMM",     program_id: pumpfun_amm::PUMPFUN_AMM_PROGRAM,          data_sizes: &[300, 301],  disc_skip: 8 },
];

// ---------------------------------------------------------------------------
// Mint decimals cache
// ---------------------------------------------------------------------------

/// Read decimals from raw SPL Mint account data.
/// SPL Mint layout: [mint_authority_option(4), mint_authority(32), supply(8), decimals(1), ...]
/// Decimals is at byte offset 44.
fn parse_mint_decimals(data: &[u8]) -> Option<u8> {
    if data.len() < 45 {
        return None;
    }
    Some(data[44])
}

/// Thread-safe mint decimals cache.
#[derive(Clone)]
struct DecimalsCache {
    inner: Arc<Mutex<HashMap<Pubkey, u8>>>,
}

impl DecimalsCache {
    fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn get(&self, mint: &Pubkey) -> Option<u8> {
        self.inner.lock().unwrap().get(mint).copied()
    }

    fn insert(&self, mint: Pubkey, decimals: u8) {
        self.inner.lock().unwrap().insert(mint, decimals);
    }

    fn get_or_default(&self, mint: &Pubkey) -> u8 {
        if let Some(d) = self.get(mint) { return d; }
        // pump.fun tokens always have 6 decimals.
        let s = mint.to_string();
        if s.ends_with("pump") { return 6; }
        match s.as_str() {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => 6, // USDC
            "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => 6, // USDT
            "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo" => 6, // PYUSD
            _ => 9,
        }
    }
}

// ---------------------------------------------------------------------------
// Pool info (what we display)
// ---------------------------------------------------------------------------

struct PoolInfo {
    dex: &'static str,
    pool_address: String,
    mint_a: Pubkey,
    mint_b: Pubkey,
    price: f64,
    fee_bps: u64,
}

// ---------------------------------------------------------------------------
// Pool parsing — extracts mints, sets decimals, computes price
// ---------------------------------------------------------------------------

/// Extract the two mint pubkeys from a pool account without constructing a full Market.
fn extract_mints(dex: &DexConfig, data: &[u8]) -> Option<(Pubkey, Pubkey)> {
    if data.len() < dex.disc_skip { return None; }
    let body = &data[dex.disc_skip..];

    match dex.dex {
        "Raydium V4" => {
            let pool = raydium_amm_v4::RaydiumAMMV4::deserialize(&mut &*body).ok()?;
            Some((pool.quote_mint, pool.base_mint))
        }
        "Raydium CLMM" => {
            let pool = raydium_clmm::RaydiumCLMMPool::deserialize(&mut &*body).ok()?;
            Some((pool.token_mint_0, pool.token_mint_1))
        }
        "Meteora DAMM" => {
            let pool = meteora_damm::MeteoraDAMMPool::deserialize(&mut &*body).ok()?;
            Some((pool.token_a_mint, pool.token_b_mint))
        }
        "Meteora DAMM V2" => {
            let pool = meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &*body).ok()?;
            Some((pool.token_a_mint, pool.token_b_mint))
        }
        "Meteora DLMM" => {
            let pool = meteora_dlmm::MeteoraDLMMPool::deserialize(&mut &*body).ok()?;
            Some((pool.token_x_mint, pool.token_y_mint))
        }
        "Pumpfun AMM" => {
            let pool = pumpfun_amm::PumpfunAmmPool::deserialize(&mut &*body).ok()?;
            Some((pool.quote_mint, pool.base_mint))
        }
        _ => None,
    }
}

/// Parse pool account into PoolInfo, using cached decimals for correct pricing.
fn try_parse_pool(dex: &DexConfig, pubkey: &Pubkey, data: &[u8], cache: &DecimalsCache) -> Option<PoolInfo> {
    if !dex.data_sizes.contains(&data.len()) { return None; }
    if data.len() < dex.disc_skip { return None; }
    let body = &data[dex.disc_skip..];
    let addr = pubkey.to_string();

    match dex.dex {
        "Raydium V4" => {
            let pool = raydium_amm_v4::RaydiumAMMV4::deserialize(&mut &*body).ok()?;
            let m = raydium_amm_v4::RaydiumAmmV4Market::new(pool, addr.clone(), 0, 0);
            extract_info(dex.dex, addr, &m)
        }
        "Raydium CLMM" => {
            // CLMM pool struct has mint_decimals_0 and mint_decimals_1 — no cache needed.
            let pool = raydium_clmm::RaydiumCLMMPool::deserialize(&mut &*body).ok()?;
            let m = raydium_clmm::RaydiumClmmMarket::new(pool, addr.clone());
            extract_info(dex.dex, addr, &m)
        }
        "Meteora DAMM" => {
            let pool = meteora_damm::MeteoraDAMMPool::deserialize(&mut &*body).ok()?;
            let mut m = meteora_damm::MeteoraDAMMMarket::new(pool, addr.clone());
            m.token_a_decimals = cache.get_or_default(&m.pool.token_a_mint);
            m.token_b_decimals = cache.get_or_default(&m.pool.token_b_mint);
            extract_info(dex.dex, addr, &m)
        }
        "Meteora DAMM V2" => {
            let pool = meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &*body).ok()?;
            let mut m = meteora_damm::MeteoraDAMMV2Market::new(pool, addr.clone());
            m.token_a_decimals = cache.get_or_default(&m.pool.token_a_mint);
            m.token_b_decimals = cache.get_or_default(&m.pool.token_b_mint);
            extract_info(dex.dex, addr, &m)
        }
        "Meteora DLMM" => {
            let pool = meteora_dlmm::MeteoraDLMMPool::deserialize(&mut &*body).ok()?;
            let mut m = meteora_dlmm::MeteoraDlmmMarket::new(pool, addr.clone());
            m.token_x_decimals = cache.get_or_default(&m.pool.token_x_mint);
            m.token_y_decimals = cache.get_or_default(&m.pool.token_y_mint);
            extract_info(dex.dex, addr, &m)
        }
        "Pumpfun AMM" => {
            let pool = pumpfun_amm::PumpfunAmmPool::deserialize(&mut &*body).ok()?;
            let mut m = pumpfun_amm::PumpfunAmmMarket::new(pool, addr.clone());
            m.base_decimals = cache.get_or_default(&m.pool.base_mint);
            extract_info(dex.dex, addr, &m)
        }
        _ => None,
    }
}

fn extract_info(dex: &'static str, pool_address: String, market: &dyn Market) -> Option<PoolInfo> {
    let meta = market.metadata().ok()?;
    let price = market.current_price().unwrap_or(0.0);
    Some(PoolInfo {
        dex,
        pool_address,
        mint_a: meta.quote_mint,
        mint_b: meta.base_mint,
        price,
        fee_bps: meta.fees.trade_fee_bps,
    })
}

// ---------------------------------------------------------------------------
// Mint fetcher — runs in background, subscribes to mint accounts for decimals
// ---------------------------------------------------------------------------

async fn fetch_mint_decimals(mints: Vec<Pubkey>, cache: DecimalsCache) {
    if mints.is_empty() { return; }

    // Chunk to avoid overwhelming the Geyser snapshot.
    for chunk in mints.chunks(30) {
        let request = SubscribeRequest {
            accounts: HashMap::from([("mints".to_string(), SubscribeRequestFilterAccounts {
                account: chunk.iter().map(|p| p.to_string()).collect(),
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            })]),
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        };

        let mut geyser = helpers::geyser_client(true).await;
        let Ok(mut stream) = geyser.subscribe_once(request).await else {
            continue;
        };

    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(Some(msg)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        stream.next(),
    ).await {
        let Ok(msg) = msg else { continue };
        if let Some(subscribe_update::UpdateOneof::Account(au)) = msg.update_oneof {
            if let Some(acct) = au.account {
                if let Ok(pk) = <[u8; 32]>::try_from(acct.pubkey.as_slice()) {
                    if let Some(dec) = parse_mint_decimals(&acct.data) {
                        cache.insert(Pubkey::from(pk), dec);
                    }
                }
            }
        }
    }
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_all_pool_updates() {
    let _ = dotenvy::dotenv();
    if std::env::var("GEYSER_ENDPOINT").is_err() {
        println!("GEYSER_ENDPOINT not set, skipping");
        return;
    }

    // Phase 1: Subscribe to pool accounts.
    let mut account_filters: HashMap<String, SubscribeRequestFilterAccounts> = HashMap::new();
    for dex in DEXES {
        for &size in dex.data_sizes {
            account_filters.insert(
                format!("{}_{}", dex.dex, size),
                SubscribeRequestFilterAccounts {
                    account: vec![],
                    owner: vec![dex.program_id.to_string()],
                    filters: vec![SubscribeRequestFilterAccountsFilter {
                        filter: Some(subscribe_request_filter_accounts_filter::Filter::Datasize(size as u64)),
                    }],
                    nonempty_txn_signature: None,
                },
            );
        }
    }

    let request = SubscribeRequest {
        accounts: account_filters,
        commitment: Some(CommitmentLevel::Confirmed as i32),
        ..Default::default()
    };

    eprintln!("Connecting to Geyser...");
    let mut client = helpers::geyser_client(false).await;
    let mut stream = client.subscribe_once(request).await.expect("subscribe failed");
    eprintln!("Subscribed. Streaming pool updates...\n");

    let owner_map: HashMap<String, &DexConfig> = DEXES
        .iter()
        .map(|d| (d.program_id.to_string(), d))
        .collect();

    let cache = DecimalsCache::new();
    let mut table: HashMap<String, PoolInfo> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut updates = 0u64;
    let mut last_redraw = Instant::now();

    // Track mints we've already requested decimals for.
    let mut requested_mints: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    // Buffer mints whose decimals we need to fetch.
    let mut pending_mints: Vec<Pubkey> = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(300);

    while let Ok(Some(msg)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        stream.next(),
    ).await {
        let Ok(msg) = msg else { continue };

        if let Some(subscribe_update::UpdateOneof::Account(au)) = msg.update_oneof {
            let Some(acct) = au.account else { continue };
            let Ok(pk_bytes) = <[u8; 32]>::try_from(acct.pubkey.as_slice()) else { continue };
            let pubkey = Pubkey::from(pk_bytes);
            let owner = <[u8; 32]>::try_from(acct.owner.as_slice())
                .map(|b| Pubkey::from(b).to_string())
                .unwrap_or_default();

            let Some(dex) = owner_map.get(&owner) else { continue };

            // Extract mints and queue decimal fetches for unknown ones.
            if let Some((mint_a, mint_b)) = extract_mints(dex, &acct.data) {
                for mint in [mint_a, mint_b] {
                    if cache.get(&mint).is_none() && requested_mints.insert(mint) {
                        pending_mints.push(mint);
                    }
                }
            }

            // Batch-fetch decimals: every 100 pending or every 3 seconds.
            if pending_mints.len() >= 100
                || (!pending_mints.is_empty() && last_redraw.elapsed() >= Duration::from_secs(3))
            {
                let batch = std::mem::take(&mut pending_mints);
                let c = cache.clone();
                tokio::spawn(async move { fetch_mint_decimals(batch, c).await });
            }

            if let Some(info) = try_parse_pool(dex, &pubkey, &acct.data, &cache) {
                updates += 1;
                let addr = info.pool_address.clone();
                if !table.contains_key(&addr) {
                    order.push(addr.clone());
                }
                table.insert(addr, info);

                if last_redraw.elapsed() >= Duration::from_millis(500) {
                    redraw_table(&order, &table, updates);
                    last_redraw = Instant::now();
                }
            }
        }
    }

    // Flush any remaining pending mints.
    if !pending_mints.is_empty() {
        fetch_mint_decimals(pending_mints, cache).await;
    }

    redraw_table(&order, &table, updates);
    eprintln!("\nDone. {} unique pools, {} total updates.", table.len(), updates);
}

fn redraw_table(order: &[String], table: &HashMap<String, PoolInfo>, updates: u64) {
    print!("\x1b[2J\x1b[H");

    println!(
        "{:<16} {:<46} {:<46} {:<46} {:>16} {:>8}",
        "DEX", "POOL", "MINT A", "MINT B", "PRICE", "FEE"
    );
    println!("{}", "=".repeat(180));

    for addr in order {
        if let Some(info) = table.get(addr) {
            if info.price == 0.0 || !info.price.is_finite() {
                continue;
            }
            println!(
                "{:<16} {:<46} {:<46} {:<46} {:>16.10} {:>7}bp",
                info.dex,
                info.pool_address,
                info.mint_a,
                info.mint_b,
                info.price,
                info.fee_bps,
            );
        }
    }

    println!(
        "\n{} pools | {} updates",
        order.len(),
        updates,
    );
}
