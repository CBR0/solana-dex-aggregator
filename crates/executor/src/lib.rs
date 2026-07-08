//! # solroute Executor
//!
//! Swap-instruction builders for executing routes produced by the aggregator.
//! Each supported DEX exposes a small `*Accounts` struct (populated from the
//! pool state the aggregator already parses) and a pure `build_swap` function
//! that emits the on-chain swap instruction plus optional ATA setup/teardown.
//!
//! Instruction layouts are ported from the FnZero `sol-trade-sdk` (MIT) and
//! adapted to feed from solroute's own pool structs. No SWQoS/nonce/gas
//! infrastructure — just correct instruction construction.

pub mod alt;
pub mod ata;
pub mod meteora_dlmm;
pub mod meteora_damm_v1;
pub mod meteora_damm_v2;
pub mod pumpswap;
pub mod raydium_clmm;
pub mod raydium_amm_v4;
pub mod submit;
pub mod types;

pub use types::{SwapLeg, SwapOptions};
