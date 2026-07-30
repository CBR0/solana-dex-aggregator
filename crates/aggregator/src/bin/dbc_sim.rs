//! Simulate a Meteora DBC BUY against live state — verifies the executor layout
//! AND bounds the quote: `min_out` = 98% of solroute's quote, so a PASS means the
//! program produced within 2% of what solroute quoted.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin dbc_sim -- <pool_addr> [funded_payer]

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use meteora_dbc::{parse_pool_config, parse_virtual_pool, DbcMarket};
use solroute_core::{Market, SwapDirection, TOKEN_PROGRAM, TOKEN_PROGRAM_2022, WSOL};
use solroute_executor::meteora_dbc::{self as dbc_exec, DbcAccounts};
use solroute_executor::{submit, SwapLeg, SwapOptions};

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(std::env::var("RPC_URL").expect("set RPC_URL"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_addr = Pubkey::from_str(&args[0]).expect("pool addr");
    let payer = args.get(1).and_then(|s| Pubkey::from_str(s).ok())
        .unwrap_or_else(|| Pubkey::from_str_const("5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9"));

    let pacc = rpc.get_account(&pool_addr).await.expect("pool");
    let pool = parse_virtual_pool(&pacc.data).expect("parse pool");
    let cacc = rpc.get_account(&pool.config).await.expect("config");
    let cfg = parse_pool_config(&cacc.data).expect("parse config");
    let base_mint = pool.base_mint;
    let (base_vault, quote_vault, config, quote_mint) = (pool.base_vault, pool.quote_vault, pool.config, cfg.quote_mint);

    // Clock for the fee scheduler.
    let slot = rpc.get_slot().await.unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let current_point = if cfg.activation_type == 1 { now } else { slot };

    let amount_in = 50_000_000u64; // 0.05 SOL
    println!("  base_fee_mode {}  cliff {}  period_freq {}  current_point {}",
        cfg.pool_fees.base_fee.base_fee_mode, cfg.pool_fees.base_fee.cliff_fee_numerator, cfg.pool_fees.base_fee.second_factor, current_point);
    let m = DbcMarket::new(pool, cfg, pool_addr.to_string()).with_current_point(current_point);
    println!("pool {pool_addr}  base_mint {base_mint}  tradeable {}", meteora_dbc::quote::is_tradeable(&m.pool, &m.config));
    let quote = m.calculate_output(amount_in, SwapDirection::Buy).expect("quote");
    let min_out = quote * 98 / 100;
    println!("solroute buy 0.05 SOL -> {quote} base ; min_out(98%)={min_out}");

    let base_prog = match rpc.get_account(&base_mint).await {
        Ok(a) if a.owner == Pubkey::from_str_const(TOKEN_PROGRAM_2022) => Pubkey::from_str_const(TOKEN_PROGRAM_2022),
        _ => Pubkey::from_str_const(TOKEN_PROGRAM),
    };
    let quote_prog = Pubkey::from_str_const(TOKEN_PROGRAM);

    let leg = SwapLeg { payer, input_mint: Pubkey::from_str_const(WSOL), output_mint: base_mint, amount_in, min_amount_out: min_out };
    let opts = SwapOptions { create_input_ata: true, create_output_ata: true, wrap_input_sol: true, close_wsol: true };
    let accounts = DbcAccounts { pool: pool_addr, config, base_mint, quote_mint, base_vault, quote_vault };

    let mut ixs = submit::compute_budget_ixs(250_000, 0);
    ixs.extend(dbc_exec::build_swap(&accounts, &leg, base_prog, quote_prog, &opts).expect("build"));
    let tx = submit::build_unsigned_transaction(&payer, &ixs);

    match submit::simulate(&rpc, &tx).await {
        Ok(r) => match &r.err {
            None => println!("VERDICT=PASS units={:?}", r.units_consumed),
            Some(e) => {
                println!("VERDICT=FAIL err={e:?}");
                for l in r.logs.unwrap_or_default().iter().filter(|l| l.contains("Error") || l.contains("failed") || l.contains("Program log")).take(14) {
                    println!("  {l}");
                }
            }
        },
        Err(e) => println!("SIM-ERR {e}"),
    }
}
