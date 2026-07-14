//! Offline Raydium CLMM swap quoting with full tick traversal.
//!
//! Ported from Raydium's on-chain program (`raydium-io/raydium-clmm`,
//! programs/amm/src/libraries + the swap loop in instructions/swap.rs).
//! Classic path only: static `trade_fee_rate` from the pool's AmmConfig,
//! fee charged on input, no limit orders, no adaptive fee — the semantics
//! every classic (pre-adaptive) pool executes, which is where virtually all
//! CLMM volume lives.
//!
//! Deviations (deliberate, quote-conservative):
//! - Internal 1024-bit tick-array bitmap only; liquidity beyond ±512 arrays
//!   (bitmap extension account) is unreachable — traversal stops.
//! - A tick array missing from the store ends the swap with the output
//!   accumulated so far (lower bound), not an error.

use crate::RaydiumCLMMPool;
use solana_pubkey::Pubkey;

uint::construct_uint! {
    pub struct U256(4);
}
uint::construct_uint! {
    pub struct U512(8);
}

pub const MIN_TICK: i32 = -443636;
pub const MAX_TICK: i32 = 443636;
pub const MIN_SQRT_PRICE_X64: u128 = 4295048016;
pub const MAX_SQRT_PRICE_X64: u128 = 79226673521066979257578248091;
pub const FEE_RATE_DENOMINATOR: u32 = 1_000_000;
pub const TICK_ARRAY_SIZE: i32 = 60;
const Q64: u128 = 1u128 << 64;
const RESOLUTION: u8 = 64;

// ---------------------------------------------------------------------------
// Fixed-point helpers (full_math.rs / unsafe_math.rs)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Rounding {
    Up,
    Down,
}

fn widen(a: U256) -> U512 {
    let mut limbs = [0u64; 8];
    limbs[..4].copy_from_slice(&a.0);
    U512(limbs)
}

fn narrow(q: U512) -> Option<U256> {
    if q.0[4..].iter().any(|&x| x != 0) {
        return None;
    }
    let mut limbs = [0u64; 4];
    limbs.copy_from_slice(&q.0[..4]);
    Some(U256(limbs))
}

/// (a * b) / c on U256 via U512 intermediate.
fn mul_div_u256(a: U256, b: U256, c: U256, rounding: Rounding) -> Option<U256> {
    if c.is_zero() {
        return None;
    }
    let (a5, b5, c5) = (widen(a), widen(b), widen(c));
    let prod = a5.checked_mul(b5)?;
    let mut q = prod / c5;
    if rounding == Rounding::Up && !(prod % c5).is_zero() {
        q = q.checked_add(U512::one())?;
    }
    narrow(q)
}

fn div_rounding_up_u256(x: U256, y: U256) -> U256 {
    let q = x / y;
    if (x % y).is_zero() { q } else { q + U256::one() }
}

/// (a * b) >> 64 on u128 pair via U256 (used by tick math).
fn mul_shr64_u128(a: u128, b: u128) -> u128 {
    let prod = U256::from(a) * U256::from(b);
    ((prod >> 64).low_u128()) as u128
}

// ---------------------------------------------------------------------------
// Tick math (tick_math.rs, magic constants verbatim)
// ---------------------------------------------------------------------------

/// sqrt(1.0001^tick) in Q64.64.
pub fn get_sqrt_price_at_tick(tick: i32) -> Option<u128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let abs_tick = tick.unsigned_abs();

    const FACTORS: [(u32, u128); 19] = [
        (0x1, 0xfffcb933bd6fb800),
        (0x2, 0xfff97272373d4000),
        (0x4, 0xfff2e50f5f657000),
        (0x8, 0xffe5caca7e10f000),
        (0x10, 0xffcb9843d60f7000),
        (0x20, 0xff973b41fa98e800),
        (0x40, 0xff2ea16466c9b000),
        (0x80, 0xfe5dee046a9a3800),
        (0x100, 0xfcbe86c7900bb000),
        (0x200, 0xf987a7253ac65800),
        (0x400, 0xf3392b0822bb6000),
        (0x800, 0xe7159475a2caf000),
        (0x1000, 0xd097f3bdfd2f2000),
        (0x2000, 0xa9f746462d9f8000),
        (0x4000, 0x70d869a156f31c00),
        (0x8000, 0x31be135f97ed3200),
        (0x10000, 0x9aa508b5b85a500),
        (0x20000, 0x5d6af8dedc582c),
        (0x40000, 0x2216e584f5fa),
    ];

    let mut ratio: u128 = if abs_tick & 0x1 != 0 { FACTORS[0].1 } else { Q64 };
    for &(bit, factor) in FACTORS.iter().skip(1) {
        if abs_tick & bit != 0 {
            ratio = mul_shr64_u128(ratio, factor);
        }
    }

    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Some(ratio)
}

/// Greatest tick such that get_sqrt_price_at_tick(tick) <= sqrt_price.
pub fn get_tick_at_sqrt_price(sqrt_price_x64: u128) -> Option<i32> {
    if !(MIN_SQRT_PRICE_X64..MAX_SQRT_PRICE_X64).contains(&sqrt_price_x64) {
        return None;
    }
    const BIT_PRECISION: u32 = 16;

    let msb: u32 = 128 - sqrt_price_x64.leading_zeros() - 1;
    let log2p_integer_x32 = (msb as i128 - 64) << 32;

    let mut bit: i128 = 0x8000_0000_0000_0000i128;
    let mut precision = 0;
    let mut log2p_fraction_x64: i128 = 0;

    let mut r = if msb >= 64 {
        sqrt_price_x64 >> (msb - 63)
    } else {
        sqrt_price_x64 << (63 - msb)
    };

    while bit > 0 && precision < BIT_PRECISION {
        r *= r;
        let is_r_more_than_two = r >> 127;
        r >>= 63 + is_r_more_than_two;
        log2p_fraction_x64 += bit * is_r_more_than_two as i128;
        bit >>= 1;
        precision += 1;
    }
    let log2p_fraction_x32 = log2p_fraction_x64 >> 32;
    let log2p_x32 = log2p_integer_x32 + log2p_fraction_x32;

    let log_sqrt_10001_x64 = log2p_x32 * 59543866431248i128;
    let tick_low = ((log_sqrt_10001_x64 - 184467440737095516i128) >> 64) as i32;
    let tick_high = ((log_sqrt_10001_x64 + 15793534762490258745i128) >> 64) as i32;

    if tick_low == tick_high {
        Some(tick_low)
    } else if get_sqrt_price_at_tick(tick_high)? <= sqrt_price_x64 {
        Some(tick_high)
    } else {
        Some(tick_low)
    }
}

// ---------------------------------------------------------------------------
// Liquidity deltas (liquidity_math.rs)
// ---------------------------------------------------------------------------

/// Δx = L/√P_lower − L/√P_upper (token 0).
fn get_delta_amount_0(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Option<u64> {
    if sqrt_a > sqrt_b {
        std::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_a == 0 {
        return None;
    }
    let numerator_1 = U256::from(liquidity) << RESOLUTION;
    let numerator_2 = U256::from(sqrt_b - sqrt_a);

    let result = if round_up {
        div_rounding_up_u256(
            mul_div_u256(numerator_1, numerator_2, U256::from(sqrt_b), Rounding::Up)?,
            U256::from(sqrt_a),
        )
    } else {
        mul_div_u256(numerator_1, numerator_2, U256::from(sqrt_b), Rounding::Down)?
            / U256::from(sqrt_a)
    };
    if result > U256::from(u64::MAX) {
        return None; // MaxTokenOverflow — caller treats as "beyond range"
    }
    Some(result.as_u64())
}

/// Δy = L × (√P_upper − √P_lower) (token 1).
fn get_delta_amount_1(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Option<u64> {
    if sqrt_a > sqrt_b {
        std::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    let rounding = if round_up { Rounding::Up } else { Rounding::Down };
    let result = mul_div_u256(
        U256::from(liquidity),
        U256::from(sqrt_b - sqrt_a),
        U256::from(Q64),
        rounding,
    )?;
    if result > U256::from(u64::MAX) {
        return None;
    }
    Some(result.as_u64())
}

// ---------------------------------------------------------------------------
// Next sqrt price (sqrt_price_math.rs)
// ---------------------------------------------------------------------------

fn next_sqrt_price_from_amount_0_up(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    if amount == 0 {
        return Some(sqrt_price);
    }
    let numerator_1 = U256::from(liquidity) << RESOLUTION;
    if let Some(product) = U256::from(amount).checked_mul(U256::from(sqrt_price)) {
        let denominator = numerator_1.checked_add(product)?;
        if denominator >= numerator_1 {
            let r = mul_div_u256(numerator_1, U256::from(sqrt_price), denominator, Rounding::Up)?;
            return Some(r.low_u128());
        }
    }
    let r = div_rounding_up_u256(
        numerator_1,
        (numerator_1 / U256::from(sqrt_price)).checked_add(U256::from(amount))?,
    );
    Some(r.low_u128())
}

fn next_sqrt_price_from_amount_1_down(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    if amount == 0 {
        return Some(sqrt_price);
    }
    let quotient = ((U256::from(amount) << RESOLUTION) / U256::from(liquidity)).low_u128();
    sqrt_price.checked_add(quotient)
}

fn next_sqrt_price_from_input(
    sqrt_price: u128,
    liquidity: u128,
    amount_in: u64,
    zero_for_one: bool,
) -> Option<u128> {
    if sqrt_price == 0 || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        next_sqrt_price_from_amount_0_up(sqrt_price, liquidity, amount_in)
    } else {
        next_sqrt_price_from_amount_1_down(sqrt_price, liquidity, amount_in)
    }
}

// ---------------------------------------------------------------------------
// Single swap step (swap_math.rs compute_swap, base-input + fee-on-input path)
// ---------------------------------------------------------------------------

struct StepResult {
    sqrt_price_next: u128,
    amount_in: u64,
    amount_out: u64,
    fee_amount: u64,
}

fn compute_swap_step(
    sqrt_price_current: u128,
    sqrt_price_target: u128,
    liquidity: u128,
    amount_remaining: u64,
    fee_rate: u32,
    zero_for_one: bool,
) -> Option<StepResult> {
    // Fee off the input first.
    let amount_for_price = (amount_remaining as u128)
        .checked_mul((FEE_RATE_DENOMINATOR - fee_rate) as u128)?
        / FEE_RATE_DENOMINATOR as u128;
    let amount_for_price = u64::try_from(amount_for_price).ok()?;

    // Input needed to reach the target price (None = overflows u64 = beyond reach).
    let amount_in_to_target = if zero_for_one {
        get_delta_amount_0(sqrt_price_target, sqrt_price_current, liquidity, true)
    } else {
        get_delta_amount_1(sqrt_price_current, sqrt_price_target, liquidity, true)
    };

    let mut amount_in = amount_in_to_target.unwrap_or(0);
    let sqrt_price_next = if amount_in_to_target.is_some() && amount_for_price >= amount_in {
        sqrt_price_target
    } else {
        next_sqrt_price_from_input(sqrt_price_current, liquidity, amount_for_price, zero_for_one)?
    };

    let max = sqrt_price_next == sqrt_price_target;
    let mut amount_out;
    if zero_for_one {
        if !max {
            amount_in = get_delta_amount_0(sqrt_price_next, sqrt_price_current, liquidity, true)?;
        }
        amount_out = get_delta_amount_1(sqrt_price_next, sqrt_price_current, liquidity, false)?;
    } else {
        if !max {
            amount_in = get_delta_amount_1(sqrt_price_current, sqrt_price_next, liquidity, true)?;
        }
        amount_out = get_delta_amount_0(sqrt_price_current, sqrt_price_next, liquidity, false)?;
    }
    let _ = &mut amount_out;

    let fee_amount = if sqrt_price_next != sqrt_price_target {
        // Didn't reach target: everything left over is fee.
        amount_remaining.checked_sub(amount_in)?
    } else {
        // fee = ceil(amount_in × rate / (1e6 − rate))
        let f = (amount_in as u128)
            .checked_mul(fee_rate as u128)?
            .checked_add((FEE_RATE_DENOMINATOR - fee_rate) as u128 - 1)?
            / (FEE_RATE_DENOMINATOR - fee_rate) as u128;
        u64::try_from(f).ok()?
    };

    Some(StepResult { sqrt_price_next, amount_in, amount_out, fee_amount })
}

// ---------------------------------------------------------------------------
// Tick array account parsing (zero-copy layout, offsets fixed on-chain)
// ---------------------------------------------------------------------------

/// Minimal view of one tick (only what quoting needs).
#[derive(Clone, Copy, Debug)]
pub struct TickInfo {
    pub tick: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}

/// Parsed fixed tick array (10240-byte account).
pub struct ClmmTickArray {
    pub start_tick_index: i32,
    pub ticks: Vec<TickInfo>, // always 60
}

const TICK_LEN: usize = 168;

impl ClmmTickArray {
    /// Parse from raw account bytes: 8 disc + pool_id 32 + start i32 + 60×168 ticks.
    pub fn from_account_bytes(data: &[u8]) -> Option<ClmmTickArray> {
        if data.len() < 8 + 32 + 4 + 60 * TICK_LEN {
            return None;
        }
        let start_tick_index = i32::from_le_bytes(data[40..44].try_into().ok()?);
        let mut ticks = Vec::with_capacity(60);
        for i in 0..60 {
            let off = 44 + i * TICK_LEN;
            ticks.push(TickInfo {
                tick: i32::from_le_bytes(data[off..off + 4].try_into().ok()?),
                liquidity_net: i128::from_le_bytes(data[off + 4..off + 20].try_into().ok()?),
                liquidity_gross: u128::from_le_bytes(data[off + 20..off + 36].try_into().ok()?),
            });
        }
        Some(ClmmTickArray { start_tick_index, ticks })
    }

    /// Start index of the array containing `tick_index`.
    pub fn array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
        let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
        tick_index.div_euclid(ticks_in_array) * ticks_in_array
    }

    /// Next initialized tick from `current_tick` inside this array
    /// (inclusive of current for zero_for_one, exclusive for one_for_zero —
    /// on-chain semantics).
    fn next_initialized_tick(
        &self,
        current_tick: i32,
        tick_spacing: u16,
        zero_for_one: bool,
    ) -> Option<TickInfo> {
        if Self::array_start_index(current_tick, tick_spacing) != self.start_tick_index {
            return None;
        }
        let offset = (current_tick - self.start_tick_index) / tick_spacing as i32;
        if zero_for_one {
            (0..=offset).rev().map(|i| self.ticks[i as usize]).find(|t| t.liquidity_gross != 0)
        } else {
            ((offset + 1)..TICK_ARRAY_SIZE)
                .map(|i| self.ticks[i as usize])
                .find(|t| t.liquidity_gross != 0)
        }
    }

    fn first_initialized_tick(&self, zero_for_one: bool) -> Option<TickInfo> {
        if zero_for_one {
            self.ticks.iter().rev().copied().find(|t| t.liquidity_gross != 0)
        } else {
            self.ticks.iter().copied().find(|t| t.liquidity_gross != 0)
        }
    }
}

// ---------------------------------------------------------------------------
// Internal tick-array bitmap ([u64;16] = 1024 arrays around index 0)
// ---------------------------------------------------------------------------

/// Next bitmap-set array start index at or beyond `start_index` in the swap
/// direction. Returns None when out of internal bitmap range or no bit set.
fn next_bitmap_array_start(
    bitmap: &[u64; 16],
    start_index: i32,
    ticks_in_array: i32,
    zero_for_one: bool,
) -> Option<i32> {
    let array_index = start_index.div_euclid(ticks_in_array);
    let offset = array_index + 512; // 0..1023
    if !(0..1024).contains(&offset) {
        return None;
    }
    if zero_for_one {
        // Search this offset and below.
        let mut off = offset;
        while off >= 0 {
            if bitmap[(off / 64) as usize] & (1u64 << (off % 64)) != 0 {
                return Some((off - 512) * ticks_in_array);
            }
            off -= 1;
        }
        None
    } else {
        let mut off = offset;
        while off < 1024 {
            if bitmap[(off / 64) as usize] & (1u64 << (off % 64)) != 0 {
                return Some((off - 512) * ticks_in_array);
            }
            off += 1;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Full traversal quote
// ---------------------------------------------------------------------------

/// Exact-in CLMM quote with real tick traversal.
///
/// `fee_rate` comes from the pool's AmmConfig (hundredths of a bp of 1e6).
/// `get_tick_array(start_index)` supplies arrays on demand; a missing array
/// ends traversal with the output accumulated so far.
pub fn quote_exact_in(
    pool: &RaydiumCLMMPool,
    fee_rate: u32,
    amount_in: u64,
    zero_for_one: bool,
    get_tick_array: &mut dyn FnMut(i32) -> Option<ClmmTickArray>,
) -> Option<u64> {
    if pool.liquidity == 0 && amount_in == 0 {
        return Some(0);
    }
    let ticks_in_array = TICK_ARRAY_SIZE * pool.tick_spacing as i32;
    let price_limit = if zero_for_one {
        MIN_SQRT_PRICE_X64 + 1
    } else {
        MAX_SQRT_PRICE_X64 - 1
    };

    let mut sqrt_price = pool.sqrt_price_x64;
    let mut tick = pool.tick_current;
    let mut liquidity = pool.liquidity;
    let mut remaining = amount_in;
    let mut total_out: u64 = 0;

    // First array with liquidity at/behind the current tick (bitmap-driven).
    let start = ClmmTickArray::array_start_index(tick, pool.tick_spacing);
    let Some(mut current_array_start) =
        next_bitmap_array_start(&pool.tick_array_bitmap, start, ticks_in_array, zero_for_one)
    else {
        return Some(0);
    };
    let mut first_array_contains_tick = current_array_start == start;
    let mut current_array = get_tick_array(current_array_start)?;

    const MAX_ARRAYS: usize = 10;
    let mut arrays_walked = 1usize;

    while remaining != 0 && sqrt_price != price_limit {
        // Locate the next initialized tick.
        let next_tick = if let Some(t) =
            current_array.next_initialized_tick(tick, pool.tick_spacing, zero_for_one)
        {
            t
        } else if !first_array_contains_tick {
            first_array_contains_tick = true;
            current_array.first_initialized_tick(zero_for_one)?
        } else {
            // Move to the next array with liquidity per the bitmap.
            let probe = if zero_for_one {
                current_array_start - ticks_in_array
            } else {
                current_array_start + ticks_in_array
            };
            let Some(next_start) = next_bitmap_array_start(
                &pool.tick_array_bitmap,
                probe,
                ticks_in_array,
                zero_for_one,
            ) else {
                break; // out of internal bitmap — conservative stop
            };
            if arrays_walked >= MAX_ARRAYS {
                break;
            }
            let Some(arr) = get_tick_array(next_start) else {
                break; // array not in store — return what we have
            };
            arrays_walked += 1;
            current_array_start = next_start;
            current_array = arr;
            match current_array.first_initialized_tick(zero_for_one) {
                Some(t) => t,
                None => break,
            }
        };

        // Clamp the target tick's price by the global limit.
        let tick_price = get_sqrt_price_at_tick(next_tick.tick)?;
        let target_price = if zero_for_one {
            tick_price.max(price_limit)
        } else {
            tick_price.min(price_limit)
        };

        if liquidity > 0 && sqrt_price != target_price {
            let step = compute_swap_step(
                sqrt_price,
                target_price,
                liquidity,
                remaining,
                fee_rate,
                zero_for_one,
            )?;
            remaining = remaining
                .checked_sub(step.amount_in.checked_add(step.fee_amount)?)?;
            total_out = total_out.checked_add(step.amount_out)?;
            sqrt_price = step.sqrt_price_next;
        } else {
            sqrt_price = target_price;
        }

        if sqrt_price == tick_price {
            // Crossed the tick: apply liquidity_net and step over it.
            let net = if zero_for_one {
                next_tick.liquidity_net.checked_neg()?
            } else {
                next_tick.liquidity_net
            };
            liquidity = if net < 0 {
                liquidity.checked_sub(net.unsigned_abs())?
            } else {
                liquidity.checked_add(net.unsigned_abs())?
            };
            tick = if zero_for_one { next_tick.tick - 1 } else { next_tick.tick };
        } else {
            tick = get_tick_at_sqrt_price(sqrt_price)?;
        }
    }

    Some(total_out)
}

// ---------------------------------------------------------------------------
// AmmConfig parsing
// ---------------------------------------------------------------------------

/// trade_fee_rate from a raw AmmConfig account (offset 47..51, verified
/// on-chain). Hundredths of a bp out of 1e6.
pub fn parse_amm_config_trade_fee_rate(data: &[u8]) -> Option<u32> {
    data.get(47..51)?.try_into().ok().map(u32::from_le_bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_math_bounds_and_roundtrip() {
        assert_eq!(get_sqrt_price_at_tick(MIN_TICK), Some(MIN_SQRT_PRICE_X64));
        assert_eq!(get_sqrt_price_at_tick(MAX_TICK), Some(MAX_SQRT_PRICE_X64));
        assert_eq!(get_sqrt_price_at_tick(0), Some(Q64));
        for tick in [-28861, -100, -1, 0, 1, 100, 28861, 400000] {
            let p = get_sqrt_price_at_tick(tick).unwrap();
            assert_eq!(get_tick_at_sqrt_price(p), Some(tick), "tick {tick}");
        }
    }

    #[test]
    fn swap_step_price_one_parity() {
        // liquidity 1e12 at price 1.0, no target constraint nearby.
        let target = get_sqrt_price_at_tick(-100).unwrap();
        let step = compute_swap_step(Q64, target, 1_000_000_000_000, 1_000_000, 2500, true).unwrap();
        // 0.25% fee, tiny price move: out ≈ in × (1 − fee) slightly less.
        assert!(step.amount_in + step.fee_amount == 1_000_000);
        assert!(step.amount_out > 995_000 && step.amount_out < 998_000, "{}", step.amount_out);
    }

    #[test]
    fn traversal_crosses_ticks() {
        // Pool at tick 0, spacing 1, liquidity concentrated: a tick at -5
        // drops liquidity to a tenth; big sell must cross it and get less.
        let mut pool = crate::RaydiumCLMMPool {
            bump: [0],
            amm_config: Pubkey::default(),
            owner: Pubkey::default(),
            token_mint_0: Pubkey::default(),
            token_mint_1: Pubkey::default(),
            token_vault_0: Pubkey::default(),
            token_vault_1: Pubkey::default(),
            observation_key: Pubkey::default(),
            mint_decimals_0: 9,
            mint_decimals_1: 9,
            tick_spacing: 1,
            liquidity: 10_000_000_000,
            sqrt_price_x64: Q64,
            tick_current: 0,
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
            reward_infos: core::array::from_fn(|_| crate::RewardInfo {
                reward_state: 0,
                open_time: 0,
                end_time: 0,
                last_update_time: 0,
                emissions_per_second_x64: 0,
                reward_total_emissioned: 0,
                reward_claimed: 0,
                token_mint: Pubkey::default(),
                token_vault: Pubkey::default(),
                authority: Pubkey::default(),
                reward_growth_global_x64: 0,
            }),
            tick_array_bitmap: [0; 16],
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
        };
        // Mark array 0 (offset 512) present in the bitmap.
        pool.tick_array_bitmap[512 / 64] |= 1 << (512 % 64);
        // Array -60..-1 present too (offset 511).
        pool.tick_array_bitmap[511 / 64] |= 1 << (511 % 64);

        let mk_array = |start: i32| {
            let mut ticks: Vec<TickInfo> = (0..60)
                .map(|i| TickInfo { tick: start + i, liquidity_net: 0, liquidity_gross: 0 })
                .collect();
            if start == -60 {
                // Tick -5: crossing downward removes 90% of liquidity.
                ticks[55] = TickInfo { tick: -5, liquidity_net: 9_000_000_000, liquidity_gross: 9_000_000_000 };
                // Floor tick at -55 holds the rest.
                ticks[5] = TickInfo { tick: -55, liquidity_net: 1_000_000_000, liquidity_gross: 1_000_000_000 };
            }
            ClmmTickArray { start_tick_index: start, ticks }
        };

        let mut fetch = |start: i32| Some(mk_array(start));
        // Sell token0 (price down). Enough to push through tick -5.
        let out = quote_exact_in(&pool, 2500, 3_000_000, true, &mut fetch).unwrap();
        assert!(out > 0);
        // Same trade with 10x liquidity everywhere must yield strictly more.
        pool.liquidity = 100_000_000_000;
        let mut fetch2 = |start: i32| {
            let mut arr = mk_array(start);
            if start == -60 {
                arr.ticks[55].liquidity_net = 90_000_000_000;
                arr.ticks[55].liquidity_gross = 90_000_000_000;
            }
            Some(arr)
        };
        let out_deep = quote_exact_in(&pool, 2500, 3_000_000, true, &mut fetch2).unwrap();
        assert!(out_deep > out, "deep {out_deep} vs {out}");
    }
}
