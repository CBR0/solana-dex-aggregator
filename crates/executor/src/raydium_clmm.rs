//! Raydium CLMM (concentrated liquidity) swap instruction builder.
//!
//! Account order + args verified against the raydium-clmm program
//! (`instructions/swap_v2.rs`): `swap_v2(amount, other_amount_threshold,
//! sqrt_price_limit_x64, is_base_input)`, discriminator
//! `sha256("global:swap_v2")[..8]`, 13 named accounts + tick-array remaining
//! accounts. Tick arrays crossed by the swap are computed from the pool's
//! current tick + bitmap via `raydium_clmm::tick_arrays`.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use raydium_clmm::tick_arrays::compute_clmm_remaining_accounts;
use raydium_clmm::RaydiumCLMMPool;
use solroute_core::{GenericError, MEMO_PROGRAM_V2, TOKEN_PROGRAM, TOKEN_PROGRAM_2022};

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// `swap_v2` discriminator = sha256("global:swap_v2")[..8].
pub const SWAP_V2_DISCRIMINATOR: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];

/// Assemble the swap_v2 instructions given pre-computed tick-array accounts (pure).
#[allow(clippy::too_many_arguments)]
pub fn assemble_swap(
    pool: &RaydiumCLMMPool,
    pool_pubkey: Pubkey,
    tick_arrays: &[Pubkey],
    input_program: Pubkey,
    output_program: Pubkey,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    let input_is_0 = leg.input_mint == pool.token_mint_0;
    if !input_is_0 && leg.input_mint != pool.token_mint_1 {
        return Err("input_mint does not belong to this pool".into());
    }

    let (input_vault, output_vault) = if input_is_0 {
        (pool.token_vault_0, pool.token_vault_1)
    } else {
        (pool.token_vault_1, pool.token_vault_0)
    };

    let token_program = Pubkey::from_str_const(TOKEN_PROGRAM);
    let token_program_2022 = Pubkey::from_str_const(TOKEN_PROGRAM_2022);
    let memo_program = Pubkey::from_str_const(MEMO_PROGRAM_V2);

    let input_token_account = ata(&leg.payer, &leg.input_mint, &input_program);
    let output_token_account = ata(&leg.payer, &leg.output_mint, &output_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &input_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &output_program));
    }

    let mut metas = vec![
        AccountMeta::new(leg.payer, true),                        // 0 payer (signer)
        AccountMeta::new_readonly(pool.amm_config, false),        // 1 amm_config
        AccountMeta::new(pool_pubkey, false),                     // 2 pool_state
        AccountMeta::new(input_token_account, false),             // 3 input_token_account
        AccountMeta::new(output_token_account, false),            // 4 output_token_account
        AccountMeta::new(input_vault, false),                     // 5 input_vault
        AccountMeta::new(output_vault, false),                    // 6 output_vault
        AccountMeta::new(pool.observation_key, false),            // 7 observation_state
        AccountMeta::new_readonly(token_program, false),          // 8 token_program
        AccountMeta::new_readonly(token_program_2022, false),     // 9 token_program_2022
        AccountMeta::new_readonly(memo_program, false),           // 10 memo_program
        AccountMeta::new_readonly(leg.input_mint, false),         // 11 input_vault_mint
        AccountMeta::new_readonly(leg.output_mint, false),        // 12 output_vault_mint
    ];
    // Remaining: tick arrays crossed by the swap (writable).
    for ta in tick_arrays {
        metas.push(AccountMeta::new(*ta, false));
    }

    // data: [disc(8)][amount u64][other_amount_threshold u64][sqrt_price_limit_x64 u128][is_base_input u8]
    let mut data = Vec::with_capacity(41);
    data.extend_from_slice(&SWAP_V2_DISCRIMINATOR);
    data.extend_from_slice(&leg.amount_in.to_le_bytes());
    data.extend_from_slice(&leg.min_amount_out.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes()); // sqrt_price_limit_x64 = 0 (no limit)
    data.push(1u8); // is_base_input = true (exact in)

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    if opts.close_wsol {
        if leg.input_mint == wsol() {
            ixs.push(close_account_ix(&input_program, &input_token_account, &leg.payer));
        }
        if leg.output_mint == wsol() {
            ixs.push(close_account_ix(&output_program, &output_token_account, &leg.payer));
        }
    }

    Ok(ixs)
}

/// Compute the tick arrays crossed by the swap, then assemble the instructions.
/// `extension_data` is the tick-array-bitmap-extension account data when the
/// swap may reach far tick arrays (pass `None` for near-price swaps, ±512 arrays).
#[allow(clippy::too_many_arguments)]
pub fn build_swap(
    pool: &RaydiumCLMMPool,
    pool_pubkey: Pubkey,
    input_program: Pubkey,
    output_program: Pubkey,
    leg: &SwapLeg,
    extension_data: Option<&[u8]>,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    let input_is_0 = leg.input_mint == pool.token_mint_0;
    // is_buy = a_to_b = zero_for_one = spending token_0.
    let tick_arrays = compute_clmm_remaining_accounts(pool, &pool_pubkey, input_is_0, extension_data)?;
    assemble_swap(pool, pool_pubkey, &tick_arrays, input_program, output_program, leg, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn pool() -> RaydiumCLMMPool {
        use borsh::BorshDeserialize;
        let buf = [0u8; 2048];
        let mut slice = &buf[..];
        let mut p = RaydiumCLMMPool::deserialize(&mut slice).unwrap();
        p.amm_config = pk(20);
        p.token_mint_0 = wsol();
        p.token_mint_1 = pk(2);
        p.token_vault_0 = pk(3);
        p.token_vault_1 = pk(4);
        p.observation_key = pk(5);
        p
    }

    fn leg() -> SwapLeg {
        SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 42 }
    }

    #[test]
    fn swap_v2_layout_and_data() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let ticks = vec![pk(50), pk(51)];
        let ixs = assemble_swap(&pool(), pk(1), &ticks, tp, tp, &leg(), &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 15); // 13 named + 2 tick arrays
        assert!(ix.accounts[0].is_signer); // payer
        assert_eq!(ix.accounts[2].pubkey, pk(1)); // pool_state
        assert_eq!(ix.accounts[5].pubkey, pk(3)); // input_vault = vault_0 (input is mint_0)
        assert_eq!(ix.accounts[6].pubkey, pk(4)); // output_vault = vault_1
        assert_eq!(ix.accounts[7].pubkey, pk(5)); // observation
        assert_eq!(ix.accounts[13].pubkey, pk(50)); // first tick array
        assert_eq!(&ix.data[..8], &SWAP_V2_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 42);
        assert_eq!(ix.data[40], 1); // is_base_input
        assert_eq!(ix.data.len(), 41);
    }

    #[test]
    fn vault_direction_flips_for_mint_1_input() {
        let tp = Pubkey::from_str_const(TOKEN_PROGRAM);
        let l = SwapLeg { payer: pk(9), input_mint: pk(2), output_mint: wsol(), amount_in: 5, min_amount_out: 1 };
        let ixs = assemble_swap(&pool(), pk(1), &[pk(50)], tp, tp, &l, &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts[5].pubkey, pk(4)); // input_vault = vault_1
        assert_eq!(ix.accounts[6].pubkey, pk(3)); // output_vault = vault_0
    }
}
