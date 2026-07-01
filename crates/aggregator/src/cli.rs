//! Interactive CLI: progress bars during loading, REPL for commands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rustyline::DefaultEditor;
use solana_pubkey::Pubkey;
use std::str::FromStr;

use crate::pool_index::PoolIndex;
use crate::price::get_token_price;
use crate::router::Router;
use crate::stats::StatsCollector;
use crate::types::{LoadPhase, LoadProgress};
use thunder_core::{infer_mint_decimals, WSOL};

// ---------------------------------------------------------------------------
// Loading progress display
// ---------------------------------------------------------------------------

/// Manages a set of progress bars (one per DEX) during pool loading.
pub struct LoadingDisplay {
    multi: MultiProgress,
    bars: Arc<Mutex<HashMap<String, ProgressBar>>>,
}

impl LoadingDisplay {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            bars: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns a callback suitable for `PoolLoader::load_all`.
    ///
    /// Each invocation updates the progress bar for the named DEX, creating it
    /// on first sight.
    pub fn progress_callback(&self) -> Box<dyn Fn(LoadProgress) + Send + Sync> {
        let bars = self.bars.clone();
        let multi = self.multi.clone();

        Box::new(move |progress: LoadProgress| {
            let mut bars = bars.lock().unwrap();
            let pb = bars.entry(progress.dex_name.clone()).or_insert_with(|| {
                let pb = multi.add(ProgressBar::new(0));
                pb.set_style(
                    ProgressStyle::with_template(
                        "{spinner:.green} {prefix:>20} [{bar:30.cyan/blue}] {pos}/{len} {msg}",
                    )
                    .unwrap()
                    .progress_chars("=>-"),
                );
                pb.set_prefix(progress.dex_name.clone());
                pb
            });

            match &progress.phase {
                LoadPhase::FetchingPools => {
                    pb.set_message("Fetching pools...");
                    pb.enable_steady_tick(Duration::from_millis(120));
                }
                LoadPhase::Deserializing { done, total } => {
                    pb.set_length(*total as u64);
                    pb.set_position(*done as u64);
                    pb.set_message(format!("{total} pools discovered"));
                }
                LoadPhase::FetchingBalances { done, total } => {
                    pb.set_length(*total as u64);
                    pb.set_position(*done as u64);
                    pb.set_message("Fetching balances...");
                }
                LoadPhase::BuildingMarkets { done, total } => {
                    pb.set_length(*total as u64);
                    pb.set_position(*done as u64);
                    pb.set_message("Building markets...");
                }
                LoadPhase::Complete { pool_count } => {
                    pb.set_length(*pool_count as u64);
                    pb.set_position(*pool_count as u64);
                    pb.finish_with_message(format!("{pool_count} pools loaded"));
                }
                LoadPhase::Error(msg) => {
                    pb.finish_with_message(format!("Error: {msg}"));
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Interactive REPL
// ---------------------------------------------------------------------------

pub async fn run_repl(index: &PoolIndex, stats: &mut StatsCollector, sol_usd_price: Option<f64>) {
    println!("\nThunder Aggregator ready. Type 'help' for commands.\n");

    let mut rl = DefaultEditor::new().expect("Failed to create editor");

    loop {
        let readline = rl.readline("thunder> ");
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);

                let parts: Vec<&str> = line.split_whitespace().collect();
                match parts[0] {
                    "help" => print_help(),
                    "price" => cmd_price(index, &parts, sol_usd_price),
                    "route" | "quote" => cmd_route(index, &parts),
                    "stats" => cmd_stats(index, stats),
                    "exit" | "quit" => break,
                    other => println!("Unknown command: {other}. Type 'help' for commands."),
                }
            }
            // Ctrl-C or Ctrl-D
            Err(_) => break,
        }
    }
}

fn print_help() {
    println!("Commands:");
    println!("  price <mint>                     - Token price in SOL and USD");
    println!("  route <from> <to> <amount>       - Find best route and simulate");
    println!("  stats                            - Pool and system statistics");
    println!("  help                             - Show this help");
    println!("  exit                             - Exit");
}

/// Parse a mint from user input, accepting base58 pubkeys or "SOL" shorthand.
fn parse_mint(s: &str) -> Option<Pubkey> {
    if s.eq_ignore_ascii_case("SOL") {
        return Some(Pubkey::from_str_const(WSOL));
    }
    Pubkey::from_str(s).ok()
}

fn cmd_price(index: &PoolIndex, parts: &[&str], sol_usd: Option<f64>) {
    if parts.len() < 2 {
        println!("Usage: price <mint_address>");
        return;
    }

    let mint = match parse_mint(parts[1]) {
        Some(m) => m,
        None => {
            println!("Invalid mint address: {}", parts[1]);
            return;
        }
    };

    match get_token_price(index, &mint, sol_usd) {
        Ok(tp) => {
            let mut has_price = false;
            if let Some(sol) = tp.price_sol {
                println!("  Price (SOL): {sol:.10}");
                has_price = true;
            }
            if let Some(usd) = tp.price_usd {
                println!("  Price (USD): ${usd:.6}");
                has_price = true;
            }
            if !has_price {
                println!("  No price data available for this token");
            }
        }
        Err(e) => println!("  Error: {e}"),
    }
}

fn cmd_route(index: &PoolIndex, parts: &[&str]) {
    if parts.len() < 4 {
        println!("Usage: route <from_mint> <to_mint> <amount_raw>");
        return;
    }

    let from_mint = match parse_mint(parts[1]) {
        Some(m) => m,
        None => {
            println!("Invalid from mint: {}", parts[1]);
            return;
        }
    };

    let to_mint = match parse_mint(parts[2]) {
        Some(m) => m,
        None => {
            println!("Invalid to mint: {}", parts[2]);
            return;
        }
    };

    let amount: u64 = if parts[3].contains('.') {
        match parts[3].parse::<f64>() {
            Ok(human) => {
                let decimals = infer_mint_decimals(&from_mint);
                (human * 10f64.powi(decimals as i32)) as u64
            }
            Err(_) => {
                println!("Invalid amount: {}", parts[3]);
                return;
            }
        }
    } else {
        match parts[3].parse() {
            Ok(a) => a,
            Err(_) => {
                println!("Invalid amount: {}", parts[3]);
                return;
            }
        }
    };

    let router = Router::new(index, 4);
    match router.find_routes(from_mint, to_mint, amount, 5) {
        Ok(quote) => {
            if quote.routes.is_empty() {
                println!("  No routes found");
                return;
            }
            for (i, route) in quote.routes.iter().enumerate() {
                let hop_s = if route.hops.len() == 1 { "" } else { "s" };
                println!(
                    "  Route {} ({} hop{hop_s}):",
                    i + 1,
                    route.hops.len()
                );
                for (j, hop) in route.hops.iter().enumerate() {
                    println!(
                        "    Hop {}: {} -> {} via {} ({}) | {} -> {} | impact: {:.2}%",
                        j + 1,
                        trunc(&hop.input_mint.to_string()),
                        trunc(&hop.output_mint.to_string()),
                        trunc(&hop.pool_address),
                        hop.dex_name,
                        fmt_amount(hop.input_amount, &hop.input_mint),
                        fmt_amount(hop.output_amount, &hop.output_mint),
                        hop.price_impact_bps as f64 / 100.0,
                    );
                }
                println!(
                    "    Output: {} | Total impact: {:.2}%",
                    fmt_amount(route.output_amount, &route.output_mint),
                    route.price_impact_bps as f64 / 100.0,
                );
                if i == 0 {
                    println!("    ^ Best route");
                }
                println!();
            }
        }
        Err(e) => println!("  Error finding routes: {e}"),
    }
}

fn cmd_stats(index: &PoolIndex, stats: &mut StatsCollector) {
    let s = stats.collect(index);
    println!("  Pools:");
    for (dex, count) in &s.pools_per_dex {
        println!("    {dex:>25}: {count:>8}");
    }
    println!("    {:>25}: {:>8}", "TOTAL", s.total_pools);
    println!("  Unique tokens: {}", s.unique_tokens);
    println!("  Memory: {:.1} MB", s.memory_mb);
    println!("  CPU: {:.1}%", s.cpu_percent);
    println!("  Uptime: {}s", s.uptime_secs);
}


/// Format a raw token amount as human-readable using inferred decimals.
fn fmt_amount(raw: u64, mint: &Pubkey) -> String {
    let decimals = infer_mint_decimals(mint);
    let divisor = 10f64.powi(decimals as i32);
    let human = raw as f64 / divisor;
    if human >= 1_000_000.0 {
        format!("{:.2}M", human / 1_000_000.0)
    } else if human >= 1_000.0 {
        format!("{:.2}K", human / 1_000.0)
    } else if human >= 1.0 {
        format!("{:.4}", human)
    } else {
        format!("{:.8}", human)
    }
}

/// Truncate a pubkey or address string for display.
fn trunc(s: &str) -> String {
    if s.len() > 12 {
        format!("{}..{}", &s[..6], &s[s.len()-4..])
    } else {
        s.to_string()
    }
}