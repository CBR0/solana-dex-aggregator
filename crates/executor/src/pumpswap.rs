//! PumpSwap (Pump AMM) swap instruction builder — full account fidelity.
//!
//! Ported from sol-trade-sdk `instruction/pumpswap.rs` + `utils/pumpswap.rs` (MIT).
//! Supports the full account layout including coin-creator vaults, volume
//! accumulators (cashback), mayhem-mode fee recipients, `pool-v2`, and the
//! trailing buyback fee-recipient accounts, so every live pool is executable.
//!
//! Adaptations vs the source SDK:
//! - `min_amount_out` bounds come from the caller (solroute's quote × slippage),
//!   so the pump-specific fee math + global/fee-config RPC caches are not needed.
//! - Fee recipients are chosen deterministically (first known-valid), not
//!   randomly — correctness is identical; only load-spreading is skipped.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::{GenericError, TOKEN_PROGRAM, USDC, WSOL};

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

// --- Program + fixed accounts -------------------------------------------------

pub const AMM_PROGRAM: Pubkey = Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const GLOBAL_ACCOUNT: Pubkey =
    Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
pub const ASSOCIATED_TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM_PROGRAM: Pubkey =
    Pubkey::from_str_const("11111111111111111111111111111111");
pub const PROTOCOL_FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");
pub const FEE_PROGRAM: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const GLOBAL_VOLUME_ACCUMULATOR: Pubkey =
    Pubkey::from_str_const("C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw");
pub const FEE_CONFIG: Pubkey = Pubkey::from_str_const("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");
pub const DEFAULT_COIN_CREATOR_VAULT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("8N3GDaZ2iwN65oxVatKTLPNooAVUJTbfiVJ1ahyqwjSk");

/// First mayhem fee recipient (deterministic pick from the valid set).
pub const MAYHEM_FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS");
/// First buyback (trailing) fee recipient (deterministic pick from the valid set).
pub const PROTOCOL_EXTRA_FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");

pub const BUY_EXACT_QUOTE_IN_DISCRIMINATOR: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

const CREATOR_VAULT_SEED: &[u8] = b"creator_vault";
const POOL_V2_SEED: &[u8] = b"pool-v2";
const USER_VOLUME_ACCUMULATOR_SEED: &[u8] = b"user_volume_accumulator";

fn token_program() -> Pubkey {
    Pubkey::from_str_const(TOKEN_PROGRAM)
}

// --- PDA helpers --------------------------------------------------------------

/// Coin-creator vault authority PDA (falls back to the default authority when
/// the pool has no creator).
pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    if *coin_creator == Pubkey::default() {
        return DEFAULT_COIN_CREATOR_VAULT_AUTHORITY;
    }
    Pubkey::find_program_address(&[CREATOR_VAULT_SEED, coin_creator.as_ref()], &AMM_PROGRAM).0
}

/// Coin-creator vault ATA (quote mint, standard Token program).
pub fn coin_creator_vault_ata(coin_creator: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    ata(&coin_creator_vault_authority(coin_creator), quote_mint, &token_program())
}

/// `pool-v2` PDA for a base mint.
pub fn pool_v2_pda(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[POOL_V2_SEED, base_mint.as_ref()], &AMM_PROGRAM).0
}

/// Per-user volume accumulator PDA.
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()], &AMM_PROGRAM).0
}

/// Fee-recipient ATA (quote mint, standard Token program).
pub fn fee_recipient_ata(recipient: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    ata(recipient, quote_mint, &token_program())
}

fn is_stable(mint: &Pubkey) -> bool {
    *mint == Pubkey::from_str_const(WSOL) || *mint == Pubkey::from_str_const(USDC)
}

// --- Accounts + builder -------------------------------------------------------

/// Pool accounts for a PumpSwap swap. Populate from the parsed `PumpfunAmmPool`
/// (`base_mint`, `quote_mint`, `pool_base_token_account`, `pool_quote_token_account`,
/// `coin_creator`). `is_cashback` / `is_mayhem` come from pool/global state when
/// known (default `false`).
#[derive(Debug, Clone)]
pub struct PumpSwapAccounts {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub is_cashback: bool,
    pub is_mayhem: bool,
}

enum Side {
    /// Spend quote (WSOL/USDC), receive base — `buy_exact_quote_in`.
    Buy,
    /// Spend base, receive quote — `sell`.
    Sell,
}

/// Build instructions for one PumpSwap leg. Direction is inferred from the
/// leg's mints relative to the pool (quote mint must be WSOL or USDC).
pub fn build_swap(
    accounts: &PumpSwapAccounts,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    if !is_stable(&accounts.quote_mint) {
        return Err("PumpSwap executor requires the pool quote mint to be WSOL or USDC".into());
    }

    let side = if leg.input_mint == accounts.quote_mint && leg.output_mint == accounts.base_mint {
        Side::Buy
    } else if leg.input_mint == accounts.base_mint && leg.output_mint == accounts.quote_mint {
        Side::Sell
    } else {
        return Err("swap leg mints do not match this pool".into());
    };

    let user_base_ata = ata(&leg.payer, &accounts.base_mint, &accounts.base_token_program);
    let user_quote_ata = ata(&leg.payer, &accounts.quote_mint, &accounts.quote_token_program);

    // Fee recipient: deterministic valid pick (mayhem set when flagged).
    let fee_recipient = if accounts.is_mayhem { MAYHEM_FEE_RECIPIENT } else { PROTOCOL_FEE_RECIPIENT };
    let fee_recipient_token_account = fee_recipient_ata(&fee_recipient, &accounts.quote_mint);
    let coin_creator_authority = coin_creator_vault_authority(&accounts.coin_creator);
    let coin_creator_ata = coin_creator_vault_ata(&accounts.coin_creator, &accounts.quote_mint);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(6);

    // Input-side ATA setup.
    let (input_mint, input_program) = match side {
        Side::Buy => (accounts.quote_mint, accounts.quote_token_program),
        Side::Sell => (accounts.base_mint, accounts.base_token_program),
    };
    let (output_mint, output_program) = match side {
        Side::Buy => (accounts.base_mint, accounts.base_token_program),
        Side::Sell => (accounts.quote_mint, accounts.quote_token_program),
    };
    if opts.wrap_input_sol && input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &input_mint, &input_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &output_mint, &output_program));
    }

    // Shared leading accounts (0..=18).
    let mut metas = vec![
        AccountMeta::new(accounts.pool, false),                    // 0 pool
        AccountMeta::new(leg.payer, true),                         // 1 user (signer)
        AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),          // 2 global
        AccountMeta::new_readonly(accounts.base_mint, false),      // 3 base mint
        AccountMeta::new_readonly(accounts.quote_mint, false),     // 4 quote mint
        AccountMeta::new(user_base_ata, false),                    // 5 user base ata
        AccountMeta::new(user_quote_ata, false),                   // 6 user quote ata
        AccountMeta::new(accounts.pool_base_token_account, false), // 7 pool base
        AccountMeta::new(accounts.pool_quote_token_account, false),// 8 pool quote
        AccountMeta::new_readonly(fee_recipient, false),           // 9 fee recipient
        AccountMeta::new(fee_recipient_token_account, false),      // 10 fee recipient ata
        AccountMeta::new_readonly(accounts.base_token_program, false), // 11 base token program
        AccountMeta::new_readonly(accounts.quote_token_program, false),// 12 quote token program
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),          // 13 system program
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM, false),// 14 associated token program
        AccountMeta::new_readonly(EVENT_AUTHORITY, false),         // 15 event authority
        AccountMeta::new_readonly(AMM_PROGRAM, false),             // 16 amm program
        AccountMeta::new(coin_creator_ata, false),                 // 17 coin creator vault ata
        AccountMeta::new_readonly(coin_creator_authority, false),  // 18 coin creator vault authority
    ];

    let has_creator = accounts.coin_creator != Pubkey::default();
    let data: Vec<u8> = match side {
        Side::Buy => {
            // Volume accumulators sit before fee config on buys.
            metas.push(AccountMeta::new(GLOBAL_VOLUME_ACCUMULATOR, false));
            metas.push(AccountMeta::new(user_volume_accumulator_pda(&leg.payer), false));
            metas.push(AccountMeta::new_readonly(FEE_CONFIG, false));
            metas.push(AccountMeta::new_readonly(FEE_PROGRAM, false));
            if accounts.is_cashback {
                metas.push(AccountMeta::new(
                    ata(&user_volume_accumulator_pda(&leg.payer), &wsol(), &token_program()),
                    false,
                ));
            }
            if has_creator {
                metas.push(AccountMeta::new_readonly(pool_v2_pda(&accounts.base_mint), false));
            }
            let track_volume: u8 = accounts.is_cashback as u8;
            let mut d = Vec::with_capacity(25);
            d.extend_from_slice(&BUY_EXACT_QUOTE_IN_DISCRIMINATOR);
            d.extend_from_slice(&leg.amount_in.to_le_bytes()); // spendable quote in
            d.extend_from_slice(&leg.min_amount_out.to_le_bytes()); // min base out
            d.push(track_volume);
            d
        }
        Side::Sell => {
            metas.push(AccountMeta::new_readonly(FEE_CONFIG, false));
            metas.push(AccountMeta::new_readonly(FEE_PROGRAM, false));
            if accounts.is_cashback {
                metas.push(AccountMeta::new(
                    ata(
                        &user_volume_accumulator_pda(&leg.payer),
                        &accounts.quote_mint,
                        &accounts.quote_token_program,
                    ),
                    false,
                ));
                metas.push(AccountMeta::new(user_volume_accumulator_pda(&leg.payer), false));
            }
            if has_creator {
                metas.push(AccountMeta::new_readonly(pool_v2_pda(&accounts.base_mint), false));
            }
            let mut d = Vec::with_capacity(24);
            d.extend_from_slice(&SELL_DISCRIMINATOR);
            d.extend_from_slice(&leg.amount_in.to_le_bytes()); // base amount in
            d.extend_from_slice(&leg.min_amount_out.to_le_bytes()); // min quote out
            d
        }
    };

    // Trailing buyback fee recipient + its ATA (both sides).
    metas.push(AccountMeta::new_readonly(PROTOCOL_EXTRA_FEE_RECIPIENT, false));
    metas.push(AccountMeta::new(
        fee_recipient_ata(&PROTOCOL_EXTRA_FEE_RECIPIENT, &accounts.quote_mint),
        false,
    ));

    ixs.push(Instruction::new_with_bytes(AMM_PROGRAM, &data, metas));

    if opts.close_wsol {
        if input_mint == wsol() {
            ixs.push(close_account_ix(&input_program, &user_quote_ata, &leg.payer));
        }
        if output_mint == wsol() {
            ixs.push(close_account_ix(&output_program, &user_quote_ata, &leg.payer));
        }
    }

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn accounts(coin_creator: Pubkey, cashback: bool) -> PumpSwapAccounts {
        PumpSwapAccounts {
            pool: pk(1),
            base_mint: pk(2),
            quote_mint: wsol(),
            pool_base_token_account: pk(3),
            pool_quote_token_account: pk(4),
            coin_creator,
            base_token_program: token_program(),
            quote_token_program: token_program(),
            is_cashback: cashback,
            is_mayhem: false,
        }
    }

    fn buy_leg() -> SwapLeg {
        SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 500 }
    }

    fn sell_leg() -> SwapLeg {
        SwapLeg { payer: pk(9), input_mint: pk(2), output_mint: wsol(), amount_in: 500, min_amount_out: 900_000 }
    }

    #[test]
    fn buy_layout_with_creator_no_cashback() {
        let ixs = build_swap(&accounts(pk(7), false), &buy_leg(), &SwapOptions::default()).unwrap();
        let ix = &ixs[0];
        assert_eq!(ix.program_id, AMM_PROGRAM);
        // 19 base + global_vol + user_vol + fee_config + fee_program + pool_v2 + extra + extra_ata = 26
        assert_eq!(ix.accounts.len(), 26);
        assert_eq!(ix.accounts[0].pubkey, pk(1));
        assert!(ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[19].pubkey, GLOBAL_VOLUME_ACCUMULATOR);
        assert_eq!(&ix.data[..8], &BUY_EXACT_QUOTE_IN_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 500);
        assert_eq!(ix.data[24], 0); // track_volume off
    }

    #[test]
    fn buy_cashback_sets_track_volume_and_extra_account() {
        let ixs = build_swap(&accounts(pk(7), true), &buy_leg(), &SwapOptions::default()).unwrap();
        let ix = &ixs[0];
        assert_eq!(ix.accounts.len(), 27); // + uva wsol ata
        assert_eq!(ix.data[24], 1);
    }

    #[test]
    fn sell_layout_no_volume_accumulator_on_normal_quote() {
        let ixs = build_swap(&accounts(pk(7), false), &sell_leg(), &SwapOptions::default()).unwrap();
        let ix = &ixs[0];
        // 19 base + fee_config + fee_program + pool_v2 + extra + extra_ata = 24
        assert_eq!(ix.accounts.len(), 24);
        assert_eq!(&ix.data[..8], &SELL_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 500);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 900_000);
    }

    #[test]
    fn no_creator_omits_pool_v2() {
        let ixs = build_swap(&accounts(Pubkey::default(), false), &buy_leg(), &SwapOptions::default()).unwrap();
        // 26 - pool_v2 = 25
        assert_eq!(ixs[0].accounts.len(), 25);
    }

    #[test]
    fn rejects_non_stable_quote() {
        let mut a = accounts(pk(7), false);
        a.quote_mint = pk(88);
        let leg = SwapLeg { payer: pk(9), input_mint: pk(88), output_mint: pk(2), amount_in: 1, min_amount_out: 0 };
        assert!(build_swap(&a, &leg, &SwapOptions::default()).is_err());
    }
}
