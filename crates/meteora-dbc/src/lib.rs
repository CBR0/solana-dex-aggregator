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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_guards() {
        assert!(parse_virtual_pool(&[0u8; 400]).is_none());
        assert!(parse_pool_config(&[0u8; 1000]).is_none());
    }
}
