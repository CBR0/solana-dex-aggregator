//! # Solana Thunder
//!
//! A Solana DEX aggregator library providing unified account parsing and
//! swap instruction building across multiple DEX protocols.
//!
//! Each DEX is an independent crate implementing the [`thunder_core::Market`] trait.
//! Use the concrete market types directly — there is no dispatch enum.
//!
//! ## Crates
//!
//! - [`thunder_core`] — `Market` trait, shared types, constants
//! - [`raydium_amm_v4`] — Raydium AMM V4 (constant product)
//! - [`raydium_clmm`] — Raydium CLMM (concentrated liquidity)
//! - [`meteora_damm`] — Meteora DAMM V1 + V2
//! - [`meteora_dlmm`] — Meteora DLMM (dynamic liquidity bins)
//! - [`pumpfun_amm`] — Pumpfun AMM (bonding curve)

pub use thunder_core;

pub use meteora_damm;
pub use meteora_dlmm;
pub use pumpfun_amm;
pub use raydium_amm_v4;
pub use raydium_clmm;
