
use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use solroute_core::{
    GenericError, Market, PoolFees, PoolFinancials, PoolMetadata,
    SwapDirection, WSOL, calculate_price_impact_bps, infer_mint_decimals,
};


// ---------------------------------------------------------------------------
// DEX-specific constants
// ---------------------------------------------------------------------------

pub const PUMPFUN_AMM_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
pub const PUMPFUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const PUMPFUN_FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PumpfunBondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PumpfunAmmPool {
    pub pool_bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// Bonding curve data — fetched separately, not part of pool account borsh layout
    #[borsh(skip)]
    #[serde(default)]
    pub bonding_curve: Option<PumpfunBondingCurve>,
}


// ---------------------------------------------------------------------------
// Market wrapper
// ---------------------------------------------------------------------------

pub struct PumpfunAmmMarket {
    pub pool: PumpfunAmmPool,
    pub pool_address: String,
    pub base_decimals: u8,
}

impl PumpfunAmmMarket {
    pub fn new(pool: PumpfunAmmPool, pool_address: String) -> Self {
        let base_decimals = infer_mint_decimals(&pool.base_mint);
        Self { pool, pool_address, base_decimals }
    }

    /// Calculate bonding curve output using virtual reserves and constant-product formula.
    fn calculate_bonding_curve_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let bonding_curve = self
            .pool
            .bonding_curve
            .as_ref()
            .ok_or("Bonding curve data not available")?;

        // Pumpfun uses 1% fee (100 bps)
        let fee_bps = 100u64;
        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        let virtual_sol = bonding_curve.virtual_sol_reserves as u128;
        let virtual_token = bonding_curve.virtual_token_reserves as u128;

        if virtual_sol == 0 || virtual_token == 0 {
            return Err("Bonding curve has zero virtual reserves".into());
        }

        let output = match direction {
            SwapDirection::Buy => {
                // SOL -> Token: constant product k = virtual_sol * virtual_token
                let k = virtual_sol * virtual_token;
                let new_sol = virtual_sol + amount_in_with_fee;
                let new_token = k / new_sol;
                virtual_token.saturating_sub(new_token) as u64
            }
            SwapDirection::Sell => {
                // Token -> SOL: constant product
                let k = virtual_sol * virtual_token;
                let new_token = virtual_token + amount_in_with_fee;
                let new_sol = k / new_token;
                virtual_sol.saturating_sub(new_sol) as u64
            }
        };

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Market trait
// ---------------------------------------------------------------------------

impl Market for PumpfunAmmMarket {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Pumpfun AMM".to_string(),
            quote_mint: self.pool.quote_mint,
            base_mint: self.pool.base_mint,
            quote_vault: self.pool.pool_quote_token_account,
            base_vault: self.pool.pool_base_token_account,
            fees: PoolFees {
                trade_fee_bps: 100,
                protocol_fee_bps: None,
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        let bonding_curve = self
            .pool
            .bonding_curve
            .as_ref()
            .ok_or("Bonding curve data not available")?;

        Ok(PoolFinancials {
            quote_balance: bonding_curve.real_sol_reserves,
            base_balance: bonding_curve.real_token_reserves,
            quote_decimals: 9,
            base_decimals: self.base_decimals,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        self.calculate_bonding_curve_output(amount_in, direction)
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        _pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Pumpfun swap data lives in the bonding curve account, not the pool account.
        // Live bonding curve updates would require a separate account mapping.
        self.calculate_bonding_curve_output(amount_in, direction)
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre_swap_price = self.current_price()?;
        let output = self.calculate_output(amount_in, direction)?;

        let bonding_curve = self
            .pool
            .bonding_curve
            .as_ref()
            .ok_or("Bonding curve data not available")?;

        let post_swap_price = match direction {
            SwapDirection::Buy => {
                let new_sol = bonding_curve.virtual_sol_reserves + amount_in;
                let new_token = bonding_curve.virtual_token_reserves.saturating_sub(output);
                if new_token == 0 {
                    return Err("Insufficient liquidity in bonding curve".into());
                }
                new_sol as f64 / new_token as f64
            }
            SwapDirection::Sell => {
                let new_token = bonding_curve.virtual_token_reserves + amount_in;
                let new_sol = bonding_curve.virtual_sol_reserves.saturating_sub(output);
                if new_token == 0 {
                    return Err("Insufficient liquidity in bonding curve".into());
                }
                new_sol as f64 / new_token as f64
            }
        };

        Ok(calculate_price_impact_bps(pre_swap_price, post_swap_price))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let bonding_curve = self
            .pool
            .bonding_curve
            .as_ref()
            .ok_or("Bonding curve data not available")?;

        if bonding_curve.virtual_token_reserves == 0 {
            return Err("Bonding curve has zero virtual token reserves".into());
        }

        // Raw price in lamports: SOL_lamports / token_raw_units
        let raw = bonding_curve.virtual_sol_reserves as f64 / bonding_curve.virtual_token_reserves as f64;
        // Adjust: (sol / 10^9) / (tokens / 10^base_dec) = raw * 10^base_dec / 10^9
        let decimal_adj = 10f64.powi(self.base_decimals as i32 - 9);
        Ok(raw * decimal_adj)
    }

}

// ===========================================================================
// Pump.fun bonding curve (pre-graduation) — a distinct venue from PumpSwap AMM.
//
// Tokens on the pump.fun bonding-curve program `6EF8…F6P` trade against virtual
// SOL/token reserves via constant product with a protocol + creator fee. When a
// curve's `complete` flag flips it has migrated to the PumpSwap AMM (`pAMMBay…`)
// and must NOT be quoted here (the AMM venue owns it).
//
// Math + fees ported verbatim from sol-trade-sdk `utils/calc/pumpfun.rs`
// (`FEE_BASIS_POINTS = 95`, `CREATOR_FEE = 30`): buy is fee-inclusive on the SOL
// input, sell takes the fee on the SOL output.
// ===========================================================================

/// BondingCurve account Anchor discriminator (`sha256("account:BondingCurve")[..8]`).
pub const DISC_BONDING_CURVE: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// Protocol fee (basis points) on every bonding-curve trade.
pub const PUMPFUN_BC_FEE_BASIS_POINTS: u64 = 95;
/// Additional creator fee (basis points) when the curve has a creator.
pub const PUMPFUN_BC_CREATOR_FEE: u64 = 30;

/// pump.fun mints are always 6 decimals.
pub const PUMPFUN_TOKEN_DECIMALS: u8 = 6;

#[inline]
fn bc_total_fee_bps(creator: &Pubkey) -> u64 {
    PUMPFUN_BC_FEE_BASIS_POINTS
        + if *creator != Pubkey::default() { PUMPFUN_BC_CREATOR_FEE } else { 0 }
}

/// Ceil-div fee, matching sol-trade-sdk `compute_fee` exactly (no precision loss
/// on the sub-10_000 remainder).
#[inline]
fn bc_compute_fee(amount: u128, fee_basis_points: u128) -> u128 {
    let whole = (amount / 10_000).saturating_mul(fee_basis_points);
    let remainder_product = (amount % 10_000).saturating_mul(fee_basis_points);
    whole.saturating_add(remainder_product.div_ceil(10_000))
}

/// Tokens received for spending `sol_in` lamports (fee-inclusive), capped at the
/// curve's real token reserves.
pub fn bc_buy_tokens_out(
    virtual_token_reserves: u128,
    virtual_sol_reserves: u128,
    real_token_reserves: u128,
    creator: &Pubkey,
    sol_in: u64,
) -> u64 {
    if sol_in == 0 || virtual_token_reserves == 0 {
        return 0;
    }
    let total = bc_total_fee_bps(creator) as u128;
    // Strip the fee from the SOL input, then apply constant product.
    let input = (sol_in as u128) * 10_000 / (total + 10_000);
    let denominator = virtual_sol_reserves + input;
    if denominator == 0 {
        return 0;
    }
    let out = (input * virtual_token_reserves / denominator).min(real_token_reserves);
    out.min(u64::MAX as u128) as u64
}

/// SOL received (after fee) for selling `token_in` base units.
pub fn bc_sell_sol_out(
    virtual_token_reserves: u128,
    virtual_sol_reserves: u128,
    creator: &Pubkey,
    token_in: u64,
) -> u64 {
    if token_in == 0 || virtual_token_reserves == 0 {
        return 0;
    }
    let numerator = (token_in as u128) * virtual_sol_reserves;
    let denominator = virtual_token_reserves + token_in as u128;
    let sol_cost = numerator / denominator;
    let fee = bc_compute_fee(sol_cost, bc_total_fee_bps(creator) as u128);
    sol_cost.saturating_sub(fee).min(u64::MAX as u128) as u64
}

/// Parse a `BondingCurve` account's raw data (including the 8-byte discriminator).
/// Uses borsh `deserialize` (not `try_from_slice`) so newer trailing fields
/// (`is_mayhem_mode`, `is_cashback_coin`, `quote_mint`) on extended accounts are
/// ignored — only the stable prefix through `creator` is read. Returns None if
/// the discriminator doesn't match or the buffer is too short.
pub fn parse_bonding_curve(data: &[u8]) -> Option<PumpfunBondingCurve> {
    if data.len() < 8 || data[..8] != DISC_BONDING_CURVE {
        return None;
    }
    PumpfunBondingCurve::deserialize(&mut &data[8..]).ok()
}

/// A pump.fun bonding curve as a routable "pool": the parsed curve state plus the
/// mint it prices. Always quoted against WSOL (SOL-paired curves only).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PumpfunBondingCurvePool {
    pub mint: Pubkey,
    /// The bonding-curve account address (PDA `["bonding-curve", mint]`).
    pub bonding_curve: Pubkey,
    pub curve: PumpfunBondingCurve,
}

pub struct PumpfunBondingCurveMarket {
    pub pool: PumpfunBondingCurvePool,
    pub pool_address: String,
}

impl PumpfunBondingCurveMarket {
    pub fn new(pool: PumpfunBondingCurvePool, pool_address: String) -> Self {
        Self { pool, pool_address }
    }

    fn wsol() -> Pubkey {
        Pubkey::from_str_const(WSOL)
    }
}

impl Market for PumpfunBondingCurveMarket {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        let total_fee = bc_total_fee_bps(&self.pool.curve.creator);
        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Pumpfun BC".to_string(),
            quote_mint: Self::wsol(),
            base_mint: self.pool.mint,
            // Reserves live in the curve account itself; watch it for freshness.
            quote_vault: self.pool.bonding_curve,
            base_vault: self.pool.bonding_curve,
            fees: PoolFees { trade_fee_bps: total_fee, protocol_fee_bps: None },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: self.pool.curve.real_sol_reserves,
            base_balance: self.pool.curve.real_token_reserves,
            quote_decimals: 9,
            base_decimals: PUMPFUN_TOKEN_DECIMALS,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let c = &self.pool.curve;
        if c.complete {
            return Err("bonding curve complete (migrated to PumpSwap AMM)".into());
        }
        let out = match direction {
            // Buy = spend SOL (quote), receive token (base).
            SwapDirection::Buy => bc_buy_tokens_out(
                c.virtual_token_reserves as u128,
                c.virtual_sol_reserves as u128,
                c.real_token_reserves as u128,
                &c.creator,
                amount_in,
            ),
            // Sell = spend token (base), receive SOL (quote).
            SwapDirection::Sell => bc_sell_sol_out(
                c.virtual_token_reserves as u128,
                c.virtual_sol_reserves as u128,
                &c.creator,
                amount_in,
            ),
        };
        if out == 0 {
            return Err("bonding curve produced zero output".into());
        }
        Ok(out)
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        _pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Curve state is self-contained in the parsed account; no vault reads.
        self.calculate_output(amount_in, direction)
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre = self.current_price()?;
        let out = self.calculate_output(amount_in, direction)?;
        let c = &self.pool.curve;
        let (new_sol, new_tok) = match direction {
            SwapDirection::Buy => (
                c.virtual_sol_reserves as u128 + amount_in as u128,
                (c.virtual_token_reserves as u128).saturating_sub(out as u128),
            ),
            SwapDirection::Sell => (
                (c.virtual_sol_reserves as u128).saturating_sub(out as u128),
                c.virtual_token_reserves as u128 + amount_in as u128,
            ),
        };
        if new_tok == 0 {
            return Err("bonding curve exhausted".into());
        }
        let post = (new_sol as f64 / new_tok as f64) * 10f64.powi(PUMPFUN_TOKEN_DECIMALS as i32 - 9);
        Ok(calculate_price_impact_bps(pre, post))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let c = &self.pool.curve;
        if c.virtual_token_reserves == 0 {
            return Err("bonding curve has zero virtual token reserves".into());
        }
        let raw = c.virtual_sol_reserves as f64 / c.virtual_token_reserves as f64;
        Ok(raw * 10f64.powi(PUMPFUN_TOKEN_DECIMALS as i32 - 9))
    }
}

#[cfg(test)]
mod bc_tests {
    use super::*;

    // A fresh curve (pump.fun genesis constants) with a creator → 125 bps fee.
    fn fresh_curve(creator: Pubkey) -> PumpfunBondingCurve {
        PumpfunBondingCurve {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_sol_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator,
        }
    }

    #[test]
    fn buy_matches_sdk_fee_inclusive_cp() {
        let creator = Pubkey::new_from_array([7u8; 32]);
        let out = bc_buy_tokens_out(
            1_073_000_000_000_000,
            30_000_000_000,
            793_100_000_000_000,
            &creator,
            1_000_000_000,
        );
        let input = 1_000_000_000u128 * 10_000 / (125 + 10_000);
        let expected = (input * 1_073_000_000_000_000u128 / (30_000_000_000u128 + input)) as u64;
        assert_eq!(out, expected);
        assert!(out > 0 && (out as u128) < 793_100_000_000_000);
    }

    #[test]
    fn no_creator_uses_95_bps_only() {
        let out_creator = bc_buy_tokens_out(
            1_073_000_000_000_000, 30_000_000_000, 793_100_000_000_000,
            &Pubkey::new_from_array([9u8; 32]), 1_000_000_000);
        let out_none = bc_buy_tokens_out(
            1_073_000_000_000_000, 30_000_000_000, 793_100_000_000_000,
            &Pubkey::default(), 1_000_000_000);
        // Lower fee (95 vs 125) → more tokens out.
        assert!(out_none > out_creator);
    }

    #[test]
    fn sell_takes_fee_on_sol_out() {
        let creator = Pubkey::new_from_array([7u8; 32]);
        let gross = 1_000_000_000_000u128 * 30_000_000_000u128
            / (1_073_000_000_000_000u128 + 1_000_000_000_000u128);
        let fee = bc_compute_fee(gross, 125);
        let expected = (gross - fee) as u64;
        let out = bc_sell_sol_out(1_073_000_000_000_000, 30_000_000_000, &creator, 1_000_000_000_000);
        assert_eq!(out, expected);
    }

    #[test]
    fn complete_curve_rejected() {
        let mut c = fresh_curve(Pubkey::new_from_array([7u8; 32]));
        c.complete = true;
        let m = PumpfunBondingCurveMarket::new(
            PumpfunBondingCurvePool { mint: Pubkey::default(), bonding_curve: Pubkey::default(), curve: c },
            "x".into(),
        );
        assert!(m.calculate_output(1_000_000_000, SwapDirection::Buy).is_err());
    }

    #[test]
    fn parse_rejects_bad_discriminator() {
        let mut data = vec![0u8; 120];
        data[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(parse_bonding_curve(&data).is_none());
    }
}
