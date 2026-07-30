//! Meteora Dynamic Bonding Curve (DBC) — pre-graduation, concentrated-liquidity
//! sqrt-price curve. Program `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`.
//!
//! Account layouts ported from the on-chain IDL. The pool (`VirtualPool`) holds
//! the live `sqrt_price` + reserves; the `PoolConfig` holds the 20-segment
//! liquidity curve and the fee config. Quoting traverses the curve segments
//! (CLMM-style Δbase/Δquote over each `{sqrt_price, liquidity}` step). Swap math
//! lives in `quote.rs`.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use solroute_core::{
    GenericError, Market, PoolFees, PoolFinancials, PoolMetadata, SwapDirection,
    calculate_price_impact_bps,
};

pub mod quote;

pub const METEORA_DBC_PROGRAM: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";

/// `VirtualPool` account discriminator.
pub const DISC_VIRTUAL_POOL: [u8; 8] = [213, 224, 5, 209, 98, 69, 119, 92];
/// `PoolConfig` account discriminator.
pub const DISC_POOL_CONFIG: [u8; 8] = [26, 108, 14, 123, 116, 230, 129, 43];

/// Number of curve segments in a `PoolConfig`.
pub const MAX_CURVE_POINTS: usize = 20;

// ---------------------------------------------------------------------------
// Nested types (borsh field order matches the on-chain layout exactly)
// ---------------------------------------------------------------------------

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VolatilityTracker {
    pub last_update_timestamp: u64,
    pub padding: [u8; 8],
    pub sqrt_price_reference: u128,
    pub volatility_accumulator: u128,
    pub volatility_reference: u128,
}

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PoolMetrics {
    pub total_protocol_base_fee: u64,
    pub total_protocol_quote_fee: u64,
    pub total_trading_base_fee: u64,
    pub total_trading_quote_fee: u64,
}

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct BaseFeeConfig {
    pub cliff_fee_numerator: u64,
    pub second_factor: u64,
    pub third_factor: u64,
    pub first_factor: u16,
    pub base_fee_mode: u8,
    pub padding_0: [u8; 5],
}

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DynamicFeeConfig {
    pub initialized: u8,
    pub padding: [u8; 7],
    pub max_volatility_accumulator: u32,
    pub variable_fee_control: u32,
    pub bin_step: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub padding2: [u8; 8],
    pub bin_step_u128: u128,
}

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PoolFeesConfig {
    pub base_fee: BaseFeeConfig,
    pub dynamic_fee: DynamicFeeConfig,
    pub padding_0: [u64; 5],
    pub padding_1: [u8; 6],
    pub protocol_fee_percent: u8,
    pub referral_fee_percent: u8,
}

#[derive(BorshDeserialize, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LockedVestingConfig {
    pub amount_per_period: u64,
    pub cliff_duration_from_migration_time: u64,
    pub frequency: u64,
    pub number_of_period: u64,
    pub cliff_unlock_amount: u64,
    pub _padding: u64,
}

/// One curve segment: an upper `sqrt_price` bound and the `liquidity` active up
/// to it.
#[derive(BorshDeserialize, Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LiquidityDistributionConfig {
    pub sqrt_price: u128,
    pub liquidity: u128,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(BorshDeserialize, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VirtualPool {
    pub volatility_tracker: VolatilityTracker,
    pub config: Pubkey,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub protocol_base_fee: u64,
    pub protocol_quote_fee: u64,
    pub partner_base_fee: u64,
    pub partner_quote_fee: u64,
    pub sqrt_price: u128,
    pub activation_point: u64,
    pub pool_type: u8,
    pub is_migrated: u8,
    pub is_partner_withdraw_surplus: u8,
    pub is_protocol_withdraw_surplus: u8,
    pub migration_progress: u8,
    pub is_withdraw_leftover: u8,
    pub is_creator_withdraw_surplus: u8,
    pub migration_fee_withdraw_status: u8,
    pub metrics: PoolMetrics,
    pub finish_curve_timestamp: u64,
    pub creator_base_fee: u64,
    pub creator_quote_fee: u64,
    pub _padding_1: [u64; 7],
}

#[derive(BorshDeserialize, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PoolConfig {
    pub quote_mint: Pubkey,
    pub fee_claimer: Pubkey,
    pub leftover_receiver: Pubkey,
    pub pool_fees: PoolFeesConfig,
    pub collect_fee_mode: u8,
    pub migration_option: u8,
    pub activation_type: u8,
    pub token_decimal: u8,
    pub version: u8,
    pub token_type: u8,
    pub quote_token_flag: u8,
    pub partner_locked_lp_percentage: u8,
    pub partner_lp_percentage: u8,
    pub creator_locked_lp_percentage: u8,
    pub creator_lp_percentage: u8,
    pub migration_fee_option: u8,
    pub fixed_token_supply_flag: u8,
    pub creator_trading_fee_percentage: u8,
    pub token_update_authority: u8,
    pub migration_fee_percentage: u8,
    pub creator_migration_fee_percentage: u8,
    pub _padding_0: [u8; 7],
    pub swap_base_amount: u64,
    pub migration_quote_threshold: u64,
    pub migration_base_threshold: u64,
    pub migration_sqrt_price: u128,
    pub locked_vesting_config: LockedVestingConfig,
    pub pre_migration_token_supply: u64,
    pub post_migration_token_supply: u64,
    pub migrated_collect_fee_mode: u8,
    pub migrated_dynamic_fee: u8,
    pub migrated_pool_fee_bps: u16,
    pub _padding_1: [u8; 12],
    pub _padding_2: u128,
    pub sqrt_start_price: u128,
    pub curve: [LiquidityDistributionConfig; MAX_CURVE_POINTS],
}

/// Parse a `VirtualPool` account (data includes the 8-byte discriminator).
pub fn parse_virtual_pool(data: &[u8]) -> Option<VirtualPool> {
    if data.len() < 8 || data[..8] != DISC_VIRTUAL_POOL {
        return None;
    }
    VirtualPool::deserialize(&mut &data[8..]).ok()
}

/// Parse a `PoolConfig` account (data includes the 8-byte discriminator).
pub fn parse_pool_config(data: &[u8]) -> Option<PoolConfig> {
    if data.len() < 8 || data[..8] != DISC_POOL_CONFIG {
        return None;
    }
    PoolConfig::deserialize(&mut &data[8..]).ok()
}

/// A DBC pool bundled with its config (quoting needs both). `config` is static
/// per pool; `pool` carries the live sqrt_price/reserves.
pub struct DbcMarket {
    pub pool: VirtualPool,
    pub config: PoolConfig,
    pub pool_address: String,
    /// Slot or unix time (per `config.activation_type`) used by the fee
    /// scheduler; 0 = unknown → the cliff (max) fee is used.
    pub current_point: u64,
}

impl DbcMarket {
    pub fn new(pool: VirtualPool, config: PoolConfig, pool_address: String) -> Self {
        Self { pool, config, pool_address, current_point: 0 }
    }

    pub fn with_current_point(mut self, current_point: u64) -> Self {
        self.current_point = current_point;
        self
    }

    fn quote_decimals(&self) -> u8 {
        // WSOL = 9; otherwise fall back to the config's token decimal is base-side,
        // so default quote to 9 (SOL) — USDC-quoted DBC pools are rare here.
        9
    }
}

impl Market for DbcMarket {
    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Meteora DBC".to_string(),
            quote_mint: self.config.quote_mint,
            base_mint: self.pool.base_mint,
            quote_vault: self.pool.quote_vault,
            base_vault: self.pool.base_vault,
            fees: PoolFees {
                // numerator/1e9 → bps = numerator/1e5.
                trade_fee_bps: quote::fee_numerator(&self.pool, &self.config, self.current_point) / 100_000,
                protocol_fee_bps: None,
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        Ok(PoolFinancials {
            quote_balance: self.pool.quote_reserve,
            base_balance: self.pool.base_reserve,
            quote_decimals: self.quote_decimals(),
            base_decimals: self.config.token_decimal,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // Buy = quote(WSOL) -> base(token); Sell = base -> quote.
        let buy = matches!(direction, SwapDirection::Buy);
        quote::quote_exact_in(&self.pool, &self.config, amount_in, buy, self.current_point)
            .ok_or_else(|| GenericError::from("dbc quote failed (not tradeable / overflow / zero)"))
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        // Re-parse live VirtualPool (sqrt_price/reserves change per trade); the
        // config is static so keep the baked copy.
        if let Some(data) = pool_data {
            if let Some(live) = parse_virtual_pool(data) {
                let m = DbcMarket::new(live, self.config.clone(), self.pool_address.clone())
                    .with_current_point(self.current_point);
                return m.calculate_output(amount_in, direction);
            }
        }
        self.calculate_output(amount_in, direction)
    }

    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        let pre = self.current_price()?;
        let out = self.calculate_output(amount_in, direction)? as f64;
        // Approximate post price from executed rate.
        let post = match direction {
            SwapDirection::Buy => {
                let base_dec = 10f64.powi(self.config.token_decimal as i32);
                let quote_dec = 10f64.powi(self.quote_decimals() as i32);
                (amount_in as f64 / quote_dec) / (out / base_dec)
            }
            SwapDirection::Sell => {
                let base_dec = 10f64.powi(self.config.token_decimal as i32);
                let quote_dec = 10f64.powi(self.quote_decimals() as i32);
                (out / quote_dec) / (amount_in as f64 / base_dec)
            }
        };
        Ok(calculate_price_impact_bps(pre, post))
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        // price(quote per base, raw) = (sqrt_price / 2^64)^2 ; decimal-adjust to UI.
        let sp = self.pool.sqrt_price as f64 / 2f64.powi(64);
        let raw = sp * sp;
        Ok(raw * 10f64.powi(self.config.token_decimal as i32 - self.quote_decimals() as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_guards() {
        assert!(parse_virtual_pool(&[0u8; 400]).is_none());
        assert!(parse_pool_config(&[0u8; 1000]).is_none());
    }
}
