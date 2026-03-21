//! Engine events emitted to the platform.

use murmur_types::{BlobHash, DeviceId, DeviceRole};

/// Events emitted by the engine to notify the platform of state changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A new device wants to join the network.
    DeviceJoinRequested {
        /// The joining device's ID.
        device_id: DeviceId,
        /// Human-readable name.
        name: String,
    },
    /// A device was approved.
    DeviceApproved {
        /// The approved device's ID.
        device_id: DeviceId,
        /// The approved device's role.
        role: DeviceRole,
    },
    /// A device was revoked.
    DeviceRevoked {
        /// The revoked device's ID.
        device_id: DeviceId,
    },
    /// A file was synced (DAG entry received).
    FileSynced {
        /// The file's content hash.
        blob_hash: BlobHash,
        /// The filename.
        filename: String,
    },
    /// A blob was received from a peer.
    BlobReceived {
        /// The blob's content hash.
        blob_hash: BlobHash,
    },
    /// Access was requested by a remote device.
    AccessRequested {
        /// The requesting device.
        from: DeviceId,
    },
    /// Access was granted.
    AccessGranted {
        /// The device that received access.
        to: DeviceId,
    },
    /// The DAG was synced with a peer.
    DagSynced {
        /// Number of new entries received.
        new_entries: usize,
    },
    /// Network created (first device).
    NetworkCreated {
        /// This device's ID.
        device_id: DeviceId,
    },
    /// Joined an existing network (pending approval).
    NetworkJoined {
        /// This device's ID.
        device_id: DeviceId,
    },
    /// Progress update for an in-flight blob transfer.
    BlobTransferProgress {
        /// The blob being transferred.
        blob_hash: BlobHash,
        /// Bytes transferred so far.
        bytes_transferred: u64,
        /// Total blob size.
        total_bytes: u64,
    },
}
