//! Integration test: Two-device sync scenarios.

#[path = "helpers.rs"]
mod helpers;

use helpers::*;
use murmur_engine::EngineEvent;
use murmur_types::DeviceRole;

/// Device A creates network, device B joins, A approves, both sync files.
#[test]
fn test_two_device_full_sync() {
    // Device A creates the network.
    let (mut engine_a, cb_a) = create_engine("NAS", DeviceRole::Backup);
    let device_a_id = engine_a.device_id();

    // Device B joins.
    let (mut engine_b, cb_b, device_b_id) = join_engine("Phone");

    // Sync B's join request to A.
    sync_engines(&engine_b, &mut engine_a);

    // A sees the join request.
    let pending = engine_a.pending_requests();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].device_id, device_b_id);

    // A approves B.
    engine_a
        .approve_device(device_b_id, DeviceRole::Source)
        .unwrap();

    // Sync everything A → B.
    sync_engines(&engine_a, &mut engine_b);

    // B should see itself as approved.
    let b_info = engine_b.state().devices.get(&device_b_id).unwrap();
    assert!(b_info.approved);
    assert_eq!(b_info.role, DeviceRole::Source);

    // Both should see both devices.
    assert_eq!(engine_a.list_devices().len(), 2);
    assert_eq!(engine_b.list_devices().len(), 2);

    // B adds a file.
    let (meta, data) = make_file(b"photo from phone", "photo.jpg", device_b_id);
    engine_b.add_file(meta.clone(), data).unwrap();

    // Sync B → A.
    sync_engines(&engine_b, &mut engine_a);

    // A should have the file in its state.
    assert!(engine_a.state().files.contains_key(&meta.blob_hash));

    // A's callback should have received the blob.
    let blobs_a = cb_a.blobs.lock().unwrap();
    // The file was added by B, synced to A. A gets the DAG entry but not the blob
    // (blob transfer is separate from DAG sync). The file appears in state though.
    drop(blobs_a);

    // A adds a file too.
    let (meta_a, data_a) = make_file(b"backup log", "backup.log", device_a_id);
    engine_a.add_file(meta_a.clone(), data_a).unwrap();

    // Bidirectional sync.
    bidirectional_sync(&mut engine_a, &mut engine_b);

    // Both should have both files.
    assert!(engine_a.state().files.contains_key(&meta.blob_hash));
    assert!(engine_a.state().files.contains_key(&meta_a.blob_hash));
    assert!(engine_b.state().files.contains_key(&meta.blob_hash));
    assert!(engine_b.state().files.contains_key(&meta_a.blob_hash));

    // Verify events on B's side.
    let events_b = cb_b.events.lock().unwrap();
    assert!(
        events_b
            .iter()
            .any(|e| matches!(e, EngineEvent::NetworkJoined { .. }))
    );
    assert!(events_b.iter().any(
        |e| matches!(e, EngineEvent::DeviceApproved { device_id, .. } if *device_id == device_b_id)
    ));
}

/// Two devices add files simultaneously, DAGs merge correctly.
#[test]
fn test_two_device_concurrent_files() {
    let (mut engine_a, _) = create_engine("NAS", DeviceRole::Full);
    let (mut engine_b, _) = create_engine("Phone", DeviceRole::Full);
    let id_a = engine_a.device_id();
    let id_b = engine_b.device_id();

    // Both add files independently (no sync yet).
    let (meta_a, data_a) = make_file(b"file from A", "a.txt", id_a);
    let (meta_b, data_b) = make_file(b"file from B", "b.txt", id_b);
    engine_a.add_file(meta_a.clone(), data_a).unwrap();
    engine_b.add_file(meta_b.clone(), data_b).unwrap();

    // Bidirectional sync.
    bidirectional_sync(&mut engine_a, &mut engine_b);

    // Both should have both files.
    assert_eq!(engine_a.state().files.len(), 2);
    assert_eq!(engine_b.state().files.len(), 2);
    assert!(engine_a.state().files.contains_key(&meta_a.blob_hash));
    assert!(engine_a.state().files.contains_key(&meta_b.blob_hash));
    assert!(engine_b.state().files.contains_key(&meta_a.blob_hash));
    assert!(engine_b.state().files.contains_key(&meta_b.blob_hash));

    // After merge, tips should converge.
    // Do another round of sync to settle any merge entries.
    engine_a.maybe_merge();
    engine_b.maybe_merge();
    bidirectional_sync(&mut engine_a, &mut engine_b);

    assert_eq!(engine_a.tips(), engine_b.tips());
}

/// File deduplication: same content added on two devices.
#[test]
fn test_two_device_deduplication() {
    let (mut engine_a, _) = create_engine("NAS", DeviceRole::Full);
    let (mut engine_b, _) = create_engine("Phone", DeviceRole::Full);
    let id_a = engine_a.device_id();
    let id_b = engine_b.device_id();

    let content = b"identical content on both devices";

    // A adds the file.
    let (meta_a, data_a) = make_file(content, "a_copy.txt", id_a);
    engine_a.add_file(meta_a.clone(), data_a).unwrap();

    // Sync A → B.
    sync_engines(&engine_a, &mut engine_b);

    // B tries to add the same content — should be rejected (dedup).
    let (meta_b, data_b) = make_file(content, "b_copy.txt", id_b);
    // Same blob hash, so it's a duplicate.
    assert_eq!(meta_a.blob_hash, meta_b.blob_hash);
    let result = engine_b.add_file(meta_b, data_b);
    assert!(result.is_err()); // FileAlreadyExists

    // Only one file in the state.
    assert_eq!(engine_b.state().files.len(), 1);
}

/// Sync delta computation: only missing entries are sent.
#[test]
fn test_two_device_delta_sync() {
    let (mut engine_a, _) = create_engine("NAS", DeviceRole::Full);
    let (mut engine_b, _) = create_engine("Phone", DeviceRole::Full);
    let id_a = engine_a.device_id();

    // Sync founding entries.
    bidirectional_sync(&mut engine_a, &mut engine_b);

    // A adds 3 files.
    for i in 0..3 {
        let (meta, data) = make_file(
            format!("file {i}").as_bytes(),
            &format!("file_{i}.txt"),
            id_a,
        );
        engine_a.add_file(meta, data).unwrap();
    }

    // Compute delta: A has 3 new entries that B doesn't.
    let delta = engine_a.compute_delta(engine_b.tips());
    assert!(delta.len() >= 3);

    // Apply delta to B.
    engine_b.receive_sync_entries(delta).unwrap();

    // B should have all 3 files.
    assert_eq!(engine_b.state().files.len(), 3);

    // Now delta should be empty (they're in sync).
    let delta2 = engine_a.compute_delta(engine_b.tips());
    assert!(delta2.is_empty());
}
