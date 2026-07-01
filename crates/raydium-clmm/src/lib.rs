//! Raydium CLMM (Concentrated Liquidity Market Maker) DEX implementation.
//!
//! Tick-based pricing with concentrated liquidity positions.

pub mod tick_arrays;


use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use thunder_core::{
    GenericError, Market, PoolFees, PoolFinancials, PoolMetadata, SwapDirection,
    quote_priority,
};


// ============================================================================
// Constants
// ============================================================================

pub const RAYDIUM_CLMM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

// ============================================================================
// Models
// ============================================================================

#[derive(
    Debug, BorshDeserialize, serde::Deserialize, serde::Serialize, PartialEq, Eq, Clone, Hash,
)]
pub struct RewardInfo {
    /// Reward state
    pub reward_state: u8,
    /// Reward open time
    pub open_time: u64,
    /// Reward end time
    pub end_time: u64,
    /// Reward last update time
    pub last_update_time: u64,
    /// Q64.64 number indicates how many tokens per second are earned per unit of liquidity.
    pub emissions_per_second_x64: u128,
    /// The total amount of reward emissioned
    pub reward_total_emissioned: u64,
    /// The total amount of claimed reward
    pub reward_claimed: u64,
    /// Reward token mint.
    pub token_mint: Pubkey,
    /// Reward vault token account.
    pub token_vault: Pubkey,
    /// The owner that has permission to set reward param
    pub authority: Pubkey,
    /// Q64.64 number that tracks the total tokens earned per unit of liquidity since the reward
    /// emissions were turned on.
    pub reward_growth_global_x64: u128,
}

#[derive(
    Debug, BorshDeserialize, serde::Deserialize, serde::Serialize, PartialEq, Eq, Clone, Hash,
)]
pub struct RaydiumCLMMPool {
    pub bump: [u8; 1],
    pub amm_config: Pubkey,
    pub owner: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub observation_key: Pubkey,
    pub mint_decimals_0: u8,
    pub mint_decimals_1: u8,
    pub tick_spacing: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    pub padding3: u16,
    pub padding4: u16,
    pub fee_growth_global_0_x64: u128,
    pub fee_growth_global_1_x64: u128,
    pub protocol_fees_token_0: u64,
    pub protocol_fees_token_1: u64,
    pub swap_in_amount_token_0: u128,
    pub swap_out_amount_token_1: u128,
    pub swap_in_amount_token_1: u128,
    pub swap_out_amount_token_0: u128,
    pub status: u8,
    pub padding: [u8; 7],
    pub reward_infos: [RewardInfo; 3],
    pub tick_array_bitmap: [u64; 16],
    pub total_fees_token_0: u64,
    pub total_fees_claimed_token_0: u64,
    pub total_fees_token_1: u64,
    pub total_fees_claimed_token_1: u64,
    pub fund_fees_token_0: u64,
    pub fund_fees_token_1: u64,
    pub open_time: u64,
    pub recent_epoch: u64,
    pub padding1: [u64; 24],
    pub padding2: [u64; 32],
}


// ============================================================================
// Market Struct
// ============================================================================

/// Wrapper for RaydiumCLMMPool that implements the Market trait.
pub struct RaydiumClmmMarket {
    pub pool: RaydiumCLMMPool,
    pub pool_address: String,
    /// Current vault balances (cached from last fetch)
    pub vault_0_balance: u64,
    pub vault_1_balance: u64,
    /// If true, token_mint_1 is the quote currency and assignments are swapped.
    pub flipped: bool,
}

impl RaydiumClmmMarket {
    pub fn new(pool: RaydiumCLMMPool, pool_address: String) -> Self {
        let flipped = quote_priority(&pool.token_mint_1).unwrap_or(usize::MAX) < quote_priority(&pool.token_mint_0).unwrap_or(usize::MAX);
        Self {
            pool,
            pool_address,
            vault_0_balance: 0,
            vault_1_balance: 0,
            flipped,
        }
    }

    /// Convert sqrt_price_x64 to a price in quote-per-base terms.
    ///
    /// CLMM sqrt_price_x64 encodes sqrt(token_1_per_token_0) in Q64.64.
    /// raw = (sqrt_price / 2^64)^2 = token_1 per token_0 in raw units.
    /// Decimal adjustment: raw * 10^(dec_1 - dec_0) gives human-readable token_1 per token_0.
    ///
    /// When flipped (token_1 = quote): human_price is quote/base, return as-is.
    /// When NOT flipped (token_0 = quote): human_price is base/quote, return 1/human_price.
    fn sqrt_price_to_price(&self) -> f64 {
        let sqrt_price_f64 = self.pool.sqrt_price_x64 as f64 / (1u128 << 64) as f64;
        let raw = sqrt_price_f64 * sqrt_price_f64;
        // raw = token_1_lamports per token_0_lamports
        // human = raw * 10^(dec_0 - dec_1)
        let decimal_adj = 10f64.powi(self.pool.mint_decimals_0 as i32 - self.pool.mint_decimals_1 as i32);
        let human_price = raw * decimal_adj;
        // human_price = token_1 per token_0 (human-readable)
        // NOT flipped (token_0=quote): want quote_per_base = 1/human_price
        // Flipped (token_1=quote): want quote_per_base = human_price
        if self.flipped { human_price } else { 1.0 / human_price }
    }

    /// Raw price: token_1_raw per token_0_raw (no decimal adjustment, no flip).
    fn raw_price_token1_per_token0(&self) -> f64 {
        let sqrt = self.pool.sqrt_price_x64 as f64 / (1u128 << 64) as f64;
        sqrt * sqrt
    }

    /// Calculate output for CLMM swap.
    ///
    /// Note: This is a simplified calculation. Real CLMM swaps traverse multiple ticks.
    /// For production, use the actual CLMM math library or fetch from RPC simulation.
    fn calculate_clmm_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // When flipped, a Market-level Buy (spend quote to get base) is physically
        // a swap in the opposite direction through the pool, and vice versa.
        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        let raw_price = self.raw_price_token1_per_token0();
        // raw_price = token_1_raw per token_0_raw

        if self.pool.liquidity == 0 {
            return Err("Pool has zero liquidity".into());
        }

        let fee_bps = 25u64;
        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        // physical_direction is in the pool's raw token ordering.
        // Buy physically: token_0 -> token_1, output = input * raw_price
        // Sell physically: token_1 -> token_0, output = input / raw_price
        // Cap at available reserves — can't output more than the pool holds.
        let output = match physical_direction {
            SwapDirection::Buy => {
                let raw = (amount_in_with_fee as f64 * raw_price) as u64;
                raw.min(self.vault_1_balance)
            }
            SwapDirection::Sell => {
                if raw_price == 0.0 {
                    return Err("Zero price".into());
                }
                let raw = (amount_in_with_fee as f64 / raw_price) as u64;
                raw.min(self.vault_0_balance)
            }
        };

        Ok(output)
    }
}

// ============================================================================
// Market Trait Implementation
// ============================================================================

impl Market for RaydiumClmmMarket {
    fn is_active(&self) -> bool {
        // Bitfield: bit4 = disable swap. Pool is swappable when bit4 is clear.
        self.pool.status & (1 << 4) == 0
    }

    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        // CLMM fees are typically 0.25% (25 bps)
        let trade_fee_bps = 25u64;

        let (quote_mint, base_mint, quote_vault, base_vault) = if self.flipped {
            (self.pool.token_mint_1, self.pool.token_mint_0, self.pool.token_vault_1, self.pool.token_vault_0)
        } else {
            (self.pool.token_mint_0, self.pool.token_mint_1, self.pool.token_vault_0, self.pool.token_vault_1)
        };

        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Raydium CLMM".to_string(),
            quote_mint,
            base_mint,
            quote_vault,
            base_vault,
            fees: PoolFees {
                trade_fee_bps,
                protocol_fee_bps: None,
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        let (quote_balance, base_balance, quote_decimals, base_decimals) = if self.flipped {
            (self.vault_1_balance, self.vault_0_balance, self.pool.mint_decimals_1, self.pool.mint_decimals_0)
        } else {
            (self.vault_0_balance, self.vault_1_balance, self.pool.mint_decimals_0, self.pool.mint_decimals_1)
        };
        Ok(PoolFinancials {
            quote_balance,
            base_balance,
            quote_decimals,
            base_decimals,
        })
    }

    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        self.calculate_clmm_output(amount_in, direction)
    }

    fn calculate_price_impact(
        &self,
        _amount_in: u64,
        _direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // CLMM uses concentrated liquidity with tick-based pricing.
        // Reserve-based impact estimation is incorrect for CLMM.
        // Return 0 — actual impact from tick traversal is handled on-chain.
        Ok(0)
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        Ok(self.sqrt_price_to_price())
    }

    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        quote_vault_balance: u64,
        base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        let pool_data = match pool_data {
            Some(d) => d,
            None => return self.calculate_clmm_output(amount_in, direction),
        };

        if pool_data.len() < 269 {
            return self.calculate_clmm_output(amount_in, direction);
        }

        let liquidity = u128::from_le_bytes(pool_data[237..253].try_into().unwrap());
        let sqrt_price_x64 = u128::from_le_bytes(pool_data[253..269].try_into().unwrap());

        if liquidity == 0 {
            return Err("Pool has zero liquidity".into());
        }

        let sqrt = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
        let raw_price = sqrt * sqrt;

        let physical_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        // Map normalized vault balances back to physical vault_0 / vault_1.
        // Caller already flipped for us, so:
        //   flipped:     quote=vault_1, base=vault_0
        //   not flipped: quote=vault_0, base=vault_1
        let (vault_0_balance, vault_1_balance) = if self.flipped {
            (base_vault_balance, quote_vault_balance)
        } else {
            (quote_vault_balance, base_vault_balance)
        };

        let fee_bps = 25u64;
        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        let output = match physical_direction {
            SwapDirection::Buy => {
                let raw = (amount_in_with_fee as f64 * raw_price) as u64;
                raw.min(vault_1_balance)
            }
            SwapDirection::Sell => {
                if raw_price == 0.0 {
                    return Err("Zero price".into());
                }
                let raw = (amount_in_with_fee as f64 / raw_price) as u64;
                raw.min(vault_0_balance)
            }
        };

        Ok(output)
    }
}

