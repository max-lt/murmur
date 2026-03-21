//! Optional Prometheus metrics for murmurd.
//!
//! Enabled with the `metrics` feature flag. When disabled, all recording
//! functions are no-ops.

#[cfg(feature = "metrics")]
use prometheus::{
    Counter, CounterVec, Gauge, Histogram, register_counter, register_counter_vec, register_gauge,
    register_histogram,
};

#[cfg(feature = "metrics")]
use std::sync::LazyLock;

#[cfg(feature = "metrics")]
static DAG_ENTRIES_TOTAL: LazyLock<Counter> = LazyLock::new(|| {
    register_counter!(
        "murmur_dag_entries_total",
        "Total number of DAG entries received"
    )
    .expect("register murmur_dag_entries_total")
});

#[cfg(feature = "metrics")]
static BLOBS_STORED_BYTES: LazyLock<Gauge> = LazyLock::new(|| {
    register_gauge!(
        "murmur_blobs_stored_bytes",
        "Total bytes of stored blobs on disk"
    )
    .expect("register murmur_blobs_stored_bytes")
});

#[cfg(feature = "metrics")]
static BLOBS_PENDING_SYNC: LazyLock<Gauge> = LazyLock::new(|| {
    register_gauge!(
        "murmur_blobs_pending_sync",
        "Number of blobs pending push sync to peers"
    )
    .expect("register murmur_blobs_pending_sync")
});

#[cfg(feature = "metrics")]
static SYNC_DURATION_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    register_histogram!(
        "murmur_sync_duration_seconds",
        "Duration of DAG sync operations in seconds"
    )
    .expect("register murmur_sync_duration_seconds")
});

#[cfg(feature = "metrics")]
static CONNECTED_PEERS: LazyLock<Gauge> = LazyLock::new(|| {
    register_gauge!(
        "murmur_connected_peers",
        "Number of currently connected gossip peers"
    )
    .expect("register murmur_connected_peers")
});

#[cfg(feature = "metrics")]
static BLOB_TRANSFER_BYTES_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "murmur_blob_transfer_bytes_total",
        "Total bytes transferred for blob sync",
        &["direction"]
    )
    .expect("register murmur_blob_transfer_bytes_total")
});

/// Record that a DAG entry was received/created.
pub fn record_dag_entry() {
    #[cfg(feature = "metrics")]
    DAG_ENTRIES_TOTAL.inc();
}

/// Set the total stored blob bytes gauge.
#[allow(dead_code)] // Will be called when blob storage size tracking is added.
pub fn set_blobs_stored_bytes(_bytes: u64) {
    #[cfg(feature = "metrics")]
    BLOBS_STORED_BYTES.set(_bytes as f64);
}

/// Set the pending sync queue size.
pub fn set_blobs_pending_sync(_count: u64) {
    #[cfg(feature = "metrics")]
    BLOBS_PENDING_SYNC.set(_count as f64);
}

/// Record a sync duration.
pub fn observe_sync_duration(_seconds: f64) {
    #[cfg(feature = "metrics")]
    SYNC_DURATION_SECONDS.observe(_seconds);
}

/// Set the connected peers gauge.
pub fn set_connected_peers(_count: u64) {
    #[cfg(feature = "metrics")]
    CONNECTED_PEERS.set(_count as f64);
}

/// Record bytes transferred (upload or download).
pub fn record_blob_transfer_bytes(_direction: &str, _bytes: u64) {
    #[cfg(feature = "metrics")]
    BLOB_TRANSFER_BYTES_TOTAL
        .with_label_values(&[_direction])
        .inc_by(_bytes as f64);
}

/// Encode all registered metrics in Prometheus text format.
#[cfg(feature = "metrics")]
pub fn encode_metrics() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let mut buf = Vec::new();
    encoder.encode(&families, &mut buf).expect("encode metrics");
    String::from_utf8(buf).expect("metrics are valid UTF-8")
}
