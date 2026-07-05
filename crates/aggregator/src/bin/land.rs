//! Live on-chain test: create an Address Lookup Table and land a real swap as a
//! v0 transaction routed through it. Spends real SOL — for a burner wallet.
//!
//! Guardrail: simulates the exact route first and only proceeds if it succeeds.
//!
//! Usage:
//!   RPC_URL=... SIGNER_KEY=<base58 secret> \
//!     cargo run --release -p thunder-aggregator --bin land -- [pools.cache] [amount_lamports] [slippage_bps]

use std::path::PathBuf;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signer};

use thunder_aggregator::cache;
use thunder_aggregator::execute;
use thunder_aggregator::types::{Route, RouteHop};
use thunder_core::{SwapDirection, WSOL};
use thunder_executor::{alt, submit};

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL");
    let secret = std::env::var("SIGNER_KEY").expect("SIGNER_KEY (base58 secret)");
    let payer = Keypair::from_base58_string(&secret);

    let mut args = std::env::args().skip(1);
    let cache_path = args.next().unwrap_or_else(|| "pools.cache".to_string());
    let amount_in: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000_000); // 0.01 SOL
    let slippage_bps: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1_500); // 15%

    let (index, _) = cache::load_cache(&PathBuf::from(&cache_path)).expect("load cache");
    let rpc = RpcClient::new(rpc_url);
    let wsol = Pubkey::from_str_const(WSOL);
    let me = payer.pubkey();
    println!("payer {me}, {} pools, in={amount_in} slip={slippage_bps}bps", index.pool_count());

    // Find a Raydium V4 WSOL pool whose swap simulates green (no ALT, fits).
    let mut chosen: Option<Route> = None;
    for (addr, e) in index.iter_pools() {
        if e.dex_name != "Raydium AMM V4" {
            continue;
        }
        let other = if e.quote_mint == wsol {
            e.base_mint
        } else if e.base_mint == wsol {
            e.quote_mint
        } else {
            continue;
        };
        let dir = if e.quote_mint == wsol { SwapDirection::Buy } else { SwapDirection::Sell };
        let out = e.market.calculate_output(amount_in, dir).unwrap_or(0);
        if out == 0 {
            continue;
        }
        let hop = RouteHop {
            pool_address: addr.to_string(),
            dex_name: e.dex_name.clone(),
            input_mint: wsol,
            output_mint: other,
            input_amount: amount_in,
            output_amount: out,
            price_impact_bps: 0,
        };
        let route = Route {
            hops: vec![hop],
            input_mint: wsol,
            output_mint: other,
            input_amount: amount_in,
            output_amount: out,
            price_impact_bps: 0,
        };

        // Pre-flight simulate the exact instructions (no ALT — 1 hop fits).
        let ixs = match execute::build_route_instructions(&rpc, &index, &route, me, slippage_bps).await {
            Ok(i) => i,
            Err(_) => continue,
        };
        let tx = submit::build_unsigned_v0_transaction(&me, &ixs, &[]).unwrap();
        match submit::simulate_versioned(&rpc, &tx).await {
            Ok(r) if r.err.is_none() => {
                println!("green pool {addr}  WSOL -> {other}  quoted_out={out}  units={:?}", r.units_consumed);
                chosen = Some(route);
                break;
            }
            _ => continue,
        }
    }

    let Some(route) = chosen else {
        println!("no Raydium V4 pool simulated green — aborting (no spend)");
        return;
    };

    // 1) Create a real ALT with the route's accounts (refundable rent).
    let ixs = execute::build_route_instructions(&rpc, &index, &route, me, slippage_bps).await.unwrap();
    let addresses = alt::collect_addresses(&ixs, &[me]);
    println!("creating ALT with {} addresses...", addresses.len());
    let table = match alt::create_and_extend_lookup_table(&rpc, &payer, &addresses).await {
        Ok(t) => {
            println!("ALT created: {}", t.key);
            t
        }
        Err(e) => {
            println!("ALT creation failed: {e}");
            return;
        }
    };

    // 2) Land the swap as a v0 tx routed through the ALT (real trade).
    println!("landing swap via v0+ALT...");
    match execute::execute_route(&rpc, &index, &route, &payer, slippage_bps, 300_000, 1_000, Some(table))
        .await
    {
        Ok(sig) => {
            println!("✅ LANDED: {sig}");
            println!("   https://solscan.io/tx/{sig}");
        }
        Err(e) => println!("swap send failed: {e}"),
    }
}
