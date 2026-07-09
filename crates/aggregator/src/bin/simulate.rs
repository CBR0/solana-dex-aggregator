//! End-to-end route→execute test via transaction simulation (no SOL, no key).
//! Also demonstrates the multi-hop transaction-size problem and the ALT fix:
//! builds one combined tx across all executable DEXs and reports its size with
//! and without an Address Lookup Table, against the 1232-byte network cap.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin simulate -- [pools.cache] [payer_pubkey]

use std::path::PathBuf;
use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use solroute_aggregator::cache;
use solroute_aggregator::execute;
use solroute_aggregator::pool_index::PoolIndex;
use solroute_aggregator::types::RouteHop;
use solroute_core::{SwapDirection, WSOL};
use solroute_executor::{alt, submit, SwapOptions};

const EXECUTABLE: [&str; 6] = ["Meteora DAMM V2", "Pumpfun AMM", "Raydium AMM V4", "Meteora DAMM V1", "Meteora DLMM", "Raydium CLMM"];
const TX_LIMIT: usize = 1232;

/// Find the DEEPEST WSOL-paired pool for `dex` (random pools are usually dead —
/// pick the one with the most WSOL locked so the swap actually has liquidity).
fn find_wsol_pool(index: &PoolIndex, dex: &str, wsol: Pubkey, amount_in: u64) -> Option<(String, Pubkey, u64)> {
    index
        .iter_pools()
        .filter_map(|(addr, e)| {
            if e.dex_name != dex {
                return None;
            }
            let other = if e.quote_mint == wsol {
                e.base_mint
            } else if e.base_mint == wsol {
                e.quote_mint
            } else {
                return None;
            };
            let fin = e.market.financials().ok()?;
            let wsol_depth = if e.quote_mint == wsol { fin.quote_balance } else { fin.base_balance };
            let dir = if e.quote_mint == wsol { SwapDirection::Buy } else { SwapDirection::Sell };
            let out = e.market.calculate_output(amount_in, dir).unwrap_or(0);
            (out > 0).then_some((addr.to_string(), other, out, wsol_depth))
        })
        .max_by_key(|(_, _, _, depth)| *depth)
        .map(|(a, o, out, _)| (a, o, out))
}

fn hop(addr: String, dex: &str, wsol: Pubkey, other: Pubkey, amount_in: u64, out: u64) -> RouteHop {
    RouteHop {
        pool_address: addr,
        dex_name: dex.to_string(),
        input_mint: wsol,
        output_mint: other,
        input_amount: amount_in,
        output_amount: out,
        price_impact_bps: 0,
    }
}

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL env var must be set");
    let mut args = std::env::args().skip(1);
    let cache_path = args.next().unwrap_or_else(|| "pools.cache".to_string());
    let payer = args
        .next()
        .and_then(|s| Pubkey::from_str(&s).ok())
        .unwrap_or_else(Pubkey::new_unique);

    let (index, _) = cache::load_cache(&PathBuf::from(&cache_path)).expect("load cache");
    println!("Loaded {} pools\n", index.pool_count());

    let rpc = RpcClient::new(rpc_url);
    let wsol = Pubkey::from_str_const(WSOL);
    let amount_in = 10_000_000u64; // 0.01 SOL

    let opts = SwapOptions {
        create_input_ata: false,
        create_output_ata: true,
        wrap_input_sol: true,
        close_wsol: false,
    };

    // Accumulate one swap per executable DEX for the combined-size demo.
    let mut combined: Vec<solana_sdk::instruction::Instruction> = Vec::new();

    for dex in EXECUTABLE {
        let Some((addr, other, out)) = find_wsol_pool(&index, dex, wsol, amount_in) else {
            println!("[{dex}] no WSOL-paired pool in cache — skipping");
            continue;
        };
        let h = hop(addr.clone(), dex, wsol, other, amount_in, out);

        match execute::build_hop_instructions(&rpc, &index, &h, payer, 100, &opts).await {
            Ok(ixs) => {
                // Simulate this hop as a v0 tx.
                match submit::build_unsigned_v0_transaction(&payer, &ixs, &[]) {
                    Ok(tx) => match submit::simulate_versioned(&rpc, &tx).await {
                        Ok(r) => match r.err {
                            None => println!("[{dex}] {addr}\n  ✅ sim OK  units={:?}", r.units_consumed),
                            Some(e) => println!("[{dex}] {addr}\n  ⚠️  {e:?}  units={:?}", r.units_consumed),
                        },
                        Err(e) => println!("[{dex}] simulate failed: {e}"),
                    },
                    Err(e) => println!("[{dex}] v0 build failed: {e}"),
                }
                combined.extend(ixs);
            }
            Err(e) => println!("[{dex}] build failed: {e}"),
        }
    }

    if combined.is_empty() {
        println!("\nNo executable pools in cache.");
        return;
    }

    // --- Combined multi-protocol tx: size with vs without an ALT ---
    let mut all = submit::compute_budget_ixs(600_000, 1_000);
    all.extend(combined);

    let no_alt = submit::build_unsigned_v0_transaction(&payer, &all, &[]).unwrap();
    let size_no_alt = submit::versioned_tx_size(&no_alt);

    let addresses = alt::collect_addresses(&all, &[payer]);
    let table = alt::lookup_table(Pubkey::new_unique(), addresses.clone());
    let with_alt = submit::build_unsigned_v0_transaction(&payer, &all, &[table]).unwrap();
    let size_with_alt = submit::versioned_tx_size(&with_alt);

    println!("\n=== combined tx across {} swaps ===", EXECUTABLE.len());
    println!("unique accounts to table: {}", addresses.len());
    println!(
        "size without ALT: {size_no_alt} bytes  ({})",
        if size_no_alt > TX_LIMIT { "OVER 1232 — won't send" } else { "fits" }
    );
    println!(
        "size with    ALT: {size_with_alt} bytes  ({})",
        if size_with_alt > TX_LIMIT { "still over" } else { "fits ✅" }
    );
    println!("payer={payer}");
}
