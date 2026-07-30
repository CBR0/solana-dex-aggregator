//! Meteora Dynamic Bonding Curve (DBC) `swap` builder (exact-in).
//!
//! Program `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`. Account order verified
//! against the IDL and a live `swap2` tx: 15 accounts, data = disc + amount_in
//! u64 + minimum_amount_out u64 (classic `swap`, `sha256("global:swap")`). The
//! `referral_token_account` (index 12) is optional — when absent the program id
//! itself is passed in its slot (Anchor `Option<Account>` convention). Quote side
//! is WSOL (an SPL token account) → SOL wrapped on buy, WSOL closed after.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::GenericError;

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const POOL_AUTHORITY: Pubkey =
    Pubkey::from_str_const("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("8Ks12pbrD6PXxfty1hVQiE9sc289zgU1zHkvXhrSdriF");

/// `swap` discriminator = sha256("global:swap")[..8].
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Pool accounts for a DBC swap, from the parsed `VirtualPool`.
#[derive(Debug, Clone)]
pub struct DbcAccounts {
    pub pool: Pubkey,
    pub config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

/// Build the `swap` instructions for one DBC hop (exact-in).
///
/// `base_token_program` / `quote_token_program` are the base/quote mints' owning
/// programs (pump-style DBC base mints are often Token-2022).
pub fn build_swap(
    accounts: &DbcAccounts,
    leg: &SwapLeg,
    base_token_program: Pubkey,
    quote_token_program: Pubkey,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    // Direction from the leg mints (base<->quote).
    let (in_prog, out_prog) = if leg.input_mint == accounts.quote_mint && leg.output_mint == accounts.base_mint {
        (quote_token_program, base_token_program) // buy: spend quote, receive base
    } else if leg.input_mint == accounts.base_mint && leg.output_mint == accounts.quote_mint {
        (base_token_program, quote_token_program) // sell: spend base, receive quote
    } else {
        return Err("dbc leg mints do not match pool".into());
    };

    let input_ta = ata(&leg.payer, &leg.input_mint, &in_prog);
    let output_ta = ata(&leg.payer, &leg.output_mint, &out_prog);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(4);

    let input_is_wsol = leg.input_mint == wsol();
    if opts.wrap_input_sol && input_is_wsol {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &in_prog));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &out_prog));
    }

    let metas = vec![
        AccountMeta::new_readonly(POOL_AUTHORITY, false),        // 0 pool_authority
        AccountMeta::new_readonly(accounts.config, false),       // 1 config
        AccountMeta::new(accounts.pool, false),                  // 2 pool
        AccountMeta::new(input_ta, false),                       // 3 input_token_account
        AccountMeta::new(output_ta, false),                      // 4 output_token_account
        AccountMeta::new(accounts.base_vault, false),            // 5 base_vault
        AccountMeta::new(accounts.quote_vault, false),           // 6 quote_vault
        AccountMeta::new_readonly(accounts.base_mint, false),    // 7 base_mint
        AccountMeta::new_readonly(accounts.quote_mint, false),   // 8 quote_mint
        AccountMeta::new(leg.payer, true),                       // 9 payer (signer)
        AccountMeta::new_readonly(base_token_program, false),    // 10 token_base_program
        AccountMeta::new_readonly(quote_token_program, false),   // 11 token_quote_program
        AccountMeta::new_readonly(PROGRAM_ID, false),            // 12 referral (none → program id)
        AccountMeta::new_readonly(EVENT_AUTHORITY, false),       // 13 event_authority
        AccountMeta::new_readonly(PROGRAM_ID, false),            // 14 program
    ];

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&leg.amount_in.to_le_bytes());
    data.extend_from_slice(&leg.min_amount_out.to_le_bytes());
    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    if opts.close_wsol && leg.output_mint == wsol() {
        ixs.push(close_account_ix(&quote_token_program, &output_ta, &leg.payer));
    } else if opts.close_wsol && leg.input_mint == wsol() {
        ixs.push(close_account_ix(&quote_token_program, &input_ta, &leg.payer));
    }

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solroute_core::{TOKEN_PROGRAM, WSOL};

    fn pk(n: u8) -> Pubkey { Pubkey::new_from_array([n; 32]) }
    fn accts() -> DbcAccounts {
        DbcAccounts { pool: pk(1), config: pk(2), base_mint: pk(3), quote_mint: Pubkey::from_str_const(WSOL), base_vault: pk(5), quote_vault: pk(6) }
    }
    fn opts() -> SwapOptions { SwapOptions { create_input_ata: false, create_output_ata: true, wrap_input_sol: true, close_wsol: true } }

    #[test]
    fn buy_layout() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let leg = SwapLeg { payer: pk(9), input_mint: Pubkey::from_str_const(WSOL), output_mint: pk(3), amount_in: 1_000_000, min_amount_out: 7 };
        let ix = build_swap(&accts(), &leg, tp, tp, &opts()).unwrap().into_iter().find(|i| i.program_id == PROGRAM_ID).unwrap();
        assert_eq!(ix.accounts.len(), 15);
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(ix.data.len(), 24);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 7);
        assert_eq!(ix.accounts[0].pubkey, POOL_AUTHORITY);
        assert!(ix.accounts[9].is_signer);
    }

    #[test]
    fn rejects_foreign_mints() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let leg = SwapLeg { payer: pk(9), input_mint: pk(88), output_mint: pk(3), amount_in: 1, min_amount_out: 0 };
        assert!(build_swap(&accts(), &leg, tp, tp, &opts()).is_err());
    }
}
