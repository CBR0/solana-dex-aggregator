//! Probe live bonk.fun / Raydium LaunchLab pools: fetch a pool account (by pool
//! address), parse, print reserves + solroute's buy quote for a fixed SOL amount.
//! Fee-calibration / sanity vs Jupiter.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin bonk_probe -- <pool_addr>...

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use bonk::{parse_pool_state, BonkMarket};
use solroute_core::{Market, SwapDirection};

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(std::env::var("RPC_URL").expect("set RPC_URL"));
    let pools: Vec<Pubkey> = std::env::args().skip(1).filter_map(|s| Pubkey::from_str(&s).ok()).collect();
    if pools.is_empty() {
        eprintln!("usage: bonk_probe <pool_addr>...");
        std::process::exit(1);
    }
    let accts = rpc.get_multiple_accounts(&pools).await.expect("fetch");
    let amount = 100_000_000u64; // 0.1 SOL
    for (addr, maybe) in pools.iter().zip(accts) {
        let Some(a) = maybe else { println!("{addr}  NOT FOUND\n"); continue };
        let Some(pool) = parse_pool_state(&a.data) else { println!("{addr}  parse fail (len {})\n", a.data.len()); continue };
        let base_mint = pool.base_mint;
        let m = BonkMarket::new(pool, addr.to_string());
        let fin = m.financials().unwrap();
        let price = m.current_price().unwrap_or(0.0);
        let out = m.calculate_output(amount, SwapDirection::Buy);
        println!("pool  {addr}");
        println!("  base_mint {base_mint}");
        println!("  status {}  real_quote {:.4} SOL   base_avail {}", m.pool.status, fin.quote_balance as f64 / 1e9, fin.base_balance);
        println!("  price  {price:.6e}");
        match out {
            Ok(o) => println!("  BUY 0.1 SOL -> {o} base ({:.4} tokens)\n", o as f64 / 10f64.powi(m.pool.base_decimals as i32)),
            Err(e) => println!("  BUY err: {e}\n"),
        }
    }
}
