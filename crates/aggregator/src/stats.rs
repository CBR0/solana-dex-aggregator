//! System and pool statistics collection.

use std::time::Instant;

use sysinfo::System;

use crate::pool_index::PoolIndex;
use crate::types::AggregatorStats;

/// Collects aggregator statistics including per-DEX pool counts,
/// process memory/CPU, and uptime.
pub struct StatsCollector {
    start_time: Instant,
    system: System,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            system: System::new_all(),
        }
    }

    /// Snapshot current stats. Refreshes sysinfo before reading process metrics.
    pub fn collect(&mut self, index: &PoolIndex) -> AggregatorStats {
        self.system.refresh_all();

        let (memory_mb, cpu_percent) = sysinfo::get_current_pid()
            .ok()
            .and_then(|pid| self.system.process(pid))
            .map(|p| (p.memory() as f64 / 1024.0 / 1024.0, p.cpu_usage()))
            .unwrap_or((0.0, 0.0));

        let mut pools_per_dex: Vec<(String, usize)> = index
            .dex_counts()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        pools_per_dex.sort_by(|a, b| b.1.cmp(&a.1));

        AggregatorStats {
            pools_per_dex,
            total_pools: index.pool_count(),
            unique_tokens: index.unique_mints(),
            memory_mb,
            cpu_percent,
            uptime_secs: self.start_time.elapsed().as_secs(),
        }
    }
}
