//! Offline DAMM V1 swap quoting with correct vault-share reserves and real
//! stableswap math.
//!
//! Ported from Meteora's official `damm-v1-sdk` (`dynamic-amm-quote` crate,
//! MIT) and the pinned `mercurial-finance/stable-swap` math it depends on.
//!
//! Two things the old solroute math got wrong:
//! 1. **Reserves.** DAMM V1 keeps liquidity in shared dynamic vaults; a
//!    pool's true reserve is its LP share of the vault's unlocked amount
//!    (`lp_amount × unlocked_total ÷ lp_supply`), NOT the vault's token
//!    balance. Using the vault total overstated depth for every pool on a
//!    shared vault (all SOL/USDC/USDT pairs).
//! 2. **Stable curve.** Real stableswap invariant (Curve D/y Newton
//!    iterations) with token multipliers and — for depeg pools (LSTs) — the
//!    cached `base_virtual_price` from pool state. The old code approximated
//!    with constant-product × an amp fudge factor, which is simply wrong
//!    (JitoSOL-SOL quoted 1.69 where truth is ~0.78).
//!
//! Deviations from the SDK (deliberate, quote-conservative):
//! - `base_virtual_price` is used as cached in pool state (no stake-pool
//!   account reads). It is refreshed on-chain by pool activity; LST rates
//!   move a few bps per day, so staleness is negligible.
//! - The vault deposit/withdraw LP round-trip is reproduced faithfully
//!   (floor at every step, like the program).

use crate::models::{CurveType, DepegType, MeteoraDAMMPool, VaultAuthority};

uint::construct_uint! {
    pub struct U192(3);
}

/// dynamic-amm fee::FEE_DENOMINATOR — not used directly (fees are exact
/// num/denom from pool state) but kept for reference parity.
pub const FEE_DENOMINATOR: u64 = 100_000;
/// dynamic-amm depeg::PRECISION — virtual-price scale for depeg pools.
pub const DEPEG_PRECISION: u64 = 1_000_000;
/// dynamic-vault LOCKED_PROFIT_DEGRADATION_DENOMINATOR.
const LOCKED_PROFIT_DEGRADATION_DENOMINATOR: u128 = 1_000_000_000_000;
const N_COINS: u64 = 2;

// ---------------------------------------------------------------------------
// Vault share math (dynamic-vault state.rs)
// ---------------------------------------------------------------------------

/// Locked profit remaining at `current_time` (Yearn-style linear release).
fn calculate_locked_profit(vault: &VaultAuthority, current_time: u64) -> Option<u64> {
    let t = &vault.locked_profit_tracker;
    let duration = u128::from(current_time.checked_sub(t.last_report)?);
    let degradation = u128::from(t.locked_profit_degradation);
    let locked_fund_ratio = duration.checked_mul(degradation)?;
    if locked_fund_ratio > LOCKED_PROFIT_DEGRADATION_DENOMINATOR {
        return Some(0);
    }
    let locked_profit = u128::from(t.last_updated_locked_profit)
        .checked_mul(
            LOCKED_PROFIT_DEGRADATION_DENOMINATOR.checked_sub(locked_fund_ratio)?,
        )?
        .checked_div(LOCKED_PROFIT_DEGRADATION_DENOMINATOR)?;
    u64::try_from(locked_profit).ok()
}

/// Vault total minus still-locked profit.
pub fn get_unlocked_amount(vault: &VaultAuthority, current_time: u64) -> Option<u64> {
    vault
        .total_amount
        .checked_sub(calculate_locked_profit(vault, current_time)?)
}

/// Token amount corresponding to `share` LP of the vault.
pub fn get_amount_by_share(
    vault: &VaultAuthority,
    current_time: u64,
    share: u64,
    total_supply: u64,
) -> Option<u64> {
    if total_supply == 0 {
        return None;
    }
    let total = get_unlocked_amount(vault, current_time)?;
    u64::try_from(
        u128::from(share)
            .checked_mul(u128::from(total))?
            .checked_div(u128::from(total_supply))?,
    )
    .ok()
}

/// LP amount that `out_token` tokens correspond to (inverse of the above).
pub fn get_unmint_amount(
    vault: &VaultAuthority,
    current_time: u64,
    out_token: u64,
    total_supply: u64,
) -> Option<u64> {
    let total = get_unlocked_amount(vault, current_time)?;
    if total == 0 {
        return None;
    }
    u64::try_from(
        u128::from(out_token)
            .checked_mul(u128::from(total_supply))?
            .checked_div(u128::from(total))?,
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Fees (dynamic-amm state.rs calculate_fee: floor, minimum 1)
// ---------------------------------------------------------------------------

fn calculate_fee(token_amount: u128, numerator: u128, denominator: u128) -> Option<u128> {
    if numerator == 0 || token_amount == 0 {
        return Some(0);
    }
    let fee = token_amount.checked_mul(numerator)?.checked_div(denominator)?;
    if fee == 0 { Some(1) } else { Some(fee) }
}

// ---------------------------------------------------------------------------
// Constant product (spl-token-swap, ceil-div variant)
// ---------------------------------------------------------------------------

/// spl-math CheckedCeilDiv: ceil quotient, then round the divisor back down.
fn checked_ceil_div(dividend: u128, mut divisor: u128) -> Option<(u128, u128)> {
    let mut quotient = dividend.checked_div(divisor)?;
    if quotient == 0 {
        return None;
    }
    let remainder = dividend.checked_rem(divisor)?;
    if remainder > 0 {
        quotient = quotient.checked_add(1)?;
        divisor = dividend.checked_div(quotient)?;
        let remainder = dividend.checked_rem(quotient)?;
        if remainder > 0 {
            divisor = divisor.checked_add(1)?;
        }
    }
    Some((quotient, divisor))
}

/// Constant-product swap without fees (fees handled by the caller).
fn constant_product_swap_exact(
    source_amount: u128,
    swap_source_amount: u128,
    swap_destination_amount: u128,
) -> Option<u128> {
    let invariant = swap_source_amount.checked_mul(swap_destination_amount)?;
    let new_swap_source_amount = swap_source_amount.checked_add(source_amount)?;
    let (new_swap_destination_amount, _) = checked_ceil_div(invariant, new_swap_source_amount)?;
    let destination_amount_swapped =
        swap_destination_amount.checked_sub(new_swap_destination_amount)?;
    if destination_amount_swapped == 0 {
        None
    } else {
        Some(destination_amount_swapped)
    }
}

// ---------------------------------------------------------------------------
// Stableswap (mercurial-finance/stable-swap math, amp fixed — no ramp)
// ---------------------------------------------------------------------------

fn compute_next_d(amp: u64, d_init: U192, d_prod: U192, sum_x: u128) -> Option<U192> {
    let ann = amp.checked_mul(N_COINS)?;
    let leverage = sum_x.checked_mul(ann.into())?;
    // d = (ann * sum_x + d_prod * n) * d / ((ann - 1) * d + (n + 1) * d_prod)
    let numerator = d_init.checked_mul(
        d_prod
            .checked_mul(N_COINS.into())?
            .checked_add(leverage.into())?,
    )?;
    let denominator = d_init
        .checked_mul((ann.checked_sub(1)?).into())?
        .checked_add(d_prod.checked_mul((N_COINS + 1).into())?)?;
    numerator.checked_div(denominator)
}

/// Stableswap invariant D via Newton's method.
fn compute_d(amp: u64, amount_a: u128, amount_b: u128) -> Option<U192> {
    let sum_x = amount_a.checked_add(amount_b)?;
    if sum_x == 0 {
        return Some(0.into());
    }
    let amount_a_times_coins = amount_a.checked_mul(N_COINS.into())?;
    let amount_b_times_coins = amount_b.checked_mul(N_COINS.into())?;

    let mut d_prev: U192;
    let mut d: U192 = sum_x.into();
    for _ in 0..256 {
        let mut d_prod = d;
        d_prod = d_prod.checked_mul(d)?.checked_div(amount_a_times_coins.into())?;
        d_prod = d_prod.checked_mul(d)?.checked_div(amount_b_times_coins.into())?;
        d_prev = d;
        d = compute_next_d(amp, d, d_prod, sum_x)?;
        if d > d_prev {
            if d.checked_sub(d_prev)? <= 1.into() {
                break;
            }
        } else if d_prev.checked_sub(d)? <= 1.into() {
            break;
        }
    }
    Some(d)
}

/// Solve for the destination-side balance y given source-side balance x.
fn compute_y(amp: u64, x: u128, d: U192) -> Option<u128> {
    let ann = amp.checked_mul(N_COINS)?;

    // c = D**(n+1) / (n**(2n) * x * ann)
    let mut c = d.checked_mul(d)?.checked_div(x.checked_mul(N_COINS.into())?.into())?;
    c = c.checked_mul(d)?.checked_div((ann.checked_mul(N_COINS)?).into())?;
    // b = x + D / ann  (d subtracted in the loop denominator)
    let b = d.checked_div(ann.into())?.checked_add(x.into())?;

    let mut y_prev: U192;
    let mut y = d;
    for _ in 0..256 {
        y_prev = y;
        // y = (y² + c) / (2y + b − D)
        let numerator = y.checked_pow(2.into())?.checked_add(c)?;
        let denominator = y.checked_mul(2.into())?.checked_add(b)?.checked_sub(d)?;
        y = numerator.checked_div(denominator)?;
        if y > y_prev {
            if y.checked_sub(y_prev)? <= 1.into() {
                break;
            }
        } else if y_prev.checked_sub(y)? <= 1.into() {
            break;
        }
    }
    u128::try_from(y).ok()
}

/// swap_to2 with zero curve-level fees (the pool fee is charged outside the
/// curve by the caller, matching dynamic-amm-quote).
fn stable_swap_exact(
    amp: u64,
    source_amount: u128,
    swap_source_amount: u128,
    swap_destination_amount: u128,
) -> Option<u128> {
    let d = compute_d(amp, swap_source_amount, swap_destination_amount)?;
    let y = compute_y(amp, swap_source_amount.checked_add(source_amount)?, d)?;
    // Curve convention: subtract 1 to round against the trader.
    swap_destination_amount.checked_sub(y)?.checked_sub(1)
}

// ---------------------------------------------------------------------------
// Depeg / multiplier scaling (dynamic-amm-quote stable_swap.rs)
// ---------------------------------------------------------------------------

struct StableScaler<'a> {
    token_a_multiplier: u64,
    token_b_multiplier: u64,
    depeg_type: &'a DepegType,
    base_virtual_price: u64,
}

impl StableScaler<'_> {
    fn upscale_a(&self, amount: u128) -> Option<u128> {
        let n = amount.checked_mul(self.token_a_multiplier.into())?;
        if !matches!(self.depeg_type, DepegType::None) {
            n.checked_mul(DEPEG_PRECISION.into())
        } else {
            Some(n)
        }
    }
    fn downscale_a(&self, amount: u128) -> Option<u128> {
        let n = amount.checked_div(self.token_a_multiplier.into())?;
        if !matches!(self.depeg_type, DepegType::None) {
            n.checked_div(DEPEG_PRECISION.into())
        } else {
            Some(n)
        }
    }
    fn upscale_b(&self, amount: u128) -> Option<u128> {
        let n = amount.checked_mul(self.token_b_multiplier.into())?;
        if !matches!(self.depeg_type, DepegType::None) {
            n.checked_mul(self.base_virtual_price.into())
        } else {
            Some(n)
        }
    }
    fn downscale_b(&self, amount: u128) -> Option<u128> {
        let n = amount.checked_div(self.token_b_multiplier.into())?;
        if !matches!(self.depeg_type, DepegType::None) {
            n.checked_div(self.base_virtual_price.into())
        } else {
            Some(n)
        }
    }
}

// ---------------------------------------------------------------------------
// Full quote (dynamic-amm-quote lib.rs compute_quote)
// ---------------------------------------------------------------------------

/// Everything the quote needs about one side's vault.
pub struct VaultSide<'a> {
    /// Vault state account.
    pub vault: &'a VaultAuthority,
    /// Pool's LP token balance in this vault (pool.X_vault_lp token account).
    pub pool_lp_amount: u64,
    /// Vault LP mint supply.
    pub lp_supply: u64,
    /// Vault's token account balance (reserve withdrawable right now).
    pub vault_token_amount: u64,
}

/// Exact-in DAMM V1 quote. `in_is_a` = the input token is the pool's token A.
pub fn quote_exact_in(
    pool: &MeteoraDAMMPool,
    in_amount: u64,
    in_is_a: bool,
    vault_a: &VaultSide,
    vault_b: &VaultSide,
    current_time: u64,
) -> Option<u64> {
    if !pool.enabled {
        return None;
    }

    // Pool's true reserves = its LP share of each vault's unlocked amount.
    let token_a_amount = get_amount_by_share(
        vault_a.vault,
        current_time,
        vault_a.pool_lp_amount,
        vault_a.lp_supply,
    )?;
    let token_b_amount = get_amount_by_share(
        vault_b.vault,
        current_time,
        vault_b.pool_lp_amount,
        vault_b.lp_supply,
    )?;

    let (in_vault, out_vault, in_token_total, out_token_total) = if in_is_a {
        (vault_a, vault_b, token_a_amount, token_b_amount)
    } else {
        (vault_b, vault_a, token_b_amount, token_a_amount)
    };

    // Fees on input (protocol fee is a cut of the trade fee).
    let trade_fee = calculate_fee(
        in_amount.into(),
        pool.fees.trade_fee_numerator.into(),
        pool.fees.trade_fee_denominator.into(),
    )?;
    let protocol_fee = calculate_fee(
        trade_fee,
        pool.fees.protocol_trade_fee_numerator.into(),
        pool.fees.protocol_trade_fee_denominator.into(),
    )?;
    let trade_fee = trade_fee.checked_sub(protocol_fee)?;
    let in_amount_after_protocol_fee =
        in_amount.checked_sub(u64::try_from(protocol_fee).ok()?)?;

    // Vault deposit round-trip: actual credited amount differs from the
    // deposit by vault LP floor-rounding. Reproduce it exactly.
    let in_lp = get_unmint_amount(
        in_vault.vault,
        current_time,
        in_amount_after_protocol_fee,
        in_vault.lp_supply,
    )?;
    let mut in_vault_after = (*in_vault.vault).clone();
    in_vault_after.total_amount = in_vault
        .vault
        .total_amount
        .checked_add(in_amount_after_protocol_fee)?;
    let after_in_token_total = get_amount_by_share(
        &in_vault_after,
        current_time,
        in_lp.checked_add(in_vault.pool_lp_amount)?,
        in_vault.lp_supply.checked_add(in_lp)?,
    )?;
    let actual_in_amount = after_in_token_total.checked_sub(in_token_total)?;
    let actual_in_after_fee = actual_in_amount.checked_sub(u64::try_from(trade_fee).ok()?)?;

    // Curve swap on true reserves.
    let destination_amount: u128 = match &pool.curve_type {
        CurveType::ConstantProduct => constant_product_swap_exact(
            actual_in_after_fee.into(),
            in_token_total.into(),
            out_token_total.into(),
        )?,
        CurveType::Stable {
            amp,
            token_multiplier,
            depeg,
            ..
        } => {
            let scaler = StableScaler {
                token_a_multiplier: token_multiplier.token_a_multiplier,
                token_b_multiplier: token_multiplier.token_b_multiplier,
                depeg_type: &depeg.depeg_type,
                base_virtual_price: depeg.base_virtual_price.max(1),
            };
            let (up_src, up_src_res, up_dst_res) = if in_is_a {
                (
                    scaler.upscale_a(actual_in_after_fee.into())?,
                    scaler.upscale_a(in_token_total.into())?,
                    scaler.upscale_b(out_token_total.into())?,
                )
            } else {
                (
                    scaler.upscale_b(actual_in_after_fee.into())?,
                    scaler.upscale_b(in_token_total.into())?,
                    scaler.upscale_a(out_token_total.into())?,
                )
            };
            let out = stable_swap_exact(*amp, up_src, up_src_res, up_dst_res)?;
            if in_is_a {
                scaler.downscale_b(out)?
            } else {
                scaler.downscale_a(out)?
            }
        }
    };

    // Vault withdraw round-trip on the output side.
    let out_lp = get_unmint_amount(
        out_vault.vault,
        current_time,
        u64::try_from(destination_amount).ok()?,
        out_vault.lp_supply,
    )?;
    let out_amount =
        get_amount_by_share(out_vault.vault, current_time, out_lp, out_vault.lp_supply)?;

    // The vault must be able to pay out from its liquid reserve.
    if out_amount >= out_vault.vault_token_amount {
        return None;
    }

    Some(out_amount)
}

// ---------------------------------------------------------------------------
// Account byte parsing helpers
// ---------------------------------------------------------------------------

/// Parse a vault state account (skips the 8-byte anchor discriminator).
pub fn parse_vault(data: &[u8]) -> Option<VaultAuthority> {
    if data.len() < 8 {
        return None;
    }
    borsh::BorshDeserialize::deserialize(&mut &data[8..]).ok()
}

/// SPL token account balance (offset 64..72).
pub fn parse_token_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72)?.try_into().ok().map(u64::from_le_bytes)
}

/// SPL mint supply (offset 36..44).
pub fn parse_mint_supply(data: &[u8]) -> Option<u64> {
    data.get(36..44)?.try_into().ok().map(u64::from_le_bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_div_matches_spl_semantics() {
        // 10 / 3 -> ceil 4, divisor rounds back: 10/4=2 rem 2 -> 3
        assert_eq!(checked_ceil_div(10, 3), Some((4, 3)));
        // Exact division unchanged.
        assert_eq!(checked_ceil_div(10, 5), Some((2, 5)));
    }

    #[test]
    fn constant_product_basics() {
        // 1000 in, reserves 1M/1M -> out ≈ 999 (floor + ceil-div rounding)
        let out = constant_product_swap_exact(1_000, 1_000_000, 1_000_000).unwrap();
        assert!(out == 999 || out == 998, "{out}");
    }

    #[test]
    fn stable_swap_near_parity_on_balanced_pool() {
        // Balanced stable pool, amp 100: 10k in vs 10M reserves -> ~1:1 out.
        let out = stable_swap_exact(100, 10_000, 10_000_000, 10_000_000).unwrap();
        let diff = (out as i128 - 10_000).abs();
        assert!(diff < 50, "expected ~10_000, got {out}");
    }

    #[test]
    fn stable_swap_stays_near_parity_on_imbalance() {
        // Destination-rich reserves: CP overpays wildly (~3:1); stableswap
        // must stay near 1:1 with only a small imbalance premium.
        let cp = constant_product_swap_exact(10_000, 2_000_000, 6_000_000).unwrap();
        let ss = stable_swap_exact(100, 10_000, 2_000_000, 6_000_000).unwrap();
        assert!(cp > 25_000, "CP sanity: {cp}");
        assert!(ss > 10_000 && ss < 10_500, "stable should be ~parity+premium: {ss}");
    }

    #[test]
    fn depeg_scaling_applies_virtual_price() {
        // Depeg pool: token A is SOL (upscaled by PRECISION), token B is the
        // LST worth 1.2 SOL (upscaled by virtual price 1.2e6). Balanced in
        // value: 1.2M SOL vs 1M LST.
        let pool_stable = |in_is_a: bool, amount: u128| {
            let scaler = StableScaler {
                token_a_multiplier: 1,
                token_b_multiplier: 1,
                depeg_type: &DepegType::SplStake,
                base_virtual_price: 1_200_000,
            };
            let (a_res, b_res) = (1_200_000u128, 1_000_000u128);
            let (src, src_res, dst_res) = if in_is_a {
                (
                    scaler.upscale_a(amount).unwrap(),
                    scaler.upscale_a(a_res).unwrap(),
                    scaler.upscale_b(b_res).unwrap(),
                )
            } else {
                (
                    scaler.upscale_b(amount).unwrap(),
                    scaler.upscale_b(b_res).unwrap(),
                    scaler.upscale_a(a_res).unwrap(),
                )
            };
            let out = stable_swap_exact(100, src, src_res, dst_res).unwrap();
            if in_is_a {
                scaler.downscale_b(out).unwrap()
            } else {
                scaler.downscale_a(out).unwrap()
            }
        };

        // A (SOL) in -> B (LST) out: ~1000/1.2 ≈ 833
        let lst_out = pool_stable(true, 1_000);
        assert!((820..=840).contains(&(lst_out as i64)), "{lst_out}");
        // B (LST) in -> A (SOL) out: ~1.2 × 1000 ≈ 1200
        let sol_out = pool_stable(false, 1_000);
        assert!((1185..=1205).contains(&(sol_out as i64)), "{sol_out}");
    }

    #[test]
    fn locked_profit_degrades_linearly() {
        use crate::models::{LockedProfitTracker, VaultBumps};
        let vault = VaultAuthority {
            enabled: [1],
            bumps: VaultBumps { vault_bump: 0, token_vault_bump: 0 },
            total_amount: 1_000_000,
            token_vault: Default::default(),
            fee_vault: Default::default(),
            token_mint: Default::default(),
            lp_mint: Default::default(),
            strategies: [Default::default(); 30],
            base: Default::default(),
            admin: Default::default(),
            operator: Default::default(),
            locked_profit_tracker: LockedProfitTracker {
                last_updated_locked_profit: 100_000,
                last_report: 1_000,
                // Full release over 100s: denominator / 100
                locked_profit_degradation: 10_000_000_000,
            },
        };
        // At t = last_report: everything locked.
        assert_eq!(get_unlocked_amount(&vault, 1_000), Some(900_000));
        // Halfway (50s): half released.
        assert_eq!(get_unlocked_amount(&vault, 1_050), Some(950_000));
        // Past window: all released.
        assert_eq!(get_unlocked_amount(&vault, 1_200), Some(1_000_000));
    }
}
