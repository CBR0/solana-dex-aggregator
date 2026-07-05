//! Bounded pool-cache sampler for testing.
//!
//! Reads a JSON map of `{ "<DEX name>": ["<pool address>", ...] }` (produced by
//! a paginated `getProgramAccountsV2` discovery pass) and fetches only those
//! pools + their vaults, reusing the loader's per-DEX build path. Writes a
//! standard `pools.cache` the engine/CLI/bench can load with zero further RPC.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin sample-cache -- <addrs.json> [out.cache]

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use solana_pubkey::Pubkey;

use solroute_aggregator::cache;
use solroute_aggregator::loader::{PoolLoader, ProgressCallback};
use solroute_aggregator::pool_index::PoolIndex;
use solroute_aggregator::types::LoadProgress;

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL env var must be set");
    let mut args = std::env::args().skip(1);
    let addrs_path = args.next().unwrap_or_else(|| "sample_addrs.json".to_string());
    let out_path = args.next().unwrap_or_else(|| "pools.cache".to_string());

    let json = std::fs::read_to_string(&addrs_path).expect("failed to read addresses file");
    let by_dex: HashMap<String, Vec<String>> =
        serde_json::from_str(&json).expect("addresses file must be {dex: [addr,...]}");

    let loader = PoolLoader::new(&rpc_url);
    let cb: ProgressCallback = Box::new(|p: LoadProgress| {
        println!("[sample] {}: {:?}", p.dex_name, p.phase);
    });

    let mut index = PoolIndex::new();
    for (dex, addr_strs) in &by_dex {
        let Some(idx) = PoolLoader::descriptor_index(dex) else {
            eprintln!("[sample] unknown DEX '{dex}', skipping");
            continue;
        };
        let addrs: Vec<Pubkey> = addr_strs
            .iter()
            .filter_map(|s| Pubkey::from_str(s).ok())
            .collect();
        println!("[sample] {dex}: fetching {} pools", addrs.len());

        match loader.build_sample_from_addresses(idx, &addrs, &cb).await {
            Ok(entries) => {
                let built = entries.len();
                for (addr, entry) in entries {
                    let _ = index.add_pool(addr, entry);
                }
                println!("[sample] {dex}: built {built} pools");
            }
            Err(e) => eprintln!("[sample] {dex}: build failed: {e}"),
        }
    }

    println!("[sample] total pools: {}", index.pool_count());
    println!("[sample] unique mints: {}", index.unique_mints());

    match cache::save_cache(&index, &PathBuf::from(&out_path)) {
        Ok(n) => println!("[sample] saved {n} pools to {out_path}"),
        Err(e) => eprintln!("[sample] cache save failed: {e}"),
    }
}
