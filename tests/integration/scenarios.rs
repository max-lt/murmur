//! Integration tests: Various multi-device scenarios.

#[path = "helpers.rs"]
mod helpers;

use helpers::*;
use murmur_types::{AccessScope, DeviceRole};

// =========================================================================
// Offline reconnect
// =========================================================================

/// Device goes offline, adds files, reconnects, syncs.
#[test]
fn test_offline_reconnect() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // NAS creates a folder, sync to phone, phone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    subscribe_test_folder(&mut phone, folder_id);

    // Phone goes "offline" — adds files without syncing.
    for i in 0..5 {
        let (meta, data) = make_file(
            format!("offline photo {i}").as_bytes(),
            &format!("offline_{i}.jpg"),
            phone_id,
            folder_id,
        );
        phone.add_file(meta, data).unwrap();
    }

    // NAS also adds files while Phone is offline.
    let nas_id = nas.device_id();
    for i in 0..3 {
        let (meta, data) = make_file(
            format!("nas file {i}").as_bytes(),
            &format!("nas_{i}.dat"),
            nas_id,
            folder_id,
        );
        nas.add_file(meta, data).unwrap();
    }

    // Phone "reconnects" — bidirectional sync.
    bidirectional_sync(&mut nas, &mut phone);

    // Both should have all 8 files.
    assert_eq!(nas.state().files.len(), 8);
    assert_eq!(phone.state().files.len(), 8);

    // Merge tips.
    nas.maybe_merge();
    phone.maybe_merge();
    bidirectional_sync(&mut nas, &mut phone);

    // Tips should converge.
    assert_eq!(nas.tips(), phone.tips());
}

/// Multiple offline/reconnect cycles.
#[test]
fn test_multiple_offline_cycles() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    let nas_id = nas.device_id();

    // NAS creates a folder, sync to phone, phone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    subscribe_test_folder(&mut phone, folder_id);

    // Cycle 1: offline then sync.
    let (m1, d1) = make_file(b"cycle1-phone", "c1p.txt", phone_id, folder_id);
    phone.add_file(m1, d1).unwrap();
    let (m2, d2) = make_file(b"cycle1-nas", "c1n.txt", nas_id, folder_id);
    nas.add_file(m2, d2).unwrap();
    bidirectional_sync(&mut nas, &mut phone);

    // Cycle 2.
    let (m3, d3) = make_file(b"cycle2-phone", "c2p.txt", phone_id, folder_id);
    phone.add_file(m3, d3).unwrap();
    bidirectional_sync(&mut nas, &mut phone);

    // Cycle 3.
    let (m4, d4) = make_file(b"cycle3-nas-a", "c3na.txt", nas_id, folder_id);
    let (m5, d5) = make_file(b"cycle3-nas-b", "c3nb.txt", nas_id, folder_id);
    nas.add_file(m4, d4).unwrap();
    nas.add_file(m5, d5).unwrap();
    bidirectional_sync(&mut nas, &mut phone);

    assert_eq!(nas.state().files.len(), 5);
    assert_eq!(phone.state().files.len(), 5);
}

// =========================================================================
// Device revocation
// =========================================================================

/// Revoked device's files remain, but device is marked revoked.
#[test]
fn test_device_revocation() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // NAS creates a folder, sync to phone, phone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    subscribe_test_folder(&mut phone, folder_id);

    // Phone adds a file.
    let (meta, data) = make_file(b"pre-revoke photo", "pre.jpg", phone_id, folder_id);
    phone.add_file(meta, data).unwrap();
    sync_engines(&phone, &mut nas);

    // NAS revokes Phone.
    nas.revoke_device(phone_id).unwrap();

    // Sync revocation to Phone.
    sync_engines(&nas, &mut phone);

    // Phone should see itself as revoked.
    let phone_info = phone.state().devices.get(&phone_id).unwrap();
    assert!(!phone_info.approved);

    // The file added before revocation still exists.
    assert!(
        nas.state()
            .files
            .contains_key(&(folder_id, "pre.jpg".to_string()))
    );
}

/// Revoked device's access grants are separate from device revocation.
#[test]
fn test_revoke_device_with_grants() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // NAS grants phone access.
    let grant = make_grant(phone_id, nas.device_id(), AccessScope::AllFiles, u64::MAX);
    nas.grant_access(grant).unwrap();
    sync_engines(&nas, &mut phone);

    assert!(phone.has_active_grant(phone_id, 0));

    // Revoke device — grant still technically in the state
    // (access revocation is a separate action).
    nas.revoke_device(phone_id).unwrap();
    sync_engines(&nas, &mut phone);

    let phone_info = phone.state().devices.get(&phone_id).unwrap();
    assert!(!phone_info.approved);
}

// =========================================================================
// Access grant lifecycle
// =========================================================================

/// Request → grant → use → expiry.
#[test]
fn test_access_grant_lifecycle() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Backup);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // Grant with expiration at timestamp 1000.
    let grant = make_grant(
        phone_id,
        nas.device_id(),
        AccessScope::SingleFile(murmur_types::BlobHash::from_data(b"test")),
        1000,
    );
    nas.grant_access(grant).unwrap();
    sync_engines(&nas, &mut phone);

    // Before expiry: grant is active.
    assert!(phone.has_active_grant(phone_id, 500));

    // At expiry boundary: grant is no longer active.
    assert!(!phone.has_active_grant(phone_id, 1000));

    // After expiry: grant is not active.
    assert!(!phone.has_active_grant(phone_id, 2000));
}

/// Access grant with AllFiles scope.
#[test]
fn test_access_grant_all_files() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut tablet, _, tablet_id) = join_engine("Tablet");
    join_approve_sync(&mut nas, &mut tablet, tablet_id, DeviceRole::Source);

    let grant = make_grant(tablet_id, nas.device_id(), AccessScope::AllFiles, u64::MAX);
    nas.grant_access(grant).unwrap();
    sync_engines(&nas, &mut tablet);

    assert!(tablet.has_active_grant(tablet_id, 0));
    assert!(tablet.has_active_grant(tablet_id, u64::MAX - 1));
}

/// Revoke access explicitly.
#[test]
fn test_access_revoke_explicit() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    let grant = make_grant(phone_id, nas.device_id(), AccessScope::AllFiles, u64::MAX);
    nas.grant_access(grant).unwrap();
    sync_engines(&nas, &mut phone);
    assert!(phone.has_active_grant(phone_id, 0));

    // Revoke.
    nas.revoke_access(phone_id).unwrap();
    sync_engines(&nas, &mut phone);
    assert!(!phone.has_active_grant(phone_id, 0));
}

// =========================================================================
// Large file transfer (integrity)
// =========================================================================

/// Multi-MB file with blake3 integrity verification.
#[test]
fn test_large_file_integrity() {
    let (mut nas, cb_nas) = create_engine("NAS", DeviceRole::Full);
    let nas_id = nas.device_id();

    // NAS creates a folder.
    let folder_id = create_test_folder(&mut nas);

    // Generate a 1MB file.
    let large_data: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();
    let (meta, data) = make_file(&large_data, "large_file.bin", nas_id, folder_id);
    let hash = meta.blob_hash;

    nas.add_file(meta, data).unwrap();

    // Verify the blob was stored via callback.
    let blobs = cb_nas.blobs.lock().unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].0, hash);
    assert_eq!(blobs[0].1.len(), 1_000_000);

    // Verify blake3 hash matches.
    let actual_hash = murmur_types::BlobHash::from_data(&blobs[0].1);
    assert_eq!(actual_hash, hash);
}

/// Bad hash detection.
#[test]
fn test_large_file_bad_hash_rejected() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let nas_id = nas.device_id();

    // NAS creates a folder.
    let folder_id = create_test_folder(&mut nas);

    let data = vec![0u8; 100_000];
    let wrong_hash = murmur_types::BlobHash::from_data(b"wrong content");
    let meta = murmur_types::FileMetadata {
        blob_hash: wrong_hash,
        folder_id,
        path: "bad.bin".to_string(),
        size: data.len() as u64,
        mime_type: None,
        created_at: 0,
        modified_at: 0,
        device_origin: nas_id,
    };

    let result = nas.add_file(meta, data);
    assert!(result.is_err());
}

// =========================================================================
// DAG convergence
// =========================================================================

/// After network partition heals, all devices reach same state.
#[test]
fn test_dag_convergence_after_partition() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    let (mut tablet, _, tablet_id) = join_engine("Tablet");

    // Setup: approve both devices via NAS.
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);
    join_approve_sync(&mut nas, &mut tablet, tablet_id, DeviceRole::Source);
    // Ensure phone sees tablet and vice versa.
    bidirectional_sync(&mut nas, &mut phone);

    let nas_id = nas.device_id();

    // NAS creates a folder, sync to all, everyone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    sync_engines(&nas, &mut tablet);
    subscribe_test_folder(&mut phone, folder_id);
    subscribe_test_folder(&mut tablet, folder_id);

    // Network partition: Phone and Tablet can't see each other or NAS.
    // Each adds files independently.
    let (mp, dp) = make_file(
        b"partition phone file",
        "part_phone.jpg",
        phone_id,
        folder_id,
    );
    phone.add_file(mp, dp).unwrap();

    let (mt, dt) = make_file(
        b"partition tablet file",
        "part_tablet.pdf",
        tablet_id,
        folder_id,
    );
    tablet.add_file(mt, dt).unwrap();

    let (mn, dn) = make_file(b"partition nas file", "part_nas.log", nas_id, folder_id);
    nas.add_file(mn, dn).unwrap();

    // Partition heals: full mesh sync.
    bidirectional_sync(&mut nas, &mut phone);
    bidirectional_sync(&mut nas, &mut tablet);
    bidirectional_sync(&mut phone, &mut tablet);

    // All merge.
    nas.maybe_merge();
    phone.maybe_merge();
    tablet.maybe_merge();
    bidirectional_sync(&mut nas, &mut phone);
    bidirectional_sync(&mut nas, &mut tablet);
    bidirectional_sync(&mut phone, &mut tablet);

    // All devices should have all 3 files.
    assert_eq!(nas.state().files.len(), 3);
    assert_eq!(phone.state().files.len(), 3);
    assert_eq!(tablet.state().files.len(), 3);

    // All should have converged to same tips.
    assert_eq!(nas.tips(), phone.tips());
    assert_eq!(phone.tips(), tablet.tips());

    // Same devices.
    assert_eq!(nas.list_devices().len(), 3);
    assert_eq!(phone.list_devices().len(), 3);
    assert_eq!(tablet.list_devices().len(), 3);
}

/// DAG convergence with many entries from different devices.
#[test]
fn test_dag_convergence_many_entries() {
    let (mut a, _) = create_engine("A", DeviceRole::Full);
    let (mut b, _, id_b) = join_engine("B");
    let id_a = a.device_id();

    // A approves B and they sync.
    join_approve_sync(&mut a, &mut b, id_b, DeviceRole::Full);

    // A creates a folder, sync to B, B subscribes.
    let folder_id = create_test_folder(&mut a);
    sync_engines(&a, &mut b);
    subscribe_test_folder(&mut b, folder_id);

    // Each adds 10 files independently.
    for i in 0..10 {
        let (meta, data) = make_file(
            format!("a-file-{i}").as_bytes(),
            &format!("a_{i}.txt"),
            id_a,
            folder_id,
        );
        a.add_file(meta, data).unwrap();
    }
    for i in 0..10 {
        let (meta, data) = make_file(
            format!("b-file-{i}").as_bytes(),
            &format!("b_{i}.txt"),
            id_b,
            folder_id,
        );
        b.add_file(meta, data).unwrap();
    }

    // Sync and merge.
    bidirectional_sync(&mut a, &mut b);
    a.maybe_merge();
    b.maybe_merge();
    bidirectional_sync(&mut a, &mut b);

    assert_eq!(a.state().files.len(), 20);
    assert_eq!(b.state().files.len(), 20);
    assert_eq!(a.tips(), b.tips());
}

// =========================================================================
// File addition and sync (engine doesn't expose delete_file yet)
// =========================================================================

/// File added on one device, synced, verified on the other.
#[test]
fn test_file_add_and_sync() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // NAS creates a folder, sync to phone, phone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    subscribe_test_folder(&mut phone, folder_id);

    // Phone adds a file.
    let (meta, data) = make_file(b"temp file", "temp.txt", phone_id, folder_id);
    phone.add_file(meta, data).unwrap();
    sync_engines(&phone, &mut nas);

    // NAS sees the file.
    let file_key = (folder_id, "temp.txt".to_string());
    assert!(nas.state().files.contains_key(&file_key));

    // Verify file metadata matches.
    let nas_meta = nas.state().files.get(&file_key).unwrap();
    assert_eq!(nas_meta.path, "temp.txt");
    assert_eq!(nas_meta.device_origin, phone_id);
}

// =========================================================================
// New device full sync
// =========================================================================

/// A new device joining late gets the complete history.
#[test]
fn test_new_device_gets_full_history() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let nas_id = nas.device_id();

    // NAS creates a folder.
    let folder_id = create_test_folder(&mut nas);

    // NAS adds several files over time.
    for i in 0..5 {
        let (meta, data) = make_file(
            format!("history file {i}").as_bytes(),
            &format!("hist_{i}.dat"),
            nas_id,
            folder_id,
        );
        nas.add_file(meta, data).unwrap();
    }

    // New phone joins late.
    let (mut phone, _, phone_id) = join_engine("Phone");
    sync_engines(&phone, &mut nas);
    nas.approve_device(phone_id, DeviceRole::Source).unwrap();

    // Full sync: NAS → Phone.
    sync_engines(&nas, &mut phone);

    // Phone should have all 5 files.
    assert_eq!(phone.state().files.len(), 5);

    // Phone should see 2 devices.
    assert_eq!(phone.list_devices().len(), 2);
}

/// Multiple new devices join sequentially.
#[test]
fn test_sequential_device_joins() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);

    let mut devices = vec![];
    for i in 0..4 {
        let (mut dev, _, dev_id) = join_engine(&format!("Device{i}"));
        join_approve_sync(&mut nas, &mut dev, dev_id, DeviceRole::Source);
        devices.push((dev, dev_id));
    }

    // Final sync round: NAS has all entries, push to each device.
    for (dev, _) in &mut devices {
        sync_engines(&nas, dev);
    }

    // All should see 5 devices (NAS + 4 others).
    assert_eq!(nas.list_devices().len(), 5);
    for (dev, _) in &devices {
        assert_eq!(dev.list_devices().len(), 5);
    }
}

// =========================================================================
// Callbacks verification
// =========================================================================

/// Verify all platform callbacks are triggered correctly.
#[test]
fn test_callbacks_triggered_on_sync() {
    let (mut nas, cb_nas) = create_engine("NAS", DeviceRole::Full);
    let (mut phone, _, phone_id) = join_engine("Phone");
    join_approve_sync(&mut nas, &mut phone, phone_id, DeviceRole::Source);

    // NAS creates a folder, sync to phone, phone subscribes.
    let folder_id = create_test_folder(&mut nas);
    sync_engines(&nas, &mut phone);
    subscribe_test_folder(&mut phone, folder_id);

    // Phone adds a file.
    let (meta, data) = make_file(b"callback test file", "cb.txt", phone_id, folder_id);
    phone.add_file(meta, data).unwrap();

    // Clear NAS callbacks to measure just the sync.
    cb_nas.entries.lock().unwrap().clear();
    cb_nas.events.lock().unwrap().clear();

    // Sync Phone → NAS.
    sync_engines(&phone, &mut nas);

    // NAS should have received DAG entries via callback.
    let entries = cb_nas.entries.lock().unwrap();
    assert!(!entries.is_empty());

    // NAS should have received events.
    let events = cb_nas.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, murmur_engine::EngineEvent::FileSynced { .. }))
    );
}
