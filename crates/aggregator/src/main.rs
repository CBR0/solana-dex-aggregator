use std::env;
use std::path::PathBuf;
use std::time::Instant;

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use thunder_aggregator::{cache, cli, loader, price, stats};

#[tokio::main]
async fn main() {
    println!("Solana Thunder Aggregator");
    println!("========================\n");

    let rpc_url =
        env::var("RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let cache_path = PathBuf::from(env::var("CACHE_PATH").unwrap_or_else(|_| "pools.cache".into()));
    // Max cache age in seconds before forcing a reload (default: 1 hour)
    let max_cache_age: u64 = env::var("CACHE_MAX_AGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    println!("RPC:   {rpc_url}");
    println!("Cache: {}\n", cache_path.display());

    // Fetch SOL/USD from on-chain CLMM sqrt_price.
    let rpc = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed());
    let sol_usd_price = match price::fetch_sol_usd_onchain(&rpc).await {
        Some(p) => {
            println!("SOL/USD: ${p:.2} (on-chain CLMM)\n");
            Some(p)
        }
        None => {
            println!("Warning: Could not fetch SOL/USD from CLMM\n");
            None
        }
    };

    // Try loading from cache first.
    let t0 = Instant::now();
    let index = match cache::cache_age(&cache_path) {
        Some(age) if age < max_cache_age => {
            println!("Loading from cache ({age}s old)...");
            match cache::load_cache(&cache_path) {
                Ok((idx, _ts)) => {
                    println!(
                        "Loaded {} pools across {} tokens from cache in {:.1}s\n",
                        idx.pool_count(),
                        idx.unique_mints(),
                        t0.elapsed().as_secs_f64()
                    );
                    idx
                }
                Err(e) => {
                    println!("Cache load failed ({e}), falling back to RPC...\n");
                    load_from_rpc(&rpc_url).await
                }
            }
        }
        _ => {
            if cache_path.exists() {
                let age = cache::cache_age(&cache_path).unwrap_or(0);
                println!("Cache expired ({age}s old, max {max_cache_age}s), reloading from RPC...\n");
            } else {
                println!("No cache found, loading from RPC...\n");
            }
            load_from_rpc(&rpc_url).await
        }
    };

    // Save cache for next startup.
    let save_t0 = Instant::now();
    match cache::save_cache(&index, &cache_path) {
        Ok(n) => println!("Saved {n} pools to cache in {:.1}s\n", save_t0.elapsed().as_secs_f64()),
        Err(e) => println!("Warning: cache save failed: {e}\n"),
    }

    // Fall back to pool-derived price if CLMM fetch failed.
    let sol_usd_price = sol_usd_price.or_else(|| {
        let p = price::get_sol_usd_price(&index);
        if let Some(p) = p {
            println!("SOL/USD: ${p:.2} (pool-derived)\n");
        }
        p
    });

    // Interactive command loop.
    let mut stats_collector = stats::StatsCollector::new();
    cli::run_repl(&index, &mut stats_collector, sol_usd_price).await;
}

async fn load_from_rpc(rpc_url: &str) -> thunder_aggregator::pool_index::PoolIndex {
    let t0 = Instant::now();
    let display = cli::LoadingDisplay::new();
    let progress_cb = display.progress_callback();

    let loader = loader::PoolLoader::new(rpc_url);
    let index = match loader.load_all(&progress_cb).await {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("Fatal: Failed to load pools: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "\nLoaded {} pools across {} tokens from RPC in {:.1}s\n",
        index.pool_count(),
        index.unique_mints(),
        t0.elapsed().as_secs_f64()
    );

    index
}
