//! LTFS 2.5 Full + Incremental 链的纯软件介质验证，不访问设备。
use std::io::Cursor;
use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::RecoveryBudget;
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};
use tape_rs::tape::commands::TapeDrive;

fn formatted() -> (SimLibrary, SimTransport) {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank("DEL001L8", 64 << 20), 0)
        .unwrap();
    lib.load_into_drive("DEL001L8", 0).unwrap();
    let dev = lib.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "DEL001".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    (lib, dev)
}

#[test]
fn increments_recover_and_clean_unmount_writes_full_on_both_partitions() {
    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/dir/a", &mut Cursor::new(b"old")).unwrap();
    vol.commit_incremental().unwrap();
    let first = vol.index().self_location;
    assert!(vol.index().incremental);
    drop(vol);
    lib.power_cut();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable());
    assert_eq!(vol.index().generation, 2);
    vol.remove_file("/dir/a").unwrap();
    vol.append_file("/dir/b", &mut Cursor::new(b"new")).unwrap();
    vol.commit_incremental().unwrap();
    assert_eq!(vol.index().previous_incremental_location, Some(first));
    drop(vol);
    lib.power_cut();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.index().find_file("dir/a").is_none());
    let mut bytes = Vec::new();
    vol.read_file_to_writer("/dir/b", &mut bytes).unwrap();
    assert_eq!(bytes, b"new");
    assert!(vol.recovery().ip_debt);
    vol.unmount().unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.index().incremental);
    assert!(!vol.recovery().ip_debt);
    let ip = &vol.recovery().ip.last_index.as_ref().unwrap().index;
    assert!(!ip.incremental);
    assert_eq!(ip.previous_location, Some(vol.index().self_location));
    assert!(ip.previous_incremental_location.is_none());
}

#[test]
fn delta_is_not_a_full_snapshot_and_partial_chain_is_not_accepted() {
    let (_lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for i in 0..40 {
        vol.append_file(&format!("/f{i}"), &mut Cursor::new(b"x"))
            .unwrap();
    }
    vol.commit().unwrap();
    let full_size = vol.index().to_xml().unwrap().len();
    vol.append_file("/change", &mut Cursor::new(b"changed"))
        .unwrap();
    vol.commit_incremental().unwrap();
    let pos = vol.index().self_location.start_block;
    let drive = TapeDrive::new(&dev);
    drive.locate(1, pos, true).unwrap();
    let mut buf = vec![0; 65536];
    let n = drive.read_block(&mut buf).unwrap();
    let delta = LtfsIndex::parse(&buf[..n]).unwrap();
    assert!(delta.incremental);
    assert_eq!(delta.root.files.len(), 1);
    assert!(n < full_size / 4);
    vol.append_file("/again", &mut Cursor::new(b"second"))
        .unwrap();
    vol.commit_incremental().unwrap();
    drop(vol);
    let budget = RecoveryBudget {
        max_chain_len: 1,
        ..Default::default()
    };
    assert!(LtfsVolume::mount_with_budget(&dev, &budget).is_err());
}

#[test]
fn broken_incremental_predecessor_refuses_mount() {
    let (_lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/one", &mut Cursor::new(b"1")).unwrap();
    vol.commit_incremental().unwrap();
    vol.append_file("/two", &mut Cursor::new(b"2")).unwrap();
    vol.commit_incremental().unwrap();
    let pos = vol.index().self_location.start_block;
    drop(vol);
    let drive = TapeDrive::new(&dev);
    drive.locate(1, pos, true).unwrap();
    let mut buf = vec![0; 65536];
    let n = drive.read_block(&mut buf).unwrap();
    let mut delta = LtfsIndex::parse(&buf[..n]).unwrap();
    delta
        .previous_incremental_location
        .as_mut()
        .unwrap()
        .start_block = 4;
    drive.locate(1, pos, true).unwrap();
    drive.write_block(&delta.to_xml().unwrap()).unwrap();
    drive.write_filemark(1).unwrap();
    drive.write_filemark(0).unwrap();
    assert!(LtfsVolume::mount(&dev).is_err());
}

#[test]
fn failed_incremental_barrier_never_publishes_and_freezes_writer() {
    let (_lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/one", &mut Cursor::new(b"1")).unwrap();
    dev.inject(Fault {
        opcode: opcode::WRITE_FILEMARKS_6,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0c,
            ascq: 0,
        },
        after: 2,
        remaining: 1,
    });
    assert!(vol.commit_incremental().is_err());
    assert!(!vol.writable());
    assert_eq!(vol.index().generation, 1);
    assert!(vol.index().find_file("one").is_none());
    assert!(vol.append_file("/two", &mut Cursor::new(b"2")).is_err());
}

#[test]
fn namespace_metadata_survives_incremental_replay_without_copying_file_data() {
    use tape_rs::ltfs::index::Xattr;
    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/source/a", &mut Cursor::new(b"payload"))
        .unwrap();
    vol.commit().unwrap();
    let before = vol.index().find_file("source/a").unwrap().clone();
    vol.rename_path("/source", "/renamed").unwrap();
    vol.create_directory("/empty").unwrap();
    vol.set_node_xattr(
        "/renamed/a",
        Xattr {
            key: "note".into(),
            value: "AP8=".into(),
            base64: true,
        },
        true,
        false,
    )
    .unwrap();
    vol.set_node_readonly("/renamed/a", true).unwrap();
    vol.add_symlink("/link", "renamed/a").unwrap();
    vol.commit_incremental().unwrap();
    drop(vol);
    lib.power_cut();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    let f = vol.index().find_file("renamed/a").unwrap();
    assert_eq!(f.meta.file_uid, before.meta.file_uid);
    assert_eq!(f.extents, before.extents);
    assert!(f.meta.readonly);
    assert_eq!(f.xattr("note"), Some("AP8="));
    assert!(vol.index().root.subdirs.iter().any(|d| d.name == "empty"));
    assert_eq!(
        vol.index().find_file("link").unwrap().symlink.as_deref(),
        Some("renamed/a")
    );
    vol.remove_node_xattr("/renamed/a", "note").unwrap();
    vol.remove_directory("/empty").unwrap();
    vol.commit_incremental().unwrap();
    drop(vol);
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(
        vol.index()
            .find_file("renamed/a")
            .unwrap()
            .xattr("note")
            .is_none()
    );
    assert!(!vol.index().root.subdirs.iter().any(|d| d.name == "empty"));
}

#[test]
fn periodic_full_checkpoint_bounds_chain_across_remounts() {
    let (lib, dev) = formatted();
    for n in 1..=13 {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file(&format!("/f{n}"), &mut Cursor::new([n as u8]))
            .unwrap();
        vol.commit_incremental().unwrap();
        assert_eq!(vol.index().incremental, n % 6 != 0);
        drop(vol);
        lib.power_cut();
        let vol = LtfsVolume::mount(&dev).unwrap();
        assert_eq!(vol.recovery().incremental_depth, n % 6);
        assert_eq!(vol.list().len(), n as usize);
        if n % 6 == 0 {
            assert!(!vol.recovery().ip_debt);
            assert_eq!(
                vol.recovery()
                    .ip
                    .last_index
                    .as_ref()
                    .unwrap()
                    .index
                    .previous_location,
                Some(vol.index().self_location)
            );
        }
    }
}

#[test]
fn checkpoint_limit_respects_smaller_recovery_budget_and_zero_disables_deltas() {
    for limit in [0, 1, 2] {
        let (_lib, dev) = formatted();
        let budget = RecoveryBudget {
            max_chain_len: limit,
            ..Default::default()
        };
        for n in 1..=6 {
            let mut vol = LtfsVolume::mount_with_budget(&dev, &budget).unwrap();
            vol.append_file(&format!("/f{n}"), &mut Cursor::new(b"x"))
                .unwrap();
            vol.commit_incremental().unwrap();
            assert_eq!(vol.index().incremental, n % (limit + 1) != 0);
        }
    }
}

#[test]
fn failed_periodic_checkpoint_freezes_without_publishing_new_generation() {
    let (_lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for n in 0..5 {
        vol.append_file(&format!("/f{n}"), &mut Cursor::new(b"x"))
            .unwrap();
        vol.commit_incremental().unwrap();
    }
    let previous = vol.index().generation;
    vol.append_file("/pending", &mut Cursor::new(b"x")).unwrap();
    dev.inject(Fault {
        opcode: opcode::WRITE_FILEMARKS_6,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0c,
            ascq: 0,
        },
        after: 2, // Full checkpoint 的 IP 开始处失败，DP 已完整。
        remaining: 1,
    });
    assert!(vol.commit_incremental().is_err());
    assert!(!vol.writable());
    assert_eq!(vol.index().generation, previous);
    assert!(vol.index().find_file("pending").is_none());
    drop(vol);
    let recovered = LtfsVolume::mount(&dev).unwrap();
    assert!(!recovered.index().incremental);
    assert!(recovered.index().find_file("pending").is_some());
}

#[test]
fn shutdown_drain_keeps_completed_queue_and_rejects_late_uploads() {
    use tape_rs::daemon::files::{FileService, ServiceError, TapeIdent, TapeLimits};
    let dir = std::env::temp_dir().join(format!("ltfs-drain-{}", uuid::Uuid::new_v4()));
    let files = FileService::new(dir.clone()).unwrap();
    let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
    files.open(
        1,
        TapeIdent {
            barcode: "TEST".into(),
            pool_uuid: "pool".into(),
        },
        TapeLimits {
            file_limit: 100,
            usable_capacity: 1024,
        },
        &index,
        1024,
        true,
    );
    let completed = files.begin("/complete", 1).unwrap();
    std::fs::write(&completed.spool, b"x").unwrap();
    files.ingest(&completed, 1).unwrap();
    let task = files.finish(completed, 1).unwrap();
    let late = files.begin("/late", 1).unwrap();
    std::fs::write(&late.spool, b"x").unwrap();
    files.ingest(&late, 1).unwrap();
    let late_spool = late.spool.clone();
    files.drain();
    assert!(matches!(
        files.begin("/new", 1),
        Err(ServiceError::NotServing(_))
    ));
    assert!(matches!(
        files.finish(late, 1),
        Err(ServiceError::NotServing(_))
    ));
    assert!(!late_spool.exists());
    let (_, uploads) = files.take_batch_now(1).unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].task, task);
    assert_eq!(uploads[0].path, "/complete");
    assert_eq!(std::fs::read(&uploads[0].spool).unwrap(), b"x");
    files.close("test completed");
    drop(files);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_rename_commit_recovers_a_whole_namespace_after_power_cut() {
    for after in [0, 1, 2] {
        let (lib, dev) = formatted();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/before/file", &mut Cursor::new(b"payload"))
            .unwrap();
        vol.create_directory("/before/empty").unwrap();
        vol.commit().unwrap();
        vol.rename_path("/before", "/after").unwrap();
        dev.inject(Fault {
            opcode: opcode::WRITE_FILEMARKS_6,
            action: FaultAction::CheckCondition {
                sense_key: 3,
                asc: 0x0c,
                ascq: 0,
            },
            after,
            remaining: 1,
        });
        assert!(vol.commit_incremental().is_err());
        assert!(vol.index().find_file("before/file").is_some());
        assert!(vol.index().find_directory("after").is_none());
        drop(vol);
        lib.power_cut();
        let vol = LtfsVolume::mount(&dev).unwrap();
        let index = vol.index();
        let moved = index.find_directory("after").is_some();
        assert_eq!(index.find_directory("before").is_some(), !moved);
        let base = if moved { "after" } else { "before" };
        assert!(index.find_directory(&format!("{base}/empty")).is_some());
        let mut data = Vec::new();
        vol.read_file_to_writer(&format!("{base}/file"), &mut data)
            .unwrap();
        assert_eq!(data, b"payload");
    }
}

#[test]
fn incremental_barrier_and_partial_vci_failures_recover_metadata_and_allow_next_commit() {
    use tape_rs::error::TapeError;
    use tape_rs::ltfs::index::Xattr;
    use tape_rs::ltfs::recovery::TailKind;
    use tape_rs::scsi::sim::SimQuirks;

    for constant_vcr in [false, true] {
        for power_cut in [false, true] {
            for (opcode, after, stage) in [
                (opcode::WRITE_FILEMARKS_6, 2, "barrier"),
                (opcode::WRITE_ATTRIBUTE, 0, "vci"),
                (opcode::WRITE_ATTRIBUTE, 1, "vci"),
            ] {
                let (lib, dev) = formatted();
                lib.set_quirks(SimQuirks {
                    vcr_never_changes: constant_vcr,
                    ..Default::default()
                });
                let mut vol = LtfsVolume::mount(&dev).unwrap();
                vol.append_file("/kept", &mut Cursor::new(b"unchanged"))
                    .unwrap();
                vol.set_node_xattr(
                    "/kept",
                    Xattr {
                        key: "remove".into(),
                        value: "old".into(),
                        base64: false,
                    },
                    true,
                    false,
                )
                .unwrap();
                vol.commit().unwrap();
                let original = vol.index().find_file("kept").unwrap().clone();
                vol.create_directory("/anchor").unwrap();
                vol.commit_incremental().unwrap();
                assert_eq!(vol.index().generation, 3);
                for (key, value) in [("binary", "AP8="), ("empty", "")] {
                    vol.set_node_xattr(
                        "/kept",
                        Xattr {
                            key: key.into(),
                            value: value.into(),
                            base64: true,
                        },
                        true,
                        false,
                    )
                    .unwrap();
                }
                vol.remove_node_xattr("/kept", "remove").unwrap();
                dev.inject(Fault {
                    opcode,
                    action: FaultAction::CheckCondition {
                        sense_key: 3,
                        asc: 0x0c,
                        ascq: 0,
                    },
                    after,
                    remaining: 1,
                });
                let err = vol.commit_incremental().unwrap_err();
                assert!(matches!(err, TapeError::CommitFailed { stage: ref s, .. } if s == stage));
                assert!(!vol.writable());
                assert_eq!(vol.index().generation, 3);
                let published = vol.index().find_file("kept").unwrap();
                assert_eq!(published.xattr("remove"), Some("old"));
                assert_eq!(published.xattr("binary"), None);
                assert!(matches!(
                    vol.create_directory("/blocked"),
                    Err(TapeError::RecoveryRestricted { .. })
                ));
                assert!(matches!(
                    vol.commit_incremental(),
                    Err(TapeError::RecoveryRestricted { .. })
                ));
                drop(vol);
                if power_cut {
                    lib.power_cut();
                }
                dev.clear_faults();
                let mut recovered = LtfsVolume::mount(&dev).unwrap();
                assert_eq!(recovered.index().generation, 4);
                assert_eq!(recovered.recovery().incremental_depth, 2);
                assert_eq!(recovered.recovery().dp.tail, TailKind::Complete);
                assert!(!recovered.recovery().dp.hint_used, "旧DP提示不得掩盖新增量");
                assert_eq!(
                    recovered.recovery().ip.hint_used,
                    constant_vcr || (stage == "vci" && after == 1)
                );
                assert!(recovered.writable());
                let file = recovered.index().find_file("kept").unwrap();
                assert_eq!(file.xattr("binary"), Some("AP8="));
                assert_eq!(file.xattr("empty"), Some(""));
                assert_eq!(file.xattr("remove"), None);
                assert_eq!(file.extents, original.extents);
                assert_eq!(file.meta.modify_time, original.meta.modify_time);
                let mut data = Vec::new();
                recovered.read_file_to_writer("/kept", &mut data).unwrap();
                assert_eq!(data, b"unchanged");
                recovered.create_directory("/after-recovery").unwrap();
                recovered.commit_incremental().unwrap();
                assert_eq!(recovered.index().generation, 5);
                recovered.unmount().unwrap();
                let final_view = LtfsVolume::mount(&dev).unwrap();
                assert!(!final_view.index().incremental);
                assert!(final_view.index().find_directory("/anchor").is_some());
                assert!(
                    final_view
                        .index()
                        .find_directory("/after-recovery")
                        .is_some()
                );
                assert_eq!(
                    final_view.index().find_file("kept").unwrap().xattr("empty"),
                    Some("")
                );
            }
        }
    }
}
