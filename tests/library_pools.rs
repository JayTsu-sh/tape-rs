//! 池管理只访问临时 SQLite 和 SimTransport，不连接真实设备。
use rusqlite::{Connection, params};
use serde_json::json;
use tape_rs::scsi::sim::SimCartridge;

#[allow(dead_code)]
#[path = "support/library_fixture.rs"]
mod support;
use support::Fixture;

#[test]
fn capacity_warnings_use_complete_observations_and_persist_configuration() {
    const TIB: u64 = 1 << 40;
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.pool_create("pool").unwrap();
    let shown = lib.pool_show("pool").unwrap();
    assert_eq!(shown["low_space_warning_enable"], true);
    assert_eq!(shown["low_space_warning_threshold"], 0);
    assert_eq!(shown["no_space_warning_enable"], true);
    assert!(shown["low_space_warning"].is_null());
    assert!(shown["no_space_warning"].is_null());
    lib.pool_set("pool", "lowspacewarningthreshold", "2")
        .unwrap();
    for (attr, value) in [
        ("lowspacewarningenable", "true"),
        ("nospacewarningenable", "YES"),
        ("lowspacewarningthreshold", "-1"),
        ("lowspacewarningthreshold", "32768"),
        ("lowspacewarningthreshold", "+1"),
        ("lowspacewarningthreshold", "1.5"),
    ] {
        assert!(lib.pool_set("pool", attr, value).is_err());
    }
    lib.assign("pool", "LIB001L8").unwrap();
    lib.assign("pool", "LIB002L8").unwrap();
    lib.record_tape_capacity("LIB001L8", Some((2 * TIB, TIB)))
        .unwrap();
    assert!(lib.pool_show("pool").unwrap()["capacity"].is_null());
    assert!(lib.pool_show("pool").unwrap()["low_space_warning"].is_null());
    lib.record_tape_capacity("LIB002L8", Some((2 * TIB, TIB / 2)))
        .unwrap();
    let shown = lib.pool_show("pool").unwrap();
    assert_eq!(shown["capacity"], 4 * TIB);
    assert_eq!(shown["free_space"], TIB + TIB / 2);
    assert_eq!(shown["used_space"], 2 * TIB + TIB / 2);
    assert_eq!(shown["capacity_source"], "observed_dp_mam");
    assert!(shown["capacity_observed_at"].is_string());
    assert_eq!(shown["low_space_warning"], true);
    assert_eq!(shown["no_space_warning"], false);
    assert!(
        lib.record_tape_capacity("LIB002L8", Some((TIB, TIB + 1)))
            .is_err()
    );
    assert_eq!(lib.pool_show("pool").unwrap(), shown);
    assert!(lib.record_tape_capacity("missing", Some((TIB, 0))).is_err());
    lib.pool_set("pool", "lowspacewarningenable", "no").unwrap();
    assert_eq!(lib.pool_show("pool").unwrap()["low_space_warning"], false);
    lib.pool_set("pool", "lowspacewarningenable", "yes")
        .unwrap();
    lib.pool_set("pool", "lowspacewarningthreshold", "0")
        .unwrap();
    assert_eq!(lib.pool_show("pool").unwrap()["low_space_warning"], false);
    lib.record_tape_capacity("LIB001L8", Some((2 * TIB, 0)))
        .unwrap();
    lib.record_tape_capacity("LIB002L8", Some((2 * TIB, 0)))
        .unwrap();
    assert_eq!(lib.pool_show("pool").unwrap()["no_space_warning"], true);
    lib.pool_set("pool", "nospacewarningenable", "no").unwrap();
    assert_eq!(lib.pool_show("pool").unwrap()["no_space_warning"], false);
    lib.pool_set("pool", "lowspacewarningthreshold", "32767")
        .unwrap();
    lib.pool_set("pool", "poolname", "renamed").unwrap();
    let renamed = lib.pool_show("renamed").unwrap();
    drop(lib);
    let mut lib = fixture.open();
    assert_eq!(lib.pool_show("renamed").unwrap(), renamed);
    let cached = tape_rs::library::cached_status_if_active(&fixture.path)
        .unwrap()
        .unwrap();
    assert_eq!(cached["pools"][0], renamed);
    lib.record_tape_capacity("LIB001L8", None).unwrap();
    assert!(lib.pool_show("renamed").unwrap()["no_space_warning"].is_null());
    for barcode in ["LIB001L8", "LIB002L8"] {
        lib.unassign(barcode).unwrap();
    }
    lib.pool_delete("renamed").unwrap();
    lib.pool_create("renamed").unwrap();
    lib.assign("renamed", "LIB002L8").unwrap();
    let reset = lib.pool_show("renamed").unwrap();
    assert_eq!(reset["low_space_warning_threshold"], 0);
    assert_eq!(reset["no_space_warning_enable"], true);
    assert!(reset["capacity"].is_null());
}

#[test]
fn pool_nodegroups_survive_rename_and_restrict_real_drive_eligibility() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.assign("pool", "LIB001L8").unwrap();
    assert_eq!(lib.pool_nodegroup("pool").unwrap(), "G0");
    assert!(lib.pool_nodegroup("missing").is_err());
    assert!(lib.pool_set("pool", "nodegroup", "missing").is_err());
    assert_eq!(lib.pool_show("pool").unwrap()["explicit"], false);
    lib.nodegroup_create("G1").unwrap();
    lib.pool_set("pool", "nodegroup", "G1").unwrap();
    assert!(!lib.drive_eligible(0, "pool", 'r').unwrap());
    lib.drive_set_nodegroup("SIMDRV0000", Some("G1")).unwrap();
    assert!(!lib.drive_eligible(0, "pool", 'r').unwrap());
    lib.nodegroup_set_local(Some("G1")).unwrap();
    assert!(lib.drive_eligible(0, "pool", 'r').unwrap());
    lib.pool_set("pool", "poolname", "renamed").unwrap();
    assert_eq!(lib.pool_nodegroup("renamed").unwrap(), "G1");
    assert_eq!(lib.pool_show("renamed").unwrap()["nodegroup_name"], "G1");
    let cached = tape_rs::library::cached_status_if_active(&fixture.path)
        .unwrap()
        .unwrap();
    assert_eq!(cached["pools"][0]["nodegroup_name"], "G1");
    assert_eq!(cached["nodegroups"], lib.nodegroup_list().unwrap());
    assert_eq!(cached["local_nodegroup"], "G1");
    lib.drive_set_nodegroup("SIMDRV0000", None).unwrap();
    lib.nodegroup_set_local(None).unwrap();
    assert!(lib.nodegroup_delete("G1").is_err());
    lib.unassign("LIB001L8").unwrap();
    assert!(lib.nodegroup_delete("G1").is_err());
    drop(lib);
    let mut lib = fixture.open();
    assert_eq!(lib.pool_nodegroup("renamed").unwrap(), "G1");
    lib.pool_delete("renamed").unwrap();
    lib.nodegroup_delete("G1").unwrap();
    lib.pool_create("renamed").unwrap();
    assert_eq!(lib.pool_nodegroup("renamed").unwrap(), "G0");
}

#[test]
fn explicit_empty_pool_crud_and_uuid_survive_reopen() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    let drive_commands = fixture.drive.command_log();
    assert_eq!(lib.pool_list().unwrap(), json!([]));
    lib.pool_create("z_pool").unwrap();
    lib.pool_create("A1").unwrap();
    let shown = lib.pool_show("z_pool").unwrap();
    assert_eq!(shown["name"], "z_pool");
    assert_eq!(shown["explicit"], true);
    assert_eq!(shown["num_of_tapes"], 0);
    assert_eq!(shown["media_restriction"], "none");
    assert_eq!(shown["mount_limit"], 0);
    uuid::Uuid::parse_str(shown["uuid"].as_str().unwrap()).unwrap();
    for key in ["capacity", "used_space", "free_space", "reclaimable_space"] {
        assert!(
            shown.get(key).is_none_or(serde_json::Value::is_null),
            "不能伪造容量：{key}"
        );
    }
    assert_eq!(lib.pools().unwrap(), vec!["A1", "z_pool"]);
    assert_eq!(lib.pool_list().unwrap()[1], shown);
    assert!(fixture.sim.loaded_barcode(0).is_none());
    assert_eq!(fixture.drive.command_log(), drive_commands);
    drop(lib);

    let mut lib = fixture.open();
    assert_eq!(lib.pool_show("z_pool").unwrap(), shown);
    lib.pool_delete("z_pool").unwrap();
    assert!(lib.pool_show("z_pool").is_err());
    lib.pool_delete("A1").unwrap();
    assert!(lib.pools().unwrap().is_empty());
    drop(lib);
    assert_eq!(fixture.open().pool_list().unwrap(), json!([]));
}

#[test]
fn create_validates_names_and_rejects_explicit_and_implicit_duplicates() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    for name in [
        "",
        "abcdefghijklmnopq",
        "a-b",
        "a b",
        "a/b",
        "a\0b",
        "池",
        ".",
    ] {
        assert!(lib.pool_create(name).is_err(), "{name:?}");
    }
    assert_eq!(lib.pool_list().unwrap(), json!([]));
    for name in ["a", "0123456789ABC_dE"] {
        lib.pool_create(name).unwrap();
        let before = lib.pool_show(name).unwrap();
        assert!(lib.pool_create(name).is_err());
        assert_eq!(lib.pool_show(name).unwrap(), before);
    }
    lib.assign("implicit", "LIB001L8").unwrap();
    let before = lib.pool_show("implicit").unwrap();
    assert!(lib.pool_create("implicit").is_err());
    assert_eq!(lib.pool_show("implicit").unwrap(), before);
    assert_eq!(before["uuid"], json!(null));
    assert_eq!(before["explicit"], false);
}

#[test]
fn unknown_pool_operations_do_not_create_pools() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    assert!(lib.pool_show("missing").is_err());
    assert!(lib.pool_delete("missing").is_err());
    for (attribute, value) in [
        ("poolname", "new"),
        ("mountlimit", "1"),
        ("mediarestriction", "L9"),
    ] {
        assert!(lib.pool_set("missing", attribute, value).is_err());
    }
    assert_eq!(lib.pool_list().unwrap(), json!([]));
}

#[test]
fn nonempty_delete_and_counts_use_real_assignments() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.pool_create("explicit").unwrap();
    lib.assign("explicit", "LIB001L8").unwrap();
    lib.assign("implicit", "LIB002L8").unwrap();
    for name in ["explicit", "implicit"] {
        let before = lib.pool_show(name).unwrap();
        assert_eq!(before["num_of_tapes"], 1);
        assert!(lib.pool_delete(name).is_err());
        assert_eq!(lib.pool_show(name).unwrap(), before);
    }
    lib.unassign("LIB002L8").unwrap();
    assert!(lib.pool_show("implicit").is_err());
    lib.assign("explicit", "LIB002L8").unwrap();
    lib.blacklist("LIB001L8", Some("测试")).unwrap();
    assert_eq!(lib.pool_show("explicit").unwrap()["num_of_tapes"], 2);
    assert_eq!(lib.pool_list().unwrap()[0]["num_of_tapes"], 2);
    lib.unassign("LIB001L8").unwrap();
    lib.unassign("LIB002L8").unwrap();
    assert_eq!(lib.pool_show("explicit").unwrap()["num_of_tapes"], 0);
    lib.pool_delete("explicit").unwrap();
    assert_eq!(lib.pool_list().unwrap(), json!([]));
}

#[test]
fn legacy_database_and_names_remain_implicit_until_set() {
    let fixture = Fixture::new();
    let legacy = "legacy-pool name longer than 16";
    // 模拟升级前只有 library_volumes 的数据库。
    let db = Connection::open(&fixture.path).unwrap();
    db.execute_batch(
        "CREATE TABLE library_volumes (
            barcode TEXT PRIMARY KEY, pool TEXT NOT NULL, home INTEGER NOT NULL,
            blacklist TEXT, phase TEXT NOT NULL DEFAULT 'Assigned', failure TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO library_volumes(barcode,pool,home) VALUES('LIB001L8',?1,1000)",
        [legacy],
    )
    .unwrap();
    drop(db);

    let mut lib = fixture.open();
    assert_eq!(lib.pools().unwrap(), vec![legacy]);
    let shown = lib.pool_show(legacy).unwrap();
    for (key, value) in json!({"name":legacy, "uuid":null, "explicit":false,
               "media_restriction":"none", "mount_limit":0, "num_of_tapes":1,
               "nodegroup_name":"G0"})
    .as_object()
    .unwrap()
    {
        assert_eq!(&shown[key], value);
    }
    assert!(shown["capacity"].is_null());
    drop(lib);
    lib = fixture.open();
    assert_eq!(lib.pool_show(legacy).unwrap()["uuid"], json!(null));
    lib.unassign("LIB001L8").unwrap();
    assert!(lib.pools().unwrap().is_empty());
    lib.assign(legacy, "LIB001L8").unwrap();
    lib.pool_set(legacy, "mountlimit", "2").unwrap();
    let uuid = lib.pool_show(legacy).unwrap()["uuid"].clone();
    assert!(uuid.is_string());
    lib.unassign("LIB001L8").unwrap();
    drop(lib);
    let lib = fixture.open();
    let shown = lib.pool_show(legacy).unwrap();
    assert_eq!(shown["uuid"], uuid);
    assert_eq!(shown["mount_limit"], 2);
    assert_eq!(shown["explicit"], true);
    assert_eq!(shown["num_of_tapes"], 0);
}

#[test]
fn rename_preserves_uuid_configuration_and_assignment_metadata() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.pool_create("old").unwrap();
    lib.assign("old", "LIB001L8").unwrap();
    lib.assign("old", "LIB002L8").unwrap();
    lib.blacklist("LIB002L8", Some("保留原因")).unwrap();
    lib.pool_set("old", "mediarestriction", "L8").unwrap();
    lib.pool_set("old", "mountlimit", "3").unwrap();
    let mut before = lib.pool_show("old").unwrap();
    let mut volumes = lib.status().unwrap()["volumes"].clone();
    lib.pool_set("old", "poolname", "new").unwrap();
    before["name"] = json!("new");
    assert_eq!(lib.pool_show("new").unwrap(), before);
    assert!(lib.pool_show("old").is_err());
    assert!(lib.volumes("old").unwrap().is_empty());
    assert_eq!(lib.volumes("new").unwrap(), vec!["LIB001L8", "LIB002L8"]);
    assert!(lib.resolve("/old/LIB001L8/file").is_err());
    assert!(lib.resolve("/new/LIB001L8/file").is_ok());
    for volume in volumes.as_array_mut().unwrap() {
        volume["pool"] = json!("new");
    }
    assert_eq!(lib.status().unwrap()["volumes"], volumes);
    lib.pool_set("new", "poolname", "new").unwrap();
    assert_eq!(lib.pool_show("new").unwrap(), before);
    drop(lib);
    assert_eq!(fixture.open().pool_show("new").unwrap(), before);
}

#[test]
fn rename_collisions_do_not_merge_assignments_or_change_uuids() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.pool_create("source").unwrap();
    lib.pool_create("empty").unwrap();
    lib.assign("source", "LIB001L8").unwrap();
    lib.assign("implicit", "LIB002L8").unwrap();
    let before = lib.pool_list().unwrap();
    for name in ["empty", "implicit"] {
        assert!(lib.pool_set("source", "poolname", name).is_err());
        assert_eq!(lib.pool_list().unwrap(), before);
    }
    assert_eq!(lib.volumes("source").unwrap(), vec!["LIB001L8"]);
    assert_eq!(lib.volumes("implicit").unwrap(), vec!["LIB002L8"]);
}

#[test]
fn failed_assignment_update_rolls_back_rename_and_implicit_promotion() {
    for explicit in [false, true] {
        let fixture = Fixture::new();
        let mut lib = fixture.open();
        if explicit {
            lib.pool_create("old").unwrap();
        }
        lib.assign("old", "LIB001L8").unwrap();
        lib.assign("old", "LIB002L8").unwrap();
        let before = lib.pool_list().unwrap();
        let db = Connection::open(&fixture.path).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_pool_rename BEFORE UPDATE OF pool ON library_volumes
             WHEN OLD.barcode='LIB002L8'
             BEGIN SELECT RAISE(ABORT, 'injected assignment failure'); END;",
        )
        .unwrap();
        assert!(lib.pool_set("old", "poolname", "new").is_err());
        assert_eq!(lib.pool_list().unwrap(), before);
        assert_eq!(lib.volumes("old").unwrap(), vec!["LIB001L8", "LIB002L8"]);
        assert!(lib.volumes("new").unwrap().is_empty());
        assert!(lib.pool_show("new").is_err());
        db.execute_batch("DROP TRIGGER reject_pool_rename").unwrap();
        lib.pool_set("old", "poolname", "new").unwrap();
        assert_eq!(lib.pool_show("new").unwrap()["explicit"], true);
        assert_eq!(lib.pool_show("new").unwrap()["num_of_tapes"], 2);
        assert!(lib.pool_show("old").is_err());
    }
}

#[test]
fn invalid_attributes_and_values_preserve_existing_configuration() {
    for explicit in [false, true] {
        let fixture = Fixture::new();
        let mut lib = fixture.open();
        if explicit {
            lib.pool_create("pool").unwrap();
            lib.pool_set("pool", "mountlimit", "5").unwrap();
            lib.pool_set("pool", "mediarestriction", "L8").unwrap();
        }
        lib.assign("pool", "LIB001L8").unwrap();
        let before = lib.pool_list().unwrap();
        for (attribute, value) in [
            ("unsupported", "value"),
            ("capacity", "123"),
            ("worm", "yes"),
            ("mount_limit", "1"),
            ("mountlimit", ""),
            ("mountlimit", "-1"),
            ("mountlimit", "+1"),
            ("mountlimit", "1.0"),
            ("mountlimit", " 1"),
            ("mountlimit", "32768"),
            ("mountlimit", "999999999999999999999"),
            ("poolname", "invalid-name"),
            ("poolname", ""),
            ("mediarestriction", "l8"),
            ("mediarestriction", "^.{6}L8$"),
            ("mediarestriction", "L4"),
            ("mediarestriction", "L9"),
        ] {
            assert!(
                lib.pool_set("pool", attribute, value).is_err(),
                "{attribute}={value}"
            );
            assert_eq!(lib.pool_list().unwrap(), before, "{attribute}={value}");
        }
    }
}

#[test]
fn supported_settings_promote_implicit_pools_and_keep_uuid_stable() {
    for (attribute, value, name) in [
        ("mountlimit", "32767", "legacy-pool"),
        ("mediarestriction", "L8", "legacy-pool"),
        ("poolname", "renamed", "renamed"),
    ] {
        let fixture = Fixture::new();
        let mut lib = fixture.open();
        lib.assign("legacy-pool", "LIB001L8").unwrap();
        lib.pool_set("legacy-pool", attribute, value).unwrap();
        if attribute == "mountlimit" {
            assert_eq!(lib.pool_show(name).unwrap()["mount_limit"], 32767);
        }
        let uuid = lib.pool_show(name).unwrap()["uuid"].clone();
        assert!(uuid.is_string());
        lib.pool_set(name, "mountlimit", "0").unwrap();
        lib.pool_set(name, "mediarestriction", "none").unwrap();
        assert_eq!(lib.pool_show(name).unwrap()["uuid"], uuid);
        lib.unassign("LIB001L8").unwrap();
        drop(lib);
        let lib = fixture.open();
        assert_eq!(lib.pool_show(name).unwrap()["uuid"], uuid);
        assert_eq!(lib.pools().unwrap(), vec![name]);
    }
}

#[test]
fn every_supported_media_suffix_is_checked_without_drive_density_inference() {
    for suffix in [
        "L5", "L6", "L7", "L8", "L9", "M8", "JB", "JC", "JD", "JE", "JY", "JZ", "JV", "JK", "JL",
        "JM", "JF",
    ] {
        let fixture = Fixture::new();
        let barcode = format!("ABC123{suffix}");
        fixture
            .sim
            .insert_cartridge(SimCartridge::blank(&barcode, 256 << 20), 2)
            .unwrap();
        let mut lib = fixture.open();
        let drive_commands = fixture.drive.command_log();
        lib.pool_create("restricted").unwrap();
        lib.pool_set("restricted", "mediarestriction", suffix)
            .unwrap();
        lib.assign("restricted", &barcode).unwrap();
        assert_eq!(lib.pool_show("restricted").unwrap()["num_of_tapes"], 1);
        if suffix != "L8" {
            assert!(lib.assign("restricted", "LIB001L8").is_err());
            assert!(
                lib.volumes("restricted")
                    .unwrap()
                    .iter()
                    .all(|b| b == &barcode)
            );
        }
        assert!(fixture.sim.loaded_barcode(0).is_none());
        assert_eq!(fixture.drive.command_log(), drive_commands);
    }
}

#[test]
fn restricted_media_requires_exact_six_character_prefix_and_suffix() {
    for barcode in ["ABCDE L9", "12345L9", "1234567L9", "123456l9", "123456L9x"] {
        let fixture = Fixture::new();
        fixture
            .sim
            .insert_cartridge(SimCartridge::blank(barcode, 256 << 20), 2)
            .unwrap();
        let mut lib = fixture.open();
        lib.pool_create("pool").unwrap();
        lib.pool_set("pool", "mediarestriction", "L9").unwrap();
        let result = lib.assign("pool", barcode);
        assert_eq!(result.is_ok(), barcode == "ABCDE L9", "{barcode}");
        assert_eq!(
            lib.pool_show("pool").unwrap()["num_of_tapes"],
            usize::from(result.is_ok())
        );
    }
}

#[test]
fn none_allows_legacy_barcodes_and_rejected_assignment_leaves_no_row() {
    let fixture = Fixture::new();
    fixture
        .sim
        .insert_cartridge(SimCartridge::blank("OLD", 256 << 20), 2)
        .unwrap();
    let mut lib = fixture.open();
    lib.pool_create("pool").unwrap();
    lib.pool_set("pool", "mediarestriction", "L9").unwrap();
    assert!(lib.assign("pool", "OLD").is_err());
    assert_eq!(lib.pool_show("pool").unwrap()["num_of_tapes"], 0);
    lib.pool_set("pool", "mediarestriction", "none").unwrap();
    lib.assign("pool", "OLD").unwrap();
    let before = lib.pool_show("pool").unwrap();
    assert!(lib.pool_set("pool", "mediarestriction", "L8").is_err());
    assert_eq!(lib.pool_show("pool").unwrap(), before);
    assert_eq!(lib.volumes("pool").unwrap(), vec!["OLD"]);
}

#[test]
fn list_union_is_sorted_unique_and_queries_legacy_names_verbatim() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    lib.pool_create("shared").unwrap();
    lib.assign("shared", "LIB001L8").unwrap();
    let legacy = "legacy' SELECT; name";
    lib.assign(legacy, "LIB002L8").unwrap();
    assert_eq!(lib.pools().unwrap(), vec![legacy, "shared"]);
    let list = lib.pool_list().unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0], lib.pool_show(legacy).unwrap());
    assert_eq!(list[1], lib.pool_show("shared").unwrap());
    assert_eq!(list[0]["uuid"], json!(null));
    let db = Connection::open(&fixture.path).unwrap();
    let stored: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM library_pools WHERE name=?1",
            params![legacy],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 0, "查询不能隐式创建注册表记录");
}

#[test]
fn cached_status_recomputes_pools_after_registry_and_assignment_changes() {
    let fixture = Fixture::new();
    let mut lib = fixture.open();
    let cached_pools = || {
        tape_rs::library::cached_status_if_active(&fixture.path)
            .unwrap()
            .unwrap()["pools"]
            .clone()
    };
    let changer_commands = fixture.changer.command_log();
    let drive_commands = fixture.drive.command_log();
    lib.pool_create("empty").unwrap();
    lib.pool_set("empty", "mountlimit", "2").unwrap();
    assert_eq!(cached_pools(), lib.pool_list().unwrap());
    assert_eq!(fixture.changer.command_log(), changer_commands);
    assert_eq!(fixture.drive.command_log(), drive_commands);
    lib.assign("implicit", "LIB001L8").unwrap();
    assert_eq!(cached_pools(), lib.pool_list().unwrap());
    lib.pool_set("implicit", "poolname", "renamed").unwrap();
    assert_eq!(cached_pools(), lib.pool_list().unwrap());
    lib.unassign("LIB001L8").unwrap();
    assert_eq!(cached_pools(), lib.pool_list().unwrap());
    lib.pool_delete("renamed").unwrap();
    lib.pool_delete("empty").unwrap();
    assert_eq!(cached_pools(), json!([]));
}
