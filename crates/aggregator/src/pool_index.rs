//! In-memory pool index: token graph for route discovery.
//!
//! Stores pools as `Box<dyn Market>` and maintains an adjacency list
//! mapping each token mint to the pools it participates in.

use std::collections::HashMap;
use std::sync::OnceLock;

use solana_pubkey::Pubkey;
use thunder_core::GenericError;

use crate::types::PoolEntry;

/// Edge in the token graph: connects to `other_mint` via `pool_address`.
#[derive(Debug, Clone)]
struct Edge {
    other_mint: Pubkey,
    pool_address: String,
}

/// Direction-independent key for a token pair (canonical ordering by bytes).
fn pair_key(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a.to_bytes() <= b.to_bytes() {
        (*a, *b)
    } else {
        (*b, *a)
    }
}

/// In-memory index of all loaded pools, organized as a token-pair graph.
pub struct PoolIndex {
    /// Pool address -> PoolEntry (owns the Market trait object).
    pools: HashMap<String, PoolEntry>,
    /// Mint -> list of edges to other mints via pools.
    edges: HashMap<Pubkey, Vec<Edge>>,
    /// Per-DEX pool counts for statistics.
    dex_counts: HashMap<String, usize>,
    /// Canonical edge map: deepest-liquidity pool per token pair.
    /// Built lazily once from `pools`, then shared across every quote request
    /// (this index lives behind an `Arc` in the engine). Rebuilding only makes
    /// sense when the pool set changes — the streaming path mutates balances,
    /// not membership, so a single build is correct for the process lifetime.
    canonical: OnceLock<HashMap<(Pubkey, Pubkey), String>>,
    /// Top mints by edge count (pool degree), highest first. The router uses
    /// these as routing hubs instead of a hardcoded list. Built lazily once.
    hub_ranking: OnceLock<Vec<Pubkey>>,
}

impl PoolIndex {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
            edges: HashMap::new(),
            dex_counts: HashMap::new(),
            canonical: OnceLock::new(),
            hub_ranking: OnceLock::new(),
        }
    }

    /// Insert a pool into the index. Uses pre-resolved mints from the entry
    /// to build bidirectional edges in the token graph.
    pub fn add_pool(&mut self, address: String, entry: PoolEntry) -> Result<(), GenericError> {
        // Bidirectional edges: quote_mint <-> base_mint via this pool.
        self.edges
            .entry(entry.quote_mint)
            .or_default()
            .push(Edge {
                other_mint: entry.base_mint,
                pool_address: address.clone(),
            });
        self.edges
            .entry(entry.base_mint)
            .or_default()
            .push(Edge {
                other_mint: entry.quote_mint,
                pool_address: address.clone(),
            });

        *self.dex_counts.entry(entry.dex_name.clone()).or_insert(0) += 1;
        self.pools.insert(address, entry);
        Ok(())
    }

    /// Look up a pool by address.
    pub fn get_pool(&self, address: &str) -> Option<&PoolEntry> {
        self.pools.get(address)
    }

    /// All (other_mint, pool_address) pairs reachable from `mint` in one hop.
    pub fn neighbors(&self, mint: &Pubkey) -> Vec<(Pubkey, String)> {
        self.edges
            .get(mint)
            .map(|edges| {
                edges
                    .iter()
                    .map(|e| (e.other_mint, e.pool_address.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All pools that directly connect `mint_a` and `mint_b`.
    pub fn direct_pools(&self, mint_a: &Pubkey, mint_b: &Pubkey) -> Vec<String> {
        self.edges
            .get(mint_a)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.other_mint == *mint_b)
                    .map(|e| e.pool_address.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Total number of pools in the index.
    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// Number of unique token mints in the graph.
    pub fn unique_mints(&self) -> usize {
        self.edges.len()
    }

    /// Per-DEX pool counts.
    pub fn dex_counts(&self) -> &HashMap<String, usize> {
        &self.dex_counts
    }

    /// All mints that have at least one pool (for iteration).
    pub fn all_mints(&self) -> Vec<Pubkey> {
        self.edges.keys().copied().collect()
    }

    /// Iterate over all (address, PoolEntry) pairs.
    pub fn iter_pools(&self) -> impl Iterator<Item = (&str, &PoolEntry)> {
        self.pools.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Canonical (deepest-liquidity) pool address for the `(a, b)` pair, if any.
    ///
    /// Direction-independent. The router uses this for intermediate hops so it
    /// never re-scans every pool on a pair mid-route; only the final hop scans.
    pub fn canonical_pool(&self, a: &Pubkey, b: &Pubkey) -> Option<&str> {
        self.canonical
            .get_or_init(|| self.build_canonical())
            .get(&pair_key(a, b))
            .map(|s| s.as_str())
    }

    /// Force the canonical edge map to build now (otherwise built on first
    /// `canonical_pool` call). Returns the number of pairs indexed. Call at
    /// startup to keep the first quote off the build cost.
    pub fn warm_canonical(&self) -> usize {
        self.canonical.get_or_init(|| self.build_canonical()).len()
    }

    /// Build the canonical edge map: for each token pair, the pool holding the
    /// most of the pair's reference mint (a liquidity-depth proxy that is
    /// comparable across pools of the same pair regardless of quote/base order).
    fn build_canonical(&self) -> HashMap<(Pubkey, Pubkey), String> {
        let mut best: HashMap<(Pubkey, Pubkey), (u128, String)> = HashMap::new();
        for (addr, entry) in self.pools.iter() {
            if entry.quote_mint == entry.base_mint {
                continue;
            }
            let Ok(fin) = entry.market.financials() else {
                continue;
            };
            let key = pair_key(&entry.quote_mint, &entry.base_mint);
            // Reference mint = key.0; score by its balance so all pools of the
            // pair are ranked on the same token.
            let score = if entry.quote_mint == key.0 {
                fin.quote_balance
            } else {
                fin.base_balance
            } as u128;

            match best.get(&key) {
                Some((existing, _)) if *existing >= score => {}
                _ => {
                    best.insert(key, (score, addr.clone()));
                }
            }
        }
        best.into_iter().map(|(k, (_, addr))| (k, addr)).collect()
    }

    /// The `k` highest-degree mints (by pool count), highest first. Used by the
    /// router as data-driven routing hubs.
    pub fn top_hubs(&self, k: usize) -> Vec<Pubkey> {
        self.hub_ranking
            .get_or_init(|| self.build_hub_ranking())
            .iter()
            .take(k)
            .copied()
            .collect()
    }

    /// Force the hub ranking to build now. Returns the number of ranked hubs.
    pub fn warm_hubs(&self) -> usize {
        self.hub_ranking
            .get_or_init(|| self.build_hub_ranking())
            .len()
    }

    /// Rank mints by edge count (number of pools they participate in) and keep
    /// the top few. Uses `select_nth` so we never hold a fully-sorted copy of
    /// every mint — only the small hub set is sorted.
    fn build_hub_ranking(&self) -> Vec<Pubkey> {
        /// How many top hubs to retain — comfortably above any `k` the router asks for.
        const CAP: usize = 32;

        let mut degrees: Vec<(usize, Pubkey)> =
            self.edges.iter().map(|(mint, e)| (e.len(), *mint)).collect();

        if degrees.len() > CAP {
            // Partition so the CAP highest-degree mints sit in [0, CAP).
            degrees.select_nth_unstable_by(CAP - 1, |a, b| b.0.cmp(&a.0));
            degrees.truncate(CAP);
        }
        degrees.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        degrees.into_iter().map(|(_, mint)| mint).collect()
    }
}
