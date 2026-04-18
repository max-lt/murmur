//! IPC protocol types for communication between `murmur-cli` and `murmurd`.
//!
//! Communication happens over a Unix domain socket using length-prefixed
//! postcard serialization. This crate defines the request/response types
//! and the socket path convention.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod templates;

/// Error type for IPC operations.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// IO error during socket communication.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization/deserialization error.
    #[error("codec: {0}")]
    Codec(String),

    /// Message too large.
    #[error("message too large: {0} bytes (max {MAX_MESSAGE_SIZE})")]
    MessageTooLarge(usize),
}

/// Maximum IPC message size (1 MB).
const MAX_MESSAGE_SIZE: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// A request sent from `murmur-cli` to `murmurd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CliRequest {
    /// Query daemon status.
    Status,
    /// List approved devices.
    ListDevices,
    /// List devices pending approval.
    ListPending,
    /// Approve a pending device.
    ApproveDevice {
        /// Device ID as 64-character hex string.
        device_id_hex: String,
    },
    /// Revoke an approved device.
    RevokeDevice {
        /// Device ID as 64-character hex string.
        device_id_hex: String,
    },
    /// Show the network mnemonic.
    ShowMnemonic,
    /// List synced files.
    ListFiles,
    /// Add a file to the network.
    AddFile {
        /// Filesystem path to the file.
        path: String,
    },
    /// Query in-flight blob transfer status.
    TransferStatus,

    // -- Folder management (M17) --
    /// Create a new shared folder.
    CreateFolder {
        /// Human-readable folder name.
        name: String,
        /// Local directory path to sync. If provided, murmurd registers it in
        /// config and starts watching for files.
        local_path: Option<String>,
        /// Optional initial `.murmurignore` content (folder templates).
        /// Only written to disk when a `local_path` is also provided.
        ignore_patterns: Option<String>,
    },
    /// Remove a shared folder.
    RemoveFolder {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },
    /// List all shared folders.
    ListFolders,
    /// Subscribe this device to a folder.
    SubscribeFolder {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Human-readable folder name (optional; defaults to the folder's
        /// original name if omitted).
        name: Option<String>,
        /// Local directory path for the folder's files.
        local_path: String,
        /// Sync mode: "full", "send-only", or "receive-only".
        mode: String,
    },
    /// Unsubscribe this device from a folder.
    UnsubscribeFolder {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Whether to keep local files after unsubscribing.
        keep_local: bool,
    },
    /// List files in a specific folder.
    FolderFiles {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },
    /// Get status for a specific folder.
    FolderStatus {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },
    /// List active conflicts, optionally filtered by folder.
    ListConflicts {
        /// Optional folder ID filter (64-character hex string).
        folder_id_hex: Option<String>,
    },
    /// Resolve a conflict by choosing a version.
    ResolveConflict {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
        /// Chosen blob hash as 64-character hex string.
        chosen_hash_hex: String,
    },
    /// Get version history for a file.
    FileHistory {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
    },
    /// Change the sync mode for a folder subscription.
    SetFolderMode {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// New sync mode: "full", "send-only", or "receive-only".
        mode: String,
    },

    // -- M18 additions --
    /// Preview the first `max_bytes` bytes of a blob stored locally.
    BlobPreview {
        /// Blob hash as 64-character hex string.
        blob_hash_hex: String,
        /// Maximum number of bytes to return.
        max_bytes: u64,
    },
    /// Restore a historical file version by writing its blob to the local path.
    RestoreFileVersion {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
        /// Blob hash of the version to restore, as 64-character hex string.
        blob_hash_hex: String,
    },

    /// Subscribe to real-time engine events (long-lived connection).
    SubscribeEvents,

    // -- M19a: Zero-Config Onboarding --
    /// Get the current daemon configuration.
    GetConfig,
    /// Create and subscribe to the default "Murmur" folder.
    InitDefaultFolder {
        /// Local path for the default folder (e.g. `~/Murmur`).
        local_path: String,
    },

    // -- M20a: System Tray & Notifications --
    /// Pause all blob transfers globally.
    PauseSync,
    /// Resume all blob transfers globally.
    ResumeSync,

    // -- M21a: Folder Discovery & Selective Sync --
    /// List all folders on the network, including unsubscribed ones.
    ListNetworkFolders,
    /// List devices subscribed to a specific folder.
    FolderSubscribers {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },

    // -- M22a: Rich Conflict Resolution --
    /// Resolve all conflicts in a folder with a single strategy.
    BulkResolveConflicts {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Strategy: "keep_newest", "keep_local", or "keep_remote".
        strategy: String,
    },
    /// Set auto-resolve strategy for a folder.
    SetFolderAutoResolve {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Strategy: "none", "newest", or "mine".
        strategy: String,
    },
    /// Dismiss a conflict without choosing a version (keep both files).
    DismissConflict {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
    },

    // -- M23a: Device Management --
    /// Get per-device online/offline presence.
    GetDevicePresence,
    /// Rename the local device.
    SetDeviceName {
        /// New device name.
        name: String,
    },

    // -- M24a: Sync Progress --
    /// Pause sync for a single folder.
    PauseFolderSync {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },
    /// Resume sync for a single folder.
    ResumeFolderSync {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },

    // -- M25a: File Browser --
    /// Delete a file from a folder.
    DeleteFile {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
    },

    // -- M26a: Settings & Configuration --
    /// Toggle auto-approve for new devices.
    SetAutoApprove {
        /// Whether to auto-approve.
        enabled: bool,
    },
    /// Toggle mDNS LAN peer discovery.
    SetMdns {
        /// Whether mDNS is enabled.
        enabled: bool,
    },
    /// Delete orphaned blobs (no DAG entry). Returns bytes freed.
    ReclaimOrphanedBlobs,
    /// Change the local path for a subscribed folder.
    SetFolderLocalPath {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// New absolute local path.
        new_local_path: String,
    },
    /// Rename a folder's local display name.
    SetFolderName {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// New display name.
        name: String,
    },
    /// Read the `.murmurignore` file for a folder.
    GetIgnorePatterns {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
    },
    /// Write the `.murmurignore` file for a folder.
    SetIgnorePatterns {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Newline-separated ignore patterns.
        patterns: String,
    },
    /// Set bandwidth throttle limits.
    SetThrottle {
        /// Max upload bytes per second (0 = unlimited).
        upload_bytes_per_sec: u64,
        /// Max download bytes per second (0 = unlimited).
        download_bytes_per_sec: u64,
    },

    // -- M26a: Settings --
    /// Leave the network and wipe all daemon data (config, DAG, blobs, keys).
    /// User files in synced folders on disk are NOT deleted.
    /// The daemon shuts down after wiping.
    LeaveNetwork,

    // -- M27a: Diagnostics & Network Health --
    /// List connected peers with connection info.
    ListPeers,
    /// Get storage statistics (blob counts, sizes, DAG entries).
    StorageStats,
    /// Test connectivity to the relay server.
    RunConnectivityCheck,
    /// Export diagnostics to a JSON file.
    ExportDiagnostics {
        /// Output file path.
        output_path: String,
    },

    // -- Onboarding: pairing invites --
    /// Issue a short-lived pairing invite (signed token carrying the encrypted
    /// mnemonic). Used by `murmur-cli pair invite` and the desktop pairing
    /// modal.
    IssuePairingInvite,
    /// Redeem a pairing invite URL offline — validates signature and expiry,
    /// returns the decrypted mnemonic. Used by `murmur-cli pair redeem <url>`.
    RedeemPairingInvite {
        /// A `murmur://join?token=…` URL.
        url: String,
    },

    // -- M29a: Conflict Resolution Improvements --
    /// Fetch the two competing versions of a conflicted file for side-by-side
    /// rendering. Returns raw blob bytes plus an `is_text` flag so all clients
    /// share a single UTF-8 detection result.
    ConflictDiff {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// File path within the folder.
        path: String,
    },
    /// Set (or clear) the conflict-expiry window for a folder.
    ///
    /// When enabled, conflicts older than `days` days are auto-resolved on
    /// the daemon's periodic tick using the folder's auto-resolve strategy
    /// (or "keep both" if the strategy is `none`).
    SetConflictExpiry {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Days until unresolved conflicts are auto-resolved. `0` disables.
        days: u64,
    },

    // -- M31: Sync Progress & Desktop UX Polish --
    /// Set a per-folder cosmetic color (hex, e.g. `"#4f8cff"`). `None` clears.
    ///
    /// Stored in the local `config.toml` (not the DAG) since color/icon are
    /// per-device cosmetic preferences, not shared-network state.
    SetFolderColor {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// CSS-style hex color, or `None` to clear.
        color_hex: Option<String>,
    },
    /// Set a per-folder icon identifier (short slug or emoji). `None` clears.
    SetFolderIcon {
        /// Folder ID as 64-character hex string.
        folder_id_hex: String,
        /// Icon slug/emoji, or `None` to clear.
        icon: Option<String>,
    },
    /// Query the current notification-preference settings.
    GetNotificationSettings,
    /// Replace the notification-preference settings.
    SetNotificationSettings {
        /// The new settings to persist.
        settings: NotificationSettingsIpc,
    },
}

/// A response sent from `murmurd` to `murmur-cli`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CliResponse {
    /// Daemon status information.
    Status {
        /// This device's ID (hex).
        device_id: String,
        /// This device's name.
        device_name: String,
        /// Network ID (hex).
        network_id: String,
        /// Number of approved peers.
        peer_count: u64,
        /// Total DAG entries.
        dag_entries: u64,
        /// Daemon uptime in seconds.
        uptime_secs: u64,
    },
    /// List of devices.
    Devices {
        /// Device info list.
        devices: Vec<DeviceInfoIpc>,
    },
    /// List of pending devices.
    Pending {
        /// Pending device info list.
        devices: Vec<DeviceInfoIpc>,
    },
    /// Network mnemonic.
    Mnemonic {
        /// The BIP39 mnemonic phrase.
        mnemonic: String,
    },
    /// List of synced files.
    Files {
        /// File info list.
        files: Vec<FileInfoIpc>,
    },
    /// Operation completed successfully.
    Ok {
        /// Human-readable message.
        message: String,
    },
    /// In-flight blob transfer status.
    TransferStatus {
        /// Active transfers.
        transfers: Vec<TransferInfoIpc>,
    },
    /// Operation failed.
    Error {
        /// Human-readable error message.
        message: String,
    },

    // -- Folder responses (M17) --
    /// List of shared folders.
    Folders {
        /// Folder info list.
        folders: Vec<FolderInfoIpc>,
    },
    /// Status of a specific folder.
    FolderStatus {
        /// Folder ID (hex).
        folder_id: String,
        /// Folder name.
        name: String,
        /// Number of files in the folder.
        file_count: u64,
        /// Number of active conflicts.
        conflict_count: u64,
        /// Sync status description.
        sync_status: String,
    },
    /// List of active conflicts.
    Conflicts {
        /// Conflict info list.
        conflicts: Vec<ConflictInfoIpc>,
    },
    /// File version history.
    FileVersions {
        /// Versions ordered by HLC.
        versions: Vec<FileVersionIpc>,
    },
    /// Raw blob data (response to `BlobPreview`).
    BlobData {
        /// The blob bytes (may be truncated to `max_bytes`).
        data: Vec<u8>,
    },
    /// A real-time engine event (pushed via event stream).
    Event {
        /// The event data.
        event: EngineEventIpc,
    },

    // -- M19a: Config response --
    /// Daemon configuration.
    Config {
        /// Device name.
        device_name: String,
        /// Network ID (hex).
        network_id: String,
        /// Folder configurations.
        folders: Vec<FolderConfigIpc>,
        /// Whether auto-approve is enabled.
        auto_approve: bool,
        /// Whether mDNS is enabled.
        mdns: bool,
        /// Upload throttle (bytes/sec, 0 = unlimited).
        upload_throttle: u64,
        /// Download throttle (bytes/sec, 0 = unlimited).
        download_throttle: u64,
        /// Whether sync is globally paused.
        sync_paused: bool,
    },

    // -- M21a: Network folders --
    /// List of all folders on the network (including unsubscribed).
    NetworkFolders {
        /// Folder info list with subscriber counts.
        folders: Vec<NetworkFolderInfoIpc>,
    },

    // -- M21a: Folder subscribers --
    /// List of devices subscribed to a folder.
    FolderSubscriberList {
        /// Subscriber info list.
        subscribers: Vec<FolderSubscriberIpc>,
    },

    // -- M23a: Device presence --
    /// Per-device online/offline presence.
    DevicePresence {
        /// Presence info list.
        devices: Vec<DevicePresenceIpc>,
    },

    // -- M26a: Settings --
    /// Ignore patterns for a folder.
    IgnorePatterns {
        /// The newline-separated pattern string.
        patterns: String,
    },
    /// Bytes freed by orphaned-blob reclamation.
    ReclaimedBytes {
        /// Number of bytes freed.
        bytes_freed: u64,
        /// Number of blobs removed.
        blobs_removed: u64,
    },

    // -- M27a: Diagnostics --
    /// Connected peer list.
    Peers {
        /// Peer info list.
        peers: Vec<PeerInfoIpc>,
    },
    /// Storage statistics.
    StorageStatsResponse {
        /// Per-folder storage info.
        folders: Vec<FolderStorageIpc>,
        /// Total blob count on disk.
        total_blob_count: u64,
        /// Total blob bytes on disk.
        total_blob_bytes: u64,
        /// Orphaned blob count.
        orphaned_blob_count: u64,
        /// Orphaned blob bytes.
        orphaned_blob_bytes: u64,
        /// DAG entry count.
        dag_entry_count: u64,
    },
    /// Connectivity check result.
    ConnectivityResult {
        /// Whether the relay is reachable.
        relay_reachable: bool,
        /// Round-trip latency in milliseconds (if reachable).
        latency_ms: Option<u64>,
    },

    // -- Onboarding: pairing invites --
    /// A freshly issued pairing invite URL (response to `IssuePairingInvite`).
    PairingInvite {
        /// Full `murmur://join?token=…` URL. Clients render this as a QR code.
        url: String,
        /// UNIX timestamp (seconds) at which the invite stops being valid.
        expires_at_unix: u64,
    },
    /// Mnemonic extracted from a redeemed pairing invite (response to
    /// `RedeemPairingInvite`).
    RedeemedMnemonic {
        /// The BIP39 mnemonic phrase.
        mnemonic: String,
        /// Device ID of the issuer, as 64-character hex.
        issued_by: String,
    },

    // -- M29a: Conflict Resolution Improvements --
    /// Two-way conflict diff payload (response to [`CliRequest::ConflictDiff`]).
    ConflictDiff {
        /// Whether both blobs decoded as UTF-8 (and so should be diffed as text).
        is_text: bool,
        /// Left (first) side of the conflict.
        left: ConflictDiffSide,
        /// Right (second) side of the conflict.
        right: ConflictDiffSide,
    },

    // -- M31: Sync Progress & Desktop UX Polish --
    /// Notification preferences (response to [`CliRequest::GetNotificationSettings`]).
    NotificationSettings {
        /// The current notification settings.
        settings: NotificationSettingsIpc,
    },
}

// ---------------------------------------------------------------------------
// IPC data types
// ---------------------------------------------------------------------------

/// Device information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfoIpc {
    /// Device ID (hex).
    pub device_id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether the device is approved.
    pub approved: bool,
}

/// File information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileInfoIpc {
    /// Blob hash (hex).
    pub blob_hash: String,
    /// Folder ID (hex).
    pub folder_id: String,
    /// Relative path within folder.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (if known).
    pub mime_type: Option<String>,
    /// Origin device ID (hex).
    pub device_origin: String,
}

/// Blob transfer status for IPC transport.
///
/// Computed daemon-side so that all clients (desktop, CLI, mobile) render
/// identical numbers. The smoothed speed uses an EWMA (α ≈ 0.2) against a
/// running baseline — naive `bytes/elapsed` is too jittery on real networks
/// to produce useful ETAs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransferInfoIpc {
    /// Blob hash (hex).
    pub blob_hash: String,
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total blob size in bytes.
    pub total_bytes: u64,
    /// UNIX timestamp (seconds) when the transfer was first observed.
    #[serde(default)]
    pub started_at_unix: u64,
    /// UNIX timestamp (seconds) of the most recent progress sample.
    #[serde(default)]
    pub last_progress_unix: u64,
    /// Smoothed transfer speed in bytes/sec (EWMA over progress samples).
    #[serde(default)]
    pub bytes_per_sec_smoothed: u64,
    /// Estimated seconds until completion, or `None` when insufficient data.
    #[serde(default)]
    pub eta_seconds: Option<u64>,
}

/// Folder information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderInfoIpc {
    /// Folder ID (hex).
    pub folder_id: String,
    /// Human-readable folder name.
    pub name: String,
    /// Creator device ID (hex).
    pub created_by: String,
    /// Number of files in the folder.
    pub file_count: u64,
    /// Whether this device is subscribed.
    pub subscribed: bool,
    /// Sync mode if subscribed.
    pub mode: Option<String>,
    /// Local directory path if subscribed.
    pub local_path: Option<String>,
    /// Short sync status label (e.g. "Up to date", "Syncing", "Paused", "Conflicts").
    #[serde(default)]
    pub sync_status: String,
    /// Per-device cosmetic color (hex string, e.g. `"#4f8cff"`). M31.
    #[serde(default)]
    pub color_hex: Option<String>,
    /// Per-device cosmetic icon slug or emoji. M31.
    #[serde(default)]
    pub icon: Option<String>,
}

/// Conflict information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictInfoIpc {
    /// Folder ID (hex).
    pub folder_id: String,
    /// Folder name.
    pub folder_name: String,
    /// File path within the folder.
    pub path: String,
    /// Competing versions.
    pub versions: Vec<ConflictVersionIpc>,
}

/// One side of a conflict for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictVersionIpc {
    /// Content hash (hex).
    pub blob_hash: String,
    /// Device ID that created this version (hex).
    pub device_id: String,
    /// Device name.
    pub device_name: String,
    /// HLC timestamp.
    pub hlc: u64,
}

/// File version for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileVersionIpc {
    /// Content hash (hex).
    pub blob_hash: String,
    /// Device ID that created this version (hex).
    pub device_id: String,
    /// Device name.
    pub device_name: String,
    /// Modification HLC timestamp.
    pub modified_at: u64,
    /// File size in bytes.
    pub size: u64,
}

/// Engine event for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EngineEventIpc {
    /// Event type name (e.g., "file_synced", "conflict_detected").
    pub event_type: String,
    /// JSON-serialized event details.
    pub data: String,
}

/// Folder configuration for IPC transport (M19a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderConfigIpc {
    /// Folder ID (hex).
    pub folder_id: String,
    /// Human-readable folder name.
    pub name: String,
    /// Local path on disk.
    pub local_path: String,
    /// Sync mode: "full", "send-only", or "receive-only".
    pub mode: String,
    /// Auto-resolve strategy: "none", "newest", or "mine".
    pub auto_resolve: String,
    /// Days until unresolved conflicts auto-resolve. `None` = disabled (M29).
    #[serde(default)]
    pub conflict_expiry_days: Option<u64>,
    /// Per-device cosmetic color (hex string, e.g. `"#4f8cff"`). M31.
    #[serde(default)]
    pub color_hex: Option<String>,
    /// Per-device cosmetic icon slug or emoji. M31.
    #[serde(default)]
    pub icon: Option<String>,
}

/// Notification preferences for IPC transport (M31).
///
/// Each toggle controls whether a given engine event is allowed to surface
/// a tray notification. Clients persist these in local config so they
/// survive daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationSettingsIpc {
    /// Notify when a conflict is detected.
    pub conflict: bool,
    /// Notify when a transfer completes.
    pub transfer_completed: bool,
    /// Notify when a new device joins or is approved.
    pub device_joined: bool,
    /// Notify on errors surfaced by the daemon.
    pub error: bool,
}

impl Default for NotificationSettingsIpc {
    fn default() -> Self {
        Self {
            conflict: true,
            transfer_completed: true,
            device_joined: true,
            error: true,
        }
    }
}

/// One side of a conflict diff for IPC transport (M29).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictDiffSide {
    /// Content hash of this version (hex).
    pub blob_hash: String,
    /// Name of the device that produced this version.
    pub device_name: String,
    /// Full size of the blob in bytes.
    pub size: u64,
    /// Blob bytes. May be truncated for very large blobs; empty for unreadable
    /// blobs (e.g., not yet synced locally). Clients compare `size` to
    /// `bytes.len()` to detect truncation.
    pub bytes: Vec<u8>,
}

/// Network folder info for discovery (M21a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkFolderInfoIpc {
    /// Folder ID (hex).
    pub folder_id: String,
    /// Folder name.
    pub name: String,
    /// Creator device ID (hex).
    pub created_by: String,
    /// Number of files.
    pub file_count: u64,
    /// Number of subscribers.
    pub subscriber_count: u64,
    /// Whether this device is subscribed.
    pub subscribed: bool,
    /// Sync mode if subscribed.
    pub mode: Option<String>,
}

/// Folder subscriber info (M21a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderSubscriberIpc {
    /// Device ID (hex).
    pub device_id: String,
    /// Device name.
    pub device_name: String,
    /// Sync mode.
    pub mode: String,
}

/// Device presence info (M23a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DevicePresenceIpc {
    /// Device ID (hex).
    pub device_id: String,
    /// Device name.
    pub device_name: String,
    /// Whether the device is currently online.
    pub online: bool,
    /// Last seen UNIX timestamp (0 if never connected).
    pub last_seen_unix: u64,
}

/// Peer connection info (M27a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerInfoIpc {
    /// Device ID (hex).
    pub device_id: String,
    /// Device name.
    pub device_name: String,
    /// Connection type: "direct" or "relay".
    pub connection_type: String,
    /// Last seen UNIX timestamp.
    pub last_seen_unix: u64,
}

/// Per-folder storage info (M27a).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderStorageIpc {
    /// Folder ID (hex).
    pub folder_id: String,
    /// Folder name.
    pub name: String,
    /// Number of files.
    pub file_count: u64,
    /// Total bytes on disk.
    pub total_bytes: u64,
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Returns the default socket path: `~/.murmur/murmurd.sock`.
pub fn default_socket_path() -> PathBuf {
    default_base_dir().join("murmurd.sock")
}

/// Returns the socket path for a given base directory.
pub fn socket_path(base_dir: &std::path::Path) -> PathBuf {
    base_dir.join("murmurd.sock")
}

/// Returns the default murmur base directory: `~/.murmur`.
pub fn default_base_dir() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".murmur")
}

// ---------------------------------------------------------------------------
// Wire protocol: length-prefixed postcard
// ---------------------------------------------------------------------------

/// Send a message (request or response) over a writer with length prefix.
pub fn send_message<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> Result<(), IpcError> {
    let bytes = postcard::to_allocvec(msg).map_err(|e| IpcError::Codec(e.to_string()))?;
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Receive a message (request or response) from a reader with length prefix.
pub fn recv_message<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T, IpcError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(IpcError::MessageTooLarge(len));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map_err(|e| IpcError::Codec(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_request_roundtrip_status() {
        let req = CliRequest::Status;
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: CliRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_request_roundtrip_approve() {
        let req = CliRequest::ApproveDevice {
            device_id_hex: "ab".repeat(32),
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: CliRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_request_roundtrip_revoke() {
        let req = CliRequest::RevokeDevice {
            device_id_hex: "cd".repeat(32),
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: CliRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_request_roundtrip_add_file() {
        let req = CliRequest::AddFile {
            path: "/home/user/photo.jpg".to_string(),
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: CliRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_request_roundtrip_all_variants() {
        let variants = vec![
            CliRequest::Status,
            CliRequest::ListDevices,
            CliRequest::ListPending,
            CliRequest::ApproveDevice {
                device_id_hex: "aa".repeat(32),
            },
            CliRequest::RevokeDevice {
                device_id_hex: "bb".repeat(32),
            },
            CliRequest::ShowMnemonic,
            CliRequest::ListFiles,
            CliRequest::AddFile {
                path: "/tmp/file.txt".to_string(),
            },
            CliRequest::TransferStatus,
            CliRequest::CreateFolder {
                name: "Photos".to_string(),
                local_path: Some("/home/user/Photos".to_string()),
                ignore_patterns: None,
            },
            CliRequest::CreateFolder {
                name: "Rust Project".to_string(),
                local_path: Some("/home/user/code/proj".to_string()),
                ignore_patterns: Some("target/\n**/*.rs.bk\n".to_string()),
            },
            CliRequest::IssuePairingInvite,
            CliRequest::RedeemPairingInvite {
                url: "murmur://join?token=abc".to_string(),
            },
            CliRequest::RemoveFolder {
                folder_id_hex: "cc".repeat(32),
            },
            CliRequest::ListFolders,
            CliRequest::SubscribeFolder {
                folder_id_hex: "dd".repeat(32),
                name: Some("Photos".to_string()),
                local_path: "/home/user/Sync/Photos".to_string(),
                mode: "read-write".to_string(),
            },
            CliRequest::UnsubscribeFolder {
                folder_id_hex: "ee".repeat(32),
                keep_local: true,
            },
            CliRequest::FolderFiles {
                folder_id_hex: "ff".repeat(32),
            },
            CliRequest::FolderStatus {
                folder_id_hex: "aa".repeat(32),
            },
            CliRequest::ListConflicts {
                folder_id_hex: None,
            },
            CliRequest::ResolveConflict {
                folder_id_hex: "bb".repeat(32),
                path: "readme.txt".to_string(),
                chosen_hash_hex: "cc".repeat(32),
            },
            CliRequest::FileHistory {
                folder_id_hex: "dd".repeat(32),
                path: "docs/plan.md".to_string(),
            },
            CliRequest::SetFolderMode {
                folder_id_hex: "ee".repeat(32),
                mode: "read-only".to_string(),
            },
            CliRequest::BlobPreview {
                blob_hash_hex: "ff".repeat(32),
                max_bytes: 4096,
            },
            CliRequest::RestoreFileVersion {
                folder_id_hex: "aa".repeat(32),
                path: "docs/readme.md".to_string(),
                blob_hash_hex: "bb".repeat(32),
            },
            CliRequest::SubscribeEvents,
            // M19a
            CliRequest::GetConfig,
            CliRequest::InitDefaultFolder {
                local_path: "/home/user/Murmur".to_string(),
            },
            // M20a
            CliRequest::PauseSync,
            CliRequest::ResumeSync,
            // M21a
            CliRequest::ListNetworkFolders,
            CliRequest::FolderSubscribers {
                folder_id_hex: "aa".repeat(32),
            },
            // M22a
            CliRequest::BulkResolveConflicts {
                folder_id_hex: "bb".repeat(32),
                strategy: "keep_newest".to_string(),
            },
            CliRequest::SetFolderAutoResolve {
                folder_id_hex: "cc".repeat(32),
                strategy: "newest".to_string(),
            },
            CliRequest::DismissConflict {
                folder_id_hex: "dd".repeat(32),
                path: "conflict.txt".to_string(),
            },
            // M23a
            CliRequest::GetDevicePresence,
            CliRequest::SetDeviceName {
                name: "My Device".to_string(),
            },
            // M24a
            CliRequest::PauseFolderSync {
                folder_id_hex: "ee".repeat(32),
            },
            CliRequest::ResumeFolderSync {
                folder_id_hex: "ff".repeat(32),
            },
            // M25a
            CliRequest::DeleteFile {
                folder_id_hex: "aa".repeat(32),
                path: "old.txt".to_string(),
            },
            // M26a
            CliRequest::SetAutoApprove { enabled: true },
            CliRequest::SetMdns { enabled: false },
            CliRequest::ReclaimOrphanedBlobs,
            CliRequest::SetFolderLocalPath {
                folder_id_hex: "aa".repeat(32),
                new_local_path: "/new/path".to_string(),
            },
            CliRequest::SetFolderName {
                folder_id_hex: "aa".repeat(32),
                name: "Renamed".to_string(),
            },
            CliRequest::GetIgnorePatterns {
                folder_id_hex: "bb".repeat(32),
            },
            CliRequest::SetIgnorePatterns {
                folder_id_hex: "cc".repeat(32),
                patterns: "*.tmp\n.DS_Store".to_string(),
            },
            CliRequest::SetThrottle {
                upload_bytes_per_sec: 1_048_576,
                download_bytes_per_sec: 0,
            },
            // M27a
            CliRequest::ListPeers,
            CliRequest::StorageStats,
            CliRequest::RunConnectivityCheck,
            CliRequest::ExportDiagnostics {
                output_path: "/tmp/diag.json".to_string(),
            },
            // M29a
            CliRequest::ConflictDiff {
                folder_id_hex: "aa".repeat(32),
                path: "readme.txt".to_string(),
            },
            CliRequest::SetConflictExpiry {
                folder_id_hex: "bb".repeat(32),
                days: 7,
            },
            // M31
            CliRequest::SetFolderColor {
                folder_id_hex: "cc".repeat(32),
                color_hex: Some("#4f8cff".to_string()),
            },
            CliRequest::SetFolderIcon {
                folder_id_hex: "dd".repeat(32),
                icon: Some("photos".to_string()),
            },
            CliRequest::GetNotificationSettings,
            CliRequest::SetNotificationSettings {
                settings: NotificationSettingsIpc {
                    conflict: true,
                    transfer_completed: false,
                    device_joined: true,
                    error: false,
                },
            },
        ];
        for req in variants {
            let bytes = postcard::to_allocvec(&req).unwrap();
            let decoded: CliRequest = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(req, decoded);
        }
    }

    #[test]
    fn test_response_roundtrip_status() {
        let resp = CliResponse::Status {
            device_id: "ab".repeat(32),
            device_name: "NAS".to_string(),
            network_id: "cd".repeat(32),
            peer_count: 3,
            dag_entries: 42,
            uptime_secs: 3600,
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_devices() {
        let resp = CliResponse::Devices {
            devices: vec![DeviceInfoIpc {
                device_id: "ab".repeat(32),
                name: "Phone".to_string(),
                approved: true,
            }],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_files() {
        let resp = CliResponse::Files {
            files: vec![FileInfoIpc {
                blob_hash: "ef".repeat(32),
                folder_id: "cc".repeat(32),
                path: "photos/photo.jpg".to_string(),
                size: 1024,
                mime_type: Some("image/jpeg".to_string()),
                device_origin: "ab".repeat(32),
            }],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_folders() {
        let resp = CliResponse::Folders {
            folders: vec![FolderInfoIpc {
                folder_id: "aa".repeat(32),
                name: "Photos".to_string(),
                created_by: "bb".repeat(32),
                file_count: 42,
                subscribed: true,
                mode: Some("read-write".to_string()),
                local_path: Some("/home/user/Photos".to_string()),
                sync_status: "Up to date".to_string(),
                color_hex: Some("#4f8cff".to_string()),
                icon: Some("photos".to_string()),
            }],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_conflicts() {
        let resp = CliResponse::Conflicts {
            conflicts: vec![ConflictInfoIpc {
                folder_id: "aa".repeat(32),
                folder_name: "Photos".to_string(),
                path: "readme.txt".to_string(),
                versions: vec![ConflictVersionIpc {
                    blob_hash: "bb".repeat(32),
                    device_id: "cc".repeat(32),
                    device_name: "Phone".to_string(),
                    hlc: 12345,
                }],
            }],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_file_versions() {
        let resp = CliResponse::FileVersions {
            versions: vec![FileVersionIpc {
                blob_hash: "aa".repeat(32),
                device_id: "bb".repeat(32),
                device_name: "NAS".to_string(),
                modified_at: 999,
                size: 2048,
            }],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_event() {
        let resp = CliResponse::Event {
            event: EngineEventIpc {
                event_type: "file_synced".to_string(),
                data: r#"{"path":"photo.jpg"}"#.to_string(),
            },
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_folder_status() {
        let resp = CliResponse::FolderStatus {
            folder_id: "aa".repeat(32),
            name: "Documents".to_string(),
            file_count: 10,
            conflict_count: 2,
            sync_status: "synced".to_string(),
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_response_roundtrip_all_variants() {
        let variants = vec![
            CliResponse::Status {
                device_id: "aa".repeat(32),
                device_name: "Test".to_string(),
                network_id: "bb".repeat(32),
                peer_count: 0,
                dag_entries: 0,
                uptime_secs: 0,
            },
            CliResponse::Devices { devices: vec![] },
            CliResponse::Pending { devices: vec![] },
            CliResponse::Mnemonic {
                mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
            },
            CliResponse::Files { files: vec![] },
            CliResponse::Ok {
                message: "done".to_string(),
            },
            CliResponse::TransferStatus {
                transfers: vec![TransferInfoIpc {
                    blob_hash: "dd".repeat(32),
                    bytes_transferred: 512,
                    total_bytes: 1024,
                    started_at_unix: 1_711_700_000,
                    last_progress_unix: 1_711_700_015,
                    bytes_per_sec_smoothed: 34,
                    eta_seconds: Some(15),
                }],
            },
            CliResponse::Error {
                message: "failed".to_string(),
            },
            CliResponse::Folders { folders: vec![] },
            CliResponse::FolderStatus {
                folder_id: "aa".repeat(32),
                name: "Test".to_string(),
                file_count: 0,
                conflict_count: 0,
                sync_status: "empty".to_string(),
            },
            CliResponse::Conflicts { conflicts: vec![] },
            CliResponse::FileVersions { versions: vec![] },
            CliResponse::BlobData {
                data: vec![0x48, 0x65, 0x6c, 0x6c, 0x6f],
            },
            CliResponse::Event {
                event: EngineEventIpc {
                    event_type: "test".to_string(),
                    data: "{}".to_string(),
                },
            },
            // M19a
            CliResponse::Config {
                device_name: "NAS".to_string(),
                network_id: "aa".repeat(32),
                folders: vec![FolderConfigIpc {
                    folder_id: "bb".repeat(32),
                    name: "Murmur".to_string(),
                    local_path: "/home/user/Murmur".to_string(),
                    mode: "read-write".to_string(),
                    auto_resolve: "none".to_string(),
                    conflict_expiry_days: None,
                    color_hex: None,
                    icon: None,
                }],
                auto_approve: false,
                mdns: true,
                upload_throttle: 0,
                download_throttle: 0,
                sync_paused: false,
            },
            // M21a
            CliResponse::NetworkFolders {
                folders: vec![NetworkFolderInfoIpc {
                    folder_id: "cc".repeat(32),
                    name: "Photos".to_string(),
                    created_by: "dd".repeat(32),
                    file_count: 10,
                    subscriber_count: 2,
                    subscribed: true,
                    mode: Some("read-write".to_string()),
                }],
            },
            CliResponse::FolderSubscriberList {
                subscribers: vec![FolderSubscriberIpc {
                    device_id: "ee".repeat(32),
                    device_name: "Phone".to_string(),
                    mode: "read-write".to_string(),
                }],
            },
            // M23a
            CliResponse::DevicePresence {
                devices: vec![DevicePresenceIpc {
                    device_id: "ff".repeat(32),
                    device_name: "NAS".to_string(),
                    online: true,
                    last_seen_unix: 1711700000,
                }],
            },
            // M26a
            CliResponse::IgnorePatterns {
                patterns: "*.tmp\n.DS_Store".to_string(),
            },
            CliResponse::ReclaimedBytes {
                bytes_freed: 1024,
                blobs_removed: 3,
            },
            // M27a
            CliResponse::Peers {
                peers: vec![PeerInfoIpc {
                    device_id: "aa".repeat(32),
                    device_name: "Phone".to_string(),
                    connection_type: "relay".to_string(),
                    last_seen_unix: 1711700000,
                }],
            },
            CliResponse::StorageStatsResponse {
                folders: vec![FolderStorageIpc {
                    folder_id: "bb".repeat(32),
                    name: "Photos".to_string(),
                    file_count: 42,
                    total_bytes: 1_000_000,
                }],
                total_blob_count: 100,
                total_blob_bytes: 5_000_000,
                orphaned_blob_count: 2,
                orphaned_blob_bytes: 50_000,
                dag_entry_count: 200,
            },
            CliResponse::ConnectivityResult {
                relay_reachable: true,
                latency_ms: Some(42),
            },
            // Onboarding: pairing invites
            CliResponse::PairingInvite {
                url: "murmur://join?token=abc".to_string(),
                expires_at_unix: 1_700_000_000,
            },
            CliResponse::RedeemedMnemonic {
                mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
                issued_by: "aa".repeat(32),
            },
            // M29a
            CliResponse::ConflictDiff {
                is_text: true,
                left: ConflictDiffSide {
                    blob_hash: "cc".repeat(32),
                    device_name: "Phone".to_string(),
                    size: 12,
                    bytes: b"hello world\n".to_vec(),
                },
                right: ConflictDiffSide {
                    blob_hash: "dd".repeat(32),
                    device_name: "NAS".to_string(),
                    size: 13,
                    bytes: b"hello, world\n".to_vec(),
                },
            },
            // M31
            CliResponse::NotificationSettings {
                settings: NotificationSettingsIpc::default(),
            },
        ];
        for resp in variants {
            let bytes = postcard::to_allocvec(&resp).unwrap();
            let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(resp, decoded);
        }
    }

    // -----------------------------------------------------------------------
    // Serialization stability (golden-byte) tests
    //
    // These catch wire-format breaking changes. If you intentionally change
    // a type's layout, update the expected bytes here — but understand that
    // you are breaking compatibility between daemon and CLI binaries.
    // -----------------------------------------------------------------------

    /// Helper: hex-encode bytes for readable assertion messages.
    fn hex(b: &[u8]) -> String {
        b.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn test_request_wire_stability() {
        // Snapshot: every CliRequest variant's postcard encoding.
        // The enum discriminant is a varint index; field order is declaration order.
        let cases: Vec<(CliRequest, &str)> = vec![
            // Status = variant 0, no fields
            (CliRequest::Status, "00"),
            // ListDevices = variant 1
            (CliRequest::ListDevices, "01"),
            // ListPending = variant 2
            (CliRequest::ListPending, "02"),
            // PauseSync = variant 25
            (CliRequest::PauseSync, "19"),
            // ResumeSync = variant 26
            (CliRequest::ResumeSync, "1a"),
        ];
        for (req, expected_hex) in cases {
            let bytes = postcard::to_allocvec(&req).unwrap();
            assert_eq!(
                hex(&bytes),
                expected_hex,
                "wire format changed for {req:?} — this breaks IPC compatibility"
            );
        }
    }

    #[test]
    fn test_response_wire_stability() {
        // Snapshot: CliResponse variants with known encodings.
        let cases: Vec<(CliResponse, &str)> = vec![
            // Ok { message: "ok" } = variant 5, string "ok" (len 2 + bytes)
            (
                CliResponse::Ok {
                    message: "ok".to_string(),
                },
                "05026f6b",
            ),
            // Error { message: "e" } = variant 7, string "e" (len 1 + byte)
            (
                CliResponse::Error {
                    message: "e".to_string(),
                },
                "070165",
            ),
            // Files { files: [] } = variant 4, empty vec (len 0)
            (CliResponse::Files { files: vec![] }, "0400"),
            // Devices { devices: [] } = variant 1, empty vec
            (CliResponse::Devices { devices: vec![] }, "0100"),
        ];
        for (resp, expected_hex) in cases {
            let bytes = postcard::to_allocvec(&resp).unwrap();
            assert_eq!(
                hex(&bytes),
                expected_hex,
                "wire format changed for {resp:?} — this breaks IPC compatibility"
            );
        }
    }

    #[test]
    fn test_file_info_wire_stability() {
        // A FileInfoIpc with known field values — catches field reordering,
        // additions, or type changes.
        let info = FileInfoIpc {
            blob_hash: "aa".to_string(),
            folder_id: "bb".to_string(),
            path: "c".to_string(),
            size: 1,
            mime_type: None,
            device_origin: "dd".to_string(),
        };
        let bytes = postcard::to_allocvec(&info).unwrap();
        let expected = "02616102626201630100026464";
        assert_eq!(
            hex(&bytes),
            expected,
            "FileInfoIpc wire format changed — this breaks IPC compatibility"
        );

        // Also verify None vs Some(mime_type) encoding differs correctly.
        let with_mime = FileInfoIpc {
            mime_type: Some("x".to_string()),
            ..info.clone()
        };
        let bytes2 = postcard::to_allocvec(&with_mime).unwrap();
        assert_ne!(bytes, bytes2);
        // The Option discriminant byte should be 0x01 (Some) instead of 0x00 (None).
        let expected_with_mime = "026161026262016301010178026464";
        assert_eq!(
            hex(&bytes2),
            expected_with_mime,
            "FileInfoIpc (with mime) wire format changed"
        );
    }

    #[test]
    fn test_folder_info_wire_stability() {
        let info = FolderInfoIpc {
            folder_id: "a".to_string(),
            name: "b".to_string(),
            created_by: "c".to_string(),
            file_count: 0,
            subscribed: false,
            mode: None,
            local_path: None,
            sync_status: String::new(),
            color_hex: None,
            icon: None,
        };
        let bytes = postcard::to_allocvec(&info).unwrap();
        // Trailing `0000` accounts for the two Option::None slots added in
        // M31 (color_hex, icon).
        let expected = "01610162016300000000000000";
        assert_eq!(
            hex(&bytes),
            expected,
            "FolderInfoIpc wire format changed — this breaks IPC compatibility"
        );
    }

    #[test]
    fn test_response_variant_count_guard() {
        // If someone adds or removes a CliResponse variant, this test
        // forces them to also update the golden-byte tests above.
        //
        // Last variant is now NotificationSettings at index 26 (0x1a). Payload:
        // four `bool` fields (conflict, transfer_completed, device_joined,
        // error) all true → "01 01 01 01".
        let last = CliResponse::NotificationSettings {
            settings: NotificationSettingsIpc::default(),
        };
        let bytes = postcard::to_allocvec(&last).unwrap();
        assert_eq!(
            hex(&bytes),
            "1a01010101",
            "CliResponse variant count changed — update golden-byte tests for new variants"
        );
    }

    #[test]
    fn test_request_variant_count_guard() {
        // Last variant is now SetNotificationSettings at index 57 (0x39).
        // Payload: four `bool` fields (all true) → "01 01 01 01".
        let last = CliRequest::SetNotificationSettings {
            settings: NotificationSettingsIpc::default(),
        };
        let bytes = postcard::to_allocvec(&last).unwrap();
        assert_eq!(
            hex(&bytes),
            "3901010101",
            "CliRequest variant count changed — update golden-byte tests for new variants"
        );
    }

    #[test]
    fn test_cross_version_deserialization_detects_mismatch() {
        // Simulate what happens when a daemon sends bytes from a different
        // type layout: deserializing garbage at an Option field should fail
        // (the original user-reported bug).
        //
        // Craft bytes where the Option<String> discriminant is 0x02 (invalid).
        let bad_bytes: &[u8] = &[
            0x02, 0x61, 0x61, // blob_hash: "aa"
            0x02, 0x62, 0x62, // folder_id: "bb"
            0x01, 0x63, // path: "c"
            0x01, // size: 1
            0x02, // <-- invalid Option discriminant (not 0 or 1)
            0x01, 0x78, // would-be string
            0x02, 0x64, 0x64, // device_origin: "dd"
        ];
        let result = postcard::from_bytes::<FileInfoIpc>(bad_bytes);
        assert!(result.is_err(), "should reject invalid Option discriminant");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Option"),
            "error should mention Option, got: {err_msg}"
        );
    }

    #[test]
    fn test_socket_path_default() {
        let path = default_socket_path();
        assert!(path.ends_with("murmurd.sock"));
    }

    #[test]
    fn test_socket_path_custom_base() {
        let base = std::path::Path::new("/data/murmur");
        let path = socket_path(base);
        assert_eq!(path, PathBuf::from("/data/murmur/murmurd.sock"));
    }

    #[test]
    fn test_send_recv_request() {
        let req = CliRequest::Status;
        let mut buf = Vec::new();
        send_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: CliRequest = recv_message(&mut cursor).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_send_recv_response() {
        let resp = CliResponse::Ok {
            message: "device approved".to_string(),
        };
        let mut buf = Vec::new();
        send_message(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: CliResponse = recv_message(&mut cursor).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_send_recv_complex_response() {
        let resp = CliResponse::Devices {
            devices: vec![
                DeviceInfoIpc {
                    device_id: "aa".repeat(32),
                    name: "Phone".to_string(),
                    approved: true,
                },
                DeviceInfoIpc {
                    device_id: "bb".repeat(32),
                    name: "NAS".to_string(),
                    approved: true,
                },
            ],
        };
        let mut buf = Vec::new();
        send_message(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: CliResponse = recv_message(&mut cursor).unwrap();
        assert_eq!(resp, decoded);
    }
}
