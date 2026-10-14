//! 逻辑库测试默认只使用 SimTransport，绝不访问真实设备。
use tape_rs::library::{Library, LibraryBackend};
use tape_rs::scsi::reservation::ReservationKey;
use tape_rs::scsi::sim::{Fault, FaultAction, SimTransport};
use tape_rs::tapefs::Backend;

#[path = "support/library_fixture.rs"]
mod support;
use support::Fixture;

#[test]
fn assignment_blacklist_persistence_and_no_implicit_move() {
    let f = Fixture::new();
    let mut lib = f.open();
    assert_eq!(lib.status().unwrap()["volumes"][0]["state"], "NEED_ASSIGN");
    lib.assign("pool", "LIB001L8").unwrap();
    assert!(lib.assign("pool", "absent").is_err());
    assert!(lib.resolve("/pool/LIB001L8/../file").is_err());
    assert!(lib.resolve("/other/LIB001L8/file").is_err());
    lib.blacklist("LIB001L8", Some("测试隔离")).unwrap();
    assert!(lib.read_to("/pool/LIB001L8/file", &mut Vec::new()).is_err());
    assert!(f.sim.loaded_barcode(0).is_none());
    assert_eq!(lib.status().unwrap()["volumes"][0]["state"], "BLACKLISTED");
    assert!(
        Library::<SimTransport, SimTransport>::open(
            f.sim.changer(),
            vec![],
            &f.path,
            ReservationKey::new(2, 101)
        )
        .is_err()
    );
    let snapshot = tape_rs::library::cached_status_if_active(&f.path)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["source"], "cached_inventory");
    assert_eq!(snapshot["volumes"][0]["state"], "BLACKLISTED");
    drop(lib);
    assert!(
        tape_rs::library::cached_status_if_active(&f.path)
            .unwrap()
            .is_none()
    );
    let mut reopened = f.open();
    assert_eq!(reopened.volumes("pool").unwrap(), vec!["LIB001L8"]);
    assert!(
        reopened
            .read_to("/pool/LIB001L8/file", &mut Vec::new())
            .is_err()
    );
    reopened.blacklist("LIB001L8", None).unwrap();
    reopened.unassign("LIB001L8").unwrap();
    assert!(reopened.pools().unwrap().is_empty());
}

#[test]
fn multi_volume_backend_reads_writes_metadata_and_switches() {
    let f = Fixture::new();
    let lib = f.formatted();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(lib));
    let backend = LibraryBackend(shared.clone());
    assert_eq!(backend.list_dir("/").unwrap().unwrap()[0].name, "pool");
    assert_eq!(backend.list_dir("/pool").unwrap().unwrap().len(), 2);
    let local = f.path.with_extension("input");
    std::fs::write(&local, b"payload").unwrap();
    for b in ["LIB001L8", "LIB002L8"] {
        let p = format!("/pool/{b}/dir");
        backend.mkdir(&p).unwrap();
        backend
            .upload(&format!("{p}/f"), &local, true, true)
            .unwrap();
        backend
            .change_xattr(&format!("{p}/f"), "user.binary", Some(&[0, 255]), 0)
            .unwrap();
        backend.symlink("f", &format!("{p}/link")).unwrap();
    }
    assert_eq!(
        backend.rename("/pool/LIB001L8/dir/f", "/pool/LIB002L8/dir/g", false),
        Err(nix::libc::EXDEV)
    );
    for b in ["LIB001L8", "LIB002L8", "LIB001L8"] {
        let mut out = Vec::new();
        shared
            .lock()
            .unwrap()
            .read_to(&format!("/pool/{b}/dir/f"), &mut out)
            .unwrap();
        assert_eq!(out, b"payload");
        let stat = backend.stat(&format!("/pool/{b}/dir/f")).unwrap().unwrap();
        assert_eq!(stat.current.unwrap().barcode, b);
        assert_eq!(
            backend
                .list_dir(&format!("/pool/{b}/dir"))
                .unwrap()
                .unwrap()
                .len(),
            2
        );
    }
    shared.lock().unwrap().close().unwrap();
    assert!(f.sim.loaded_barcode(0).is_none());
    std::fs::remove_file(local).unwrap();
}

#[test]
fn lost_ownership_never_refences_or_moves_again() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.write_from("/pool/LIB002L8/f", &mut &b"safe"[..], true)
        .unwrap();
    let other = f.sim.drive_as(0, 2);
    tape_rs::scsi::reservation::fence(&other, ReservationKey::new(2, 101)).unwrap();
    assert!(lib.read_to("/pool/LIB002L8/f", &mut Vec::new()).is_err());
    let changer_log = f.changer.command_log();
    assert!(lib.read_to("/pool/LIB001L8/f", &mut Vec::new()).is_err());
    assert_eq!(f.changer.command_log(), changer_log);
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB002L8"));
    assert!(lib.close().is_err());
}

#[test]
fn unassigned_drive_is_never_evicted() {
    let f = Fixture::new();
    f.sim.load_into_drive("LIB002L8", 0).unwrap();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    assert!(lib.read_to("/pool/LIB001L8/f", &mut Vec::new()).is_err());
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB002L8"));
}

#[test]
fn load_failure_retains_stage_and_does_not_blindly_unload() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    f.drive.inject(Fault {
        opcode: tape_rs::scsi::cdb::opcode::LOAD_UNLOAD,
        action: FaultAction::CheckCondition {
            sense_key: 3,
            asc: 0x11,
            ascq: 0,
        },
        after: 0,
        remaining: 1,
    });
    assert!(lib.read_to("/pool/LIB001L8/f", &mut Vec::new()).is_err());
    let status = lib.status().unwrap();
    let v = status["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["barcode"] == "LIB001L8")
        .unwrap();
    assert_eq!(v["state"], "Failed");
    assert!(
        v["failure"]
            .as_str()
            .unwrap()
            .contains("Loading: SCSI command failed: status=0x02, sense_key=0x03"),
        "{status}"
    );
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    lib.close().unwrap();
    let final_status = lib.status().unwrap();
    let row = final_status["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["barcode"] == "LIB001L8")
        .unwrap();
    assert!(
        row["last_failure"]
            .as_str()
            .unwrap()
            .contains("Loading: SCSI command failed")
    );
}

#[test]
fn unsupported_changer_pr_refuses_before_move() {
    let f = Fixture::new();
    let mut lib = Library::open(
        f.sim.changer(),
        vec![tape_rs::library::LibraryDrive {
            offset: 0,
            device: f.sim.drive(0),
        }],
        &f.path,
        ReservationKey::new(1, 100),
    )
    .unwrap();
    lib.assign("pool", "LIB001L8").unwrap();
    let error = lib
        .read_to("/pool/LIB001L8/f", &mut Vec::new())
        .unwrap_err();
    assert!(error.to_string().contains("Unsupported"));
    assert!(f.sim.loaded_barcode(0).is_none());
}
