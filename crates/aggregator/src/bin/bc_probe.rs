//! Probe live pump.fun bonding curves: fetch each mint's curve via RPC, print
//! reserves + solroute's buy quote for a fixed SOL amount. Used to sanity-check
//! and fee-calibrate the bonding-curve venue against a real on-chain curve.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin bc_probe -- <mint>...

use std::str::FromStr;

use solana_pubkey::Pubkey;

use solroute_aggregator::loader::{PoolLoader, ProgressCallback};
use solroute_core::SwapDirection;

#[tokio::main]
async fn main() {
    let rpc = std::env::var("RPC_URL").expect("set RPC_URL");
    let mints: Vec<Pubkey> = std::env::args().skip(1).filter_map(|s| Pubkey::from_str(&s).ok()).collect();
    if mints.is_empty() {
        eprintln!("usage: bc_probe <mint>...");
        std::process::exit(1);
    }

    let loader = PoolLoader::new(&rpc);
    let cb: ProgressCallback = Box::new(|_| {});
    let pools = loader.load_bonding_curves_for_mints(&mints, &cb).await.expect("load curves");

    let amount = 100_000_000u64; // 0.1 SOL
    println!("loaded {} live (non-complete) curves of {} mints\n", pools.len(), mints.len());
    for (addr, e) in &pools {
        let fin = e.market.financials().unwrap();
        let price = e.market.current_price().unwrap_or(0.0);
        let out = e.market.calculate_output(amount, SwapDirection::Buy).unwrap_or(0);
        println!("mint  {}", e.base_mint);
        println!("  curve {}", addr);
        println!("  real_sol {:.4} SOL   real_tok {}", fin.quote_balance as f64 / 1e9, fin.base_balance);
        println!("  price    {:.6e} SOL/token", price);
        println!("  BUY 0.1 SOL -> {} tokens ({} raw)\n", out as f64 / 1e6, out);
    }
}
