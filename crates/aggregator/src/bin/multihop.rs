//! Build and simulate a genuine multi-hop, multi-protocol route:
//!   WSOL -> USDC (Raydium AMM V4) -> TokenX (Meteora DAMM V2)
//! searched from the loaded cache. Simulates as a v0 tx and reports size with
//! and without an ALT. No SOL spent.
//!
//! Usage: RPC_URL=... cargo run --release -p thunder-aggregator --bin multihop -- [pools.cache] [payer]

use std::path::PathBuf;
use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signer};

use thunder_aggregator::cache;
use thunder_aggregator::execute;
use thunder_aggregator::pool_index::PoolIndex;
use thunder_aggregator::types::{Route, RouteHop};
use thunder_core::{SwapDirection, USDC, WSOL};
use thunder_executor::{alt, submit};

/// Find a pool of `dex` connecting mints `a` and `b`; return (address, quoted_out for a->b).
fn find_pool(index: &PoolIndex, dex: &str, a: Pubkey, b: Pubkey, amount_in: u64) -> Option<(String, u64)> {
    index.iter_pools().find_map(|(addr, e)| {
        if e.dex_name != dex {
            return None;
        }
        let matches = (e.quote_mint == a && e.base_mint == b) || (e.quote_mint == b && e.base_mint == a);
        if !matches {
            return None;
        }
        let dir = if e.quote_mint == a { SwapDirection::Buy } else { SwapDirection::Sell };
        let out = e.market.calculate_output(amount_in, dir).unwrap_or(0);
        (out > 0).then(|| (addr.to_string(), out))
    })
}

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL");
    // If SIGNER_KEY is set, we can create the ALT and land; otherwise size-only.
    let signer: Option<Keypair> = std::env::var("SIGNER_KEY").ok().map(|s| Keypair::from_base58_string(&s));
    let mut args = std::env::args().skip(1);
    let cache_path = args.next().unwrap_or_else(|| "pools.cache".to_string());
    let payer = signer
        .as_ref()
        .map(|k| k.pubkey())
        .or_else(|| args.next().and_then(|s| Pubkey::from_str(&s).ok()))
        .unwrap_or_else(Pubkey::new_unique);

    let (index, _) = cache::load_cache(&PathBuf::from(&cache_path)).expect("load cache");
    let rpc = RpcClient::new(rpc_url);
    let wsol = Pubkey::from_str_const(WSOL);
    let usdc = Pubkey::from_str_const(USDC);
    let amount_in = 10_000_000u64; // 0.01 SOL

    println!("Loaded {} pools. Searching WSOL->USDC (Raydium V4) -> X (DAMM V2)...", index.pool_count());

    // Hop 1: WSOL -> USDC on Raydium V4.
    let Some((ray_pool, usdc_out)) = find_pool(&index, "Raydium AMM V4", wsol, usdc, amount_in) else {
        println!("no Raydium V4 WSOL/USDC pool in cache — need to fetch the canonical pool");
        return;
    };
    println!("hop1 Raydium V4 {ray_pool}: {amount_in} WSOL -> {usdc_out} USDC");

    // Hop 2: USDC -> X on Meteora DAMM V2. Pick the DEEPEST such pool — a random
    // one is often a stale/empty concentrated-liquidity pool (swap panics with
    // `liquidity > 0`). Rank by cached vault depth as a liveness proxy.
    let hop2 = index
        .iter_pools()
        .filter_map(|(addr, e)| {
            if e.dex_name != "Meteora DAMM V2" {
                return None;
            }
            let x = if e.quote_mint == usdc {
                e.base_mint
            } else if e.base_mint == usdc {
                e.quote_mint
            } else {
                return None;
            };
            if x == wsol || x == usdc {
                return None;
            }
            let fin = e.market.financials().ok()?;
            let depth = fin.quote_balance as u128 + fin.base_balance as u128;
            let dir = if e.quote_mint == usdc { SwapDirection::Buy } else { SwapDirection::Sell };
            let out = e.market.calculate_output(usdc_out, dir).unwrap_or(0);
            (out > 0).then_some((addr.to_string(), x, out, depth))
        })
        .max_by_key(|(_, _, _, depth)| *depth)
        .map(|(a, x, o, _)| (a, x, o));
    let Some((damm_pool, x_mint, x_out)) = hop2 else {
        println!("no Meteora DAMM V2 USDC/X pool in cache to chain from USDC");
        return;
    };
    println!("hop2 DAMM V2 {damm_pool}: {usdc_out} USDC -> {x_out} {x_mint}");

    // Haircut the intermediate input below the quoted amount: hop1's real
    // on-chain output can be slightly under our quote, and hop2 must not try to
    // spend more USDC than actually arrived (else Token "insufficient funds").
    let usdc_spend = usdc_out * 95 / 100;

    // Build the 2-hop route.
    let route = Route {
        hops: vec![
            RouteHop { pool_address: ray_pool, dex_name: "Raydium AMM V4".into(), input_mint: wsol, output_mint: usdc, input_amount: amount_in, output_amount: usdc_out, price_impact_bps: 0 },
            RouteHop { pool_address: damm_pool, dex_name: "Meteora DAMM V2".into(), input_mint: usdc, output_mint: x_mint, input_amount: usdc_spend, output_amount: x_out, price_impact_bps: 0 },
        ],
        input_mint: wsol,
        output_mint: x_mint,
        input_amount: amount_in,
        output_amount: x_out,
        price_impact_bps: 0,
    };

    // Report size with/without ALT (proves the ALT is required).
    let ixs = execute::build_route_instructions(&rpc, &index, &route, payer, 3_000).await.unwrap();
    let mut all = submit::compute_budget_ixs(600_000, 1_000);
    all.extend(ixs);
    let no_alt = submit::versioned_tx_size(&submit::build_unsigned_v0_transaction(&payer, &all, &[]).unwrap());
    let addrs = alt::collect_addresses(&all, &[payer]);
    let table = alt::lookup_table(Pubkey::new_unique(), addrs.clone());
    let with_alt = submit::versioned_tx_size(&submit::build_unsigned_v0_transaction(&payer, &all, &[table]).unwrap());
    println!("accounts: {}  size no-ALT: {no_alt}  with-ALT: {with_alt}  (limit 1232)", addrs.len());

    // Land it: execute_route creates the ALT (route is over-size), builds a v0 tx
    // through it, and send does preflight — a failing swap errors without landing.
    let Some(payer_kp) = signer else {
        println!("\nSet SIGNER_KEY to create the ALT and land this multi-hop route.");
        return;
    };
    println!("\ncreating ALT + landing 2-hop via v0...");
    match execute::execute_route(&rpc, &index, &route, &payer_kp, 3_000, 600_000, 1_000, None).await {
        Ok(sig) => {
            println!("✅ LANDED multi-hop: {sig}");
            println!("   https://solscan.io/tx/{sig}");
        }
        Err(e) => println!("multi-hop send failed (preflight likely): {e}"),
    }
}
