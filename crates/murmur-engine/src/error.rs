//! Error types for the engine crate.

/// Errors that can occur in engine operations.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// DAG error.
    #[error("dag: {0}")]
    Dag(#[from] murmur_dag::DagError),
    /// Network error.
    #[error("net: {0}")]
    Net(#[from] murmur_net::NetError),
    /// Device not found.
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    /// Device not approved.
    #[error("device not approved: {0}")]
    DeviceNotApproved(String),
    /// File already exists (dedup).
    #[error("file already exists: {0}")]
    FileAlreadyExists(String),
    /// Access denied.
    #[error("access denied: {0}")]
    AccessDenied(String),
    /// Access expired.
    #[error("access expired")]
    AccessExpired,
    /// Blob integrity check failed.
    #[error("blob integrity: expected {expected}, got {actual}")]
    BlobIntegrity {
        /// Expected hash.
        expected: String,
        /// Actual hash.
        actual: String,
    },
    /// Folder not found.
    #[error("folder not found: {0}")]
    FolderNotFound(String),
    /// Device not subscribed to folder.
    #[error("not subscribed to folder: {0}")]
    NotSubscribed(String),
    /// Cannot write to a read-only folder.
    #[error("folder is read-only: {0}")]
    ReadOnlyFolder(String),
    /// File not found in folder.
    #[error("file not found: {0}")]
    FileNotFound(String),
    /// Old hash mismatch during file modification.
    #[error("old hash mismatch: expected {expected}, got {actual}")]
    OldHashMismatch {
        /// Expected hash.
        expected: String,
        /// Actual hash.
        actual: String,
    },
    /// Conflict not found.
    #[error("conflict not found: {0}")]
    ConflictNotFound(String),
}
