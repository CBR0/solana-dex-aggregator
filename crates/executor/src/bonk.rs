//! bonk.fun / Raydium LaunchLab `buy_exact_in` / `sell_exact_in` builder.
//!
//! Program `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj`. Data = disc + amount_in
//! u64 + minimum_amount_out u64 + share_fee_rate u64 (0). Quote side is WSOL/USDC
//! (an SPL token account), so SOL is wrapped on buy and the WSOL account closed
//! after — the standard AMM wrap/close flow.
//!
//! **18 accounts** (verified by live sim). The sol-trade-sdk reference and both
//! the bundled and current on-chain IDLs define only 15; the extra 3 are dynamic
//! `remaining_accounts` the program appends for fee distribution:
//!   15 `system_program` (`111…111`)
//!   16 platform fee vault — a WSOL token account (per platform_config)
//!   17 creator/second fee vault — a WSOL token account (per pool)
//! Their owners aren't in the pool's `global_config`/`platform_config`/`creator`
//! and they aren't the WSOL ATAs of the fee wallets, so they're not derivable
//! from public account data. The aggregator resolves them at execution time by
//! observing accounts 16/17 from a recent swap on the pool
//! (`execute::observe_bonk_fee_vaults`); this builder takes them as parameters.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::GenericError;

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
pub const AUTHORITY: Pubkey =
    Pubkey::from_str_const("WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh");
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("2DPAtwB8L12vrMRExbLuyGnC7n2J5LNoZQSejeQGpwkr");
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

pub const BUY_EXACT_IN_DISCRIMINATOR: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236];
pub const SELL_EXACT_IN_DISCRIMINATOR: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];

/// Pool accounts for a LaunchLab swap, from the parsed `BonkPoolState`.
#[derive(Debug, Clone)]
pub struct BonkAccounts {
    pub pool_state: Pubkey,
    pub global_config: Pubkey,
    pub platform_config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

/// Build `buy_exact_in` / `sell_exact_in` for one LaunchLab hop.
///
/// `base_token_program` is the base mint's owning program (SPL or Token-2022);
/// `quote_token_program` is the quote mint's (WSOL/USDC → legacy SPL Token).
/// `platform_fee_vault` / `creator_fee_vault` are the quote-mint token accounts
/// the current program requires as trailing accounts (see module docs) — the
/// caller must supply them (their derivation is the open item).
#[allow(clippy::too_many_arguments)]
pub fn build_swap(
    accounts: &BonkAccounts,
    leg: &SwapLeg,
    base_token_program: Pubkey,
    quote_token_program: Pubkey,
    platform_fee_vault: Pubkey,
    creator_fee_vault: Pubkey,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    let buy = if leg.input_mint == accounts.quote_mint && leg.output_mint == accounts.base_mint {
        true
    } else if leg.input_mint == accounts.base_mint && leg.output_mint == accounts.quote_mint {
        false
    } else {
        return Err("bonk leg mints do not match pool".into());
    };

    let user_base = ata(&leg.payer, &accounts.base_mint, &base_token_program);
    let user_quote = ata(&leg.payer, &accounts.quote_mint, &quote_token_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(4);

    // Input-side ATA setup: wrap SOL when the quote (input on buy) is WSOL.
    let input_is_wsol_quote = buy && accounts.quote_mint == wsol();
    if opts.wrap_input_sol && input_is_wsol_quote {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        let (mint, prog) = if buy {
            (accounts.quote_mint, quote_token_program)
        } else {
            (accounts.base_mint, base_token_program)
        };
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &mint, &prog));
    }
    // Output-side ATA (the token you receive).
    if opts.create_output_ata {
        let (mint, prog) = if buy {
            (accounts.base_mint, base_token_program)
        } else {
            (accounts.quote_mint, quote_token_program)
        };
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &mint, &prog));
    }

    let metas = vec![
        AccountMeta::new(leg.payer, true),                          // 0 payer (signer)
        AccountMeta::new_readonly(AUTHORITY, false),                // 1 authority
        AccountMeta::new_readonly(accounts.global_config, false),   // 2 global_config
        AccountMeta::new_readonly(accounts.platform_config, false), // 3 platform_config
        AccountMeta::new(accounts.pool_state, false),               // 4 pool_state
        AccountMeta::new(user_base, false),                         // 5 user_base_token
        AccountMeta::new(user_quote, false),                        // 6 user_quote_token
        AccountMeta::new(accounts.base_vault, false),               // 7 base_vault
        AccountMeta::new(accounts.quote_vault, false),              // 8 quote_vault
        AccountMeta::new_readonly(accounts.base_mint, false),       // 9 base_token_mint
        AccountMeta::new_readonly(accounts.quote_mint, false),      // 10 quote_token_mint
        AccountMeta::new_readonly(base_token_program, false),       // 11 base_token_program
        AccountMeta::new_readonly(quote_token_program, false),      // 12 quote_token_program
        AccountMeta::new_readonly(EVENT_AUTHORITY, false),          // 13 event_authority
        AccountMeta::new_readonly(PROGRAM_ID, false),               // 14 program
        // Trailing accounts the current on-chain program requires (18 total).
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),           // 15 system_program
        AccountMeta::new(platform_fee_vault, false),                // 16 platform fee vault (WSOL)
        AccountMeta::new(creator_fee_vault, false),                 // 17 creator/second fee vault (WSOL)
    ];

    // data: disc(8) + amount_in u64 + minimum_amount_out u64 + share_fee_rate u64(0)
    let disc = if buy { BUY_EXACT_IN_DISCRIMINATOR } else { SELL_EXACT_IN_DISCRIMINATOR };
    let mut data = Vec::with_capacity(32);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&leg.amount_in.to_le_bytes());
    data.extend_from_slice(&leg.min_amount_out.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // share_fee_rate
    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    // Close WSOL: on buy the input WSOL wrapper, on sell the received WSOL.
    if opts.close_wsol && accounts.quote_mint == wsol() {
        ixs.push(close_account_ix(&quote_token_program, &user_quote, &leg.payer));
    }

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solroute_core::TOKEN_PROGRAM;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }
    fn accts() -> BonkAccounts {
        BonkAccounts {
            pool_state: pk(1),
            global_config: pk(2),
            platform_config: pk(3),
            base_mint: pk(4),
            quote_mint: wsol(),
            base_vault: pk(5),
            quote_vault: pk(6),
        }
    }
    fn opts() -> SwapOptions {
        SwapOptions { create_input_ata: false, create_output_ata: true, wrap_input_sol: true, close_wsol: true }
    }

    #[test]
    fn buy_layout_and_data() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let leg = SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(4), amount_in: 1_000_000, min_amount_out: 5 };
        let ixs = build_swap(&accts(), &leg, tp, tp, pk(7), pk(8), &opts()).unwrap();
        let ix = ixs.iter().find(|i| i.program_id == PROGRAM_ID).unwrap();
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(&ix.data[..8], &BUY_EXACT_IN_DISCRIMINATOR);
        assert_eq!(ix.data.len(), 32);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 5);
        assert_eq!(u64::from_le_bytes(ix.data[24..32].try_into().unwrap()), 0);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[4].pubkey, pk(1)); // pool_state
        assert_eq!(ix.accounts[15].pubkey, SYSTEM_PROGRAM);
        assert_eq!(ix.accounts[16].pubkey, pk(7)); // platform fee vault
        assert_eq!(ix.accounts[17].pubkey, pk(8)); // creator fee vault
    }

    #[test]
    fn sell_uses_sell_disc() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let leg = SwapLeg { payer: pk(9), input_mint: pk(4), output_mint: wsol(), amount_in: 100, min_amount_out: 1 };
        let ix = build_swap(&accts(), &leg, tp, tp, pk(7), pk(8), &opts()).unwrap().into_iter().find(|i| i.program_id == PROGRAM_ID).unwrap();
        assert_eq!(&ix.data[..8], &SELL_EXACT_IN_DISCRIMINATOR);
        assert_eq!(ix.accounts.len(), 18);
    }
}
