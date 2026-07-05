//! Validation test: compare our computed pool prices against DexScreener's prices.
//!
//! 1. Streams pool updates from Geyser for 15 seconds
//! 2. Fetches mint decimals
//! 3. Computes prices using our library
//! 4. Looks up each pool on DexScreener to get the reference price
//! 5. Compares and reports differences
//!
//! Run: cargo test --test validate_prices -- --nocapture

mod helpers;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use borsh::BorshDeserialize;
use futures::StreamExt;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::*;

use thunder_core::Market;

// ---------------------------------------------------------------------------
// DEX config (CLMM, DAMM V2, DLMM only — these have price from pool data)
// ---------------------------------------------------------------------------

struct DexConfig {
    dex: &'static str,
    program_id: &'static str,
    data_sizes: &'static [usize],
    disc_skip: usize,
}

const DEXES: &[DexConfig] = &[
    DexConfig { dex: "Raydium CLMM",    program_id: raydium_clmm::RAYDIUM_CLMM,           data_sizes: &[1544],      disc_skip: 8 },
    DexConfig { dex: "Meteora DAMM V2", program_id: meteora_damm::METEORA_DYNAMIC_AMM_V2,  data_sizes: &[1112],      disc_skip: 8 },
    DexConfig { dex: "Meteora DLMM",    program_id: meteora_dlmm::METEORA_DYNAMIC_LMM,     data_sizes: &[904],       disc_skip: 8 },
];

fn parse_mint_decimals(data: &[u8]) -> Option<u8> {
    if data.len() < 45 { return None; }
    Some(data[44])
}

struct PoolResult {
    dex: &'static str,
    pool_address: String,
    mint_a: Pubkey,
    mint_b: Pubkey,
    our_price: f64,
}

fn try_parse(dex: &DexConfig, pubkey: &Pubkey, data: &[u8], dec_cache: &HashMap<Pubkey, u8>) -> Option<PoolResult> {
    if !dex.data_sizes.contains(&data.len()) || data.len() < dex.disc_skip { return None; }
    let body = &data[dex.disc_skip..];
    let addr = pubkey.to_string();
    let get_dec = |m: &Pubkey| {
        if let Some(&d) = dec_cache.get(m) { return d; }
        // pump.fun tokens always have 6 decimals — detect by address ending in "pump".
        let s = m.to_string();
        if s.ends_with("pump") { return 6; }
        // Known 6-decimal stablecoins.
        match s.as_str() {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => 6, // USDC
            "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => 6, // USDT
            "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo" => 6, // PYUSD
            _ => 9,
        }
    };

    match dex.dex {
        "Raydium CLMM" => {
            let pool = raydium_clmm::RaydiumCLMMPool::deserialize(&mut &*body).ok()?;
            let m = raydium_clmm::RaydiumClmmMarket::new(pool, addr.clone());
            let meta = m.metadata().ok()?;
            let price = m.current_price().ok()?;
            if !price.is_finite() || price == 0.0 { return None; }
            Some(PoolResult { dex: dex.dex, pool_address: addr, mint_a: meta.quote_mint, mint_b: meta.base_mint, our_price: price })
        }
        "Meteora DAMM V2" => {
            let pool = meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &*body).ok()?;
            let mut m = meteora_damm::MeteoraDAMMV2Market::new(pool, addr.clone());
            m.token_a_decimals = get_dec(&m.pool.token_a_mint);
            m.token_b_decimals = get_dec(&m.pool.token_b_mint);
            let meta = m.metadata().ok()?;
            let price = m.current_price().ok()?;
            if !price.is_finite() || price == 0.0 { return None; }
            Some(PoolResult { dex: dex.dex, pool_address: addr, mint_a: meta.quote_mint, mint_b: meta.base_mint, our_price: price })
        }
        "Meteora DLMM" => {
            let pool = meteora_dlmm::MeteoraDLMMPool::deserialize(&mut &*body).ok()?;
            let mut m = meteora_dlmm::MeteoraDlmmMarket::new(pool, addr.clone());
            m.token_x_decimals = get_dec(&m.pool.token_x_mint);
            m.token_y_decimals = get_dec(&m.pool.token_y_mint);
            let meta = m.metadata().ok()?;
            let price = m.current_price().ok()?;
            if !price.is_finite() || price == 0.0 { return None; }
            Some(PoolResult { dex: dex.dex, pool_address: addr, mint_a: meta.quote_mint, mint_b: meta.base_mint, our_price: price })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// DexScreener price lookup
// ---------------------------------------------------------------------------

/// Fetch the reference price for a pool from DexScreener.
/// Returns (base_symbol, quote_symbol, price_of_base_in_quote_terms).
async fn dexscreener_price(pool_address: &str) -> Option<(String, String, f64)> {
    let url = format!(
        "https://api.dexscreener.com/latest/dex/pairs/solana/{}",
        pool_address
    );
    let resp = reqwest::get(&url).await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let pair = json.get("pair")?;
    let price_native: f64 = pair.get("priceNative")?.as_str()?.parse().ok()?;
    let base_symbol = pair.get("baseToken")?.get("symbol")?.as_str()?.to_string();
    let quote_symbol = pair.get("quoteToken")?.get("symbol")?.as_str()?.to_string();
    Some((base_symbol, quote_symbol, price_native))
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_pool_prices_against_dexscreener() {
    let _ = dotenvy::dotenv();
    if std::env::var("GEYSER_ENDPOINT").is_err() {
        println!("GEYSER_ENDPOINT not set, skipping");
        return;
    }

    // Phase 1: Collect pools for 15 seconds.
    eprintln!("Phase 1: Collecting pools (15s)...");
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

    let mut client = helpers::geyser_client(false).await;
    let mut stream = client.subscribe_once(request).await.expect("subscribe failed");
    let owner_map: HashMap<String, &DexConfig> = DEXES.iter().map(|d| (d.program_id.to_string(), d)).collect();

    let mut pool_data: HashMap<String, (Vec<u8>, &DexConfig, Pubkey)> = HashMap::new();
    let mut all_mints: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();

    let deadline = Instant::now() + Duration::from_secs(15);
    while let Ok(Some(msg)) = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), stream.next()).await {
        let Ok(msg) = msg else { continue };
        if let Some(subscribe_update::UpdateOneof::Account(au)) = msg.update_oneof {
            let Some(acct) = au.account else { continue };
            let Ok(pk) = <[u8; 32]>::try_from(acct.pubkey.as_slice()) else { continue };
            let pubkey = Pubkey::from(pk);
            let owner = <[u8; 32]>::try_from(acct.owner.as_slice()).map(|b| Pubkey::from(b).to_string()).unwrap_or_default();
            let Some(dex) = owner_map.get(&owner) else { continue };
            if !dex.data_sizes.contains(&acct.data.len()) { continue; }
            let body = &acct.data[dex.disc_skip..];
            let mints: Option<(Pubkey, Pubkey)> = match dex.dex {
                "Raydium CLMM" => raydium_clmm::RaydiumCLMMPool::deserialize(&mut &*body).ok().map(|p| (p.token_mint_0, p.token_mint_1)),
                "Meteora DAMM V2" => meteora_damm::MeteoraDAMMV2Pool::deserialize(&mut &*body).ok().map(|p| (p.token_a_mint, p.token_b_mint)),
                "Meteora DLMM" => meteora_dlmm::MeteoraDLMMPool::deserialize(&mut &*body).ok().map(|p| (p.token_x_mint, p.token_y_mint)),
                _ => None,
            };
            if let Some((a, b)) = mints { all_mints.insert(a); all_mints.insert(b); }
            pool_data.insert(pubkey.to_string(), (acct.data.clone(), *dex, pubkey));
        }
    }
    drop(stream);
    eprintln!("Collected {} pools, {} mints.", pool_data.len(), all_mints.len());

    // Phase 2: Fetch mint decimals in chunks (Geyser snapshot can choke on too many at once).
    eprintln!("Phase 2: Fetching mint decimals...");
    let mint_list: Vec<Pubkey> = all_mints.into_iter().collect();
    let mut dec_cache: HashMap<Pubkey, u8> = HashMap::new();

    for chunk in mint_list.chunks(30) {
        let mint_req = SubscribeRequest {
            accounts: HashMap::from([("m".into(), SubscribeRequestFilterAccounts {
                account: chunk.iter().map(|p| p.to_string()).collect(),
                owner: vec![], filters: vec![], nonempty_txn_signature: None,
            })]),
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        };
        let mut mc = helpers::geyser_client(true).await;
        if let Ok(mut ms) = mc.subscribe_once(mint_req).await {
            let dl = Instant::now() + Duration::from_secs(8);
            while let Ok(Some(msg)) = tokio::time::timeout(dl.saturating_duration_since(Instant::now()), ms.next()).await {
                let Ok(msg) = msg else { continue };
                if let Some(subscribe_update::UpdateOneof::Account(au)) = msg.update_oneof {
                    if let Some(acct) = au.account {
                        if let Ok(pk) = <[u8; 32]>::try_from(acct.pubkey.as_slice()) {
                            if let Some(d) = parse_mint_decimals(&acct.data) {
                                dec_cache.insert(Pubkey::from(pk), d);
                            }
                        }
                    }
                }
            }
        }
        eprintln!("  ...got {} decimals so far", dec_cache.len());
    }
    eprintln!("Got decimals for {} mints.", dec_cache.len());

    // Phase 3: Parse pools.
    let mut results: Vec<PoolResult> = Vec::new();
    for (_, (data, dex, pubkey)) in &pool_data {
        if let Some(r) = try_parse(dex, pubkey, data, &dec_cache) {
            results.push(r);
        }
    }
    eprintln!("Parsed {} pools with valid prices.", results.len());

    // Phase 4: Validate against DexScreener (rate limit: ~300 req/min).
    // Pick up to 50 pools to validate.
    let to_validate: Vec<&PoolResult> = results.iter().take(50).collect();
    eprintln!("Phase 4: Validating {} pools against DexScreener...\n", to_validate.len());

    println!(
        "{:<16} {:<14} {:<8} {:<8} {:>14} {:>14} {:>10} {:>8}",
        "DEX", "POOL", "BASE", "QUOTE", "OUR_PRICE", "DS_PRICE", "DIFF_%", "STATUS"
    );
    println!("{}", "=".repeat(100));

    let mut checked = 0u32;
    let mut passed = 0u32;
    let mut warned = 0u32;
    let mut failed = 0u32;

    for r in to_validate {
        // DexScreener rate limit: be gentle.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let Some((base_sym, quote_sym, ds_price)) = dexscreener_price(&r.pool_address).await else {
            continue;
        };

        // DexScreener returns price of base in quote terms (e.g., 84 USDC per SOL).
        // Our price is quote_per_base (e.g., 0.0119 SOL per USDC).
        // To compare: 1/our_price should ≈ ds_price (when mints match).
        //
        // But the base/quote assignment may differ between us and DexScreener.
        // We try both orientations.
        let our_inverted = if r.our_price != 0.0 { 1.0 / r.our_price } else { 0.0 };

        let (diff_pct, used_price) = {
            let diff_direct = ((r.our_price - ds_price) / ds_price * 100.0).abs();
            let diff_inverted = ((our_inverted - ds_price) / ds_price * 100.0).abs();
            if diff_direct < diff_inverted {
                (diff_direct, r.our_price)
            } else {
                (diff_inverted, our_inverted)
            }
        };

        let status = if diff_pct < 2.0 { "OK" } else if diff_pct < 10.0 { "WARN" } else { "FAIL" };
        checked += 1;
        match status {
            "OK" => passed += 1,
            "WARN" => warned += 1,
            _ => failed += 1,
        }

        let trunc = |s: &str| if s.len() > 12 { format!("{}..{}", &s[..5], &s[s.len()-3..]) } else { s.to_string() };

        println!(
            "{:<16} {:<14} {:<8} {:<8} {:>14.8} {:>14.8} {:>9.2}% {:>8}",
            r.dex,
            trunc(&r.pool_address),
            base_sym,
            quote_sym,
            used_price,
            ds_price,
            diff_pct,
            status,
        );
    }

    println!("\n{checked} validated: {passed} OK (<2%), {warned} WARN (2-10%), {failed} FAIL (>10%).");
    if checked > 0 {
        println!("Pass rate: {:.1}%", (passed + warned) as f64 / checked as f64 * 100.0);
    }
}
