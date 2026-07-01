use std::error::Error;

use solana_pubkey::Pubkey;

use crate::RaydiumCLMMPool;
use crate::RAYDIUM_CLMM;

/// Number of ticks stored in each tick array account (Raydium CLMM constant)
const TICK_ARRAY_SIZE: i32 = 60;

/// Number of tick arrays to include as remaining accounts for a swap
const NUM_REMAINING_TICK_ARRAYS: usize = 3;

const TICK_ARRAY_BITMAP_SEED: &[u8] = b"pool_tick_array_bitmap_extension";
const TICK_ARRAY_SEED: &[u8] = b"tick_array";

/// Derive the tick array bitmap extension PDA address for a pool.
pub fn pda_array_bitmap_address(
    pool_id: &Pubkey,
) -> Result<(Pubkey, u8), Box<dyn Error + Send + Sync>> {
    let program_id = Pubkey::from_str_const(RAYDIUM_CLMM);
    let (address, bump) =
        Pubkey::find_program_address(&[TICK_ARRAY_BITMAP_SEED, pool_id.as_ref()], &program_id);
    Ok((address, bump))
}

/// Derive a tick array PDA address for a given pool and start index.
pub fn pda_tick_array_address(
    pool_id: &Pubkey,
    start_index: i32,
) -> Result<(Pubkey, u8), Box<dyn Error + Send + Sync>> {
    let program_id = Pubkey::from_str_const(RAYDIUM_CLMM);
    let (address, bump) = Pubkey::find_program_address(
        &[
            TICK_ARRAY_SEED,
            pool_id.as_ref(),
            &start_index.to_be_bytes(),
        ],
        &program_id,
    );
    Ok((address, bump))
}

/// Calculate the start index of the tick array containing the given tick.
///
/// Each tick array covers `tick_spacing * TICK_ARRAY_SIZE` ticks.
/// The start index is always aligned to tick array boundaries.
fn get_tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_per_array = tick_spacing as i32 * TICK_ARRAY_SIZE;
    tick.div_euclid(ticks_per_array) * ticks_per_array
}

/// Check if a tick array is initialized using the pool's tick_array_bitmap (indices -512 to +511).
fn is_tick_array_initialized_in_bitmap(bitmap: &[u64; 16], tick_array_index: i32) -> bool {
    let position = tick_array_index + 512;
    if !(0..1024).contains(&position) {
        return false;
    }
    let word = position as usize / 64;
    let bit = position as u32 % 64;
    bitmap[word] & (1u64 << bit) != 0
}

/// Raydium CLMM TickArrayBitmapExtension layout (after 8-byte discriminator):
/// - pool_id: Pubkey (32 bytes)
/// - positive_tick_array_bitmap: [[u64; 8]; 14] = 896 bytes (indices 512..7679)
/// - negative_tick_array_bitmap: [[u64; 8]; 14] = 896 bytes (indices -7680..-513)
const EXTENSION_BITMAP_OFFSET: usize = 8 + 32; // discriminator + pool_id
const EXTENSION_GROUP_SIZE: usize = 512; // bits per group (8 x u64)
const EXTENSION_NUM_GROUPS: usize = 14;

/// Check if a tick array is initialized using the bitmap extension account data.
/// Returns false if tick_array_index is within the in-pool bitmap range (+-512).
fn is_tick_array_initialized_in_extension(extension_data: &[u8], tick_array_index: i32) -> bool {
    if (-512..=511).contains(&tick_array_index) {
        return false; // Handled by in-pool bitmap
    }

    if tick_array_index > 0 {
        // Positive extension: indices 512..7679
        let offset_idx = tick_array_index - 512;
        if offset_idx < 0 || offset_idx as usize >= EXTENSION_GROUP_SIZE * EXTENSION_NUM_GROUPS {
            return false;
        }
        let byte_start = EXTENSION_BITMAP_OFFSET;
        let bit_offset = offset_idx as usize;
        let word_idx = bit_offset / 64;
        let bit_idx = bit_offset % 64;
        let data_offset = byte_start + word_idx * 8;
        if data_offset + 8 > extension_data.len() {
            return false;
        }
        let word = u64::from_le_bytes(
            extension_data[data_offset..data_offset + 8]
                .try_into()
                .unwrap_or([0; 8]),
        );
        word & (1u64 << bit_idx) != 0
    } else {
        // Negative extension: indices -7680..-513
        let offset_idx = -tick_array_index - 513;
        if offset_idx < 0 || offset_idx as usize >= EXTENSION_GROUP_SIZE * EXTENSION_NUM_GROUPS {
            return false;
        }
        // Negative bitmap starts after positive bitmap (14 groups x 8 words x 8 bytes)
        let byte_start = EXTENSION_BITMAP_OFFSET + EXTENSION_NUM_GROUPS * 8 * 8;
        let bit_offset = offset_idx as usize;
        let word_idx = bit_offset / 64;
        let bit_idx = bit_offset % 64;
        let data_offset = byte_start + word_idx * 8;
        if data_offset + 8 > extension_data.len() {
            return false;
        }
        let word = u64::from_le_bytes(
            extension_data[data_offset..data_offset + 8]
                .try_into()
                .unwrap_or([0; 8]),
        );
        word & (1u64 << bit_idx) != 0
    }
}

/// Check if a tick array is initialized, checking both in-pool bitmap and extension.
fn is_tick_array_initialized(
    bitmap: &[u64; 16],
    extension_data: Option<&[u8]>,
    tick_array_index: i32,
) -> bool {
    // First check the in-pool bitmap (covers -512 to +511)
    if (-512..=511).contains(&tick_array_index) {
        return is_tick_array_initialized_in_bitmap(bitmap, tick_array_index);
    }
    // Then check extension bitmap if available
    if let Some(ext) = extension_data {
        return is_tick_array_initialized_in_extension(ext, tick_array_index);
    }
    false
}

/// Derive tick array PDAs for all initialized tick arrays near the current tick.
/// Scans both directions (buy and sell) from the current tick array, collecting
/// up to `NUM_REMAINING_TICK_ARRAYS` per direction. Used during cold start to
/// pre-fetch tick array accounts without a massive getProgramAccounts call.
pub fn derive_pool_tick_array_pdas(
    pool: &RaydiumCLMMPool,
    pool_id: &Pubkey,
) -> Vec<Pubkey> {
    let ticks_per_array = pool.tick_spacing as i32 * TICK_ARRAY_SIZE;
    if ticks_per_array == 0 {
        return vec![];
    }

    let current_start = get_tick_array_start_index(pool.tick_current, pool.tick_spacing);
    let mut pdas = Vec::new();

    // Search both directions from the current tick array.
    for direction in [-1i32, 1] {
        let step = direction * ticks_per_array;
        let mut pos = current_start;
        let mut found = 0;
        while found < NUM_REMAINING_TICK_ARRAYS {
            let idx = pos.div_euclid(ticks_per_array);
            // In-pool bitmap covers tick array indices -512..+511.
            if idx < -512 || idx > 511 {
                break;
            }
            if is_tick_array_initialized_in_bitmap(&pool.tick_array_bitmap, idx) {
                if let Ok((pda, _)) = pda_tick_array_address(pool_id, pos) {
                    pdas.push(pda);
                    found += 1;
                }
            }
            pos += step;
        }
    }

    pdas.sort();
    pdas.dedup();
    pdas
}

/// Compute the tick array remaining accounts for a CLMM swap locally.
///
/// This replaces the external HTTP API call to `clmm.liquidharmony.xyz/remaining_accounts`.
/// Pure computation using pool state data - no RPC or network calls needed.
///
/// For a swap:
/// - **Buy** (a_to_b = true, tick decreases): initialized tick arrays at/below current tick
/// - **Sell** (b_to_a = true, tick increases): initialized tick arrays at/above current tick
///
/// Only returns PDAs for tick arrays that are initialized in the bitmap (exist on-chain).
/// The Raydium CLMM program breaks loading remaining accounts at the first account with
/// wrong data_len, so passing non-existent tick arrays would prevent valid ones from loading.
///
/// `extension_data`: raw account data of the TickArrayBitmapExtension account.
/// Required for pools where tick arrays fall outside the in-pool bitmap range (+-512).
pub fn compute_clmm_remaining_accounts(
    pool: &RaydiumCLMMPool,
    pool_id: &Pubkey,
    is_buy: bool,
    extension_data: Option<&[u8]>,
) -> Result<Vec<Pubkey>, Box<dyn Error + Send + Sync>> {
    let tick_spacing = pool.tick_spacing;
    let ticks_per_array = tick_spacing as i32 * TICK_ARRAY_SIZE;

    if ticks_per_array == 0 {
        return Err("Invalid tick spacing (zero)".into());
    }

    let current_start = get_tick_array_start_index(pool.tick_current, tick_spacing);
    let mut result: Vec<Pubkey> = Vec::with_capacity(NUM_REMAINING_TICK_ARRAYS);

    // Walk the bitmap in the swap direction, starting from the current tick array.
    // Include the current tick array index in the search (it might be initialized).
    let step = if is_buy {
        // Buy = a_to_b = price decreases = tick decreases = walk lower
        -ticks_per_array
    } else {
        // Sell = b_to_a = price increases = tick increases = walk higher
        ticks_per_array
    };

    // Max search range: +-7680 with extension, +-512 without
    let max_index = if extension_data.is_some() { 7680 } else { 512 };

    let mut search_pos = current_start;

    while result.len() < NUM_REMAINING_TICK_ARRAYS {
        let tick_array_index = search_pos.div_euclid(ticks_per_array);
        if tick_array_index.abs() > max_index {
            break;
        }

        if is_tick_array_initialized(&pool.tick_array_bitmap, extension_data, tick_array_index) {
            result.push(pda_tick_array_address(pool_id, search_pos)?.0);
        }

        search_pos += step;
    }

    if result.is_empty() {
        return Err(format!(
            "No initialized tick arrays found in swap direction (tick={}, spacing={}, is_buy={}, has_extension={})",
            pool.tick_current, tick_spacing, is_buy, extension_data.is_some()
        )
        .into());
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_tick_array_start_index() {
        // tick_spacing = 1, ticks_per_array = 60
        assert_eq!(get_tick_array_start_index(0, 1), 0);
        assert_eq!(get_tick_array_start_index(59, 1), 0);
        assert_eq!(get_tick_array_start_index(60, 1), 60);
        assert_eq!(get_tick_array_start_index(-1, 1), -60);
        assert_eq!(get_tick_array_start_index(-60, 1), -60);
        assert_eq!(get_tick_array_start_index(-61, 1), -120);

        // tick_spacing = 10, ticks_per_array = 600
        assert_eq!(get_tick_array_start_index(0, 10), 0);
        assert_eq!(get_tick_array_start_index(599, 10), 0);
        assert_eq!(get_tick_array_start_index(600, 10), 600);
        assert_eq!(get_tick_array_start_index(-1, 10), -600);
    }

    #[test]
    fn test_is_tick_array_initialized() {
        let mut bitmap = [0u64; 16];

        // Set bit for tick array index 0 (position 512 in bitmap)
        // word = 512/64 = 8, bit = 512%64 = 0
        bitmap[8] = 1;

        assert!(is_tick_array_initialized_in_bitmap(&bitmap, 0));
        assert!(!is_tick_array_initialized_in_bitmap(&bitmap, 1));

        // Set bit for tick array index 1 (position 513)
        // word = 513/64 = 8, bit = 513%64 = 1
        bitmap[8] |= 1 << 1;
        assert!(is_tick_array_initialized_in_bitmap(&bitmap, 1));

        // Set bit for tick array index -1 (position 511)
        // word = 511/64 = 7, bit = 511%64 = 63
        bitmap[7] |= 1u64 << 63;
        assert!(is_tick_array_initialized_in_bitmap(&bitmap, -1));
    }

    fn make_test_pool(tick_current: i32, tick_spacing: u16, bitmap: [u64; 16]) -> RaydiumCLMMPool {
        RaydiumCLMMPool {
            bump: [0],
            amm_config: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 6,
            tick_spacing,
            liquidity: 1000000,
            sqrt_price_x64: 1u128 << 64,
            tick_current,
            padding3: 0,
            padding4: 0,
            fee_growth_global_0_x64: 0,
            fee_growth_global_1_x64: 0,
            protocol_fees_token_0: 0,
            protocol_fees_token_1: 0,
            swap_in_amount_token_0: 0,
            swap_out_amount_token_1: 0,
            swap_in_amount_token_1: 0,
            swap_out_amount_token_0: 0,
            status: 0,
            padding: [0; 7],
            reward_infos: std::array::from_fn(|_| crate::RewardInfo {
                reward_state: 0,
                open_time: 0,
                end_time: 0,
                last_update_time: 0,
                emissions_per_second_x64: 0,
                reward_total_emissioned: 0,
                reward_claimed: 0,
                token_mint: Pubkey::new_unique(),
                token_vault: Pubkey::new_unique(),
                authority: Pubkey::new_unique(),
                reward_growth_global_x64: 0,
            }),
            tick_array_bitmap: bitmap,
            total_fees_token_0: 0,
            total_fees_claimed_token_0: 0,
            total_fees_token_1: 0,
            total_fees_claimed_token_1: 0,
            fund_fees_token_0: 0,
            fund_fees_token_1: 0,
            open_time: 0,
            recent_epoch: 0,
            padding1: [0; 24],
            padding2: [0; 32],
        }
    }

    #[test]
    fn test_compute_only_initialized_tick_arrays() {
        let pool_id = Pubkey::new_unique();
        let mut bitmap = [0u64; 16];
        // Initialize indices 0, 1, 2, -1, -2
        bitmap[8] = 0b111; // indices 0, 1, 2
        bitmap[7] |= 1u64 << 63; // index -1
        bitmap[7] |= 1u64 << 62; // index -2

        let pool = make_test_pool(0, 1, bitmap);

        // Buy direction (tick decreases): current (0) + -1 + -2
        let buy_accounts = compute_clmm_remaining_accounts(&pool, &pool_id, true, None).unwrap();
        assert_eq!(buy_accounts.len(), 3);

        // Sell direction (tick increases): current (0) + 1 + 2
        let sell_accounts = compute_clmm_remaining_accounts(&pool, &pool_id, false, None).unwrap();
        assert_eq!(sell_accounts.len(), 3);

        // First account should be the current tick array (same for both since index 0 is initialized)
        assert_eq!(buy_accounts[0], sell_accounts[0]);

        // Buy and sell should have different remaining accounts (different directions)
        assert_ne!(buy_accounts[1], sell_accounts[1]);
    }

    #[test]
    fn test_compute_skips_uninitialized_current_tick_array() {
        let pool_id = Pubkey::new_unique();
        let mut bitmap = [0u64; 16];
        // Current tick at index 5, but only indices 3 and 7 are initialized
        // index 3: position 515, word 8, bit 3
        bitmap[8] |= 1u64 << 3;
        // index 7: position 519, word 8, bit 7
        bitmap[8] |= 1u64 << 7;

        let pool = make_test_pool(300, 1, bitmap); // tick 300 -> index 5 (start=300)

        // Buy (going down): should find index 3, skip 4 and 5 (uninitialized)
        let buy_accounts = compute_clmm_remaining_accounts(&pool, &pool_id, true, None).unwrap();
        assert_eq!(buy_accounts.len(), 1); // Only index 3

        // Sell (going up): should find index 7, skip 5 and 6 (uninitialized)
        let sell_accounts = compute_clmm_remaining_accounts(&pool, &pool_id, false, None).unwrap();
        assert_eq!(sell_accounts.len(), 1); // Only index 7

        // They should be different PDAs
        assert_ne!(buy_accounts[0], sell_accounts[0]);
    }

    #[test]
    fn test_compute_no_initialized_returns_error() {
        let pool_id = Pubkey::new_unique();
        let bitmap = [0u64; 16]; // No initialized tick arrays
        let pool = make_test_pool(0, 1, bitmap);

        let result = compute_clmm_remaining_accounts(&pool, &pool_id, true, None);
        assert!(result.is_err());
    }
}
