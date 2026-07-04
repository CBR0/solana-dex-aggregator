//! Raydium AMM V4 swap instruction builder.
//!
//! Ported from sol-trade-sdk `instruction/raydium_amm_v4.rs` + `utils/raydium_amm_v4.rs` (MIT).
//! Uses `swap_base_in` (discriminator `9`). The V4 swap needs Serum/OpenBook
//! market accounts (bids/asks/event-queue/vaults/vault-signer) that don't live
//! in the AMM pool struct, so `build_swap` fetches and decodes the market
//! account from RPC. `assemble_swap` is the pure, offline-testable core.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};

use raydium_amm_v4::RaydiumAMMV4;
use thunder_core::{GenericError, TOKEN_PROGRAM};

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
/// Fixed Raydium AMM authority.
pub const AUTHORITY: Pubkey = Pubkey::from_str_const("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");

/// `swap_base_in` instruction discriminator (single byte).
pub const SWAP_BASE_IN: u8 = 9;

/// Serum v3 (OpenBook) market account layout — only the fields the swap needs.
#[derive(Clone, Debug, Default, BorshDeserialize)]
pub struct MarketState {
    pub padding: [u8; 5],
    pub account_flags: u64,
    pub own_address: Pubkey,
    pub vault_signer_nonce: u64,
    pub coin_mint: Pubkey,
    pub pc_mint: Pubkey,
    pub serum_coin_vault_account: Pubkey,
    pub coin_deposits_total: u64,
    pub coin_fees_accrued: u64,
    pub serum_pc_vault_account: Pubkey,
    pub pc_deposits_total: u64,
    pub pc_fees_accrued: u64,
    pub pc_dust_threshold: u64,
    pub request_queue: Pubkey,
    pub serum_event_queue: Pubkey,
    pub serum_bids: Pubkey,
    pub serum_asks: Pubkey,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub fee_rate_bps: u64,
    pub referrer_rebate_accrued: u64,
    pub padding2: [u8; 7],
}

const MARKET_STATE_SIZE: usize = 388;

fn decode_market_state(data: &[u8]) -> Option<MarketState> {
    if data.len() < MARKET_STATE_SIZE {
        return None;
    }
    borsh::from_slice::<MarketState>(&data[..MARKET_STATE_SIZE]).ok()
}

/// Serum market accounts required by the V4 swap.
#[derive(Debug, Clone)]
pub struct SerumMarketAccounts {
    pub bids: Pubkey,
    pub asks: Pubkey,
    pub event_queue: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub vault_signer: Pubkey,
}

/// Derive the Serum vault signer from the market + nonce (with a legacy fallback).
pub fn derive_vault_signer(
    serum_program: &Pubkey,
    serum_market: &Pubkey,
    vault_signer_nonce: u64,
) -> Result<Pubkey, GenericError> {
    let nonce = vault_signer_nonce.to_le_bytes();
    Pubkey::create_program_address(&[serum_market.as_ref(), &nonce], serum_program)
        .or_else(|_| {
            let legacy = [vault_signer_nonce as u8];
            Pubkey::create_program_address(&[serum_market.as_ref(), &legacy], serum_program)
        })
        .map_err(|e| format!("failed to derive Serum vault signer: {e}").into())
}

/// Fetch and decode the Serum market account, deriving its vault signer.
pub async fn fetch_serum(
    rpc: &RpcClient,
    market: &Pubkey,
    serum_program: &Pubkey,
) -> Result<SerumMarketAccounts, GenericError> {
    let data = rpc.get_account_data(market).await?;
    let market_state =
        decode_market_state(&data).ok_or_else(|| GenericError::from("failed to decode Serum market state"))?;
    let vault_signer = derive_vault_signer(serum_program, market, market_state.vault_signer_nonce)?;
    Ok(SerumMarketAccounts {
        bids: market_state.serum_bids,
        asks: market_state.serum_asks,
        event_queue: market_state.serum_event_queue,
        coin_vault: market_state.serum_coin_vault_account,
        pc_vault: market_state.serum_pc_vault_account,
        vault_signer,
    })
}

/// Assemble the swap instructions given pre-fetched Serum accounts (pure).
pub fn assemble_swap(
    pool: &RaydiumAMMV4,
    pool_pubkey: Pubkey,
    serum: &SerumMarketAccounts,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    // Raydium V4 pools mints must match the leg.
    let known = [pool.base_mint, pool.quote_mint];
    if !known.contains(&leg.input_mint) || !known.contains(&leg.output_mint) {
        return Err("swap leg mints do not match this pool".into());
    }

    let token_program = Pubkey::from_str_const(TOKEN_PROGRAM);
    let user_source = ata(&leg.payer, &leg.input_mint, &token_program);
    let user_destination = ata(&leg.payer, &leg.output_mint, &token_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(6);

    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &token_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &token_program));
    }

    let accounts = vec![
        AccountMeta::new_readonly(token_program, false), // 0 token program
        AccountMeta::new(pool_pubkey, false),            // 1 amm
        AccountMeta::new_readonly(AUTHORITY, false),     // 2 authority
        AccountMeta::new(pool.open_orders, false),       // 3 amm open orders
        AccountMeta::new(pool.target_oders, false),      // 4 amm target orders
        AccountMeta::new(pool.base_vault, false),        // 5 pool coin token account
        AccountMeta::new(pool.quote_vault, false),       // 6 pool pc token account
        AccountMeta::new_readonly(pool.market_program_id, false), // 7 serum program
        AccountMeta::new(pool.market_id, false),         // 8 serum market
        AccountMeta::new(serum.bids, false),             // 9 serum bids
        AccountMeta::new(serum.asks, false),             // 10 serum asks
        AccountMeta::new(serum.event_queue, false),      // 11 serum event queue
        AccountMeta::new(serum.coin_vault, false),       // 12 serum coin vault
        AccountMeta::new(serum.pc_vault, false),         // 13 serum pc vault
        AccountMeta::new_readonly(serum.vault_signer, false), // 14 serum vault signer
        AccountMeta::new(user_source, false),            // 15 user source
        AccountMeta::new(user_destination, false),       // 16 user destination
        AccountMeta::new(leg.payer, true),               // 17 user source owner (signer)
    ];

    // swap_base_in: [9][amount_in u64][min_amount_out u64]
    let mut data = [0u8; 17];
    data[0] = SWAP_BASE_IN;
    data[1..9].copy_from_slice(&leg.amount_in.to_le_bytes());
    data[9..17].copy_from_slice(&leg.min_amount_out.to_le_bytes());

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, accounts));

    if opts.close_wsol {
        if leg.input_mint == wsol() {
            ixs.push(close_account_ix(&token_program, &user_source, &leg.payer));
        }
        if leg.output_mint == wsol() {
            ixs.push(close_account_ix(&token_program, &user_destination, &leg.payer));
        }
    }

    Ok(ixs)
}

/// Fetch the Serum market from RPC, then assemble the swap instructions.
pub async fn build_swap(
    rpc: &RpcClient,
    pool: &RaydiumAMMV4,
    pool_pubkey: Pubkey,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    let serum = fetch_serum(rpc, &pool.market_id, &pool.market_program_id).await?;
    assemble_swap(pool, pool_pubkey, &serum, leg, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn pool() -> RaydiumAMMV4 {
        // Only the fields assemble_swap reads need to be meaningful.
        let mut p: RaydiumAMMV4 = borsh::from_slice(&[0u8; 752]).unwrap();
        p.base_mint = wsol();
        p.quote_mint = pk(2);
        p.base_vault = pk(3);
        p.quote_vault = pk(4);
        p.open_orders = pk(5);
        p.target_oders = pk(6);
        p.market_id = pk(7);
        p.market_program_id = pk(8);
        p
    }

    fn serum() -> SerumMarketAccounts {
        SerumMarketAccounts {
            bids: pk(10),
            asks: pk(11),
            event_queue: pk(12),
            coin_vault: pk(13),
            pc_vault: pk(14),
            vault_signer: pk(15),
        }
    }

    fn leg() -> SwapLeg {
        SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 42 }
    }

    #[test]
    fn swap_base_in_account_order_and_data() {
        let ixs = assemble_swap(&pool(), pk(1), &serum(), &leg(), &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(ix.accounts[1].pubkey, pk(1)); // amm
        assert_eq!(ix.accounts[2].pubkey, AUTHORITY);
        assert_eq!(ix.accounts[4].pubkey, pk(6)); // target orders
        assert_eq!(ix.accounts[9].pubkey, pk(10)); // serum bids
        assert_eq!(ix.accounts[14].pubkey, pk(15)); // vault signer
        assert!(!ix.accounts[14].is_writable);
        assert_eq!(ix.accounts[17].pubkey, pk(9)); // owner
        assert!(ix.accounts[17].is_signer);

        assert_eq!(ix.data[0], SWAP_BASE_IN);
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[9..17].try_into().unwrap()), 42);
    }

    #[test]
    fn rejects_foreign_mints() {
        let mut l = leg();
        l.input_mint = pk(200);
        assert!(assemble_swap(&pool(), pk(1), &serum(), &l, &SwapOptions::default()).is_err());
    }

    #[test]
    fn market_state_size_is_388() {
        // Ensures the borsh layout matches the on-chain fixed size.
        let ms: MarketState = borsh::from_slice(&[0u8; MARKET_STATE_SIZE]).unwrap();
        assert_eq!(ms.vault_signer_nonce, 0);
    }
}
