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
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",   // Token Program
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",   // Token-2022
];

const STATS_INTERVAL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_DECODING_SIZE: usize = 64 * 1024 * 1024;

/// Streams account updates from Yellowstone gRPC into `store`, forever.
///
/// Reconnects with exponential backoff on any error. Meant to be spawned
/// as a background tokio task.
pub async fn start_streaming(
    store: Arc<AccountStore>,
    registry: Arc<RwLock<PoolRegistry>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_stream(&store, &registry).await {
            Ok(()) => {
                // Stream ended cleanly (server closed) — reset backoff, reconnect.
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                eprintln!("gRPC disconnected: {e}, reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Single connect → subscribe → consume cycle. Returns on stream end or error.
async fn run_stream(store: &AccountStore, registry: &RwLock<PoolRegistry>) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("GEYSER_ENDPOINT").expect("GEYSER_ENDPOINT env var must be set");
    let token = std::env::var("GEYSER_TOKEN").ok();

    // Ensure rustls crypto provider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build client.
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.clone())?
        .x_token(token)?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(STREAM_TIMEOUT)
        .max_decoding_message_size(MAX_DECODING_SIZE);

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
        ..Default::default()
    };

    let mut stream = client.subscribe_once(request).await?;

    let mut updates_count: u64 = 0;
    let mut last_stats = Instant::now();
    let mut last_stats_count: u64 = 0;

    while let Some(msg) = stream.next().await {
        let update = msg?;

        if let Some(UpdateOneof::Account(account_update)) = update.update_oneof {
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
