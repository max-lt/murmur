//! Platform callback trait.
//!
//! The engine calls these methods to communicate with the platform.
//! The platform implements this trait to persist data, serve blobs,
//! and display events to the user.

use murmur_types::BlobHash;

use crate::EngineEvent;

/// Callbacks from the engine to the platform.
///
/// The platform implements this trait. All methods have default no-op
/// implementations so platforms can override only what they need.
pub trait PlatformCallbacks: Send + Sync {
    /// Persist a new DAG entry (serialized bytes).
    fn on_dag_entry(&self, entry_bytes: Vec<u8>) {
        let _ = entry_bytes;
    }

    /// Store a received blob.
    fn on_blob_received(&self, blob_hash: BlobHash, data: Vec<u8>) {
        let _ = (blob_hash, data);
    }

    /// Load a blob for transfer to a peer. Returns `None` if not available.
    fn on_blob_needed(&self, blob_hash: BlobHash) -> Option<Vec<u8>> {
        let _ = blob_hash;
        None
    }

    /// Notify the platform of an engine event.
    fn on_event(&self, event: EngineEvent) {
        let _ = event;
    }

    // --- Streaming blob transfer callbacks ---

    /// Called when a streaming blob transfer begins.
    ///
    /// The platform should prepare to receive chunks (e.g., create a temp file).
    fn on_blob_stream_start(&self, blob_hash: BlobHash, total_size: u64) {
        let _ = (blob_hash, total_size);
    }

    /// Called with each chunk of a streaming blob transfer.
    ///
    /// `offset` is the byte offset within the full blob. The platform should
    /// append/write the data to its temporary storage.
    fn on_blob_chunk(&self, blob_hash: BlobHash, offset: u64, data: &[u8]) {
        let _ = (blob_hash, offset, data);
    }

    /// Called when a streaming blob transfer is complete.
    ///
    /// The platform should verify the blob's integrity (blake3 hash) and
    /// finalize storage (e.g., rename temp file to final path). Returns an
    /// error string if verification or storage fails.
    fn on_blob_stream_complete(&self, blob_hash: BlobHash) -> Result<(), String> {
        let _ = blob_hash;
        Ok(())
    }

    /// Called when a streaming blob transfer is aborted (e.g., due to error).
    ///
    /// The platform should clean up any temporary storage.
    fn on_blob_stream_abort(&self, blob_hash: BlobHash) {
        let _ = blob_hash;
    }
}
