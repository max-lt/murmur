//! The main engine orchestrator.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use murmur_dag::{Dag, DagEntry};
use murmur_types::{
    AccessGrant, Action, BlobHash, ConflictInfo, ConflictVersion, DeviceId, DeviceInfo, DeviceRole,
    FileMetadata, FolderId, FolderSubscription, SharedFolder, SyncMode,
};
use tracing::{debug, info};

use crate::callbacks::PlatformCallbacks;
use crate::error::EngineError;
use crate::event::EngineEvent;

/// The main Murmur engine — orchestrates DAG, sync, and device management.
///
/// The engine is **storage-agnostic**: it never writes to disk. All persistence
/// is delegated to the platform via [`PlatformCallbacks`].
pub struct MurmurEngine {
    /// The in-memory DAG.
    dag: Dag,
    /// Platform callbacks for persistence and UI.
    callbacks: Arc<dyn PlatformCallbacks>,
}

impl MurmurEngine {
    // -----------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------

    /// Create a new engine for the **first device** in a network.
    ///
    /// The device is auto-approved with the given role.
    pub fn create_network(
        device_id: DeviceId,
        signing_key: SigningKey,
        device_name: String,
        role: DeviceRole,
        callbacks: Arc<dyn PlatformCallbacks>,
    ) -> Self {
        let mut dag = Dag::new(device_id, signing_key);

        // Auto-approve the first device.
        let entry = dag.append(Action::DeviceApproved { device_id, role });
        callbacks.on_dag_entry(entry.to_bytes());

        // Set the device name.
        let entry = dag.append(Action::DeviceNameChanged {
            device_id,
            name: device_name,
        });
        callbacks.on_dag_entry(entry.to_bytes());

        info!(%device_id, "engine: network created");
        callbacks.on_event(EngineEvent::NetworkCreated { device_id });

        Self { dag, callbacks }
    }

    /// Create a new engine for a device **joining** an existing network.
    ///
    /// The device is not yet approved — it broadcasts a join request.
    pub fn join_network(
        device_id: DeviceId,
        signing_key: SigningKey,
        device_name: String,
        callbacks: Arc<dyn PlatformCallbacks>,
    ) -> Self {
        let mut dag = Dag::new(device_id, signing_key);

        // Broadcast a join request.
        let entry = dag.append(Action::DeviceJoinRequest {
            device_id,
            name: device_name,
        });
        callbacks.on_dag_entry(entry.to_bytes());

        info!(%device_id, "engine: join request sent");
        callbacks.on_event(EngineEvent::NetworkJoined { device_id });

        Self { dag, callbacks }
    }

    /// Create an engine from an already-initialized DAG (for testing or
    /// advanced use). No initial entries are appended.
    pub fn from_dag(dag: Dag, callbacks: Arc<dyn PlatformCallbacks>) -> Self {
        Self { dag, callbacks }
    }

    // -----------------------------------------------------------------
    // Startup: loading persisted entries
    // -----------------------------------------------------------------

    /// Load a previously persisted DAG entry on startup.
    pub fn load_entry(&mut self, entry: DagEntry) -> Result<(), EngineError> {
        self.dag.load_entry(entry)?;
        Ok(())
    }

    /// Load a DAG entry from serialized bytes.
    pub fn load_entry_bytes(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let entry = DagEntry::from_bytes(bytes)?;
        self.dag.load_entry(entry)?;
        Ok(())
    }

    /// Re-detect all conflicts after loading entries (e.g., on startup).
    ///
    /// Must be called after all entries have been loaded via [`load_entry`] or
    /// [`load_entry_bytes`]. During loading, conflicts are not detected because
    /// parent entries may not yet be available for ancestry checks.
    ///
    /// Also restores correct file metadata for previously resolved conflicts.
    pub fn rebuild_conflicts(&mut self) {
        self.dag.state_mut().conflicts.clear();

        // Collect all unique file keys that have mutations.
        let all_entries = self.dag.all_entries();
        let mut file_keys: std::collections::BTreeSet<(FolderId, String)> =
            std::collections::BTreeSet::new();
        for e in &all_entries {
            match &e.action {
                Action::FileAdded { metadata } => {
                    file_keys.insert((metadata.folder_id, metadata.path.clone()));
                }
                Action::FileModified {
                    folder_id, path, ..
                } => {
                    file_keys.insert((*folder_id, path.clone()));
                }
                Action::FileDeleted { folder_id, path } => {
                    file_keys.insert((*folder_id, path.clone()));
                }
                _ => {}
            }
        }

        // For each file, check for unresolved concurrent mutations.
        for key in &file_keys {
            let file_entries = self.find_file_mutation_entries(key);
            if file_entries.len() < 2 {
                continue;
            }

            let mut versions = Vec::new();
            for fe in &file_entries {
                let is_ancestor_of_any = file_entries.iter().any(|other| {
                    other.hash != fe.hash && self.dag.is_ancestor(&fe.hash, &other.hash)
                });
                if !is_ancestor_of_any
                    && let Some(version) = self.entry_to_conflict_version(fe)
                    && !versions
                        .iter()
                        .any(|v: &ConflictVersion| v.dag_entry_hash == fe.hash)
                {
                    versions.push(version);
                }
            }

            if versions.len() >= 2 {
                let hlc = versions.iter().map(|v| v.hlc).max().unwrap_or(0);
                let conflict = ConflictInfo {
                    folder_id: key.0,
                    path: key.1.clone(),
                    versions,
                    detected_at: hlc,
                };
                self.dag.state_mut().conflicts.push(conflict);
            }
        }

        // Fix metadata for ConflictResolved entries whose apply() only set blob_hash.
        for entry in &all_entries {
            if let Action::ConflictResolved {
                folder_id,
                path,
                chosen_hash,
                ..
            } = &entry.action
            {
                // Find the original DAG entry for the chosen version.
                let chosen_entry = all_entries.iter().find(|e| match &e.action {
                    Action::FileAdded { metadata } => {
                        metadata.blob_hash == *chosen_hash
                            && metadata.folder_id == *folder_id
                            && metadata.path == *path
                    }
                    Action::FileModified {
                        metadata,
                        folder_id: fid,
                        path: p,
                        ..
                    } => metadata.blob_hash == *chosen_hash && fid == folder_id && p == path,
                    _ => false,
                });

                let key = (*folder_id, path.clone());
                if let Some(orig) = chosen_entry {
                    match &orig.action {
                        Action::FileAdded { metadata } | Action::FileModified { metadata, .. } => {
                            self.dag.state_mut().files.insert(key, metadata.clone());
                        }
                        _ => {}
                    }
                } else if chosen_hash.as_bytes() == &[0u8; 32] {
                    // Resolved in favour of delete.
                    self.dag.state_mut().files.remove(&key);
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Device management
    // -----------------------------------------------------------------

    /// Approve a pending device.
    pub fn approve_device(
        &mut self,
        device_id: DeviceId,
        role: DeviceRole,
    ) -> Result<DagEntry, EngineError> {
        let entry = self.dag.append(Action::DeviceApproved { device_id, role });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%device_id, ?role, "engine: device approved");
        self.callbacks
            .on_event(EngineEvent::DeviceApproved { device_id, role });

        Ok(entry)
    }

    /// Revoke a device.
    pub fn revoke_device(&mut self, device_id: DeviceId) -> Result<DagEntry, EngineError> {
        let entry = self.dag.append(Action::DeviceRevoked { device_id });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%device_id, "engine: device revoked");
        self.callbacks
            .on_event(EngineEvent::DeviceRevoked { device_id });

        Ok(entry)
    }

    /// List all known devices.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.dag.state().devices.values().cloned().collect()
    }

    /// List pending (unapproved) devices.
    pub fn pending_requests(&self) -> Vec<DeviceInfo> {
        self.dag
            .state()
            .devices
            .values()
            .filter(|d| !d.approved)
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------
    // Folder management
    // -----------------------------------------------------------------

    /// Create a new shared folder.
    ///
    /// The creator is automatically subscribed as ReadWrite.
    pub fn create_folder(
        &mut self,
        name: &str,
    ) -> Result<(SharedFolder, Vec<DagEntry>), EngineError> {
        let clock = self.dag.clock_tick();
        let device_id = self.dag.device_id();

        // Compute a deterministic folder ID.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&clock.to_le_bytes());
        hasher.update(device_id.as_bytes());
        hasher.update(name.as_bytes());
        let folder_id = FolderId::from_bytes(*hasher.finalize().as_bytes());

        let folder = SharedFolder {
            folder_id,
            name: name.to_string(),
            created_by: device_id,
            created_at: clock,
        };

        let entry = self.dag.append(Action::FolderCreated {
            folder: folder.clone(),
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        // Auto-subscribe the creator as ReadWrite.
        let sub_entry = self.dag.append(Action::FolderSubscribed {
            folder_id,
            device_id,
            mode: SyncMode::ReadWrite,
        });
        self.callbacks.on_dag_entry(sub_entry.to_bytes());

        info!(%folder_id, name, "engine: folder created");
        self.callbacks.on_event(EngineEvent::FolderCreated {
            folder_id,
            name: name.to_string(),
        });

        Ok((folder, vec![entry, sub_entry]))
    }

    /// Remove a shared folder from the network.
    pub fn remove_folder(&mut self, folder_id: FolderId) -> Result<DagEntry, EngineError> {
        if !self.dag.state().folders.contains_key(&folder_id) {
            return Err(EngineError::FolderNotFound(folder_id.to_string()));
        }

        let entry = self.dag.append(Action::FolderRemoved { folder_id });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%folder_id, "engine: folder removed");
        Ok(entry)
    }

    /// Subscribe this device to a folder.
    pub fn subscribe_folder(
        &mut self,
        folder_id: FolderId,
        mode: SyncMode,
    ) -> Result<DagEntry, EngineError> {
        if !self.dag.state().folders.contains_key(&folder_id) {
            return Err(EngineError::FolderNotFound(folder_id.to_string()));
        }

        let device_id = self.dag.device_id();
        let entry = self.dag.append(Action::FolderSubscribed {
            folder_id,
            device_id,
            mode,
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%folder_id, %device_id, %mode, "engine: folder subscribed");
        self.callbacks.on_event(EngineEvent::FolderSubscribed {
            folder_id,
            device_id,
            mode,
        });

        Ok(entry)
    }

    /// Unsubscribe this device from a folder.
    pub fn unsubscribe_folder(&mut self, folder_id: FolderId) -> Result<DagEntry, EngineError> {
        let device_id = self.dag.device_id();
        let entry = self.dag.append(Action::FolderUnsubscribed {
            folder_id,
            device_id,
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%folder_id, %device_id, "engine: folder unsubscribed");
        Ok(entry)
    }

    /// List all shared folders.
    pub fn list_folders(&self) -> Vec<SharedFolder> {
        self.dag.state().folders.values().cloned().collect()
    }

    /// Get subscriptions for a folder.
    pub fn folder_subscriptions(&self, folder_id: FolderId) -> Vec<FolderSubscription> {
        self.dag
            .state()
            .subscriptions
            .iter()
            .filter(|((fid, _), _)| *fid == folder_id)
            .map(|(_, sub)| sub.clone())
            .collect()
    }

    /// List files in a folder.
    pub fn folder_files(&self, folder_id: FolderId) -> Vec<FileMetadata> {
        self.dag
            .state()
            .files
            .iter()
            .filter(|((fid, _), _)| *fid == folder_id)
            .map(|(_, meta)| meta.clone())
            .collect()
    }

    /// Get version history for a file.
    pub fn file_history(&self, folder_id: FolderId, path: &str) -> Vec<(BlobHash, u64)> {
        self.dag
            .state()
            .file_versions
            .get(&(folder_id, path.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------
    // Conflict detection & resolution
    // -----------------------------------------------------------------

    /// List all active conflicts across all folders.
    pub fn list_conflicts(&self) -> Vec<ConflictInfo> {
        self.dag.state().conflicts.clone()
    }

    /// List active conflicts in a specific folder.
    pub fn list_conflicts_in_folder(&self, folder_id: FolderId) -> Vec<ConflictInfo> {
        self.dag
            .state()
            .conflicts
            .iter()
            .filter(|c| c.folder_id == folder_id)
            .cloned()
            .collect()
    }

    /// Resolve a conflict by choosing one version.
    ///
    /// Creates a `ConflictResolved` DAG entry and removes the conflict from
    /// the active conflicts list. The chosen hash becomes the current version
    /// with its original full metadata restored.
    pub fn resolve_conflict(
        &mut self,
        folder_id: FolderId,
        path: &str,
        chosen_hash: BlobHash,
    ) -> Result<DagEntry, EngineError> {
        // Extract data from the conflict before mutating state.
        let (chosen_entry_hash, discarded_hashes) = {
            let conflict = self
                .dag
                .state()
                .conflicts
                .iter()
                .find(|c| c.folder_id == folder_id && c.path == path)
                .ok_or_else(|| EngineError::ConflictNotFound(path.to_string()))?;

            if !conflict.versions.iter().any(|v| v.blob_hash == chosen_hash) {
                return Err(EngineError::ConflictNotFound(format!(
                    "chosen hash {} not in conflict versions",
                    chosen_hash
                )));
            }

            let chosen_entry_hash = conflict
                .versions
                .iter()
                .find(|v| v.blob_hash == chosen_hash)
                .map(|v| v.dag_entry_hash);

            let discarded: Vec<BlobHash> = conflict
                .versions
                .iter()
                .filter(|v| v.blob_hash != chosen_hash)
                .map(|v| v.blob_hash)
                .collect();

            (chosen_entry_hash, discarded)
        };

        let entry = self.dag.append(Action::ConflictResolved {
            folder_id,
            path: path.to_string(),
            chosen_hash,
            discarded_hashes,
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        // Restore full metadata from the chosen version's original DAG entry.
        // apply() only sets blob_hash; this corrects size, device_origin, etc.
        self.restore_file_metadata(folder_id, path, chosen_entry_hash);

        info!(%folder_id, path, %chosen_hash, "engine: conflict resolved");
        Ok(entry)
    }

    /// Restore full file metadata from a DAG entry after conflict resolution.
    ///
    /// `apply()` only updates `blob_hash` in the chosen file's metadata.
    /// This method looks up the original DAG entry to restore all fields
    /// (size, device_origin, modified_at, etc.). For deletes, it removes
    /// the file entirely.
    fn restore_file_metadata(
        &mut self,
        folder_id: FolderId,
        path: &str,
        chosen_entry_hash: Option<[u8; 32]>,
    ) {
        let Some(entry_hash) = chosen_entry_hash else {
            return;
        };
        let Some(chosen_entry) = self.dag.get_entry(&entry_hash).cloned() else {
            return;
        };
        let key = (folder_id, path.to_string());
        match &chosen_entry.action {
            Action::FileAdded { metadata } | Action::FileModified { metadata, .. } => {
                self.dag.state_mut().files.insert(key, metadata.clone());
            }
            Action::FileDeleted { .. } => {
                self.dag.state_mut().files.remove(&key);
            }
            _ => {}
        }
    }

    /// Check a DAG entry for conflicts and record them if detected.
    ///
    /// Called after applying an entry to the DAG. If the entry is a file
    /// mutation and a previous entry for the same file is not an ancestor
    /// of this entry, a conflict is detected.
    fn detect_conflicts(&mut self, entry: &DagEntry) {
        let file_key = match &entry.action {
            Action::FileAdded { metadata } => Some((metadata.folder_id, metadata.path.clone())),
            Action::FileModified {
                folder_id, path, ..
            } => Some((*folder_id, path.clone())),
            Action::FileDeleted {
                folder_id, path, ..
            } => Some((*folder_id, path.clone())),
            _ => None,
        };

        let Some(key) = file_key else { return };

        // Check if there's already a conflict for this file — if so, check
        // whether this new entry adds another concurrent version.
        let existing_conflict = self
            .dag
            .state()
            .conflicts
            .iter()
            .any(|c| c.folder_id == key.0 && c.path == key.1);
        if existing_conflict {
            // Update existing conflict with new version if concurrent.
            self.update_existing_conflict(entry, &key);
            return;
        }

        // Get the previous entry hash for this file (before this entry was applied).
        // Since the entry was just applied, `file_entry_hashes` now points to
        // `entry.hash`. We need to check if any sibling entry (concurrent) also
        // modified this file.
        //
        // Strategy: look at all entries in the DAG that modify this file.
        // If any two are concurrent (neither is ancestor of the other), conflict.
        let file_entries = self.find_file_mutation_entries(&key);
        if file_entries.len() < 2 {
            return;
        }

        // Check the latest two entries. If the second-to-last is not an ancestor
        // of the last, they're concurrent → conflict.
        let last = &file_entries[file_entries.len() - 1];
        let prev = &file_entries[file_entries.len() - 2];

        if !self.dag.is_ancestor(&prev.hash, &last.hash) {
            // Concurrent modification detected!
            let mut versions = Vec::new();
            // Collect all concurrent versions (not just the last two).
            for fe in &file_entries {
                let is_ancestor_of_any_other = file_entries.iter().any(|other| {
                    other.hash != fe.hash && self.dag.is_ancestor(&fe.hash, &other.hash)
                });
                if !is_ancestor_of_any_other
                    && let Some(version) = self.entry_to_conflict_version(fe)
                    && !versions
                        .iter()
                        .any(|v: &ConflictVersion| v.dag_entry_hash == fe.hash)
                {
                    versions.push(version);
                }
            }

            if versions.len() >= 2 {
                let conflict = ConflictInfo {
                    folder_id: key.0,
                    path: key.1.clone(),
                    versions: versions.clone(),
                    detected_at: entry.hlc,
                };

                // Add to state (we need mutable access to state through the dag).
                // Since MaterializedState is inside Dag and we can't get &mut to it
                // directly, we store conflicts via a method.
                self.push_conflict(conflict.clone());

                self.callbacks.on_event(EngineEvent::ConflictDetected {
                    folder_id: key.0,
                    path: key.1,
                    versions,
                });
            }
        }
    }

    /// Find all DAG entries that mutate or resolve a specific file, in HLC order.
    ///
    /// Includes `ConflictResolved` entries so that ancestry checks correctly
    /// recognise resolved conflicts as causal descendants of the conflicting
    /// entries, preventing re-detection after restart.
    fn find_file_mutation_entries(&self, key: &(FolderId, String)) -> Vec<DagEntry> {
        let mut entries: Vec<DagEntry> = self
            .dag
            .all_entries()
            .into_iter()
            .filter(|e| match &e.action {
                Action::FileAdded { metadata } => {
                    metadata.folder_id == key.0 && metadata.path == key.1
                }
                Action::FileModified {
                    folder_id, path, ..
                } => *folder_id == key.0 && *path == key.1,
                Action::FileDeleted {
                    folder_id, path, ..
                } => *folder_id == key.0 && *path == key.1,
                Action::ConflictResolved {
                    folder_id, path, ..
                } => *folder_id == key.0 && *path == key.1,
                _ => false,
            })
            .collect();
        // Sort by HLC for consistent ordering.
        entries.sort_by_key(|e| e.hlc);
        entries
    }

    /// Convert a DAG entry to a ConflictVersion (if it's a file mutation).
    fn entry_to_conflict_version(&self, entry: &DagEntry) -> Option<ConflictVersion> {
        match &entry.action {
            Action::FileAdded { metadata } => Some(ConflictVersion {
                blob_hash: metadata.blob_hash,
                device_id: entry.device_id,
                hlc: entry.hlc,
                dag_entry_hash: entry.hash,
            }),
            Action::FileModified { metadata, .. } => Some(ConflictVersion {
                blob_hash: metadata.blob_hash,
                device_id: entry.device_id,
                hlc: entry.hlc,
                dag_entry_hash: entry.hash,
            }),
            Action::FileDeleted { .. } => Some(ConflictVersion {
                blob_hash: BlobHash::from_bytes([0u8; 32]),
                device_id: entry.device_id,
                hlc: entry.hlc,
                dag_entry_hash: entry.hash,
            }),
            _ => None,
        }
    }

    /// Update an existing conflict with a new concurrent version.
    fn update_existing_conflict(&mut self, entry: &DagEntry, key: &(FolderId, String)) {
        if let Some(version) = self.entry_to_conflict_version(entry) {
            // Check it's actually concurrent with existing versions.
            let dominated = self
                .dag
                .state()
                .conflicts
                .iter()
                .find(|c| c.folder_id == key.0 && c.path == key.1)
                .map(|c| {
                    c.versions
                        .iter()
                        .any(|v| self.dag.is_ancestor(&entry.hash, &v.dag_entry_hash))
                })
                .unwrap_or(false);

            if !dominated {
                self.push_conflict_version(key, version);
            }
        }
    }

    /// Push a new conflict into the materialized state.
    fn push_conflict(&mut self, conflict: ConflictInfo) {
        // Remove any existing conflict for same file first.
        self.dag
            .state_mut()
            .conflicts
            .retain(|c| !(c.folder_id == conflict.folder_id && c.path == conflict.path));
        self.dag.state_mut().conflicts.push(conflict);
    }

    /// Add a version to an existing conflict.
    fn push_conflict_version(&mut self, key: &(FolderId, String), version: ConflictVersion) {
        if let Some(c) = self
            .dag
            .state_mut()
            .conflicts
            .iter_mut()
            .find(|c| c.folder_id == key.0 && c.path == key.1)
            && !c
                .versions
                .iter()
                .any(|v| v.dag_entry_hash == version.dag_entry_hash)
        {
            c.versions.push(version);
        }
    }

    // -----------------------------------------------------------------
    // File management
    // -----------------------------------------------------------------

    /// Add a file to a folder.
    ///
    /// Creates a `FileAdded` DAG entry. Verifies the device is subscribed as
    /// ReadWrite. Returns `Err` if the file already exists at the same path.
    pub fn add_file(
        &mut self,
        metadata: FileMetadata,
        data: Vec<u8>,
    ) -> Result<DagEntry, EngineError> {
        // Verify blob integrity.
        let actual_hash = BlobHash::from_data(&data);
        if actual_hash != metadata.blob_hash {
            return Err(EngineError::BlobIntegrity {
                expected: metadata.blob_hash.to_string(),
                actual: actual_hash.to_string(),
            });
        }

        // Check folder exists.
        if !self.dag.state().folders.contains_key(&metadata.folder_id) {
            return Err(EngineError::FolderNotFound(metadata.folder_id.to_string()));
        }

        // Check subscription and write permission.
        self.check_write_permission(metadata.folder_id)?;

        // Dedup: skip if file already exists at this path.
        let key = (metadata.folder_id, metadata.path.clone());
        if self.dag.state().files.contains_key(&key) {
            return Err(EngineError::FileAlreadyExists(metadata.path.clone()));
        }

        let blob_hash = metadata.blob_hash;
        let folder_id = metadata.folder_id;
        let path = metadata.path.clone();

        let entry = self.dag.append(Action::FileAdded { metadata });
        self.callbacks.on_dag_entry(entry.to_bytes());
        self.callbacks.on_blob_received(blob_hash, data);

        debug!(%blob_hash, %path, "engine: file added");
        self.callbacks.on_event(EngineEvent::FileSynced {
            blob_hash,
            folder_id,
            path,
        });

        Ok(entry)
    }

    /// Add a file to a folder by streaming from disk.
    ///
    /// Unlike [`add_file`], this method never loads the full file into memory.
    /// It reads the file in two passes:
    /// 1. Streaming blake3 hash computation (64 KiB buffers)
    /// 2. Streaming blob transfer to platform via callbacks
    ///
    /// The platform receives chunks via [`PlatformCallbacks::on_blob_stream_start`],
    /// [`PlatformCallbacks::on_blob_chunk`], and [`PlatformCallbacks::on_blob_stream_complete`].
    pub fn add_file_streaming(
        &mut self,
        folder_id: FolderId,
        path: &str,
        file_path: &std::path::Path,
    ) -> Result<DagEntry, EngineError> {
        use std::io::Read;

        // Check folder exists.
        if !self.dag.state().folders.contains_key(&folder_id) {
            return Err(EngineError::FolderNotFound(folder_id.to_string()));
        }

        // Check subscription and write permission.
        self.check_write_permission(folder_id)?;

        // Dedup: skip if file already exists at this path.
        let key = (folder_id, path.to_string());
        if self.dag.state().files.contains_key(&key) {
            return Err(EngineError::FileAlreadyExists(path.to_string()));
        }

        // Open file and get metadata.
        let file = std::fs::File::open(file_path).map_err(|e| EngineError::Io(e.to_string()))?;
        let file_meta = file
            .metadata()
            .map_err(|e| EngineError::Io(e.to_string()))?;
        let file_size = file_meta.len();

        // Pass 1: streaming blake3 hash computation.
        let blob_hash = BlobHash::from_reader(file).map_err(|e| EngineError::Io(e.to_string()))?;

        // Get file timestamps.
        let created_at = file_meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let modified_at = file_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Build metadata.
        let metadata = FileMetadata {
            blob_hash,
            folder_id,
            path: path.to_string(),
            size: file_size,
            mime_type: None,
            created_at,
            modified_at,
            device_origin: self.dag.device_id(),
        };

        // Pass 2: stream file data to platform via callbacks.
        // NOTE: DAG entry is created AFTER streaming succeeds to avoid orphaned
        // entries when streaming fails (e.g., I/O error, hash verification failure).
        self.callbacks.on_blob_stream_start(blob_hash, file_size);

        let stream_result = (|| -> Result<(), EngineError> {
            let mut file =
                std::fs::File::open(file_path).map_err(|e| EngineError::Io(e.to_string()))?;
            let mut buf = vec![0u8; 65536]; // 64 KiB
            let mut offset = 0u64;

            // Throttle progress events: emit at most every 1 MiB or every 1%.
            let progress_interval = (file_size / 100).max(1024 * 1024).max(1);
            let mut last_progress_at = 0u64;

            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| EngineError::Io(e.to_string()))?;
                if n == 0 {
                    break;
                }
                self.callbacks.on_blob_chunk(blob_hash, offset, &buf[..n]);
                offset += n as u64;

                // Emit throttled progress events.
                if offset - last_progress_at >= progress_interval {
                    self.callbacks.on_event(EngineEvent::BlobTransferProgress {
                        blob_hash,
                        bytes_transferred: offset,
                        total_bytes: file_size,
                    });
                    last_progress_at = offset;
                }
            }

            // Always emit a final progress event at 100%.
            if last_progress_at < offset {
                self.callbacks.on_event(EngineEvent::BlobTransferProgress {
                    blob_hash,
                    bytes_transferred: offset,
                    total_bytes: file_size,
                });
            }

            Ok(())
        })();

        if let Err(e) = stream_result {
            self.callbacks.on_blob_stream_abort(blob_hash);
            return Err(e);
        }

        self.callbacks
            .on_blob_stream_complete(blob_hash)
            .map_err(EngineError::Io)?;

        // Create DAG entry only after blob is fully stored.
        let entry = self.dag.append(Action::FileAdded {
            metadata: metadata.clone(),
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        debug!(%blob_hash, %path, size = file_size, "engine: file added (streaming)");
        self.callbacks.on_event(EngineEvent::FileSynced {
            blob_hash,
            folder_id,
            path: path.to_string(),
        });

        Ok(entry)
    }

    /// Modify an existing file (create a new version).
    pub fn modify_file(
        &mut self,
        folder_id: FolderId,
        path: &str,
        new_metadata: FileMetadata,
        data: Vec<u8>,
    ) -> Result<DagEntry, EngineError> {
        // Verify blob integrity.
        let actual_hash = BlobHash::from_data(&data);
        if actual_hash != new_metadata.blob_hash {
            return Err(EngineError::BlobIntegrity {
                expected: new_metadata.blob_hash.to_string(),
                actual: actual_hash.to_string(),
            });
        }

        // Check folder exists.
        if !self.dag.state().folders.contains_key(&folder_id) {
            return Err(EngineError::FolderNotFound(folder_id.to_string()));
        }

        // Check write permission.
        self.check_write_permission(folder_id)?;

        // Get the current file.
        let key = (folder_id, path.to_string());
        let old_hash = self
            .dag
            .state()
            .files
            .get(&key)
            .map(|m| m.blob_hash)
            .ok_or_else(|| EngineError::FileNotFound(path.to_string()))?;

        let new_hash = new_metadata.blob_hash;

        let entry = self.dag.append(Action::FileModified {
            folder_id,
            path: path.to_string(),
            old_hash,
            new_hash,
            metadata: new_metadata,
        });
        self.callbacks.on_dag_entry(entry.to_bytes());
        self.callbacks.on_blob_received(new_hash, data);

        debug!(%new_hash, %path, "engine: file modified");
        self.callbacks.on_event(EngineEvent::FileModified {
            folder_id,
            path: path.to_string(),
            new_hash,
        });

        Ok(entry)
    }

    /// Delete a file from a folder.
    pub fn delete_file(
        &mut self,
        folder_id: FolderId,
        path: &str,
    ) -> Result<DagEntry, EngineError> {
        // Check folder exists.
        if !self.dag.state().folders.contains_key(&folder_id) {
            return Err(EngineError::FolderNotFound(folder_id.to_string()));
        }

        // Check write permission.
        self.check_write_permission(folder_id)?;

        // Check file exists.
        let key = (folder_id, path.to_string());
        if !self.dag.state().files.contains_key(&key) {
            return Err(EngineError::FileNotFound(path.to_string()));
        }

        let entry = self.dag.append(Action::FileDeleted {
            folder_id,
            path: path.to_string(),
        });
        self.callbacks.on_dag_entry(entry.to_bytes());

        debug!(%folder_id, path, "engine: file deleted");
        Ok(entry)
    }

    /// Check that this device has write permission to a folder.
    fn check_write_permission(&self, folder_id: FolderId) -> Result<(), EngineError> {
        let device_id = self.dag.device_id();
        match self.dag.state().subscriptions.get(&(folder_id, device_id)) {
            Some(sub) if sub.mode == SyncMode::ReadOnly => {
                Err(EngineError::ReadOnlyFolder(folder_id.to_string()))
            }
            Some(_) => Ok(()),
            None => Err(EngineError::NotSubscribed(folder_id.to_string())),
        }
    }

    // -----------------------------------------------------------------
    // Access control
    // -----------------------------------------------------------------

    /// Grant access to a device.
    pub fn grant_access(&mut self, grant: AccessGrant) -> Result<DagEntry, EngineError> {
        let to = grant.to;
        let entry = self.dag.append(Action::AccessGranted { grant });
        self.callbacks.on_dag_entry(entry.to_bytes());

        info!(%to, "engine: access granted");
        self.callbacks.on_event(EngineEvent::AccessGranted { to });

        Ok(entry)
    }

    /// Revoke access for a device.
    pub fn revoke_access(&mut self, to: DeviceId) -> Result<DagEntry, EngineError> {
        let entry = self.dag.append(Action::AccessRevoked { to });
        self.callbacks.on_dag_entry(entry.to_bytes());
        Ok(entry)
    }

    /// Check if a device has an active (non-expired) access grant.
    pub fn has_active_grant(&self, device_id: DeviceId, now_unix: u64) -> bool {
        self.dag
            .state()
            .grants
            .iter()
            .any(|g| g.to == device_id && g.expires_at > now_unix)
    }

    // -----------------------------------------------------------------
    // DAG sync (receive remote entries)
    // -----------------------------------------------------------------

    /// Receive a single remote DAG entry.
    ///
    /// Verifies, applies, and persists via callback. Returns the entry.
    pub fn receive_entry(&mut self, entry: DagEntry) -> Result<DagEntry, EngineError> {
        let entry = self.dag.receive_entry(entry)?;
        self.callbacks.on_dag_entry(entry.to_bytes());

        // Emit events based on the action.
        self.emit_action_event(&entry);

        // Check for conflicts after applying.
        self.detect_conflicts(&entry);

        Ok(entry)
    }

    /// Receive a batch of remote DAG entries (from sync).
    pub fn receive_sync_entries(
        &mut self,
        entries: Vec<DagEntry>,
    ) -> Result<Vec<DagEntry>, EngineError> {
        let new_entries = self.dag.apply_sync_entries(entries)?;
        for entry in &new_entries {
            self.callbacks.on_dag_entry(entry.to_bytes());
            self.emit_action_event(entry);
        }

        // Check for conflicts after all entries are applied.
        for entry in &new_entries {
            self.detect_conflicts(entry);
        }

        if !new_entries.is_empty() {
            self.callbacks.on_event(EngineEvent::DagSynced {
                new_entries: new_entries.len(),
            });
        }

        Ok(new_entries)
    }

    /// Compute the delta (entries the remote is missing).
    pub fn compute_delta(
        &self,
        remote_tips: &std::collections::HashSet<[u8; 32]>,
    ) -> Vec<DagEntry> {
        self.dag.compute_delta(remote_tips)
    }

    /// Auto-merge if multiple tips exist.
    pub fn maybe_merge(&mut self) -> Option<DagEntry> {
        let entry = self.dag.maybe_merge();
        if let Some(ref e) = entry {
            self.callbacks.on_dag_entry(e.to_bytes());
        }
        entry
    }

    // -----------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------

    /// The underlying DAG (read-only).
    pub fn dag(&self) -> &Dag {
        &self.dag
    }

    /// This device's ID.
    pub fn device_id(&self) -> DeviceId {
        self.dag.device_id()
    }

    /// The materialized state.
    pub fn state(&self) -> &murmur_dag::MaterializedState {
        self.dag.state()
    }

    /// All DAG entries (for full sync to a new peer).
    pub fn all_entries(&self) -> Vec<DagEntry> {
        self.dag.all_entries()
    }

    /// Current tip hashes.
    pub fn tips(&self) -> &std::collections::HashSet<[u8; 32]> {
        self.dag.tips()
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    /// Emit an event based on a DAG entry's action.
    fn emit_action_event(&self, entry: &DagEntry) {
        match &entry.action {
            Action::DeviceJoinRequest { device_id, name } => {
                self.callbacks.on_event(EngineEvent::DeviceJoinRequested {
                    device_id: *device_id,
                    name: name.clone(),
                });
            }
            Action::DeviceApproved { device_id, role } => {
                self.callbacks.on_event(EngineEvent::DeviceApproved {
                    device_id: *device_id,
                    role: *role,
                });
            }
            Action::DeviceRevoked { device_id } => {
                self.callbacks.on_event(EngineEvent::DeviceRevoked {
                    device_id: *device_id,
                });
            }
            Action::FolderCreated { folder } => {
                self.callbacks.on_event(EngineEvent::FolderCreated {
                    folder_id: folder.folder_id,
                    name: folder.name.clone(),
                });
            }
            Action::FolderSubscribed {
                folder_id,
                device_id,
                mode,
            } => {
                self.callbacks.on_event(EngineEvent::FolderSubscribed {
                    folder_id: *folder_id,
                    device_id: *device_id,
                    mode: *mode,
                });
            }
            Action::FileAdded { metadata } => {
                self.callbacks.on_event(EngineEvent::FileSynced {
                    blob_hash: metadata.blob_hash,
                    folder_id: metadata.folder_id,
                    path: metadata.path.clone(),
                });
            }
            Action::FileModified {
                folder_id,
                path,
                new_hash,
                ..
            } => {
                self.callbacks.on_event(EngineEvent::FileModified {
                    folder_id: *folder_id,
                    path: path.clone(),
                    new_hash: *new_hash,
                });
            }
            Action::AccessGranted { grant } => {
                self.callbacks
                    .on_event(EngineEvent::AccessGranted { to: grant.to });
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use murmur_types::{AccessScope, DeviceRole};
    use std::sync::Mutex;

    // --- Test callback implementation ---

    #[derive(Default)]
    struct TestCallbacks {
        dag_entries: Mutex<Vec<Vec<u8>>>,
        blobs: Mutex<Vec<(BlobHash, Vec<u8>)>>,
        events: Mutex<Vec<EngineEvent>>,
        /// Streaming blob data: hash → accumulated bytes.
        stream_data: Mutex<std::collections::HashMap<BlobHash, Vec<u8>>>,
        /// Track stream_start calls.
        stream_starts: Mutex<Vec<(BlobHash, u64)>>,
        /// Track stream_complete calls.
        stream_completes: Mutex<Vec<BlobHash>>,
    }

    impl PlatformCallbacks for TestCallbacks {
        fn on_dag_entry(&self, entry_bytes: Vec<u8>) {
            self.dag_entries.lock().unwrap().push(entry_bytes);
        }

        fn on_blob_received(&self, blob_hash: BlobHash, data: Vec<u8>) {
            self.blobs.lock().unwrap().push((blob_hash, data));
        }

        fn on_blob_needed(&self, blob_hash: BlobHash) -> Option<Vec<u8>> {
            self.blobs
                .lock()
                .unwrap()
                .iter()
                .find(|(h, _)| *h == blob_hash)
                .map(|(_, d)| d.clone())
        }

        fn on_event(&self, event: EngineEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn on_blob_stream_start(&self, blob_hash: BlobHash, total_size: u64) {
            self.stream_starts
                .lock()
                .unwrap()
                .push((blob_hash, total_size));
            self.stream_data
                .lock()
                .unwrap()
                .insert(blob_hash, Vec::new());
        }

        fn on_blob_chunk(&self, blob_hash: BlobHash, offset: u64, data: &[u8]) {
            let mut streams = self.stream_data.lock().unwrap();
            if let Some(buf) = streams.get_mut(&blob_hash) {
                // Ensure buffer is large enough.
                let needed = offset as usize + data.len();
                if buf.len() < needed {
                    buf.resize(needed, 0);
                }
                buf[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            }
        }

        fn on_blob_stream_complete(&self, blob_hash: BlobHash) -> Result<(), String> {
            // Verify hash.
            let streams = self.stream_data.lock().unwrap();
            if let Some(data) = streams.get(&blob_hash) {
                let actual = BlobHash::from_data(data);
                if actual != blob_hash {
                    return Err(format!("hash mismatch: expected {blob_hash}, got {actual}"));
                }
            }
            self.stream_completes.lock().unwrap().push(blob_hash);
            Ok(())
        }

        fn on_blob_stream_abort(&self, blob_hash: BlobHash) {
            self.stream_data.lock().unwrap().remove(&blob_hash);
        }
    }

    fn make_keypair() -> (DeviceId, SigningKey) {
        let sk = SigningKey::from_bytes(&rand::random());
        let id = DeviceId::from_verifying_key(&sk.verifying_key());
        (id, sk)
    }

    fn make_engine(name: &str) -> (MurmurEngine, Arc<TestCallbacks>) {
        let (id, sk) = make_keypair();
        let cb = Arc::new(TestCallbacks::default());
        let engine =
            MurmurEngine::create_network(id, sk, name.to_string(), DeviceRole::Full, cb.clone());
        (engine, cb)
    }

    /// Create a test engine with a folder already set up.
    fn make_engine_with_folder(name: &str) -> (MurmurEngine, Arc<TestCallbacks>, FolderId) {
        let (mut engine, cb) = make_engine(name);
        let (folder, _) = engine.create_folder("test-folder").unwrap();
        (engine, cb, folder.folder_id)
    }

    fn make_file_data(content: &[u8], folder_id: FolderId) -> (FileMetadata, Vec<u8>) {
        let data = content.to_vec();
        let hash = BlobHash::from_data(&data);
        let meta = FileMetadata {
            blob_hash: hash,
            folder_id,
            path: "test.txt".to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: DeviceId::from_data(b"origin"),
        };
        (meta, data)
    }

    // --- Network creation ---

    #[test]
    fn test_create_network_first_device_approved() {
        let (engine, _cb) = make_engine("NAS");
        let devices = engine.list_devices();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].approved);
        assert_eq!(devices[0].name, "NAS");
    }

    #[test]
    fn test_create_network_emits_event() {
        let (engine, cb) = make_engine("NAS");
        let events = cb.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::NetworkCreated { .. }))
        );
        let _ = engine;
    }

    #[test]
    fn test_create_network_persists_entries() {
        let (_engine, cb) = make_engine("NAS");
        // Should have at least 2 entries: DeviceApproved + DeviceNameChanged
        assert!(cb.dag_entries.lock().unwrap().len() >= 2);
    }

    // --- Join network ---

    #[test]
    fn test_join_network_creates_pending_device() {
        let (id, sk) = make_keypair();
        let cb = Arc::new(TestCallbacks::default());
        let engine = MurmurEngine::join_network(id, sk, "Phone".to_string(), cb.clone());

        let devices = engine.list_devices();
        assert_eq!(devices.len(), 1);
        assert!(!devices[0].approved);
    }

    #[test]
    fn test_join_network_emits_event() {
        let (id, sk) = make_keypair();
        let cb = Arc::new(TestCallbacks::default());
        let _engine = MurmurEngine::join_network(id, sk, "Phone".to_string(), cb.clone());
        let events = cb.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::NetworkJoined { .. }))
        );
    }

    // --- Device approval ---

    #[test]
    fn test_approve_device() {
        let (mut engine, cb) = make_engine("NAS");
        let (new_id, _) = make_keypair();

        engine.approve_device(new_id, DeviceRole::Source).unwrap();

        let devices = engine.list_devices();
        assert_eq!(devices.len(), 2);
        let new_dev = devices.iter().find(|d| d.device_id == new_id).unwrap();
        assert!(new_dev.approved);
        assert_eq!(new_dev.role, DeviceRole::Source);

        let events = cb.events.lock().unwrap();
        assert!(events.iter().any(
            |e| matches!(e, EngineEvent::DeviceApproved { device_id, .. } if *device_id == new_id)
        ));
    }

    #[test]
    fn test_approve_device_on_existing_device() {
        let (mut engine1, _cb1) = make_engine("NAS");
        let (new_id, new_sk) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());
        let mut engine2 =
            MurmurEngine::join_network(new_id, new_sk, "Phone".to_string(), cb2.clone());

        // Sync engine1's existing entries to engine2 before approval.
        let pre_delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(pre_delta).unwrap();

        // Engine1 approves new device.
        let approval = engine1.approve_device(new_id, DeviceRole::Source).unwrap();

        // Engine2 receives the approval entry.
        engine2.receive_entry(approval).unwrap();

        // Engine2 should now see itself as approved.
        let dev = engine2
            .state()
            .devices
            .get(&new_id)
            .expect("device should exist");
        assert!(dev.approved);
    }

    // --- Device revocation ---

    #[test]
    fn test_revoke_device() {
        let (mut engine, _cb) = make_engine("NAS");
        let (new_id, _) = make_keypair();

        engine.approve_device(new_id, DeviceRole::Backup).unwrap();
        engine.revoke_device(new_id).unwrap();

        let dev = engine.state().devices.get(&new_id).unwrap();
        assert!(!dev.approved);
    }

    // --- Pending requests ---

    #[test]
    fn test_pending_requests() {
        let (mut engine, _cb) = make_engine("NAS");
        let (id_a, _) = make_keypair();
        let (id_b, _) = make_keypair();

        // Simulate join requests by appending directly.
        engine.dag.append(Action::DeviceJoinRequest {
            device_id: id_a,
            name: "Phone A".to_string(),
        });
        engine.dag.append(Action::DeviceJoinRequest {
            device_id: id_b,
            name: "Phone B".to_string(),
        });

        let pending = engine.pending_requests();
        assert_eq!(pending.len(), 2);
    }

    // --- Folder management ---

    #[test]
    fn test_create_folder() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _entry) = engine.create_folder("Photos").unwrap();

        assert_eq!(folder.name, "Photos");
        let folders = engine.list_folders();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Photos");
    }

    #[test]
    fn test_create_folder_auto_subscribes_creator() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();

        let subs = engine.folder_subscriptions(folder.folder_id);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].device_id, engine.device_id());
        assert_eq!(subs[0].mode, SyncMode::ReadWrite);
    }

    #[test]
    fn test_remove_folder() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();

        engine.remove_folder(folder.folder_id).unwrap();
        assert!(engine.list_folders().is_empty());
    }

    #[test]
    fn test_remove_folder_not_found() {
        let (mut engine, _cb) = make_engine("NAS");
        let result = engine.remove_folder(FolderId::from_data(b"nonexistent"));
        assert!(matches!(result, Err(EngineError::FolderNotFound(_))));
    }

    #[test]
    fn test_subscribe_folder_read_write() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();

        // Creator is already subscribed; subscribe another device
        let (new_id, _) = make_keypair();
        engine.approve_device(new_id, DeviceRole::Full).unwrap();

        // The subscription is for THIS device, so we test the mode is recorded
        let subs = engine.folder_subscriptions(folder.folder_id);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].mode, SyncMode::ReadWrite);
    }

    #[test]
    fn test_subscribe_folder_read_only() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();

        // Unsubscribe and re-subscribe as ReadOnly
        engine.unsubscribe_folder(folder.folder_id).unwrap();
        engine
            .subscribe_folder(folder.folder_id, SyncMode::ReadOnly)
            .unwrap();

        let subs = engine.folder_subscriptions(folder.folder_id);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].mode, SyncMode::ReadOnly);
    }

    #[test]
    fn test_unsubscribe_folder() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();

        engine.unsubscribe_folder(folder.folder_id).unwrap();
        let subs = engine.folder_subscriptions(folder.folder_id);
        assert!(subs.is_empty());
    }

    #[test]
    fn test_subscribe_nonexistent_folder() {
        let (mut engine, _cb) = make_engine("NAS");
        let result = engine.subscribe_folder(FolderId::from_data(b"nope"), SyncMode::ReadWrite);
        assert!(matches!(result, Err(EngineError::FolderNotFound(_))));
    }

    // --- File management ---

    #[test]
    fn test_add_file() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let (meta, data) = make_file_data(b"hello world", folder_id);

        engine.add_file(meta.clone(), data).unwrap();

        let key = (folder_id, "test.txt".to_string());
        assert!(engine.state().files.contains_key(&key));

        // Callback: blob received.
        let blobs = cb.blobs.lock().unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].0, meta.blob_hash);

        // Callback: event.
        let events = cb.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::FileSynced { .. }))
        );
    }

    #[test]
    fn test_add_duplicate_file_rejected() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");
        let (meta, data) = make_file_data(b"duplicate", folder_id);

        engine.add_file(meta.clone(), data.clone()).unwrap();
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::FileAlreadyExists(_))));
    }

    #[test]
    fn test_add_file_bad_hash_rejected() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");
        let data = b"actual content".to_vec();
        let meta = FileMetadata {
            blob_hash: BlobHash::from_data(b"wrong content"),
            folder_id,
            path: "bad.txt".to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: DeviceId::from_data(b"x"),
        };
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::BlobIntegrity { .. })));
    }

    #[test]
    fn test_add_file_on_dag_entry_callback() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let initial_count = cb.dag_entries.lock().unwrap().len();
        let (meta, data) = make_file_data(b"content", folder_id);
        engine.add_file(meta, data).unwrap();
        assert!(cb.dag_entries.lock().unwrap().len() > initial_count);
    }

    #[test]
    fn test_add_file_not_subscribed() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();
        engine.unsubscribe_folder(folder.folder_id).unwrap();

        let (meta, data) = make_file_data(b"content", folder.folder_id);
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::NotSubscribed(_))));
    }

    #[test]
    fn test_add_file_read_only_rejected() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder, _) = engine.create_folder("Photos").unwrap();
        engine.unsubscribe_folder(folder.folder_id).unwrap();
        engine
            .subscribe_folder(folder.folder_id, SyncMode::ReadOnly)
            .unwrap();

        let (meta, data) = make_file_data(b"content", folder.folder_id);
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::ReadOnlyFolder(_))));
    }

    #[test]
    fn test_modify_file() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");
        let (meta, data) = make_file_data(b"version 1", folder_id);
        engine.add_file(meta.clone(), data).unwrap();

        // Modify the file.
        let new_data = b"version 2".to_vec();
        let new_hash = BlobHash::from_data(&new_data);
        let new_meta = FileMetadata {
            blob_hash: new_hash,
            folder_id,
            path: "test.txt".to_string(),
            size: new_data.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 1,
            device_origin: engine.device_id(),
        };
        engine
            .modify_file(folder_id, "test.txt", new_meta, new_data)
            .unwrap();

        // Check version history.
        let history = engine.file_history(folder_id, "test.txt");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, meta.blob_hash);
        assert_eq!(history[1].0, new_hash);
    }

    #[test]
    fn test_file_history_ordered() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");
        let (meta, data) = make_file_data(b"v1", folder_id);
        engine.add_file(meta, data).unwrap();

        let history = engine.file_history(folder_id, "test.txt");
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn test_folder_files_scoped() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder_a, _) = engine.create_folder("FolderA").unwrap();
        let (folder_b, _) = engine.create_folder("FolderB").unwrap();

        let data_a = b"file in A".to_vec();
        let meta_a = FileMetadata {
            blob_hash: BlobHash::from_data(&data_a),
            folder_id: folder_a.folder_id,
            path: "a.txt".to_string(),
            size: data_a.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine.device_id(),
        };
        engine.add_file(meta_a, data_a).unwrap();

        let data_b = b"file in B".to_vec();
        let meta_b = FileMetadata {
            blob_hash: BlobHash::from_data(&data_b),
            folder_id: folder_b.folder_id,
            path: "b.txt".to_string(),
            size: data_b.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine.device_id(),
        };
        engine.add_file(meta_b, data_b).unwrap();

        assert_eq!(engine.folder_files(folder_a.folder_id).len(), 1);
        assert_eq!(engine.folder_files(folder_b.folder_id).len(), 1);
        assert_eq!(engine.folder_files(folder_a.folder_id)[0].path, "a.txt");
    }

    #[test]
    fn test_same_path_different_folders() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder_a, _) = engine.create_folder("FolderA").unwrap();
        let (folder_b, _) = engine.create_folder("FolderB").unwrap();

        let data1 = b"content A".to_vec();
        let meta1 = FileMetadata {
            blob_hash: BlobHash::from_data(&data1),
            folder_id: folder_a.folder_id,
            path: "same.txt".to_string(),
            size: data1.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine.device_id(),
        };
        engine.add_file(meta1, data1).unwrap();

        let data2 = b"content B".to_vec();
        let meta2 = FileMetadata {
            blob_hash: BlobHash::from_data(&data2),
            folder_id: folder_b.folder_id,
            path: "same.txt".to_string(),
            size: data2.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine.device_id(),
        };
        engine.add_file(meta2, data2).unwrap();

        // Both should exist independently.
        assert_eq!(engine.state().files.len(), 2);
    }

    #[test]
    fn test_remove_folder_does_not_affect_other_folders() {
        let (mut engine, _cb) = make_engine("NAS");
        let (folder_a, _) = engine.create_folder("FolderA").unwrap();
        let (folder_b, _) = engine.create_folder("FolderB").unwrap();

        let data = b"file in B".to_vec();
        let meta = FileMetadata {
            blob_hash: BlobHash::from_data(&data),
            folder_id: folder_b.folder_id,
            path: "keep.txt".to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine.device_id(),
        };
        engine.add_file(meta, data).unwrap();

        engine.remove_folder(folder_a.folder_id).unwrap();

        assert_eq!(engine.list_folders().len(), 1);
        assert_eq!(engine.folder_files(folder_b.folder_id).len(), 1);
    }

    // --- Access control ---

    #[test]
    fn test_grant_access() {
        let (mut engine, _cb) = make_engine("NAS");
        let (to_id, _) = make_keypair();

        let grant = AccessGrant {
            to: to_id,
            from: engine.device_id(),
            scope: AccessScope::AllFiles,
            expires_at: u64::MAX,
            signature_r: [0; 32],
            signature_s: [0; 32],
        };
        engine.grant_access(grant).unwrap();
        assert!(engine.has_active_grant(to_id, 0));
    }

    #[test]
    fn test_revoke_access() {
        let (mut engine, _cb) = make_engine("NAS");
        let (to_id, _) = make_keypair();

        let grant = AccessGrant {
            to: to_id,
            from: engine.device_id(),
            scope: AccessScope::AllFiles,
            expires_at: u64::MAX,
            signature_r: [0; 32],
            signature_s: [0; 32],
        };
        engine.grant_access(grant).unwrap();
        engine.revoke_access(to_id).unwrap();
        assert!(!engine.has_active_grant(to_id, 0));
    }

    #[test]
    fn test_access_expiration() {
        let (mut engine, _cb) = make_engine("NAS");
        let (to_id, _) = make_keypair();

        let grant = AccessGrant {
            to: to_id,
            from: engine.device_id(),
            scope: AccessScope::AllFiles,
            expires_at: 1000,
            signature_r: [0; 32],
            signature_s: [0; 32],
        };
        engine.grant_access(grant).unwrap();

        // Before expiry.
        assert!(engine.has_active_grant(to_id, 999));
        // After expiry.
        assert!(!engine.has_active_grant(to_id, 1000));
    }

    // --- DAG sync ---

    #[test]
    fn test_receive_entry() {
        let (mut engine1, _cb1, folder_id) = make_engine_with_folder("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins as a fresh DAG and syncs engine1's entries.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2.clone());
        let pre_entries = engine1.all_entries();
        engine2.receive_sync_entries(pre_entries).unwrap();

        let (meta, data) = make_file_data(b"sync me", folder_id);
        let entry = engine1.add_file(meta, data).unwrap();

        engine2.receive_entry(entry).unwrap();

        let events = cb2.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::FileSynced { .. }))
        );
    }

    #[test]
    fn test_receive_sync_entries_batch() {
        let (engine1, _cb1) = make_engine("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2.clone());

        // Engine1 creates several entries.
        let entries = engine1.all_entries();

        // Engine2 syncs all entries.
        let new = engine2.receive_sync_entries(entries).unwrap();
        assert!(!new.is_empty());

        let events = cb2.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::DagSynced { .. }))
        );
    }

    #[test]
    fn test_two_devices_add_files_concurrently() {
        let (mut engine1, _cb1, folder_id) = make_engine_with_folder("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins and syncs engine1's entries.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();
        engine2
            .subscribe_folder(folder_id, SyncMode::ReadWrite)
            .unwrap();

        let data1 = b"file from NAS".to_vec();
        let meta1 = FileMetadata {
            blob_hash: BlobHash::from_data(&data1),
            folder_id,
            path: "nas-file.txt".to_string(),
            size: data1.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine1.device_id(),
        };
        engine1.add_file(meta1.clone(), data1).unwrap();

        let data2 = b"file from Phone".to_vec();
        let meta2 = FileMetadata {
            blob_hash: BlobHash::from_data(&data2),
            folder_id,
            path: "phone-file.txt".to_string(),
            size: data2.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: DeviceId::from_data(b"phone"),
        };
        engine2.add_file(meta2.clone(), data2).unwrap();

        // Sync: engine1 → engine2.
        let delta1 = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta1).unwrap();

        // Sync: engine2 → engine1.
        let delta2 = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta2).unwrap();

        // Both should have both files.
        let key1 = (folder_id, "nas-file.txt".to_string());
        let key2 = (folder_id, "phone-file.txt".to_string());
        assert!(engine1.state().files.contains_key(&key1));
        assert!(engine1.state().files.contains_key(&key2));
        assert!(engine2.state().files.contains_key(&key1));
        assert!(engine2.state().files.contains_key(&key2));
    }

    #[test]
    fn test_full_sync_new_device_gets_everything() {
        let (mut engine1, _cb1, folder_id) = make_engine_with_folder("NAS");

        // Add a file and approve a device.
        let (meta, data) = make_file_data(b"important", folder_id);
        engine1.add_file(meta.clone(), data).unwrap();
        let (peer_id, _) = make_keypair();
        engine1.approve_device(peer_id, DeviceRole::Backup).unwrap();

        // New device joins with empty DAG and syncs.
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2.clone());

        let all = engine1.all_entries();
        engine2.receive_sync_entries(all).unwrap();

        // Engine2 should have the file and the approved device.
        let key = (folder_id, "test.txt".to_string());
        assert!(engine2.state().files.contains_key(&key));
        assert!(engine2.state().devices.contains_key(&peer_id));
    }

    // --- Merge ---

    #[test]
    fn test_maybe_merge() {
        let (mut engine1, _cb1, folder_id) = make_engine_with_folder("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins and syncs.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();
        engine2
            .subscribe_folder(folder_id, SyncMode::ReadWrite)
            .unwrap();

        let data_a = b"a".to_vec();
        let meta_a = FileMetadata {
            blob_hash: BlobHash::from_data(&data_a),
            folder_id,
            path: "a.txt".to_string(),
            size: data_a.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: engine1.device_id(),
        };
        engine1.add_file(meta_a, data_a).unwrap();

        let data_b = b"b".to_vec();
        let meta_b = FileMetadata {
            blob_hash: BlobHash::from_data(&data_b),
            folder_id,
            path: "b.txt".to_string(),
            size: data_b.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: DeviceId::from_data(b"phone"),
        };
        engine2.add_file(meta_b, data_b).unwrap();

        // Sync engine2's entries into engine1 → creates 2+ tips.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Merge.
        let merge = engine1.maybe_merge();
        assert!(merge.is_some());
        assert_eq!(engine1.tips().len(), 1);
    }

    // --- Callbacks ---

    #[test]
    fn test_on_blob_needed_callback() {
        let cb = Arc::new(TestCallbacks::default());
        let content = b"test blob data".to_vec();
        let hash = BlobHash::from_data(&content);

        cb.on_blob_received(hash, content.clone());

        let loaded = cb.on_blob_needed(hash);
        assert_eq!(loaded, Some(content));

        let missing = cb.on_blob_needed(BlobHash::from_data(b"nonexistent"));
        assert_eq!(missing, None);
    }

    // --- Load entries on startup ---

    #[test]
    fn test_load_entry_bytes() {
        let (engine1, _cb1) = make_engine("NAS");
        let entries = engine1.all_entries();

        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);

        for entry in entries {
            let bytes = entry.to_bytes();
            engine2.load_entry_bytes(&bytes).unwrap();
        }

        // Should have reconstructed the device.
        assert!(!engine2.list_devices().is_empty());
    }

    // --- Device ID ---

    #[test]
    fn test_device_id() {
        let (id, sk) = make_keypair();
        let cb = Arc::new(TestCallbacks::default());
        let engine = MurmurEngine::create_network(id, sk, "NAS".to_string(), DeviceRole::Full, cb);
        assert_eq!(engine.device_id(), id);
    }

    // --- Conflict detection & resolution (Milestone 14) ---

    /// Helper: set up two engines with a shared folder, engine2 approved and subscribed.
    fn make_two_engines_with_folder() -> (
        MurmurEngine,
        Arc<TestCallbacks>,
        MurmurEngine,
        Arc<TestCallbacks>,
        FolderId,
    ) {
        let (mut engine1, cb1, folder_id) = make_engine_with_folder("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2.clone());
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();
        engine2
            .subscribe_folder(folder_id, SyncMode::ReadWrite)
            .unwrap();

        (engine1, cb1, engine2, cb2, folder_id)
    }

    /// Helper: make file metadata with a specific path.
    fn make_file_data_path(
        content: &[u8],
        folder_id: FolderId,
        path: &str,
        device_id: DeviceId,
    ) -> (FileMetadata, Vec<u8>) {
        let data = content.to_vec();
        let hash = BlobHash::from_data(&data);
        let meta = FileMetadata {
            blob_hash: hash,
            folder_id,
            path: path.to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
            modified_at: 0,
            device_origin: device_id,
        };
        (meta, data)
    }

    #[test]
    fn test_concurrent_file_add_same_path_creates_conflict() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Both devices add a file at the same path concurrently (before syncing).
        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        // Sync: engine2's entries → engine1.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // engine1 should detect a conflict.
        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].folder_id, folder_id);
        assert_eq!(conflicts[0].path, "doc.txt");
        assert_eq!(conflicts[0].versions.len(), 2);
    }

    #[test]
    fn test_concurrent_file_modify_creates_conflict() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Add a file on engine1 and sync to engine2.
        let (meta, data) =
            make_file_data_path(b"original", folder_id, "shared.txt", engine1.device_id());
        engine1.add_file(meta, data).unwrap();
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();

        // Both modify the same file concurrently.
        let (new_meta1, new_data1) = make_file_data_path(
            b"modified by NAS",
            folder_id,
            "shared.txt",
            engine1.device_id(),
        );
        engine1
            .modify_file(folder_id, "shared.txt", new_meta1, new_data1)
            .unwrap();

        let (new_meta2, new_data2) = make_file_data_path(
            b"modified by Phone",
            folder_id,
            "shared.txt",
            engine2.device_id(),
        );
        engine2
            .modify_file(folder_id, "shared.txt", new_meta2, new_data2)
            .unwrap();

        // Sync engine2 → engine1.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "shared.txt");
        assert_eq!(conflicts[0].versions.len(), 2);
    }

    #[test]
    fn test_sequential_modifications_no_conflict() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Add a file on engine1 and sync to engine2.
        let (meta, data) =
            make_file_data_path(b"original", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta, data).unwrap();
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();

        // Engine1 modifies, syncs to engine2, then engine2 modifies.
        let (new_meta1, new_data1) =
            make_file_data_path(b"v2 by NAS", folder_id, "doc.txt", engine1.device_id());
        engine1
            .modify_file(folder_id, "doc.txt", new_meta1, new_data1)
            .unwrap();

        // Sync engine1 → engine2.
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();

        // Now engine2 modifies (this is sequential, not concurrent).
        let (new_meta2, new_data2) =
            make_file_data_path(b"v3 by Phone", folder_id, "doc.txt", engine2.device_id());
        engine2
            .modify_file(folder_id, "doc.txt", new_meta2, new_data2)
            .unwrap();

        // Sync engine2 → engine1.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // No conflict since edits were sequential.
        assert!(engine1.list_conflicts().is_empty());
    }

    #[test]
    fn test_resolve_conflict_removes_from_list() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create a conflict.
        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        assert_eq!(engine1.list_conflicts().len(), 1);

        // Resolve choosing version A.
        engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        assert!(engine1.list_conflicts().is_empty());
    }

    #[test]
    fn test_resolve_conflict_creates_dag_entry() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        let hash_b = meta2.blob_hash;
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        let entry = engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        match &entry.action {
            Action::ConflictResolved {
                folder_id: fid,
                path,
                chosen_hash,
                discarded_hashes,
            } => {
                assert_eq!(*fid, folder_id);
                assert_eq!(path, "doc.txt");
                assert_eq!(*chosen_hash, hash_a);
                assert_eq!(discarded_hashes.len(), 1);
                assert_eq!(discarded_hashes[0], hash_b);
            }
            other => panic!("expected ConflictResolved, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_conflict_updates_folder_files() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        // The chosen version should be the current file.
        let file = engine1
            .state()
            .files
            .get(&(folder_id, "doc.txt".to_string()))
            .expect("file should exist");
        assert_eq!(file.blob_hash, hash_a);
    }

    #[test]
    fn test_multiple_conflicts_independent() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create conflicts on two different files.
        let (meta1a, data1a) =
            make_file_data_path(b"A doc1", folder_id, "doc1.txt", engine1.device_id());
        engine1.add_file(meta1a, data1a).unwrap();
        let (meta1b, data1b) =
            make_file_data_path(b"A doc2", folder_id, "doc2.txt", engine1.device_id());
        engine1.add_file(meta1b, data1b).unwrap();

        let (meta2a, data2a) =
            make_file_data_path(b"B doc1", folder_id, "doc1.txt", engine2.device_id());
        engine2.add_file(meta2a, data2a).unwrap();
        let (meta2b, data2b) =
            make_file_data_path(b"B doc2", folder_id, "doc2.txt", engine2.device_id());
        engine2.add_file(meta2b, data2b).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Should have 2 independent conflicts.
        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 2);

        // Resolving one doesn't affect the other.
        let hash_doc1 = BlobHash::from_data(b"A doc1");
        engine1
            .resolve_conflict(folder_id, "doc1.txt", hash_doc1)
            .unwrap();

        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "doc2.txt");
    }

    #[test]
    fn test_three_way_conflict() {
        let (mut engine1, _cb1, folder_id) = make_engine_with_folder("NAS");
        let (id2, sk2) = make_keypair();
        let (id3, sk3) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());
        let cb3 = Arc::new(TestCallbacks::default());

        engine1.approve_device(id2, DeviceRole::Source).unwrap();
        engine1.approve_device(id3, DeviceRole::Source).unwrap();

        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();
        engine2
            .subscribe_folder(folder_id, SyncMode::ReadWrite)
            .unwrap();

        let mut engine3 = MurmurEngine::from_dag(Dag::new(id3, sk3), cb3);
        engine3.receive_sync_entries(engine1.all_entries()).unwrap();
        engine3
            .subscribe_folder(folder_id, SyncMode::ReadWrite)
            .unwrap();

        // All three add the same file path concurrently.
        let (m1, d1) = make_file_data_path(b"v1", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(m1, d1).unwrap();

        let (m2, d2) = make_file_data_path(b"v2", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(m2, d2).unwrap();

        let (m3, d3) = make_file_data_path(b"v3", folder_id, "doc.txt", engine3.device_id());
        engine3.add_file(m3, d3).unwrap();

        // Sync all to engine1.
        let delta2 = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta2).unwrap();

        let delta3 = engine3.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta3).unwrap();

        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 1);
        // Should capture all 3 concurrent versions.
        assert!(conflicts[0].versions.len() >= 3);
    }

    #[test]
    fn test_conflict_detected_event_emitted() {
        let (mut engine1, cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Check that a ConflictDetected event was emitted.
        let events = cb1.events.lock().unwrap();
        let conflict_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, EngineEvent::ConflictDetected { .. }))
            .collect();
        assert_eq!(conflict_events.len(), 1);

        match &conflict_events[0] {
            EngineEvent::ConflictDetected {
                folder_id: fid,
                path,
                versions,
            } => {
                assert_eq!(*fid, folder_id);
                assert_eq!(path, "doc.txt");
                assert_eq!(versions.len(), 2);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_delete_vs_modify_concurrent_conflict() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Add a file and sync.
        let (meta, data) =
            make_file_data_path(b"original", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta, data).unwrap();
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();

        // Engine1 modifies the file, engine2 deletes it — concurrently.
        let (new_meta, new_data) =
            make_file_data_path(b"updated", folder_id, "doc.txt", engine1.device_id());
        engine1
            .modify_file(folder_id, "doc.txt", new_meta, new_data)
            .unwrap();

        engine2.delete_file(folder_id, "doc.txt").unwrap();

        // Sync engine2 → engine1.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Should be a conflict (delete vs modify).
        let conflicts = engine1.list_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "doc.txt");
        assert!(conflicts[0].versions.len() >= 2);
    }

    #[test]
    fn test_resolve_already_resolved_conflict_error() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Resolve the conflict.
        engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        // Try to resolve again — should error.
        let result = engine1.resolve_conflict(folder_id, "doc.txt", hash_a);
        assert!(matches!(result, Err(EngineError::ConflictNotFound(_))));
    }

    #[test]
    fn test_list_conflicts_in_folder() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create another folder on engine1.
        let (folder2, _) = engine1.create_folder("folder2").unwrap();
        let folder2_id = folder2.folder_id;

        // Sync to engine2 so it knows about folder2.
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();
        engine2
            .subscribe_folder(folder2_id, SyncMode::ReadWrite)
            .unwrap();

        // Create a conflict in folder1.
        let (m1, d1) = make_file_data_path(b"A", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(m1, d1).unwrap();
        let (m2, d2) = make_file_data_path(b"B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(m2, d2).unwrap();

        // Create a conflict in folder2.
        let (m3, d3) = make_file_data_path(b"C", folder2_id, "other.txt", engine1.device_id());
        engine1.add_file(m3, d3).unwrap();
        let (m4, d4) = make_file_data_path(b"D", folder2_id, "other.txt", engine2.device_id());
        engine2.add_file(m4, d4).unwrap();

        // Sync both ways.
        let delta2 = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta2).unwrap();

        // Total conflicts = 2.
        assert_eq!(engine1.list_conflicts().len(), 2);

        // Filter by folder.
        assert_eq!(engine1.list_conflicts_in_folder(folder_id).len(), 1);
        assert_eq!(engine1.list_conflicts_in_folder(folder2_id).len(), 1);
        assert_eq!(
            engine1.list_conflicts_in_folder(folder_id)[0].path,
            "doc.txt"
        );
        assert_eq!(
            engine1.list_conflicts_in_folder(folder2_id)[0].path,
            "other.txt"
        );
    }

    #[test]
    fn test_same_path_different_folders_no_conflict() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create a second folder.
        let (folder2, _) = engine1.create_folder("folder2").unwrap();
        let folder2_id = folder2.folder_id;
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();
        engine2
            .subscribe_folder(folder2_id, SyncMode::ReadWrite)
            .unwrap();

        // Engine1 adds doc.txt in folder1, engine2 adds doc.txt in folder2.
        let (m1, d1) =
            make_file_data_path(b"folder1 doc", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(m1, d1).unwrap();

        let (m2, d2) =
            make_file_data_path(b"folder2 doc", folder2_id, "doc.txt", engine2.device_id());
        engine2.add_file(m2, d2).unwrap();

        // Sync.
        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // No conflict — different folders.
        assert!(engine1.list_conflicts().is_empty());
    }

    #[test]
    fn test_rebuild_conflicts_restores_on_restart() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create a conflict.
        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();
        assert_eq!(engine1.list_conflicts().len(), 1);

        // Simulate restart: create a new engine from serialised entries.
        let (id3, sk3) = make_keypair();
        let cb3 = Arc::new(TestCallbacks::default());
        let mut engine3 = MurmurEngine::from_dag(Dag::new(id3, sk3), cb3);
        for entry in engine1.all_entries() {
            engine3.load_entry(entry).unwrap();
        }

        // Before rebuild, no conflicts (load_entry doesn't detect them).
        assert!(engine3.list_conflicts().is_empty());

        // After rebuild, the unresolved conflict reappears.
        engine3.rebuild_conflicts();
        assert_eq!(engine3.list_conflicts().len(), 1);
        assert_eq!(engine3.list_conflicts()[0].path, "doc.txt");
        assert_eq!(engine3.list_conflicts()[0].versions.len(), 2);
    }

    #[test]
    fn test_rebuild_conflicts_skips_resolved() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Create and resolve a conflict.
        let (meta1, data1) =
            make_file_data_path(b"version A", folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(b"version B", folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();
        engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        // Simulate restart.
        let (id3, sk3) = make_keypair();
        let cb3 = Arc::new(TestCallbacks::default());
        let mut engine3 = MurmurEngine::from_dag(Dag::new(id3, sk3), cb3);
        for entry in engine1.all_entries() {
            engine3.load_entry(entry).unwrap();
        }
        engine3.rebuild_conflicts();

        // Resolved conflict should NOT reappear.
        assert!(engine3.list_conflicts().is_empty());
    }

    #[test]
    fn test_resolve_conflict_restores_full_metadata() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Two files with DIFFERENT sizes.
        let content_a = b"short";
        let content_b = b"this is a much longer version of the file";

        let (meta1, data1) =
            make_file_data_path(content_a, folder_id, "doc.txt", engine1.device_id());
        let hash_a = meta1.blob_hash;
        let size_a = meta1.size;
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) =
            make_file_data_path(content_b, folder_id, "doc.txt", engine2.device_id());
        engine2.add_file(meta2, data2).unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Before resolution, state.files has engine2's metadata (last applied).
        let file_before = engine1
            .state()
            .files
            .get(&(folder_id, "doc.txt".to_string()))
            .unwrap();
        assert_ne!(file_before.blob_hash, hash_a, "last-applied overwrote hash");

        // Resolve choosing engine1's version (content_a).
        engine1
            .resolve_conflict(folder_id, "doc.txt", hash_a)
            .unwrap();

        let file = engine1
            .state()
            .files
            .get(&(folder_id, "doc.txt".to_string()))
            .expect("file should exist");
        assert_eq!(file.blob_hash, hash_a);
        assert_eq!(file.size, size_a, "size should match chosen version");
        assert_eq!(
            file.device_origin,
            engine1.device_id(),
            "device_origin should match chosen version"
        );
    }

    #[test]
    fn test_resolve_delete_vs_modify_in_favour_of_delete() {
        let (mut engine1, _cb1, mut engine2, _cb2, folder_id) = make_two_engines_with_folder();

        // Add a file and sync.
        let (meta, data) =
            make_file_data_path(b"original", folder_id, "doc.txt", engine1.device_id());
        engine1.add_file(meta, data).unwrap();
        let delta = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta).unwrap();

        // Engine1 modifies, engine2 deletes — concurrently.
        let (new_meta, new_data) =
            make_file_data_path(b"updated", folder_id, "doc.txt", engine1.device_id());
        engine1
            .modify_file(folder_id, "doc.txt", new_meta, new_data)
            .unwrap();
        engine2.delete_file(folder_id, "doc.txt").unwrap();

        let delta = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta).unwrap();

        // Pick the delete version (zeroed hash).
        let delete_hash = BlobHash::from_bytes([0u8; 32]);
        engine1
            .resolve_conflict(folder_id, "doc.txt", delete_hash)
            .unwrap();

        // File should be removed from state.
        assert!(
            engine1
                .state()
                .files
                .get(&(folder_id, "doc.txt".to_string()))
                .is_none()
        );
        assert!(engine1.list_conflicts().is_empty());
    }

    // --- Streaming file add tests (M15) ---

    #[test]
    fn test_add_file_streaming_creates_dag_entry() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"streaming file content").unwrap();

        let entry = engine
            .add_file_streaming(folder_id, "test.txt", &file_path)
            .unwrap();

        assert!(matches!(entry.action, Action::FileAdded { .. }));
        let files = engine.folder_files(folder_id);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "test.txt");
        assert_eq!(files[0].size, 22); // "streaming file content".len()

        // Verify blob hash matches content.
        let expected_hash = BlobHash::from_data(b"streaming file content");
        assert_eq!(files[0].blob_hash, expected_hash);

        // Verify streaming callbacks were invoked.
        let starts = cb.stream_starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, expected_hash);
        assert_eq!(starts[0].1, 22);

        let completes = cb.stream_completes.lock().unwrap();
        assert_eq!(completes.len(), 1);
        assert_eq!(completes[0], expected_hash);
    }

    #[test]
    fn test_add_file_streaming_data_arrives_intact() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();
        let content = b"the quick brown fox jumps over the lazy dog";
        let file_path = dir.path().join("fox.txt");
        std::fs::write(&file_path, content).unwrap();

        engine
            .add_file_streaming(folder_id, "fox.txt", &file_path)
            .unwrap();

        // Verify streamed data matches file content.
        let hash = BlobHash::from_data(content);
        let streams = cb.stream_data.lock().unwrap();
        let received = streams.get(&hash).unwrap();
        assert_eq!(received.as_slice(), content);
    }

    #[test]
    fn test_add_file_streaming_large_file() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();

        // 5 MiB file — exceeds CHUNK_THRESHOLD, crosses multiple 64 KiB read buffers.
        let size = 5 * 1024 * 1024;
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let file_path = dir.path().join("large.bin");
        std::fs::write(&file_path, &content).unwrap();

        let entry = engine
            .add_file_streaming(folder_id, "large.bin", &file_path)
            .unwrap();

        // Verify DAG entry.
        let expected_hash = BlobHash::from_data(&content);
        if let Action::FileAdded { metadata } = &entry.action {
            assert_eq!(metadata.blob_hash, expected_hash);
            assert_eq!(metadata.size, size as u64);
        } else {
            panic!("expected FileAdded action");
        }

        // Verify streamed data matches.
        let streams = cb.stream_data.lock().unwrap();
        let received = streams.get(&expected_hash).unwrap();
        assert_eq!(received.len(), size);
        assert_eq!(received.as_slice(), content.as_slice());

        // Verify progress events were emitted.
        let events = cb.events.lock().unwrap();
        let progress_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, EngineEvent::BlobTransferProgress { .. }))
            .collect();
        assert!(
            !progress_events.is_empty(),
            "should emit progress events for large file"
        );
    }

    #[test]
    fn test_add_file_streaming_rejects_duplicate() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("dup.txt");
        std::fs::write(&file_path, b"content").unwrap();

        engine
            .add_file_streaming(folder_id, "dup.txt", &file_path)
            .unwrap();

        // Second add at same path should fail.
        let result = engine.add_file_streaming(folder_id, "dup.txt", &file_path);
        assert!(matches!(result, Err(EngineError::FileAlreadyExists(_))));
    }

    #[test]
    fn test_add_file_streaming_rejects_read_only() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");

        // Unsubscribe and resubscribe as ReadOnly.
        engine.unsubscribe_folder(folder_id).unwrap();
        engine
            .subscribe_folder(folder_id, SyncMode::ReadOnly)
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("readonly.txt");
        std::fs::write(&file_path, b"content").unwrap();

        let result = engine.add_file_streaming(folder_id, "readonly.txt", &file_path);
        assert!(matches!(result, Err(EngineError::ReadOnlyFolder(_))));
    }

    #[test]
    fn test_add_file_streaming_nonexistent_file() {
        let (mut engine, _cb, folder_id) = make_engine_with_folder("NAS");

        let result =
            engine.add_file_streaming(folder_id, "ghost.txt", std::path::Path::new("/nonexistent"));
        assert!(matches!(result, Err(EngineError::Io(_))));
    }

    #[test]
    fn test_add_file_streaming_nonexistent_folder() {
        let (mut engine, _cb) = make_engine("NAS");
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"content").unwrap();

        let fake_folder = FolderId::from_data(b"nonexistent");
        let result = engine.add_file_streaming(fake_folder, "test.txt", &file_path);
        assert!(matches!(result, Err(EngineError::FolderNotFound(_))));
    }

    #[test]
    fn test_add_file_streaming_emits_file_synced_event() {
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("event.txt");
        std::fs::write(&file_path, b"event content").unwrap();

        engine
            .add_file_streaming(folder_id, "event.txt", &file_path)
            .unwrap();

        let events = cb.events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EngineEvent::FileSynced {
                path,
                ..
            } if path == "event.txt"
        )));
    }

    #[test]
    fn test_add_file_streaming_small_file_no_chunk_overhead() {
        // Small file (<4 MiB) still works correctly via streaming.
        let (mut engine, cb, folder_id) = make_engine_with_folder("NAS");
        let dir = tempfile::tempdir().unwrap();
        let content = b"small";
        let file_path = dir.path().join("small.txt");
        std::fs::write(&file_path, content).unwrap();

        engine
            .add_file_streaming(folder_id, "small.txt", &file_path)
            .unwrap();

        let hash = BlobHash::from_data(content);
        let starts = cb.stream_starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].1, content.len() as u64);

        // Data arrives intact regardless of size.
        let streams = cb.stream_data.lock().unwrap();
        assert_eq!(streams.get(&hash).unwrap().as_slice(), content);
    }
}
