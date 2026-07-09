//! Simulate a swap through one specific pool (by address). Builds a WSOL->other
//! hop and simulates it as a v0 transaction. No SOL spent.
//!
//! Usage: RPC_URL=... cargo run --release -p solroute-aggregator --bin sim-pool -- <pool_addr> [amount_lamports] [payer_pubkey]

use std::path::PathBuf;
use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use solroute_aggregator::cache;
use solroute_aggregator::execute;
use solroute_aggregator::types::{Route, RouteHop};
use solroute_core::{SwapDirection, WSOL};
use solroute_executor::submit;

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL");
    let mut args = std::env::args().skip(1);
    let pool_addr = args.next().expect("pool address arg");
    let amount_in: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000_000);
    let payer = args.next().and_then(|s| Pubkey::from_str(&s).ok()).unwrap_or_else(Pubkey::new_unique);

    let (index, _) = cache::load_cache(&PathBuf::from("pools.cache")).expect("load cache");
    let rpc = RpcClient::new(rpc_url);
    let wsol = Pubkey::from_str_const(WSOL);

    let entry = index.get_pool(&pool_addr).expect("pool not in cache");
    let (input, output) = if entry.quote_mint == wsol {
        (wsol, entry.base_mint)
    } else if entry.base_mint == wsol {
        (wsol, entry.quote_mint)
    } else {
        panic!("pool has no WSOL side");
    };
    let dir = if entry.quote_mint == wsol { SwapDirection::Buy } else { SwapDirection::Sell };
    let out = entry.market.calculate_output(amount_in, dir).unwrap_or(0);
    println!("{} [{}]  WSOL -> {}  in={amount_in} quoted_out={out}", pool_addr, entry.dex_name, output);

    let route = Route {
        hops: vec![RouteHop {
            pool_address: pool_addr.clone(),
            dex_name: entry.dex_name.clone(),
            input_mint: input,
            output_mint: output,
            input_amount: amount_in,
            output_amount: out,
            price_impact_bps: 0,
        }],
        input_mint: input,
        output_mint: output,
        input_amount: amount_in,
        output_amount: out,
        price_impact_bps: 0,
    };

    let ixs = match execute::build_route_instructions(&rpc, &index, &route, payer, 2_000).await {
        Ok(i) => i,
        Err(e) => { println!("build failed: {e}"); return; }
    };
    let mut all = submit::compute_budget_ixs(400_000, 1_000);
    all.extend(ixs);
    let tx = submit::build_unsigned_v0_transaction(&payer, &all, &[]).unwrap();
    match submit::simulate_versioned(&rpc, &tx).await {
        Ok(r) => match r.err {
            None => println!("✅ sim OK  units={:?}", r.units_consumed),
            Some(e) => {
                println!("⚠️  {e:?}  units={:?}", r.units_consumed);
                if let Some(logs) = r.logs { for l in logs.iter().rev().take(4).rev() { println!("   {l}"); } }
            }
        },
        Err(e) => println!("simulate failed: {e}"),
    }
}
