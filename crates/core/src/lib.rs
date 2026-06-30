//! # Thunder Core
//!
//! Core traits and types for the Solana Thunder DEX aggregator.
//!
//! This crate defines the unified `Market` trait and shared types used by all
//! DEX implementations. Each DEX crate depends only on this core crate,
//! keeping implementations fully independent.
//!
//! ## Supported DEXs
//!
//! - Raydium AMM V4 (constant product)
//! - Raydium CLMM (concentrated liquidity)
//! - Meteora DAMM V1/V2 (dynamic AMM)
//! - Meteora DLMM (dynamic liquidity bins)
//! - Pumpfun AMM (bonding curve)

mod constants;
mod traits;

pub use constants::*;
pub use traits::*;
