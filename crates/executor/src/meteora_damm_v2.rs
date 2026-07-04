//! Meteora DAMM V2 swap instruction builder.
//!
//! Ported from sol-trade-sdk `instruction/meteora_damm_v2.rs` (MIT). Uses the
//! `swap2` instruction. Account order and data layout match the on-chain program.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use thunder_core::GenericError;

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

/// Meteora DAMM V2 program.
pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
/// Fixed pool authority PDA.
pub const POOL_AUTHORITY: Pubkey =
    Pubkey::from_str_const("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");

/// `swap2` Anchor discriminator.
pub const SWAP2_DISCRIMINATOR: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];

/// swap_mode: exact input, `amount_1` is the minimum output.
pub const SWAP_MODE_EXACT_IN: u8 = 0;
/// swap_mode: exact output, `amount_0` is the exact output, `amount_1` the max input.
pub const SWAP_MODE_EXACT_OUT: u8 = 2;

const EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";

/// Pool accounts needed to build a DAMM V2 swap. Populate from the parsed pool
/// state (`MeteoraDAMMV2Pool`) plus the token programs owning each mint.
#[derive(Debug, Clone)]
pub struct DammV2Accounts {
    pub pool: Pubkey,
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
    pub token_a_program: Pubkey,
    pub token_b_program: Pubkey,
}

/// The `__event_authority` PDA for the program.
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[EVENT_AUTHORITY_SEED], &PROGRAM_ID).0
}

/// Build the instructions for one DAMM V2 swap leg (exact-in, slippage floor).
///
/// Emits: optional input ATA setup (or WSOL wrap), optional output ATA,
/// the `swap2` instruction, and optional WSOL close.
pub fn build_swap(
    accounts: &DammV2Accounts,
    leg: &SwapLeg,
    referral_token_account: Option<Pubkey>,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }

    // Direction: is the input the pool's token A?
    let is_a_in = leg.input_mint == accounts.token_a_mint;
    if !is_a_in && leg.input_mint != accounts.token_b_mint {
        return Err("input_mint does not belong to this pool".into());
    }
    let (input_program, output_program) = if is_a_in {
        (accounts.token_a_program, accounts.token_b_program)
    } else {
        (accounts.token_b_program, accounts.token_a_program)
    };

    let input_ata = ata(&leg.payer, &leg.input_mint, &input_program);
    let output_ata = ata(&leg.payer, &leg.output_mint, &output_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(6);

    // Input side setup.
    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &input_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &output_program));
    }

    // swap2 data: [disc(8)][amount_0(8)][amount_1(8)][mode(1)]
    // exact-in → amount_0 = amount_in, amount_1 = min_out.
    let mut data = [0u8; 25];
    data[..8].copy_from_slice(&SWAP2_DISCRIMINATOR);
    data[8..16].copy_from_slice(&leg.amount_in.to_le_bytes());
    data[16..24].copy_from_slice(&leg.min_amount_out.to_le_bytes());
    data[24] = SWAP_MODE_EXACT_IN;

    let mut metas = vec![
        AccountMeta::new_readonly(POOL_AUTHORITY, false), // Pool Authority
        AccountMeta::new(accounts.pool, false),           // Pool
        AccountMeta::new(input_ata, false),               // Input Token Account
        AccountMeta::new(output_ata, false),              // Output Token Account
        AccountMeta::new(accounts.token_a_vault, false),  // Token A Vault
        AccountMeta::new(accounts.token_b_vault, false),  // Token B Vault
        AccountMeta::new_readonly(accounts.token_a_mint, false), // Token A Mint
        AccountMeta::new_readonly(accounts.token_b_mint, false), // Token B Mint
        AccountMeta::new(leg.payer, true),                // User Transfer Authority
        AccountMeta::new_readonly(accounts.token_a_program, false), // Token A Program
        AccountMeta::new_readonly(accounts.token_b_program, false), // Token B Program
    ];
    // The referral slot is always present. When there's no referral, Anchor
    // expects the program's own id as the `None` placeholder for the optional
    // account (verified against on-chain swap2 txs — 14 accounts total).
    match referral_token_account {
        Some(referral) => metas.push(AccountMeta::new(referral, false)),
        None => metas.push(AccountMeta::new_readonly(PROGRAM_ID, false)),
    }
    metas.push(AccountMeta::new_readonly(event_authority(), false)); // Event Authority
    metas.push(AccountMeta::new_readonly(PROGRAM_ID, false)); // Program

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    // Unwrap WSOL back to native SOL if requested.
    if opts.close_wsol {
        if leg.input_mint == wsol() {
            ixs.push(close_account_ix(&input_program, &input_ata, &leg.payer));
        }
        if leg.output_mint == wsol() {
            ixs.push(close_account_ix(&output_program, &output_ata, &leg.payer));
        }
    }

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ata::token_program;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn accounts() -> DammV2Accounts {
        DammV2Accounts {
            pool: pk(1),
            token_a_mint: wsol(),
            token_b_mint: pk(2),
            token_a_vault: pk(3),
            token_b_vault: pk(4),
            token_a_program: token_program(),
            token_b_program: token_program(),
        }
    }

    fn leg() -> SwapLeg {
        SwapLeg {
            payer: pk(9),
            input_mint: wsol(),
            output_mint: pk(2),
            amount_in: 100_000,
            min_amount_out: 42,
        }
    }

    #[test]
    fn swap2_account_order_and_data_layout() {
        let ixs = build_swap(&accounts(), &leg(), None, &SwapOptions::default()).unwrap();
        assert_eq!(ixs.len(), 1);
        let ix = &ixs[0];

        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 14); // referral slot always present
        assert_eq!(ix.accounts[0].pubkey, POOL_AUTHORITY);
        assert_eq!(ix.accounts[1].pubkey, pk(1));
        assert_eq!(ix.accounts[8].pubkey, pk(9)); // payer
        assert!(ix.accounts[8].is_signer);
        assert_eq!(ix.accounts[11].pubkey, PROGRAM_ID); // referral None placeholder
        assert!(!ix.accounts[11].is_writable);
        assert_eq!(ix.accounts[12].pubkey, event_authority());
        assert_eq!(ix.accounts[13].pubkey, PROGRAM_ID);

        assert_eq!(&ix.data[..8], &SWAP2_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 100_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 42);
        assert_eq!(ix.data[24], SWAP_MODE_EXACT_IN);
    }

    #[test]
    fn referral_account_inserted_writable_before_event_authority() {
        let referral = pk(50);
        let ixs = build_swap(&accounts(), &leg(), Some(referral), &SwapOptions::default()).unwrap();
        let ix = &ixs[0];
        assert_eq!(ix.accounts.len(), 14);
        assert_eq!(ix.accounts[11].pubkey, referral); // real referral in the slot
        assert!(ix.accounts[11].is_writable);
        assert_eq!(ix.accounts[12].pubkey, event_authority());
        assert_eq!(ix.accounts[13].pubkey, PROGRAM_ID);
    }

    #[test]
    fn wrap_sol_prepends_setup_and_close_appends() {
        let opts = SwapOptions { wrap_input_sol: true, close_wsol: true, ..Default::default() };
        let ixs = build_swap(&accounts(), &leg(), None, &opts).unwrap();
        // 3 wrap ixs + swap + 1 close (input is WSOL) = 5
        assert_eq!(ixs.len(), 5);
        assert_eq!(ixs[3].program_id, PROGRAM_ID); // swap in the middle
    }

    #[test]
    fn rejects_foreign_input_mint() {
        let mut l = leg();
        l.input_mint = pk(200);
        l.output_mint = pk(2);
        assert!(build_swap(&accounts(), &l, None, &SwapOptions::default()).is_err());
    }

    #[test]
    fn rejects_zero_amount() {
        let mut l = leg();
        l.amount_in = 0;
        assert!(build_swap(&accounts(), &l, None, &SwapOptions::default()).is_err());
    }
}
