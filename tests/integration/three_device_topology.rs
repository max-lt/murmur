//! Integration test: Three-device topology scenarios.

#[path = "helpers.rs"]
mod helpers;

use helpers::*;
use murmur_types::{AccessGrant, AccessScope, DeviceRole};

/// Phone (source) → NAS (backup), Tablet requests access from Phone.
#[test]
fn test_three_device_source_backup_tablet() {
    // NAS creates the network.
    let (mut nas, _) = create_engine("NAS", DeviceRole::Backup);

    // Phone joins.
    let (mut phone, _, phone_id) = join_engine("Phone");
    sync_engines(&phone, &mut nas);
    nas.approve_device(phone_id, DeviceRole::Source).unwrap();
    sync_engines(&nas, &mut phone);

    // Tablet joins.
    let (mut tablet, _, tablet_id) = join_engine("Tablet");
    sync_engines(&tablet, &mut nas);
    nas.approve_device(tablet_id, DeviceRole::Source).unwrap();

    // Full sync: NAS → Phone, NAS → Tablet.
    bidirectional_sync(&mut nas, &mut phone);
    bidirectional_sync(&mut nas, &mut tablet);

    // All three see all three devices.
    assert_eq!(nas.list_devices().len(), 3);
    assert_eq!(phone.list_devices().len(), 3);
    assert_eq!(tablet.list_devices().len(), 3);

    // Phone adds a file.
    let (meta, data) = make_file(b"vacation photo", "vacation.jpg", phone_id);
    phone.add_file(meta.clone(), data).unwrap();

    // Sync: Phone → NAS → Tablet.
    sync_engines(&phone, &mut nas);
    sync_engines(&nas, &mut tablet);

    // NAS and Tablet see the file.
    assert!(nas.state().files.contains_key(&meta.blob_hash));
    assert!(tablet.state().files.contains_key(&meta.blob_hash));
}

/// Three devices, star topology: NAS is hub, Phone and Tablet are spokes.
#[test]
fn test_three_device_star_sync() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Full);
    let nas_id = nas.device_id();

    let (mut phone, _, phone_id) = join_engine("Phone");
    sync_engines(&phone, &mut nas);
    nas.approve_device(phone_id, DeviceRole::Source).unwrap();
    bidirectional_sync(&mut nas, &mut phone);

    let (mut tablet, _, tablet_id) = join_engine("Tablet");
    sync_engines(&tablet, &mut nas);
    nas.approve_device(tablet_id, DeviceRole::Source).unwrap();
    bidirectional_sync(&mut nas, &mut tablet);

    // Phone and Tablet add files independently.
    let (meta_phone, data_phone) = make_file(b"phone pic", "phone.jpg", phone_id);
    phone.add_file(meta_phone.clone(), data_phone).unwrap();

    let (meta_tablet, data_tablet) = make_file(b"tablet doc", "notes.txt", tablet_id);
    tablet.add_file(meta_tablet.clone(), data_tablet).unwrap();

    // NAS as hub: collect from both.
    sync_engines(&phone, &mut nas);
    sync_engines(&tablet, &mut nas);

    // NAS has both files.
    assert!(nas.state().files.contains_key(&meta_phone.blob_hash));
    assert!(nas.state().files.contains_key(&meta_tablet.blob_hash));

    // Distribute from NAS to both.
    sync_engines(&nas, &mut phone);
    sync_engines(&nas, &mut tablet);

    // Both have both files.
    assert!(phone.state().files.contains_key(&meta_tablet.blob_hash));
    assert!(tablet.state().files.contains_key(&meta_phone.blob_hash));

    // NAS adds its own file.
    let (meta_nas, data_nas) = make_file(b"nas backup index", "index.json", nas_id);
    nas.add_file(meta_nas.clone(), data_nas).unwrap();

    sync_engines(&nas, &mut phone);
    sync_engines(&nas, &mut tablet);

    // Everyone has 3 files.
    assert_eq!(nas.state().files.len(), 3);
    assert_eq!(phone.state().files.len(), 3);
    assert_eq!(tablet.state().files.len(), 3);
}

/// Access grant flow in three-device topology.
#[test]
fn test_three_device_access_grant() {
    let (mut nas, _) = create_engine("NAS", DeviceRole::Backup);

    let (mut phone, _, phone_id) = join_engine("Phone");
    sync_engines(&phone, &mut nas);
    nas.approve_device(phone_id, DeviceRole::Source).unwrap();
    bidirectional_sync(&mut nas, &mut phone);

    let (mut tablet, _, tablet_id) = join_engine("Tablet");
    sync_engines(&tablet, &mut nas);
    nas.approve_device(tablet_id, DeviceRole::Source).unwrap();
    bidirectional_sync(&mut nas, &mut tablet);
    // Also sync tablet approval to phone.
    bidirectional_sync(&mut nas, &mut phone);

    // Phone grants tablet access to all files.
    let grant = AccessGrant {
        to: tablet_id,
        from: phone_id,
        scope: AccessScope::AllFiles,
        expires_at: u64::MAX,
        signature_r: [0u8; 32],
        signature_s: [0u8; 32],
    };
    phone.grant_access(grant).unwrap();

    // Sync grant to all.
    sync_engines(&phone, &mut nas);
    sync_engines(&nas, &mut tablet);

    // Tablet should see the grant.
    assert!(tablet.has_active_grant(tablet_id, 0));

    // NAS also sees it.
    assert!(nas.has_active_grant(tablet_id, 0));
}
