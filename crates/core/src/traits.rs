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

    /// Like `calculate_output_live`, but with access to the full account
    /// store. Override in DEX crates whose quote needs accounts beyond the
    /// pool + vaults (DLMM bin arrays, CLMM tick arrays) — pulled from the
    /// provider by derived PDA. Defaults to `calculate_output_live`.
    fn calculate_output_live_ex(
        &self,
        amount_in: u64,
        direction: SwapDirection,
        pool_data: Option<&[u8]>,
        quote_vault_balance: u64,
        base_vault_balance: u64,
        _provider: &dyn AccountDataProvider,
    ) -> Result<u64, GenericError> {
        self.calculate_output_live(
            amount_in,
            direction,
            pool_data,
            quote_vault_balance,
            base_vault_balance,
        )
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

// ============================================================================
// Price freshness (sqrt-implied vs vault-balance-implied)
// ============================================================================

/// Tolerância máxima de frescor para pools que precificam no pool account
/// (Raydium CLMM, Orca Whirlpool): o preço implícito pela sqrt_price não pode
/// divergir dos balanços reais dos vaults por mais de `MAX_PRICE_FRESHNESS_FACTOR`
/// (fator >= 1; 2 = 2x de diferença).
///
/// Valor GROSSO (50x) de propósito: para CLMM/Whirlpool a razão dos vaults
/// legitimamente diverge do spot por geometria de liquidez concentrada
/// (posição fora do centro do range — observado até ~15x em pools reais), então
/// um limiar apertado rejeitaria pools saudáveis. O objetivo aqui é só pegar o
/// caso patológico — pool account cacheado de outra era de preço (fator 1e6+) —
/// quando o quote cai no struct cacheado (fallback defensivo). O fix real de
/// frescor é o cold-start buscar os pool accounts (o stream cobre os ativos).
pub const MAX_PRICE_FRESHNESS_FACTOR: f64 = 50.0;

/// Preço implícito (quote por base, humano) a partir dos balanços de vault.
/// `None` quando qualquer balanço é zero (sem dados para inferir preço).
pub fn vault_implied_price(
    quote_balance: u64,
    base_balance: u64,
    quote_decimals: u8,
    base_decimals: u8,
) -> Option<f64> {
    if quote_balance == 0 || base_balance == 0 {
        return None;
    }
    let q = quote_balance as f64 / 10f64.powi(quote_decimals as i32);
    let b = base_balance as f64 / 10f64.powi(base_decimals as i32);
    Some(q / b)
}

/// Fator de divergência entre dois preços: `max(p/q, q/p)`. Sempre >= 1
/// (1 = idênticos, 2 = 2x de diferença). `None` se algum preço for inválido
/// (<= 0 ou não finito) — o chamador não deve tratar como stale nesse caso.
pub fn price_divergence_factor(p: f64, q: f64) -> Option<f64> {
    if !p.is_finite() || !q.is_finite() || p <= 0.0 || q <= 0.0 {
        return None;
    }
    Some((p / q).max(q / p))
}

/// Fator de frescor de um market: quanto o preço implícito pela fonte de
/// preço do market (pool account / dados cacheados) diverge do preço dos
/// balanços de vault passados. `None` quando não dá para comparar (algum
/// lado indisponível) — o chamador NÃO deve tratar como stale nesse caso.
pub fn market_price_freshness_factor(
    market: &dyn Market,
    quote_vault_balance: u64,
    base_vault_balance: u64,
) -> Option<f64> {
    let fin = market.financials().ok()?;
    let p_implied = market.current_price().ok()?;
    let p_vaults = vault_implied_price(
        quote_vault_balance,
        base_vault_balance,
        fin.quote_decimals,
        fin.base_decimals,
    )?;
    price_divergence_factor(p_implied, p_vaults)
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

    /// Account data together with the slot it was last written at.
    /// Slot 0 means "age unknown" — callers must not treat it as stale.
    fn account_data_with_slot(&self, pubkey: &Pubkey) -> Option<(Vec<u8>, u64)> {
        self.pool_account_data(pubkey).map(|d| (d, 0))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_divergence_factor_math() {
        // Idênticos -> 1.
        assert_eq!(price_divergence_factor(10.0, 10.0), Some(1.0));
        // 2x de diferença em qualquer direção -> 2.
        assert_eq!(price_divergence_factor(2.0, 1.0), Some(2.0));
        assert_eq!(price_divergence_factor(1.0, 2.0), Some(2.0));
        // ~50x (limiar de frescor CLMM).
        assert_eq!(price_divergence_factor(1.0, 50.0), Some(50.0));
        // Entradas inválidas -> None (não tratar como stale).
        assert_eq!(price_divergence_factor(0.0, 1.0), None);
        assert_eq!(price_divergence_factor(-1.0, 1.0), None);
        assert_eq!(price_divergence_factor(f64::NAN, 1.0), None);
        assert_eq!(price_divergence_factor(1.0, f64::INFINITY), None);
    }

    #[test]
    fn vault_implied_price_math() {
        // 1.5 SOL por USDC (SOL 9 dec, USDC 6 dec): vault de SOL = 1.5e9,
        // vault de USDC = 1e6 -> price = 1.5e9/1e9 / (1e6/1e6) = 1.5 / 1.0.
        let p = vault_implied_price(1_500_000_000, 1_000_000, 9, 6).unwrap();
        assert!((p - 1.5).abs() < 1e-9, "{p}");
        // Balanço zero -> None.
        assert_eq!(vault_implied_price(0, 1_000_000, 9, 6), None);
        assert_eq!(vault_implied_price(1_000_000, 0, 9, 6), None);
    }
}
