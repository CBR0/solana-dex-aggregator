//! Core types for the aggregator: routes, quotes, and pool entries.

use std::fmt;

use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use thunder_core::Market;

// ---------------------------------------------------------------------------
// Pool storage
// ---------------------------------------------------------------------------

/// A pool stored in the index, wrapping a type-erased Market implementation.
pub struct PoolEntry {
    pub market: Box<dyn Market>,
    pub dex_name: String,
    /// Pre-resolved mints from metadata for fast direction lookup in routing.
    pub quote_mint: Pubkey,
    pub base_mint: Pubkey,
    /// On-chain pubkey of the pool account (for live data lookups).
    pub pool_pubkey: Pubkey,
    /// Vault token account pubkeys (for live balance lookups).
    pub quote_vault: Pubkey,
    pub base_vault: Pubkey,
    /// Serialized pool data for disk cache (bincode of CachedPool variant).
    pub cached_data: Vec<u8>,
}

impl fmt::Debug for PoolEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolEntry")
            .field("dex_name", &self.dex_name)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// A single hop within a multi-hop route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteHop {
    /// Pool address used for this hop.
    pub pool_address: String,
    /// DEX that owns the pool.
    pub dex_name: String,
    /// Token mint going in.
    pub input_mint: Pubkey,
    /// Token mint coming out.
    pub output_mint: Pubkey,
    /// Raw amount entering this hop (lamports / raw units).
    pub input_amount: u64,
    /// Raw amount exiting this hop after fees.
    pub output_amount: u64,
    /// Price impact of this hop in basis points.
    pub price_impact_bps: u64,
}

/// A complete route from source token to destination token (1-3 hops).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub hops: Vec<RouteHop>,
    /// First hop's input mint.
    pub input_mint: Pubkey,
    /// Last hop's output mint.
    pub output_mint: Pubkey,
    /// Amount entering the first hop.
    pub input_amount: u64,
    /// Amount exiting the last hop.
    pub output_amount: u64,
    /// Aggregate price impact across all hops (bps).
    pub price_impact_bps: u64,
}

/// Result of a quote request: ranked routes for a given swap.
#[derive(Debug, Clone)]
pub struct Quote {
    /// Routes sorted by output amount descending (best first).
    pub routes: Vec<Route>,
}

impl Quote {
    pub fn best(&self) -> Option<&Route> {
        self.routes.first()
    }
}

// ---------------------------------------------------------------------------
// Loading progress
// ---------------------------------------------------------------------------

/// Progress update emitted during pool loading.
#[derive(Debug, Clone)]
pub struct LoadProgress {
    pub dex_name: String,
    pub phase: LoadPhase,
}

#[derive(Debug, Clone)]
pub enum LoadPhase {
    /// Fetching pool accounts from RPC.
    FetchingPools,
    /// Deserializing pool accounts.
    Deserializing { done: usize, total: usize },
    /// Fetching vault balances.
    FetchingBalances { done: usize, total: usize },
    /// Building market structs.
    BuildingMarkets { done: usize, total: usize },
    /// Done loading this DEX.
    Complete { pool_count: usize },
    /// An error occurred (non-fatal, this DEX skipped).
    Error(String),
}

// ---------------------------------------------------------------------------
// Price
// ---------------------------------------------------------------------------

/// Token price result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPrice {
    pub mint: Pubkey,
    /// Price in SOL (None if the token IS SOL).
    pub price_sol: Option<f64>,
    /// Price in USD (None if unavailable).
    pub price_usd: Option<f64>,
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Aggregated statistics about loaded pools and system resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatorStats {
    pub pools_per_dex: Vec<(String, usize)>,
    pub total_pools: usize,
    pub unique_tokens: usize,
    pub memory_mb: f64,
    pub cpu_percent: f32,
    pub uptime_secs: u64,
}
