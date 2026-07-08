//! Meteora DLMM swap instruction builder.
//!
//! Account order + data verified against MeteoraAg/dlmm-sdk IDL (`swap`) and the
//! commons PDA helpers. Uses the legacy `swap` instruction (15 named accounts +
//! bin-array remaining accounts), `swap(amount_in, min_amount_out)`,
//! discriminator `sha256("global:swap")[..8]`.
//!
//! DLMM prices liquidity in **bins**; a swap reads the bin array(s) covering the
//! active bin and the direction it moves. We pass the active array plus its two
//! neighbors — enough for typical trade sizes. Very large swaps that cross more
//! than one array boundary would need additional arrays.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};

use solroute_core::GenericError;

use crate::ata::{ata, close_account_ix, create_ata_idempotent, wrap_sol_ixs, wsol};
use crate::types::{SwapLeg, SwapOptions};

pub const PROGRAM_ID: Pubkey = Pubkey::from_str_const("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// `swap` discriminator = sha256("global:swap")[..8].
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Bins per bin-array account.
const MAX_BIN_PER_ARRAY: i64 = 70;

const BIN_ARRAY_SEED: &[u8] = b"bin_array";
const EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";

/// Bin-array PDA: `["bin_array", lb_pair, index_le_i64]`.
pub fn derive_bin_array(lb_pair: &Pubkey, index: i64) -> Pubkey {
    Pubkey::find_program_address(&[BIN_ARRAY_SEED, lb_pair.as_ref(), &index.to_le_bytes()], &PROGRAM_ID).0
}

fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[EVENT_AUTHORITY_SEED], &PROGRAM_ID).0
}

/// Pool accounts for a DLMM swap. Populate from the parsed `MeteoraDLMMPool`
/// (`reserve_x`, `reserve_y`, `token_x_mint`, `token_y_mint`, `oracle`, `active_id`).
#[derive(Debug, Clone)]
pub struct DlmmAccounts {
    pub lb_pair: Pubkey,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub oracle: Pubkey,
    pub active_id: i32,
    pub token_x_program: Pubkey,
    pub token_y_program: Pubkey,
}

/// Build the DLMM swap instructions for one leg (exact-in, slippage floor).
pub fn build_swap(
    accounts: &DlmmAccounts,
    leg: &SwapLeg,
    opts: &SwapOptions,
) -> Result<Vec<Instruction>, GenericError> {
    if leg.amount_in == 0 {
        return Err("amount_in cannot be zero".into());
    }
    let input_is_x = leg.input_mint == accounts.token_x_mint;
    if !input_is_x && leg.input_mint != accounts.token_y_mint {
        return Err("input_mint does not belong to this pool".into());
    }
    let input_program = if input_is_x { accounts.token_x_program } else { accounts.token_y_program };
    let output_program = if input_is_x { accounts.token_y_program } else { accounts.token_x_program };

    let user_token_in = ata(&leg.payer, &leg.input_mint, &input_program);
    let user_token_out = ata(&leg.payer, &leg.output_mint, &output_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(5);
    if opts.wrap_input_sol && leg.input_mint == wsol() {
        ixs.extend(wrap_sol_ixs(&leg.payer, leg.amount_in));
    } else if opts.create_input_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.input_mint, &input_program));
    }
    if opts.create_output_ata {
        ixs.push(create_ata_idempotent(&leg.payer, &leg.payer, &leg.output_mint, &output_program));
    }

    // 15 named accounts. Optional accounts (bitmap extension, host fee) are set
    // to the program id as the None placeholder, matching the SDK.
    let mut metas = vec![
        AccountMeta::new(accounts.lb_pair, false),                    // 0 lb_pair
        AccountMeta::new_readonly(PROGRAM_ID, false),                 // 1 bin_array_bitmap_extension (none)
        AccountMeta::new(accounts.reserve_x, false),                  // 2 reserve_x
        AccountMeta::new(accounts.reserve_y, false),                  // 3 reserve_y
        AccountMeta::new(user_token_in, false),                       // 4 user_token_in
        AccountMeta::new(user_token_out, false),                      // 5 user_token_out
        AccountMeta::new_readonly(accounts.token_x_mint, false),      // 6 token_x_mint
        AccountMeta::new_readonly(accounts.token_y_mint, false),      // 7 token_y_mint
        AccountMeta::new(accounts.oracle, false),                     // 8 oracle
        AccountMeta::new_readonly(PROGRAM_ID, false),                 // 9 host_fee_in (none)
        AccountMeta::new(leg.payer, true),                            // 10 user (signer)
        AccountMeta::new_readonly(accounts.token_x_program, false),   // 11 token_x_program
        AccountMeta::new_readonly(accounts.token_y_program, false),   // 12 token_y_program
        AccountMeta::new_readonly(event_authority(), false),          // 13 event_authority
        AccountMeta::new_readonly(PROGRAM_ID, false),                 // 14 program
    ];

    // Bin arrays covering the active bin ± one array (writable remaining accounts).
    let active_index = (accounts.active_id as i64).div_euclid(MAX_BIN_PER_ARRAY);
    for idx in [active_index - 1, active_index, active_index + 1] {
        metas.push(AccountMeta::new(derive_bin_array(&accounts.lb_pair, idx), false));
    }

    // data: [disc(8)][amount_in u64][min_amount_out u64]
    let mut data = [0u8; 24];
    data[..8].copy_from_slice(&SWAP_DISCRIMINATOR);
    data[8..16].copy_from_slice(&leg.amount_in.to_le_bytes());
    data[16..24].copy_from_slice(&leg.min_amount_out.to_le_bytes());

    ixs.push(Instruction::new_with_bytes(PROGRAM_ID, &data, metas));

    if opts.close_wsol {
        if leg.input_mint == wsol() {
            ixs.push(close_account_ix(&input_program, &user_token_in, &leg.payer));
        }
        if leg.output_mint == wsol() {
            ixs.push(close_account_ix(&output_program, &user_token_out, &leg.payer));
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

    fn accounts(active_id: i32) -> DlmmAccounts {
        DlmmAccounts {
            lb_pair: pk(1),
            token_x_mint: wsol(),
            token_y_mint: pk(2),
            reserve_x: pk(3),
            reserve_y: pk(4),
            oracle: pk(5),
            active_id,
            token_x_program: token_program(),
            token_y_program: token_program(),
        }
    }

    fn leg() -> SwapLeg {
        SwapLeg { payer: pk(9), input_mint: wsol(), output_mint: pk(2), amount_in: 1_000_000, min_amount_out: 42 }
    }

    #[test]
    fn swap_layout_and_bin_arrays() {
        let ixs = build_swap(&accounts(150), &leg(), &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 18); // 15 named + 3 bin arrays
        assert_eq!(ix.accounts[8].pubkey, pk(5)); // oracle
        assert_eq!(ix.accounts[10].pubkey, pk(9)); // user
        assert!(ix.accounts[10].is_signer);
        assert_eq!(ix.accounts[14].pubkey, PROGRAM_ID);
        // active_id 150 -> index 2; neighbors 1,2,3
        assert_eq!(ix.accounts[15].pubkey, derive_bin_array(&pk(1), 1));
        assert_eq!(ix.accounts[16].pubkey, derive_bin_array(&pk(1), 2));
        assert_eq!(ix.accounts[17].pubkey, derive_bin_array(&pk(1), 3));
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
    }

    #[test]
    fn negative_active_id_index_floors() {
        // active_id -1 -> div_euclid(70) = -1, neighbors -2,-1,0
        let ixs = build_swap(&accounts(-1), &leg(), &SwapOptions::default()).unwrap();
        let ix = ixs.last().unwrap();
        assert_eq!(ix.accounts[15].pubkey, derive_bin_array(&pk(1), -2));
        assert_eq!(ix.accounts[16].pubkey, derive_bin_array(&pk(1), -1));
        assert_eq!(ix.accounts[17].pubkey, derive_bin_array(&pk(1), 0));
    }

    #[test]
    fn rejects_foreign_mint() {
        let mut l = leg();
        l.input_mint = pk(200);
        assert!(build_swap(&accounts(0), &l, &SwapOptions::default()).is_err());
    }
}
