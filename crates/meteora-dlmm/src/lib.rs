//! Meteora Dynamic Liquidity Market Maker (DLMM) DEX crate.
//!
//! Implements the `Market` trait from `thunder-core`
//! for Meteora DLMM bin-based concentrated liquidity pools.


use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use thunder_core::{
    quote_priority, GenericError, Market,
    PoolFees, PoolFinancials, PoolMetadata,
    SwapDirection, infer_mint_decimals,
};

// ---------------------------------------------------------------------------
// DEX-specific constants
// ---------------------------------------------------------------------------

pub const METEORA_DYNAMIC_LMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

// ---------------------------------------------------------------------------
// On-chain model structs
// ---------------------------------------------------------------------------

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct StaticParameters {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub protocol_share: u16,
    pub base_fee_power_factor: u8,
    pub padding: [u8; 5],
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct VariableParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub padding: [u8; 4],
    pub last_update_timestamp: i64,
    pub padding1: [u8; 8],
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct ProtocolFee {
    pub amount_x: u64,
    pub amount_y: u64,
}

#[derive(
    BorshDeserialize, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Hash,
)]
pub struct RewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub funder: Pubkey,
    pub reward_duration: u64,
    pub reward_duration_end: u64,
    pub reward_rate: u128,
    pub last_update_time: u64,
    pub cumulative_seconds_with_empty_liquidity_reward: u64,
}

#[derive(BorshDeserialize, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeteoraDLMMPool {
    pub parameters: StaticParameters,
    pub v_parameters: VariableParameters,
    pub bump_seed: [u8; 1],
    pub bin_step_seed: [u8; 2],
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub require_base_factor_seed: u8,
    pub base_factor_seed: [u8; 2],
    pub activation_type: u8,
    pub creator_pool_on_off_control: u8,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub protocol_fee: ProtocolFee,
    pub padding1: [u8; 32],
    pub reward_infos: [RewardInfo; 2],
    pub oracle: Pubkey,
    pub bin_array_bitmap: [u64; 16],
    pub last_updated_at: i64,
    pub padding2: [u8; 32],
    pub pre_activation_swap_address: Pubkey,
    pub base_key: Pubkey,
    pub activation_point: u64,
    pub pre_activation_duration: u64,
    pub padding3: [u8; 8],
    pub padding4: u64,
    pub creator: Pubkey,
    pub token_mint_x_program_flag: u8,
    pub token_mint_y_program_flag: u8,
    pub reserved: [u8; 22],
}


// ---------------------------------------------------------------------------
// Market wrapper
// ---------------------------------------------------------------------------

pub struct MeteoraDlmmMarket {
    pub pool: MeteoraDLMMPool,
    pub pool_address: String,
    pub reserve_x_balance: u64,
    pub reserve_y_balance: u64,
    pub token_x_decimals: u8,
    pub token_y_decimals: u8,
    pub flipped: bool,
    /// Bitmap extension account address (if the pool needs one).
    /// Set via `set_bitmap_extension()` after looking it up on-chain.
    pub bitmap_extension: Option<Pubkey>,
}

impl MeteoraDlmmMarket {
    pub fn new(pool: MeteoraDLMMPool, pool_address: String) -> Self {
        let flipped = quote_priority(&pool.token_x_mint).unwrap_or(usize::MAX) < quote_priority(&pool.token_y_mint).unwrap_or(usize::MAX);
        let token_x_decimals = infer_mint_decimals(&pool.token_x_mint);
        let token_y_decimals = infer_mint_decimals(&pool.token_y_mint);
        Self {
            pool,
            pool_address,
            reserve_x_balance: 0,
            reserve_y_balance: 0,
            token_x_decimals,
            token_y_decimals,
            flipped,
            bitmap_extension: None,
        }
    }

    /// Calculate output for DLMM swap.
    ///
    /// Simplified calculation — real DLMM swaps traverse multiple bins.
    /// For production, use the actual DLMM math library or RPC simulation.
    fn calculate_dlmm_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // Use raw bin price (no decimal adjustment) since amount_in/output are raw token units.
        let bin_step = self.pool.bin_step as f64;
        let active_id = self.pool.active_id;
        let base = 1.0 + (bin_step / 10000.0);
        let price = base.powi(active_id);

        // Fee = base_factor * bin_step / 10_000 (in bps).
        let fee_bps = (self.pool.parameters.base_factor as u64 * self.pool.bin_step as u64) / 10_000;

        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        // After normalization, Buy = spend quote to get base, Sell = spend base to get quote.
        // Flip the physical direction when the pool sides are swapped.
        let effective_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        // Cap at available reserves — can't output more than the pool holds.
        let output = match effective_direction {
            SwapDirection::Buy => {
                // Y → X: output ≈ amount_in / price, capped at reserve_x.
                let raw = (amount_in_with_fee as f64 / price) as u64;
                raw.min(self.reserve_x_balance)
            }
            SwapDirection::Sell => {
                // X → Y: output ≈ amount_in * price, capped at reserve_y.
                let raw = (amount_in_with_fee as f64 * price) as u64;
                raw.min(self.reserve_y_balance)
            }
        };

        Ok(output)
    }

    /// Bin price: (1 + bin_step / 10000)^active_id * 10^(dec_x - dec_y)
    /// Result = token_y per token_x (human-readable).
    /// E.g. for SOL/USDC: returns ~82 (USDC per SOL).
    fn calculate_dlmm_price(&self) -> f64 {
        let bin_step = self.pool.bin_step as f64;
        let active_id = self.pool.active_id;
        let base = 1.0 + (bin_step / 10000.0);
        let raw = base.powi(active_id);
        raw * 10f64.powi(self.token_x_decimals as i32 - self.token_y_decimals as i32)
    }
}

// ---------------------------------------------------------------------------
// Market trait
// ---------------------------------------------------------------------------

impl Market for MeteoraDlmmMarket {
    fn is_active(&self) -> bool {
        // 0 = Enabled, 1 = Disabled (Meteora PairStatus enum).
        self.pool.status == 0
    }

    fn metadata(&self) -> Result<PoolMetadata, GenericError> {
        // Fee = base_factor * bin_step / 10_000 (in bps).
        let trade_fee_bps = (self.pool.parameters.base_factor as u64 * self.pool.bin_step as u64) / 10_000;

        let (quote_mint, base_mint, quote_vault, base_vault) = if self.flipped {
            (self.pool.token_x_mint, self.pool.token_y_mint, self.pool.reserve_x, self.pool.reserve_y)
        } else {
            (self.pool.token_y_mint, self.pool.token_x_mint, self.pool.reserve_y, self.pool.reserve_x)
        };

        Ok(PoolMetadata {
            address: self.pool_address.clone(),
            dex_name: "Meteora DLMM".to_string(),
            quote_mint,
            base_mint,
            quote_vault,
            base_vault,
            fees: PoolFees {
                trade_fee_bps,
                protocol_fee_bps: Some(self.pool.parameters.protocol_share as u64),
            },
        })
    }

    fn financials(&self) -> Result<PoolFinancials, GenericError> {
        let (quote_balance, base_balance, quote_decimals, base_decimals) = if self.flipped {
            (self.reserve_x_balance, self.reserve_y_balance, self.token_x_decimals, self.token_y_decimals)
        } else {
            (self.reserve_y_balance, self.reserve_x_balance, self.token_y_decimals, self.token_x_decimals)
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
        self.calculate_dlmm_output(amount_in, direction)
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
            Some(d) if d.len() >= 80 => d,
            _ => return self.calculate_dlmm_output(amount_in, direction),
        };

        // Parse live active_id (i32 at account offset 76, after 8-byte discriminator).
        let active_id = i32::from_le_bytes(
            pool_data[76..80].try_into().unwrap(),
        );

        let bin_step = self.pool.bin_step as f64;
        let base = 1.0 + (bin_step / 10000.0);
        let price = base.powi(active_id);

        // Fee = base_factor * bin_step / 10_000 (in bps).
        let fee_bps = (self.pool.parameters.base_factor as u64 * self.pool.bin_step as u64) / 10_000;
        let fee_multiplier = 10000 - fee_bps;
        let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;

        // Reconstruct reserve balances from caller-provided vault balances.
        // Caller already flipped if needed: quote=quote, base=base in normalized orientation.
        // DLMM: token_x = base, token_y = quote (when !flipped).
        let (reserve_x_balance, reserve_y_balance) = if self.flipped {
            (quote_vault_balance, base_vault_balance)
        } else {
            (base_vault_balance, quote_vault_balance)
        };

        let effective_direction = if self.flipped {
            match direction {
                SwapDirection::Buy => SwapDirection::Sell,
                SwapDirection::Sell => SwapDirection::Buy,
            }
        } else {
            direction
        };

        let output = match effective_direction {
            SwapDirection::Buy => {
                // Y -> X: output ~ amount_in / price, capped at reserve_x.
                let raw = (amount_in_with_fee as f64 / price) as u64;
                raw.min(reserve_x_balance)
            }
            SwapDirection::Sell => {
                // X -> Y: output ~ amount_in * price, capped at reserve_y.
                let raw = (amount_in_with_fee as f64 * price) as u64;
                raw.min(reserve_y_balance)
            }
        };

        Ok(output)
    }

    fn calculate_price_impact(
        &self,
        _amount_in: u64,
        _direction: SwapDirection,
    ) -> Result<u64, GenericError> {
        // DLMM uses bin-based pricing, not constant-product reserves.
        // Within a single bin, the price is fixed (zero impact).
        // Price impact only occurs when the swap crosses bin boundaries.
        // For a conservative estimate, return 0 (the swap_builder and
        // on-chain program handle the actual bin traversal).
        Ok(0)
    }

    fn current_price(&self) -> Result<f64, GenericError> {
        let price = self.calculate_dlmm_price();
        // price = token_y per token_x (human-readable).
        // NOT flipped (quote=y, base=x): price is already quote_per_base. Return as-is.
        // Flipped (quote=x, base=y): want x_per_y = 1/price.
        if self.flipped {
            if price == 0.0 {
                return Err("Pool has zero price".into());
            }
            Ok(1.0 / price)
        } else {
            Ok(price)
        }
    }

}
