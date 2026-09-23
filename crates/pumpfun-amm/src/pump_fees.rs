//! PumpSwap (pump-amm) fee schedule + exact quote math.
//!
//! Ported from the official `@pump-fun/pump-swap-sdk` (v1.20.0, `src/sdk/buy.ts`,
//! `sell.ts`, `fees.ts`, `util.ts`) and validated against live mainnet events
//! (3 buys + 1 sell on the FAMILY pool — exact match on output and on every fee
//! slice, 2026-09-23).
//!
//! Two things the legacy 100-bps CPMM-on-vaults model got wrong:
//!
//! 1. **Effective quote reserves** — pools can carry a `virtual_quote_reserves`
//!    (i128) appended to the `Pool` account; quotes price against
//!    `vault_quote_balance + virtual_quote_reserves`. Boosted pools carry tens of
//!    SOL of virtual reserves (~7-13% price offset), which the raw vault ratio
//!    cannot see.
//! 2. **Tiered fees** — canonical pump pools pay `lp 20 + protocol 5 + creator
//!    5..95 bps` by market-cap tier from the pump-fees `FeeConfig` account, not a
//!    flat 100 bps.
//!
//! Integer math mirrors the SDK exactly (verified):
//!
//! * buy:  `eff = floor(q*1e4/(1e4+total))`, each fee `ceil(eff*bps/1e4)`,
//!         `input = eff - 1`, `out = floor(base*input/(E+input))`
//! * sell: `raw = floor(E*base_in/(base+base_in))`, `out = raw - Σ ceil(raw*bps/1e4)`

use solana_pubkey::Pubkey;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const PUMPFUN_AMM_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
pub const PUMPFUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const PUMPFUN_FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
/// pump-amm `GlobalConfig` PDA (`["global_config"]`).
pub const PUMP_AMM_GLOBAL_CONFIG: &str = "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw";
/// pump-fees `FeeConfig` PDA (`["fee_config", pump_amm_program]`).
pub const PUMP_AMM_FEE_CONFIG: &str = "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx";
/// Token-2022 native mint — SOL-like for the fee schedule.
pub const WSOL_2022: &str = "9pan9bMn5HatX4EJdBwg9VgCa7Uz5HL8N1m5D3NdXejP";
/// pump-amm `TOTAL_TOKEN_SUPPLY` used as market-cap basis for mayhem pools.
pub const MAYHEM_TOTAL_SUPPLY: u128 = 1_000_000_000_000_000;

/// Anchor discriminators.
pub const DISC_FEE_CONFIG: [u8; 8] = [0x8f, 0x34, 0x92, 0xbb, 0xdb, 0x7b, 0x4c, 0x9b];
pub const DISC_GLOBAL_CONFIG: [u8; 8] = [0x95, 0x08, 0x9c, 0xca, 0xa0, 0xfc, 0xb0, 0xd9];

/// Offset of `Pool::virtual_quote_reserves` (i128) in the pool account
/// (8 disc + 1 bump + 2 index + 8×32 pubkeys + 8 lp_supply + 32 coin_creator
///  + 1 mayhem + 1 cashback = 245).
pub const POOL_VIRTUAL_QUOTE_RESERVES_OFFSET: usize = 245;
/// Offset of the appended `Pool::creator_fee_bps` (u64) right after it.
pub const POOL_CREATOR_FEE_BPS_OFFSET: usize = POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 16;

/// Conservative fallback when the fee schedule cannot be resolved: the highest
/// total on the live tier table (tier-0 `2 + 93 + 30 = 125 bps`). Overestimating
/// fees can only hide opportunities (the recheck re-prices exactly), never
/// invent them.
pub const FALLBACK_FEES: Fees = Fees { lp_bps: 2, protocol_bps: 93, creator_bps: 30 };

// ---------------------------------------------------------------------------
// Fees
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fees {
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

impl Fees {
    pub fn total_bps(&self) -> u64 {
        self.lp_bps
            .saturating_add(self.protocol_bps)
            .saturating_add(self.creator_bps)
    }

    fn is_zero(&self) -> bool {
        self.total_bps() == 0
    }

    /// Drop the creator slice (pools with `coin_creator == default` pay none).
    pub fn without_creator(mut self) -> Self {
        self.creator_bps = 0;
        self
    }
}

/// pump-fees `FeeConfig` (the schedule lives on-chain so tiers can change
/// without a program upgrade).
#[derive(Clone, Debug, Default)]
pub struct FeeConfig {
    pub flat_fees: Fees,
    /// `(market_cap_lamports_threshold, fees)`, ascending.
    pub fee_tiers: Vec<(u128, Fees)>,
    pub stable_fee_tiers: Vec<(u128, Fees)>,
    pub exotic_flat_fees: Fees,
}

/// pump-amm `GlobalConfig` — the fields the quote needs.
#[derive(Clone, Debug, Default)]
pub struct GlobalConfig {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub coin_creator_fee_bps: u64,
    pub creator_fee_configurable: bool,
}

// ---------------------------------------------------------------------------
// Parsing (cursor-based, tolerates shorter legacy accounts)
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn u128(&mut self) -> Option<u128> {
        self.take(16)
            .map(|b| u128::from_le_bytes(b.try_into().unwrap()))
    }
    fn fees(&mut self) -> Option<Fees> {
        Some(Fees {
            lp_bps: self.u64()?,
            protocol_bps: self.u64()?,
            creator_bps: self.u64()?,
        })
    }
    fn tiers(&mut self) -> Option<Vec<(u128, Fees)>> {
        let n = self.u32()? as usize;
        // Defensive cap: a bogus length must not allocate GBs.
        if n > 4096 {
            return None;
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let threshold = self.u128()?;
            let fees = self.fees()?;
            out.push((threshold, fees));
        }
        Some(out)
    }
}

/// Parse a pump-fees `FeeConfig` account (raw bytes, discriminator included).
pub fn parse_fee_config(data: &[u8]) -> Option<FeeConfig> {
    if data.len() < 8 || data[..8] != DISC_FEE_CONFIG {
        return None;
    }
    let mut c = Cursor::new(&data[8..]);
    let _bump = c.u8()?;
    c.take(32)?; // admin
    let flat_fees = c.fees()?;
    let fee_tiers = c.tiers()?;
    let stable_fee_tiers = c.tiers()?;
    let exotic_flat_fees = c.fees()?;
    Some(FeeConfig {
        flat_fees,
        fee_tiers,
        stable_fee_tiers,
        exotic_flat_fees,
    })
}

/// Parse the pump-amm `GlobalConfig` account (raw bytes, discriminator included).
pub fn parse_global_config(data: &[u8]) -> Option<GlobalConfig> {
    if data.len() < 8 || data[..8] != DISC_GLOBAL_CONFIG {
        return None;
    }
    let mut c = Cursor::new(&data[8..]);
    c.take(32)?; // admin
    let lp_fee_bps = c.u64()?;
    let protocol_fee_bps = c.u64()?;
    let _disable_flags = c.u8()?;
    c.take(32 * 8)?; // protocol_fee_recipients
    let coin_creator_fee_bps = c.u64()?;
    // Everything past here is only needed for `creator_fee_configurable`; old
    // (shorter) accounts simply decode with the flag off.
    let mut creator_fee_configurable = false;
    if c.take(32 * 3 + 32).is_some() // admin_set_coin_creator_authority, whitelist, reserved_fee_recipient
        && c.u8().is_some() // mayhem_mode_enabled
        && c.take(32 * 7 + 1 + 32 * 8 + 8 + 32 + 1).is_some() // reserved recipients, cashback, buyback recipients, buyback bps, boost authority, boost enabled
    {
        creator_fee_configurable = c.u8().unwrap_or(0) != 0;
    }
    Some(GlobalConfig {
        lp_fee_bps,
        protocol_fee_bps,
        coin_creator_fee_bps,
        creator_fee_configurable,
    })
}

/// SPL mint supply (`u64` at offset 36; identical for Token and Token-2022).
pub fn read_mint_supply(data: &[u8]) -> Option<u64> {
    if data.len() < 44 {
        return None;
    }
    Some(u64::from_le_bytes(data[36..44].try_into().unwrap()))
}

/// Read the appended `Pool::virtual_quote_reserves` (i128) and
/// `Pool::creator_fee_bps` (u64) from raw pool-account bytes.
/// Pools written before an appended field existed are shorter — missing fields
/// read as 0.
pub fn read_pool_extras(data: &[u8]) -> (i128, u64) {
    let v = if data.len() >= POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 16 {
        let low = u64::from_le_bytes(
            data[POOL_VIRTUAL_QUOTE_RESERVES_OFFSET..POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        let high = u64::from_le_bytes(
            data[POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 8..POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 16]
                .try_into()
                .unwrap(),
        );
        (high as i128) << 64 | (low as i128)
    } else {
        0
    };
    let fee = if data.len() >= POOL_CREATOR_FEE_BPS_OFFSET + 8 {
        u64::from_le_bytes(
            data[POOL_CREATOR_FEE_BPS_OFFSET..POOL_CREATOR_FEE_BPS_OFFSET + 8]
                .try_into()
                .unwrap(),
        )
    } else {
        0
    };
    (v, fee)
}

// ---------------------------------------------------------------------------
// Schedule selection
// ---------------------------------------------------------------------------

fn is_sol_like_quote_mint(quote_mint: &Pubkey) -> bool {
    *quote_mint == Pubkey::default()
        || *quote_mint == Pubkey::from_str_const(solroute_core::WSOL)
        || *quote_mint == Pubkey::from_str_const(WSOL_2022)
}

fn is_stable_quote_mint(quote_mint: &Pubkey) -> bool {
    *quote_mint == Pubkey::from_str_const(solroute_core::USDC)
}

/// `["pool-authority", base_mint]` PDA under the pump program — canonical pump
/// pools are the ones whose `Pool::creator` is this PDA.
pub fn pump_pool_authority(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"pool-authority", base_mint.as_ref()],
        &Pubkey::from_str_const(PUMPFUN_PROGRAM),
    )
    .0
}

pub fn is_pump_pool(base_mint: &Pubkey, pool_creator: &Pubkey) -> bool {
    pump_pool_authority(base_mint) == *pool_creator
}

/// `mcap = effective_quote_reserves * supply / base_reserve` (floor).
pub fn pool_market_cap(supply: u128, base_reserve: u64, effective_quote_reserve: u128) -> u128 {
    if base_reserve == 0 {
        return 0;
    }
    effective_quote_reserve.saturating_mul(supply) / base_reserve as u128
}

fn calculate_fee_tier(tiers: &[(u128, Fees)], market_cap: u128) -> Option<Fees> {
    let first = tiers.first()?;
    if market_cap < first.0 {
        return Some(first.1);
    }
    for (threshold, fees) in tiers.iter().rev() {
        if market_cap >= *threshold {
            return Some(*fees);
        }
    }
    Some(first.1)
}

/// Inputs the fee schedule depends on (mirrors the SDK's `computeFeesBps`).
pub struct FeeInputs<'a> {
    pub fee_config: Option<&'a FeeConfig>,
    pub global_config: Option<&'a GlobalConfig>,
    pub base_mint: &'a Pubkey,
    pub pool_creator: &'a Pubkey,
    pub quote_mint: &'a Pubkey,
    /// `Pool::creator_fee_bps` (appended; 0 = not configured).
    pub pool_creator_fee_bps: u64,
    /// Live mint supply (`None` = unknown → assume mayhem-style fixed supply).
    pub base_mint_supply: Option<u64>,
    pub base_reserve: u64,
    pub effective_quote_reserve: u128,
    pub is_mayhem_mode: bool,
    /// `Pool::coin_creator == Pubkey::default()` → no creator fee.
    pub coin_creator_default: bool,
}

/// Resolve the fee schedule for one quote. `None` = unknown (caller falls back
/// to [`FALLBACK_FEES`]).
pub fn compute_fees_bps(input: &FeeInputs<'_>) -> Option<Fees> {
    let mut fees = match (input.fee_config, input.global_config) {
        (Some(fc), Some(gc)) => {
            let supply = if input.is_mayhem_mode {
                MAYHEM_TOTAL_SUPPLY
            } else {
                input
                    .base_mint_supply
                    .map(|s| s as u128)
                    .unwrap_or(MAYHEM_TOTAL_SUPPLY)
            };
            let market_cap =
                pool_market_cap(supply, input.base_reserve, input.effective_quote_reserve);

            let mut f = if !is_pump_pool(input.base_mint, input.pool_creator) {
                fc.flat_fees
            } else if is_sol_like_quote_mint(input.quote_mint) {
                calculate_fee_tier(&fc.fee_tiers, market_cap)?
            } else if is_stable_quote_mint(input.quote_mint) {
                let tiers = if fc.stable_fee_tiers.is_empty() {
                    &fc.fee_tiers
                } else {
                    &fc.stable_fee_tiers
                };
                calculate_fee_tier(tiers, market_cap)?
            } else if !fc.exotic_flat_fees.is_zero() {
                fc.exotic_flat_fees
            } else {
                fc.flat_fees
            };

            if gc.creator_fee_configurable && input.pool_creator_fee_bps > 0 {
                f.creator_bps = input.pool_creator_fee_bps;
            }
            f
        }
        (None, Some(gc)) => Fees {
            lp_bps: gc.lp_fee_bps,
            protocol_bps: gc.protocol_fee_bps,
            creator_bps: gc.coin_creator_fee_bps,
        },
        _ => return None,
    };
    if input.coin_creator_default {
        fees = fees.without_creator();
    }
    Some(fees)
}

// ---------------------------------------------------------------------------
// Exact quote math
// ---------------------------------------------------------------------------

#[inline]
fn ceil_div(a: u128, b: u128) -> u128 {
    if b == 0 {
        return 0;
    }
    (a + b - 1) / b
}

/// Tokens out for an exact quote input (`buy_exact_quote_in`).
pub fn buy_exact_quote_in(
    quote_in: u64,
    fees: Fees,
    base_reserve: u64,
    effective_quote_reserve: u128,
) -> u64 {
    if quote_in == 0 || base_reserve == 0 {
        return 0;
    }
    let total = fees.total_bps() as u128;
    let mut effective_quote = (quote_in as u128) * 10_000 / (10_000 + total);
    let lp = ceil_div(effective_quote * fees.lp_bps as u128, 10_000);
    let protocol = ceil_div(effective_quote * fees.protocol_bps as u128, 10_000);
    let creator = ceil_div(effective_quote * fees.creator_bps as u128, 10_000);
    let total_with_fees = effective_quote + lp + protocol + creator;
    if total_with_fees > quote_in as u128 {
        effective_quote = effective_quote.saturating_sub(total_with_fees - quote_in as u128);
    }
    // The program quotes with `effective_quote - 1`.
    let input = effective_quote.saturating_sub(1);
    if input == 0 {
        return 0;
    }
    let out = (base_reserve as u128) * input / (effective_quote_reserve + input);
    out.min(u64::MAX as u128) as u64
}

/// Quote out for an exact base input (`sell`).
pub fn sell_base_in(
    base_in: u64,
    fees: Fees,
    base_reserve: u64,
    effective_quote_reserve: u128,
) -> u64 {
    if base_in == 0 || base_reserve == 0 {
        return 0;
    }
    let raw = effective_quote_reserve * (base_in as u128) / (base_reserve as u128 + base_in as u128);
    let lp = ceil_div(raw * fees.lp_bps as u128, 10_000);
    let protocol = ceil_div(raw * fees.protocol_bps as u128, 10_000);
    let creator = ceil_div(raw * fees.creator_bps as u128, 10_000);
    let out = raw.saturating_sub(lp + protocol + creator);
    out.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fees(lp: u64, protocol: u64, creator: u64) -> Fees {
        Fees {
            lp_bps: lp,
            protocol_bps: protocol,
            creator_bps: creator,
        }
    }

    /// Live mainnet fixture (FAMILY pool, slot 449173140): `buy_exact_quote_in`
    /// of 100_000 lamports against vault 239_276_192_495 + virtual
    /// 17_584_505_550, tier fees (20, 5, 85) → 35_409_575 tokens and fee slices
    /// 198 / 50 / 841.
    #[test]
    fn buy_matches_live_family_event() {
        let base = 91_955_633_894_610u64;
        let quote = 239_276_192_495u128;
        let v = 17_584_505_550u128;
        let f = fees(20, 5, 85);
        let out = buy_exact_quote_in(100_000, f, base, quote + v);
        assert_eq!(out, 35_409_575);

        // Fee slices, exactly as the on-chain BuyEvent.
        let eff = (100_000u128 * 10_000 / (10_000 + f.total_bps() as u128)) as u128;
        assert_eq!(ceil_div(eff * 20, 10_000), 198);
        assert_eq!(ceil_div(eff * 5, 10_000), 50);
        assert_eq!(ceil_div(eff * 85, 10_000), 841);
    }

    /// Second live fixture (slot 449709581, tier creator=95): qin 6_112_481 →
    /// 8_152_022_216 tokens, fees 12_080 / 3_020 / 57_380.
    #[test]
    fn buy_matches_second_live_event() {
        let out = buy_exact_quote_in(
            6_112_481,
            fees(20, 5, 95),
            182_918_903_872_498,
            117_937_789_279 + 17_584_505_550,
        );
        assert_eq!(out, 8_152_022_216);
    }

    /// Live sell fixture (slot 449710676): 30_509_327_828 base in →
    /// raw 22_520_269, fees 45_041 / 11_261 / 213_943, user 22_250_024.
    #[test]
    fn sell_matches_live_event() {
        let out = sell_base_in(
            30_509_327_828,
            fees(20, 5, 95),
            183_243_664_423_373,
            117_698_176_817 + 17_584_505_550,
        );
        assert_eq!(out, 22_250_024);
    }

    #[test]
    fn tier_selection_uses_first_tier_below_threshold_and_last_above() {
        let tiers = vec![
            (0u128, fees(2, 93, 30)),
            (420_000_000_000, fees(20, 5, 95)),
            (2_460_000_000_000, fees(20, 5, 85)),
        ];
        assert_eq!(calculate_fee_tier(&tiers, 1), Some(fees(2, 93, 30)));
        assert_eq!(
            calculate_fee_tier(&tiers, 419_999_999_999),
            Some(fees(2, 93, 30))
        );
        assert_eq!(
            calculate_fee_tier(&tiers, 420_000_000_000),
            Some(fees(20, 5, 95))
        );
        assert_eq!(
            calculate_fee_tier(&tiers, 2_500_000_000_000),
            Some(fees(20, 5, 85))
        );
        assert_eq!(
            calculate_fee_tier(&tiers, u128::MAX),
            Some(fees(20, 5, 85))
        );
    }

    #[test]
    fn coin_creator_default_drops_creator_slice() {
        let fc = FeeConfig {
            flat_fees: fees(25, 5, 0),
            fee_tiers: vec![(0, fees(20, 5, 85))],
            ..Default::default()
        };
        let gc = GlobalConfig::default();
        let base_mint = Pubkey::new_unique();
        let pool_creator = pump_pool_authority(&base_mint);
        let quote_mint = Pubkey::from_str_const(solroute_core::WSOL);
        let input = FeeInputs {
            fee_config: Some(&fc),
            global_config: Some(&gc),
            base_mint: &base_mint,
            pool_creator: &pool_creator,
            quote_mint: &quote_mint,
            pool_creator_fee_bps: 0,
            base_mint_supply: Some(1_000_000_000_000_000),
            base_reserve: 1_000_000,
            effective_quote_reserve: 1_000_000_000,
            is_mayhem_mode: false,
            coin_creator_default: true,
        };
        assert_eq!(compute_fees_bps(&input), Some(fees(20, 5, 0)));
    }

    #[test]
    fn read_pool_extras_reads_i128_and_creator_fee() {
        let mut data = vec![0u8; POOL_CREATOR_FEE_BPS_OFFSET + 8];
        data[POOL_VIRTUAL_QUOTE_RESERVES_OFFSET..POOL_VIRTUAL_QUOTE_RESERVES_OFFSET + 8]
            .copy_from_slice(&17_584_505_550u64.to_le_bytes());
        data[POOL_CREATOR_FEE_BPS_OFFSET..POOL_CREATOR_FEE_BPS_OFFSET + 8]
            .copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(read_pool_extras(&data), (17_584_505_550, 42));
        // Legacy short account: both read as zero.
        assert_eq!(read_pool_extras(&data[..245]), (0, 0));
    }
}
