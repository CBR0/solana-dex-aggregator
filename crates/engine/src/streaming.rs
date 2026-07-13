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

/// DEX program IDs + Token Program to subscribe to.
const OWNER_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium V4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", // Meteora DAMM V1
    "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",   // Meteora DAMM V2
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",   // Meteora DLMM
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",   // Pumpfun AMM
    "24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi",  // Meteora Dynamic Vault (DAMM V1 vault states)
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",   // Orca Whirlpool
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",   // Token Program
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",   // Token-2022
];

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
        match run_stream(&store, &registry, from_slot).await {
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
) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("GEYSER_ENDPOINT").expect("GEYSER_ENDPOINT env var must be set");
    let token = std::env::var("GEYSER_TOKEN").ok();

    // Ensure rustls crypto provider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build client. Default HTTP/2 flow-control windows (64KB) choke
    // high-rate Yellowstone streams — the server stalls waiting for window
    // updates and the client falls minutes behind. Large windows + adaptive
    // sizing let the server send at line rate.
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.clone())?
        .x_token(token)?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(STREAM_TIMEOUT)
        .max_decoding_message_size(MAX_DECODING_SIZE)
        .initial_connection_window_size(64 * 1024 * 1024)
        .initial_stream_window_size(16 * 1024 * 1024)
        .http2_adaptive_window(true)
        .buffer_size(2 * 1024 * 1024)
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(15))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true);

    if endpoint.starts_with("https") {
        builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }

    let mut client = builder.connect().await?;
    eprintln!("gRPC connected to {endpoint}");

    // Subscribe to all DEX + Token Program account updates.
    let request = SubscribeRequest {
        accounts: HashMap::from([(
            "dex_accounts".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: OWNER_PROGRAMS.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )]),
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
                        let mut reg = registry.write().await;
                        reg.on_vault_update(&pubkey, store);
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
