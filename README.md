# solroute

A Rust DEX **routing + execution** engine for Solana. Loads pools across 6 DEX
protocols, finds optimal multi-hop swap routes with live on-chain pricing, and
builds/simulates/lands the resulting swaps as versioned transactions — all
without external routing or price APIs.

## Features

- **Routing (6 DEXs)** — Raydium AMM V4, Raydium CLMM, Meteora DAMM V1/V2,
  Meteora DLMM, Pumpfun AMM. 2M+ pools loadable.
- **Multi-hop routing** (1–4 hops) with hub-based + bidirectional neighbor
  search, a **canonical-edge cache** (deepest-liquidity pool per pair for
  intermediate hops), and a **reverse-reachability prune** for 3/4-hop search.
- **Data-driven hubs** — top-K mints by pool degree, unioned with settlement
  seeds (WSOL/USDC/USDT).
- **Execution (3 DEXs)** — Raydium AMM V4, Meteora DAMM V2, Pumpfun AMM
  (PumpSwap). Builds swap instructions from parsed pool state, resolves the real
  token program (SPL Token / Token-2022), signs, and submits.
- **v0 transactions + Address Lookup Tables** — multi-hop routes that exceed the
  1232-byte legacy limit are compressed via an ALT (on-the-fly creation or a
  pre-warmed table) and landed as v0 transactions.
- **Transaction simulation** — validate any route against live state with zero
  SOL and no signature (`sigVerify=false` + `replaceRecentBlockhash=true`).
- **On-chain SOL/USD pricing** from Raydium CLMM `sqrt_price_x64` — no oracles.
- **Real-time streaming** via Yellowstone gRPC (Geyser) for live pool updates.
- **Instant startup** from a binary pool cache (~6s vs ~4min from RPC).
- **Pure DEX crates** — no I/O, no async, just math. Each implements the
  `Market` trait independently.

## Coverage: routing vs execution

| DEX | Routing | Execution |
|---|:---:|:---:|
| Raydium AMM V4 | ✅ | ✅ |
| Meteora DAMM V2 | ✅ | ✅ |
| Pumpfun AMM (PumpSwap) | ✅ | ✅ |
| Raydium CLMM | ✅ | ❌ |
| Meteora DAMM V1 | ✅ | ❌ |
| Meteora DLMM | ✅ | ❌ |

CLMM / DLMM / DAMM V1 are routable/quotable but not yet executable (tick-array /
bin-array / dynamic-vault swap building is unimplemented).

## Quick Start

### Run the Engine (HTTP API)

Persistent service that keeps all pool data in memory and serves quotes.

```bash
RPC_URL="https://your-rpc-endpoint.com" cargo run --release --bin solroute-engine

# In another terminal:
curl "http://localhost:8080/health"
curl "http://localhost:8080/price?mint=SOL"
curl "http://localhost:8080/quote?inputMint=SOL&outputMint=<mint>&amount=100000000&maxHops=2"
```

### Aggregator CLI

```bash
cargo build --release -p solroute-aggregator
RPC_URL="https://your-rpc-endpoint.com" ./target/release/solroute-cli
```

### Testing tools (no engine required)

```bash
# Build a bounded sample cache from a handful of pools (needs RPC once)
RPC_URL=... cargo run --release -p solroute-aggregator --bin sample-cache -- <addrs.json> pools.cache

# Time find_routes over the cache — pure in-memory, zero RPC
cargo run --release -p solroute-aggregator --bin bench -- pools.cache 2000

# Simulate route execution against live state — no SOL, no signature
RPC_URL=... cargo run --release -p solroute-aggregator --bin simulate -- pools.cache [payer_pubkey]

# Land a real single-hop swap via v0 + ALT (spends SOL — burner wallet only)
RPC_URL=... SIGNER_KEY=<base58 secret> cargo run --release -p solroute-aggregator --bin land -- pools.cache

# Build + land a real multi-hop, multi-protocol route via v0 + ALT
RPC_URL=... SIGNER_KEY=<base58 secret> cargo run --release -p solroute-aggregator --bin multihop -- pools.cache
```

## Architecture

```
solroute-core              Market trait, shared types, constants, AccountDataProvider
    ^
    +-- raydium-amm-v4    Constant product AMM
    +-- raydium-clmm      Concentrated liquidity
    +-- meteora-damm      Dynamic AMM V1 + V2
    +-- meteora-dlmm      Dynamic liquidity bins
    +-- pumpfun-amm       Bonding curve / AMM

solroute-aggregator        Pool loading, routing, pricing, caching, CLI,
                          route->executor bridge (execute.rs)
solroute-executor          Swap instruction builders, ATA/wrap helpers, ALT,
                          v0 tx assembly + simulate + submit
solroute-engine            Persistent service: AccountStore + gRPC streaming + HTTP API
solroute            Root crate: re-exports all DEX crates; solroute-engine bin
```

### Routing flow

```
GET /quote -> Router.find_routes
   1-hop direct + 2-hop (hubs + neighbor fwd/rev)
   + 3-hop (hub-hub, neighbor-hub, reverse-reachability pruned)
   + 4-hop (neighbor -> hub -> neighbor)
   intermediate legs use the canonical-edge cache; the FINAL leg full-scans
   every pool on the pair (deepest liquidity != best price for a given size)
   -> routes ranked by output amount
```

### Execution flow

```
Route -> execute::build_hop_instructions        (per hop)
   look up pool in PoolIndex, decode CachedPool, dispatch on dex_name
   resolve real token program (SPL Token / Token-2022) per mint
   -> executor::{raydium_amm_v4,meteora_damm_v2,pumpswap}::build_swap
      (+ ATA create / WSOL wrap / close as needed)

execute::execute_route
   compute-budget ixs + per-hop slippage floor
   -> fits under 1232 bytes?  v0 tx, no ALT
      else pre-warmed ALT supplied?  use it
      else create + extend an ALT on-chain (finalized slot, wait a slot)
   -> sign v0 tx -> send (preflight-protected)
```

### Engine startup

```
1. Load cache              ~6s      pools.cache -> PoolIndex
2. validate_from_cache     instant  cached vault balances -> swappable set
3. warm canonical + hubs   ~ms      router structures ready
4. Start HTTP server       instant  /quote works immediately
5. gRPC streaming          bg       live account updates + vault re-validation
6. Vault fetch             bg       4M+ accounts, 100 concurrent batches
7. SOL/USD price refresh   bg       every 15s
```

### Project Structure

```
solroute/
├── bin/engine.rs                     Engine binary entry point
├── crates/
│   ├── core/                         Market trait, AccountDataProvider, constants
│   ├── raydium-amm-v4/               RaydiumAMMV4 + market
│   ├── raydium-clmm/                 CLMM pool + market + tick arrays
│   ├── meteora-damm/                 DAMM V1 + V2 markets + models
│   ├── meteora-dlmm/                 DLMM pool + market
│   ├── pumpfun-amm/                  Pumpfun AMM pool + market
│   ├── aggregator/                   Loading, routing, pricing, caching, CLI, execute bridge
│   │   └── src/
│   │       ├── loader.rs             RPC pool loading (+ bounded sample fetch)
│   │       ├── cache.rs              Disk cache + PDA extraction
│   │       ├── router.rs             Multi-hop routing (canonical cache, hubs, prune)
│   │       ├── pool_index.rs         Token-pair graph + canonical-edge + hub ranking
│   │       ├── price.rs              On-chain pricing (CLMM sqrt_price)
│   │       ├── execute.rs            Route -> executor bridge (build / simulate / land)
│   │       ├── cli.rs                Progress bars + REPL
│   │       └── bin/                  sample_cache, bench, simulate, land, multihop
│   ├── engine/                       Persistent service (library)
│   │   └── src/
│   │       ├── account_store.rs      DashMap store, implements AccountDataProvider
│   │       ├── pool_registry.rs      Swappable validation, vault->pool index
│   │       ├── cold_start.rs         Background vault/tick/bin-array fetch
│   │       ├── streaming.rs          Yellowstone gRPC live updates
│   │       └── api.rs                Axum HTTP: /quote, /price, /health
│   └── executor/                     Swap execution
│       └── src/
│           ├── meteora_damm_v2.rs    swap2 builder (14-account layout)
│           ├── pumpswap.rs           Pump AMM buy/sell (full account fidelity)
│           ├── raydium_amm_v4.rs     swap_base_in (fetches Serum market from RPC)
│           ├── ata.rs                ATA create / WSOL wrap / close
│           ├── alt.rs                Address Lookup Table create/extend/fetch
│           ├── submit.rs             Compute budget, v0 tx, sign, simulate, send
│           └── types.rs             SwapLeg, SwapOptions
└── tests/                            Live gRPC/RPC integration tests
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC endpoint |
| `CACHE_PATH` | `pools.cache` | Pool cache file location |
| `CACHE_MAX_AGE` | `3600` | Max cache age (seconds) before RPC reload |
| `GEYSER_ENDPOINT` | (none) | Yellowstone gRPC endpoint for live streaming |
| `GEYSER_TOKEN` | (none) | Yellowstone gRPC auth token |
| `PORT` | `8080` | Engine HTTP API port |
| `SIGNER_KEY` | (none) | Base58 secret key for the `land` / `multihop` bins |

## Using the DEX crates as a library

The DEX crates are pure — no RPC, no async, no I/O:

```rust
use solroute_core::{Market, SwapDirection};

let pool: raydium_amm_v4::RaydiumAMMV4 = borsh::from_slice(&account_data)?;
let market = raydium_amm_v4::RaydiumAmmV4Market::new(pool, address, quote_bal, base_bal);

let price = market.current_price()?;
let output = market.calculate_output(1_000_000_000, SwapDirection::Buy)?;
```

Building a swap instruction from parsed pool state:

```rust
use solroute_executor::{meteora_damm_v2::{self, DammV2Accounts}, SwapLeg, SwapOptions};

let ixs = meteora_damm_v2::build_swap(&accounts, &leg, None, &SwapOptions::default())?;
```

## Development

```bash
cargo check                    # Type-check workspace
cargo build --workspace        # Build all crates + bins
cargo test --workspace --lib   # Unit tests (router + executor)

cargo build --release -p solroute-aggregator     # Aggregator CLI + tools
cargo build --release --bin solroute-engine       # Engine
```

## Notes & limitations

- **Multi-hop amount chaining** uses the router's quoted per-hop amounts with a
  safety haircut on intermediate inputs (a hop can only spend what the previous
  hop actually produced on-chain); a production version would quote the whole
  route atomically.
- **No SWQoS/Jito fan-out** — swaps submit via plain RPC (preflight-protected).
  Callers wanting MEV lanes can serialize the signed tx and forward it.
- Execution builders are ported from FnZero `sol-trade-sdk` (MIT), adapted to
  feed from solroute's own pool structs.

## References

- [Raydium AMM](https://github.com/raydium-io/raydium-amm)
- [Raydium CLMM](https://github.com/raydium-io/raydium-clmm)
- [Meteora DAMM V1](https://github.com/MeteoraAg/damm-v1-sdk)
- [Meteora DAMM V2](https://github.com/MeteoraAg/damm-v2)
- [Meteora DLMM](https://github.com/MeteoraAg/dlmm-sdk)
- [Pumpfun AMM](https://solscan.io/account/pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA)

## License

MIT
