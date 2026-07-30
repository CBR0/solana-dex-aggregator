//! Meteora DBC swap quoting — exact port of the on-chain `get_swap_result`
//! (github.com/MeteoraAg/dynamic-bonding-curve, program v0.1.6).
//!
//! Concentrated-liquidity sqrt-price (Q64.64) curve over a fixed ladder of up to
//! 20 `{sqrt_price, liquidity}` segments. Segment i spans
//! `(prev_upper, curve[i].sqrt_price]` (segment 0's lower bound is
//! `sqrt_start_price`) with liquidity `curve[i].liquidity`.
//!
//! Per-segment CLMM deltas (liquidity and sqrt-price both Q64.64):
//!   Δbase  = L·(√Pu − √Pl) / (√Pl·√Pu)
//!   Δquote = L·(√Pu − √Pl) >> 128
//! Rounding is load-bearing: output amounts round DOWN, segment capacity rounds UP.

use crate::{PoolConfig, VirtualPool, MAX_CURVE_POINTS};

uint::construct_uint! { pub struct U256(4); }
uint::construct_uint! { pub struct U512(8); }

pub const FEE_DENOMINATOR: u128 = 1_000_000_000;
pub const MAX_FEE_NUMERATOR: u64 = 990_000_000;
const PROTOCOL_FEE_PERCENT: u128 = 20;

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

/// (a * b) / c on U256 via a U512 intermediate.
fn mul_div(a: U256, b: U256, c: U256, rounding: Rounding) -> Option<U256> {
    if c.is_zero() {
        return None;
    }
    let (a5, b5, c5) = (widen(a), widen(b), widen(c));
    let prod = a5.checked_mul(b5)?;
    let mut q = prod / c5;
    if rounding == Rounding::Up && (prod % c5) != U512::zero() {
        q = q.checked_add(U512::one())?;
    }
    narrow(q)
}

/// Δbase = L·(upper − lower) / (lower·upper).
fn delta_base(lower: u128, upper: u128, liquidity: u128, rounding: Rounding) -> Option<u128> {
    if upper <= lower || liquidity == 0 {
        return Some(0);
    }
    let denom = U256::from(lower).checked_mul(U256::from(upper))?;
    let res = mul_div(U256::from(liquidity), U256::from(upper - lower), denom, rounding)?;
    (res <= U256::from(u128::MAX)).then(|| res.as_u128())
}

/// Δquote = L·(upper − lower) >> 128.
fn delta_quote(lower: u128, upper: u128, liquidity: u128, rounding: Rounding) -> Option<u128> {
    if upper <= lower || liquidity == 0 {
        return Some(0);
    }
    let prod = U256::from(liquidity).checked_mul(U256::from(upper - lower))?;
    let shift = U256::one() << 128;
    let mut q = prod >> 128;
    if rounding == Rounding::Up && (prod % shift) != U256::zero() {
        q = q.checked_add(U256::one())?;
    }
    (q <= U256::from(u128::MAX)).then(|| q.as_u128())
}

/// Buy: price rises. √P' = √P + (amount << 128) / L  (round down).
fn next_sqrt_from_quote_in(sqrt_price: u128, liquidity: u128, amount: u128) -> Option<u128> {
    if liquidity == 0 {
        return None;
    }
    let quotient = (U256::from(amount) << 128) / U256::from(liquidity);
    let res = U256::from(sqrt_price).checked_add(quotient)?;
    (res <= U256::from(u128::MAX)).then(|| res.as_u128())
}

/// Sell: price falls. √P' = √P·L / (L + amount·√P)  (round up).
fn next_sqrt_from_base_in(sqrt_price: u128, liquidity: u128, amount: u128) -> Option<u128> {
    let prod = U256::from(amount).checked_mul(U256::from(sqrt_price))?;
    let denom = U256::from(liquidity).checked_add(prod)?;
    let res = mul_div(U256::from(sqrt_price), U256::from(liquidity), denom, Rounding::Up)?;
    (res <= U256::from(u128::MAX)).then(|| res.as_u128())
}

/// QUOTE → BASE (buy). Returns base output (curve only, no fee), price-capped at
/// `migration_sqrt_price`.
fn quote_to_base(pool: &VirtualPool, config: &PoolConfig, amount_in: u128) -> Option<u128> {
    let mut current = pool.sqrt_price;
    let mut amount_left = amount_in;
    let mut total_out: u128 = 0;
    let stop = config.migration_sqrt_price;

    for i in 0..MAX_CURVE_POINTS {
        let seg = config.curve[i];
        if seg.sqrt_price == 0 || seg.liquidity == 0 {
            break;
        }
        let ref_price = stop.min(seg.sqrt_price);
        if ref_price > current {
            let max_in = delta_quote(current, ref_price, seg.liquidity, Rounding::Up)?;
            if amount_left < max_in {
                let next = next_sqrt_from_quote_in(current, seg.liquidity, amount_left)?;
                total_out = total_out.checked_add(delta_base(current, next, seg.liquidity, Rounding::Down)?)?;
                amount_left = 0;
                break;
            } else {
                total_out = total_out.checked_add(delta_base(current, ref_price, seg.liquidity, Rounding::Down)?)?;
                current = ref_price;
                amount_left -= max_in;
                if ref_price == stop {
                    break;
                }
            }
        }
    }
    Some(total_out)
}

/// BASE → QUOTE (sell). Returns quote output (curve only, no fee).
fn base_to_quote(pool: &VirtualPool, config: &PoolConfig, amount_in: u128) -> Option<u128> {
    let mut current = pool.sqrt_price;
    let mut amount_left = amount_in;
    let mut total_out: u128 = 0;

    for i in (0..MAX_CURVE_POINTS - 1).rev() {
        let seg = config.curve[i];
        if seg.sqrt_price == 0 || seg.liquidity == 0 {
            continue;
        }
        if seg.sqrt_price < current {
            let l = config.curve[i + 1].liquidity;
            let max_in = delta_base(seg.sqrt_price, current, l, Rounding::Up)?;
            if amount_left < max_in {
                let next = next_sqrt_from_base_in(current, l, amount_left)?;
                total_out = total_out.checked_add(delta_quote(next, current, l, Rounding::Down)?)?;
                current = next;
                amount_left = 0;
                break;
            } else {
                total_out = total_out.checked_add(delta_quote(seg.sqrt_price, current, l, Rounding::Down)?)?;
                current = seg.sqrt_price;
                amount_left -= max_in;
            }
        }
    }

    // Tail: below curve[0].sqrt_price down to sqrt_start_price, liquidity curve[0].
    if amount_left != 0 {
        let l = config.curve[0].liquidity;
        let mut next = next_sqrt_from_base_in(current, l, amount_left)?;
        if next < config.sqrt_start_price {
            next = config.sqrt_start_price;
        }
        total_out = total_out.checked_add(delta_quote(next, current, l, Rounding::Down)?)?;
    }
    Some(total_out)
}

/// Variable (dynamic) fee numerator from the pool's stored volatility. Zero when
/// dynamic fees are not enabled (the common case).
fn variable_fee_numerator(pool: &VirtualPool, config: &PoolConfig) -> u64 {
    let df = &config.pool_fees.dynamic_fee;
    if df.initialized == 0 {
        return 0;
    }
    let vol = pool.volatility_tracker.volatility_accumulator;
    // (volatility_accumulator * bin_step)^2 * variable_fee_control, ceil-div 1e11.
    let vfa = match U256::from(vol).checked_mul(U256::from(df.bin_step)) {
        Some(v) => v,
        None => return 0,
    };
    let square = match vfa.checked_mul(vfa) {
        Some(s) => s,
        None => return MAX_FEE_NUMERATOR,
    };
    let v_fee = match square.checked_mul(U256::from(df.variable_fee_control)) {
        Some(v) => v,
        None => return MAX_FEE_NUMERATOR,
    };
    let denom = U256::from(100_000_000_000u128);
    let num = (v_fee + denom - U256::one()) / denom;
    if num > U256::from(MAX_FEE_NUMERATOR) {
        MAX_FEE_NUMERATOR
    } else {
        num.as_u64()
    }
}

/// Total trading fee numerator (base + variable, capped). NOTE: base fee uses the
/// flat `cliff_fee_numerator`; time-decayed fee schedulers and the size-dependent
/// rate limiter (`base_fee_mode == 2`) are not modeled yet — those pools quote
/// with the cliff (max) fee, a conservative slight under-estimate of output.
fn total_fee_numerator(pool: &VirtualPool, config: &PoolConfig) -> u64 {
    let base = config.pool_fees.base_fee.cliff_fee_numerator;
    let total = base.saturating_add(variable_fee_numerator(pool, config));
    total.min(MAX_FEE_NUMERATOR)
}

/// Trading fee on an amount = ceil(amount * fee_numerator / 1e9).
fn fee_on_amount(amount: u128, fee_numerator: u64) -> u128 {
    let prod = amount.saturating_mul(fee_numerator as u128);
    prod.div_ceil(FEE_DENOMINATOR)
}

/// True if the base fee uses the size-dependent rate limiter (mode 2) — quotes
/// for such pools may be off during the limiter window.
pub fn uses_rate_limiter(config: &PoolConfig) -> bool {
    config.pool_fees.base_fee.base_fee_mode == 2
}

/// Whether the pool is still tradeable on-curve.
pub fn is_tradeable(pool: &VirtualPool, config: &PoolConfig) -> bool {
    pool.is_migrated == 0
        && pool.migration_progress == 0
        && pool.quote_reserve < config.migration_quote_threshold
}

/// Exact-in quote (post-fee). `buy = true` spends quote (WSOL) for base; `false`
/// spends base for quote. Returns output token amount, or None if not tradeable /
/// math overflow / zero output.
pub fn quote_exact_in(
    pool: &VirtualPool,
    config: &PoolConfig,
    amount_in: u64,
    buy: bool,
) -> Option<u64> {
    if amount_in == 0 || !is_tradeable(pool, config) {
        return None;
    }
    let fee_num = total_fee_numerator(pool, config);
    let quote_token_mode = config.collect_fee_mode == 0;

    let out: u128 = if buy {
        if quote_token_mode {
            // Fee on input (quote), then curve.
            let fee = fee_on_amount(amount_in as u128, fee_num);
            let net = (amount_in as u128).checked_sub(fee)?;
            quote_to_base(pool, config, net)?
        } else {
            // Curve, then fee on output (base).
            let gross = quote_to_base(pool, config, amount_in as u128)?;
            gross.checked_sub(fee_on_amount(gross, fee_num))?
        }
    } else {
        // Sell: fee always on quote output.
        let gross = base_to_quote(pool, config, amount_in as u128)?;
        gross.checked_sub(fee_on_amount(gross, fee_num))?
    };

    (out > 0 && out <= u64::MAX as u128).then_some(out as u64)
}

/// Fee numerator exposed for the `Market` fee metadata (as bps ≈ numerator/1e5).
pub fn fee_numerator(pool: &VirtualPool, config: &PoolConfig) -> u64 {
    total_fee_numerator(pool, config)
}

// A minimal borrow of the protocol split, kept for parity/documentation.
#[allow(dead_code)]
fn protocol_split(trading_fee: u128) -> u128 {
    trading_fee * PROTOCOL_FEE_PERCENT / 100
}
