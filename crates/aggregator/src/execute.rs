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
use solana_sdk::signature::{Signature, Signer};
use solana_rpc_client_api::response::RpcSimulateTransactionResult;

use solroute_core::{GenericError, TOKEN_PROGRAM, TOKEN_PROGRAM_2022};
use solroute_executor::alt;
use solroute_executor::meteora_damm_v1::{self, DammV1Accounts};
use solroute_executor::meteora_damm_v2::{self, DammV2Accounts};
use solroute_executor::pumpswap::{self, PumpSwapAccounts};
use solroute_executor::raydium_amm_v4;
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
        CachedPool::RaydiumV4 { pool, .. } => {
            raydium_amm_v4::build_swap(rpc, &pool, pool_pubkey, &leg, opts).await
        }
        CachedPool::MeteoraDAMMV1 { pool, .. } => {
            let accounts = DammV1Accounts {
                pool: pool_pubkey,
                token_a_mint: pool.token_a_mint,
                token_b_mint: pool.token_b_mint,
                a_vault: pool.a_vault,
                b_vault: pool.b_vault,
                a_vault_lp: pool.a_vault_lp,
                b_vault_lp: pool.b_vault_lp,
                protocol_token_a_fee: pool.protocol_token_a_fee,
                protocol_token_b_fee: pool.protocol_token_b_fee,
            };
            meteora_damm_v1::build_swap(&accounts, &leg, opts)
        }
        CachedPool::RaydiumClmm { .. } | CachedPool::MeteoraDLMM { .. } => Err(format!(
            "execution not implemented for {} ({})",
            hop.dex_name, hop.pool_address
        )
        .into()),
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
    payer: &dyn Signer,
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
