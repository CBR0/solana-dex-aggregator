//! Offline DLMM swap quoting with full bin traversal.
//!
//! Ported from Meteora's official `dlmm-sdk` (`commons/src/quote.rs` and its
//! math/extension modules, MIT). Walks bins exactly like the on-chain program:
//! per-bin fills at the bin's fixed Q64.64 price, advancing through bin arrays
//! (located via the pool's internal liquidity bitmap) until the input is
//! exhausted. Replaces the old single-bin approximation, which ignored bin
//! traversal entirely and overquoted any swap larger than the active bin.
//!
//! Deviations from the SDK (deliberate, all quote-conservative):
//! - No `Clock`: `update_references` (time-decay of the volatility reference)
//!   is skipped, so the variable fee can only be over-, never under-estimated.
//!   The per-bin `update_volatility_accumulator` (pure account state) IS run.
//! - No Token-2022 transfer-fee adjustment (mint accounts not available here).
//! - No bitmap-extension: liquidity beyond ±512 bin arrays is unreachable.
//! - A bin array missing from the store ends traversal with the output
//!   accumulated so far (a lower bound) instead of erroring.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use crate::{MeteoraDLMMPool, METEORA_DYNAMIC_LMM};

// ---------------------------------------------------------------------------
// Constants (dlmm-sdk commons/src/constants.rs)
// ---------------------------------------------------------------------------

pub const BASIS_POINT_MAX: u128 = 10_000;
pub const MAX_BIN_PER_ARRAY: i32 = 70;
pub const MIN_BIN_ID: i32 = -443_636;
pub const MAX_BIN_ID: i32 = 443_636;
pub const MAX_FEE_RATE: u128 = 100_000_000; // 10%
pub const FEE_PRECISION: u128 = 1_000_000_000;
pub const BIN_ARRAY_BITMAP_SIZE: i32 = 512;
pub const SCALE_OFFSET: u8 = 64;
pub const ONE: u128 = 1u128 << SCALE_OFFSET;
const MAX_EXPONENTIAL: u32 = 0x80000;

// ---------------------------------------------------------------------------
// On-chain bin structs (layout per dlmm IDL; Bin = 144 bytes)
// ---------------------------------------------------------------------------

#[derive(BorshDeserialize, Debug, Clone, Copy)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    pub price: u128,
    pub liquidity_supply: u128,
    pub fulfilled_order_amount_x: u64,
    pub fulfilled_order_amount_y: u64,
    pub limit_order_fee_ask_side: u64,
    pub limit_order_fee_bid_side: u64,
    pub fee_amount_x_per_token_stored: u128,
    pub fee_amount_y_per_token_stored: u128,
    pub open_order_amount: u64,
    pub total_processing_order_amount: u64,
    pub processed_order_remaining_amount: u64,
    pub order_age: u32,
    pub limit_order_ask_side: u8,
    pub padding: [u8; 3],
}

#[derive(BorshDeserialize, Debug, Clone)]
pub struct BinArray {
    pub index: i64,
    pub version: u8,
    pub padding: [u8; 7],
    pub lb_pair: Pubkey,
    pub bins: [Bin; 70],
}

impl BinArray {
    /// Deserialize from raw account bytes (skips the 8-byte discriminator).
    pub fn from_account_bytes(data: &[u8]) -> Option<BinArray> {
        if data.len() < 8 {
            return None;
        }
        BinArray::try_from_slice(&data[8..]).ok()
    }

    pub fn bin_id_to_bin_array_index(bin_id: i32) -> i32 {
        let idx = bin_id.div_euclid(MAX_BIN_PER_ARRAY);
        idx
    }

    pub fn lower_upper_bin_id(index: i32) -> (i32, i32) {
        let lower = index * MAX_BIN_PER_ARRAY;
        (lower, lower + MAX_BIN_PER_ARRAY - 1)
    }

    pub fn is_bin_id_within_range(&self, bin_id: i32) -> bool {
        let (lower, upper) = Self::lower_upper_bin_id(self.index as i32);
        bin_id >= lower && bin_id <= upper
    }

    pub fn get_bin(&self, bin_id: i32) -> Option<&Bin> {
        if !self.is_bin_id_within_range(bin_id) {
            return None;
        }
        let (lower, _) = Self::lower_upper_bin_id(self.index as i32);
        self.bins.get((bin_id - lower) as usize)
    }
}

/// PDA of the bin array at `index` for `lb_pair`.
pub fn derive_bin_array_pda(lb_pair: &Pubkey, index: i32) -> Pubkey {
    let program_id = Pubkey::from_str_const(METEORA_DYNAMIC_LMM);
    let index_i64 = index as i64;
    Pubkey::find_program_address(
        &[b"bin_array", lb_pair.as_ref(), &index_i64.to_le_bytes()],
        &program_id,
    )
    .0
}

// ---------------------------------------------------------------------------
// Fixed-point math (dlmm-sdk commons/src/math)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Rounding {
    Up,
    Down,
}

/// (x * y) >> 64 where y fits in u64 — exact via 64-bit limb split, no u256.
/// x = xh·2^64 + xl  ⇒  (x·y) >> 64 = xh·y + (xl·y >> 64), remainder in low bits.
fn mul_shr64_u64(x: u128, y: u64, rounding: Rounding) -> Option<u64> {
    let xh = x >> 64;
    let xl = x & (u128::MAX >> 64);
    let y = y as u128;

    let high = xh.checked_mul(y)?; // ≤ 2^128, whole part
    let low = xl.checked_mul(y)?; // ≤ 2^128
    let result = high.checked_add(low >> 64)?;
    let rem = low & (u128::MAX >> 64);

    let result = if rounding == Rounding::Up && rem > 0 {
        result.checked_add(1)?
    } else {
        result
    };
    u64::try_from(result).ok()
}

/// (x << 64) / y where x fits in u64 — exact in u128.
fn shl64_div_u64(x: u64, y: u128, rounding: Rounding) -> Option<u64> {
    if y == 0 {
        return None;
    }
    let shifted = (x as u128) << 64;
    let q = shifted / y;
    let rem = shifted % y;
    let q = if rounding == Rounding::Up && rem > 0 {
        q.checked_add(1)?
    } else {
        q
    };
    u64::try_from(q).ok()
}

/// Binary-exponentiation pow for Q64.64 (dlmm-sdk u64x64_math.rs, verbatim
/// logic with the 19-step unrolled loop collapsed into a real loop).
pub fn pow(base: u128, exp: i32) -> Option<u128> {
    let mut invert = exp.is_negative();
    if exp == 0 {
        return Some(ONE);
    }
    let exp: u32 = exp.unsigned_abs();
    if exp >= MAX_EXPONENTIAL {
        return None;
    }

    let mut squared_base = base;
    let mut result = ONE;

    if squared_base >= result {
        squared_base = u128::MAX.checked_div(squared_base)?;
        invert = !invert;
    }

    let mut bit = 0x1u32;
    while bit < MAX_EXPONENTIAL {
        if exp & bit > 0 {
            result = result.checked_mul(squared_base)? >> SCALE_OFFSET;
        }
        bit <<= 1;
        if bit < MAX_EXPONENTIAL {
            squared_base = squared_base.checked_mul(squared_base)? >> SCALE_OFFSET;
        }
    }

    if result == 0 {
        return None;
    }
    if invert {
        result = u128::MAX.checked_div(result)?;
    }
    Some(result)
}

/// Bin price in Q64.64: (1 + bin_step/10000)^active_id.
pub fn get_price_from_id(active_id: i32, bin_step: u16) -> Option<u128> {
    let bps = (u128::from(bin_step) << SCALE_OFFSET) / BASIS_POINT_MAX;
    pow(ONE.checked_add(bps)?, active_id)
}

/// Amount out for `amount_in` at `price` (Q64.64).
/// swap_for_y (X→Y): out = price·in >> 64. Y→X: out = (in << 64)/price.
fn get_amount_out(amount_in: u64, price: u128, swap_for_y: bool, rounding: Rounding) -> Option<u64> {
    if swap_for_y {
        mul_shr64_u64(price, amount_in, rounding)
    } else {
        shl64_div_u64(amount_in, price, rounding)
    }
}

/// Amount in required for `amount_out` at `price` (inverse of get_amount_out).
fn get_amount_in(amount_out: u64, price: u128, swap_for_y: bool, rounding: Rounding) -> Option<u64> {
    if swap_for_y {
        shl64_div_u64(amount_out, price, rounding)
    } else {
        mul_shr64_u64(price, amount_out, rounding)
    }
}

// ---------------------------------------------------------------------------
// 1024-bit internal liquidity bitmap ([u64;16] limbs, little-endian limb order)
// ---------------------------------------------------------------------------

/// Next bin-array index with liquidity per the pool's internal bitmap.
/// Returns (index, has_liquidity); has_liquidity=false means "out of the
/// internal bitmap range in that direction" (extension territory — v1 stops).
/// Mirrors LbPairExtension::next_bin_array_index_with_liquidity_internal.
fn next_bin_array_index_with_liquidity(
    bitmap: &[u64; 16],
    swap_for_y: bool,
    start_index: i32,
) -> (i32, bool) {
    let offset = (start_index + BIN_ARRAY_BITMAP_SIZE) as u32; // 0..1023

    if swap_for_y {
        // Searching downward: shift left by (1023 - offset), count leading zeros.
        let shift = 1023 - offset;
        let shifted = bitmap_shl(bitmap, shift);
        match bitmap_leading_zeros(&shifted) {
            Some(lz) => (start_index - lz as i32, true),
            None => (-BIN_ARRAY_BITMAP_SIZE - 1, false),
        }
    } else {
        // Searching upward: shift right by offset, count trailing zeros.
        let shifted = bitmap_shr(bitmap, offset);
        match bitmap_trailing_zeros(&shifted) {
            Some(tz) => (start_index + tz as i32, true),
            None => (BIN_ARRAY_BITMAP_SIZE, false),
        }
    }
}

fn bitmap_shl(bitmap: &[u64; 16], shift: u32) -> [u64; 16] {
    let mut out = [0u64; 16];
    let limb_shift = (shift / 64) as usize;
    let bit_shift = shift % 64;
    for i in (limb_shift..16).rev() {
        let src = i - limb_shift;
        out[i] = bitmap[src] << bit_shift;
        if bit_shift > 0 && src > 0 {
            out[i] |= bitmap[src - 1] >> (64 - bit_shift);
        }
    }
    out
}

fn bitmap_shr(bitmap: &[u64; 16], shift: u32) -> [u64; 16] {
    let mut out = [0u64; 16];
    let limb_shift = (shift / 64) as usize;
    let bit_shift = shift % 64;
    for i in 0..(16 - limb_shift) {
        let src = i + limb_shift;
        out[i] = bitmap[src] >> bit_shift;
        if bit_shift > 0 && src + 1 < 16 {
            out[i] |= bitmap[src + 1] << (64 - bit_shift);
        }
    }
    out
}

fn bitmap_leading_zeros(bitmap: &[u64; 16]) -> Option<u32> {
    for (i, limb) in bitmap.iter().enumerate().rev() {
        if *limb != 0 {
            return Some((15 - i as u32) * 64 + limb.leading_zeros());
        }
    }
    None
}

fn bitmap_trailing_zeros(bitmap: &[u64; 16]) -> Option<u32> {
    for (i, limb) in bitmap.iter().enumerate() {
        if *limb != 0 {
            return Some(i as u32 * 64 + limb.trailing_zeros());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Fee math (dlmm-sdk LbPairExtension)
// ---------------------------------------------------------------------------

struct FeeState {
    total_fee_rate: u128, // out of FEE_PRECISION
    protocol_share: u16,
}

fn base_fee(pool: &MeteoraDLMMPool) -> Option<u128> {
    u128::from(pool.parameters.base_factor)
        .checked_mul(pool.bin_step.into())?
        .checked_mul(10)?
        .checked_mul(10u128.checked_pow(pool.parameters.base_fee_power_factor.into())?)
        .into()
}

fn variable_fee(pool: &MeteoraDLMMPool, volatility_accumulator: u32) -> Option<u128> {
    if pool.parameters.variable_fee_control == 0 {
        return Some(0);
    }
    let square_vfa_bin = u128::from(volatility_accumulator)
        .checked_mul(pool.bin_step.into())?
        .checked_pow(2)?;
    let v_fee = u128::from(pool.parameters.variable_fee_control).checked_mul(square_vfa_bin)?;
    // Scale down + ceil (SDK constant).
    Some((v_fee.checked_add(99_999_999_999)?) / 100_000_000_000)
}

fn total_fee_rate(pool: &MeteoraDLMMPool, volatility_accumulator: u32) -> Option<u128> {
    let total = base_fee(pool)?.checked_add(variable_fee(pool, volatility_accumulator)?)?;
    Some(total.min(MAX_FEE_RATE))
}

/// Fee to ADD on top of `amount` (net→gross). Ceil division.
fn compute_fee(fee: &FeeState, amount: u64) -> Option<u64> {
    let denominator = FEE_PRECISION.checked_sub(fee.total_fee_rate)?;
    let f = u128::from(amount)
        .checked_mul(fee.total_fee_rate)?
        .checked_add(denominator)?
        .checked_sub(1)?
        / denominator;
    u64::try_from(f).ok()
}

/// Fee INCLUDED in `amount_with_fees` (gross→fee part). Ceil division.
fn compute_fee_from_amount(fee: &FeeState, amount_with_fees: u64) -> Option<u64> {
    let f = u128::from(amount_with_fees)
        .checked_mul(fee.total_fee_rate)?
        .checked_add(FEE_PRECISION - 1)?
        / FEE_PRECISION;
    u64::try_from(f).ok()
}

// ---------------------------------------------------------------------------
// Volatility accumulator (no-clock variant: update_references skipped)
// ---------------------------------------------------------------------------

fn volatility_accumulator_for_bin(pool: &MeteoraDLMMPool, active_id: i32) -> u32 {
    let delta_id = i64::from(pool.v_parameters.index_reference)
        .saturating_sub(active_id.into())
        .unsigned_abs();
    let acc = u64::from(pool.v_parameters.volatility_reference)
        .saturating_add(delta_id.saturating_mul(BASIS_POINT_MAX as u64));
    acc.min(pool.parameters.max_volatility_accumulator.into()) as u32
}

// ---------------------------------------------------------------------------
// Limit orders
// ---------------------------------------------------------------------------

/// FunctionType: 0 Undetermined, 1 LiquidityMining, 2 LimitOrder.
fn is_support_limit_order(pool: &MeteoraDLMMPool) -> bool {
    match pool.parameters.function_type {
        2 => true,
        1 => false,
        0 => pool.reward_infos.iter().all(|r| r.mint == Pubkey::default()),
        _ => false,
    }
}

/// CollectFeeMode: 0 InputOnly, 1 OnlyY.
fn fee_on_input(pool: &MeteoraDLMMPool, swap_for_y: bool) -> bool {
    match pool.parameters.collect_fee_mode {
        1 => !swap_for_y,
        _ => true,
    }
}

/// (open_order_amount, processed_order_remaining) fillable by this direction.
fn limit_order_amounts(bin: &Bin, swap_for_y: bool) -> (u64, u64) {
    let is_ask_side = bin.limit_order_ask_side != 0;
    if (swap_for_y && !is_ask_side) || (!swap_for_y && is_ask_side) {
        (bin.open_order_amount, bin.processed_order_remaining_amount)
    } else {
        (0, 0)
    }
}

fn max_amount_out_with_limit_orders(bin: &Bin, swap_for_y: bool, support_lo: bool) -> u64 {
    let mm = if swap_for_y { bin.amount_y } else { bin.amount_x };
    if !support_lo {
        return mm;
    }
    let (open, processed) = limit_order_amounts(bin, swap_for_y);
    mm.saturating_add(open).saturating_add(processed)
}

// ---------------------------------------------------------------------------
// Per-bin fill (dlmm-sdk quote.rs)
// ---------------------------------------------------------------------------

struct FillResult {
    amount_in: u64,
    amount_left: u64,
    out_amount: u64,
}

fn fill_at_price(bin: &Bin, amount: u64, max_amount_out: u64, swap_for_y: bool) -> Option<FillResult> {
    if max_amount_out == 0 {
        return Some(FillResult { amount_in: 0, amount_left: amount, out_amount: 0 });
    }
    let max_amount_in = get_amount_in(max_amount_out, bin.price, swap_for_y, Rounding::Up)?;
    if amount >= max_amount_in {
        Some(FillResult {
            amount_in: max_amount_in,
            amount_left: amount.checked_sub(max_amount_in)?,
            out_amount: max_amount_out,
        })
    } else {
        let out_amount = get_amount_out(amount, bin.price, swap_for_y, Rounding::Down)?;
        Some(FillResult { amount_in: amount, amount_left: 0, out_amount })
    }
}

struct BinFill {
    amount_in_consumed: u64, // gross input consumed (incl. fee when fee_on_input)
    amount_out: u64,         // net output (excl. fee when fee on output)
}

/// Exact-in quote against a single bin: MM liquidity, then limit orders.
fn quote_at_bin(
    bin: &Bin,
    pool: &MeteoraDLMMPool,
    fee: &FeeState,
    in_amount: u64,
    swap_for_y: bool,
    support_lo: bool,
    fee_input: bool,
) -> Option<BinFill> {
    let mut excluded_fee_amount_in = in_amount;
    if fee_input {
        let f = compute_fee_from_amount(fee, in_amount)?;
        excluded_fee_amount_in = in_amount.checked_sub(f)?;
    }

    // MM layer
    let mm_amount = if swap_for_y { bin.amount_y } else { bin.amount_x };
    let mm_fill = fill_at_price(bin, excluded_fee_amount_in, mm_amount, swap_for_y)?;

    let mut total_in = mm_fill.amount_in;
    let mut total_out = mm_fill.out_amount;

    // Limit-order layers (processed first, then open)
    if support_lo && mm_fill.amount_left > 0 {
        let (open, processed) = limit_order_amounts(bin, swap_for_y);
        let p_fill = fill_at_price(bin, mm_fill.amount_left, processed, swap_for_y)?;
        total_in = total_in.checked_add(p_fill.amount_in)?;
        total_out = total_out.checked_add(p_fill.out_amount)?;
        if p_fill.amount_left > 0 {
            let o_fill = fill_at_price(bin, p_fill.amount_left, open, swap_for_y)?;
            total_in = total_in.checked_add(o_fill.amount_in)?;
            total_out = total_out.checked_add(o_fill.out_amount)?;
        }
    }

    let amount_left = excluded_fee_amount_in.checked_sub(total_in)?;

    // Gross input actually consumed
    let amount_in_consumed = if amount_left > 0 {
        if fee_input {
            let f = compute_fee(fee, total_in)?;
            total_in.checked_add(f)?
        } else {
            total_in
        }
    } else {
        in_amount
    };

    // Fee on output side
    let amount_out = if !fee_input {
        let f = compute_fee_from_amount(fee, total_out)?;
        total_out.checked_sub(f)?
    } else {
        total_out
    };

    let _ = pool;
    Some(BinFill { amount_in_consumed, amount_out })
}

// ---------------------------------------------------------------------------
// Full traversal quote
// ---------------------------------------------------------------------------

/// Exact-in DLMM quote with real bin traversal.
///
/// `get_bin_array(index)` supplies bin arrays on demand (from an account
/// store, RPC batch, or test fixture). Traversal follows the pool's internal
/// liquidity bitmap; a missing array ends traversal with the output
/// accumulated so far (conservative lower bound).
pub fn quote_exact_in(
    pool: &MeteoraDLMMPool,
    amount_in: u64,
    swap_for_y: bool,
    get_bin_array: &mut dyn FnMut(i32) -> Option<BinArray>,
) -> Option<u64> {
    if pool.status != 0 {
        return None;
    }

    let support_lo = is_support_limit_order(pool);
    let fee_input = fee_on_input(pool, swap_for_y);

    let mut active_id = pool.active_id;
    let mut amount_left = amount_in;
    let mut total_out: u64 = 0;

    // Hard bound: a swap can cross at most a handful of arrays before the
    // 1.4M CU budget would kill it on-chain anyway. Also guards against
    // any bitmap-walk edge case looping forever.
    const MAX_ARRAYS: usize = 8;
    let mut arrays_walked = 0usize;

    while amount_left > 0 && arrays_walked < MAX_ARRAYS {
        // Locate the next bin array with liquidity via the internal bitmap.
        let start_idx = BinArray::bin_id_to_bin_array_index(active_id);
        if start_idx > BIN_ARRAY_BITMAP_SIZE - 1 || start_idx < -BIN_ARRAY_BITMAP_SIZE {
            break; // extension territory — v1 stops
        }
        let (next_idx, has_liquidity) =
            next_bin_array_index_with_liquidity(&pool.bin_array_bitmap, swap_for_y, start_idx);
        if !has_liquidity {
            break;
        }

        let Some(bin_array) = get_bin_array(next_idx) else {
            break; // array not in store — return what we have
        };
        arrays_walked += 1;

        // If the bitmap jumped past empty arrays, snap active_id to the
        // near edge of the found array (shift_active_bin_if_empty_gap).
        if next_idx != BinArray::bin_id_to_bin_array_index(active_id) {
            let (lower, upper) = BinArray::lower_upper_bin_id(next_idx);
            active_id = if swap_for_y { upper } else { lower };
        }

        // Walk bins inside this array.
        while amount_left > 0 && bin_array.is_bin_id_within_range(active_id) {
            let Some(bin) = bin_array.get_bin(active_id) else { break };

            let mut bin = *bin;
            if bin.price == 0 {
                bin.price = get_price_from_id(active_id, pool.bin_step)?;
            }

            if max_amount_out_with_limit_orders(&bin, swap_for_y, support_lo) > 0 {
                let vol_acc = volatility_accumulator_for_bin(pool, active_id);
                let fee = FeeState {
                    total_fee_rate: total_fee_rate(pool, vol_acc)?,
                    protocol_share: pool.parameters.protocol_share,
                };
                let _ = fee.protocol_share;

                let fill = quote_at_bin(
                    &bin, pool, &fee, amount_left, swap_for_y, support_lo, fee_input,
                )?;
                if fill.amount_in_consumed > 0 {
                    amount_left = amount_left.checked_sub(fill.amount_in_consumed)?;
                    total_out = total_out.checked_add(fill.amount_out)?;
                }
            }

            if amount_left > 0 {
                let next = if swap_for_y { active_id - 1 } else { active_id + 1 };
                if !(MIN_BIN_ID..=MAX_BIN_ID).contains(&next) {
                    return Some(total_out);
                }
                active_id = next;
            }
        }
    }

    Some(total_out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pow_identity_and_known_values() {
        // base^0 = 1.0
        assert_eq!(pow(ONE + 123456, 0), Some(ONE));
        // (1 + 10/10000)^1 in Q64.64 ≈ 1.001
        let p = get_price_from_id(1, 10).unwrap();
        let as_f = p as f64 / ONE as f64;
        assert!((as_f - 1.001).abs() < 1e-9, "{as_f}");
        // Negative exponent inverts: p(-1) ≈ 1/1.001
        let p_neg = get_price_from_id(-1, 10).unwrap();
        let as_f_neg = p_neg as f64 / ONE as f64;
        assert!((as_f_neg - 1.0 / 1.001).abs() < 1e-9, "{as_f_neg}");
    }

    #[test]
    fn amount_out_round_trip() {
        // price = 2.0 in Q64.64: X→Y doubles, Y→X halves.
        let price = ONE * 2;
        assert_eq!(get_amount_out(1000, price, true, Rounding::Down), Some(2000));
        assert_eq!(get_amount_out(1000, price, false, Rounding::Down), Some(500));
        assert_eq!(get_amount_in(2000, price, true, Rounding::Up), Some(1000));
        assert_eq!(get_amount_in(500, price, false, Rounding::Up), Some(1000));
    }

    #[test]
    fn bitmap_next_index() {
        let mut bitmap = [0u64; 16];
        // Set bit for index 0 (offset 512) and index -3 (offset 509).
        let set = |bm: &mut [u64; 16], idx: i32| {
            let off = (idx + BIN_ARRAY_BITMAP_SIZE) as usize;
            bm[off / 64] |= 1u64 << (off % 64);
        };
        set(&mut bitmap, 0);
        set(&mut bitmap, -3);

        // Downward search from 0 finds 0 itself.
        assert_eq!(next_bin_array_index_with_liquidity(&bitmap, true, 0), (0, true));
        // Downward search from -1 skips to -3.
        assert_eq!(next_bin_array_index_with_liquidity(&bitmap, true, -1), (-3, true));
        // Upward search from -2 finds 0.
        assert_eq!(next_bin_array_index_with_liquidity(&bitmap, false, -2), (0, true));
        // Upward search from 1 finds nothing.
        assert_eq!(next_bin_array_index_with_liquidity(&bitmap, false, 1).1, false);
    }

    #[test]
    fn bin_array_index_math() {
        assert_eq!(BinArray::bin_id_to_bin_array_index(0), 0);
        assert_eq!(BinArray::bin_id_to_bin_array_index(69), 0);
        assert_eq!(BinArray::bin_id_to_bin_array_index(70), 1);
        assert_eq!(BinArray::bin_id_to_bin_array_index(-1), -1);
        assert_eq!(BinArray::bin_id_to_bin_array_index(-70), -1);
        assert_eq!(BinArray::bin_id_to_bin_array_index(-71), -2);
        assert_eq!(BinArray::lower_upper_bin_id(-1), (-70, -1));
    }
}
