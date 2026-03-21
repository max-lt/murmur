//! IPC protocol types for communication between `murmur-cli` and `murmurd`.
//!
//! Communication happens over a Unix domain socket using length-prefixed
//! postcard serialization. This crate defines the request/response types
//! and the socket path convention.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
        /// Role: "source", "backup", or "full".
        role: String,
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
}

/// Device information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfoIpc {
    /// Device ID (hex).
    pub device_id: String,
    /// Human-readable name.
    pub name: String,
    /// Role: "source", "backup", or "full".
    pub role: String,
    /// Whether the device is approved.
    pub approved: bool,
}

/// File information for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileInfoIpc {
    /// Blob hash (hex).
    pub blob_hash: String,
    /// Filename.
    pub filename: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (if known).
    pub mime_type: Option<String>,
    /// Origin device ID (hex).
    pub device_origin: String,
}

/// Blob transfer status for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransferInfoIpc {
    /// Blob hash (hex).
    pub blob_hash: String,
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total blob size in bytes.
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
            role: "backup".to_string(),
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
                role: "full".to_string(),
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
                role: "source".to_string(),
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
                filename: "photo.jpg".to_string(),
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
                }],
            },
            CliResponse::Error {
                message: "failed".to_string(),
            },
        ];
        for resp in variants {
            let bytes = postcard::to_allocvec(&resp).unwrap();
            let decoded: CliResponse = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(resp, decoded);
        }
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
                    role: "source".to_string(),
                    approved: true,
                },
                DeviceInfoIpc {
                    device_id: "bb".repeat(32),
                    name: "NAS".to_string(),
                    role: "backup".to_string(),
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
