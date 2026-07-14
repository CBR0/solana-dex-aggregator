use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use tokio::sync::RwLock;

use solroute_aggregator::cache;
use solroute_aggregator::loader::PoolLoader;
use solroute_aggregator::price;

use solroute_engine::account_store::AccountStore;
use solroute_engine::api::{self, AppState};
use solroute_engine::cold_start;
use solroute_engine::pool_registry::PoolRegistry;
use solroute_engine::streaming;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    let cache_path = PathBuf::from(env::var("CACHE_PATH").unwrap_or_else(|_| "pools.cache".into()));
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    println!("solroute engine");
    println!("==============");

    // 1. Load pools from cache or RPC.
    let (pool_index, loaded_from_cache) = if cache_path.exists() {
        println!("Loading pools from cache: {}", cache_path.display());
        match cache::load_cache(&cache_path) {
            Ok((index, ts)) => {
                println!(
                    "Loaded {} pools from cache (timestamp {})",
                    index.pool_count(),
                    ts
                );
                (index, true)
            }
            Err(e) => {
                eprintln!("Cache load failed: {e}, falling back to RPC");
                let index = load_from_rpc(&rpc_url).await;
                (index, false)
            }
        }
    } else {
        println!("No cache found at {}, loading from RPC", cache_path.display());
        let index = load_from_rpc(&rpc_url).await;
        (index, false)
    };

    println!("Pool index: {} pools", pool_index.pool_count());

    // 2. Build PoolRegistry and validate immediately from cached vault balances.
    //    No RPC needed — the market objects already have balances from the cache.
    let mut registry = PoolRegistry::from_pool_index(&pool_index);
    registry.validate_from_cache();
    println!(
        "Registry: {} pools, {} swappable, {} unique mints",
        registry.pool_count(),
        registry.swappable_count(),
        registry.unique_mints()
    );

    // 3. Build shared state. Server starts serving quotes immediately using
    //    cached balances. Background task fetches fresh vault data and re-validates.
    let store = Arc::new(AccountStore::new());
    let pool_index = Arc::new(pool_index);

    // Warm the canonical edge map (deepest-liquidity pool per pair) so the
    // router's intermediate hops are O(1) lookups from the first quote onward.
    let canon_pairs = pool_index.warm_canonical();
    println!("Canonical edges: {canon_pairs} token pairs indexed");

    // Warm the degree-ranked hub set so hub selection is instant per quote.
    let hub_count = pool_index.warm_hubs();
    println!("Hub ranking: top {hub_count} mints by pool degree");
    let state = Arc::new(AppState {
        store: store.clone(),
        pool_index: pool_index.clone(),
        registry: Arc::new(RwLock::new(registry)),
        sol_usd_price: RwLock::new(None),
        start_time: Instant::now(),
    });

    // 4. Start gRPC streaming in background.
    // start_streaming holds a !Send future (Box<dyn Error> without Send bound),
    // so we run it on a dedicated single-threaded runtime.
    let store_bg = store.clone();
    let registry_bg = state.registry.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build streaming runtime");
        rt.block_on(streaming::start_streaming(store_bg, registry_bg));
    });

    // 5. Background: fetch fresh vault data, auxiliary accounts, re-validate.
    //    Vault fetching (4M+ accounts) is the only slow phase.
    let cold_state = state.clone();
    let cold_rpc_url = rpc_url.clone();
    tokio::spawn(async move {
        let rpc = RpcClient::new_with_timeout_and_commitment(
            cold_rpc_url,
            std::time::Duration::from_secs(120),
            CommitmentConfig::confirmed(),
        );

        println!("[cold_start] starting background vault fetch");

        // Fetch fresh vault balances (slow, read lock only).
        {
            let reg = cold_state.registry.read().await;
            cold_start::fetch_all_vaults(&rpc, &reg, &cold_state.store).await;
        }

        // Auxiliary accounts + re-validate (fast, write lock).
        {
            let mut reg = cold_state.registry.write().await;
            cold_start::fetch_bitmap_extensions(&rpc, &mut reg, &cold_state.store).await;
            cold_start::fetch_tick_arrays(&rpc, &mut reg, &cold_state.store).await;
            cold_start::fetch_dlmm_bin_arrays(&rpc, &mut reg, &cold_state.store).await;
            cold_start::fetch_damm_v1_aux(&rpc, &reg, &cold_state.store).await;
            cold_start::fetch_whirlpool_tick_arrays(&rpc, &reg, &cold_state.store).await;
            cold_start::fetch_clmm_amm_configs(&rpc, &cold_state.store).await;
            reg.validate_all(&cold_state.store);

            println!("[cold_start] === cold start complete ===");
            println!("[cold_start] total accounts in store: {}", cold_state.store.len());
            println!(
                "[cold_start] swappable pools: {}/{}",
                reg.swappable_count(),
                reg.pool_count()
            );
            for (dex, count) in reg.dex_counts() {
                let swappable = reg
                    .iter_pools()
                    .filter(|(_, info)| info.dex_name == *dex && info.swappable)
                    .count();
                println!("[cold_start]   {dex}: {swappable}/{count} swappable");
            }
        }

        // Save cache if loaded from RPC.
        if !loaded_from_cache {
            println!("Saving pool cache to {}", cache_path.display());
            match cache::save_cache(&pool_index, &cache_path) {
                Ok(n) => println!("Saved {n} pools to cache"),
                Err(e) => eprintln!("Cache save failed: {e}"),
            }
        }
    });

    // 6. SOL/USD price refresh every 15s.
    let price_state = state.clone();
    let price_rpc_url = rpc_url.clone();
    tokio::spawn(async move {
        let rpc = RpcClient::new_with_timeout_and_commitment(
            price_rpc_url,
            Duration::from_secs(10),
            CommitmentConfig::confirmed(),
        );
        loop {
            if let Some(p) = price::fetch_sol_usd_onchain(&rpc).await {
                *price_state.sol_usd_price.write().await = Some(p);
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    // 7. Start HTTP server (blocks main task).
    let app = api::create_router(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("failed to bind TCP listener");
    println!("Serving on http://0.0.0.0:{port}");
    solroute_engine::axum::serve(listener, app).await.expect("HTTP server error");
}

async fn load_from_rpc(rpc_url: &str) -> solroute_aggregator::pool_index::PoolIndex {
    let loader = PoolLoader::new(rpc_url);
    let cb: solroute_aggregator::loader::ProgressCallback = Box::new(|progress| {
        println!("[loader] {}: {:?}", progress.dex_name, progress.phase);
    });
    loader
        .load_all(&cb)
        .await
        .expect("failed to load pools from RPC")
}
