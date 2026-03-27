//! Materialized state derived from DAG replay.

use std::collections::BTreeMap;

use murmur_types::{
    AccessGrant, Action, BlobHash, DeviceId, DeviceInfo, DeviceRole, FileMetadata, FolderId,
    FolderSubscription, SharedFolder,
};
use serde::{Deserialize, Serialize};

use crate::DagEntry;

/// The materialized state: a derived cache rebuilt by replaying the DAG.
///
/// This is the "current view" of the network: which devices exist, what folders
/// and files are tracked, folder subscriptions, and which access grants are active.
/// It is always reconstructible from the DAG entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaterializedState {
    /// All known devices and their status.
    pub devices: BTreeMap<DeviceId, DeviceInfo>,
    /// All shared folders.
    pub folders: BTreeMap<FolderId, SharedFolder>,
    /// Folder subscriptions keyed by `(folder_id, device_id)`.
    pub subscriptions: BTreeMap<(FolderId, DeviceId), FolderSubscription>,
    /// All known files indexed by `(folder_id, path)`.
    pub files: BTreeMap<(FolderId, String), FileMetadata>,
    /// File version history: `(folder_id, path) → [(blob_hash, hlc)]` ordered by HLC.
    pub file_versions: BTreeMap<(FolderId, String), Vec<(BlobHash, u64)>>,
    /// Active access grants.
    pub grants: Vec<AccessGrant>,
}

impl MaterializedState {
    /// Create a new empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize the state to bytes (postcard).
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("MaterializedState serialization")
    }

    /// Deserialize from bytes (postcard).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, crate::DagError> {
        postcard::from_bytes(bytes).map_err(|e| crate::DagError::Deserialization(e.to_string()))
    }

    /// Compute the blake3 hash of the serialized state.
    pub fn state_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.to_bytes()).as_bytes()
    }

    /// Apply a single DAG entry to the state.
    ///
    /// For name changes and other LWW fields, uses HLC + DeviceId for
    /// tiebreaking: higher HLC wins, then higher DeviceId.
    pub fn apply(&mut self, entry: &DagEntry) {
        match &entry.action {
            Action::DeviceJoinRequest { device_id, name } => {
                self.devices
                    .entry(*device_id)
                    .or_insert_with(|| DeviceInfo {
                        device_id: *device_id,
                        name: name.clone(),
                        role: DeviceRole::Source,
                        approved: false,
                        approved_by: None,
                        joined_at: entry.hlc,
                    });
            }
            Action::DeviceApproved { device_id, role } => {
                let info = self
                    .devices
                    .entry(*device_id)
                    .or_insert_with(|| DeviceInfo {
                        device_id: *device_id,
                        name: String::new(),
                        role: *role,
                        approved: false,
                        approved_by: None,
                        joined_at: entry.hlc,
                    });
                info.approved = true;
                info.role = *role;
                info.approved_by = Some(entry.device_id);
            }
            Action::DeviceRevoked { device_id } => {
                if let Some(info) = self.devices.get_mut(device_id) {
                    info.approved = false;
                }
            }
            Action::DeviceNameChanged { device_id, name } => {
                if let Some(info) = self.devices.get_mut(device_id) {
                    info.name = name.clone();
                }
            }
            Action::FolderCreated { folder } => {
                self.folders
                    .entry(folder.folder_id)
                    .or_insert_with(|| folder.clone());
            }
            Action::FolderRemoved { folder_id } => {
                self.folders.remove(folder_id);
                // Remove all subscriptions for this folder.
                self.subscriptions.retain(|(fid, _), _| *fid != *folder_id);
                // Remove all files in this folder.
                self.files.retain(|(fid, _), _| *fid != *folder_id);
                self.file_versions.retain(|(fid, _), _| *fid != *folder_id);
            }
            Action::FolderSubscribed {
                folder_id,
                device_id,
                mode,
            } => {
                self.subscriptions.insert(
                    (*folder_id, *device_id),
                    FolderSubscription {
                        folder_id: *folder_id,
                        device_id: *device_id,
                        mode: *mode,
                    },
                );
            }
            Action::FolderUnsubscribed {
                folder_id,
                device_id,
            } => {
                self.subscriptions.remove(&(*folder_id, *device_id));
            }
            Action::FileAdded { metadata } => {
                let key = (metadata.folder_id, metadata.path.clone());
                self.files.insert(key.clone(), metadata.clone());
                // Add to version history.
                self.file_versions
                    .entry(key)
                    .or_default()
                    .push((metadata.blob_hash, entry.hlc));
            }
            Action::FileModified {
                folder_id,
                path,
                new_hash: _,
                old_hash: _,
                metadata,
            } => {
                let key = (*folder_id, path.clone());
                self.files.insert(key.clone(), metadata.clone());
                // Append to version history.
                self.file_versions
                    .entry(key)
                    .or_default()
                    .push((metadata.blob_hash, entry.hlc));
            }
            Action::FileDeleted { folder_id, path } => {
                let key = (*folder_id, path.clone());
                self.files.remove(&key);
            }
            Action::AccessGranted { grant } => {
                // Remove any existing grant to the same device first.
                self.grants
                    .retain(|g| g.to != grant.to || g.from != grant.from);
                self.grants.push(grant.clone());
            }
            Action::AccessRevoked { to } => {
                self.grants.retain(|g| g.to != *to);
            }
            Action::Merge => {
                // No state change — merge entries only collapse tips.
            }
            Action::Snapshot { .. } => {
                // Snapshot is informational — no state change.
            }
        }
    }
}
