//! Shared types and identifiers for Murmur.
//!
//! This crate defines all core types used across the Murmur workspace:
//! identifiers ([`DeviceId`], [`BlobHash`], [`NetworkId`]),
//! device types ([`DeviceRole`], [`DeviceInfo`]),
//! file metadata ([`FileMetadata`]),
//! access control ([`AccessGrant`], [`AccessScope`]),
//! DAG actions ([`Action`]),
//! the hybrid logical clock ([`HybridClock`]),
//! and gossip payload types ([`GossipPayload`], [`GossipMessage`]).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ID types
// ---------------------------------------------------------------------------

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Create an ID from raw bytes.
            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// Create an ID by hashing arbitrary data with BLAKE3.
            pub fn from_data(data: &[u8]) -> Self {
                Self(blake3::hash(data).into())
            }

            /// Return the raw 32-byte representation.
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl From<[u8; 32]> for $name {
            fn from(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in &self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }
    };
}

define_id!(
    /// Device identifier — an Ed25519 public key (= iroh NodeId).
    DeviceId
);

impl DeviceId {
    /// Create a `DeviceId` from an Ed25519 verifying key.
    pub fn from_verifying_key(key: &ed25519_dalek::VerifyingKey) -> Self {
        Self(key.to_bytes())
    }
}

define_id!(
    /// Content-addressed identifier for a blob: `blake3(file_content)`.
    BlobHash
);

define_id!(
    /// Network identifier derived from the seed: `blake3(hkdf(seed, "murmur/network-id"))`.
    NetworkId
);

// ---------------------------------------------------------------------------
// Device types
// ---------------------------------------------------------------------------

/// Role of a device in the Murmur network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceRole {
    /// Produces files (phone, tablet).
    Source,
    /// Stores files (NAS, server).
    Backup,
    /// Both source and backup.
    Full,
}

/// Information about a device in the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// The device's unique identifier.
    pub device_id: DeviceId,
    /// Human-readable name (e.g. "Max's iPhone").
    pub name: String,
    /// The device's role.
    pub role: DeviceRole,
    /// Whether the device has been approved.
    pub approved: bool,
    /// Which device approved this one.
    pub approved_by: Option<DeviceId>,
    /// HLC timestamp when the device joined.
    pub joined_at: u64,
}

// ---------------------------------------------------------------------------
// File metadata
// ---------------------------------------------------------------------------

/// Metadata for a file tracked by Murmur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Content hash of the file.
    pub blob_hash: BlobHash,
    /// Original filename.
    pub filename: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (if known).
    pub mime_type: Option<String>,
    /// Filesystem creation timestamp.
    pub created_at: u64,
    /// Which device created this file.
    pub device_origin: DeviceId,
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

/// A grant of temporary access from one device to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessGrant {
    /// Device receiving access.
    pub to: DeviceId,
    /// Device granting access.
    pub from: DeviceId,
    /// Scope of the grant.
    pub scope: AccessScope,
    /// Expiration unix timestamp.
    pub expires_at: u64,
    /// Ed25519 signature over the grant (first half).
    pub signature_r: [u8; 32],
    /// Ed25519 signature over the grant (second half).
    pub signature_s: [u8; 32],
}

/// Scope of an access grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessScope {
    /// Access to all files.
    AllFiles,
    /// Access to files matching a prefix.
    FilesByPrefix(String),
    /// Access to a single file.
    SingleFile(BlobHash),
}

// ---------------------------------------------------------------------------
// DAG action
// ---------------------------------------------------------------------------

/// A mutation action recorded in the DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// A device requests to join the network.
    DeviceJoinRequest {
        /// The joining device's ID.
        device_id: DeviceId,
        /// Human-readable name.
        name: String,
    },
    /// A device has been approved to join.
    DeviceApproved {
        /// The approved device's ID.
        device_id: DeviceId,
        /// The approved device's role.
        role: DeviceRole,
    },
    /// A device has been revoked.
    DeviceRevoked {
        /// The revoked device's ID.
        device_id: DeviceId,
    },
    /// A device's name has changed.
    DeviceNameChanged {
        /// The device whose name changed.
        device_id: DeviceId,
        /// The new name.
        name: String,
    },
    /// A file has been added.
    FileAdded {
        /// The file's metadata.
        metadata: FileMetadata,
    },
    /// A file has been deleted.
    FileDeleted {
        /// The deleted file's hash.
        blob_hash: BlobHash,
    },
    /// Access has been granted.
    AccessGranted {
        /// The access grant.
        grant: AccessGrant,
    },
    /// Access has been revoked for a device.
    AccessRevoked {
        /// The device whose access is revoked.
        to: DeviceId,
    },
    /// DAG merge — converges multiple tips into one.
    Merge,
    /// Snapshot — records a materialized state hash.
    Snapshot {
        /// blake3 hash of the serialized materialized state.
        state_hash: [u8; 32],
    },
}

impl Action {
    /// Return a human-readable name for this action variant (for error messages).
    pub fn action_name(&self) -> &'static str {
        match self {
            Action::DeviceJoinRequest { .. } => "DeviceJoinRequest",
            Action::DeviceApproved { .. } => "DeviceApproved",
            Action::DeviceRevoked { .. } => "DeviceRevoked",
            Action::DeviceNameChanged { .. } => "DeviceNameChanged",
            Action::FileAdded { .. } => "FileAdded",
            Action::FileDeleted { .. } => "FileDeleted",
            Action::AccessGranted { .. } => "AccessGranted",
            Action::AccessRevoked { .. } => "AccessRevoked",
            Action::Merge => "Merge",
            Action::Snapshot { .. } => "Snapshot",
        }
    }
}

// ---------------------------------------------------------------------------
// Hybrid Logical Clock
// ---------------------------------------------------------------------------

/// A hybrid logical clock combining wall-clock time with a logical counter.
///
/// Produces monotonically increasing timestamps (nanoseconds since UNIX epoch)
/// that are always at least as large as the wall clock and strictly increasing
/// even when the wall clock hasn't advanced. Thread-safe via `AtomicU64`.
pub struct HybridClock {
    last: AtomicU64,
}

impl HybridClock {
    /// Create a new clock initialised to the current wall-clock time.
    pub fn new() -> Self {
        let now = wall_clock_nanos();
        Self {
            last: AtomicU64::new(now),
        }
    }

    /// Advance and return a new unique timestamp.
    ///
    /// The returned value is `max(wall_clock, last) + 1`, guaranteeing strict
    /// monotonicity even under rapid successive calls or clock skew.
    pub fn tick(&self) -> u64 {
        loop {
            let prev = self.last.load(Ordering::SeqCst);
            let now = wall_clock_nanos();
            let candidate = prev.max(now) + 1;

            if self
                .last
                .compare_exchange(prev, candidate, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return candidate;
            }
        }
    }

    /// Witness a remote HLC timestamp, advancing the local clock if necessary.
    ///
    /// After witnessing, `last = max(last, remote_hlc)`. The next [`tick`](Self::tick)
    /// will produce a value strictly greater than both.
    pub fn witness(&self, remote_hlc: u64) {
        self.last.fetch_max(remote_hlc, Ordering::SeqCst);
    }

    /// Return the current clock value without advancing it.
    pub fn current(&self) -> u64 {
        self.last.load(Ordering::SeqCst)
    }
}

impl Default for HybridClock {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HybridClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HybridClock")
            .field("last", &self.last.load(Ordering::SeqCst))
            .finish()
    }
}

/// Current wall-clock time in nanoseconds since UNIX epoch.
fn wall_clock_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Gossip types
// ---------------------------------------------------------------------------

/// Payload types for gossip broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GossipPayload {
    /// A DAG entry broadcast (serialized bytes).
    DagEntry {
        /// Postcard-serialized DAG entry.
        entry_bytes: Vec<u8>,
    },
    /// A membership event (device join/leave notifications).
    MembershipEvent {
        /// The device this event concerns.
        device_id: DeviceId,
        /// Whether the device is now online.
        online: bool,
    },
    /// Request missing DAG entries from a peer.
    DagSyncRequest {
        /// The requester's current tip hashes.
        tips: Vec<[u8; 32]>,
    },
    /// Response with missing DAG entries (delta).
    DagSyncResponse {
        /// Serialized DAG entries (each as postcard bytes) in topological order.
        entries: Vec<Vec<u8>>,
    },
    /// Request a blob from peers.
    BlobRequest {
        /// Content hash of the requested blob.
        blob_hash: BlobHash,
    },
    /// Response with blob data (for small blobs, or when chunking is not needed).
    BlobResponse {
        /// Content hash of the blob.
        blob_hash: BlobHash,
        /// Blob data (empty if sender doesn't have it).
        data: Vec<u8>,
    },
    /// A chunk of a large blob being transferred.
    BlobChunk {
        /// Content hash of the full blob.
        blob_hash: BlobHash,
        /// Zero-based chunk index.
        chunk_index: u32,
        /// Total number of chunks.
        total_chunks: u32,
        /// Chunk data.
        data: Vec<u8>,
    },
}

/// Wire envelope for gossip messages.
///
/// Wraps a [`GossipPayload`] with a random nonce so that PlumTree
/// (iroh-gossip) never deduplicates two distinct broadcasts that happen
/// to have identical payload bytes. The `sender` field identifies the
/// originating device for pre-DAG authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Random nonce to guarantee byte-level uniqueness.
    pub nonce: u64,
    /// The device that originated this message.
    pub sender: DeviceId,
    /// The actual payload.
    pub payload: GossipPayload,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    // --- ID type tests ---

    #[test]
    fn test_device_id_from_verifying_key() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let id = DeviceId::from_verifying_key(&verifying_key);
        assert_eq!(id.as_bytes(), &verifying_key.to_bytes());
    }

    #[test]
    fn test_device_id_from_data_deterministic() {
        let id1 = DeviceId::from_data(b"test device");
        let id2 = DeviceId::from_data(b"test device");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_different_data_different_id() {
        let id1 = DeviceId::from_data(b"device a");
        let id2 = DeviceId::from_data(b"device b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_id_from_bytes_roundtrip() {
        let bytes = [42u8; 32];
        let id = BlobHash::from_bytes(bytes);
        assert_eq!(id.as_bytes(), &bytes);
    }

    #[test]
    fn test_id_from_trait() {
        let bytes = [7u8; 32];
        let id = NetworkId::from(bytes);
        assert_eq!(id.as_bytes(), &bytes);
    }

    #[test]
    fn test_id_as_ref() {
        let id = DeviceId::from_data(b"test");
        let slice: &[u8] = id.as_ref();
        assert_eq!(slice.len(), 32);
    }

    #[test]
    fn test_display_outputs_hex() {
        let bytes = [
            0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7,
            0xe8, 0xf9, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ];
        let id = DeviceId::from(bytes);
        let hex = id.to_string();
        assert_eq!(
            hex,
            "0a1b2c3d4e5f60718293a4b5c6d7e8f900112233445566778899aabbccddeeff"
        );
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn test_debug_format() {
        let id = DeviceId::from([0u8; 32]);
        let debug = format!("{id:?}");
        assert!(debug.starts_with("DeviceId("));
        assert!(debug.ends_with(')'));
    }

    #[test]
    fn test_id_ordering() {
        let id_low = DeviceId::from([0u8; 32]);
        let id_high = DeviceId::from([0xffu8; 32]);
        assert!(id_low < id_high);
    }

    #[test]
    fn test_id_hash_in_set() {
        use std::collections::HashSet;
        let id1 = DeviceId::from_data(b"a");
        let id2 = DeviceId::from_data(b"b");
        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2);
        set.insert(id1); // duplicate
        assert_eq!(set.len(), 2);
    }

    // --- Postcard roundtrip tests ---

    #[test]
    fn test_device_id_roundtrip_postcard() {
        let id = DeviceId::from_data(b"device");
        let encoded = postcard::to_allocvec(&id).unwrap();
        let decoded: DeviceId = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_blob_hash_roundtrip_postcard() {
        let id = BlobHash::from_data(b"blob content");
        let encoded = postcard::to_allocvec(&id).unwrap();
        let decoded: BlobHash = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_network_id_roundtrip_postcard() {
        let id = NetworkId::from_data(b"network");
        let encoded = postcard::to_allocvec(&id).unwrap();
        let decoded: NetworkId = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_device_role_roundtrip_postcard() {
        for role in [DeviceRole::Source, DeviceRole::Backup, DeviceRole::Full] {
            let encoded = postcard::to_allocvec(&role).unwrap();
            let decoded: DeviceRole = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(role, decoded);
        }
    }

    #[test]
    fn test_device_info_roundtrip_postcard() {
        let info = DeviceInfo {
            device_id: DeviceId::from_data(b"dev1"),
            name: "Test Device".to_string(),
            role: DeviceRole::Full,
            approved: true,
            approved_by: Some(DeviceId::from_data(b"admin")),
            joined_at: 1234567890,
        };
        let encoded = postcard::to_allocvec(&info).unwrap();
        let decoded: DeviceInfo = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(info, decoded);
    }

    #[test]
    fn test_file_metadata_roundtrip_postcard() {
        let meta = FileMetadata {
            blob_hash: BlobHash::from_data(b"file content"),
            filename: "photo.jpg".to_string(),
            size: 1024,
            mime_type: Some("image/jpeg".to_string()),
            created_at: 1700000000,
            device_origin: DeviceId::from_data(b"phone"),
        };
        let encoded = postcard::to_allocvec(&meta).unwrap();
        let decoded: FileMetadata = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn test_access_grant_roundtrip_postcard() {
        let grant = AccessGrant {
            to: DeviceId::from_data(b"tablet"),
            from: DeviceId::from_data(b"phone"),
            scope: AccessScope::AllFiles,
            expires_at: 1700003600,
            signature_r: [0xab; 32],
            signature_s: [0xcd; 32],
        };
        let encoded = postcard::to_allocvec(&grant).unwrap();
        let decoded: AccessGrant = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(grant, decoded);
    }

    #[test]
    fn test_access_scope_variants_roundtrip_postcard() {
        let scopes = vec![
            AccessScope::AllFiles,
            AccessScope::FilesByPrefix("photos/2025/".to_string()),
            AccessScope::SingleFile(BlobHash::from_data(b"specific file")),
        ];
        for scope in scopes {
            let encoded = postcard::to_allocvec(&scope).unwrap();
            let decoded: AccessScope = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(scope, decoded);
        }
    }

    #[test]
    fn test_action_device_join_request_roundtrip() {
        let action = Action::DeviceJoinRequest {
            device_id: DeviceId::from_data(b"new device"),
            name: "New Phone".to_string(),
        };
        let encoded = postcard::to_allocvec(&action).unwrap();
        let decoded: Action = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(action, decoded);
    }

    #[test]
    fn test_action_all_variants_roundtrip() {
        let actions = vec![
            Action::DeviceJoinRequest {
                device_id: DeviceId::from_data(b"d1"),
                name: "Phone".to_string(),
            },
            Action::DeviceApproved {
                device_id: DeviceId::from_data(b"d1"),
                role: DeviceRole::Source,
            },
            Action::DeviceRevoked {
                device_id: DeviceId::from_data(b"d1"),
            },
            Action::DeviceNameChanged {
                device_id: DeviceId::from_data(b"d1"),
                name: "New Name".to_string(),
            },
            Action::FileAdded {
                metadata: FileMetadata {
                    blob_hash: BlobHash::from_data(b"file"),
                    filename: "test.txt".to_string(),
                    size: 100,
                    mime_type: None,
                    created_at: 0,
                    device_origin: DeviceId::from_data(b"d1"),
                },
            },
            Action::FileDeleted {
                blob_hash: BlobHash::from_data(b"file"),
            },
            Action::AccessGranted {
                grant: AccessGrant {
                    to: DeviceId::from_data(b"d2"),
                    from: DeviceId::from_data(b"d1"),
                    scope: AccessScope::AllFiles,
                    expires_at: 9999,
                    signature_r: [0; 32],
                    signature_s: [0; 32],
                },
            },
            Action::AccessRevoked {
                to: DeviceId::from_data(b"d2"),
            },
            Action::Merge,
            Action::Snapshot {
                state_hash: [0xfe; 32],
            },
        ];

        for action in actions {
            let encoded = postcard::to_allocvec(&action).unwrap();
            let decoded: Action = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(action, decoded);
        }
    }

    // --- HLC tests ---

    #[test]
    fn test_hlc_tick_monotonic() {
        let clock = HybridClock::new();
        let mut prev = clock.tick();
        for _ in 0..1000 {
            let next = clock.tick();
            assert!(next > prev, "tick must be strictly increasing");
            prev = next;
        }
    }

    #[test]
    fn test_hlc_tick_advances_beyond_wall_clock() {
        let clock = HybridClock::new();
        let t1 = clock.tick();
        let t2 = clock.tick();
        let t3 = clock.tick();
        assert!(t2 > t1);
        assert!(t3 > t2);
    }

    #[test]
    fn test_hlc_witness_advances_local() {
        let clock = HybridClock::new();
        let far_future = clock.current() + 1_000_000_000; // 1 second ahead
        clock.witness(far_future);
        assert!(
            clock.current() >= far_future,
            "witness should advance local clock"
        );
        let next = clock.tick();
        assert!(next > far_future, "tick after witness should exceed remote");
    }

    #[test]
    fn test_hlc_witness_no_retreat() {
        let clock = HybridClock::new();
        let current = clock.tick();
        let past = current.saturating_sub(1_000_000);
        clock.witness(past);
        assert!(
            clock.current() >= current,
            "witness with past value should not retreat"
        );
    }

    #[test]
    fn test_hlc_concurrent_ticks_unique() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let clock = Arc::new(HybridClock::new());
        let n_threads = 4;
        let ticks_per_thread = 1000;

        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let clock = clock.clone();
            handles.push(std::thread::spawn(move || {
                let mut values = Vec::with_capacity(ticks_per_thread);
                for _ in 0..ticks_per_thread {
                    values.push(clock.tick());
                }
                values
            }));
        }

        let mut all_values = HashSet::new();
        for h in handles {
            for v in h.join().unwrap() {
                assert!(
                    all_values.insert(v),
                    "concurrent tick produced duplicate value"
                );
            }
        }
        assert_eq!(all_values.len(), n_threads * ticks_per_thread);
    }

    // --- Gossip types ---

    #[test]
    fn test_gossip_payload_dag_entry_roundtrip() {
        let payload = GossipPayload::DagEntry {
            entry_bytes: vec![1, 2, 3, 4, 5],
        };
        let encoded = postcard::to_allocvec(&payload).unwrap();
        let decoded: GossipPayload = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(payload, decoded);
    }

    #[test]
    fn test_gossip_payload_membership_roundtrip() {
        let payload = GossipPayload::MembershipEvent {
            device_id: DeviceId::from_data(b"node"),
            online: true,
        };
        let encoded = postcard::to_allocvec(&payload).unwrap();
        let decoded: GossipPayload = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(payload, decoded);
    }

    #[test]
    fn test_gossip_message_roundtrip() {
        let msg = GossipMessage {
            nonce: 42,
            sender: DeviceId::from_data(b"test-sender"),
            payload: GossipPayload::DagEntry {
                entry_bytes: vec![10, 20],
            },
        };
        let encoded = postcard::to_allocvec(&msg).unwrap();
        let decoded: GossipMessage = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }
}
