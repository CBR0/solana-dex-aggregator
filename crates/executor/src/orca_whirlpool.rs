//! Orca Whirlpool swap instruction builder.
//!
//! Account order + args verified against the whirlpool program's classic
//! `swap` instruction (`instructions/swap.rs`):
//! `swap(amount, other_amount_threshold, sqrt_price_limit, amount_specified_is_input, a_to_b)`,
//! discriminator `sha256("global:swap")[..8]`, 11 named accounts (token_program,
//! authority, whirlpool, owner/vault A + B, tick_array_0/1/2, oracle).
//!
//! Classic path only (SPL Token, no Token-2022 / transfer hooks); pools with
//! a Token-2022 mint would need the `swap_v2` variant. The quote layer already
//! covers those via `orca_whirlpools_core`; execution builds for the common
//! SPL case, matching the other classic builders here.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use orca_whirlpool::{
    derive_oracle_pda, swap_tick_array_pdas, WhirlpoolPool, MAX_SQRT_PRICE, MIN_SQRT_PRICE,
    ORCA_WHIRLPOOL_PROGRAM,
};
use solroute_core::{GenericError, TOKEN_PROGRAM};

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const(ORCA_WHIRLPOOL_PROGRAM);

/// `swap` discriminator = sha256("global:swap")[..8].
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Build the classic `swap` instructions for one Whirlpool hop (exact-in).
pub fn build_swap(
    pool: &WhirlpoolPool,
    pool_pubkey: Pubkey,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    // a_to_b = spending token A (price moves down).
    let a_to_b = if leg.input_mint == pool.token_mint_a {
        true
    } else if leg.input_mint == pool.token_mint_b {
        false
    } else {
        return Err("input_mint does not belong to this whirlpool".into());
    };

    let token_program = Pubkey::from_str_const(TOKEN_PROGRAM);
    // Classic swap => both sides are SPL Token.
    let input_token_account = ata(&leg.payer, &leg.input_mint, &token_program);
    let output_token_account = ata(&leg.payer, &leg.output_mint, &token_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &token_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &token_program));
    }

    // owner/vault A and B are addressed by the pool's fixed A/B orientation,
    // independent of swap direction.
    let (owner_a, owner_b) = if leg.input_mint == pool.token_mint_a {
        (input_token_account, output_token_account)
    } else {
        (output_token_account, input_token_account)
    };

    let oracle = derive_oracle_pda(&pool_pubkey);
    let [ta0, ta1, ta2] =
        swap_tick_array_pdas(&pool_pubkey, pool.tick_current_index, pool.tick_spacing, a_to_b);

    let metas = vec![
        AccountMeta::new_readonly(token_program, false), // 0 token_program
        AccountMeta::new(leg.payer, true),               // 1 token_authority (signer)
        AccountMeta::new(pool_pubkey, false),            // 2 whirlpool
        AccountMeta::new(owner_a, false),                // 3 token_owner_account_a
        AccountMeta::new(pool.token_vault_a, false),     // 4 token_vault_a
        AccountMeta::new(owner_b, false),                // 5 token_owner_account_b
        AccountMeta::new(pool.token_vault_b, false),     // 6 token_vault_b
        AccountMeta::new(ta0, false),                    // 7 tick_array_0
        AccountMeta::new(ta1, false),                    // 8 tick_array_1
        AccountMeta::new(ta2, false),                    // 9 tick_array_2
        AccountMeta::new_readonly(oracle, false),        // 10 oracle
    ];

    // Price limit: the far bound in the swap direction (no user limit).
    let sqrt_price_limit: u128 = if a_to_b { MIN_SQRT_PRICE } else { MAX_SQRT_PRICE };

    // data: disc(8) + amount u64 + other_amount_threshold u64 +
    //       sqrt_price_limit u128 + amount_specified_is_input u8 + a_to_b u8
    let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1 + 1);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&leg.amount_in.to_le_bytes());
    data.extend_from_slice(&leg.min_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8); // amount_specified_is_input = true (exact in)
    data.push(a_to_b as u8);

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    if opts.close_wsol {
        if leg.input_mint == wsol() {
            ixs.push(close_account_ix(&token_program, &input_token_account, &leg.payer));
        }
        if leg.output_mint == wsol() {
            ixs.push(close_account_ix(&token_program, &output_token_account, &leg.payer));
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

    fn pool() -> WhirlpoolPool {
        WhirlpoolPool {
            whirlpools_config: pk(1),
            whirlpool_bump: [0],
            tick_spacing: 64,
            fee_tier_index_seed: 64u16.to_le_bytes(),
            fee_rate: 3000,
            protocol_fee_rate: 0,
            liquidity: 1_000_000,
            sqrt_price: 1u128 << 64,
            tick_current_index: 0,
            protocol_fee_owed_a: 0,
            protocol_fee_owed_b: 0,
            token_mint_a: pk(10),
            token_vault_a: pk(11),
            fee_growth_global_a: 0,
            token_mint_b: pk(20),
            token_vault_b: pk(21),
            fee_growth_global_b: 0,
            reward_last_updated_timestamp: 0,
            reward_infos: core::array::from_fn(|_| orca_whirlpool::WhirlpoolRewardInfo {
                mint: Pubkey::default(),
                vault: Pubkey::default(),
                authority: Pubkey::default(),
                emissions_per_second_x64: 0,
                growth_global_x64: 0,
            }),
        }
    }

    #[test]
    fn a_to_b_layout_and_data() {
        let p = pool();
        let leg = SwapLeg {
            payer: pk(99),
            input_mint: p.token_mint_a,
            output_mint: p.token_mint_b,
            amount_in: 1_000_000,
            min_amount_out: 990_000,
        };
        let opts = SwapOptions { create_input_ata: false, create_output_ata: false, wrap_input_sol: false, close_wsol: false };
        let ixs = build_swap(&p, pk(50), &leg, &opts).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 11);
        // disc + a_to_b flag = 1 (last byte).
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(*ix.data.last().unwrap(), 1u8);
        // amount_specified_is_input just before it.
        assert_eq!(ix.data[ix.data.len() - 2], 1u8);
        // token_authority is the signer at index 1.
        assert!(ix.accounts[1].is_signer);
        // vault A at index 4.
        assert_eq!(ix.accounts[4].pubkey, p.token_vault_a);
    }

    #[test]
    fn b_to_a_flips_direction_and_price_limit() {
        let p = pool();
        let leg = SwapLeg {
            payer: pk(99),
            input_mint: p.token_mint_b,
            output_mint: p.token_mint_a,
            amount_in: 500,
            min_amount_out: 1,
        };
        let opts = SwapOptions { create_input_ata: false, create_output_ata: false, wrap_input_sol: false, close_wsol: false };
        let ix = build_swap(&p, pk(50), &leg, &opts).unwrap().pop().unwrap();
        assert_eq!(*ix.data.last().unwrap(), 0u8); // a_to_b = false
        // sqrt_price_limit = MAX for b_to_a.
        let limit = u128::from_le_bytes(ix.data[24..40].try_into().unwrap());
        assert_eq!(limit, MAX_SQRT_PRICE);
    }
}
