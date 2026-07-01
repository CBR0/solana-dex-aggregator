//! Token price discovery — fully on-chain, no external APIs.
//!
//! SOL/USD derived from the highest-liquidity Raydium CLMM SOL/USDC pool's
//! `sqrt_price_x64` field. Per-token prices from the loaded pool index.

use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use thunder_core::{GenericError, USDC, WSOL};

use crate::pool_index::PoolIndex;
use crate::types::TokenPrice;

/// Minimum combined vault balance for a pool to be used in pricing.
const MIN_PRICING_LIQUIDITY: u64 = 10_000_000_000; // ~10 SOL equivalent

/// Raydium CLMM program.
const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

// Byte offsets in a Raydium CLMM pool account (1544 bytes, 8-byte Anchor disc).
const CLMM_MINT_0_OFFSET: usize = 73;
const CLMM_MINT_1_OFFSET: usize = 105;
const CLMM_DECIMALS_0_OFFSET: usize = 233;
const CLMM_DECIMALS_1_OFFSET: usize = 234;
const CLMM_LIQUIDITY_OFFSET: usize = 237;   // u128, 16 bytes
const CLMM_SQRT_PRICE_OFFSET: usize = 253;  // u128, 16 bytes

/// Get the price of a token in SOL and USD.
///
/// `sol_usd` is the pre-fetched SOL/USD price (from on-chain CLMM oracle).
pub fn get_token_price(
    index: &PoolIndex,
    mint: &Pubkey,
    sol_usd: Option<f64>,
) -> Result<TokenPrice, GenericError> {
    let wsol = Pubkey::from_str_const(WSOL);
    let usdc = Pubkey::from_str_const(USDC);

    if *mint == wsol {
        return Ok(TokenPrice {
            mint: *mint,
            price_sol: None,
            price_usd: sol_usd,
        });
    }

    let price_sol = best_pool_price(index, mint, &wsol);

    let price_usd = if let Some(direct_usd) = best_pool_price(index, mint, &usdc) {
        Some(direct_usd)
    } else if let Some(sol_price) = &price_sol {
        sol_usd.map(|usd| sol_price * usd)
    } else {
        None
    };

    Ok(TokenPrice {
        mint: *mint,
        price_sol,
        price_usd,
    })
}

/// Fetch SOL/USD price on-chain from the highest-liquidity Raydium CLMM
/// SOL/USDC pool's `sqrt_price_x64`.
///
/// Queries `getProgramAccounts` for CLMM pools with WSOL + USDC mints,
/// picks the one with the most liquidity, and converts its sqrt_price to
/// a human-readable USDC-per-SOL price. Single RPC call + one getAccountInfo
/// per candidate pool.
pub async fn fetch_sol_usd_onchain(rpc: &RpcClient) -> Option<f64> {
    let program = Pubkey::from_str_const(RAYDIUM_CLMM_PROGRAM);
    let wsol = Pubkey::from_str_const(WSOL);
    let usdc = Pubkey::from_str_const(USDC);

    // Discover SOL/USDC CLMM pools (WSOL at mint_0, USDC at mint_1).
    let config = solana_rpc_client_api::config::RpcProgramAccountsConfig {
        filters: Some(vec![
            solana_rpc_client_api::filter::RpcFilterType::DataSize(1544),
            solana_rpc_client_api::filter::RpcFilterType::Memcmp(
                solana_rpc_client_api::filter::Memcmp::new_raw_bytes(
                    CLMM_MINT_0_OFFSET,
                    wsol.to_bytes().to_vec(),
                ),
            ),
            solana_rpc_client_api::filter::RpcFilterType::Memcmp(
                solana_rpc_client_api::filter::Memcmp::new_raw_bytes(
                    CLMM_MINT_1_OFFSET,
                    usdc.to_bytes().to_vec(),
                ),
            ),
        ]),
        account_config: solana_rpc_client_api::config::RpcAccountInfoConfig {
            encoding: Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
            commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };

    #[allow(deprecated)]
    let accounts = rpc
        .get_program_accounts_with_config(&program, config)
        .await
        .ok()?;

    // Pick the pool with the highest liquidity.
    let mut best_price: Option<f64> = None;
    let mut best_liquidity: u128 = 0;

    for (_pubkey, account) in &accounts {
        let data = &account.data;
        if data.len() < CLMM_SQRT_PRICE_OFFSET + 16 {
            continue;
        }

        let liquidity = u128::from_le_bytes(
            data[CLMM_LIQUIDITY_OFFSET..CLMM_LIQUIDITY_OFFSET + 16]
                .try_into()
                .ok()?,
        );
        if liquidity <= best_liquidity {
            continue;
        }

        let sqrt_price_x64 = u128::from_le_bytes(
            data[CLMM_SQRT_PRICE_OFFSET..CLMM_SQRT_PRICE_OFFSET + 16]
                .try_into()
                .ok()?,
        );
        let dec0 = data[CLMM_DECIMALS_0_OFFSET]; // WSOL = 9
        let dec1 = data[CLMM_DECIMALS_1_OFFSET]; // USDC = 6

        let sqrt_price = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
        let raw_price = sqrt_price * sqrt_price; // token_1_raw per token_0_raw
        let human_price = raw_price * 10f64.powi(dec0 as i32 - dec1 as i32);
        // human_price = USDC per SOL

        if human_price.is_finite() && human_price > 0.0 {
            best_price = Some(human_price);
            best_liquidity = liquidity;
        }
    }

    best_price
}

/// SOL price in USD from the highest-liquidity USDC/WSOL pool in the index (fallback).
pub fn get_sol_usd_price(index: &PoolIndex) -> Option<f64> {
    let wsol = Pubkey::from_str_const(WSOL);
    let usdc = Pubkey::from_str_const(USDC);

    let pool_addrs = index.direct_pools(&wsol, &usdc);
    let mut best_price: Option<f64> = None;
    let mut best_liquidity = 0u64;

    for addr in pool_addrs {
        let Some(pool) = index.get_pool(&addr) else { continue };
        let Ok(fin) = pool.market.financials() else { continue };
        let liquidity = fin.quote_balance.saturating_add(fin.base_balance);
        if liquidity < MIN_PRICING_LIQUIDITY || liquidity <= best_liquidity {
            continue;
        }
        let Ok(meta) = pool.market.metadata() else { continue };
        let Ok(raw_price) = pool.market.current_price() else { continue };

        let price = if meta.quote_mint == usdc { raw_price } else { 1.0 / raw_price };

        if !price.is_finite() || price <= 0.0 {
            continue;
        }

        best_price = Some(price);
        best_liquidity = liquidity;
    }

    best_price
}

/// Price of `mint_a` denominated in `mint_b`, from the highest-liquidity
/// direct pool connecting the two mints.
fn best_pool_price(index: &PoolIndex, mint_a: &Pubkey, mint_b: &Pubkey) -> Option<f64> {
    let pool_addrs = index.direct_pools(mint_a, mint_b);
    let mut best_price: Option<f64> = None;
    let mut best_liquidity = 0u64;

    for addr in pool_addrs {
        let Some(pool) = index.get_pool(&addr) else { continue };
        let Ok(fin) = pool.market.financials() else { continue };
        let liquidity = fin.quote_balance.saturating_add(fin.base_balance);
        if liquidity < MIN_PRICING_LIQUIDITY || liquidity <= best_liquidity {
            continue;
        }
        let Ok(meta) = pool.market.metadata() else { continue };
        let Ok(raw_price) = pool.market.current_price() else { continue };

        let adjusted = if meta.base_mint == *mint_a { raw_price } else { 1.0 / raw_price };

        if !adjusted.is_finite() || adjusted <= 0.0 {
            continue;
        }

        best_price = Some(adjusted);
        best_liquidity = liquidity;
    }

    best_price
}
