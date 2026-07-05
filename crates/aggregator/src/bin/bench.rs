//! Offline routing benchmark. Loads a `pools.cache`, warms the router's
//! canonical-edge map and hub ranking, then times `find_routes` across many
//! source→target pairs. Zero RPC — pure in-memory routing.
//!
//! Usage:
//!   cargo run --release -p solroute-aggregator --bin bench -- [pools.cache] [queries]

use std::path::PathBuf;
use std::time::Instant;

use solana_pubkey::Pubkey;

use solroute_aggregator::cache;
use solroute_aggregator::router::Router;
use solroute_core::WSOL;

fn main() {
    let mut args = std::env::args().skip(1);
    let cache_path = args.next().unwrap_or_else(|| "pools.cache".to_string());
    let queries: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);

    let (index, ts) = cache::load_cache(&PathBuf::from(&cache_path)).expect("failed to load cache");
    println!("Loaded {} pools (cache ts {ts}), {} unique mints", index.pool_count(), index.unique_mints());

    // Warm shared structures and report what the router will use.
    let t = Instant::now();
    let pairs = index.warm_canonical();
    let hubs = index.warm_hubs();
    println!("Warmed: {pairs} canonical pairs, {hubs} ranked hubs in {:?}", t.elapsed());
    print!("Top hubs by degree: ");
    for h in index.top_hubs(5) {
        print!("{}… ", &h.to_string()[..8]);
    }
    println!();

    let wsol = Pubkey::from_str_const(WSOL);
    let amount = 1_000_000_000u64; // 1 SOL

    // Targets: the first `queries` mints in the graph that aren't WSOL.
    let targets: Vec<Pubkey> = index
        .all_mints()
        .into_iter()
        .filter(|m| *m != wsol)
        .take(queries)
        .collect();
    println!("\nBenchmarking WSOL -> {} targets at 1 SOL in\n", targets.len());

    for max_hops in [2usize, 3, 4] {
        let mut found = 0usize;
        let mut total_hops = 0usize;
        let start = Instant::now();
        for t in &targets {
            let router = Router::new(&index, max_hops);
            if let Ok(q) = router.find_routes(wsol, *t, amount, 5) {
                if let Some(best) = q.best() {
                    found += 1;
                    total_hops += best.hops.len();
                }
            }
        }
        let elapsed = start.elapsed();
        let per = elapsed.as_micros() as f64 / targets.len() as f64;
        let avg_hops = if found > 0 { total_hops as f64 / found as f64 } else { 0.0 };
        println!(
            "maxHops={max_hops}: {found}/{} routable | {:.1} µs/query | {:.2} ms total | avg {avg_hops:.2} hops",
            targets.len(),
            per,
            elapsed.as_secs_f64() * 1000.0,
        );
    }
}
