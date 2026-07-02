use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use solana_pubkey::Pubkey;
use thunder_aggregator::cache::CachedPool;
use thunder_aggregator::pool_index::PoolIndex;
use thunder_core::Market;

use crate::account_store::AccountStore;


/// Pool with metadata, swappable flag, and DEX-specific auxiliary accounts.
pub struct PoolInfo {
    pub address: String,
    pub dex_name: String,
    pub market: Box<dyn Market>,
    pub swappable: bool,
    pub quote_mint: Pubkey,
    pub base_mint: Pubkey,
    pub quote_vault: Pubkey,
    pub base_vault: Pubkey,
    /// CLMM tick array accounts, populated by cold_start.
    pub tick_arrays: Vec<Pubkey>,
    /// DLMM bitmap extension account, populated by cold_start.
    pub bitmap_ext: Option<Pubkey>,
    /// DLMM active bin array PDA, populated by cold_start.
    pub bin_array: Option<Pubkey>,
    /// Serialized pool data for disk cache.
    pub cached_data: Vec<u8>,
}

/// Registry of all pools with graph-based lookup and per-pool swappable validation.
pub struct PoolRegistry {
    pools: HashMap<String, PoolInfo>,
    /// mint -> [(other_mint, pool_address)]
    edges: HashMap<Pubkey, Vec<(Pubkey, String)>>,
    dex_counts: HashMap<String, usize>,
    swappable_count: AtomicUsize,
    cached_swappable: Arc<HashSet<String>>,
    /// vault_pubkey -> pool_address (for streaming re-validation).
    vault_to_pool: HashMap<Pubkey, Vec<String>>,
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
            edges: HashMap::new(),
            dex_counts: HashMap::new(),
            swappable_count: AtomicUsize::new(0),
            cached_swappable: Arc::new(HashSet::new()),
            vault_to_pool: HashMap::new(),
        }
    }

    /// Build a registry from an existing PoolIndex.
    ///
    /// Reconstructs owned `Market` trait objects by deserializing each entry's
    /// `cached_data` through `CachedPool`. All pools start with `swappable: false`;
    /// call `validate_all` to set flags from an `AccountStore`.
    pub fn from_pool_index(index: &PoolIndex) -> Self {
        let mut registry = Self::new();

        for (_addr, entry) in index.iter_pools() {
            if entry.cached_data.is_empty() {
                continue;
            }

            // Reconstruct an owned Market from the serialized cache bytes.
            let cached: CachedPool = match bincode::deserialize(&entry.cached_data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let (address, owned_entry) = cached.into_pool_entry();

            // Vault addresses still need metadata (not on PoolEntry).
            let meta = match owned_entry.market.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let info = PoolInfo {
                address: address.clone(),
                dex_name: owned_entry.dex_name,
                market: owned_entry.market,
                swappable: false,
                quote_mint: owned_entry.quote_mint,
                base_mint: owned_entry.base_mint,
                quote_vault: meta.quote_vault,
                base_vault: meta.base_vault,
                tick_arrays: Vec::new(),
                bitmap_ext: None,
                bin_array: None,
                cached_data: owned_entry.cached_data,
            };

            registry.add_pool(address, info);
        }

        registry
    }

    /// Add a pool, building bidirectional edges for graph traversal.
    pub fn add_pool(&mut self, address: String, info: PoolInfo) {
        let quote_mint = info.quote_mint;
        let base_mint = info.base_mint;

        self.edges
            .entry(quote_mint)
            .or_default()
            .push((base_mint, address.clone()));
        self.edges
            .entry(base_mint)
            .or_default()
            .push((quote_mint, address.clone()));

        // Reverse index: vault pubkey -> pool address.
        self.vault_to_pool.entry(info.quote_vault).or_default().push(address.clone());
        self.vault_to_pool.entry(info.base_vault).or_default().push(address.clone());

        *self.dex_counts.entry(info.dex_name.clone()).or_insert(0) += 1;

        if info.swappable {
            self.swappable_count.fetch_add(1, Ordering::Relaxed);
        }

        self.pools.insert(address, info);
    }

    pub fn get_pool(&self, address: &str) -> Option<&PoolInfo> {
        self.pools.get(address)
    }

    pub fn get_pool_mut(&mut self, address: &str) -> Option<&mut PoolInfo> {
        self.pools.get_mut(address)
    }

    /// All pool addresses that directly connect mint_a and mint_b.
    pub fn direct_pools(&self, mint_a: &Pubkey, mint_b: &Pubkey) -> Vec<String> {
        self.edges
            .get(mint_a)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|(other, _)| other == mint_b)
                    .map(|(_, addr)| addr.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All (other_mint, pool_address) pairs reachable from `mint` in one hop.
    pub fn neighbors(&self, mint: &Pubkey) -> Vec<(Pubkey, String)> {
        self.edges.get(mint).cloned().unwrap_or_default()
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    pub fn swappable_count(&self) -> usize {
        self.swappable_count.load(Ordering::Relaxed)
    }

    pub fn unique_mints(&self) -> usize {
        self.edges.len()
    }

    pub fn dex_counts(&self) -> &HashMap<String, usize> {
        &self.dex_counts
    }

    pub fn iter_pools(&self) -> impl Iterator<Item = (&str, &PoolInfo)> {
        self.pools.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Cached swappable set -- rebuilt during validation, O(1) to retrieve.
    pub fn swappable_set(&self) -> Arc<HashSet<String>> {
        self.cached_swappable.clone()
    }

    /// Validate a single pool's swappable status against on-chain account data.
    pub fn validate_pool(&mut self, address: &str, store: &AccountStore) {
        let info = match self.pools.get_mut(address) {
            Some(i) => i,
            None => return,
        };

        let was_swappable = info.swappable;
        let now_swappable = check_swappable(info, store);

        if was_swappable != now_swappable {
            info.swappable = now_swappable;
            if now_swappable {
                self.swappable_count.fetch_add(1, Ordering::Relaxed);
            } else {
                self.swappable_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Called by streaming when a vault account changes.
    /// Looks up which pools use this vault and re-validates them.
    /// Updates the cached swappable set if any status changes.
    pub fn on_vault_update(&mut self, vault: &Pubkey, store: &AccountStore) {
        let pool_addrs = match self.vault_to_pool.get(vault) {
            Some(addrs) => addrs.clone(),
            None => return,
        };
        let mut changed = false;
        for addr in &pool_addrs {
            let info = match self.pools.get_mut(addr.as_str()) {
                Some(i) => i,
                None => continue,
            };
            let was = info.swappable;
            let now = check_swappable(info, store);
            if was != now {
                info.swappable = now;
                if now {
                    self.swappable_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.swappable_count.fetch_sub(1, Ordering::Relaxed);
                }
                changed = true;
            }
        }
        if changed {
            self.rebuild_swappable_set();
        }
    }

    fn rebuild_swappable_set(&mut self) {
        let set: HashSet<String> = self.pools
            .iter()
            .filter(|(_, info)| info.swappable)
            .map(|(addr, _)| addr.clone())
            .collect();
        self.cached_swappable = Arc::new(set);
    }

    /// Validate all pools against the account store and recompute swappable set.
    pub fn validate_all(&mut self, store: &AccountStore) {
        let mut count = 0usize;
        let mut set = HashSet::with_capacity(self.pools.len() / 2);
        for (addr, info) in self.pools.iter_mut() {
            info.swappable = check_swappable(info, store);
            if info.swappable {
                set.insert(addr.clone());
                count += 1;
            }
        }
        self.swappable_count.store(count, Ordering::Relaxed);
        self.cached_swappable = Arc::new(set);
    }

    /// Initial validation using cached vault balances from the market objects.
    pub fn validate_from_cache(&mut self) {
        let mut count = 0usize;
        let mut set = HashSet::with_capacity(self.pools.len() / 2);
        for (addr, info) in self.pools.iter_mut() {
            info.swappable = check_swappable_cached(info);
            if info.swappable {
                set.insert(addr.clone());
                count += 1;
            }
        }
        self.swappable_count.store(count, Ordering::Relaxed);
        self.cached_swappable = Arc::new(set);
    }
}

// ---------------------------------------------------------------------------
// Per-DEX swappable validation
// ---------------------------------------------------------------------------

/// Determine whether a pool can participate in route discovery.
/// Checks on-chain status and vault liquidity only — NOT auxiliary accounts
/// (tick arrays, bin arrays, bitmap extensions) which are needed for swap
/// instruction building but irrelevant for quoting.
fn check_swappable(info: &PoolInfo, store: &AccountStore) -> bool {
    match info.dex_name.as_str() {
        "Pumpfun AMM" => true,

        "Raydium AMM V4" => vaults_funded(info, store),

        // All remaining DEXs: active + funded vaults.
        _ => info.market.is_active() && vaults_funded(info, store),
    }
}

/// Both vaults must have non-zero token balance in the store.
fn vaults_funded(info: &PoolInfo, store: &AccountStore) -> bool {
    store.read_token_balance(&info.quote_vault) > 0
        && store.read_token_balance(&info.base_vault) > 0
}

/// Same as `check_swappable` but uses cached vault balances from the market
/// object instead of the AccountStore. Used during startup before vault data
/// is fetched from RPC.
fn check_swappable_cached(info: &PoolInfo) -> bool {
    match info.dex_name.as_str() {
        "Pumpfun AMM" => true,

        "Raydium AMM V4" => {
            info.market.financials().is_ok_and(|f| f.quote_balance > 0 && f.base_balance > 0)
        }

        _ => {
            info.market.is_active()
                && info.market.financials().is_ok_and(|f| f.quote_balance > 0 && f.base_balance > 0)
        }
    }
}