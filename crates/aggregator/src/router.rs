//! Multi-hop route discovery: finds optimal swap paths between any two tokens.
//!
//! Searches 1-hop through 4-hop routes using hub mints and bidirectional
//! neighbor exploration. All candidate paths are simulated end-to-end with
//! the actual input amount, then ranked by output amount descending.

use std::collections::HashSet;
use std::sync::Arc;

use solana_pubkey::Pubkey;
use solroute_core::{AccountDataProvider, GenericError, SwapDirection, USDC, USDT, WSOL};

use crate::pool_index::PoolIndex;
use crate::types::{Quote, Route, RouteHop};

/// Settlement mints always treated as hubs, regardless of degree ranking.
/// These are where the bulk of liquidity settles; guaranteeing them guards
/// against a spammy long-tail mint out-ranking a real hub by raw pool count.
const SEED_HUBS: [&str; 3] = [WSOL, USDC, USDT];

/// Number of data-driven (top-degree) hubs for the full hub set (2-hop).
const HUB_COUNT: usize = 8;

/// Number of data-driven hubs for higher-hop routes (bounds search space).
const CORE_HUB_COUNT: usize = 4;

/// Max neighbors explored per side in bidirectional search.
const MAX_NEIGHBOR_CANDIDATES: usize = 50;

/// Minimum vault balance (raw units) for a pool to be routable.
const MIN_VAULT_BALANCE: u64 = 10_000_000; // 0.01 SOL

/// A hop may take at most this fraction (1/N) of the output-side reserve.
/// Beyond it the per-DEX single-bin / constant-product quote is unreliable — a
/// real swap that drains most of a pool crosses many bins/ticks and yields far
/// less. Thin, mispriced pools (esp. DLMM) otherwise quote a huge output off
/// their active-bin price and win `best_pool`'s max-output selection, inflating
/// routes by orders of magnitude. Trades exceeding this are dropped so the
/// router falls back to genuinely deep pools.
const MAX_OUTPUT_RESERVE_DIVISOR: u64 = 4; // ≤25% of the output-side reserve

/// A route may beat the best SURVIVING route with fewer hops by at most this
/// multiple. Adding a hop adds fees; a longer route massively out-quoting a
/// shorter one (phantom 3-hops were 100–30,000× the direct route) is
/// manufacturing value from a mispriced intermediate leg. Generous so a
/// genuinely better hub path around a dusty direct pool still survives.
/// Applied ascending by hop count so a rejected phantom 2-hop can never
/// legitimize a phantom 3-hop (no cascade).
const MAX_EXTRA_HOP_GAIN: u64 = 3;

pub struct Router<'a> {
    index: &'a PoolIndex,
    max_hops: usize,
    swappable_set: Option<Arc<HashSet<String>>>,
    live_data: Option<&'a dyn AccountDataProvider>,
}

impl<'a> Router<'a> {
    pub fn new(index: &'a PoolIndex, max_hops: usize) -> Self {
        Self {
            index,
            max_hops,
            swappable_set: None,
            live_data: None,
        }
    }

    /// Restrict routing to only the given pool addresses.
    pub fn with_swappable_set(mut self, set: Arc<HashSet<String>>) -> Self {
        self.swappable_set = Some(set);
        self
    }

    /// Provide live on-chain data for routing calculations.
    pub fn with_live_data(mut self, provider: &'a dyn AccountDataProvider) -> Self {
        self.live_data = Some(provider);
        self
    }

    /// Find the best routes from `input_mint` to `output_mint` for `amount_in`.
    ///
    /// Returns up to `max_routes` routes sorted by output amount descending.
    pub fn find_routes(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount_in: u64,
        max_routes: usize,
    ) -> Result<Quote, GenericError> {
        if input_mint == output_mint || amount_in == 0 {
            return Ok(Quote { routes: vec![] });
        }

        let mut candidates: Vec<Route> = Vec::new();

        let hubs = self.hub_set(HUB_COUNT, &input_mint, &output_mint);
        let core_hubs = self.hub_set(CORE_HUB_COUNT, &input_mint, &output_mint);

        // === 1-hop: direct pools ===
        if self.max_hops >= 1 {
            self.find_direct(&input_mint, &output_mint, amount_in, &mut candidates);
        }

        // === 2-hop ===
        if self.max_hops >= 2 {
            // Via hub mints
            for hub in &hubs {
                self.try_2hop(&input_mint, hub, &output_mint, amount_in, &mut candidates);
            }

            // Via neighbors of input_mint (forward search)
            self.neighbor_2hop_forward(
                &input_mint,
                &output_mint,
                amount_in,
                &hubs,
                &mut candidates,
            );

            // Via neighbors of output_mint (reverse search)
            self.neighbor_2hop_reverse(
                &input_mint,
                &output_mint,
                amount_in,
                &hubs,
                &mut candidates,
            );
        }

        // === 3-hop ===
        if self.max_hops >= 3 {
            // Hub-hub: input → hub1 → hub2 → output
            for (i, h1) in core_hubs.iter().enumerate() {
                for h2 in &core_hubs[i + 1..] {
                    self.try_3hop(&input_mint, h1, h2, &output_mint, amount_in, &mut candidates);
                    self.try_3hop(&input_mint, h2, h1, &output_mint, amount_in, &mut candidates);
                }
            }

            // Neighbor-hub: input → neighbor → hub → output
            // and: input → hub → neighbor → output
            self.neighbor_3hop(&input_mint, &output_mint, amount_in, &core_hubs, &mut candidates);
        }

        // === 4-hop ===
        if self.max_hops >= 4 {
            // input → neighbor_in → hub → neighbor_out → output
            self.neighbor_4hop(&input_mint, &output_mint, amount_in, &core_hubs, &mut candidates);
        }

        // Cross-hop sanity: a longer route may not beat the best shorter route
        // by more than MAX_EXTRA_HOP_GAIN×. Phantom multi-hops compound a
        // mispriced intermediate leg into outputs orders of magnitude above
        // the direct route; real hub detours don't. Filter ascending by hop
        // count so the reference is always a SURVIVING shorter route — a
        // rejected phantom 2-hop must not legitimize a phantom 3-hop. Only
        // binds once some shorter route exists to compare against.
        candidates.sort_unstable_by_key(|r| r.hops.len());
        let mut shorter_best: u64 = 0; // best among fully-processed shorter levels
        let mut level_best: u64 = 0;
        let mut current_len: usize = 0;
        candidates.retain(|r| {
            let len = r.hops.len();
            if len != current_len {
                shorter_best = shorter_best.max(level_best);
                level_best = 0;
                current_len = len;
            }
            if shorter_best > 0
                && r.output_amount > shorter_best.saturating_mul(MAX_EXTRA_HOP_GAIN)
            {
                return false;
            }
            level_best = level_best.max(r.output_amount);
            true
        });

        // Sort by output amount descending, truncate to max_routes.
        candidates.sort_unstable_by(|a, b| b.output_amount.cmp(&a.output_amount));
        candidates.truncate(max_routes);

        Ok(Quote { routes: candidates })
    }

    /// Build the routing hub set: the always-on settlement seeds unioned with
    /// the top-`k` highest-degree mints from the index, excluding the trade's
    /// own endpoints. Deduplicated, seeds first.
    fn hub_set(&self, k: usize, input: &Pubkey, output: &Pubkey) -> Vec<Pubkey> {
        let mut seen: HashSet<Pubkey> = HashSet::new();
        let mut hubs: Vec<Pubkey> = Vec::new();

        for s in SEED_HUBS {
            let h = Pubkey::from_str_const(s);
            if h != *input && h != *output && seen.insert(h) {
                hubs.push(h);
            }
        }
        for h in self.index.top_hubs(k) {
            if h != *input && h != *output && seen.insert(h) {
                hubs.push(h);
            }
        }
        hubs
    }

    // =====================================================================
    // Search strategies
    // =====================================================================

    /// All direct (1-hop) routes.
    fn find_direct(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        out: &mut Vec<Route>,
    ) {
        for addr in self.index.direct_pools(input, output) {
            if let Some(route) = simulate_path(self.index, &[(addr, *input, *output)], amount_in, self.swappable_set.as_deref(), self.live_data) {
                out.push(route);
            }
        }
    }

    /// 2-hop through a specific intermediate mint.
    /// Intermediate leg (input → mid) uses the canonical pool; the final leg
    /// (mid → output) full-scans every pool and keeps the best.
    fn try_2hop(
        &self,
        input: &Pubkey,
        mid: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        out: &mut Vec<Route>,
    ) {
        let leg2 = self.index.direct_pools(mid, output);
        if leg2.is_empty() {
            return;
        }

        let Some((a1, mid_amount)) = self.canonical_leg(input, mid, amount_in) else { return };
        let Some((a2, _)) = best_pool(self.index, &leg2, *mid, mid_amount, self.swappable_set.as_deref(), self.live_data) else { return };

        if let Some(route) = simulate_path(self.index, &[(a1, *input, *mid), (a2, *mid, *output)],
        amount_in, self.swappable_set.as_deref(), self.live_data) {
            out.push(route);
        }
    }

    /// Simulate an intermediate leg through the canonical (deepest-liquidity)
    /// pool for the `(from, to)` pair. Falls back to a full pool scan when the
    /// canonical pool is unavailable (filtered out as non-swappable, or zero
    /// output) so drifting swappability never drops an otherwise-viable leg.
    fn canonical_leg(&self, from: &Pubkey, to: &Pubkey, amount_in: u64) -> Option<(String, u64)> {
        if let Some(addr) = self.index.canonical_pool(from, to)
            && let Some(hop) = simulate_hop(self.index, addr, *from, amount_in, self.swappable_set.as_deref(), self.live_data)
            && hop.output_amount > 0
        {
            return Some((addr.to_string(), hop.output_amount));
        }
        let pools = self.index.direct_pools(from, to);
        best_pool(self.index, &pools, *from, amount_in, self.swappable_set.as_deref(), self.live_data)
    }

    /// 2-hop: explore neighbors of input_mint (forward search).
    fn neighbor_2hop_forward(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        skip: &[Pubkey],
        out: &mut Vec<Route>,
    ) {
        let skip_set: HashSet<Pubkey> = skip.iter().copied().collect();
        let mut seen_mids: HashSet<Pubkey> = HashSet::new();
        let mut tried = 0usize;

        for (mid, _pool_addr) in &self.index.neighbors(input) {
            if tried >= MAX_NEIGHBOR_CANDIDATES {
                break;
            }
            if *mid == *input || *mid == *output || skip_set.contains(mid) {
                continue;
            }
            // Dedup: many edges can share the same neighbor mint; the canonical
            // pool for (input, mid) is the same regardless of which edge found it.
            if !seen_mids.insert(*mid) {
                continue;
            }

            let leg2 = self.index.direct_pools(mid, output);
            if leg2.is_empty() {
                tried += 1;
                continue;
            }

            // Intermediate leg: canonical pool (input → mid).
            let Some((a1, mid_amount)) = self.canonical_leg(input, mid, amount_in) else {
                tried += 1;
                continue;
            };

            // Final leg: full scan (mid → output).
            let Some((a2, _)) = best_pool(self.index, &leg2, *mid, mid_amount, self.swappable_set.as_deref(), self.live_data) else {
                tried += 1;
                continue;
            };

            if let Some(route) = simulate_path(self.index, &[(a1, *input, *mid), (a2, *mid, *output)],
            amount_in, self.swappable_set.as_deref(), self.live_data) {
                out.push(route);
            }

            tried += 1;
        }
    }

    /// 2-hop: explore neighbors of output_mint (reverse search).
    /// For each neighbor `mid` of output, check if input → mid has a pool.
    fn neighbor_2hop_reverse(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        skip: &[Pubkey],
        out: &mut Vec<Route>,
    ) {
        let skip_set: HashSet<Pubkey> = skip.iter().copied().collect();
        let mut tried = 0usize;

        for (mid, _pool_to_output) in &self.index.neighbors(output) {
            if tried >= MAX_NEIGHBOR_CANDIDATES {
                break;
            }
            if *mid == *input || *mid == *output || skip_set.contains(mid) {
                continue;
            }

            if self.index.direct_pools(input, mid).is_empty() {
                tried += 1;
                continue;
            }

            // Intermediate leg: canonical pool (input → mid).
            let Some((a1, mid_amount)) = self.canonical_leg(input, mid, amount_in) else {
                tried += 1;
                continue;
            };

            // Final leg: full scan (mid → output).
            let leg2 = self.index.direct_pools(mid, output);
            let Some((a2, _)) = best_pool(self.index, &leg2, *mid, mid_amount, self.swappable_set.as_deref(), self.live_data) else {
                tried += 1;
                continue;
            };

            if let Some(route) = simulate_path(self.index, &[(a1, *input, *mid), (a2, *mid, *output)],
            amount_in, self.swappable_set.as_deref(), self.live_data) {
                out.push(route);
            }

            tried += 1;
        }
    }

    /// 3-hop through two specific intermediates.
    fn try_3hop(
        &self,
        input: &Pubkey,
        h1: &Pubkey,
        h2: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        out: &mut Vec<Route>,
    ) {
        let l3 = self.index.direct_pools(h2, output);
        if l3.is_empty() {
            return;
        }

        // Intermediate legs: canonical pools. Final leg: full scan.
        let Some((a1, amt1)) = self.canonical_leg(input, h1, amount_in) else { return };
        let Some((a2, amt2)) = self.canonical_leg(h1, h2, amt1) else { return };
        let Some((a3, _)) = best_pool(self.index, &l3, *h2, amt2, self.swappable_set.as_deref(), self.live_data) else { return };

        if let Some(route) = simulate_path(self.index, &[(a1, *input, *h1), (a2, *h1, *h2), (a3, *h2, *output)],
        amount_in, self.swappable_set.as_deref(), self.live_data) {
            out.push(route);
        }
    }

    /// 3-hop via neighbor + hub.
    /// Tries: input → neighbor → hub → output  AND  input → hub → neighbor → output.
    fn neighbor_3hop(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        hubs: &[Pubkey],
        out: &mut Vec<Route>,
    ) {
        // Reverse-reachability prune: a 3-hop input→neighbor→hub→output can only
        // complete through a hub that itself has a final hop to the target.
        // Compute those target-reaching hubs once (the hub set is tiny), then
        // drop any input-neighbor that reaches none of them BEFORE simulating.
        let target_hubs: Vec<Pubkey> = hubs
            .iter()
            .copied()
            .filter(|h| !self.index.direct_pools(h, output).is_empty())
            .collect();
        if target_hubs.is_empty() {
            // No hub reaches the target — no forward 3-hop route can exist.
            return self.neighbor_3hop_reverse(input, output, amount_in, hubs, out);
        }

        // Forward: input → neighbor_of_input → hub → output
        let mut tried = 0usize;
        for (mid, _) in &self.index.neighbors(input) {
            if tried >= MAX_NEIGHBOR_CANDIDATES / 2 {
                break;
            }
            if *mid == *input || *mid == *output || hubs.contains(mid) {
                continue;
            }
            // Prune: mid must connect to a hub that reaches the target.
            let reaches_target = target_hubs
                .iter()
                .any(|h| !self.index.direct_pools(mid, h).is_empty());
            if !reaches_target {
                tried += 1;
                continue;
            }
            for hub in &target_hubs {
                self.try_3hop(input, mid, hub, output, amount_in, out);
            }
            tried += 1;
        }
        self.neighbor_3hop_reverse(input, output, amount_in, hubs, out);
    }

    /// Reverse arm of the 3-hop neighbor search: input → hub → neighbor_of_output → output.
    /// Neighbors here are adjacent to the target by construction, so no
    /// reverse-reachability prune applies.
    fn neighbor_3hop_reverse(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        hubs: &[Pubkey],
        out: &mut Vec<Route>,
    ) {

        // Reverse: input → hub → neighbor_of_output → output
        let mut tried = 0usize;
        for (mid, _) in &self.index.neighbors(output) {
            if tried >= MAX_NEIGHBOR_CANDIDATES / 2 {
                break;
            }
            if *mid == *input || *mid == *output || hubs.contains(mid) {
                continue;
            }
            for hub in hubs {
                self.try_3hop(input, hub, mid, output, amount_in, out);
            }
            tried += 1;
        }
    }

    /// 4-hop: input → neighbor_in → hub → neighbor_out → output.
    /// Meets in the middle at a hub mint.
    fn neighbor_4hop(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount_in: u64,
        hubs: &[Pubkey],
        out: &mut Vec<Route>,
    ) {
        // Collect neighbors of input that connect to any hub.
        let in_neighbors: Vec<(Pubkey, String)> = self
            .index
            .neighbors(input)
            .into_iter()
            .filter(|(mid, _)| *mid != *input && *mid != *output && !hubs.contains(mid))
            .take(MAX_NEIGHBOR_CANDIDATES / 4)
            .collect();

        // Collect neighbors of output that connect to any hub.
        let out_neighbors: Vec<(Pubkey, String)> = self
            .index
            .neighbors(output)
            .into_iter()
            .filter(|(mid, _)| *mid != *input && *mid != *output && !hubs.contains(mid))
            .take(MAX_NEIGHBOR_CANDIDATES / 4)
            .collect();

        // Reverse-reachability prune: a hub is only useful if it bridges to some
        // out-neighbor (which reaches the target in one final hop). Keep only
        // those hubs, then drop input-neighbors that reach none of them — before
        // any swap simulation.
        let reaching_hubs: Vec<Pubkey> = hubs
            .iter()
            .copied()
            .filter(|h| {
                out_neighbors
                    .iter()
                    .any(|(n_out, _)| !self.index.direct_pools(h, n_out).is_empty())
            })
            .collect();
        if reaching_hubs.is_empty() {
            return;
        }
        let in_neighbors: Vec<(Pubkey, String)> = in_neighbors
            .into_iter()
            .filter(|(n_in, _)| {
                reaching_hubs
                    .iter()
                    .any(|h| !self.index.direct_pools(n_in, h).is_empty())
            })
            .collect();

        for hub in &reaching_hubs {
            for (n_in, _) in &in_neighbors {
                // Check n_in connects to hub
                if self.index.direct_pools(n_in, hub).is_empty() {
                    continue;
                }
                for (n_out, _) in &out_neighbors {
                    if n_in == n_out {
                        continue;
                    }
                    // Check hub connects to n_out
                    if self.index.direct_pools(hub, n_out).is_empty() {
                        continue;
                    }

                    // input → n_in → hub → n_out → output
                    let l4 = self.index.direct_pools(n_out, output);
                    if l4.is_empty() {
                        continue;
                    }

                    // Intermediate legs: canonical pools. Final leg: full scan.
                    let Some((a1, amt1)) = self.canonical_leg(input, n_in, amount_in) else {
                        continue;
                    };
                    let Some((a2, amt2)) = self.canonical_leg(n_in, hub, amt1) else {
                        continue;
                    };
                    let Some((a3, amt3)) = self.canonical_leg(hub, n_out, amt2) else {
                        continue;
                    };
                    let Some((a4, _)) = best_pool(self.index, &l4, *n_out, amt3, self.swappable_set.as_deref(), self.live_data) else {
                        continue;
                    };

                    if let Some(route) = simulate_path(self.index, &[
                        (a1, *input, *n_in),
                        (a2, *n_in, *hub),
                        (a3, *hub, *n_out),
                        (a4, *n_out, *output),
                    ],
                    amount_in, self.swappable_set.as_deref(), self.live_data) {
                        out.push(route);
                    }
                }
            }
        }
    }
}

// =============================================================================
// Simulation helpers
// =============================================================================

/// Simulate a single hop using live data when available.
fn simulate_hop(
    index: &PoolIndex,
    pool_address: &str,
    input_mint: Pubkey,
    amount_in: u64,
    swappable: Option<&HashSet<String>>,
    live: Option<&dyn AccountDataProvider>,
) -> Option<RouteHop> {
    if let Some(set) = swappable {
        if !set.contains(pool_address) {
            return None;
        }
    }

    let entry = index.get_pool(pool_address)?;

    if swappable.is_none() {
        if !entry.market.is_active() {
            return None;
        }
        if let Ok(fin) = entry.market.financials() {
            if fin.quote_balance < MIN_VAULT_BALANCE && fin.base_balance < MIN_VAULT_BALANCE {
                return None;
            }
        }
    }

    let (direction, output_mint) = if input_mint == entry.quote_mint {
        (SwapDirection::Buy, entry.base_mint)
    } else if input_mint == entry.base_mint {
        (SwapDirection::Sell, entry.quote_mint)
    } else {
        return None;
    };

    // Use live data when a provider is available. Capture the output-side
    // reserve so we can reject quotes that would drain an implausible fraction
    // of the pool (where the single-bin/CP approximation breaks down).
    let (output_amount, output_reserve) = if let Some(provider) = live {
        let pool_data = provider.pool_account_data(&entry.pool_pubkey);
        let quote_bal = provider.token_balance(&entry.quote_vault);
        let base_bal = provider.token_balance(&entry.base_vault);
        let out = entry.market.calculate_output_live_ex(
            amount_in, direction, pool_data.as_deref(), quote_bal, base_bal, provider,
        ).ok()?;
        // Output side: Buy yields base, Sell yields quote.
        let reserve = match direction {
            SwapDirection::Buy => base_bal,
            SwapDirection::Sell => quote_bal,
        };
        (out, reserve)
    } else {
        let out = entry.market.calculate_output(amount_in, direction).ok()?;
        let reserve = entry.market.financials().ok().map(|fin| match direction {
            SwapDirection::Buy => fin.base_balance,
            SwapDirection::Sell => fin.quote_balance,
        });
        (out, reserve.unwrap_or(u64::MAX))
    };

    if output_amount == 0 {
        return None;
    }
    if output_amount > amount_in.saturating_mul(1_000_000) {
        return None;
    }
    // Depth gate: reject when the trade would take more than 1/N of the
    // output reserve — the pool is too thin for this size and its quote is
    // unreliable (see MAX_OUTPUT_RESERVE_DIVISOR). A reserve of 0 means
    // "unknown" (vault not yet streamed/fetched into the store), NOT empty —
    // skip the gate rather than reject a pool whose balance hasn't loaded.
    // u64::MAX is the offline "no financials" sentinel, also skipped.
    if output_reserve != 0
        && output_reserve != u64::MAX
        && (output_amount as u128) * (MAX_OUTPUT_RESERVE_DIVISOR as u128)
            > (output_reserve as u128)
    {
        return None;
    }

    Some(RouteHop {
        pool_address: pool_address.to_string(),
        dex_name: entry.dex_name.clone(),
        input_mint,
        output_mint,
        input_amount: amount_in,
        output_amount,
        price_impact_bps: 0,
    })
}

/// Simulate a full multi-hop path.
fn simulate_path(
    index: &PoolIndex,
    hops: &[(String, Pubkey, Pubkey)],
    initial_amount: u64,
    swappable: Option<&HashSet<String>>,
    live: Option<&dyn AccountDataProvider>,
) -> Option<Route> {
    if hops.is_empty() { return None; }
    let mut visited = HashSet::new();
    visited.insert(hops[0].1);
    for (_, _, out_mint) in hops {
        if !visited.insert(*out_mint) { return None; }
    }
    let mut result_hops = Vec::with_capacity(hops.len());
    let mut current_amount = initial_amount;
    let mut total_impact: u64 = 0;
    for (pool_address, input_mint, _) in hops {
        let hop = simulate_hop(index, pool_address, *input_mint, current_amount, swappable, live)?;
        current_amount = hop.output_amount;
        total_impact = total_impact.saturating_add(hop.price_impact_bps);
        result_hops.push(hop);
    }
    let first = result_hops.first()?;
    let last = result_hops.last()?;
    Some(Route {
        input_mint: first.input_mint,
        output_mint: last.output_mint,
        input_amount: initial_amount,
        output_amount: current_amount,
        price_impact_bps: total_impact,
        hops: result_hops,
    })
}

/// Among `pool_addresses`, pick the one yielding the highest output.
fn best_pool(
    index: &PoolIndex,
    pool_addresses: &[String],
    input_mint: Pubkey,
    amount_in: u64,
    swappable: Option<&HashSet<String>>,
    live: Option<&dyn AccountDataProvider>,
) -> Option<(String, u64)> {
    pool_addresses
        .iter()
        .filter_map(|addr| {
            let hop = simulate_hop(index, addr, input_mint, amount_in, swappable, live)?;
            Some((addr.clone(), hop.output_amount))
        })
        .max_by_key(|(_, out)| *out)
}
// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use solroute_core::{Market, PoolFees, PoolFinancials, PoolMetadata, constant_product_swap, USDC};

    use crate::pool_index::PoolIndex;
    use crate::types::PoolEntry;

    /// Deterministic constant-product mock pool.
    struct MockMarket {
        quote_mint: Pubkey,
        base_mint: Pubkey,
        address: String,
        quote_balance: u64,
        base_balance: u64,
    }

    impl Market for MockMarket {
        fn metadata(&self) -> Result<PoolMetadata, GenericError> {
            Ok(PoolMetadata {
                address: self.address.clone(),
                dex_name: "mock".to_string(),
                quote_mint: self.quote_mint,
                base_mint: self.base_mint,
                quote_vault: self.quote_mint,
                base_vault: self.base_mint,
                fees: PoolFees { trade_fee_bps: 30, protocol_fee_bps: None },
            })
        }

        fn financials(&self) -> Result<PoolFinancials, GenericError> {
            Ok(PoolFinancials {
                quote_balance: self.quote_balance,
                base_balance: self.base_balance,
                quote_decimals: 9,
                base_decimals: 9,
            })
        }

        fn calculate_output(&self, amount_in: u64, direction: SwapDirection) -> Result<u64, GenericError> {
            let (rin, rout) = match direction {
                SwapDirection::Buy => (self.quote_balance, self.base_balance),
                SwapDirection::Sell => (self.base_balance, self.quote_balance),
            };
            constant_product_swap(rin, rout, amount_in, 30)
        }

        fn calculate_price_impact(&self, _amount_in: u64, _direction: SwapDirection) -> Result<u64, GenericError> {
            Ok(0)
        }

        fn current_price(&self) -> Result<f64, GenericError> {
            Ok(1.0)
        }
    }

    fn mint(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn add(index: &mut PoolIndex, addr: &str, q: Pubkey, b: Pubkey, qbal: u64, bbal: u64) {
        let market = MockMarket {
            quote_mint: q,
            base_mint: b,
            address: addr.to_string(),
            quote_balance: qbal,
            base_balance: bbal,
        };
        let entry = PoolEntry {
            market: Box::new(market),
            dex_name: "mock".to_string(),
            quote_mint: q,
            base_mint: b,
            pool_pubkey: q,
            quote_vault: q,
            base_vault: b,
            cached_data: vec![],
        };
        index.add_pool(addr.to_string(), entry).unwrap();
    }

    const BAL: u64 = 1_000_000_000; // 1e9 raw, well above MIN_VAULT_BALANCE

    #[test]
    fn canonical_picks_deepest_and_is_direction_independent() {
        let (a, b) = (mint(1), mint(2));
        let mut index = PoolIndex::new();
        add(&mut index, "shallow", a, b, BAL, BAL);
        add(&mut index, "deep", a, b, BAL * 10, BAL * 10);

        assert_eq!(index.canonical_pool(&a, &b), Some("deep"));
        // Direction-independent: same answer regardless of arg order.
        assert_eq!(index.canonical_pool(&b, &a), Some("deep"));
        assert_eq!(index.canonical_pool(&mint(9), &mint(8)), None);
    }

    #[test]
    fn top_hubs_ranked_by_degree() {
        let hub = mint(100);
        let mut index = PoolIndex::new();
        // `hub` participates in 3 pools; every other mint in 1.
        add(&mut index, "p1", hub, mint(1), BAL, BAL);
        add(&mut index, "p2", hub, mint(2), BAL, BAL);
        add(&mut index, "p3", hub, mint(3), BAL, BAL);
        add(&mut index, "p4", mint(4), mint(5), BAL, BAL);

        assert_eq!(index.top_hubs(1), vec![hub]);
    }

    #[test]
    fn hub_set_always_includes_seed_mints() {
        // Graph has no settlement mints at all.
        let mut index = PoolIndex::new();
        add(&mut index, "p1", mint(1), mint(2), BAL, BAL);
        let router = Router::new(&index, 2);

        let hubs = router.hub_set(HUB_COUNT, &mint(50), &mint(51));
        assert!(hubs.contains(&Pubkey::from_str_const(WSOL)));
        assert!(hubs.contains(&Pubkey::from_str_const(USDC)));
        assert!(hubs.contains(&Pubkey::from_str_const(USDT)));
    }

    #[test]
    fn finds_direct_route() {
        let (a, b) = (mint(1), mint(2));
        let mut index = PoolIndex::new();
        add(&mut index, "ab", a, b, BAL, BAL);

        let quote = Router::new(&index, 1).find_routes(a, b, 1_000_000, 5).unwrap();
        let best = quote.best().expect("a direct route should exist");
        assert_eq!(best.hops.len(), 1);
        assert!(best.output_amount > 0);
    }

    #[test]
    fn finds_two_hop_via_hub_when_no_direct_pool() {
        let wsol = Pubkey::from_str_const(WSOL);
        let (a, b) = (mint(1), mint(2));
        let mut index = PoolIndex::new();
        // No A-B pool; route must go A -> WSOL -> B.
        add(&mut index, "a_wsol", a, wsol, BAL, BAL);
        add(&mut index, "wsol_b", wsol, b, BAL, BAL);

        let quote = Router::new(&index, 2).find_routes(a, b, 1_000_000, 5).unwrap();
        let best = quote.best().expect("a 2-hop route should exist");
        assert_eq!(best.hops.len(), 2);
        assert_eq!(best.hops[0].output_mint, wsol);
        assert!(best.output_amount > 0);
    }

    #[test]
    fn longer_route_capped_at_multiple_of_shorter() {
        let (a, b, c) = (mint(1), mint(2), mint(3));
        let mut index = PoolIndex::new();
        // Direct A-B: fair, ~1M out.
        add(&mut index, "ab", a, b, BAL, BAL);
        // Phantom 2-hop A->C->B: lopsided A-C leg quotes ~100M, far above 3× direct.
        add(&mut index, "ac", a, c, BAL, BAL * 100);
        add(&mut index, "cb", c, b, BAL * 100, BAL * 100);

        let quote = Router::new(&index, 2).find_routes(a, b, 1_000_000, 5).unwrap();
        let best = quote.best().expect("route should exist");
        assert_eq!(best.hops.len(), 1, "phantom 2-hop outranked direct");
        assert!(best.output_amount < 2_000_000);
    }

    #[test]
    fn finds_three_hop_and_reverse_prune_keeps_valid_routes() {
        let wsol = Pubkey::from_str_const(WSOL);
        let usdc = Pubkey::from_str_const(USDC);
        let (a, b) = (mint(1), mint(2));
        let mut index = PoolIndex::new();
        // Valid path: A -> WSOL -> USDC -> B.
        add(&mut index, "a_wsol", a, wsol, BAL, BAL);
        add(&mut index, "wsol_usdc", wsol, usdc, BAL, BAL);
        add(&mut index, "usdc_b", usdc, b, BAL, BAL);
        // Dead-end neighbor of A that cannot reach B — must be pruned, not crash.
        add(&mut index, "a_dead", a, mint(200), BAL, BAL);

        let quote = Router::new(&index, 3).find_routes(a, b, 1_000_000, 5).unwrap();
        let best = quote.best().expect("a 3-hop route should exist");
        assert!(best.hops.len() >= 2 && best.hops.len() <= 3);
        assert_eq!(best.output_mint, b);
        assert!(best.output_amount > 0);
    }
}
