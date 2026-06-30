# Solana Thunder

A Rust DEX aggregator for Solana. Loads all pools across 6 DEX protocols, finds optimal multi-hop swap routes, and provides real-time pricing -- all without external APIs.

## Features

- **2M+ pools loaded** across 6 DEXs (Raydium V4, Raydium CLMM, Meteora DAMM V1/V2, Meteora DLMM, Pumpfun AMM)
- **Multi-hop routing** (1-4 hops, default 2) with hub-based and bidirectional neighbor search
- **On-chain SOL/USD pricing** from Raydium CLMM `sqrt_price_x64` -- no external APIs
- **Real-time streaming** via Yellowstone gRPC (Geyser) for live pool state updates
- **Instant startup** from binary pool cache (~6s vs ~4min from RPC)
- **Pure DEX crates** -- no I/O, no async, just math. Each crate implements the `Market` trait independently

## Quick Start

### Run the Engine (HTTP API)

The engine is a persistent service that keeps all pool data in memory and serves an HTTP API.

```bash
# Start the engine (loads pools, fetches accounts, starts API on port 8080)
RPC_URL="https://your-rpc-endpoint.com" cargo run --release --bin thunder-engine

# In another terminal:
curl "http://localhost:8080/health"
curl "http://localhost:8080/price?mint=SOL"
curl "http://localhost:8080/quote?inputMint=SOL&outputMint=6p6xgHyF7AeE6TZkSmFsko444wqoP15icUSqi2jfGiPN&amount=100000000&maxHops=2"
```

### Run the Aggregator CLI

```bash
cargo build --release -p thunder-aggregator

# First run loads all pools from RPC and saves cache
RPC_URL="https://your-rpc-endpoint.com" ./target/release/thunder-agg

# Subsequent runs load from cache (~6s)
RPC_URL="https://your-rpc-endpoint.com" ./target/release/thunder-agg
```

## Architecture

```
thunder-core              Market trait, shared types, constants
    ^
    |
    +-- raydium-amm-v4    Constant product AMM
    +-- raydium-clmm      Concentrated liquidity
    +-- meteora-damm      Dynamic AMM V1 + V2
    +-- meteora-dlmm      Dynamic liquidity bins
    +-- pumpfun-amm       Bonding curve

thunder-aggregator        Pool loading, routing, pricing, caching, CLI
thunder-engine            Persistent service: AccountStore + gRPC streaming + HTTP API
solana-thunder            Root crate: re-exports all DEX crates
```

### Engine Service Flow

```
Yellowstone gRPC  --->  AccountStore (DashMap, implements AccountDataProvider)
                              |
                              +--- PoolRegistry (swappable validation, vault-to-pool index)
                              |
                              v
                    HTTP API:  GET /quote  (?maxHops=2&slippageBps=50)
                               GET /price
                               GET /health
```

### Engine Startup

```
1. Load cache              ~6s   pools.cache -> PoolIndex
2. validate_from_cache     instant  cached vault balances -> swappable set
3. Start HTTP server       instant  /quote works immediately
4. gRPC streaming          background  live account updates + vault re-validation
5. Vault fetch             background  4M+ accounts, 100 concurrent batches
6. SOL/USD price refresh   background  every 15s
```

### Project Structure

```
solana-thunder/
+-- bin/
|   +-- engine.rs                     Engine binary entry point
+-- crates/
|   +-- core/                         Market trait, AccountDataProvider, constants
|   +-- raydium-amm-v4/               RaydiumAMMV4 + RaydiumAmmV4Market
|   +-- raydium-clmm/                 RaydiumCLMMPool + RaydiumClmmMarket + tick arrays
|   +-- meteora-damm/                 MeteoraDAMMMarket + V2Market + models
|   +-- meteora-dlmm/                 MeteoraDLMMPool + MeteoraDlmmMarket
|   +-- pumpfun-amm/                  PumpfunAmmPool + PumpfunAmmMarket
|   +-- aggregator/                   Pool loading, routing, pricing, caching, CLI
|   |   +-- src/
|   |       +-- loader.rs             RPC pool loading (all DEXs)
|   |       +-- cache.rs              Disk cache + PDA extraction
|   |       +-- router.rs             Multi-hop routing (live data, pre-resolved mints)
|   |       +-- price.rs              On-chain pricing (CLMM sqrt_price)
|   |       +-- pool_index.rs         In-memory token-pair graph
|   |       +-- cli.rs                Progress bars + REPL
|   +-- engine/                       Persistent service (library)
|       +-- src/
|           +-- account_store.rs      DashMap store, implements AccountDataProvider
|           +-- pool_registry.rs      Swappable validation, vault-to-pool reverse index
|           +-- cold_start.rs         Background vault fetch, tick arrays, bin arrays
|           +-- streaming.rs          Yellowstone gRPC: live updates + vault re-validation
|           +-- api.rs                Axum HTTP: /quote, /price, /health
+-- tests/
    +-- trade_stream.rs               Live DEX swap streaming (gRPC)
    +-- creation_stream.rs            Token + pool creation streaming
    +-- pool_financials.rs            Pool financial analysis
    +-- validate_prices.rs            Price validation tests
    +-- helpers/
        +-- mod.rs                    Shared Geyser test utilities
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC endpoint |
| `CACHE_PATH` | `pools.cache` | Pool cache file location |
| `CACHE_MAX_AGE` | `3600` | Max cache age (seconds) before RPC reload |
| `PRIVATE_KEY` | (none) | Base58 keypair (in `.env`, never committed) |
| `GEYSER_ENDPOINT` | (none) | Yellowstone gRPC endpoint for live streaming |
| `GEYSER_TOKEN` | (none) | Yellowstone gRPC auth token |
| `PORT` | `8080` | Thunder Engine HTTP API port |

## Using as a Library

The DEX crates are pure -- no RPC, no async, no I/O:

```rust
use thunder_core::{Market, SwapDirection};

let pool: raydium_amm_v4::RaydiumAMMV4 = borsh::from_slice(&account_data)?;
let market = raydium_amm_v4::RaydiumAmmV4Market::new(pool, address, quote_bal, base_bal);

let price = market.current_price()?;
let output = market.calculate_output(1_000_000_000, SwapDirection::Buy)?;
```

## Development

```bash
cargo check                    # Type-check workspace
cargo build                    # Build all workspace crates
cargo test --workspace --lib   # Unit tests

cargo build --release -p thunder-aggregator    # Build aggregator CLI
cargo build --release --bin thunder-engine      # Build engine
```

## References

- [Raydium AMM](https://github.com/raydium-io/raydium-amm)
- [Raydium CLMM](https://github.com/raydium-io/raydium-clmm)
- [Meteora DAMM V1](https://github.com/MeteoraAg/damm-v1-sdk)
- [Meteora DAMM V2](https://github.com/MeteoraAg/damm-v2)
- [Meteora DLMM](https://github.com/MeteoraAg/dlmm-sdk)
- [Pumpfun AMM](https://solscan.io/account/pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA)

## License

MIT
