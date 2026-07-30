use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::time::{Duration, Instant};

use futures::StreamExt;
use solana_pubkey::Pubkey;
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::*;
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;

use crate::account_store::AccountStore;
use crate::pool_registry::PoolRegistry;

/// Account subscriptions, one named filter per (program, account shape).
/// Two hard-won rules (Triton/rpcpool, verified empirically):
/// 1. Bare owner-only filters are silently starved — every filter MUST carry
///    a dataSize or memcmp. The integration tests stream fine with dataSize
///    filters against the same endpoint; owner-only got 0 updates forever.
/// 2. Filters must be split per program — one broad multi-owner filter can
///    kill the whole stream, and a rejected firehose must not poison the
///    pool-update lanes.
/// dataSize where the account is fixed-size, discriminator memcmp otherwise.
enum Shape {
    Size(u64),
    Disc([u8; 8]),
}

const ACCOUNT_SUBS: &[(&str, &str, Shape)] = &[
    // Pool state (quote-critical).
    ("raydium_v4_pool", "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", Shape::Size(752)),
    ("raydium_clmm_pool", "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", Shape::Size(1544)),
    ("damm_v1_pool", "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", Shape::Size(944)),
    ("damm_v2_pool", "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG", Shape::Size(1112)),
    ("dlmm_pool", "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo", Shape::Size(904)),
    // Pumpfun pool size varies across versions — match the Pool discriminator.
    ("pumpfun_pool", "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", Shape::Disc([241, 154, 109, 4, 17, 177, 109, 188])),
    ("whirlpool_pool", "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", Shape::Size(653)),
    // Tick / bin arrays + aux (exact-quote inputs).
    ("clmm_tick_array", "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", Shape::Size(10240)),
    ("clmm_amm_config", "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", Shape::Size(117)),
    ("dlmm_bin_array", "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo", Shape::Size(10136)),
    ("dlmm_bitmap_ext", "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo", Shape::Size(12488)),
    ("whirlpool_tick_array", "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", Shape::Size(9988)),
    // DAMM V1 dynamic vault state (Vault discriminator).
    ("meteora_vault_state", "24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi", Shape::Disc([211, 8, 232, 43, 2, 152, 117, 119])),
    // NOTE: no token-program owner lanes. The whole-chain token firehose
    // (owner=Tokenkeg + dataSize 165) makes Triton silence the ENTIRE
    // subscription — verified by binary search over lanes. Vault balances
    // stream via explicit account-list lanes instead (vault_list_filters).
];

/// Top-N pools (by cached vault balance) whose vault token accounts get
/// explicit account-list subscriptions. Only Raydium V4 and DAMM V1 price
/// off vault balances; the other venues carry price in the pool account.
const VAULT_LIST_POOLS: usize = 15_000;
/// Keys per named account-list filter (bounds per-filter request size).
const VAULT_LIST_CHUNK: usize = 5_000;

/// Build explicit vault account-list filters from the registry: the hot set
/// of V4 / DAMM V1 pools ranked by cached balances, plus DAMM V1 vault-LP
/// token accounts (exact quoting reads them). Cheap: cached financials only.
fn vault_list_filters(
    registry: &PoolRegistry,
) -> HashMap<String, SubscribeRequestFilterAccounts> {
    let mut ranked: Vec<(u128, String, String, Option<(String, String)>)> = registry
        .iter_pools()
        .filter(|(_, info)| {
            info.dex_name == "Raydium AMM V4" || info.dex_name == "Meteora DAMM V1"
        })
        .filter_map(|(_, info)| {
            // Rank by the quote (settlement) side only — see cold_start
            // refresh_hot_vaults_loop for why quote+base raw-sum is wrong.
            let fin = info.market.financials().ok()?;
            let lp = if info.dex_name == "Meteora DAMM V1" {
                solroute_aggregator::cache::extract_damm_v1_aux(&info.cached_data)
                    .map(|(_, _, a_lp, b_lp)| (a_lp.to_string(), b_lp.to_string()))
            } else {
                None
            };
            Some((
                fin.quote_balance as u128,
                info.quote_vault.to_string(),
                info.base_vault.to_string(),
                lp,
            ))
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    ranked.truncate(VAULT_LIST_POOLS);

    let mut keys: Vec<String> = Vec::with_capacity(VAULT_LIST_POOLS * 2);
    for (_, quote_vault, base_vault, lp) in ranked {
        keys.push(quote_vault);
        keys.push(base_vault);
        if let Some((a_lp, b_lp)) = lp {
            keys.push(a_lp);
            keys.push(b_lp);
        }
    }
    keys.sort_unstable();
    keys.dedup();

    let mut filters = HashMap::new();
    for (i, chunk) in keys.chunks(VAULT_LIST_CHUNK).enumerate() {
        filters.insert(
            format!("vault_list_{i}"),
            SubscribeRequestFilterAccounts {
                account: chunk.to_vec(),
                owner: vec![],
                // dataSize(165) is REQUIRED even on an explicit account list:
                // Triton silently starves filterless lanes (verified — the top
                // SOL/USDC vault stayed frozen 125k slots while its pool
                // streamed). All vaults + vault-LP accounts are 165-byte SPL
                // token accounts, so this matches every key in the list.
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(
                        subscribe_request_filter_accounts_filter::Filter::Datasize(165),
                    ),
                }],
                ..Default::default()
            },
        );
    }
    filters
}

/// BondingCurve account discriminator (`sha256("account:BondingCurve")[..8]`).
const DISC_BONDING_CURVE: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// Explicit account-list filters for the pump.fun bonding curves currently in
/// the registry. A broad owner+disc lane on the pump program would firehose the
/// whole chain's pump trades and starve the subscription (see the token-firehose
/// note on `ACCOUNT_SUBS`); this streams only the curves we actually route.
/// Reserves change on every trade, so keeping them live is what makes BC quotes
/// current. The BondingCurve-disc memcmp is the required non-empty per-lane
/// filter — curve account sizes vary (49 legacy / 151 current), so `dataSize`
/// can't be used.
fn bonding_curve_list_filters(
    registry: &PoolRegistry,
) -> HashMap<String, SubscribeRequestFilterAccounts> {
    let mut keys: Vec<String> = registry
        .iter_pools()
        .filter(|(_, info)| info.dex_name == "Pumpfun BC")
        .map(|(addr, _)| addr.to_string())
        .collect();
    keys.sort_unstable();
    keys.dedup();

    let mut filters = HashMap::new();
    for (i, chunk) in keys.chunks(VAULT_LIST_CHUNK).enumerate() {
        filters.insert(
            format!("pumpfun_bc_list_{i}"),
            SubscribeRequestFilterAccounts {
                account: chunk.to_vec(),
                owner: vec![],
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(subscribe_request_filter_accounts_filter::Filter::Memcmp(
                        SubscribeRequestFilterAccountsFilterMemcmp {
                            offset: 0,
                            data: Some(
                                subscribe_request_filter_accounts_filter_memcmp::Data::Bytes(
                                    DISC_BONDING_CURVE.to_vec(),
                                ),
                            ),
                        },
                    )),
                }],
                ..Default::default()
            },
        );
    }
    filters
}

const STATS_INTERVAL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_DECODING_SIZE: usize = 64 * 1024 * 1024;
/// How often the consume loop wakes to check the idle clock when no
/// messages are arriving.
const WATCHDOG_POLL: Duration = Duration::from_secs(5);

/// Streams account updates from Yellowstone gRPC into `store`, forever.
///
/// Reconnects with exponential backoff on any error. Meant to be spawned
/// as a background tokio task.
pub async fn start_streaming(
    store: Arc<AccountStore>,
    registry: Arc<RwLock<PoolRegistry>>,
) {
    // Vault revalidation runs OFF the consume loop: taking the registry
    // write lock inline stalls the stream whenever cold-start holds the
    // lock for minutes — the server's send buffer fills and it cuts the
    // connection ("Unexpected EOF"). The consume loop only pushes pubkeys
    // into this channel; the drainer batches them per lock acquisition.
    let (vault_tx, mut vault_rx) = tokio::sync::mpsc::unbounded_channel::<Pubkey>();
    {
        let registry = registry.clone();
        let store = store.clone();
        tokio::spawn(async move {
            while let Some(first) = vault_rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = vault_rx.try_recv() {
                    batch.push(more);
                }
                let mut reg = registry.write().await;
                for pubkey in &batch {
                    reg.on_vault_update(pubkey, &store);
                }
            }
        });
    }

    let mut backoff = Duration::from_secs(1);
    // Resume from the slot after the last one seen so updates that landed
    // during the reconnect window are replayed instead of lost.
    let mut resume = true;
    loop {
        let from_slot = match store.last_slot() {
            0 => None,
            s if resume => Some(s + 1),
            _ => None,
        };
        match run_stream(&store, &registry, from_slot, &vault_tx).await {
            Ok(()) => {
                // Stream ended cleanly (server closed) — reset backoff, reconnect.
                backoff = Duration::from_secs(1);
                resume = true;
            }
            Err(e) => {
                eprintln!("gRPC disconnected: {e}, reconnecting in {backoff:?}");
                // Drop from_slot on the attempt after a failed resume — some
                // servers reject it (slot past retention, or unsupported),
                // which would otherwise fail every reconnect forever.
                resume = from_slot.is_none();
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Single connect → subscribe → consume cycle. Returns on stream end or error.
async fn run_stream(
    store: &AccountStore,
    registry: &RwLock<PoolRegistry>,
    from_slot: Option<u64>,
    vault_tx: &tokio::sync::mpsc::UnboundedSender<Pubkey>,
) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("GEYSER_ENDPOINT").expect("GEYSER_ENDPOINT env var must be set");
    let token = std::env::var("GEYSER_TOKEN").ok();

    // Ensure rustls crypto provider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build client. HTTP/2 flow-control tuning (large windows + adaptive
    // sizing) helps high-rate streams on some providers but SILENTLY STARVES
    // the stream on others — Triton/rpcpool accepted the subscribe yet
    // delivered zero account updates until the tuning was removed (verified
    // empirically; the plain builder streams fine). Opt in per provider with
    // GEYSER_HTTP2_TUNING=1.
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.clone())?
        .x_token(token)?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(STREAM_TIMEOUT)
        .max_decoding_message_size(MAX_DECODING_SIZE);

    if std::env::var("GEYSER_HTTP2_TUNING").map(|v| v == "1").unwrap_or(false) {
        builder = builder
            .initial_connection_window_size(64 * 1024 * 1024)
            .initial_stream_window_size(16 * 1024 * 1024)
            .http2_adaptive_window(true)
            .buffer_size(2 * 1024 * 1024)
            .tcp_nodelay(true)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true);
    }

    if endpoint.starts_with("https") {
        builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }

    let mut client = builder.connect().await?;
    eprintln!("gRPC connected to {endpoint}");

    // Subscribe account lanes — one named filter per (program, shape); every
    // filter MUST carry dataSize/memcmp (see ACCOUNT_SUBS for why).
    let mut account_filters: HashMap<String, SubscribeRequestFilterAccounts> = HashMap::new();
    for (name, program, shape) in ACCOUNT_SUBS {
        let filter = match shape {
            Shape::Size(n) => subscribe_request_filter_accounts_filter::Filter::Datasize(*n),
            Shape::Disc(d) => subscribe_request_filter_accounts_filter::Filter::Memcmp(
                SubscribeRequestFilterAccountsFilterMemcmp {
                    offset: 0,
                    data: Some(
                        subscribe_request_filter_accounts_filter_memcmp::Data::Bytes(d.to_vec()),
                    ),
                },
            ),
        };
        account_filters.insert(
            name.to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![program.to_string()],
                filters: vec![SubscribeRequestFilterAccountsFilter { filter: Some(filter) }],
                ..Default::default()
            },
        );
    }

    // Vault balances: explicit account lists for the hot set (built from the
    // registry snapshot at each (re)connect, so the set follows the ranking).
    {
        let reg = registry.read().await;
        let vault_filters = vault_list_filters(&reg);
        let lanes = vault_filters.len();
        let key_count: usize = vault_filters.values().map(|f| f.account.len()).sum();
        eprintln!("gRPC vault list: {key_count} accounts across {lanes} lanes");
        account_filters.extend(vault_filters);

        // pump.fun bonding curves currently routed — keep their reserves live.
        let bc_filters = bonding_curve_list_filters(&reg);
        if !bc_filters.is_empty() {
            let bc_keys: usize = bc_filters.values().map(|f| f.account.len()).sum();
            eprintln!("gRPC pump BC list: {bc_keys} curves across {} lanes", bc_filters.len());
            account_filters.extend(bc_filters);
        }
    }

    let request = SubscribeRequest {
        accounts: account_filters,
        commitment: Some(CommitmentLevel::Confirmed as i32),
        from_slot,
        ..Default::default()
    };

    if let Some(s) = from_slot {
        eprintln!("gRPC subscribing from slot {s}");
    }
    let mut stream = client.subscribe_once(request).await?;

    // Idle watchdog: a dead stream can sit in next() forever without
    // erroring. Track the last real message — Ping/Pong keepalives do NOT
    // count — warn after GEYSER_IDLE_WARN_SECS, force a reconnect after
    // GEYSER_IDLE_TIMEOUT_SECS.
    let warn_idle = Duration::from_secs(
        std::env::var("GEYSER_IDLE_WARN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );
    let idle_timeout = Duration::from_secs(
        std::env::var("GEYSER_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
    );

    let mut updates_count: u64 = 0;
    let mut last_stats = Instant::now();
    let mut last_stats_count: u64 = 0;
    let mut last_data = Instant::now();
    let mut warned_idle = false;

    loop {
        let msg = match tokio::time::timeout(WATCHDOG_POLL, stream.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // stream ended cleanly
            Err(_) => {
                let idle = last_data.elapsed();
                if idle >= idle_timeout {
                    return Err(format!(
                        "stream idle {}s (no data), forcing reconnect",
                        idle.as_secs()
                    )
                    .into());
                }
                if !warned_idle && idle >= warn_idle {
                    eprintln!(
                        "gRPC: no data for {}s, reconnect at {}s",
                        idle.as_secs(),
                        idle_timeout.as_secs()
                    );
                    warned_idle = true;
                }
                continue;
            }
        };
        let update = msg?;

        match update.update_oneof {
            // Keepalives are not data — do not reset the idle clock.
            Some(UpdateOneof::Ping(_)) | Some(UpdateOneof::Pong(_)) => {}
            Some(UpdateOneof::Account(account_update)) => {
                last_data = Instant::now();
                warned_idle = false;
                if let Some(account) = account_update.account {
                    let Ok(pubkey) = Pubkey::try_from(account.pubkey.as_slice()) else {
                        continue;
                    };
                    let Ok(owner) = Pubkey::try_from(account.owner.as_slice()) else {
                        continue;
                    };
                    store.upsert(pubkey, account.data, owner, account.lamports, account_update.slot);
                    updates_count += 1;

                    // Re-validate pools affected by vault balance changes.
                    // Both Token Program and Token-2022 accounts can be vaults.
                    let is_token_account =
                        owner == Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
                        || owner == Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
                    if is_token_account {
                        // Never block the consume loop on the registry lock.
                        let _ = vault_tx.send(pubkey);
                    }
                }
            }
            _ => {
                last_data = Instant::now();
                warned_idle = false;
            }
        }

        // Periodic stats.
        let elapsed = last_stats.elapsed();
        if elapsed >= STATS_INTERVAL {
            let delta = updates_count - last_stats_count;
            let rate = delta as f64 / elapsed.as_secs_f64();
            eprintln!(
                "gRPC: {rate:.0} updates/s, slot {}, store {} accounts",
                store.last_slot(),
                store.len(),
            );
            last_stats = Instant::now();
            last_stats_count = updates_count;
        }
    }

    // Stream ended (None from next()) — treat as clean disconnect.
    Ok(())
}
