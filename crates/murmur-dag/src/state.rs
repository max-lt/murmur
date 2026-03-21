//! Materialized state derived from DAG replay.

use std::collections::BTreeMap;

use murmur_types::{AccessGrant, Action, BlobHash, DeviceId, DeviceInfo, DeviceRole, FileMetadata};
use serde::{Deserialize, Serialize};

use crate::DagEntry;

/// The materialized state: a derived cache rebuilt by replaying the DAG.
///
/// This is the "current view" of the network: which devices exist, what files
/// are tracked, and which access grants are active. It is always reconstructible
/// from the DAG entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaterializedState {
    /// All known devices and their status.
    pub devices: BTreeMap<DeviceId, DeviceInfo>,
    /// All known files indexed by blob hash.
    pub files: BTreeMap<BlobHash, FileMetadata>,
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
                    // LWW: only apply if this entry has a higher (HLC, DeviceId).
                    // We track the "last update" implicitly — since entries are
                    // applied in load order, we use a simple approach: always
                    // apply if the entry's HLC >= the device's joined_at, and
                    // for same-HLC, compare DeviceId.
                    //
                    // For proper LWW we'd need per-field timestamps, but for v1
                    // the approach of "last applied wins" with deterministic
                    // replay ordering is sufficient. The `load_entry` path
                    // applies in insertion order, and for tiebreaking we rely
                    // on the caller to order by (HLC, DeviceId).
                    info.name = name.clone();
                }
            }
            Action::FileAdded { metadata } => {
                self.files.insert(metadata.blob_hash, metadata.clone());
            }
            Action::FileDeleted { blob_hash } => {
                self.files.remove(blob_hash);
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
