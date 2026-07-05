//! Shared utilities for integration tests.

use std::collections::HashMap;
use std::time::Duration;

use solana_pubkey::Pubkey;
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::*;

/// Connect to a Yellowstone gRPC endpoint and subscribe to transactions
/// touching the given program IDs. Returns the update stream.
///
/// Panics if `GEYSER_ENDPOINT` is not set or connection fails.
/// Install rustls crypto provider (required before any TLS connection).
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub async fn geyser_subscribe(
    program_ids: &[&str],
    filter_name: &str,
) -> impl futures::Stream<Item = Result<SubscribeUpdate, yellowstone_grpc_proto::tonic::Status>> {
    init_tls();
    let endpoint = std::env::var("GEYSER_ENDPOINT").expect("GEYSER_ENDPOINT not set");
    let token = std::env::var("GEYSER_TOKEN").ok();

    let builder = GeyserGrpcClient::build_from_shared(endpoint.clone())
        .expect("invalid endpoint")
        .x_token(token)
        .expect("invalid token")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .max_decoding_message_size(64 * 1024 * 1024);

    let builder = if endpoint.starts_with("https") {
        builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .expect("tls config failed")
    } else {
        builder
    };

    let mut client = builder.connect().await.expect("connection failed");

    let request = SubscribeRequest {
        transactions: HashMap::from([(
            filter_name.to_string(),
            SubscribeRequestFilterTransactions {
                account_include: program_ids.iter().map(|s| s.to_string()).collect(),
                account_exclude: vec![],
                account_required: vec![],
                vote: Some(false),
                failed: Some(false),
                signature: None,
            },
        )]),
        commitment: Some(CommitmentLevel::Confirmed as i32),
        ..Default::default()
    };

    client
        .subscribe_once(request)
        .await
        .expect("subscribe failed")
}

/// Build the full account key list from a transaction message and meta,
/// including keys resolved from address lookup tables.
pub fn build_account_keys(msg: &Message, meta: Option<&TransactionStatusMeta>) -> Vec<Pubkey> {
    let mut keys: Vec<Pubkey> = msg
        .account_keys
        .iter()
        .filter_map(|k| <[u8; 32]>::try_from(k.as_slice()).ok().map(Pubkey::from))
        .collect();

    if let Some(meta) = meta {
        for addr in &meta.loaded_writable_addresses {
            if let Ok(b) = <[u8; 32]>::try_from(addr.as_slice()) {
                keys.push(Pubkey::from(b));
            }
        }
        for addr in &meta.loaded_readonly_addresses {
            if let Ok(b) = <[u8; 32]>::try_from(addr.as_slice()) {
                keys.push(Pubkey::from(b));
            }
        }
    }

    keys
}

/// Truncate a string for display (first 6 + last 4 chars, joined by `..`).
pub fn trunc(s: &str) -> String {
    if s.len() > 12 {
        format!("{}..{}", &s[..6], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// Minimal base58 encoder (avoids adding the bs58 crate).
pub fn bs58_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if data.is_empty() {
        return String::new();
    }
    let mut digits = vec![0u8];
    for &byte in data {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut result = String::new();
    for &b in data {
        if b == 0 {
            result.push('1');
        } else {
            break;
        }
    }
    for &d in digits.iter().rev() {
        result.push(ALPHABET[d as usize] as char);
    }
    result
}


/// Build a Geyser client. Optionally request a snapshot of current state.
pub async fn geyser_client(snapshot: bool) -> GeyserGrpcClient<impl yellowstone_grpc_client::Interceptor + Clone> {
    init_tls();
    let endpoint = std::env::var("GEYSER_ENDPOINT").expect("GEYSER_ENDPOINT not set");
    let token = std::env::var("GEYSER_TOKEN").ok();

    let builder = GeyserGrpcClient::build_from_shared(endpoint.clone())
        .expect("invalid endpoint")
        .x_token(token)
        .expect("invalid token")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .max_decoding_message_size(64 * 1024 * 1024)
        .set_x_request_snapshot(snapshot);

    let builder = if endpoint.starts_with("https") {
        builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .expect("tls config failed")
    } else {
        builder
    };

    builder.connect().await.expect("connection failed")
}