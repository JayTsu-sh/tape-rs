//! 公共管理 API 与模拟 SCSI 边界；不接触真实设备。
#[path = "support/library_fixture.rs"]
mod support;

use support::Fixture;
use tape_rs::changer::commands::MediumChanger;
use tape_rs::scsi::transport::TapeTransport;

/// 外部设备边界：可报告 MOVE 成功但不改变库存，验证调用方必须回读条码。
struct MoveProbe {
    media: std::sync::Arc<tape_rs::scsi::sim::SimTransport>,
    pr: std::sync::Arc<tape_rs::scsi::sim::SimTransport>,
    false_success: bool,
}

impl TapeTransport for MoveProbe {
    fn execute_no_data(
        &self,
        c: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0xa5 && self.false_success {
            return Ok(tape_rs::scsi::device::ScsiResult {
                status: 0,
                sense: tape_rs::scsi::sense::SenseInfo::from_bytes(&[]),
                transferred: 0,
                duration_ms: 0,
            });
        }
        if c[0] == 0x5f {
            self.pr.execute_no_data(c, t)
        } else {
            self.media.execute_no_data(c, t)
        }
    }
    fn execute_read(
        &self,
        c: &[u8],
        b: &mut [u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0x5e {
            self.pr.execute_read(c, b, t)
        } else {
            self.media.execute_read(c, b, t)
        }
    }
    fn execute_write(
        &self,
        c: &[u8],
        b: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0x5f {
            self.pr.execute_write(c, b, t)
        } else {
            self.media.execute_write(c, b, t)
        }
    }
    fn identity(&self) -> &str {
        "maintenance-device-probe"
    }
}

fn probe_library(
    f: &Fixture,
    false_success: bool,
) -> (
    tape_rs::library::Library<MoveProbe, support::SharedTransport>,
    std::sync::Arc<tape_rs::scsi::sim::SimTransport>,
) {
    let pr = std::sync::Arc::new(tape_rs::scsi::sim::SimLibrary::new(1, 1, 0).drive(0));
    let lib = tape_rs::library::Library::open(
        MoveProbe {
            media: f.changer.clone(),
            pr: pr.clone(),
            false_success,
        },
        vec![tape_rs::library::LibraryDrive {
            offset: 0,
            device: support::SharedTransport(f.drive.clone()),
        }],
        &f.path,
        tape_rs::scsi::reservation::ReservationKey::new(1, 100),
    )
    .unwrap();
    (lib, pr)
}

#[test]
fn rejected_moves_leave_inventory_and_loaded_foreign_tape_untouched() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    f.sim.load_into_drive("LIB001L8", 0).unwrap();
    let before = f.changer.command_log().len();
    let drive_before = f.drive.command_log().len();
    for (barcode, location) in [
        ("LIB001L8", "bad"),
        ("LIB002L8", "homeslot"),
        ("LIB001L8", "ieslot"),
        ("LIB001L8", "homeslot"),
    ] {
        assert!(lib.tape_move(barcode, location).is_err());
    }
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    assert!(
        f.changer.command_log()[before..]
            .iter()
            .all(|op| ![0xa5, 0x5f].contains(op))
    );
    assert!(
        f.drive.command_log()[drive_before..]
            .iter()
            .all(|op| ![0x1b, 0x5f].contains(op))
    );
    lib.close().unwrap();
}

#[test]
fn ie_roundtrip_requires_offline_and_preserves_assignment_and_home() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    assert!(lib.tape_move("LIB001L8", "ieslot").is_err());
    lib.tape_offline("LIB001L8", "export test").unwrap();
    let exported = lib.tape_move("LIB001L8", "ieslot").unwrap();
    assert_eq!(exported["address"], 101);
    assert_eq!(lib.volumes("pool").unwrap(), vec!["LIB001L8"]);
    assert_eq!(lib.status().unwrap()["volumes"][0]["state"], "Offline");
    let returned = lib.tape_move("LIB001L8", "homeslot").unwrap();
    assert_eq!(returned["address"], 1001);
    assert_eq!(lib.status().unwrap()["volumes"][0]["home_slot"], 1);
    let before = f.changer.command_log().len();
    assert_eq!(
        lib.tape_move("LIB001L8", "homeslot").unwrap()["moved"],
        false
    );
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    lib.close().unwrap();
}

#[test]
fn misplaced_target_and_full_ie_do_not_trigger_move_or_unload_another_volume() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.inspect_volume("/pool/LIB001L8", |_, _| Ok(())).unwrap();
    let mut changer = MediumChanger::new(f.changer.as_ref());
    let map = changer.load_address_map().unwrap().clone();
    changer
        .move_medium(map.storage_start + 1, map.storage_start + 2)
        .unwrap();
    let before = f.changer.command_log().len();
    assert!(lib.tape_move("LIB002L8", "homeslot").is_err());
    assert_eq!(f.sim.loaded_barcode(0).as_deref(), Some("LIB001L8"));
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    lib.close().unwrap();
}

#[test]
fn misplaced_unclaimed_target_fails_before_changer_fencing() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    let mut changer = MediumChanger::new(f.changer.as_ref());
    let map = changer.load_address_map().unwrap().clone();
    changer
        .move_medium(map.storage_start, map.storage_start + 2)
        .unwrap();
    let before = f.changer.command_log().len();
    assert!(lib.tape_move("LIB001L8", "homeslot").is_err());
    assert!(
        f.changer.command_log()[before..]
            .iter()
            .all(|op| ![0xa5, 0x5f].contains(op))
    );
    // 驱动 PR 是公共设备边界，可独立确认错误请求未取得资格。
    assert!(!f.drive.command_log().contains(&0x5f));
    lib.close().unwrap();
}

#[test]
fn occupied_home_and_blacklist_are_rejected_without_move() {
    let f = Fixture::new();
    let mut lib = f.open();
    lib.assign("pool", "LIB001L8").unwrap();
    lib.tape_offline("LIB001L8", "export").unwrap();
    lib.tape_move("LIB001L8", "ieslot").unwrap();
    let mut changer = MediumChanger::new(f.changer.as_ref());
    let map = changer.load_address_map().unwrap().clone();
    changer
        .move_medium(map.storage_start + 1, map.storage_start)
        .unwrap();
    let before = f.changer.command_log().len();
    assert!(lib.tape_move("LIB001L8", "homeslot").is_err());
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    lib.blacklist("LIB001L8", Some("suspect")).unwrap();
    assert!(
        lib.tape_move("LIB001L8", "ieslot")
            .unwrap_err()
            .to_string()
            .contains("BLACKLISTED")
    );
    lib.close().unwrap();
}

#[test]
fn owned_drive_returns_normally_to_home_and_no_new_move_when_ie_is_full() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.inspect_volume("/pool/LIB001L8", |_, _| Ok(())).unwrap();
    lib.tape_move("LIB001L8", "homeslot").unwrap();
    assert!(f.sim.loaded_barcode(0).is_none());
    lib.tape_offline("LIB001L8", "export").unwrap();
    lib.tape_move("LIB001L8", "ieslot").unwrap();
    lib.tape_offline("LIB002L8", "export").unwrap();
    let before = f.changer.command_log().len();
    assert!(
        lib.tape_move("LIB002L8", "ieslot")
            .unwrap_err()
            .to_string()
            .contains("I/E")
    );
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    lib.close().unwrap();
}

#[test]
fn reconcile_replaces_stale_index_only_after_successful_actual_inspection() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    assert!(lib.tape_index("LIB001L8").unwrap().is_none());
    let first = lib.tape_reconcile("LIB001L8").unwrap();
    assert_eq!(first["generation"], "1");
    assert!(first["observed_at"].as_str().is_some_and(|s| !s.is_empty()));
    lib.write_from("/pool/LIB001L8/new-file", &mut &b"new contents"[..], true)
        .unwrap();
    assert_eq!(lib.tape_index("LIB001L8").unwrap().unwrap(), first);
    lib.drive_set_enabled("SIMDRV0000", false).unwrap();
    assert!(lib.tape_reconcile("LIB001L8").is_err());
    assert_eq!(lib.tape_index("LIB001L8").unwrap().unwrap(), first);
    lib.drive_set_enabled("SIMDRV0000", true).unwrap();
    let updated = lib.tape_reconcile("LIB001L8").unwrap();
    assert_eq!(updated["generation"], "2");
    let xml = updated["index_xml"].as_str().unwrap();
    let actual = tape_rs::ltfs::index::LtfsIndex::parse(xml.as_bytes()).unwrap();
    assert_eq!(actual.find_file("new-file").unwrap().length, 12);
    assert_eq!(
        actual.volume_uuid.to_string(),
        first["uuid"].as_str().unwrap()
    );
    lib.close().unwrap();
    drop(lib);
    assert_eq!(f.open().tape_index("LIB001L8").unwrap().unwrap(), updated);
}

#[test]
fn validation_reads_all_regular_files_and_distinguishes_unverified_checksums() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.with_volume("/pool/LIB001L8", |volume, _| {
        volume.append_file("hashed", &mut &b"payload"[..])?;
        volume.set_hash_policy(tape_rs::ltfs::volume::HashPolicy {
            sha256: false,
            md5: false,
        });
        volume.append_file("no-hash", &mut &b"plain"[..])?;
        volume.add_symlink("dangling", "missing")
    })
    .unwrap();
    let report = lib.tape_validate("LIB001L8", false).unwrap();
    let file = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "hashed")
        .unwrap();
    assert_eq!(file["read"], false);
    assert_eq!(file["checksum"], "not_verified_index_only");
    let report = lib.tape_validate("LIB001L8", true).unwrap();
    let files = report["files"].as_array().unwrap();
    for (path, checksum, read) in [
        ("hashed", "verified_sha256sum", true),
        ("no-hash", "not_verified_no_hash", true),
        ("dangling", "not_applicable_symlink", false),
    ] {
        let file = files.iter().find(|f| f["path"] == path).unwrap();
        assert_eq!(file["checksum"], checksum);
        assert_eq!(file["read"], read);
    }
    assert!(lib.tape_index("LIB001L8").unwrap().is_none());
    lib.close().unwrap();
}

#[test]
fn full_validation_detects_payload_corruption_even_without_a_stored_hash() {
    for with_hash in [true, false] {
        let f = Fixture::new();
        let mut lib = f.formatted();
        lib.with_volume("/pool/LIB001L8", |volume, _| {
            volume.set_hash_policy(tape_rs::ltfs::volume::HashPolicy {
                sha256: with_hash,
                md5: false,
            });
            volume.append_file("file", &mut &b"original-payload"[..])?;
            Ok(())
        })
        .unwrap();
        lib.close().unwrap();
        f.sim.with_cartridge_mut("LIB001L8", |cart| {
            for partition in &mut cart.partitions {
                for object in &mut partition.objects {
                    if let tape_rs::scsi::sim::LogicalObject::Record(data) = object
                        && data == b"original-payload"
                    {
                        if with_hash {
                            data[0] ^= 1;
                        } else {
                            data.truncate(2);
                        }
                    }
                }
            }
        });
        // 索引验证不能代替内容读取。
        lib.tape_validate("LIB001L8", false).unwrap();
        assert!(lib.tape_validate("LIB001L8", true).is_err());
        lib.close().unwrap();
    }
}

#[test]
fn validation_and_reconcile_require_maintenance_role_and_do_not_write_media() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.drive_set_role("SIMDRV0000", "r").unwrap();
    assert!(lib.tape_validate("LIB001L8", false).is_err());
    assert!(lib.tape_reconcile("LIB001L8").is_err());
    lib.drive_set_role("SIMDRV0000", "g").unwrap();
    let before = f.drive.command_log().len();
    lib.tape_validate("LIB001L8", true).unwrap();
    lib.tape_reconcile("LIB001L8").unwrap();
    assert!(
        f.drive.command_log()[before..]
            .iter()
            .all(|op| ![0x04, 0x0a, 0x10, 0x19, 0x8d].contains(op))
    );
    lib.close().unwrap();
}

#[test]
fn move_success_requires_post_move_barcode_and_retains_failure_diagnostics() {
    let f = Fixture::new();
    let (mut lib, _) = probe_library(&f, true);
    lib.assign("pool", "LIB001L8").unwrap();
    lib.tape_offline("LIB001L8", "export").unwrap();
    let error = lib.tape_move("LIB001L8", "ieslot").unwrap_err();
    assert!(error.to_string().contains("MOVE 后目标条码不符"));
    let status = lib.status().unwrap();
    let volume = status["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["barcode"] == "LIB001L8")
        .unwrap();
    assert_eq!(volume["address"], 1001);
    assert!(
        volume["last_failure"]
            .as_str()
            .unwrap()
            .contains("destination=101")
    );
    lib.close().unwrap();
}

#[test]
fn failed_preflight_never_registers_or_preempts_changer_reservations() {
    let f = Fixture::new();
    let (mut lib, pr) = probe_library(&f, false);
    lib.assign("pool", "LIB001L8").unwrap();
    f.sim.load_into_drive("LIB001L8", 0).unwrap();
    assert!(lib.tape_move("LIB001L8", "homeslot").is_err());
    assert!(!pr.command_log().contains(&0x5f));
    assert!(!f.changer.command_log().contains(&0xa5));
    lib.close().unwrap();
}

#[test]
fn cancelled_move_keeps_not_started_outcome_and_issues_no_mechanics() {
    let f = Fixture::new();
    let (mut lib, pr) = probe_library(&f, false);
    lib.assign("pool", "LIB001L8").unwrap();
    lib.tape_offline("LIB001L8", "export").unwrap();
    let id = lib.begin_task("tape_move", "LIB001L8").unwrap();
    let tasks = tape_rs::library::tasks::TaskStore::open(&f.path).unwrap();
    tasks.cancel(id).unwrap();
    assert!(lib.tape_move("LIB001L8", "ieslot").is_err());
    assert!(!pr.command_log().contains(&0x5f));
    assert!(!f.changer.command_log().contains(&0xa5));
    assert_eq!(
        tasks.show(id).unwrap().outcome.unwrap()["effects"],
        "not_started"
    );
    lib.close().unwrap();
    assert_eq!(
        lib.finish_task(false, "cancelled").unwrap().unwrap()["state"],
        "cancelled"
    );
}

#[test]
fn actual_index_read_failure_preserves_previous_cache() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    let previous = lib.tape_reconcile("LIB001L8").unwrap();
    lib.close().unwrap();
    f.drive.inject(tape_rs::scsi::sim::Fault {
        opcode: 0x08,
        action: tape_rs::scsi::sim::FaultAction::HostError(3),
        after: 0,
        remaining: 1,
    });
    let error = lib.tape_reconcile("LIB001L8").unwrap_err();
    assert!(matches!(error, tape_rs::error::TapeError::ScsiHost(3)));
    assert_eq!(lib.tape_index("LIB001L8").unwrap().unwrap(), previous);
    lib.close().unwrap();
}

#[test]
fn unsupported_index_xml_cannot_silently_replace_a_complete_cached_snapshot() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    let previous = lib.tape_reconcile("LIB001L8").unwrap();
    lib.close().unwrap();
    assert!(f.sim.with_cartridge_mut("LIB001L8", |cart| {
        for partition in &mut cart.partitions {
            for object in &mut partition.objects {
                if let tape_rs::scsi::sim::LogicalObject::Record(data) = object
                    && let Ok(xml) = std::str::from_utf8(data)
                    && xml.contains("</ltfsindex>")
                {
                    *data = xml
                        .replace(
                            "</ltfsindex>",
                            "<vendorprivate>keep me</vendorprivate></ltfsindex>",
                        )
                        .into_bytes();
                }
            }
        }
    }));
    assert!(lib.tape_reconcile("LIB001L8").is_err());
    assert_eq!(lib.tape_index("LIB001L8").unwrap().unwrap(), previous);
    lib.close().unwrap();
}

#[test]
fn cached_generation_preserves_full_unsigned_range_without_fake_zero() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.close().unwrap();
    f.sim.with_cartridge_mut("LIB001L8", |cart| {
        for partition in &mut cart.partitions {
            for object in &mut partition.objects {
                if let tape_rs::scsi::sim::LogicalObject::Record(data) = object
                    && let Ok(xml) = std::str::from_utf8(data)
                    && xml.contains("<generationnumber>1</generationnumber>")
                {
                    *data = xml
                        .replace(
                            "<generationnumber>1</generationnumber>",
                            "<generationnumber>18446744073709551615</generationnumber>",
                        )
                        .into_bytes();
                }
            }
        }
    });
    let index = lib.tape_reconcile("LIB001L8").unwrap();
    assert_eq!(index["generation"], "18446744073709551615");
    assert_eq!(
        lib.tape_index("LIB001L8").unwrap().unwrap()["generation"],
        "18446744073709551615"
    );
    lib.close().unwrap();
}

struct CancelAfterPayload {
    device: std::sync::Arc<tape_rs::scsi::sim::SimTransport>,
    task: std::sync::Arc<std::sync::Mutex<Option<i64>>>,
    state: std::path::PathBuf,
    second_read: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TapeTransport for CancelAfterPayload {
    fn execute_no_data(
        &self,
        c: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.device.execute_no_data(c, t)
    }
    fn execute_read(
        &self,
        c: &[u8],
        b: &mut [u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        let result = self.device.execute_read(c, b, t)?;
        if c[0] == 0x08 {
            if b[..result.transferred].starts_with(b"first-payload")
                && let Some(id) = self.task.lock().unwrap().take()
            {
                tape_rs::library::tasks::TaskStore::open(&self.state)?.cancel(id)?;
            }
            if b[..result.transferred].starts_with(b"second-payload") {
                self.second_read
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Ok(result)
    }
    fn execute_write(
        &self,
        c: &[u8],
        b: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.device.execute_write(c, b, t)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.device.session_epoch()
    }
    fn identity(&self) -> &str {
        self.device.identity()
    }
}

#[test]
fn validation_cancellation_between_files_keeps_cleanup_and_does_not_read_next_file() {
    let f = Fixture::new();
    let mut lib = f.formatted();
    lib.with_volume("/pool/LIB001L8", |volume, _| {
        volume.append_file("a", &mut &b"first-payload"[..])?;
        volume.append_file("b", &mut &b"second-payload"[..])?;
        Ok(())
    })
    .unwrap();
    lib.close().unwrap();
    drop(lib);
    let task = std::sync::Arc::new(std::sync::Mutex::new(None));
    let second_read = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut lib = tape_rs::library::Library::open(
        MoveProbe {
            media: f.changer.clone(),
            pr: std::sync::Arc::new(tape_rs::scsi::sim::SimLibrary::new(1, 1, 0).drive(0)),
            false_success: false,
        },
        vec![tape_rs::library::LibraryDrive {
            offset: 0,
            device: CancelAfterPayload {
                device: f.drive.clone(),
                task: task.clone(),
                state: f.path.clone(),
                second_read: second_read.clone(),
            },
        }],
        &f.path,
        tape_rs::scsi::reservation::ReservationKey::new(1, 100),
    )
    .unwrap();
    let id = lib.begin_task("tape_validate", "LIB001L8").unwrap();
    *task.lock().unwrap() = Some(id);
    assert!(lib.tape_validate("LIB001L8", true).is_err());
    assert!(!second_read.load(std::sync::atomic::Ordering::SeqCst));
    lib.close().unwrap();
    assert!(f.sim.loaded_barcode(0).is_none());
    assert_eq!(
        lib.finish_task(false, "cancelled").unwrap().unwrap()["state"],
        "cancelled"
    );
}
