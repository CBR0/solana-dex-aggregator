//! Simulate an exact route (as quoted by the engine) against live chain
//! state, with min_amount_out derived from the quoted output at a tight
//! slippage. PASS = the chain delivers the quote (within slippage);
//! FAIL(slippage) = the quote was inflated.
//!
//! Usage: RPC_URL=... sim-route <route.json> [slippage_bps] [payer_pubkey]
//!
//! route.json = the engine's /quote route object:
//! { "hops": [{"poolAddress","dexName","inputMint","outputMint",
//!             "inputAmount","outputAmount"}], "outputAmount": "..." }

use std::fs;
use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use solroute_aggregator::cache;
use solroute_aggregator::execute;
use solroute_aggregator::types::{Route, RouteHop};
use solroute_executor::submit;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    // Accept one or more route.json paths (comma-separated) — the cache
    // loads once and every route simulates in the same process.
    let route_paths: Vec<String> = args
        .next()
        .expect("route.json path[,route2.json,...]")
        .split(',')
        .map(str::to_string)
        .collect();
    let slippage_bps: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(50);
    let payer = args
        .next()
        .and_then(|s| Pubkey::from_str(&s).ok())
        .unwrap_or_else(|| Pubkey::from_str("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM").unwrap());

    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL not set");
    let rpc = RpcClient::new(rpc_url);

    let cache_path = std::env::var("CACHE_PATH").unwrap_or_else(|_| "pools.cache".into());
    let (index, _) = cache::load_cache(std::path::Path::new(&cache_path)).expect("cache load");

    for route_path in &route_paths {
        simulate_one(route_path, slippage_bps, payer, &rpc, &index).await;
    }
}

async fn simulate_one(
    route_path: &str,
    slippage_bps: u64,
    payer: Pubkey,
    rpc: &RpcClient,
    index: &solroute_aggregator::pool_index::PoolIndex,
) {
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(route_path).expect("read route.json"))
            .expect("parse route.json");

    let parse_hop = |h: &serde_json::Value| -> RouteHop {
        RouteHop {
            pool_address: h["poolAddress"].as_str().unwrap().to_string(),
            dex_name: h["dexName"].as_str().unwrap().to_string(),
            input_mint: Pubkey::from_str(h["inputMint"].as_str().unwrap()).unwrap(),
            output_mint: Pubkey::from_str(h["outputMint"].as_str().unwrap()).unwrap(),
            input_amount: h["inputAmount"].as_str().unwrap().parse().unwrap(),
            output_amount: h["outputAmount"].as_str().unwrap().parse().unwrap(),
            price_impact_bps: 0,
        }
    };
    let hops: Vec<RouteHop> = json["hops"].as_array().unwrap().iter().map(parse_hop).collect();
    let route = Route {
        input_mint: hops.first().unwrap().input_mint,
        output_mint: hops.last().unwrap().output_mint,
        input_amount: hops.first().unwrap().input_amount,
        output_amount: json["outputAmount"].as_str().unwrap().parse().unwrap(),
        price_impact_bps: 0,
        hops,
    };

    // build_route_instructions floors each hop's min_out by `slippage_bps`
    // against the quoted per-hop outputs — the engine's own numbers. If the
    // chain can't deliver within that, the swap program errors and the
    // simulation FAILS => quote was inflated.
    let ixs = match execute::build_route_instructions(rpc, index, &route, payer, slippage_bps).await
    {
        Ok(i) => i,
        Err(e) => {
            println!("ROUTE={route_path} VERDICT=SKIP reason=build:{e}");
            return;
        }
    };
    let mut all = submit::compute_budget_ixs(1_000_000, 1_000);
    all.extend(ixs);
    let tx = match submit::build_unsigned_v0_transaction(&payer, &all, &[]) {
        Ok(t) => t,
        Err(e) => {
            println!("ROUTE={route_path} VERDICT=SKIP reason=txbuild:{e}");
            return;
        }
    };

    match submit::simulate_versioned(rpc, &tx).await {
        Ok(r) => match r.err {
            None => println!("ROUTE={route_path} VERDICT=PASS units={:?}", r.units_consumed),
            Some(e) => {
                let logs = r.logs.unwrap_or_default();
                let slippage_hit = logs.iter().any(|l| {
                    l.contains("ExceededSlippage")
                        || l.contains("exceeds desired slippage")
                        || l.contains("Slippage")
                        || l.contains("0x1771")
                        || l.contains("TooLittle")
                        || l.contains("max_amount")
                        || l.contains("MinimumOut")
                });
                if slippage_hit {
                    println!("ROUTE={route_path} VERDICT=FAIL_SLIPPAGE err={e:?}");
                } else {
                    println!("ROUTE={route_path} VERDICT=FAIL_OTHER err={e:?}");
                }
                for l in logs.iter().rev().take(4).rev() {
                    println!("LOG {l}");
                }
            }
        },
        Err(e) => println!("ROUTE={route_path} VERDICT=SKIP reason=sim:{e}"),
    }
}
