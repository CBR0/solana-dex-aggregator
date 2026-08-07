#![allow(deprecated)]

use std::collections::HashMap;
use std::str::FromStr;

use futures::future::join_all;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::RpcFilterType;

use crate::account_store::AccountStore;
use crate::pool_registry::PoolRegistry;

const BATCH_SIZE: usize = 100;
// 100 concorrentes estrangula a NLN (503 em toda janela — o loader usa 20 e
// funciona). 20 mantém o cold-start veloz sem estourar a cota de requests.
const BATCH_CONCURRENCY: usize = 20;

const DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

const MAX_RETRIES: u32 = 6;
const RETRY_BASE_MS: u64 = 250;

/// True quando o erro é um rate limit/sobrecarga do RPC (a NLN responde 502/503
/// quando o cold-start a 100 concorrentes estoura a cota). Esses merecem retry
/// com backoff; erros reais (conta inexistente, etc.) falham imediatamente.
fn is_retryable(e: &solana_rpc_client_api::client_error::Error) -> bool {
    use solana_rpc_client_api::client_error::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::Reqwest(err)
            if err
                .status()
                .map(|s| matches!(s.as_u16(), 429 | 502 | 503 | 504))
                .unwrap_or(false)
    )
}

/// getMultipleAccounts com retry exponencial em 429/502/503/504. Retorna `None`
/// quando as tentativas esgotam — o caller segue sem o batch (a resiliência do
/// cold-start é por etapa, e os pools sem dados live simplesmente não validam).
async fn fetch_accounts_retry<T, F, Fut>(make: F) -> Option<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, solana_rpc_client_api::client_error::Error>>,
{
    let mut attempt = 0u32;
    loop {
        match make().await {
            Ok(v) => return Some(v),
            Err(e) if is_retryable(&e) && attempt < MAX_RETRIES => {
                attempt += 1;
                // Backoff exponencial com jitter determinístico (quebra a
                // sincronização dos batches quando todos falham na mesma janela).
                let delay = (RETRY_BASE_MS << attempt.min(6)) + (attempt as u64 * 53) % 100;
                eprintln!("[cold_start] retry {attempt}/{MAX_RETRIES} em {delay}ms: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Err(e) => {
                eprintln!("[cold_start] batch error: {e}");
                return None;
            }
        }
    }
}

/// Fetch all vault accounts from the registry and store them in the AccountStore.
pub async fn fetch_all_vaults(
    rpc: &RpcClient,
    registry: &PoolRegistry,
    store: &AccountStore,
) {
    // Pumpfun AMM prices from virtual reserves in the pool account, not vault
    // balances — its calculate_output_live ignores them and it is always
    // swappable (no vault-funding gate). Skip its vaults: ~1M pools / ~2M
    // accounts of pure waste in the cold-start fetch.
    let vault_keys: Vec<Pubkey> = registry
        .iter_pools()
        .filter(|(_, info)| info.dex_name != "Pumpfun AMM")
        .flat_map(|(_, info)| [info.quote_vault, info.base_vault])
        .collect();

    let total = vault_keys.len();
    println!("[cold_start] fetching {} vault accounts (Pumpfun vaults skipped)", total);

    let chunks: Vec<(usize, &[Pubkey])> = vault_keys.chunks(BATCH_SIZE).enumerate().collect();
    let mut fetched = 0usize;
    let mut last_print = 0usize;

    for window in chunks.chunks(BATCH_CONCURRENCY) {
        let futures: Vec<_> = window
            .iter()
            .map(|(_, chunk)| {
                fetch_accounts_retry(|| {
                    rpc.get_multiple_accounts_with_commitment(
                        chunk,
                        CommitmentConfig::confirmed(),
                    )
                })
            })
            .collect();
        let results = join_all(futures).await;

        for ((_, chunk), result) in window.iter().zip(results) {
            if let Some(response) = result {
                // Stamp with the real context slot so the store's slot
                // guard can order these writes against streamed updates.
                let slot = response.context.slot;
                for (pubkey, maybe_account) in chunk.iter().zip(response.value) {
                    if let Some(account) = maybe_account {
                        store.upsert(
                            *pubkey,
                            account.data,
                            account.owner,
                            account.lamports,
                            slot,
                        );
                        fetched += 1;
                    }
                }
            }
        }

        // Print progress every ~500k accounts.
        if fetched - last_print >= 500_000 {
            println!("[cold_start] vaults: {fetched}/{total}");
            last_print = fetched;
        }
    }

    println!("[cold_start] vaults done: {fetched}/{total} stored");
}

/// Fetch CLMM tick arrays by deriving PDAs from each pool's bitmap, then
/// batch-fetching with getMultipleAccounts. Replaces the previous single-GPA
/// approach which failed on large response payloads.
pub async fn fetch_tick_arrays(
    rpc: &RpcClient,
    registry: &mut PoolRegistry,
    store: &AccountStore,
) {
    // Collect CLMM pools sorted by vault balance descending. The two vault
    // balances are summed in u128: individual token amounts are u64, and a
    // pool with large reserves in both vaults overflows u64 in debug builds
    // (panic), breaking cold-start on big caches.
    let mut clmm_pools: Vec<(&str, u128)> = registry
        .iter_pools()
        .filter(|(_, info)| info.dex_name == "Raydium CLMM")
        .map(|(addr, info)| {
            let balance = store.read_token_balance(&info.quote_vault) as u128
                + store.read_token_balance(&info.base_vault) as u128;
            (addr, balance)
        })
        .collect();

    clmm_pools.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    clmm_pools.truncate(10_000);

    if clmm_pools.is_empty() {
        println!("[cold_start] no CLMM pools found, skipping tick arrays");
        return;
    }

    // Derive tick array PDAs from each pool's cached data (bitmap + current tick).
    let pool_addrs: Vec<String> = clmm_pools.iter().map(|(a, _)| a.to_string()).collect();
    let mut pool_tick_map: HashMap<String, Vec<Pubkey>> = HashMap::new();
    let mut all_pdas: Vec<Pubkey> = Vec::new();

    for addr in &pool_addrs {
        let cached_data = match registry.get_pool(addr).map(|i| &i.cached_data) {
            Some(d) if !d.is_empty() => d,
            _ => continue,
        };
        if let Some((_pool_id, pdas)) = solroute_aggregator::cache::extract_clmm_tick_pdas(cached_data) {
            pool_tick_map.insert(addr.clone(), pdas.clone());
            all_pdas.extend(pdas);
        }
    }

    // Deduplicate.
    all_pdas.sort();
    all_pdas.dedup();

    println!(
        "[cold_start] fetching {} tick array accounts for {} CLMM pools",
        all_pdas.len(),
        pool_tick_map.len()
    );

    // Batch fetch with getMultipleAccounts.
    let mut fetched = 0usize;
    let chunks: Vec<&[Pubkey]> = all_pdas.chunks(BATCH_SIZE).collect();

    for window in chunks.chunks(BATCH_CONCURRENCY) {
        let futures: Vec<_> = window
            .iter()
            .map(|chunk| {
                fetch_accounts_retry(|| {
                    rpc.get_multiple_accounts_with_commitment(
                        chunk,
                        CommitmentConfig::confirmed(),
                    )
                })
            })
            .collect();
        let results = join_all(futures).await;

        for (chunk, result) in window.iter().zip(results) {
            if let Some(response) = result {
                // Real context slot — lets the store's slot guard order
                // these writes against streamed updates.
                let slot = response.context.slot;
                for (pubkey, maybe_account) in chunk.iter().zip(response.value) {
                    if let Some(account) = maybe_account {
                        store.upsert(
                            *pubkey,
                            account.data,
                            account.owner,
                            account.lamports,
                            slot,
                        );
                        fetched += 1;
                    }
                }
            }
        }
    }

    // Assign tick arrays to pool infos, keeping only those that exist on-chain.
    for (addr, pdas) in &pool_tick_map {
        if let Some(pool_info) = registry.get_pool_mut(addr) {
            pool_info.tick_arrays = pdas
                .iter()
                .filter(|pk| store.contains(pk))
                .cloned()
                .collect();
        }
    }

    println!(
        "[cold_start] tick arrays: {fetched} stored across {} pools",
        pool_tick_map.len()
    );
}

/// Fetch DLMM bin array accounts for the active bin of each pool.
/// Derives bin array PDAs from cached pool data, batch-fetches them,
/// and records the PDA on each PoolInfo so the swappable check can
/// verify existence in the store without re-deriving.
pub async fn fetch_dlmm_bin_arrays(
    rpc: &RpcClient,
    registry: &mut PoolRegistry,
    store: &AccountStore,
) {
    // Derive bin array PDAs for all DLMM pools.
    let mut pda_map: HashMap<String, Pubkey> = HashMap::new(); // pool_addr -> bin_array PDA
    let mut all_pdas: Vec<Pubkey> = Vec::new();

    for (addr, info) in registry.iter_pools() {
        if info.dex_name != "Meteora DLMM" {
            continue;
        }
        // Skip pools that already have a bitmap extension (they pass the
        // swappable check without a bin array).
        if info.bitmap_ext.is_some() {
            continue;
        }
        if let Some((_pool_pk, pda)) = solroute_aggregator::cache::extract_dlmm_bin_pda(&info.cached_data) {
            pda_map.insert(addr.to_string(), pda);
            all_pdas.push(pda);
        }
    }

    all_pdas.sort();
    all_pdas.dedup();

    if all_pdas.is_empty() {
        return;
    }

    println!(
        "[cold_start] fetching {} DLMM bin array accounts for {} pools",
        all_pdas.len(),
        pda_map.len()
    );

    let mut fetched = 0usize;
    let chunks: Vec<&[Pubkey]> = all_pdas.chunks(BATCH_SIZE).collect();

    for window in chunks.chunks(BATCH_CONCURRENCY) {
        let futures: Vec<_> = window
            .iter()
            .map(|chunk| {
                fetch_accounts_retry(|| {
                    rpc.get_multiple_accounts_with_commitment(
                        chunk,
                        CommitmentConfig::confirmed(),
                    )
                })
            })
            .collect();
        let results = join_all(futures).await;

        for (chunk, result) in window.iter().zip(results) {
            if let Some(response) = result {
                // Real context slot — lets the store's slot guard order
                // these writes against streamed updates.
                let slot = response.context.slot;
                for (pubkey, maybe_account) in chunk.iter().zip(response.value) {
                    if let Some(account) = maybe_account {
                        store.upsert(
                            *pubkey,
                            account.data,
                            account.owner,
                            account.lamports,
                            slot,
                        );
                        fetched += 1;
                    }
                }
            }
        }
    }

    // Record the bin array PDA on each pool info.
    for (addr, pda) in &pda_map {
        if let Some(pool_info) = registry.get_pool_mut(addr) {
            pool_info.bin_array = Some(*pda);
        }
    }

    println!("[cold_start] DLMM bin arrays: {fetched} stored for {} pools", pda_map.len());
}

/// Fetch all DLMM bitmap extension accounts (dataSize=12488) and assign to pools.
pub async fn fetch_bitmap_extensions(
    rpc: &RpcClient,
    registry: &mut PoolRegistry,
    store: &AccountStore,
) {
    let dlmm_program = match Pubkey::from_str(DLMM_PROGRAM_ID) {
        Ok(pk) => pk,
        Err(_) => return,
    };

    // Slot taken BEFORE the fetch: a lower-bound stamp, so any streamed
    // update racing this GPA wins in the store's slot guard.
    let fetch_slot = rpc.get_slot().await.unwrap_or(0);

    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::DataSize(12488)]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = match rpc
        .get_program_accounts_with_config(&dlmm_program, config)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[cold_start] bitmap extension GPA error: {e}");
            return;
        }
    };

    println!(
        "[cold_start] fetched {} bitmap extension accounts",
        accounts.len()
    );

    // Build a reverse lookup: pool_pubkey -> pool address string.
    let pool_lookup: HashMap<Pubkey, String> = registry
        .iter_pools()
        .filter(|(_, info)| info.dex_name == "Meteora DLMM")
        .filter_map(|(addr, _)| Pubkey::from_str(addr).ok().map(|pk| (pk, addr.to_string())))
        .collect();

    let mut matched = 0usize;

    for (pubkey, account) in accounts {
        if account.data.len() < 40 {
            continue;
        }

        // lb_pair (pool address) at offset 8, 32 bytes.
        let lb_pair = Pubkey::try_from(&account.data[8..40]).unwrap();

        store.upsert(pubkey, account.data, account.owner, account.lamports, fetch_slot);

        if let Some(pool_addr) = pool_lookup.get(&lb_pair) {
            if let Some(pool_info) = registry.get_pool_mut(pool_addr) {
                pool_info.bitmap_ext = Some(pubkey);
                matched += 1;
            }
        }
    }

    println!("[cold_start] bitmap extensions: {matched} matched to pools");
}

/// Fetch all Raydium CLMM AmmConfig accounts (dataSize 117, a few dozen) so
/// exact CLMM quoting can read each pool's real trade_fee_rate.
pub async fn fetch_clmm_amm_configs(rpc: &RpcClient, store: &AccountStore) {
    let Ok(clmm_program) = Pubkey::from_str("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK") else {
        return;
    };
    let fetch_slot = rpc.get_slot().await.unwrap_or(0);
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::DataSize(117)]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };
    match rpc.get_program_accounts_with_config(&clmm_program, config).await {
        Ok(accounts) => {
            let n = accounts.len();
            for (pubkey, account) in accounts {
                store.upsert(pubkey, account.data, account.owner, account.lamports, fetch_slot);
            }
            println!("[cold_start] CLMM amm configs: {n} stored");
        }
        Err(e) => eprintln!("[cold_start] CLMM amm config fetch failed: {e}"),
    }
}

/// Fetch Orca Whirlpool tick arrays: the current array plus one neighbor in
/// each direction per pool (covers near-price swaps both ways; streaming
/// keeps hot pools' further arrays fresh).
pub async fn fetch_whirlpool_tick_arrays(
    rpc: &RpcClient,
    registry: &PoolRegistry,
    store: &AccountStore,
) {
    let mut all_pdas: Vec<Pubkey> = Vec::new();
    for (_, info) in registry.iter_pools() {
        if info.dex_name != "Orca Whirlpool" {
            continue;
        }
        if let Some((_, pdas)) =
            solroute_aggregator::cache::extract_whirlpool_tick_pdas(&info.cached_data)
        {
            all_pdas.extend(pdas);
        }
    }
    all_pdas.sort();
    all_pdas.dedup();
    if all_pdas.is_empty() {
        return;
    }
    let fetched = fetch_batch_into_store(rpc, store, &all_pdas).await;
    println!("[cold_start] whirlpool tick arrays: {fetched}/{} stored", all_pdas.len());
}

/// Fetch the accounts a DAMM V1 exact quote needs: each pool's vault-LP
/// token accounts, the (shared, deduped) dynamic-vault state accounts, and
/// the vault LP mints (parsed out of the fetched vault states). The vault
/// token accounts themselves are already covered by `fetch_all_vaults`.
/// Periodically refresh vault token accounts for the vault-priced DEXs
/// (Raydium V4, Meteora DAMM V1) via `getMultipleAccounts`.
///
/// These pools price off SPL token-account balances, which Triton/rpcpool
/// won't stream (neither an explicit account-list lane nor a token-program
/// owner lane delivers — the former is silently starved, the latter is the
/// whole-chain firehose that kills the whole subscription). Their POOL
/// accounts stream fine, but that carries no reserve info. So we poll: the
/// top `limit` pools by cached balance every `interval`, giving bounded
/// (~interval) vault staleness. Runs forever as a background task.
pub async fn refresh_hot_vaults_loop(
    rpc: RpcClient,
    registry: std::sync::Arc<tokio::sync::RwLock<PoolRegistry>>,
    store: std::sync::Arc<AccountStore>,
    limit: usize,
    interval: std::time::Duration,
) {
    // Vault set is stable across the process; compute it once.
    let mut keys: Vec<Pubkey> = {
        let reg = registry.read().await;
        // Rank by the QUOTE side only: it is always a settlement currency
        // (WSOL/USDC/USDT) with sane magnitude. Summing raw quote+base across
        // mismatched decimals let high-raw-supply junk tokens outrank real
        // pools and evict SOL/USDC from the hot set entirely.
        let mut ranked: Vec<(u128, &crate::pool_registry::PoolInfo)> = reg
            .iter_pools()
            .filter(|(_, info)| {
                info.dex_name == "Raydium AMM V4" || info.dex_name == "Meteora DAMM V1"
            })
            .filter_map(|(_, info)| Some((info.market.financials().ok()?.quote_balance as u128, info)))
            .collect();
        ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        ranked.truncate(limit);

        let mut ks = Vec::with_capacity(limit * 4);
        for (_, info) in ranked {
            ks.push(info.quote_vault);
            ks.push(info.base_vault);
            // DAMM V1's exact quote also reads the dynamic-vault STATE
            // accounts, the pool's vault-LP token accounts, and the vault LP
            // mints (for the LP-share -> token conversion). These don't stream
            // and go stale (~16min observed) -> keep them in the poll set.
            if info.dex_name == "Meteora DAMM V1" {
                if let Some((a_vault, b_vault, a_lp, b_lp)) =
                    solroute_aggregator::cache::extract_damm_v1_aux(&info.cached_data)
                {
                    ks.push(a_vault);
                    ks.push(b_vault);
                    ks.push(a_lp);
                    ks.push(b_lp);
                }
            }
        }
        ks.sort_unstable();
        ks.dedup();
        ks
    };
    // LP mints live inside the (now-fetched-at-cold-start) vault states at
    // offset 8+1+2+8+32+32 = 83 (disc + enabled + bumps + total + token_vault
    // + fee_vault), pubkey 83..115. Add any we can resolve from the store.
    {
        let extra: Vec<Pubkey> = keys
            .iter()
            .filter_map(|pk| {
                let data = store.get_data(pk)?;
                if data.len() != 1232 {
                    return None; // not a vault state
                }
                Pubkey::try_from(data.get(115..147)?).ok()
            })
            .collect();
        keys.extend(extra);
        keys.sort_unstable();
        keys.dedup();
    }
    keys.shrink_to_fit();

    if keys.is_empty() {
        return;
    }
    println!(
        "[refresh] hot-vault refresh loop: {} accounts every {}s",
        keys.len(),
        interval.as_secs()
    );

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cycle: u64 = 0;
    loop {
        tick.tick().await;
        let t0 = std::time::Instant::now();
        let n = fetch_batch_into_store(&rpc, &store, &keys).await;
        let fetch_ms = t0.elapsed().as_millis();
        // Log every ~10th cycle to confirm liveness + timing.
        cycle += 1;
        if cycle % 10 == 1 {
            let sample = keys.first().map(|k| store.get(k).map(|a| a.slot)).flatten();
            println!(
                "[refresh] cycle {cycle}: {n}/{} fetched in {fetch_ms}ms, sample slot {sample:?}, tip {}",
                keys.len(),
                store.last_slot()
            );
        }
    }
}

pub async fn fetch_damm_v1_aux(
    rpc: &RpcClient,
    registry: &PoolRegistry,
    store: &AccountStore,
) {
    let mut vault_states: Vec<Pubkey> = Vec::new();
    let mut lp_accounts: Vec<Pubkey> = Vec::new();

    for (_, info) in registry.iter_pools() {
        if info.dex_name != "Meteora DAMM V1" {
            continue;
        }
        if let Some((a_vault, b_vault, a_lp, b_lp)) =
            solroute_aggregator::cache::extract_damm_v1_aux(&info.cached_data)
        {
            vault_states.push(a_vault);
            vault_states.push(b_vault);
            lp_accounts.push(a_lp);
            lp_accounts.push(b_lp);
        }
    }
    vault_states.sort();
    vault_states.dedup();

    if vault_states.is_empty() {
        return;
    }

    let fetched_lp = fetch_batch_into_store(rpc, store, &lp_accounts).await;
    let fetched_vaults = fetch_batch_into_store(rpc, store, &vault_states).await;

    // Vault LP mints live inside the vault state: 8-byte discriminator +
    // enabled(1) + bumps(2) + total_amount(8) + token_vault(32) +
    // fee_vault(32) + token_mint(32) = offset 115.
    let mut lp_mints: Vec<Pubkey> = vault_states
        .iter()
        .filter_map(|vault_pk| {
            let data = store.get_data(vault_pk)?;
            Pubkey::try_from(data.get(115..147)?).ok()
        })
        .collect();
    lp_mints.sort();
    lp_mints.dedup();
    let fetched_mints = fetch_batch_into_store(rpc, store, &lp_mints).await;

    println!(
        "[cold_start] DAMM V1 aux: {fetched_vaults} vault states, {fetched_lp} lp accounts, {fetched_mints} lp mints"
    );
}

/// Batch-fetch `keys` via getMultipleAccounts and upsert into the store with
/// real context slots. Returns the number of accounts stored.
async fn fetch_batch_into_store(rpc: &RpcClient, store: &AccountStore, keys: &[Pubkey]) -> usize {
    let mut fetched = 0usize;
    let chunks: Vec<&[Pubkey]> = keys.chunks(BATCH_SIZE).collect();
    for window in chunks.chunks(BATCH_CONCURRENCY) {
        let futures: Vec<_> = window
            .iter()
            .map(|chunk| {
                fetch_accounts_retry(|| {
                    rpc.get_multiple_accounts_with_commitment(
                        chunk,
                        CommitmentConfig::confirmed(),
                    )
                })
            })
            .collect();
        let results = join_all(futures).await;
        for (chunk, result) in window.iter().zip(results) {
            if let Some(response) = result {
                let slot = response.context.slot;
                for (pubkey, maybe_account) in chunk.iter().zip(response.value) {
                    if let Some(account) = maybe_account {
                        store.upsert(*pubkey, account.data, account.owner, account.lamports, slot);
                        fetched += 1;
                    }
                }
            }
        }
    }
    fetched
}

/// Cold-start orchestrator: fetch all on-chain data and validate pools.
pub async fn cold_start(
    rpc: &RpcClient,
    registry: &mut PoolRegistry,
    store: &AccountStore,
) {
    println!("[cold_start] starting cold start sequence");

    // 1. Vault balances first — everything else depends on these.
    fetch_all_vaults(rpc, registry, store).await;

    // 2. Bitmap extensions (fast, ~43 accounts).
    fetch_bitmap_extensions(rpc, registry, store).await;

    // 3. Tick arrays (depends on vault balances for sorting).
    fetch_tick_arrays(rpc, registry, store).await;

    // 4. DLMM bin arrays (depends on bitmap extensions for skip logic).
    fetch_dlmm_bin_arrays(rpc, registry, store).await;

    // 5. DAMM V1 aux (vault states, LP accounts, LP mints) for exact quotes.
    fetch_damm_v1_aux(rpc, registry, store).await;

    // 6. Whirlpool tick arrays for exact quotes.
    fetch_whirlpool_tick_arrays(rpc, registry, store).await;

    // 7. CLMM amm configs (real fee rates) for exact quotes.
    fetch_clmm_amm_configs(rpc, store).await;

    // 5. Validate all pools against the now-populated store.
    registry.validate_all(store);

    // 6. Summary.
    println!("[cold_start] === cold start complete ===");
    println!("[cold_start] total accounts in store: {}", store.len());
    println!(
        "[cold_start] swappable pools: {}/{}",
        registry.swappable_count(),
        registry.pool_count()
    );

    for (dex, count) in registry.dex_counts() {
        // Count swappable per DEX.
        let swappable = registry
            .iter_pools()
            .filter(|(_, info)| info.dex_name == *dex && info.swappable)
            .count();
        println!("[cold_start]   {dex}: {swappable}/{count} swappable");
    }
}
