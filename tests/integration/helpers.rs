//! Shared test helpers for integration tests.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use murmur_engine::{EngineEvent, MurmurEngine, PlatformCallbacks};
use murmur_types::{AccessGrant, AccessScope, BlobHash, DeviceId, DeviceRole, FileMetadata};

/// Test platform callbacks that capture all events, entries, and blobs.
#[derive(Default)]
pub struct TestCallbacks {
    pub events: Mutex<Vec<EngineEvent>>,
    pub entries: Mutex<Vec<Vec<u8>>>,
    pub blobs: Mutex<Vec<(BlobHash, Vec<u8>)>>,
}

impl PlatformCallbacks for TestCallbacks {
    fn on_dag_entry(&self, entry_bytes: Vec<u8>) {
        self.entries.lock().unwrap().push(entry_bytes);
    }

    fn on_blob_received(&self, blob_hash: BlobHash, data: Vec<u8>) {
        self.blobs.lock().unwrap().push((blob_hash, data));
    }

    fn on_blob_needed(&self, blob_hash: BlobHash) -> Option<Vec<u8>> {
        let blobs = self.blobs.lock().unwrap();
        blobs
            .iter()
            .find(|(h, _)| *h == blob_hash)
            .map(|(_, d)| d.clone())
    }

    fn on_event(&self, event: EngineEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Create a new engine as the first device in a network.
pub fn create_engine(name: &str, role: DeviceRole) -> (MurmurEngine, Arc<TestCallbacks>) {
    let sk = SigningKey::from_bytes(&rand::random());
    let device_id = DeviceId::from_verifying_key(&sk.verifying_key());
    let cb = Arc::new(TestCallbacks::default());
    let engine = MurmurEngine::create_network(device_id, sk, name.to_string(), role, cb.clone());
    (engine, cb)
}

/// Create a new engine that joins an existing network (pending approval).
pub fn join_engine(name: &str) -> (MurmurEngine, Arc<TestCallbacks>, DeviceId) {
    let sk = SigningKey::from_bytes(&rand::random());
    let device_id = DeviceId::from_verifying_key(&sk.verifying_key());
    let cb = Arc::new(TestCallbacks::default());
    let engine = MurmurEngine::join_network(device_id, sk, name.to_string(), cb.clone());
    (engine, cb, device_id)
}

/// Simulate a full sync from `src` to `dst`.
pub fn sync_engines(src: &MurmurEngine, dst: &mut MurmurEngine) {
    let delta = src.compute_delta(dst.tips());
    if !delta.is_empty() {
        dst.receive_sync_entries(delta).unwrap();
    }
}

/// Bidirectional sync between two engines.
pub fn bidirectional_sync(a: &mut MurmurEngine, b: &mut MurmurEngine) {
    // a → b
    let delta_a = a.compute_delta(b.tips());
    if !delta_a.is_empty() {
        b.receive_sync_entries(delta_a).unwrap();
    }
    // b → a
    let delta_b = b.compute_delta(a.tips());
    if !delta_b.is_empty() {
        a.receive_sync_entries(delta_b).unwrap();
    }
}

/// Join a device to an existing network: sync join request, approve, sync back.
pub fn join_approve_sync(
    approver: &mut MurmurEngine,
    joiner: &mut MurmurEngine,
    joiner_id: DeviceId,
    role: DeviceRole,
) {
    sync_engines(joiner, approver);
    approver.approve_device(joiner_id, role).unwrap();
    bidirectional_sync(approver, joiner);
}

/// Create an `AccessGrant` with zeroed (dummy) signatures for testing.
pub fn make_grant(
    to: DeviceId,
    from: DeviceId,
    scope: AccessScope,
    expires_at: u64,
) -> AccessGrant {
    AccessGrant {
        to,
        from,
        scope,
        expires_at,
        signature_r: [0u8; 32],
        signature_s: [0u8; 32],
    }
}

/// Create test file metadata and data.
pub fn make_file(content: &[u8], filename: &str, origin: DeviceId) -> (FileMetadata, Vec<u8>) {
    let data = content.to_vec();
    let blob_hash = BlobHash::from_data(&data);
    let meta = FileMetadata {
        blob_hash,
        filename: filename.to_string(),
        size: data.len() as u64,
        mime_type: None,
        created_at: 0,
        device_origin: origin,
    };
    (meta, data)
}
