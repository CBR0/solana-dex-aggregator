
use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use thunder_core::{
    GenericError, Market, PoolFees, PoolFinancials, PoolMetadata,
    SwapDirection, calculate_price_impact_bps, infer_mint_decimals,
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
