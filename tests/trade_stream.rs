//! Integration test: stream live swap/trade transactions across all DEX pools via Yellowstone gRPC.
//!
//! Subscribes to transactions touching all 6 DEX program IDs, identifies swap instructions
//! by their discriminators, and extracts trade data from token balance changes.
//!
//! Requires `GEYSER_ENDPOINT` (and optionally `GEYSER_TOKEN`) in `.env` or environment.
//!
//! Run: `cargo test --test trade_stream -- --nocapture`

mod helpers;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::StreamExt;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::*;

use helpers::{bs58_encode, build_account_keys, trunc};

// DEX program IDs — canonical constants from each crate.
const RAYDIUM_V4: &str = raydium_amm_v4::RAYDIUM_LIQUIDITY_POOL_V4;
const RAYDIUM_CLMM: &str = raydium_clmm::RAYDIUM_CLMM;
const METEORA_DAMM: &str = meteora_damm::METEORA_DYNAMIC_AMM;
const METEORA_DAMM_V2: &str = meteora_damm::METEORA_DYNAMIC_AMM_V2;
const METEORA_DLMM: &str = meteora_dlmm::METEORA_DYNAMIC_LMM;
const PUMPFUN_AMM: &str = pumpfun_amm::PUMPFUN_AMM_PROGRAM;

const ALL_PROGRAMS: &[&str] = &[
    RAYDIUM_V4,
    RAYDIUM_CLMM,
    METEORA_DAMM,
    METEORA_DAMM_V2,
    METEORA_DLMM,
    PUMPFUN_AMM,
];

// ---------------------------------------------------------------------------
// Swap instruction discriminators
// ---------------------------------------------------------------------------

/// Identify which DEX and swap type an instruction belongs to.
fn identify_swap(program_id: &[u8], ix_data: &[u8]) -> Option<(&'static str, &'static str)> {
    if ix_data.is_empty() {
        return None;
    }

    let pid = Pubkey::from(<[u8; 32]>::try_from(program_id).ok()?);
    let pid_str = pid.to_string();

    match pid_str.as_str() {
        RAYDIUM_V4 => {
            // 1-byte discriminator: 9 = swap_base_in, 11 = swap_base_out
            match ix_data[0] {
                9 => Some(("Raydium V4", "swap_base_in")),
                11 => Some(("Raydium V4", "swap_base_out")),
                _ => None,
            }
        }
        RAYDIUM_CLMM => {
            // 8-byte Anchor discriminator
            if ix_data.len() >= 8 && ix_data[..8] == [43, 4, 237, 11, 26, 201, 30, 98] {
                Some(("Raydium CLMM", "swap"))
            } else {
                None
            }
        }
        METEORA_DAMM => {
            if ix_data.len() >= 8
                && ix_data[..8] == [248, 198, 158, 145, 225, 117, 135, 200]
            {
                Some(("Meteora DAMM", "swap"))
            } else {
                None
            }
        }
        METEORA_DAMM_V2 => {
            if ix_data.len() >= 8
                && ix_data[..8] == [248, 198, 158, 145, 225, 117, 135, 200]
            {
                Some(("Meteora DAMM V2", "swap"))
            } else {
                None
            }
        }
        METEORA_DLMM => {
            if ix_data.len() >= 8 && ix_data[..8] == [65, 75, 63, 76, 235, 91, 91, 136] {
                Some(("Meteora DLMM", "swap"))
            } else {
                None
            }
        }
        PUMPFUN_AMM => {
            if ix_data.len() >= 8 {
                if ix_data[..8] == [102, 6, 61, 18, 1, 218, 235, 234] {
                    Some(("Pumpfun AMM", "buy"))
                } else if ix_data[..8] == [51, 230, 133, 164, 1, 127, 131, 173] {
                    Some(("Pumpfun AMM", "sell"))
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Trade extraction from transaction
// ---------------------------------------------------------------------------

struct Trade {
    dex: &'static str,
    kind: &'static str,
    pool: String,
    trader: String,
    signature: String,
    /// Token balance changes: (mint, amount_change). Positive = received, negative = sent.
    balance_changes: Vec<(String, i128)>,
}

fn extract_trades(info: &SubscribeUpdateTransactionInfo) -> Vec<Trade> {
    let mut trades = vec![];
    let signature = bs58_encode(&info.signature);

    let Some(tx) = &info.transaction else {
        return trades;
    };
    let Some(msg) = &tx.message else {
        return trades;
    };

    let account_keys = build_account_keys(msg, info.meta.as_ref());

    // Scan outer instructions for swaps.
    for ix in &msg.instructions {
        let Some(program_key) = account_keys.get(ix.program_id_index as usize) else {
            continue;
        };
        let Some((dex, kind)) = identify_swap(&program_key.to_bytes(), &ix.data) else {
            continue;
        };

        // Pool address is typically the first account in the instruction.
        let pool = ix
            .accounts
            .first()
            .and_then(|&idx| account_keys.get(idx as usize))
            .map(|k| k.to_string())
            .unwrap_or_else(|| "unknown".into());

        // Trader is usually the signer (first account key in the tx).
        let trader = account_keys
            .first()
            .map(|k| k.to_string())
            .unwrap_or_else(|| "unknown".into());

        let balance_changes = compute_balance_changes(info);

        trades.push(Trade {
            dex,
            kind,
            pool,
            trader,
            signature: signature.clone(),
            balance_changes,
        });
    }

    // Also scan inner instructions (CPI swaps).
    if let Some(meta) = &info.meta {
        for inner_ixs in &meta.inner_instructions {
            for inner_ix in &inner_ixs.instructions {
                let Some(program_key) = account_keys.get(inner_ix.program_id_index as usize)
                else {
                    continue;
                };
                let Some((dex, kind)) = identify_swap(&program_key.to_bytes(), &inner_ix.data)
                else {
                    continue;
                };

                let pool = inner_ix
                    .accounts
                    .first()
                    .and_then(|&idx| account_keys.get(idx as usize))
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "unknown".into());

                let trader = account_keys
                    .first()
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "unknown".into());

                // Avoid duplicating balance changes for the same tx.
                let balance_changes = if trades.iter().any(|t| t.signature == signature) {
                    vec![]
                } else {
                    compute_balance_changes(info)
                };

                trades.push(Trade {
                    dex,
                    kind,
                    pool,
                    trader,
                    signature: signature.clone(),
                    balance_changes,
                });
            }
        }
    }

    trades
}

/// Compute token balance changes from pre/post token balances in the transaction meta.
fn compute_balance_changes(info: &SubscribeUpdateTransactionInfo) -> Vec<(String, i128)> {
    let Some(meta) = &info.meta else {
        return vec![];
    };

    let mut changes: HashMap<String, i128> = HashMap::new();

    for post in &meta.post_token_balances {
        let mint = &post.mint;
        let post_amount = post
            .ui_token_amount
            .as_ref()
            .and_then(|a| a.amount.parse::<i128>().ok())
            .unwrap_or(0);

        let pre_amount = meta
            .pre_token_balances
            .iter()
            .find(|pre| pre.account_index == post.account_index)
            .and_then(|pre| {
                pre.ui_token_amount
                    .as_ref()
                    .and_then(|a| a.amount.parse::<i128>().ok())
            })
            .unwrap_or(0);

        let diff = post_amount - pre_amount;
        if diff != 0 {
            *changes.entry(mint.clone()).or_default() += diff;
        }
    }

    let mut result: Vec<(String, i128)> = changes.into_iter().collect();
    result.sort_by_key(|(_, v)| std::cmp::Reverse(v.unsigned_abs()));
    result
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Stream live DEX trades for 30 seconds and print each one.
///
/// Run: `cargo test --test trade_stream -- --nocapture`
#[tokio::test]
async fn stream_dex_trades() {
    let _ = dotenvy::dotenv();

    let endpoint = std::env::var("GEYSER_ENDPOINT").unwrap_or_default();
    if endpoint.is_empty() {
        println!("GEYSER_ENDPOINT not set, skipping");
        return;
    }

    println!("Connecting to {endpoint} ...");

    let mut stream = helpers::geyser_subscribe(ALL_PROGRAMS, "dex_swaps").await;

    println!("Subscribed. Streaming trades for 30 seconds...\n");
    println!(
        "{:<16} {:<6} {:<12} {:<12} {:<12} {}",
        "DEX", "TYPE", "POOL", "TRADER", "SIG", "BALANCE CHANGES"
    );
    println!("{}", "-".repeat(100));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut count = 0u32;

    while let Some(msg) = stream.next().await {
        if Instant::now() > deadline {
            break;
        }
        let Ok(msg) = msg else { continue };

        if let Some(subscribe_update::UpdateOneof::Transaction(tx_update)) = msg.update_oneof {
            if let Some(info) = tx_update.transaction {
                for trade in extract_trades(&info) {
                    count += 1;

                    // Format balance changes compactly.
                    let changes: String = trade
                        .balance_changes
                        .iter()
                        .take(3)
                        .map(|(mint, amount)| {
                            let sign = if *amount > 0 { "+" } else { "" };
                            format!("{}{} {}", sign, amount, trunc(mint))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");

                    println!(
                        "{:<16} {:<6} {:<12} {:<12} {:<12} {}",
                        trade.dex,
                        trade.kind,
                        trunc(&trade.pool),
                        trunc(&trade.trader),
                        trunc(&trade.signature),
                        if changes.is_empty() {
                            "(see outer tx)".to_string()
                        } else {
                            changes
                        },
                    );
                }
            }
        }
    }

    println!("\n{} trade(s) observed in 30 seconds.", count);
}
