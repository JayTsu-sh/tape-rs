//! 默认仅使用模拟 transport，不接触物理设备。
#[path = "support/library_fixture.rs"]
mod support;
use support::Fixture;

#[test]
fn drive_roles_are_canonical_validated_and_persisted() {
    let f = Fixture::new();
    let mut lib = f.open();
    assert_eq!(lib.drive_role("SIMDRV0000").unwrap(), "mrg");
    lib.drive_set_role("SIMDRV0000", "gr").unwrap();
    assert_eq!(lib.drive_role("SIMDRV0000").unwrap(), "rg");
    for role in ["", "rr", "mmg", "M", "read", "m r", "r\0"] {
        assert!(lib.drive_set_role("SIMDRV0000", role).is_err(), "{role:?}");
        assert_eq!(lib.drive_role("SIMDRV0000").unwrap(), "rg");
    }
    assert!(lib.drive_set_role("unknown", "m").is_err());
    assert!(lib.drive_role("unknown").is_err());
    assert_eq!(lib.status().unwrap()["drives"][0]["role"], "rg");
    let cached = tape_rs::library::cached_status_if_active(&f.path)
        .unwrap()
        .unwrap();
    assert_eq!(cached["drives"][0]["role"], "rg");
    drop(lib);
    assert_eq!(f.open().drive_role("SIMDRV0000").unwrap(), "rg");
}

#[test]
fn scheduler_predicate_enforces_each_role_and_enabled_state() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.pool_create("pool").unwrap();
    for (roles, expected) in [
        ("m", [true, false, false]),
        ("r", [false, true, false]),
        ("g", [false, false, true]),
        ("rm", [true, true, false]),
        ("gm", [true, false, true]),
        ("gr", [false, true, true]),
        ("grm", [true, true, true]),
    ] {
        lib.drive_set_role("SIMDRV0000", roles).unwrap();
        for (role, allowed) in ['m', 'r', 'g'].into_iter().zip(expected) {
            assert_eq!(lib.drive_eligible(0, "pool", role).unwrap(), allowed);
        }
    }
    lib.drive_set_enabled("SIMDRV0000", false).unwrap();
    assert!(!lib.drive_eligible(0, "pool", 'r').unwrap());
    lib.drive_set_enabled("SIMDRV0000", true).unwrap();
    assert!(lib.drive_eligible(0, "pool", 'r').unwrap());
    assert!(lib.drive_eligible(1, "pool", 'r').is_err());
    assert!(lib.drive_eligible(0, "missing", 'r').is_err());
    assert!(lib.drive_eligible(0, "pool", 'x').is_err());
    assert!(f.sim.loaded_barcode(0).is_none());
}

#[test]
fn new_library_schema_has_no_scheduling_groups() {
    let f = Fixture::new();
    let lib = f.open();
    let db = rusqlite::Connection::open(&f.path).unwrap();
    let group_tables: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name LIKE '%group%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(group_tables, 0);
    let local_node_tables: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='library_local_node'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local_node_tables, 0);
    let status = lib.status().unwrap();
    assert!(status.get("nodegroups").is_none());
    assert!(status.get("local_nodegroup").is_none());
    assert!(status["drives"][0].get("nodegroup_name").is_none());
    assert_eq!(status["drives"][0]["enabled"], true);
    assert_eq!(status["drives"][0]["role"], "mrg");
}

#[test]
fn all_library_pools_share_enabled_drives_with_matching_roles() {
    let f = Fixture::with_drive_count(2);
    let mut lib = f.open();
    lib.pool_create("first").unwrap();
    lib.pool_create("second").unwrap();
    lib.drive_set_role("SIMDRV0000", "r").unwrap();
    lib.drive_set_role("SIMDRV0001", "mg").unwrap();
    for pool in ["first", "second"] {
        assert!(lib.drive_eligible(0, pool, 'r').unwrap());
        assert!(!lib.drive_eligible(0, pool, 'm').unwrap());
        assert!(!lib.drive_eligible(1, pool, 'r').unwrap());
        assert!(lib.drive_eligible(1, pool, 'm').unwrap());
        assert!(lib.drive_eligible(1, pool, 'g').unwrap());
    }
    lib.drive_set_enabled("SIMDRV0000", false).unwrap();
    drop(lib);
    let lib = f.open();
    for pool in ["first", "second"] {
        assert!(!lib.drive_eligible(0, pool, 'r').unwrap());
        assert!(lib.drive_eligible(1, pool, 'm').unwrap());
    }
}

#[test]
fn administrative_policy_changes_refuse_foreign_loaded_drives_without_side_effects() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    f.sim.load_into_drive("LIB001L8", 0).unwrap();
    let changer_log = f.changer.command_log().len();
    let drive_log = f.drive.command_log().len();
    assert!(lib.drive_set_role("SIMDRV0000", "r").is_err());
    assert!(lib.drive_set_enabled("SIMDRV0000", true).is_err());
    assert_eq!(lib.drive_role("SIMDRV0000").unwrap(), "mrg");
    assert_eq!(lib.pool_show("pool").unwrap()["explicit"], false);
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    assert!(
        f.changer.command_log()[changer_log..]
            .iter()
            .all(|c| [0x12, 0x5a, 0xb8].contains(c))
    );
    assert!(
        f.drive.command_log()[drive_log..]
            .iter()
            .all(|c| [0x12, 0x5e].contains(c))
    );
}

#[test]
fn policy_changes_refuse_a_foreign_reservation_even_when_the_drive_is_empty() {
    use tape_rs::scsi::reservation::{ReservationKey, fence, read_status};
    let f = Fixture::new();
    let mut lib = f.open();
    let foreign = f.sim.drive_as(0, 99);
    fence(&foreign, ReservationKey::new(2, 200)).unwrap();
    let before = read_status(&foreign).unwrap();
    assert!(lib.drive_set_role("SIMDRV0000", "r").is_err());
    assert!(lib.drive_set_enabled("SIMDRV0000", false).is_err());
    assert_eq!(read_status(&foreign).unwrap(), before);
    assert_eq!(lib.drive_role("SIMDRV0000").unwrap(), "mrg");
    assert!(f.sim.loaded_barcode(0).is_none());
}

#[test]
fn owned_drive_policy_changes_preserve_data_and_apply_to_actual_reads() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.write_from("/pool/LIB001L8/file", &mut &b"keep data"[..], true)
        .unwrap();
    let changer_log = f.changer.command_log().len();
    let drive_log = f.drive.command_log().len();
    lib.drive_set_role("SIMDRV0000", "r").unwrap();
    assert!(
        f.changer.command_log()[changer_log..]
            .iter()
            .all(|c| [0x12, 0x5a, 0xb8].contains(c))
    );
    assert!(
        f.drive.command_log()[drive_log..]
            .iter()
            .all(|c| [0x12, 0x5e].contains(c))
    );
    let mut bytes = Vec::new();
    lib.read_to("/pool/LIB001L8/file", &mut bytes).unwrap();
    assert_eq!(bytes, b"keep data");
    assert!(
        lib.write_from("/pool/LIB002L8/no", &mut &b"no"[..], true)
            .is_err()
    );
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    lib.close().unwrap();
}

#[test]
fn cached_drive_policy_is_current_and_requires_no_transport_calls() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.drive_set_role("SIMDRV0000", "g").unwrap();
    lib.drive_set_enabled("SIMDRV0000", false).unwrap();
    let changer_log = f.changer.command_log();
    let drive_log = f.drive.command_log();
    let cached = tape_rs::library::cached_status_if_active(&f.path)
        .unwrap()
        .unwrap();
    assert!(cached.get("nodegroups").is_none());
    assert!(cached.get("local_nodegroup").is_none());
    assert!(cached["drives"][0].get("nodegroup_name").is_none());
    assert_eq!(cached["drives"][0]["role"], "g");
    assert_eq!(cached["drives"][0]["enabled"], false);
    assert_eq!(f.changer.command_log(), changer_log);
    assert_eq!(f.drive.command_log(), drive_log);
}

#[test]
fn offline_is_persistent_and_does_not_move_a_shelved_tape() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    let log = f.changer.command_log();
    assert!(lib.tape_offline("absent", "test").is_err());
    assert!(lib.tape_offline("LIB001L8", "").is_err());
    lib.tape_offline("LIB001L8", "test room A").unwrap();
    // 允许状态查询，但没有 PR OUT / MOVE 等有状态命令。
    assert!(
        f.changer.command_log()[log.len()..]
            .iter()
            .all(|c| *c != 0x5f && *c != 0xa5)
    );
    assert!(
        lib.read_to("/pool/LIB001L8/file", &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("OFFLINE")
    );
    let cached = tape_rs::library::cached_status_if_active(&f.path)
        .unwrap()
        .unwrap();
    assert_eq!(cached["volumes"][0]["state"], "Offline");
    assert_eq!(cached["volumes"][0]["offline_msg"], "test room A");
    drop(lib);
    let mut lib = f.open();
    assert_eq!(lib.status().unwrap()["volumes"][0]["state"], "Offline");
    // 空白带不能靠 online 清除验证失败：必须保留离线标记。
    assert!(lib.tape_online("LIB001L8").is_err());
    assert_eq!(lib.status().unwrap()["volumes"][0]["state"], "Offline");
    lib.close().unwrap();
    lib.unassign("LIB001L8").unwrap();
    assert!(lib.unassign("LIB001L8").is_err());
    lib.assign("pool", "LIB001L8").unwrap();
    assert!(lib.status().unwrap()["volumes"][0]["offline_msg"].is_null());
}

#[test]
fn online_validates_ltfs_and_drive_down_is_enforced_by_scheduler() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.write_from("/pool/LIB001L8/file", &mut &b"payload"[..], true)
        .unwrap();
    lib.tape_offline("LIB001L8", "test").unwrap();
    assert!(f.sim.loaded_barcode(0).is_none());
    lib.tape_online("LIB001L8").unwrap();
    assert!(lib.status().unwrap()["volumes"][0]["offline_msg"].is_null());
    lib.drive_set_enabled("SIMDRV0000", false).unwrap();
    assert!(f.sim.loaded_barcode(0).is_none());
    let before = f.changer.command_log();
    assert!(lib.read_to("/pool/LIB001L8/file", &mut Vec::new()).is_err());
    assert!(
        f.changer.command_log()[before.len()..]
            .iter()
            .all(|c| *c != 0xa5)
    );
    assert_eq!(lib.status().unwrap()["drives"][0]["enabled"], false);
    let cached = tape_rs::library::cached_status_if_active(&f.path)
        .unwrap()
        .unwrap();
    assert_eq!(cached["drives"][0]["enabled"], false);
    lib.close().unwrap();
    drop(lib);
    let mut lib = f.open();
    assert_eq!(lib.status().unwrap()["drives"][0]["enabled"], false);
    assert!(lib.drive_set_enabled("unknown", true).is_err());
    lib.drive_set_enabled("SIMDRV0000", true).unwrap();
    let mut output = Vec::new();
    lib.read_to("/pool/LIB001L8/file", &mut output).unwrap();
    assert_eq!(output, b"payload");
    lib.close().unwrap();
}

#[test]
fn administrative_offline_and_down_never_preempt_a_foreign_loaded_tape() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    f.sim.load_into_drive("LIB001L8", 0).unwrap();
    assert!(lib.drive_set_enabled("SIMDRV0000", false).is_err());
    assert!(lib.tape_offline("LIB001L8", "test").is_err());
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    lib.close().unwrap();
}

#[test]
fn mount_limit_returns_owned_tape_instead_of_using_another_free_drive() {
    let f = Fixture::with_drive_count(2);
    let mut lib = f.open();
    lib.pool_create("pool").unwrap();
    lib.pool_set("pool", "mountlimit", "1").unwrap();
    for barcode in ["LIB001L8", "LIB002L8"] {
        lib.assign("pool", barcode).unwrap();
        assert!(
            lib.inspect_volume(&format!("/pool/{barcode}"), |_, _| Ok(()))
                .is_err()
        );
        // mountlimit=1 即使另一个驱动空闲，也应先归槽旧卷并复用第一个。
        assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some(barcode));
        assert!(f.sim.loaded_barcode(1).is_none());
        tape_rs::ltfs::mkltfs::mkltfs(
            f.drive.as_ref(),
            &tape_rs::ltfs::mkltfs::MkltfsOptions {
                volume_id: barcode[..6].into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
    }
    lib.close().unwrap();
}

#[test]
fn mount_limit_never_evicts_foreign_loaded_pool_members() {
    let f = Fixture::with_drive_count(2);
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    lib.assign("pool", "LIB002L8").unwrap();
    lib.pool_set("pool", "mountlimit", "1").unwrap();
    f.sim.load_into_drive("LIB001L8", 0).unwrap();
    let before = f.changer.command_log();
    let error = lib
        .inspect_volume("/pool/LIB002L8", |_, _| Ok(()))
        .unwrap_err();
    assert!(error.to_string().contains("挂载上限"));
    assert!(
        f.changer.command_log()[before.len()..]
            .iter()
            .all(|c| *c != 0xa5 && *c != 0x5f)
    );
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    lib.close().unwrap();
}

#[test]
fn ordinary_append_preserves_existing_ee_objects_and_refuses_their_overwrite() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.with_volume("/pool/LIB001L8", |volume, _| {
        volume.append_file(".LTFSEE_DATA/object", &mut &b"ee data"[..])?;
        volume.set_node_xattr(
            ".LTFSEE_DATA/object",
            tape_rs::ltfs::index::Xattr {
                key: "ibm.ltfsee.gpfs.objtype".into(),
                value: "regfile".into(),
                base64: false,
            },
            false,
            false,
        )?;
        volume.add_symlink("gpfs/file", "../.LTFSEE_DATA/object")
    })
    .unwrap();
    let before = lib
        .inspect_volume("/pool/LIB001L8", |volume, _| volume.index().to_xml())
        .unwrap();
    for path in [".LTFSEE_DATA/object", ".LTFSEE_DATA/new", "gpfs/file"] {
        let error = lib
            .write_from(&format!("/pool/LIB001L8/{path}"), &mut &b"bad"[..], false)
            .unwrap_err();
        assert!(error.to_string().contains("EE"));
    }
    let after = lib
        .inspect_volume("/pool/LIB001L8", |volume, _| volume.index().to_xml())
        .unwrap();
    assert_eq!(before, after);
    lib.write_from("/pool/LIB001L8/ordinary", &mut &b"new"[..], true)
        .unwrap();
    let mut readback = Vec::new();
    lib.read_to("/pool/LIB001L8/gpfs/file", &mut readback)
        .unwrap();
    assert_eq!(readback, b"ee data");
    lib.inspect_volume("/pool/LIB001L8", |volume, _| {
        let file = volume.index().find_file(".LTFSEE_DATA/object").unwrap();
        assert_eq!(
            file.xattrs
                .iter()
                .find(|x| x.key == "ibm.ltfsee.gpfs.objtype")
                .unwrap()
                .value,
            "regfile"
        );
        Ok(())
    })
    .unwrap();
    lib.close().unwrap();
}

#[test]
fn mount_limit_does_not_unload_current_tape_for_a_misplaced_target() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.inspect_volume("/pool/LIB001L8", |_, _| Ok(())).unwrap();
    lib.pool_set("pool", "mountlimit", "1").unwrap();
    let mut mover = tape_rs::changer::commands::MediumChanger::new(f.changer.as_ref());
    let map = mover.load_address_map().unwrap().clone();
    mover
        .move_medium(map.storage_start + 1, map.storage_start + 2)
        .unwrap();
    assert!(lib.inspect_volume("/pool/LIB002L8", |_, _| Ok(())).is_err());
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    lib.close().unwrap();
}
