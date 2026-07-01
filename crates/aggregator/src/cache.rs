//! Pool cache: save loaded pools to disk, restore on next startup.
//!
//! Stores all pool data as serialized structs in a compact binary file.
//! On warm start, rebuilds the PoolIndex from cache in seconds — zero RPC calls.

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use thunder_core::{GenericError, Market};

use meteora_damm::{MeteoraDAMMMarket, MeteoraDAMMPool, MeteoraDAMMV2Market, MeteoraDAMMV2Pool};
use meteora_dlmm::{MeteoraDlmmMarket, MeteoraDLMMPool};
use pumpfun_amm::{PumpfunAmmMarket, PumpfunAmmPool};
use raydium_amm_v4::{RaydiumAmmV4Market, RaydiumAMMV4};
use raydium_clmm::{RaydiumClmmMarket, RaydiumCLMMPool};

use crate::pool_index::PoolIndex;
use crate::types::PoolEntry;

const CACHE_VERSION: u32 = 1;
/// Serializable pool data. Each variant holds everything needed to
/// reconstruct the corresponding Market struct with zero RPC calls.
#[derive(Serialize, Deserialize)]
pub enum CachedPool {
    RaydiumV4 { addr: String, pool: RaydiumAMMV4, quote_bal: u64, base_bal: u64 },
    RaydiumClmm { addr: String, pool: RaydiumCLMMPool, v0_bal: u64, v1_bal: u64 },
    MeteoraDAMMV1 { addr: String, pool: MeteoraDAMMPool, a_bal: u64, b_bal: u64 },
    MeteoraDAMMV2 { addr: String, pool: MeteoraDAMMV2Pool, a_bal: u64, b_bal: u64 },
    MeteoraDLMM { addr: String, pool: MeteoraDLMMPool, rx_bal: u64, ry_bal: u64 },
    PumpfunAmm { addr: String, pool: PumpfunAmmPool },
}

impl CachedPool {
    /// Convert to a live PoolEntry (Market trait object + metadata).
    pub fn into_pool_entry(self) -> (String, PoolEntry) {
        match self {
            Self::RaydiumV4 { addr, pool, quote_bal, base_bal } => {
                let cached = bincode::serialize(&Self::RaydiumV4 {
                    addr: addr.clone(), pool: pool.clone(), quote_bal, base_bal,
                }).unwrap_or_default();
                let market = RaydiumAmmV4Market::new(pool, addr.clone(), quote_bal, base_bal);
                make_entry(addr, "Raydium AMM V4", market, cached)
            }
            Self::RaydiumClmm { addr, pool, v0_bal, v1_bal } => {
                let cached = bincode::serialize(&Self::RaydiumClmm {
                    addr: addr.clone(), pool: pool.clone(), v0_bal, v1_bal,
                }).unwrap_or_default();
                let mut market = RaydiumClmmMarket::new(pool, addr.clone());
                market.vault_0_balance = v0_bal;
                market.vault_1_balance = v1_bal;
                make_entry(addr, "Raydium CLMM", market, cached)
            }
            Self::MeteoraDAMMV1 { addr, pool, a_bal, b_bal } => {
                let cached = bincode::serialize(&Self::MeteoraDAMMV1 {
                    addr: addr.clone(), pool: pool.clone(), a_bal, b_bal,
                }).unwrap_or_default();
                let mut market = MeteoraDAMMMarket::new(pool, addr.clone());
                market.a_vault_balance = a_bal;
                market.b_vault_balance = b_bal;
                make_entry(addr, "Meteora DAMM V1", market, cached)
            }
            Self::MeteoraDAMMV2 { addr, pool, a_bal, b_bal } => {
                let cached = bincode::serialize(&Self::MeteoraDAMMV2 {
                    addr: addr.clone(), pool: pool.clone(), a_bal, b_bal,
                }).unwrap_or_default();
                let mut market = MeteoraDAMMV2Market::new(pool, addr.clone());
                market.a_vault_balance = a_bal;
                market.b_vault_balance = b_bal;
                make_entry(addr, "Meteora DAMM V2", market, cached)
            }
            Self::MeteoraDLMM { addr, pool, rx_bal, ry_bal } => {
                let cached = bincode::serialize(&Self::MeteoraDLMM {
                    addr: addr.clone(), pool: pool.clone(), rx_bal, ry_bal,
                }).unwrap_or_default();
                let mut market = MeteoraDlmmMarket::new(pool, addr.clone());
                market.reserve_x_balance = rx_bal;
                market.reserve_y_balance = ry_bal;
                make_entry(addr, "Meteora DLMM", market, cached)
            }
            Self::PumpfunAmm { addr, pool } => {
                let cached = bincode::serialize(&Self::PumpfunAmm {
                    addr: addr.clone(), pool: pool.clone(),
                }).unwrap_or_default();
                let market = PumpfunAmmMarket::new(pool, addr.clone());
                make_entry(addr, "Pumpfun AMM", market, cached)
            }
        }
    }
}

/// Build a PoolEntry, resolving quote/base mints from market metadata once.
fn make_entry(
    addr: String,
    dex: &str,
    market: impl Market + 'static,
    cached_data: Vec<u8>,
) -> (String, PoolEntry) {
    let meta = market.metadata().unwrap();
    let pool_pubkey = Pubkey::from_str(&addr).unwrap_or_default();
    (addr, PoolEntry {
        quote_mint: meta.quote_mint,
        base_mint: meta.base_mint,
        pool_pubkey,
        quote_vault: meta.quote_vault,
        base_vault: meta.base_vault,
        market: Box::new(market),
        dex_name: dex.into(),
        cached_data,
    })
}

// ---------------------------------------------------------------------------
// Auxiliary PDA extraction from cached pool data
// ---------------------------------------------------------------------------


/// Extract tick array PDAs for a CLMM pool from its serialized cached data.
/// Returns `(pool_pubkey, tick_array_pdas)` or None for non-CLMM pools.
pub fn extract_clmm_tick_pdas(cached_data: &[u8]) -> Option<(Pubkey, Vec<Pubkey>)> {
    let cached: CachedPool = bincode::deserialize(cached_data).ok()?;
    match cached {
        CachedPool::RaydiumClmm { addr, pool, .. } => {
            let pool_id = Pubkey::from_str(&addr).ok()?;
            let pdas = raydium_clmm::tick_arrays::derive_pool_tick_array_pdas(&pool, &pool_id);
            if pdas.is_empty() { None } else { Some((pool_id, pdas)) }
        }
        _ => None,
    }
}

/// Extract the active bin array PDA for a DLMM pool from its serialized cached data.
/// Returns `(pool_pubkey, bin_array_pda)` or None for non-DLMM pools.
pub fn extract_dlmm_bin_pda(cached_data: &[u8]) -> Option<(Pubkey, Pubkey)> {
    let cached: CachedPool = bincode::deserialize(cached_data).ok()?;
    match cached {
        CachedPool::MeteoraDLMM { addr, pool, .. } => {
            let pool_pubkey = Pubkey::from_str(&addr).ok()?;
            let index = (pool.active_id as i64).div_euclid(70);
            let dlmm_program = Pubkey::from_str_const(meteora_dlmm::METEORA_DYNAMIC_LMM);
            let (pda, _) = Pubkey::find_program_address(
                &[b"bin_array", pool_pubkey.as_ref(), &index.to_le_bytes()],
                &dlmm_program,
            );
            Some((pool_pubkey, pda))
        }
        _ => None,
    }
}

#[derive(Serialize, Deserialize)]
struct CacheHeader {
    version: u32,
    timestamp: u64,
    pool_count: u64,
}

/// Save the PoolIndex to a binary cache file.
pub fn save_cache(index: &PoolIndex, path: &Path) -> Result<usize, GenericError> {
    let pools: Vec<&[u8]> = index
        .iter_pools()
        .filter(|(_, e)| !e.cached_data.is_empty())
        .map(|(_, e)| e.cached_data.as_slice())
        .collect();

    let header = CacheHeader {
        version: CACHE_VERSION,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        pool_count: pools.len() as u64,
    };

    let file = fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);

    let hdr = bincode::serialize(&header)?;
    w.write_all(&(hdr.len() as u32).to_le_bytes())?;
    w.write_all(&hdr)?;

    // Write each pool's pre-serialized bytes, length-prefixed.
    for data in &pools {
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(data)?;
    }

    // Sentinel
    w.write_all(&0u32.to_le_bytes())?;
    w.flush()?;

    Ok(pools.len())
}

/// Load a PoolIndex from a cache file. Returns `(index, unix_timestamp)`.
pub fn load_cache(path: &Path) -> Result<(PoolIndex, u64), GenericError> {
    let file = fs::File::open(path)?;
    let mut r = BufReader::with_capacity(1 << 20, file);

    // Header
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let hdr_len = u32::from_le_bytes(buf4) as usize;
    let mut hdr_bytes = vec![0u8; hdr_len];
    r.read_exact(&mut hdr_bytes)?;
    let header: CacheHeader = bincode::deserialize(&hdr_bytes)?;

    if header.version != CACHE_VERSION {
        return Err(format!("Cache version mismatch: file={}, expected={}", header.version, CACHE_VERSION).into());
    }

    let mut index = PoolIndex::new();
    let mut count = 0u64;

    loop {
        r.read_exact(&mut buf4)?;
        let len = u32::from_le_bytes(buf4) as usize;
        if len == 0 { break; } // sentinel

        let mut data = vec![0u8; len];
        r.read_exact(&mut data)?;

        if let Ok(cached) = bincode::deserialize::<CachedPool>(&data) {
            let (addr, entry) = cached.into_pool_entry();
            let _ = index.add_pool(addr, entry);
            count += 1;
        }
    }

    if count != header.pool_count {
        eprintln!("Cache: expected {} pools, loaded {count}", header.pool_count);
    }

    Ok((index, header.timestamp))
}

/// Return the cache age in seconds, or None if the file doesn't exist or is unreadable.
pub fn cache_age(path: &Path) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let mut r = BufReader::new(file);
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4).ok()?;
    let hdr_len = u32::from_le_bytes(buf4) as usize;
    let mut hdr_bytes = vec![0u8; hdr_len];
    r.read_exact(&mut hdr_bytes).ok()?;
    let header: CacheHeader = bincode::deserialize(&hdr_bytes).ok()?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Some(now.saturating_sub(header.timestamp))
}
