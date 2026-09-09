//! Pump.fun bonding-curve `buy` / `sell` instruction builder (pre-graduation).
//!
//! Program `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`. Account order + args
//! verified against the pump program IDL (`buy` / `sell`) and sol-trade-sdk's
//! `instruction/pumpfun.rs` legacy (native-SOL) layout:
//!
//! - **buy** uses `buy_exact_quote_in` (disc `sha256("global:buy_exact_sol_in")`):
//!   spend exactly `amount_in` lamports of SOL, receive ≥ `min_amount_out` tokens.
//!   18 accounts, 25-byte data (`disc + sol_in u64 + min_tokens u64 + track_volume u8`).
//! - **sell** uses `sell`: spend `amount_in` tokens, receive ≥ `min_amount_out`
//!   lamports. 16 accounts, 24-byte data. Note `creator_vault` and `token_program`
//!   swap positions vs buy (idx 8/9).
//!
//! Native SOL settlement only (no WSOL ATA) — the common pre-bond path where SOL
//! is spent/received directly. USDC-quote (V2) curves are out of scope here; the
//! quote layer rejects them by pairing curves against WSOL only. Cashback coins
//! (extra `user_volume_accumulator` on sell) are not special-cased — the classic
//! non-cashback layout covers the overwhelming majority of curves.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::{GenericError, WSOL};

use crate::ata::{ata, create_ata_idempotent, wsol};
use crate::types::{SwapLeg, SwapOptions};

// --- Program + fixed accounts -------------------------------------------------

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const GLOBAL_ACCOUNT: Pubkey =
    Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
pub const FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");
pub const GLOBAL_VOLUME_ACCUMULATOR: Pubkey =
    Pubkey::from_str_const("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y");
pub const FEE_CONFIG: Pubkey =
    Pubkey::from_str_const("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
pub const FEE_PROGRAM: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// Deterministic pick from the buyback / protocol-extra fee-recipient pool.
pub const PROTOCOL_EXTRA_FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// `buy_exact_sol_in` discriminator (spend exact SOL, min tokens out).
pub const BUY_EXACT_SOL_IN_DISCRIMINATOR: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
/// `sell` discriminator.
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

const BONDING_CURVE_SEED: &[u8] = b"bonding-curve";
const BONDING_CURVE_V2_SEED: &[u8] = b"bonding-curve-v2";
const CREATOR_VAULT_SEED: &[u8] = b"creator-vault";
const USER_VOLUME_ACCUMULATOR_SEED: &[u8] = b"user_volume_accumulator";

// --- PDA helpers --------------------------------------------------------------

pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[BONDING_CURVE_SEED, mint.as_ref()], &PROGRAM_ID).0
}

pub fn bonding_curve_v2_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[BONDING_CURVE_V2_SEED, mint.as_ref()], &PROGRAM_ID).0
}

/// Creator vault PDA. `creator == default` derives the program's phantom default
/// vault (matching sol-trade-sdk), which is what the on-chain program expects for
/// creatorless curves.
pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[CREATOR_VAULT_SEED, creator.as_ref()], &PROGRAM_ID).0
}

pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()], &PROGRAM_ID).0
}

// --- Accounts + builder -------------------------------------------------------

/// Curve accounts for one pump.fun bonding-curve swap. Populate from the parsed
/// `PumpfunBondingCurvePool` (`mint`, `bonding_curve`, `curve.creator`).
#[derive(Debug, Clone)]
pub struct PumpBcAccounts {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub creator: Pubkey,
}

enum Side {
    /// Spend SOL, receive token — `buy_exact_sol_in`.
    Buy,
    /// Spend token, receive SOL — `sell`.
    Sell,
}

/// Build the `buy`/`sell` instructions for one bonding-curve hop (exact-in).
///
/// `token_program` is the mint's owning token program (SPL Token or Token-2022),
/// resolved by the caller. Native SOL settlement: no WSOL wrap/close.
pub fn build_swap(
    accounts: &PumpBcAccounts,
    leg: &SwapLeg,
    token_program: Pubkey,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    let sol = wsol();
    let side = if leg.input_mint == sol && leg.output_mint == accounts.mint {
        Side::Buy
    } else if leg.input_mint == accounts.mint && leg.output_mint == sol {
        Side::Sell
    } else {
        return Err("bonding-curve leg must be WSOL<->mint".into());
    };

    let bonding_curve = accounts.bonding_curve;
    let associated_bonding_curve = ata(&bonding_curve, &accounts.mint, &token_program);
    let associated_user = ata(&leg.payer, &accounts.mint, &token_program);
    let creator_vault = creator_vault_pda(&accounts.creator);
    let bonding_curve_v2 = bonding_curve_v2_pda(&accounts.mint);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(3);

    // The user's token ATA must exist to receive (buy) or spend (sell) the token.
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &accounts.mint, &token_program));
    }

    let (metas, data) = match side {
        Side::Buy => {
            let metas = vec![
                AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),           // 0 global
                AccountMeta::new(FEE_RECIPIENT, false),                     // 1 fee_recipient
                AccountMeta::new_readonly(accounts.mint, false),           // 2 mint
                AccountMeta::new(bonding_curve, false),                     // 3 bonding_curve
                AccountMeta::new(associated_bonding_curve, false),          // 4 associated_bonding_curve
                AccountMeta::new(associated_user, false),                   // 5 associated_user
                AccountMeta::new(leg.payer, true),                          // 6 user (signer)
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),           // 7 system_program
                AccountMeta::new_readonly(token_program, false),            // 8 token_program
                AccountMeta::new(creator_vault, false),                     // 9 creator_vault
                AccountMeta::new_readonly(EVENT_AUTHORITY, false),          // 10 event_authority
                AccountMeta::new_readonly(PROGRAM_ID, false),               // 11 program
                AccountMeta::new(GLOBAL_VOLUME_ACCUMULATOR, false),         // 12 global_volume_accumulator
                AccountMeta::new(user_volume_accumulator_pda(&leg.payer), false), // 13 user_volume_accumulator
                AccountMeta::new_readonly(FEE_CONFIG, false),               // 14 fee_config
                AccountMeta::new_readonly(FEE_PROGRAM, false),              // 15 fee_program
                AccountMeta::new_readonly(bonding_curve_v2, false),         // 16 bonding_curve_v2
                AccountMeta::new(PROTOCOL_EXTRA_FEE_RECIPIENT, false),      // 17 protocol_extra_fee_recipient
            ];
            // disc(8) + sol_in u64 + min_tokens_out u64 + track_volume u8
            let mut d = Vec::with_capacity(25);
            d.extend_from_slice(&BUY_EXACT_SOL_IN_DISCRIMINATOR);
            d.extend_from_slice(&leg.amount_in.to_le_bytes());
            d.extend_from_slice(&leg.min_amount_out.to_le_bytes());
            d.push(0u8); // track_volume off (non-cashback)
            (metas, d)
        }
        Side::Sell => {
            let metas = vec![
                AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),           // 0 global
                AccountMeta::new(FEE_RECIPIENT, false),                     // 1 fee_recipient
                AccountMeta::new_readonly(accounts.mint, false),           // 2 mint
                AccountMeta::new(bonding_curve, false),                     // 3 bonding_curve
                AccountMeta::new(associated_bonding_curve, false),          // 4 associated_bonding_curve
                AccountMeta::new(associated_user, false),                   // 5 associated_user
                AccountMeta::new(leg.payer, true),                          // 6 user (signer)
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),           // 7 system_program
                AccountMeta::new(creator_vault, false),                     // 8 creator_vault
                AccountMeta::new_readonly(token_program, false),            // 9 token_program
                AccountMeta::new_readonly(EVENT_AUTHORITY, false),          // 10 event_authority
                AccountMeta::new_readonly(PROGRAM_ID, false),               // 11 program
                AccountMeta::new_readonly(FEE_CONFIG, false),               // 12 fee_config
                AccountMeta::new_readonly(FEE_PROGRAM, false),              // 13 fee_program
                AccountMeta::new_readonly(bonding_curve_v2, false),         // 14 bonding_curve_v2
                AccountMeta::new(PROTOCOL_EXTRA_FEE_RECIPIENT, false),      // 15 protocol_extra_fee_recipient
            ];
            // disc(8) + token_amount u64 + min_sol_out u64
            let mut d = Vec::with_capacity(24);
            d.extend_from_slice(&SELL_DISCRIMINATOR);
            d.extend_from_slice(&leg.amount_in.to_le_bytes());
            d.extend_from_slice(&leg.min_amount_out.to_le_bytes());
            (metas, d)
        }
    };

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));
    Ok(ixs)
}

/// The WSOL mint, exposed so callers can build bonding-curve `SwapLeg`s.
pub fn sol_mint() -> Pubkey {
    Pubkey::from_str_const(WSOL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn accts() -> PumpBcAccounts {
        PumpBcAccounts { mint: pk(2), bonding_curve: pk(3), creator: pk(4) }
    }

    fn opts() -> SwapOptions {
        SwapOptions { create_input_ata: false, create_output_ata: true, wrap_input_sol: false, close_wsol: false }
    }

    #[test]
    fn buy_layout_and_data() {
        let leg = SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 500 };
        let ixs = build_swap(&accts(), &leg, Pubkey::from_str_const(solroute_core::TOKEN_PROGRAM), &opts()).unwrap();
        // create_output_ata + swap
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(&ix.data[..8], &BUY_EXACT_SOL_IN_DISCRIMINATOR);
        assert_eq!(ix.data.len(), 25);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 500);
        assert!(ix.accounts[6].is_signer);
        assert_eq!(ix.accounts[3].pubkey, pk(3)); // bonding_curve
    }

    #[test]
    fn sell_layout_and_data() {
        let leg = SwapLeg { payer: pk(9), input_mint: pk(2), output_mint: wsol(), amount_in: 5_000, min_amount_out: 900 };
        let ix = build_swap(&accts(), &leg, Pubkey::from_str_const(solroute_core::TOKEN_PROGRAM), &opts())
            .unwrap().pop().unwrap();
        assert_eq!(ix.accounts.len(), 16);
        assert_eq!(&ix.data[..8], &SELL_DISCRIMINATOR);
        assert_eq!(ix.data.len(), 24);
        // creator_vault at 8, token_program at 9 (swapped vs buy).
        assert_eq!(ix.accounts[8].pubkey, creator_vault_pda(&pk(4)));
        assert!(ix.accounts[8].is_writable);
        assert_eq!(ix.accounts[9].pubkey, Pubkey::from_str_const(solroute_core::TOKEN_PROGRAM));
    }

    #[test]
    fn rejects_non_sol_pair() {
        let leg = SwapLeg { payer: pk(9), input_mint: pk(88), output_mint: pk(2), amount_in: 1, min_amount_out: 0 };
        assert!(build_swap(&accts(), &leg, Pubkey::from_str_const(solroute_core::TOKEN_PROGRAM), &opts()).is_err());
    }
}
