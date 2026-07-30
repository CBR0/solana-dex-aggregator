//! Probe live Meteora DBC pools: fetch a `VirtualPool` + its `PoolConfig`, parse
//! with the crate structs, print sqrt_price / reserves / curve / fee config.
//! Validates the borsh layout on real data (and will host the quote once wired).
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin dbc_probe -- <pool_addr>...

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use meteora_dbc::{parse_pool_config, parse_virtual_pool};

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(std::env::var("RPC_URL").expect("set RPC_URL"));
    let pools: Vec<Pubkey> = std::env::args().skip(1).filter_map(|s| Pubkey::from_str(&s).ok()).collect();
    if pools.is_empty() {
        eprintln!("usage: dbc_probe <pool_addr>...");
        std::process::exit(1);
    }
    for addr in &pools {
        let Ok(acc) = rpc.get_account(addr).await else { println!("{addr} NOT FOUND\n"); continue };
        let Some(pool) = parse_virtual_pool(&acc.data) else { println!("{addr} parse-fail (len {})\n", acc.data.len()); continue };
        println!("pool {addr}");
        println!("  base_mint {}  config {}", pool.base_mint, pool.config);
        println!("  is_migrated {}  pool_type {}  sqrt_price {}", pool.is_migrated, pool.pool_type, pool.sqrt_price);
        println!("  base_reserve {}  quote_reserve {}", pool.base_reserve, pool.quote_reserve);

        let Ok(cfg_acc) = rpc.get_account(&pool.config).await else { println!("  config NOT FOUND\n"); continue };
        let Some(cfg) = parse_pool_config(&cfg_acc.data) else { println!("  config parse-fail\n"); continue };
        let active: Vec<_> = cfg.curve.iter().take_while(|c| c.liquidity > 0 || c.sqrt_price > 0).collect();
        println!("  quote_mint {}  token_decimal {}  collect_fee_mode {}", cfg.quote_mint, cfg.token_decimal, cfg.collect_fee_mode);
        println!("  sqrt_start_price {}  active_curve_pts {}", cfg.sqrt_start_price, active.len());
        println!("  base_fee.cliff_fee_numerator {}  base_fee_mode {}  dynamic.initialized {}",
            cfg.pool_fees.base_fee.cliff_fee_numerator, cfg.pool_fees.base_fee.base_fee_mode, cfg.pool_fees.dynamic_fee.initialized);
        for (i, c) in active.iter().take(4).enumerate() {
            println!("    curve[{i}] sqrt_price {} liquidity {}", c.sqrt_price, c.liquidity);
        }
        println!();
    }
}
