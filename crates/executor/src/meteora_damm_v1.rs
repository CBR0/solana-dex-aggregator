//! Meteora DAMM V1 (dynamic AMM) swap instruction builder.
//!
//! Account order + data layout verified against the MeteoraAg/damm-v1-sdk
//! program (`instructions/swap.rs`) and live mainnet swap transactions:
//! `swap(in_amount, minimum_out_amount)`, discriminator `sha256("global:swap")[..8]`,
//! 15 accounts. DAMM V1 pools deposit/withdraw through Meteora **dynamic vaults**,
//! so the swap references each side's vault, token-vault, vault-LP, and vault-LP-mint.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::{GenericError, TOKEN_PROGRAM};

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB");
/// Meteora dynamic-vault program (owns the vaults the pool routes through).
pub const VAULT_PROGRAM: Pubkey = Pubkey::from_str_const("24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi");

/// `swap` discriminator = sha256("global:swap")[..8].
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

const TOKEN_VAULT_SEED: &[u8] = b"token_vault";
const LP_MINT_SEED: &[u8] = b"lp_mint";

/// Token vault PDA for a dynamic vault: `["token_vault", vault]`.
pub fn derive_token_vault(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[TOKEN_VAULT_SEED, vault.as_ref()], &VAULT_PROGRAM).0
}

/// Vault LP-mint PDA: `["lp_mint", vault]`.
pub fn derive_vault_lp_mint(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[LP_MINT_SEED, vault.as_ref()], &VAULT_PROGRAM).0
}

/// Pool accounts for a DAMM V1 swap. Populate from the parsed `MeteoraDAMMPool`
/// (`a_vault`, `b_vault`, `a_vault_lp`, `b_vault_lp`, mints, `protocol_token_{a,b}_fee`).
#[derive(Debug, Clone)]
pub struct DammV1Accounts {
    pub pool: Pubkey,
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub a_vault: Pubkey,
    pub b_vault: Pubkey,
    pub a_vault_lp: Pubkey,
    pub b_vault_lp: Pubkey,
    pub protocol_token_a_fee: Pubkey,
    pub protocol_token_b_fee: Pubkey,
}

/// Build the DAMM V1 swap instructions for one leg (exact-in, slippage floor).
pub fn build_swap(
    accounts: &DammV1Accounts,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    let input_is_a = leg.input_mint == accounts.token_a_mint;
    if !input_is_a && leg.input_mint != accounts.token_b_mint {
        return Err("input_mint does not belong to this pool".into());
    }

    let token_program = Pubkey::from_str_const(TOKEN_PROGRAM);
    let user_source = ata(&leg.payer, &leg.input_mint, &token_program);
    let user_destination = ata(&leg.payer, &leg.output_mint, &token_program);

    // Protocol fee account must match the input (source) mint.
    let protocol_token_fee =
        if input_is_a { accounts.protocol_token_a_fee } else { accounts.protocol_token_b_fee };

    let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &token_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &token_program));
    }

    let metas = vec![
        AccountMeta::new(accounts.pool, false),                       // 0 pool
        AccountMeta::new(user_source, false),                         // 1 user source token
        AccountMeta::new(user_destination, false),                    // 2 user destination token
        AccountMeta::new(accounts.a_vault, false),                    // 3 a_vault
        AccountMeta::new(accounts.b_vault, false),                    // 4 b_vault
        AccountMeta::new(derive_token_vault(&accounts.a_vault), false),// 5 a_token_vault
        AccountMeta::new(derive_token_vault(&accounts.b_vault), false),// 6 b_token_vault
        AccountMeta::new(derive_vault_lp_mint(&accounts.a_vault), false), // 7 a_vault_lp_mint
        AccountMeta::new(derive_vault_lp_mint(&accounts.b_vault), false), // 8 b_vault_lp_mint
        AccountMeta::new(accounts.a_vault_lp, false),                 // 9 a_vault_lp
        AccountMeta::new(accounts.b_vault_lp, false),                 // 10 b_vault_lp
        AccountMeta::new(protocol_token_fee, false),                  // 11 protocol_token_fee
        AccountMeta::new(leg.payer, true),                            // 12 user (signer)
        AccountMeta::new_readonly(VAULT_PROGRAM, false),              // 13 vault program
        AccountMeta::new_readonly(token_program, false),              // 14 token program
    ];

    // data: [disc(8)][in_amount u64][minimum_out_amount u64]
    let mut data = [0u8; 24];
    data[..8].copy_from_slice(&SWAP_DISCRIMINATOR);
    data[8..16].copy_from_slice(&leg.amount_in.to_le_bytes());
    data[16..24].copy_from_slice(&leg.min_amount_out.to_le_bytes());

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // From a real mainnet DAMM V1 swap (sig 3DvdXQyVx3wXUvEifK2m...):
    //   a_vault           = A4NBzSvC6SbwFfBGFd7MAsHyUDGx9kXxWrQdusQ9tJ6z
    //   a_token_vault [5]  = E2m4WYm3LbbVifVY7KMKAiMcizr2DgrKAgw7P7EZVjRa
    //   a_vault_lp_mint[7] = GLeewidJLKiCsbMsdrVSwYUMcVhp9Fu4yXkDC7zbDQK7
    #[test]
    fn pda_seeds_match_onchain() {
        let a_vault = Pubkey::from_str("A4NBzSvC6SbwFfBGFd7MAsHyUDGx9kXxWrQdusQ9tJ6z").unwrap();
        assert_eq!(
            derive_token_vault(&a_vault),
            Pubkey::from_str("E2m4WYm3LbbVifVY7KMKAiMcizr2DgrKAgw7P7EZVjRa").unwrap()
        );
        assert_eq!(
            derive_vault_lp_mint(&a_vault),
            Pubkey::from_str("GLeewidJLKiCsbMsdrVSwYUMcVhp9Fu4yXkDC7zbDQK7").unwrap()
        );
    }

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn accounts() -> DammV1Accounts {
        DammV1Accounts {
            pool: pk(1),
            token_a_mint: wsol(),
            token_b_mint: pk(2),
            a_vault: pk(3),
            b_vault: pk(4),
            a_vault_lp: pk(5),
            b_vault_lp: pk(6),
            protocol_token_a_fee: pk(7),
            protocol_token_b_fee: pk(8),
        }
    }

    #[test]
    fn swap_layout_and_fee_selection() {
        let leg = SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 42 };
        let ixs = build_swap(&accounts(), &leg, &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 15);
        assert_eq!(ix.accounts[11].pubkey, pk(7)); // input=A -> protocol_token_a_fee
        assert_eq!(ix.accounts[12].pubkey, pk(9)); // user signer
        assert!(ix.accounts[12].is_signer);
        assert_eq!(ix.accounts[13].pubkey, VAULT_PROGRAM);
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 42);
    }

    #[test]
    fn fee_selection_flips_for_token_b_input() {
        let leg = SwapLeg { payer: pk(9), input_mint: pk(2), output_mint: wsol(), amount_in: 5, min_amount_out: 1 };
        let ixs = build_swap(&accounts(), &leg, &SwapOptions::default()).unwrap();
        assert_eq!(ixs.last().unwrap().accounts[11].pubkey, pk(8)); // input=B -> protocol_token_b_fee
    }
}
