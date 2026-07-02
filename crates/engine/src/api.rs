use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Json;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;

use thunder_aggregator::pool_index::PoolIndex;
use thunder_aggregator::price;
use thunder_aggregator::router::Router;
use thunder_core::WSOL;

use crate::account_store::AccountStore;
use crate::pool_registry::PoolRegistry;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct AppState {
    pub store: Arc<AccountStore>,
    pub pool_index: Arc<PoolIndex>,
    pub registry: Arc<RwLock<PoolRegistry>>,
    pub sol_usd_price: RwLock<Option<f64>>,
    pub start_time: Instant,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn create_router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/quote", get(handle_quote))
        .route("/price", get(handle_price))
        .route("/health", get(handle_health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// GET /quote
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteParams {
    input_mint: String,
    output_mint: String,
    amount: u64,
    slippage_bps: Option<u64>,
    max_hops: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    input_mint: String,
    output_mint: String,
    amount: String,
    slippage_bps: u64,
    routes: Vec<RouteJson>,
    time_taken_ms: u128,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteJson {
    hops: Vec<HopJson>,
    output_amount: String,
    price_impact_bps: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HopJson {
    pool_address: String,
    dex_name: String,
    input_mint: String,
    output_mint: String,
    input_amount: String,
    output_amount: String,
}

/// Parse a mint string, accepting "SOL" as shorthand for WSOL.
fn parse_mint(s: &str) -> Result<Pubkey, String> {
    if s.eq_ignore_ascii_case("SOL") {
        Ok(Pubkey::from_str_const(WSOL))
    } else {
        Pubkey::from_str(s).map_err(|e| format!("invalid mint: {e}"))
    }
}

async fn handle_quote(
    State(state): State<Arc<AppState>>,
    Query(params): Query<QuoteParams>,
) -> Result<Json<QuoteResponse>, (StatusCode, String)> {
    let input_mint = parse_mint(&params.input_mint).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let output_mint = parse_mint(&params.output_mint).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let slippage_bps = params.slippage_bps.unwrap_or(50);
    let max_hops = params.max_hops.unwrap_or(2).min(4);

    let start = Instant::now();

    let swappable = state.registry.read().await.swappable_set();
    let router = Router::new(&state.pool_index, max_hops)
        .with_swappable_set(swappable)
        .with_live_data(state.store.as_ref());
    let quote = router
        .find_routes(input_mint, output_mint, params.amount, 5)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("routing error: {e}")))?;

    let routes: Vec<RouteJson> = quote
        .routes
        .into_iter()
        .map(|route| RouteJson {
            output_amount: route.output_amount.to_string(),
            price_impact_bps: route.price_impact_bps,
            hops: route
                .hops
                .iter()
                .map(|hop| HopJson {
                    pool_address: hop.pool_address.clone(),
                    dex_name: hop.dex_name.clone(),
                    input_mint: hop.input_mint.to_string(),
                    output_mint: hop.output_mint.to_string(),
                    input_amount: hop.input_amount.to_string(),
                    output_amount: hop.output_amount.to_string(),
                })
                .collect(),
        })
        .collect();

    let elapsed = start.elapsed();

    Ok(Json(QuoteResponse {
        input_mint: input_mint.to_string(),
        output_mint: output_mint.to_string(),
        amount: params.amount.to_string(),
        slippage_bps,
        routes,
        time_taken_ms: elapsed.as_millis(),
    }))
}

// ---------------------------------------------------------------------------
// GET /price
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PriceParams {
    mint: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PriceResponse {
    mint: String,
    price_sol: Option<f64>,
    price_usd: Option<f64>,
}

async fn handle_price(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PriceParams>,
) -> Result<Json<PriceResponse>, (StatusCode, String)> {
    let mint = parse_mint(&params.mint).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let sol_usd = *state.sol_usd_price.read().await;

    let token_price = price::get_token_price(&state.pool_index, &mint, sol_usd)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("price error: {e}")))?;

    Ok(Json(PriceResponse {
        mint: mint.to_string(),
        price_sol: token_price.price_sol,
        price_usd: token_price.price_usd,
    }))
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    pools: usize,
    swappable_pools: usize,
    accounts_in_store: u64,
    last_slot: u64,
    uptime_seconds: u64,
}

async fn handle_health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let registry = state.registry.read().await;
    Json(HealthResponse {
        status: "ok",
        pools: registry.pool_count(),
        swappable_pools: registry.swappable_count(),
        accounts_in_store: state.store.len(),
        last_slot: state.store.last_slot(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
    })
}
