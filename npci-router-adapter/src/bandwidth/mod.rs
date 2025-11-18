// Bandwidth tracking module

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bandwidth statistics for a single entity (tenant/product)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandwidthStats {
    /// Entity ID (tenant or product)
    pub entity_id: String,

    /// Total bytes received (ingress)
    pub bytes_in: u64,

    /// Total bytes sent (egress)
    pub bytes_out: u64,

    /// Current ingress rate (bytes per second)
    pub rate_in_bps: u64,

    /// Current egress rate (bytes per second)
    pub rate_out_bps: u64,

    /// Timestamp of last update
    pub last_updated: u64,

    /// Number of requests processed
    pub request_count: u64,
}

/// Atomic bandwidth counter for thread-safe updates
struct BandwidthCounter {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    requests: AtomicU64,
    last_updated: AtomicU64,
}

impl BandwidthCounter {
    fn new() -> Self {
        Self {
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            last_updated: AtomicU64::new(Self::current_timestamp()),
        }
    }

    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn record_ingress(&self, bytes: u64) {
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.last_updated
            .store(Self::current_timestamp(), Ordering::Relaxed);
    }

    fn record_egress(&self, bytes: u64) {
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
        self.last_updated
            .store(Self::current_timestamp(), Ordering::Relaxed);
    }

    fn get_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.bytes_in.load(Ordering::Relaxed),
            self.bytes_out.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.last_updated.load(Ordering::Relaxed),
        )
    }

    fn reset(&self) -> (u64, u64, u64) {
        (
            self.bytes_in.swap(0, Ordering::Relaxed),
            self.bytes_out.swap(0, Ordering::Relaxed),
            self.requests.swap(0, Ordering::Relaxed),
        )
    }
}

/// Bandwidth tracker for monitoring data transfer
pub struct BandwidthTracker {
    /// Per-entity counters
    counters: Arc<DashMap<String, Arc<BandwidthCounter>>>,

    /// Historical stats (for rate calculation)
    historical_stats: Arc<DashMap<String, BandwidthStats>>,

    /// Report interval
    report_interval: Duration,
}

impl BandwidthTracker {
    /// Create a new bandwidth tracker
    pub fn new(report_interval_secs: u64) -> Self {
        Self {
            counters: Arc::new(DashMap::new()),
            historical_stats: Arc::new(DashMap::new()),
            report_interval: Duration::from_secs(report_interval_secs),
        }
    }

    /// Record ingress bandwidth for an entity
    pub fn record_ingress(&self, entity_id: &str, bytes: u64) {
        let counter = self
            .counters
            .entry(entity_id.to_string())
            .or_insert_with(|| Arc::new(BandwidthCounter::new()))
            .clone();

        counter.record_ingress(bytes);
    }

    /// Record egress bandwidth for an entity
    pub fn record_egress(&self, entity_id: &str, bytes: u64) {
        let counter = self
            .counters
            .entry(entity_id.to_string())
            .or_insert_with(|| Arc::new(BandwidthCounter::new()))
            .clone();

        counter.record_egress(bytes);
    }

    /// Get current bandwidth stats for an entity
    pub fn get_stats(&self, entity_id: &str) -> Option<BandwidthStats> {
        let counter = self.counters.get(entity_id)?;
        let (bytes_in, bytes_out, requests, last_updated) = counter.get_stats();

        // Calculate rates from historical data
        let (rate_in_bps, rate_out_bps) = if let Some(hist) = self.historical_stats.get(entity_id) {
            let time_diff = last_updated.saturating_sub(hist.last_updated).max(1);
            let bytes_in_diff = bytes_in.saturating_sub(hist.bytes_in);
            let bytes_out_diff = bytes_out.saturating_sub(hist.bytes_out);

            (
                bytes_in_diff / time_diff,
                bytes_out_diff / time_diff,
            )
        } else {
            (0, 0)
        };

        Some(BandwidthStats {
            entity_id: entity_id.to_string(),
            bytes_in,
            bytes_out,
            rate_in_bps,
            rate_out_bps,
            last_updated,
            request_count: requests,
        })
    }

    /// Get stats for all entities
    pub fn get_all_stats(&self) -> Vec<BandwidthStats> {
        self.counters
            .iter()
            .filter_map(|entry| self.get_stats(entry.key()))
            .collect()
    }

    /// Update historical stats (should be called periodically)
    pub fn update_historical_stats(&self) {
        for entry in self.counters.iter() {
            let entity_id = entry.key();
            if let Some(stats) = self.get_stats(entity_id) {
                self.historical_stats.insert(entity_id.clone(), stats);
            }
        }
    }

    /// Reset stats for an entity
    pub fn reset(&self, entity_id: &str) {
        if let Some(counter) = self.counters.get(entity_id) {
            counter.reset();
        }
        self.historical_stats.remove(entity_id);
    }

    /// Reset all stats
    pub fn reset_all(&self) {
        for entry in self.counters.iter() {
            entry.value().reset();
        }
        self.historical_stats.clear();
    }

    /// Check if entity exceeds bandwidth limit
    pub fn check_limit(&self, entity_id: &str, max_bps_in: Option<u64>, max_bps_out: Option<u64>) -> bool {
        if let Some(stats) = self.get_stats(entity_id) {
            if let Some(max_in) = max_bps_in {
                if stats.rate_in_bps > max_in {
                    return false;
                }
            }

            if let Some(max_out) = max_bps_out {
                if stats.rate_out_bps > max_out {
                    return false;
                }
            }
        }

        true
    }

    /// Export stats to JSON
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        let stats = self.get_all_stats();
        serde_json::to_string_pretty(&stats)
    }

    /// Start background task to update historical stats periodically
    pub fn start_background_updater(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let interval = self.report_interval;

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            loop {
                interval_timer.tick().await;
                self.update_historical_stats();
                log::debug!("Updated historical bandwidth stats");
            }
        })
    }
}

impl Default for BandwidthTracker {
    fn default() -> Self {
        Self::new(60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bandwidth_tracking() {
        let tracker = BandwidthTracker::new(1);

        // Record some bandwidth
        tracker.record_ingress("tenant1", 1000);
        tracker.record_egress("tenant1", 500);

        // Get stats
        let stats = tracker.get_stats("tenant1").unwrap();
        assert_eq!(stats.bytes_in, 1000);
        assert_eq!(stats.bytes_out, 500);
        assert_eq!(stats.request_count, 1);

        // Record more
        tracker.record_ingress("tenant1", 2000);
        let stats = tracker.get_stats("tenant1").unwrap();
        assert_eq!(stats.bytes_in, 3000);
        assert_eq!(stats.request_count, 2);
    }

    #[test]
    fn test_multiple_entities() {
        let tracker = BandwidthTracker::new(1);

        tracker.record_ingress("tenant1", 1000);
        tracker.record_ingress("tenant2", 2000);
        tracker.record_ingress("product1", 3000);

        let all_stats = tracker.get_all_stats();
        assert_eq!(all_stats.len(), 3);
    }

    #[test]
    fn test_reset() {
        let tracker = BandwidthTracker::new(1);

        tracker.record_ingress("tenant1", 1000);
        tracker.reset("tenant1");

        let stats = tracker.get_stats("tenant1");
        assert!(stats.is_none() || stats.unwrap().bytes_in == 0);
    }

    #[test]
    fn test_export_json() {
        let tracker = BandwidthTracker::new(1);

        tracker.record_ingress("tenant1", 1000);
        tracker.record_egress("tenant1", 500);

        let json = tracker.export_json().unwrap();
        assert!(json.contains("tenant1"));
        assert!(json.contains("bytes_in"));
    }
}
