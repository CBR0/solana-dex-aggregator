//! Core trait definitions for the DEX aggregator.

use std::error::Error;

use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

/// All trait methods use this as the error type.
pub type GenericError = Box<dyn Error + Send + Sync>;

// ============================================================================
// Core Data Structures
// ============================================================================

/// Direction of the swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapDirection {
    /// Buy: Quote → Base (SOL → Token)
    Buy,
    /// Sell: Base → Quote (Token → SOL)
    Sell,
}

/// Pool financial state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolFinancials {
    pub quote_balance: u64,
    pub base_balance: u64,
    pub quote_decimals: u8,
    pub base_decimals: u8,
}

/// Fee structure for a pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolFees {
    /// Trading fee as basis points (100 = 1%).
    pub trade_fee_bps: u64,
    /// Protocol fee (if applicable).
    pub protocol_fee_bps: Option<u64>,
}

/// Generic pool metadata (common across all DEXs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolMetadata {
    pub address: String,
    pub dex_name: String,
    pub quote_mint: Pubkey,
    pub base_mint: Pubkey,
    pub quote_vault: Pubkey,
    pub base_vault: Pubkey,
    pub fees: PoolFees,
}

// ============================================================================
// Core Trait: Market
// ============================================================================

/// Unified interface for interacting with any DEX on Solana.
///
/// Each DEX crate provides a struct implementing this trait. All methods are
/// synchronous and pure — no I/O, no RPC calls.
pub trait Market: Send + Sync {
    /// Whether this pool is active and can execute swaps.
    /// Default: true. Override for DEXs with on-chain status fields.
    fn is_active(&self) -> bool {
        true
    }

    /// Get pool metadata (address, mints, vaults, fees).
    fn metadata(&self) -> Result<PoolMetadata, GenericError>;

    /// Get current pool financials (balances, decimals).
    fn financials(&self) -> Result<PoolFinancials, GenericError>;

    /// Calculate output amount for a given input (accounting for fees).
    fn calculate_output(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError>;

    /// Calculate price impact for a swap (in basis points, 100 = 1%).
    fn calculate_price_impact(
        &self,
        amount_in: u64,
        direction: SwapDirection,
    ) -> Result<u64, GenericError>;

    /// Get the current mid-market price (quote per base, e.g. SOL per token).
    fn current_price(&self) -> Result<f64, GenericError>;

    /// Calculate output using live on-chain data from the AccountStore.
    /// `pool_data`: raw bytes of the pool account (if available from store).
    /// `quote_vault_balance` / `base_vault_balance`: live vault token balances.
    ///
    /// Default: ignores live data and delegates to `calculate_output`.
    /// Override in DEX crates to parse swap-volatile fields (sqrt_price,
    /// active_id, liquidity, virtual reserves) from `pool_data` bytes.
    fn calculate_output_live(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        _pool_data: Option<&[u8]>,
        _quote_vault_balance: u64,
        _base_vault_balance: u64,
    ) -> Result<u64, GenericError> {
        self.calculate_output(amount_in, direction)
    }
}

// ============================================================================
// Shared Utilities
// ============================================================================

/// Calculate price impact in basis points (100 = 1%).
pub fn calculate_price_impact_bps(pre_swap_price: f64, post_swap_price: f64) -> u64 {
    let impact = ((post_swap_price - pre_swap_price) / pre_swap_price).abs();
    (impact * 10000.0) as u64
}

/// Standard AMM constant product formula: x * y = k.
pub fn constant_product_swap(
    reserve_in: u64,
    reserve_out: u64,
    amount_in: u64,
    fee_bps: u64,
) -> Result<u64, GenericError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err("Pool has zero liquidity".into());
    }
    let fee_multiplier = 10000 - fee_bps;
    let amount_in_with_fee = (amount_in as u128 * fee_multiplier as u128) / 10000;
    let numerator = amount_in_with_fee * reserve_out as u128;
    let denominator = reserve_in as u128 + amount_in_with_fee;
    Ok((numerator / denominator) as u64)
}


// ============================================================================
// Live Data Provider
// ============================================================================

/// Provides live account data for routing. Implemented by the engine's
/// AccountStore so the aggregator Router can read fresh on-chain state
/// without depending on the engine crate.
pub trait AccountDataProvider: Send + Sync {
    /// Raw account data for a pool, keyed by its on-chain Pubkey.
    fn pool_account_data(&self, pubkey: &Pubkey) -> Option<Vec<u8>>;

    /// SPL token account balance (offset 64..72 in token account data).
    fn token_balance(&self, vault_pubkey: &Pubkey) -> u64;
}