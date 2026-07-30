//! Simulate a pump.fun bonding-curve BUY against live on-chain state to verify
//! the executor's account layout / PDAs / program acceptance (no SOL, no key).
//!
//! A dummy payer has no lamports, so a *balance* failure ("insufficient
//! lamports") means the program ACCEPTED the account layout — LAYOUT-OK. A
//! custom program error (6xxx / Anchor 2006 / 3012) means the layout is wrong.
//!
//! Usage:
//!   RPC_URL=... cargo run --release -p solroute-aggregator --bin bc_sim -- <mint> [payer_pubkey]

use std::str::FromStr;

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use pumpfun_amm::{derive_bonding_curve_pda, parse_bonding_curve};
use solroute_core::{TOKEN_PROGRAM, TOKEN_PROGRAM_2022, WSOL};
use solroute_executor::pumpfun_bc::{self, PumpBcAccounts};
use solroute_executor::{submit, SwapLeg, SwapOptions};

#[tokio::main]
async fn main() {
    let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL");
    let rpc = RpcClient::new(rpc_url);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mint = Pubkey::from_str(&args[0]).expect("mint");
    let payer = args
        .get(1)
        .and_then(|s| Pubkey::from_str(s).ok())
        .unwrap_or_else(|| Pubkey::new_from_array([7u8; 32]));

    // Fetch + parse the live curve.
    let curve_pda = derive_bonding_curve_pda(&mint);
    let acct = rpc.get_account(&curve_pda).await.expect("curve account");
    let curve = parse_bonding_curve(&acct.data).expect("parse curve");
    println!(
        "curve {curve_pda}  complete={}  real_sol={:.4}  creator={}",
        curve.complete,
        curve.real_sol_reserves as f64 / 1e9,
        curve.creator
    );

    // Resolve the mint's token program (pump mints may be Token-2022).
    let token_program = match rpc.get_account(&mint).await {
        Ok(a) if a.owner == Pubkey::from_str_const(TOKEN_PROGRAM_2022) => {
            Pubkey::from_str_const(TOKEN_PROGRAM_2022)
        }
        _ => Pubkey::from_str_const(TOKEN_PROGRAM),
    };

    let leg = SwapLeg {
        payer,
        input_mint: Pubkey::from_str_const(WSOL),
        output_mint: mint,
        amount_in: 10_000_000, // 0.01 SOL
        min_amount_out: 1,
    };
    let opts = SwapOptions { create_input_ata: false, create_output_ata: true, wrap_input_sol: false, close_wsol: false };
    let accounts = PumpBcAccounts { mint, bonding_curve: curve_pda, creator: curve.creator };

    let mut ixs = submit::compute_budget_ixs(200_000, 0);
    ixs.extend(pumpfun_bc::build_swap(&accounts, &leg, token_program, &opts).expect("build"));
    let tx = submit::build_unsigned_transaction(&payer, &ixs);

    match submit::simulate(&rpc, &tx).await {
        Ok(r) => match &r.err {
            None => println!("VERDICT=PASS units={:?}", r.units_consumed),
            Some(e) => {
                let logs = r.logs.clone().unwrap_or_default();
                let joined = logs.join("\n");
                let balance = joined.contains("insufficient lamports")
                    || joined.contains("Transfer: insufficient")
                    || format!("{e:?}").contains("InsufficientFundsForRent");
                let verdict = if balance { "LAYOUT-OK (balance-limited)" } else { "FAIL" };
                println!("VERDICT={verdict} err={e:?}");
                for l in logs.iter().filter(|l| l.contains("Program log") || l.contains("Error") || l.contains("failed")).take(12) {
                    println!("  {l}");
                }
            }
        },
        Err(e) => println!("SIM-ERR {e}"),
    }
}
