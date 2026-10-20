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
    assert!(!vol.recovery().history_checked);
    assert_eq!(vol.recovery().incremental_depth, 2);
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
fn clean_full_after_incrementals_mounts_without_historical_scan() {
    let (_lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/one", &mut Cursor::new(b"1")).unwrap();
    vol.commit_incremental().unwrap();
    let delta = vol.index().self_location;
    vol.unmount().unwrap();

    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.index().incremental);
    assert_eq!(vol.index().previous_incremental_location, Some(delta));
    assert!(!vol.recovery().ip_debt);
    assert!(vol.recovery().dp.hint_used && vol.recovery().ip.hint_used);
    assert!(!vol.recovery().history_checked);
    let mut data = Vec::new();
    vol.read_file_to_writer("/one", &mut data).unwrap();
    assert_eq!(data, b"1");
    drop(vol);

    let audited = LtfsVolume::mount_with_budget(&dev, &RecoveryBudget::default()).unwrap();
    assert!(audited.recovery().history_checked);
    assert!(audited.recovery().chain_ok);
    assert_eq!(audited.recovery().chain_depth, 1); // 显式核验仍读取前一份 Full
}

#[test]
fn incremental_replay_keeps_unknown_full_elements_read_only() {
    use tape_rs::error::TapeError;
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/baseline", &mut Cursor::new(b"base"))
        .unwrap();
    vol.commit().unwrap();
    for path in ["/first", "/second"] {
        vol.append_file(path, &mut Cursor::new(b"delta")).unwrap();
        vol.commit_incremental().unwrap();
    }
    drop(vol);
    lib.with_cartridge_mut("DEL001L8", |cart| {
        for partition in &mut cart.partitions {
            for object in &mut partition.objects {
                if let LogicalObject::Record(record) = object {
                    let xml = String::from_utf8_lossy(record);
                    if xml.contains("<ltfsindex") {
                        *record = xml
                            .replace(
                                "</ltfsindex>",
                                "<futurefeature/><anotherfeature/><futurefeature/></ltfsindex>",
                            )
                            .into_bytes();
                    }
                }
            }
        }
    });

    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.writable(), "合法增量不能清除 Full 基线的只读限制");
    assert_eq!(
        vol.index().unknown_elements,
        ["futurefeature", "anotherfeature"]
    );
    let mut out = Vec::new();
    vol.read_file_to_writer("/second", &mut out).unwrap();
    assert_eq!(out, b"delta");
    assert!(matches!(
        vol.append_file("/refused", &mut Cursor::new(b"no")),
        Err(TapeError::RecoveryRestricted { .. })
    ));
}

#[test]
fn apply_incremental_keeps_unknown_elements_union_across_replays() {
    let mut base = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
    let mut next = base.clone();
    next.generation = 2;
    next.self_location.start_block = 10;
    let first = next.incremental_since(&base).unwrap();
    let applied = base.apply_incremental(&first).unwrap();
    let mut next = applied.clone();
    next.generation = 3;
    next.self_location.start_block = 20;
    let second = next.incremental_since(&applied).unwrap();

    base.unknown_elements = vec!["futurefeature".into(), "anotherfeature".into()];
    let applied = base.apply_incremental(&first).unwrap();
    let applied = applied.apply_incremental(&second).unwrap();
    assert_eq!(
        applied.unknown_elements,
        ["futurefeature", "anotherfeature"]
    );
    assert_eq!(base.unknown_elements, ["futurefeature", "anotherfeature"]);
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
            assert!(vol.recovery().ip_debt, "周期Full只写DP，不应来回切换IP");
            assert!(vol.needs_checkpoint());
        }
    }
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.commit().unwrap();
    drop(vol);
    lib.power_cut();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.recovery().ip_debt);
    assert!(!vol.needs_checkpoint());
    assert_eq!(vol.list().len(), 13);
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
        after: 2, // 周期DP Full的最终屏障失败，闭FM已完成。
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

#[test]
fn adaptive_batch_uses_full_when_delta_saves_no_index_blocks() {
    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for n in 0..3 {
        vol.append_file(&format!("/batch/{n}"), &mut Cursor::new(b"payload"))
            .unwrap();
    }
    vol.sync_batch().unwrap();
    assert!(
        !vol.index().incremental,
        "同为一个索引块时应直接提交DP Full"
    );
    assert!(vol.needs_checkpoint(), "普通批次不切换IP");
    let generation = vol.index().generation;
    vol.sync_batch().unwrap();
    assert_eq!(vol.index().generation, generation, "干净同步不制造新索引");
    drop(vol);
    lib.power_cut();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.index().incremental);
    for n in 0..3 {
        let mut out = Vec::new();
        vol.read_file_to_writer(&format!("/batch/{n}"), &mut out)
            .unwrap();
        assert_eq!(out, b"payload");
    }
    vol.unmount().unwrap();
    let clean = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(
        clean.index().generation,
        generation,
        "已有DP Full只需补同代IP"
    );
    assert!(!clean.needs_checkpoint());
}

fn large_baseline(vol: &mut LtfsVolume<'_>) {
    for n in 0..160 {
        vol.append_file(&format!("/baseline/{n}"), &mut Cursor::new(b"kept"))
            .unwrap();
    }
    vol.commit().unwrap();
    assert!(vol.index().to_xml().unwrap().len() > 2 * 65536);
}

#[test]
fn adaptive_batch_preserves_smaller_deltas_and_bounds_chain_across_remounts() {
    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    large_baseline(&mut vol);
    drop(vol);
    let budget = RecoveryBudget {
        max_chain_len: 2,
        ..Default::default()
    };
    for n in 1..=4 {
        let mut vol = LtfsVolume::mount_with_budget(&dev, &budget).unwrap();
        vol.append_file(&format!("/batch/{n}"), &mut Cursor::new([n as u8]))
            .unwrap();
        vol.sync_batch().unwrap();
        assert_eq!(vol.index().incremental, n % 3 != 0);
        assert!(vol.needs_checkpoint());
        drop(vol);
        lib.power_cut();
        let vol = LtfsVolume::mount_with_budget(&dev, &budget).unwrap();
        assert_eq!(vol.recovery().incremental_depth, n % 3);
        let mut out = Vec::new();
        vol.read_file_to_writer(&format!("/batch/{n}"), &mut out)
            .unwrap();
        assert_eq!(out, [n as u8]);
    }
    LtfsVolume::mount(&dev).unwrap().unmount().unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.index().incremental && !vol.needs_checkpoint());
    let mut out = Vec::new();
    vol.read_file_to_writer("/baseline/0", &mut out).unwrap();
    assert_eq!(out, b"kept");
}

#[test]
fn adaptive_batch_barrier_failure_never_acknowledges_full_or_delta() {
    for large in [false, true] {
        let (lib, dev) = formatted();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        if large {
            large_baseline(&mut vol);
        }
        let generation = vol.index().generation;
        vol.append_file("/pending", &mut Cursor::new(b"pending"))
            .unwrap();
        dev.inject(Fault {
            opcode: opcode::WRITE_FILEMARKS_6,
            action: FaultAction::CheckCondition {
                sense_key: 3,
                asc: 0x0c,
                ascq: 0,
            },
            after: 2,
            remaining: 1,
        });
        assert!(vol.sync_batch().is_err());
        assert_eq!(vol.index().generation, generation);
        assert!(vol.index().find_file("/pending").is_none());
        assert!(!vol.writable());
        drop(vol);
        lib.power_cut();
        let recovered = LtfsVolume::mount(&dev).unwrap();
        assert_eq!(recovered.index().incremental, large);
        let mut out = Vec::new();
        recovered.read_file_to_writer("/pending", &mut out).unwrap();
        assert_eq!(out, b"pending");
    }
}

/// Full 已跨两个块时，真实 sync_batch 会选择增量；接管只依赖最近 Full 和其后增量。
#[test]
fn batch_incremental_takeover_stops_at_its_full_baseline() {
    let (lib, dev) = formatted();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.set_root_xattr("fixture.padding", &"x".repeat(65536))
        .unwrap();
    for i in 0..40 {
        vol.append_file(&format!("/base-{i}"), &mut Cursor::new(b"base"))
            .unwrap();
        vol.sync().unwrap();
    }
    for i in 0..3 {
        vol.append_file(&format!("/delta-{i}"), &mut Cursor::new(b"delta"))
            .unwrap();
        vol.sync_batch().unwrap();
        assert!(vol.index().incremental, "必须覆盖真实批次增量提交路径");
    }
    let generation = vol.index().generation;
    drop(vol);
    lib.power_cut();
    let before = dev.command_log().len();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let locates = dev.command_log()[before..]
        .iter()
        .filter(|&&op| op == opcode::LOCATE_16)
        .count();
    eprintln!(
        "locates={locates}, history={}, chain_depth={}, replay={}",
        vol.recovery().history_checked,
        vol.recovery().chain_depth,
        vol.recovery().incremental_depth
    );
    assert_eq!(vol.index().generation, generation);
    assert_eq!(vol.recovery().incremental_depth, 3);
    assert!(vol.writable() && vol.recovery().ip_debt);
    assert!(
        locates <= 12,
        "不能在完整重放所需增量后又逐代核验旧 Full：{locates}"
    );
    assert_eq!(
        vol.index()
            .root
            .xattrs
            .iter()
            .find(|x| x.key == "fixture.padding")
            .unwrap()
            .value
            .len(),
        65536
    );
    for (prefix, count, expected) in [
        ("base", 40, b"base".as_slice()),
        ("delta", 3, b"delta".as_slice()),
    ] {
        for i in 0..count {
            let mut bytes = Vec::new();
            vol.read_file_to_writer(&format!("/{prefix}-{i}"), &mut bytes)
                .unwrap();
            assert_eq!(bytes, expected);
        }
    }
}

#[test]
fn incremental_takeover_requires_its_full_baseline_but_not_older_history() {
    for corrupt_baseline in [false, true] {
        let (lib, dev) = formatted();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/old", &mut Cursor::new(b"old")).unwrap();
        vol.sync().unwrap();
        let old = vol.index().self_location.start_block;
        vol.append_file("/base", &mut Cursor::new(b"base")).unwrap();
        vol.sync().unwrap();
        let baseline = vol.index().self_location.start_block;
        vol.append_file("/delta", &mut Cursor::new(b"delta"))
            .unwrap();
        vol.commit_incremental().unwrap();
        drop(vol);
        lib.with_cartridge_mut("DEL001L8", |c| {
            let block = if corrupt_baseline { baseline } else { old };
            c.partitions[1].objects[block as usize] =
                tape_rs::scsi::sim::LogicalObject::Record(b"corrupt Full".to_vec());
        });
        if corrupt_baseline {
            assert!(
                LtfsVolume::mount(&dev).is_err(),
                "当前视图所需 Full 损坏不能开放"
            );
        } else {
            let vol = LtfsVolume::mount(&dev).unwrap();
            assert!(vol.writable() && !vol.recovery().history_checked);
            assert_eq!(vol.recovery().incremental_depth, 1);
            for (path, expected) in [
                ("/old", b"old".as_slice()),
                ("/base", b"base"),
                ("/delta", b"delta"),
            ] {
                let mut bytes = Vec::new();
                vol.read_file_to_writer(path, &mut bytes).unwrap();
                assert_eq!(bytes, expected);
            }
            drop(vol);
            assert!(
                LtfsVolume::mount_with_budget(&dev, &RecoveryBudget::default()).is_err(),
                "显式审计仍须发现旧 Full 损坏"
            );
        }
    }
}

#[test]
fn incremental_takeover_with_mismatched_vci_keeps_history_audit() {
    for partition in [0, 1] {
        let (_lib, dev) = formatted();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/first", &mut Cursor::new(b"first"))
            .unwrap();
        vol.sync().unwrap();
        vol.append_file("/delta", &mut Cursor::new(b"delta"))
            .unwrap();
        vol.commit_incremental().unwrap();
        drop(vol);
        let mam = tape_rs::ltfs::mam::Mam::with_partition(&dev, partition);
        let mut vci = mam.read_vci().unwrap().unwrap();
        vci.generation += 1;
        mam.write_vci(&vci).unwrap();
        let vol = LtfsVolume::mount(&dev).unwrap();
        assert!(vol.recovery().history_checked);
        assert_eq!(vol.recovery().incremental_depth, 1);
        assert!(vol.writable());
    }
}
