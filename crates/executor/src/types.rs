//! Shared execution types.

use solana_pubkey::Pubkey;

/// A single swap leg to execute against one pool.
///
/// `min_amount_out` is the slippage-protected floor — callers compute it from
/// the aggregator's quoted output (e.g. `quoted_out * (10_000 - slippage_bps) / 10_000`).
#[derive(Debug, Clone)]
pub struct SwapLeg {
    pub payer: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
}

/// ATA / native-SOL handling to wrap around the swap instruction.
///
/// Defaults are all `false` — the hot-path assumption is that ATAs already
/// exist (bots pre-create them). Turn these on for convenience flows.
#[derive(Debug, Clone, Default)]
pub struct SwapOptions {
    /// Create the input-mint ATA (idempotent) before swapping.
    pub create_input_ata: bool,
    /// Create the output-mint ATA (idempotent) before swapping.
    pub create_output_ata: bool,
    /// When the input mint is WSOL, wrap `amount_in` native SOL into the ATA.
    pub wrap_input_sol: bool,
    /// Close any WSOL ATA involved in the swap afterward (unwrap to SOL).
    pub close_wsol: bool,
}
