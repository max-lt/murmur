//! The main engine orchestrator.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use murmur_dag::{Dag, DagEntry};
use murmur_types::{AccessGrant, Action, BlobHash, DeviceId, DeviceInfo, DeviceRole, FileMetadata};
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
    // File management
    // -----------------------------------------------------------------

    /// Add a file to the network.
    ///
    /// Creates a `FileAdded` DAG entry. The platform provides the blob hash,
    /// metadata, and data. Returns `Err` if the file already exists (dedup).
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

        // Dedup: skip if already in the DAG.
        if self.dag.state().files.contains_key(&metadata.blob_hash) {
            return Err(EngineError::FileAlreadyExists(
                metadata.blob_hash.to_string(),
            ));
        }

        let blob_hash = metadata.blob_hash;
        let filename = metadata.filename.clone();

        let entry = self.dag.append(Action::FileAdded { metadata });
        self.callbacks.on_dag_entry(entry.to_bytes());
        self.callbacks.on_blob_received(blob_hash, data);

        debug!(%blob_hash, %filename, "engine: file added");
        self.callbacks.on_event(EngineEvent::FileSynced {
            blob_hash,
            filename,
        });

        Ok(entry)
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
            Action::FileAdded { metadata } => {
                self.callbacks.on_event(EngineEvent::FileSynced {
                    blob_hash: metadata.blob_hash,
                    filename: metadata.filename.clone(),
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
    use rand::rngs::OsRng;
    use std::sync::Mutex;

    // --- Test callback implementation ---

    #[derive(Default)]
    struct TestCallbacks {
        dag_entries: Mutex<Vec<Vec<u8>>>,
        blobs: Mutex<Vec<(BlobHash, Vec<u8>)>>,
        events: Mutex<Vec<EngineEvent>>,
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
    }

    fn make_keypair() -> (DeviceId, SigningKey) {
        let sk = SigningKey::generate(&mut OsRng);
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

    fn make_file_data(content: &[u8]) -> (FileMetadata, Vec<u8>) {
        let data = content.to_vec();
        let hash = BlobHash::from_data(&data);
        let meta = FileMetadata {
            blob_hash: hash,
            filename: "test.txt".to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
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

    // --- File management ---

    #[test]
    fn test_add_file() {
        let (mut engine, cb) = make_engine("NAS");
        let (meta, data) = make_file_data(b"hello world");

        engine.add_file(meta.clone(), data).unwrap();

        assert!(engine.state().files.contains_key(&meta.blob_hash));

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
        let (mut engine, _cb) = make_engine("NAS");
        let (meta, data) = make_file_data(b"duplicate");

        engine.add_file(meta.clone(), data.clone()).unwrap();
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::FileAlreadyExists(_))));
    }

    #[test]
    fn test_add_file_bad_hash_rejected() {
        let (mut engine, _cb) = make_engine("NAS");
        let data = b"actual content".to_vec();
        let meta = FileMetadata {
            blob_hash: BlobHash::from_data(b"wrong content"),
            filename: "bad.txt".to_string(),
            size: data.len() as u64,
            mime_type: None,
            created_at: 0,
            device_origin: DeviceId::from_data(b"x"),
        };
        let result = engine.add_file(meta, data);
        assert!(matches!(result, Err(EngineError::BlobIntegrity { .. })));
    }

    #[test]
    fn test_add_file_on_dag_entry_callback() {
        let (mut engine, cb) = make_engine("NAS");
        let initial_count = cb.dag_entries.lock().unwrap().len();
        let (meta, data) = make_file_data(b"content");
        engine.add_file(meta, data).unwrap();
        assert!(cb.dag_entries.lock().unwrap().len() > initial_count);
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
        let (mut engine1, _cb1) = make_engine("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins as a fresh DAG and syncs engine1's entries.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2.clone());
        let pre_entries = engine1.all_entries();
        engine2.receive_sync_entries(pre_entries).unwrap();

        let (meta, data) = make_file_data(b"sync me");
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
        let (mut engine1, _cb1) = make_engine("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins and syncs engine1's entries.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();

        let (meta1, data1) = make_file_data(b"file from NAS");
        engine1.add_file(meta1.clone(), data1).unwrap();

        let (meta2, data2) = make_file_data(b"file from Phone");
        engine2.add_file(meta2.clone(), data2).unwrap();

        // Sync: engine1 → engine2.
        let delta1 = engine1.compute_delta(engine2.tips());
        engine2.receive_sync_entries(delta1).unwrap();

        // Sync: engine2 → engine1.
        let delta2 = engine2.compute_delta(engine1.tips());
        engine1.receive_sync_entries(delta2).unwrap();

        // Both should have both files.
        assert!(engine1.state().files.contains_key(&meta1.blob_hash));
        assert!(engine1.state().files.contains_key(&meta2.blob_hash));
        assert!(engine2.state().files.contains_key(&meta1.blob_hash));
        assert!(engine2.state().files.contains_key(&meta2.blob_hash));
    }

    #[test]
    fn test_full_sync_new_device_gets_everything() {
        let (mut engine1, _cb1) = make_engine("NAS");

        // Add a file and approve a device.
        let (meta, data) = make_file_data(b"important");
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
        assert!(engine2.state().files.contains_key(&meta.blob_hash));
        assert!(engine2.state().devices.contains_key(&peer_id));
    }

    // --- Merge ---

    #[test]
    fn test_maybe_merge() {
        let (mut engine1, _cb1) = make_engine("NAS");
        let (id2, sk2) = make_keypair();
        let cb2 = Arc::new(TestCallbacks::default());

        // Engine1 approves device2.
        engine1.approve_device(id2, DeviceRole::Source).unwrap();

        // Engine2 joins and syncs.
        let mut engine2 = MurmurEngine::from_dag(Dag::new(id2, sk2), cb2);
        engine2.receive_sync_entries(engine1.all_entries()).unwrap();

        let (meta1, data1) = make_file_data(b"a");
        engine1.add_file(meta1, data1).unwrap();

        let (meta2, data2) = make_file_data(b"b");
        engine2.add_file(meta2, data2).unwrap();

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
}
