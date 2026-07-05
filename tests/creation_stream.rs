//! Integration test: stream live token creation AND pool creation events via Yellowstone gRPC.
//!
//! Subscribes to transactions touching:
//! - SPL Token / Token-2022 programs (for new mints)
//! - All 6 DEX programs (for new pool creation)
//!
//! Requires `GEYSER_ENDPOINT` (and optionally `GEYSER_TOKEN`) in `.env` or environment.
//!
//! Run: `cargo test --test creation_stream -- --nocapture`

mod helpers;

use std::time::{Duration, Instant};

use futures::StreamExt;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::*;

use helpers::{bs58_encode, build_account_keys, trunc};

// Program IDs — canonical constants from each crate.
const TOKEN_PROGRAM: &str = thunder_core::TOKEN_PROGRAM;
const TOKEN_2022_PROGRAM: &str = thunder_core::TOKEN_PROGRAM_2022;

const RAYDIUM_V4: &str = raydium_amm_v4::RAYDIUM_LIQUIDITY_POOL_V4;
const RAYDIUM_CLMM: &str = raydium_clmm::RAYDIUM_CLMM;
const METEORA_DAMM: &str = meteora_damm::METEORA_DYNAMIC_AMM;
const METEORA_DAMM_V2: &str = meteora_damm::METEORA_DYNAMIC_AMM_V2;
const METEORA_DLMM: &str = meteora_dlmm::METEORA_DYNAMIC_LMM;
const PUMPFUN_AMM: &str = pumpfun_amm::PUMPFUN_AMM_PROGRAM;

/// All programs we subscribe to.
const ALL_PROGRAMS: &[&str] = &[
    TOKEN_PROGRAM,
    TOKEN_2022_PROGRAM,
    RAYDIUM_V4,
    RAYDIUM_CLMM,
    METEORA_DAMM,
    METEORA_DAMM_V2,
    METEORA_DLMM,
    PUMPFUN_AMM,
];

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

enum CreationEvent {
    Token(TokenCreated),
    Pool(PoolCreated),
}

struct TokenCreated {
    mint: Pubkey,
    decimals: u8,
    authority: Option<Pubkey>,
    program: &'static str, // "SPL" or "T2022"
    signature: String,
}

struct PoolCreated {
    dex: &'static str,
    instruction: &'static str,
    pool: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    creator: Pubkey,
    signature: String,
}

// ---------------------------------------------------------------------------
// Pool creation discriminators
// ---------------------------------------------------------------------------

/// Pool creation instruction descriptor.
struct PoolCreateDesc {
    dex: &'static str,
    instruction: &'static str,
    disc: &'static [u8],
    pool_idx: usize,
    mint_a_idx: usize,
    mint_b_idx: usize,
}

/// All known pool creation instructions per program.
fn pool_create_descriptors(program_id: &str) -> &'static [PoolCreateDesc] {
    match program_id {
        RAYDIUM_V4 => &[PoolCreateDesc {
            dex: "Raydium V4",
            instruction: "Initialize2",
            disc: &[1],
            pool_idx: 4,
            mint_a_idx: 8,
            mint_b_idx: 9,
        }],
        RAYDIUM_CLMM => &[PoolCreateDesc {
            dex: "Raydium CLMM",
            instruction: "create_pool",
            disc: &[233, 146, 209, 142, 207, 104, 64, 188],
            pool_idx: 2,
            mint_a_idx: 3,
            mint_b_idx: 4,
        }],
        METEORA_DAMM => &[
            PoolCreateDesc {
                dex: "Meteora DAMM",
                instruction: "init_permissionless_pool",
                disc: &[118, 173, 41, 157, 173, 72, 97, 103],
                pool_idx: 0,
                mint_a_idx: 2,
                mint_b_idx: 3,
            },
            PoolCreateDesc {
                dex: "Meteora DAMM",
                instruction: "init_cp_pool_config2",
                disc: &[48, 149, 220, 130, 61, 11, 9, 178],
                pool_idx: 0,
                mint_a_idx: 3,
                mint_b_idx: 4,
            },
        ],
        METEORA_DAMM_V2 => &[PoolCreateDesc {
            dex: "Meteora DAMM V2",
            instruction: "initialize_pool",
            disc: &[95, 180, 10, 172, 84, 174, 232, 40],
            pool_idx: 6,
            mint_a_idx: 8,
            mint_b_idx: 9,
        }],
        METEORA_DLMM => &[
            PoolCreateDesc {
                dex: "Meteora DLMM",
                instruction: "initialize_lb_pair",
                disc: &[45, 154, 237, 210, 221, 15, 166, 92],
                pool_idx: 0,
                mint_a_idx: 2,
                mint_b_idx: 3,
            },
            PoolCreateDesc {
                dex: "Meteora DLMM",
                instruction: "init_cust_perm_lb_pair2",
                disc: &[243, 73, 129, 126, 51, 19, 241, 107],
                pool_idx: 0,
                mint_a_idx: 2,
                mint_b_idx: 3,
            },
        ],
        PUMPFUN_AMM => &[PoolCreateDesc {
            dex: "Pumpfun AMM",
            instruction: "create_pool",
            disc: &[233, 146, 209, 142, 207, 104, 64, 188],
            pool_idx: 0,
            mint_a_idx: 3,
            mint_b_idx: 4,
        }],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Transaction parsing
// ---------------------------------------------------------------------------

fn extract_events(info: &SubscribeUpdateTransactionInfo) -> Vec<CreationEvent> {
    let mut events = vec![];
    let signature = bs58_encode(&info.signature);

    let Some(tx) = &info.transaction else {
        return events;
    };
    let Some(msg) = &tx.message else {
        return events;
    };

    let account_keys = build_account_keys(msg, info.meta.as_ref());

    // Process all instructions (outer + inner).
    let mut all_instructions: Vec<(u32, &[u8], &[u8])> = vec![];

    for ix in &msg.instructions {
        all_instructions.push((ix.program_id_index, &ix.accounts, &ix.data));
    }
    if let Some(meta) = &info.meta {
        for inner_ixs in &meta.inner_instructions {
            for ix in &inner_ixs.instructions {
                all_instructions.push((ix.program_id_index, &ix.accounts, &ix.data));
            }
        }
    }

    for (program_id_index, accounts, data) in all_instructions {
        let Some(&program_key) = account_keys.get(program_id_index as usize) else {
            continue;
        };

        // Check for token creation (InitializeMint / InitializeMint2).
        if let Some(token_event) =
            try_parse_token_creation(&program_key, accounts, data, &account_keys, &signature)
        {
            events.push(CreationEvent::Token(token_event));
            continue;
        }

        // Check for pool creation across all DEXs.
        if let Some(pool_event) =
            try_parse_pool_creation(&program_key, accounts, data, &account_keys, &signature)
        {
            events.push(CreationEvent::Pool(pool_event));
        }
    }

    events
}

fn try_parse_token_creation(
    program_key: &Pubkey,
    accounts: &[u8],
    data: &[u8],
    account_keys: &[Pubkey],
    signature: &str,
) -> Option<TokenCreated> {
    let token_id = Pubkey::from_str_const(TOKEN_PROGRAM);
    let token_2022_id = Pubkey::from_str_const(TOKEN_2022_PROGRAM);

    let program_label = if *program_key == token_id {
        "SPL"
    } else if *program_key == token_2022_id {
        "T2022"
    } else {
        return None;
    };

    let disc = *data.first()?;
    // 0 = InitializeMint, 20 = InitializeMint2
    if disc != 0 && disc != 20 {
        return None;
    }
    if data.len() < 35 {
        return None;
    }

    let decimals = data[1];
    let authority = <[u8; 32]>::try_from(&data[2..34])
        .ok()
        .map(Pubkey::from);

    let mint_index = *accounts.first()? as usize;
    let mint = *account_keys.get(mint_index)?;

    Some(TokenCreated {
        mint,
        decimals,
        authority,
        program: program_label,
        signature: signature.to_string(),
    })
}

fn try_parse_pool_creation(
    program_key: &Pubkey,
    accounts: &[u8],
    data: &[u8],
    account_keys: &[Pubkey],
    signature: &str,
) -> Option<PoolCreated> {
    let program_str = program_key.to_string();
    let descriptors = pool_create_descriptors(&program_str);

    for desc in descriptors {
        if data.len() < desc.disc.len() {
            continue;
        }
        if &data[..desc.disc.len()] != desc.disc {
            continue;
        }

        // Resolve account indices through the instruction's accounts array.
        let resolve = |idx: usize| -> Option<Pubkey> {
            let account_index = *accounts.get(idx)? as usize;
            account_keys.get(account_index).copied()
        };

        let pool = resolve(desc.pool_idx)?;
        let mint_a = resolve(desc.mint_a_idx)?;
        let mint_b = resolve(desc.mint_b_idx)?;

        // Creator is typically the transaction signer (first account key).
        let creator = *account_keys.first()?;

        return Some(PoolCreated {
            dex: desc.dex,
            instruction: desc.instruction,
            pool,
            mint_a,
            mint_b,
            creator,
            signature: signature.to_string(),
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Stream live token and pool creation events for 60 seconds.
#[tokio::test]
async fn stream_token_and_pool_creations() {
    let _ = dotenvy::dotenv();

    let endpoint = std::env::var("GEYSER_ENDPOINT").unwrap_or_default();
    if endpoint.is_empty() {
        println!("GEYSER_ENDPOINT not set, skipping");
        return;
    }

    println!("Connecting to {endpoint} ...");

    let mut stream = helpers::geyser_subscribe(ALL_PROGRAMS, "creations").await;

    println!("Subscribed. Streaming for 60 seconds...\n");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut token_count = 0u32;
    let mut pool_count = 0u32;

    while let Some(msg) = stream.next().await {
        if Instant::now() > deadline {
            break;
        }
        let Ok(msg) = msg else { continue };

        if let Some(subscribe_update::UpdateOneof::Transaction(tx_update)) = msg.update_oneof {
            if let Some(info) = tx_update.transaction {
                for event in extract_events(&info) {
                    match event {
                        CreationEvent::Token(t) => {
                            token_count += 1;
                            println!(
                                "[TOKEN #{token_count:>4}]  {prog:<5}  mint={mint}  dec={dec}  auth={auth}  sig={sig}",
                                prog = t.program,
                                mint = t.mint,
                                dec = t.decimals,
                                auth = t.authority.map(|p| trunc(&p.to_string())).unwrap_or("-".into()),
                                sig = trunc(&t.signature),
                            );
                        }
                        CreationEvent::Pool(p) => {
                            pool_count += 1;
                            println!(
                                "[POOL  #{pool_count:>4}]  {dex:<16}  {ix:<24}  pool={pool}  {mint_a} / {mint_b}  creator={creator}  sig={sig}",
                                dex = p.dex,
                                ix = p.instruction,
                                pool = trunc(&p.pool.to_string()),
                                mint_a = trunc(&p.mint_a.to_string()),
                                mint_b = trunc(&p.mint_b.to_string()),
                                creator = trunc(&p.creator.to_string()),
                                sig = trunc(&p.signature),
                            );
                        }
                    }
                }
            }
        }
    }

    println!("\nDone. {token_count} token(s) + {pool_count} pool(s) created in 60 seconds.");
}
