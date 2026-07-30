//! Simulate a bonk.fun / LaunchLab BUY against live state to verify the executor
//! (account layout / PDAs / program acceptance) AND bound the quote: `min_out` is
//! set to 98% of solroute's quote, so a PASS means the program produced within 2%
//! of what solroute quoted (no gross over-quote).
//!
//! The two fee-vault accounts (platform + creator, WSOL) that the current
//! program requires as trailing accounts are passed explicitly — observe them
//! from a recent buy tx on the pool (accounts 16 and 17). A PASS proves the
//! 18-account layout is correct; deriving those vaults programmatically is the
//! open item for production execution.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin bonk_sim -- <pool_addr> <platform_fee_vault> <creator_fee_vault> [funded_payer]

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use bonk::{parse_pool_state, BonkMarket};
use solroute_core::{Market, SwapDirection, TOKEN_PROGRAM, TOKEN_PROGRAM_2022, WSOL};
use solroute_executor::bonk::{self as bonk_exec, BonkAccounts};
use solroute_executor::{submit, SwapLeg, SwapOptions};

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(std::env::var("RPC_URL").expect("set RPC_URL"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_addr = Pubkey::from_str(&args[0]).expect("pool addr");
    let platform_fee_vault = Pubkey::from_str(&args[1]).expect("platform_fee_vault (buy tx acct 16)");
    let creator_fee_vault = Pubkey::from_str(&args[2]).expect("creator_fee_vault (buy tx acct 17)");
    let payer = args.get(3).and_then(|s| Pubkey::from_str(s).ok())
        .unwrap_or_else(|| Pubkey::from_str_const("5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9"));

    let acct = rpc.get_account(&pool_addr).await.expect("pool account");
    let pool = parse_pool_state(&acct.data).expect("parse pool");
    println!("pool {pool_addr}  status={}  real_quote={:.4} SOL  base_mint={}",
        pool.status, pool.real_quote as f64 / 1e9, pool.base_mint);

    let base_mint = pool.base_mint;
    let quote_mint = pool.quote_mint;
    let (base_vault, quote_vault) = (pool.base_vault, pool.quote_vault);
    let (global_config, platform_config) = (pool.global_config, pool.platform_config);

    let amount_in = 50_000_000u64; // 0.05 SOL
    let m = BonkMarket::new(pool, pool_addr.to_string());
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
    let accounts = BonkAccounts { pool_state: pool_addr, global_config, platform_config, base_mint, quote_mint, base_vault, quote_vault };

    let mut ixs = submit::compute_budget_ixs(250_000, 0);
    ixs.extend(bonk_exec::build_swap(&accounts, &leg, base_prog, quote_prog, platform_fee_vault, creator_fee_vault, &opts).expect("build"));
    let tx = submit::build_unsigned_transaction(&payer, &ixs);

    match submit::simulate(&rpc, &tx).await {
        Ok(r) => match &r.err {
            None => println!("VERDICT=PASS units={:?}", r.units_consumed),
            Some(e) => {
                println!("VERDICT=FAIL err={e:?}");
                for l in r.logs.unwrap_or_default().iter().filter(|l| l.contains("Error") || l.contains("failed") || l.contains("Program log")).take(12) {
                    println!("  {l}");
                }
            }
        },
        Err(e) => println!("SIM-ERR {e}"),
    }
}
