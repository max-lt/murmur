//! Daemon-side transfer progress tracking with smoothed speed / ETA (M31).
//!
//! Raw `bytes/elapsed` is too jittery to produce useful ETAs on real
//! networks. We smooth the per-blob speed with an EWMA so all clients
//! (desktop, CLI, mobile) render identical, stable numbers.
//!
//! The smoothing formula matches the plan:
//! `speed_t = α·instantaneous + (1−α)·speed_{t−1}`, α ≈ 0.2.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use murmur_types::BlobHash;

/// EWMA smoothing factor.
///
/// Chosen per the plan (α ≈ 0.2): reactive enough to catch genuine speed
/// changes within a 30-second window but damped enough to swallow single-
/// sample spikes from congested networks.
pub const EWMA_ALPHA: f64 = 0.2;

/// One in-flight transfer's live statistics.
#[derive(Debug, Clone)]
pub struct TransferStats {
    /// UNIX timestamp (seconds) when the transfer was first observed.
    pub started_at_unix: u64,
    /// UNIX timestamp (seconds) of the most recent progress sample.
    pub last_progress_unix: u64,
    /// Bytes transferred at the last sample.
    pub bytes_transferred: u64,
    /// Total blob size in bytes (0 if unknown).
    pub total_bytes: u64,
    /// Smoothed transfer speed in bytes/sec (EWMA).
    pub bytes_per_sec_smoothed: f64,
}

impl TransferStats {
    /// Estimated seconds until completion, or `None` if:
    /// - total size is unknown (0),
    /// - already complete, or
    /// - smoothed speed is too low to produce a useful estimate.
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.total_bytes == 0 || self.bytes_transferred >= self.total_bytes {
            return None;
        }
        if self.bytes_per_sec_smoothed < 1.0 {
            return None;
        }
        let remaining = self.total_bytes.saturating_sub(self.bytes_transferred) as f64;
        let secs = remaining / self.bytes_per_sec_smoothed;
        if !secs.is_finite() || secs < 0.0 {
            return None;
        }
        Some(secs.round() as u64)
    }
}

/// Thread-safe tracker keyed by blob hash.
///
/// Transfers are removed when they complete (`bytes_transferred >= total_bytes`
/// and total is known) or can be evicted explicitly via [`Self::remove`]
/// when the caller sees the transfer finish some other way.
#[derive(Debug, Default)]
pub struct TransferTracker {
    transfers: Mutex<HashMap<BlobHash, TransferStats>>,
}

impl TransferTracker {
    /// Create a new empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a progress sample for `blob_hash`. Computes the smoothed
    /// speed using the time delta since the previous sample.
    ///
    /// Calling this with the same `bytes_transferred` twice in the same
    /// second is a no-op for the EWMA (time delta is 0), but the
    /// `last_progress_unix` timestamp still advances.
    pub fn record(&self, blob_hash: BlobHash, bytes_transferred: u64, total_bytes: u64) {
        let now = now_unix_secs();
        self.record_at(blob_hash, bytes_transferred, total_bytes, now);
    }

    /// Record a progress sample at a caller-supplied timestamp. Used by
    /// tests so we can feed a deterministic clock.
    pub fn record_at(
        &self,
        blob_hash: BlobHash,
        bytes_transferred: u64,
        total_bytes: u64,
        now_unix: u64,
    ) {
        let mut map = self.transfers.lock().unwrap();
        match map.get_mut(&blob_hash) {
            Some(stats) => {
                let dt = now_unix.saturating_sub(stats.last_progress_unix);
                if dt > 0 {
                    // Instantaneous bytes/sec for this window.
                    let delta = bytes_transferred.saturating_sub(stats.bytes_transferred) as f64;
                    let instant = delta / dt as f64;
                    stats.bytes_per_sec_smoothed =
                        EWMA_ALPHA * instant + (1.0 - EWMA_ALPHA) * stats.bytes_per_sec_smoothed;
                }
                stats.bytes_transferred = bytes_transferred;
                if total_bytes > 0 {
                    stats.total_bytes = total_bytes;
                }
                stats.last_progress_unix = now_unix;
            }
            None => {
                map.insert(
                    blob_hash,
                    TransferStats {
                        started_at_unix: now_unix,
                        last_progress_unix: now_unix,
                        bytes_transferred,
                        total_bytes,
                        bytes_per_sec_smoothed: 0.0,
                    },
                );
            }
        }
    }

    /// Snapshot the current stats for `blob_hash`, if any.
    pub fn get(&self, blob_hash: &BlobHash) -> Option<TransferStats> {
        self.transfers.lock().unwrap().get(blob_hash).cloned()
    }

    /// Remove a completed/abandoned transfer.
    pub fn remove(&self, blob_hash: &BlobHash) {
        self.transfers.lock().unwrap().remove(blob_hash);
    }

    /// Drop any transfers that haven't been updated in `max_age_secs` seconds.
    ///
    /// Useful after a daemon has been idle — keeps the snapshot surfaced in
    /// `TransferStatus` from growing unbounded. Not called from the hot
    /// path yet; a periodic tick can run it alongside the conflict-expiry
    /// task.
    #[allow(dead_code)]
    pub fn prune_stale(&self, now_unix: u64, max_age_secs: u64) {
        let mut map = self.transfers.lock().unwrap();
        map.retain(|_, s| now_unix.saturating_sub(s.last_progress_unix) <= max_age_secs);
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(n: u8) -> BlobHash {
        BlobHash::from_bytes([n; 32])
    }

    #[test]
    fn test_records_initial_sample() {
        let t = TransferTracker::new();
        t.record_at(blob(1), 0, 1000, 1000);
        let s = t.get(&blob(1)).unwrap();
        assert_eq!(s.started_at_unix, 1000);
        assert_eq!(s.last_progress_unix, 1000);
        assert_eq!(s.bytes_transferred, 0);
        assert_eq!(s.total_bytes, 1000);
        assert_eq!(s.bytes_per_sec_smoothed, 0.0);
        assert_eq!(s.eta_seconds(), None);
    }

    #[test]
    fn test_ewma_ramps_up_and_converges() {
        // Feed a steady 1 MB/s for 30 seconds. The smoothed speed must
        // converge to within ±10% of that after the 30-second window,
        // matching the plan-level acceptance criterion.
        let t = TransferTracker::new();
        let hash = blob(2);
        let bytes_per_sec: u64 = 1_000_000;
        let total: u64 = bytes_per_sec * 60;
        // Prime with an initial sample at t=0.
        t.record_at(hash, 0, total, 0);
        for tick in 1..=30 {
            let bytes = bytes_per_sec * tick;
            t.record_at(hash, bytes, total, tick);
        }
        let stats = t.get(&hash).unwrap();
        let actual = stats.bytes_per_sec_smoothed;
        let target = bytes_per_sec as f64;
        let relative = (actual - target).abs() / target;
        assert!(
            relative < 0.10,
            "EWMA should converge within ±10% after 30s, got {actual:.0} vs {target:.0} (rel={relative:.3})"
        );
        // ETA at 30s in: 30 seconds of bytes done → 30 seconds remaining.
        let eta = stats.eta_seconds().expect("eta should be Some");
        let eta_diff = eta as i64 - 30;
        assert!(
            eta_diff.abs() <= 3,
            "ETA should stabilise within ±3s of true remaining time, got {eta}s"
        );
    }

    #[test]
    fn test_zero_time_delta_is_no_op_for_ewma() {
        // Two samples at the same second should not change the smoothed
        // speed (division-by-zero guard).
        let t = TransferTracker::new();
        let hash = blob(3);
        t.record_at(hash, 0, 1000, 100);
        t.record_at(hash, 500, 1000, 100);
        let stats = t.get(&hash).unwrap();
        assert_eq!(stats.bytes_per_sec_smoothed, 0.0);
        assert_eq!(stats.bytes_transferred, 500);
    }

    #[test]
    fn test_eta_none_when_complete() {
        let t = TransferTracker::new();
        let hash = blob(4);
        t.record_at(hash, 0, 1000, 0);
        t.record_at(hash, 1000, 1000, 10);
        assert_eq!(t.get(&hash).unwrap().eta_seconds(), None);
    }

    #[test]
    fn test_eta_none_when_total_unknown() {
        let t = TransferTracker::new();
        let hash = blob(5);
        t.record_at(hash, 0, 0, 0);
        t.record_at(hash, 5000, 0, 5);
        assert_eq!(t.get(&hash).unwrap().eta_seconds(), None);
    }

    #[test]
    fn test_prune_stale_drops_idle_transfers() {
        let t = TransferTracker::new();
        t.record_at(blob(6), 0, 100, 0);
        t.record_at(blob(7), 0, 100, 100);
        t.prune_stale(120, 30);
        assert!(t.get(&blob(6)).is_none());
        assert!(t.get(&blob(7)).is_some());
    }

    #[test]
    fn test_remove_evicts_transfer() {
        let t = TransferTracker::new();
        let hash = blob(8);
        t.record_at(hash, 0, 100, 0);
        assert!(t.get(&hash).is_some());
        t.remove(&hash);
        assert!(t.get(&hash).is_none());
    }
}
