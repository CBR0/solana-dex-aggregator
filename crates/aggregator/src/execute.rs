//! Route → execution bridge.
//!
//! Turns an aggregator [`Route`] into on-chain swap instructions using
//! `solroute-executor`, pulling per-DEX accounts out of the cached pool state
//! the index already holds. Single-hop routes only for now — multi-hop atomic
//! execution (intermediate ATAs + chained slippage) is a follow-up.

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_rpc_client_api::response::RpcSimulateTransactionResult;

use solroute_core::{GenericError, TOKEN_PROGRAM, TOKEN_PROGRAM_2022};
use solroute_executor::alt;
use solroute_executor::meteora_damm_v1::{self, DammV1Accounts};
use solroute_executor::meteora_dlmm::{self, DlmmAccounts};
use solroute_executor::meteora_damm_v2::{self, DammV2Accounts};
use solroute_executor::pumpswap::{self, PumpSwapAccounts};
use solroute_executor::pumpfun_bc::{self, PumpBcAccounts};
use solroute_executor::meteora_dbc::{self as dbc_exec, DbcAccounts};
use solroute_executor::bonk::{self as bonk_exec, BonkAccounts};
use solroute_executor::raydium_amm_v4;
use solroute_executor::raydium_clmm;
use solroute_executor::orca_whirlpool as orca_exec;
use solroute_executor::submit;
use solroute_executor::{SwapLeg, SwapOptions};

use crate::cache::CachedPool;
use crate::pool_index::PoolIndex;
use crate::types::{Route, RouteHop};

/// Apply a slippage floor (bps) to a quoted output amount.
fn min_out_with_slippage(quoted_out: u64, slippage_bps: u64) -> u64 {
    let bps = slippage_bps.min(10_000);
    (quoted_out as u128 * (10_000 - bps) as u128 / 10_000) as u64
}

/// Resolve each mint's owning token program (SPL Token vs Token-2022) from its
/// mint account owner. One `getMultipleAccounts` for all mints; anything that
/// isn't a recognized token program (or fails to fetch) falls back to legacy
/// SPL Token. This is what makes Token-2022 pools executable.
async fn resolve_token_programs(rpc: &RpcClient, mints: &[Pubkey]) -> Vec<Pubkey> {
    let legacy = Pubkey::from_str_const(TOKEN_PROGRAM);
    let token_2022 = Pubkey::from_str_const(TOKEN_PROGRAM_2022);
    let accounts = rpc.get_multiple_accounts(mints).await.unwrap_or_default();
    mints
        .iter()
        .enumerate()
        .map(|(i, _)| {
            accounts
                .get(i)
                .and_then(|maybe| maybe.as_ref())
                .map(|acc| acc.owner)
                .filter(|owner| *owner == legacy || *owner == token_2022)
                .unwrap_or(legacy)
        })
        .collect()
}

/// Observe bonk.fun's two fee-vault remaining accounts (platform + creator, both
/// WSOL) from a recent swap on the pool. The current LaunchLab program requires
/// them but doesn't expose their derivation from public account data; every bonk
/// buy/sell carries them at instruction account indices 16 and 17. Returns
/// `(platform_fee_vault, creator_fee_vault)`.
pub async fn observe_bonk_fee_vaults(rpc: &RpcClient, pool: &Pubkey) -> Option<(Pubkey, Pubkey)> {
    use solana_rpc_client_api::request::RpcRequest;
    let bonk_program = bonk_exec::PROGRAM_ID.to_string();
    let sigs: serde_json::Value = rpc
        .send(
            RpcRequest::Custom { method: "getSignaturesForAddress" },
            serde_json::json!([pool.to_string(), {"limit": 25}]),
        )
        .await
        .ok()?;
    for s in sigs.as_array()?.iter() {
        let sig = s.get("signature")?.as_str()?;
        let tx: serde_json::Value = match rpc
            .send(
                RpcRequest::Custom { method: "getTransaction" },
                serde_json::json!([sig, {"maxSupportedTransactionVersion": 0, "encoding": "json"}]),
            )
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg = match tx.get("transaction").and_then(|t| t.get("message")) {
            Some(m) => m,
            None => continue,
        };
        // Static keys + ALT-loaded addresses, in on-chain index order.
        let mut keys: Vec<String> = msg
            .get("accountKeys")
            .and_then(|k| k.as_array())
            .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if let Some(loaded) = tx.get("meta").and_then(|m| m.get("loadedAddresses")) {
            for field in ["writable", "readonly"] {
                if let Some(arr) = loaded.get(field).and_then(|v| v.as_array()) {
                    keys.extend(arr.iter().filter_map(|x| x.as_str().map(String::from)));
                }
            }
        }
        let Some(ixs) = msg.get("instructions").and_then(|i| i.as_array()) else { continue };
        for ix in ixs {
            let pid_idx = ix.get("programIdIndex").and_then(|v| v.as_u64());
            let is_bonk = pid_idx
                .and_then(|i| keys.get(i as usize))
                .map(|k| k.as_str() == bonk_program.as_str())
                .unwrap_or(false);
            if !is_bonk {
                continue;
            }
            // Any bonk buy/sell carries 18 accounts: fee vaults at 16 and 17.
            let Some(accts) = ix.get("accounts").and_then(|a| a.as_array()) else { continue };
            if accts.len() < 18 {
                continue;
            }
            let idx16 = accts[16].as_u64()? as usize;
            let idx17 = accts[17].as_u64()? as usize;
            let v16 = Pubkey::from_str(keys.get(idx16)?).ok()?;
            let v17 = Pubkey::from_str(keys.get(idx17)?).ok()?;
            return Some((v16, v17));
        }
    }
    None
}

/// Build swap instructions for a single routed hop. Async because Raydium V4
/// fetches its Serum market from RPC.
pub async fn build_hop_instructions(
    rpc: &RpcClient,
    index: &PoolIndex,
    hop: &RouteHop,
    payer: Pubkey,
    slippage_bps: u64,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    let entry = index
        .get_pool(&hop.pool_address)
        .ok_or_else(|| GenericError::from(format!("pool {} not in index", hop.pool_address)))?;
    let pool_pubkey = Pubkey::from_str(&hop.pool_address)
        .map_err(|e| GenericError::from(format!("bad pool address: {e}")))?;

    let cached: CachedPool = bincode::deserialize(&entry.cached_data)
        .map_err(|e| GenericError::from(format!("cannot decode cached pool: {e}")))?;

    let leg = SwapLeg {
        payer,
        input_mint: hop.input_mint,
        output_mint: hop.output_mint,
        amount_in: hop.input_amount,
        min_amount_out: min_out_with_slippage(hop.output_amount, slippage_bps),
    };

    match cached {
        CachedPool::OrcaWhirlpool { pool, .. } => {
            orca_exec::build_swap(&pool, pool_pubkey, &leg, opts)
        }
        CachedPool::MeteoraDAMMV2 { pool, .. } => {
            let progs = resolve_token_programs(rpc, &[pool.token_a_mint, pool.token_b_mint]).await;
            let accounts = DammV2Accounts {
                pool: pool_pubkey,
                token_a_mint: pool.token_a_mint,
                token_b_mint: pool.token_b_mint,
                token_a_vault: pool.token_a_vault,
                token_b_vault: pool.token_b_vault,
                token_a_program: progs[0],
                token_b_program: progs[1],
            };
            meteora_damm_v2::build_swap(&accounts, &leg, None, opts)
        }
        CachedPool::PumpfunAmm { pool, .. } => {
            let progs = resolve_token_programs(rpc, &[pool.base_mint, pool.quote_mint]).await;
            let accounts = PumpSwapAccounts {
                pool: pool_pubkey,
                base_mint: pool.base_mint,
                quote_mint: pool.quote_mint,
                pool_base_token_account: pool.pool_base_token_account,
                pool_quote_token_account: pool.pool_quote_token_account,
                coin_creator: pool.coin_creator,
                base_token_program: progs[0],
                quote_token_program: progs[1],
                is_cashback: false,
                is_mayhem: false,
            };
            pumpswap::build_swap(&accounts, &leg, opts)
        }
        CachedPool::PumpfunBondingCurve { pool, .. } => {
            // Only the token side has a mint account; SOL settles natively.
            let progs = resolve_token_programs(rpc, &[pool.mint]).await;
            let accounts = PumpBcAccounts {
                mint: pool.mint,
                bonding_curve: pool.bonding_curve,
                creator: pool.curve.creator,
            };
            pumpfun_bc::build_swap(&accounts, &leg, progs[0], opts)
        }
        CachedPool::MeteoraDBC { pool, config, .. } => {
            let progs = resolve_token_programs(rpc, &[pool.base_mint, config.quote_mint]).await;
            let accounts = DbcAccounts {
                pool: pool_pubkey,
                config: pool.config,
                base_mint: pool.base_mint,
                quote_mint: config.quote_mint,
                base_vault: pool.base_vault,
                quote_vault: pool.quote_vault,
            };
            dbc_exec::build_swap(&accounts, &leg, progs[0], progs[1], opts)
        }
        CachedPool::Bonk { pool, .. } => {
            // The current LaunchLab program needs 2 fee-vault remaining accounts
            // that aren't derivable from public account data — observe them from a
            // recent swap on this pool.
            let (platform_fee_vault, creator_fee_vault) = observe_bonk_fee_vaults(rpc, &pool_pubkey)
                .await
                .ok_or_else(|| GenericError::from(
                    "bonk: could not observe fee vaults from a recent swap on this pool",
                ))?;
            let progs = resolve_token_programs(rpc, &[pool.base_mint, pool.quote_mint]).await;
            let accounts = BonkAccounts {
                pool_state: pool_pubkey,
                global_config: pool.global_config,
                platform_config: pool.platform_config,
                base_mint: pool.base_mint,
                quote_mint: pool.quote_mint,
                base_vault: pool.base_vault,
                quote_vault: pool.quote_vault,
            };
            bonk_exec::build_swap(&accounts, &leg, progs[0], progs[1], platform_fee_vault, creator_fee_vault, opts)
        }
        CachedPool::RaydiumV4 { pool, .. } => {
            raydium_amm_v4::build_swap(rpc, &pool, pool_pubkey, &leg, opts).await
        }
        CachedPool::MeteoraDAMMV1 { pool, .. } => {
            // token_vault + lp_mint must come from each vault's on-chain state
            // (not the naive PDA — older vaults differ).
            let (a_token_vault, a_vault_lp_mint) =
                meteora_damm_v1::fetch_vault_accounts(rpc, &pool.a_vault).await?;
            let (b_token_vault, b_vault_lp_mint) =
                meteora_damm_v1::fetch_vault_accounts(rpc, &pool.b_vault).await?;
            let accounts = DammV1Accounts {
                pool: pool_pubkey,
                token_a_mint: pool.token_a_mint,
                token_b_mint: pool.token_b_mint,
                a_vault: pool.a_vault,
                b_vault: pool.b_vault,
                a_token_vault,
                b_token_vault,
                a_vault_lp_mint,
                b_vault_lp_mint,
                a_vault_lp: pool.a_vault_lp,
                b_vault_lp: pool.b_vault_lp,
                protocol_token_a_fee: pool.protocol_token_a_fee,
                protocol_token_b_fee: pool.protocol_token_b_fee,
            };
            meteora_damm_v1::build_swap(&accounts, &leg, opts)
        }
        CachedPool::MeteoraDLMM { pool, .. } => {
            let progs = resolve_token_programs(rpc, &[pool.token_x_mint, pool.token_y_mint]).await;
            let accounts = DlmmAccounts {
                lb_pair: pool_pubkey,
                token_x_mint: pool.token_x_mint,
                token_y_mint: pool.token_y_mint,
                reserve_x: pool.reserve_x,
                reserve_y: pool.reserve_y,
                oracle: pool.oracle,
                active_id: pool.active_id,
                token_x_program: progs[0],
                token_y_program: progs[1],
            };
            meteora_dlmm::build_swap(&accounts, &leg, opts)
        }
        CachedPool::RaydiumClmm { pool, .. } => {
            let progs = resolve_token_programs(rpc, &[pool.token_mint_0, pool.token_mint_1]).await;
            let (in_prog, out_prog) = if leg.input_mint == pool.token_mint_0 {
                (progs[0], progs[1])
            } else {
                (progs[1], progs[0])
            };
            // None extension data: covers near-price swaps (±512 tick arrays).
            raydium_clmm::build_swap(&pool, pool_pubkey, in_prog, out_prog, &leg, None, opts)
        }
    }
}

/// Build instructions for a full multi-hop route. Wraps input SOL on the first
/// hop, creates the receiving ATA on every hop, and closes WSOL after the last
/// hop. Per-hop amounts/slippage come from the router's simulated amounts.
pub async fn build_route_instructions(
    rpc: &RpcClient,
    index: &PoolIndex,
    route: &Route,
    payer: Pubkey,
    slippage_bps: u64,
) -> Result<Vec<Instruction>, GenericError> {
    if route.hops.is_empty() {
        return Err("route has no hops".into());
    }
    let last = route.hops.len() - 1;
    let mut instructions: Vec<Instruction> = Vec::new();
    for (i, hop) in route.hops.iter().enumerate() {
        let opts = SwapOptions {
            create_input_ata: i == 0,
            wrap_input_sol: i == 0,
            create_output_ata: true,
            close_wsol: i == last,
        };
        instructions
            .extend(build_hop_instructions(rpc, index, hop, payer, slippage_bps, &opts).await?);
    }
    Ok(instructions)
}

/// Build, sign, and submit a route as a v0 transaction. Supports multi-hop.
///
/// Prepends compute budget, applies per-hop slippage floors, then picks the
/// smallest viable encoding:
/// - a caller-supplied pre-warmed `lookup_table` (fastest, no setup), else
/// - no ALT if the tx already fits under the size limit, else
/// - an on-the-fly ALT created + extended on-chain (adds setup txs + a slot).
///
/// Sends without waiting. Returns the signature.
#[allow(clippy::too_many_arguments)]
pub async fn execute_route(
    rpc: &RpcClient,
    index: &PoolIndex,
    route: &Route,
    payer: &Keypair,
    slippage_bps: u64,
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
    lookup_table: Option<AddressLookupTableAccount>,
) -> Result<Signature, GenericError> {
    let mut instructions =
        submit::compute_budget_ixs(compute_unit_limit, compute_unit_price_micro_lamports);
    instructions
        .extend(build_route_instructions(rpc, index, route, payer.pubkey(), slippage_bps).await?);

    let alts: Vec<AddressLookupTableAccount> = if let Some(table) = lookup_table {
        vec![table]
    } else {
        let probe = submit::build_unsigned_v0_transaction(&payer.pubkey(), &instructions, &[])?;
        if submit::versioned_tx_size(&probe) <= submit::TX_SIZE_LIMIT {
            vec![]
        } else {
            let addresses = alt::collect_addresses(&instructions, &[payer.pubkey()]);
            vec![alt::create_and_extend_lookup_table(rpc, payer, &addresses).await?]
        }
    };

    let blockhash = rpc.get_latest_blockhash().await?;
    let tx = submit::build_signed_v0_transaction(payer, &instructions, &alts, blockhash)?;
    submit::send_versioned(rpc, &tx).await
}

/// Build the single-hop route and **simulate** it (no signature, no SOL spent).
///
/// `payer` should be a real account that holds the input token for a fully
/// successful simulation; any pubkey still validates account layout, PDAs, and
/// program acceptance up to the balance check.
#[allow(clippy::too_many_arguments)]
pub async fn simulate_route(
    rpc: &RpcClient,
    index: &PoolIndex,
    route: &Route,
    payer: Pubkey,
    slippage_bps: u64,
    compute_unit_limit: u32,
    compute_unit_price_micro_lamports: u64,
    opts: &SwapOptions,
) -> Result<RpcSimulateTransactionResult, GenericError> {
    if route.hops.len() != 1 {
        return Err(format!(
            "simulate_route supports single-hop routes only (got {} hops)",
            route.hops.len()
        )
        .into());
    }

    let mut instructions =
        submit::compute_budget_ixs(compute_unit_limit, compute_unit_price_micro_lamports);
    instructions.extend(
        build_hop_instructions(rpc, index, &route.hops[0], payer, slippage_bps, opts).await?,
    );

    let tx = submit::build_unsigned_transaction(&payer, &instructions);
    submit::simulate(rpc, &tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_floor_math() {
        assert_eq!(min_out_with_slippage(1_000_000, 50), 995_000); // 0.5%
        assert_eq!(min_out_with_slippage(1_000_000, 0), 1_000_000);
        assert_eq!(min_out_with_slippage(1_000_000, 10_000), 0);
    }
}
