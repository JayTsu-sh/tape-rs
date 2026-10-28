//! D02 尾部协议在 SimTransport 上的验证（对应 recovery-tail-protocol.md 的 TP01—TP08）。

use std::io::Cursor;

use tape_rs::error::TapeError;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::{RecoveryBudget, TailKind};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::sim::{
    Fault, FaultAction, LogicalObject, SimCartridge, SimLibrary, SimTransport,
};

const BARCODE: &str = "REC001L8";

fn setup() -> (SimLibrary, SimTransport) {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive(0);
    let opts = MkltfsOptions {
        volume_id: "REC001".into(),
        block_size: 64 * 1024,
        ..Default::default()
    };
    mkltfs(&dev, &opts).unwrap();
    (lib, dev)
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn commit_file(dev: &SimTransport, path: &str, seed: u8) -> u64 {
    let mut vol = LtfsVolume::mount(dev).unwrap();
    vol.append_file(path, &mut Cursor::new(payload(50_000, seed)))
        .unwrap();
    vol.commit().unwrap();
    vol.index().generation
}

fn mount_err(dev: &SimTransport, budget: &RecoveryBudget) -> TapeError {
    match LtfsVolume::mount_with_budget(dev, budget) {
        Ok(_) => panic!("mount 应当失败"),
        Err(e) => e,
    }
}

fn names(vol: &LtfsVolume<'_>) -> Vec<String> {
    vol.list().into_iter().map(|(p, _)| p).collect()
}

/// 把 DP（partitions[1]）的缓冲全部视为已落带，便于构造尾部。
fn flush_all(lib: &SimLibrary) {
    lib.with_cartridge_mut(BARCODE, |c| {
        for p in c.partitions.iter_mut() {
            p.flushed = p.objects.len();
        }
    });
}

#[test]
fn tp01_fresh_volume_is_complete_and_writable() {
    let (_lib, dev) = setup();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::Complete);
    assert_eq!(r.ip.tail, TailKind::Complete);
    assert!(r.chain_ok);
    assert!(vol.writable());
    assert_eq!(vol.index().generation, 1);
    assert_eq!(r.append_position, Some(r.dp.eod));
}

#[test]
fn tp01_after_commits_chain_is_verified() {
    let (_lib, dev) = setup();
    assert_eq!(commit_file(&dev, "/a", 1), 2);
    assert_eq!(commit_file(&dev, "/b", 2), 3);
    let vol = LtfsVolume::mount_with_budget(&dev, &RecoveryBudget::default()).unwrap();
    let r = vol.recovery();
    assert_eq!(vol.index().generation, 3);
    assert_eq!(r.chain_depth, 2, "gen3 → gen2 → gen1");
    assert!(r.chain_ok && !r.ip_debt && vol.writable());
    assert_eq!(names(&vol).len(), 2);
}

/// gen13 样式的完整卷上，对照旧 SPACE EOD 与挂载使用的 LOCATE EOD。
/// SimTransport 将两种 EOD 定位建模为同一位置；此测试只验证模拟器路径一致，
/// 不代表 IBM 驱动器的 LOCATE(16) DEST_TYPE=EOD 语义已被验证。
#[test]
fn p1_full_generation_13_direct_eod_matches_legacy_scan() {
    let (_lib, dev) = setup();
    for generation in 2..=13 {
        assert_eq!(
            commit_file(&dev, &format!("/f{generation}"), generation as u8),
            generation
        );
    }

    let drive = tape_rs::tape::commands::TapeDrive::new(&dev);
    let mut legacy_eods = [0u64; 2];
    for partition in 0..=1 {
        drive.locate(partition, 0, true).unwrap();
        drive.space_to_eod().unwrap();
        let pos = drive.read_position().unwrap();
        assert_eq!(pos.partition, u32::from(partition));
        legacy_eods[partition as usize] = pos.block_number;
    }

    let vol = LtfsVolume::mount(&dev).unwrap();
    let report = vol.recovery();
    assert_eq!(vol.index().generation, 13);
    assert_eq!(report.dp.eod, legacy_eods[1]);
    assert_eq!(report.ip.eod, legacy_eods[0]);
    assert_eq!(report.dp.tail, TailKind::Complete, "{:?}", report.dp.notes);
    assert_eq!(report.ip.tail, TailKind::Complete, "{:?}", report.ip.notes);
}

#[test]
fn tp05_unindexed_data_tail_is_read_only() {
    let (lib, dev) = setup();
    commit_file(&dev, "/committed", 1);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        // 显式刷出首个未索引文件，第二个文件仍在缓冲；然后掉电。
        vol.append_file("/lost1", &mut Cursor::new(payload(30_000, 2)))
            .unwrap();
        // 构造已落带但未发布的尾部，不依赖追加过程中额外 LOCATE 的副作用。
        tape_rs::tape::commands::TapeDrive::new(&dev)
            .write_filemark(0)
            .unwrap();
        vol.append_file("/lost2", &mut Cursor::new(payload(30_000, 3)))
            .unwrap();
    }
    lib.power_cut();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::UnindexedData);
    assert!(!vol.writable());
    assert_eq!(names(&vol), vec!["committed".to_string()]);
    let mut v = vol;
    let e = v
        .append_file("/x", &mut Cursor::new(payload(10, 9)))
        .unwrap_err();
    assert!(matches!(e, TapeError::RecoveryRestricted { .. }), "{e}");
}

#[test]
fn tp04_index_interrupted_before_closing_filemark() {
    let (lib, dev) = setup();
    commit_file(&dev, "/first", 1);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/second", &mut Cursor::new(payload(20_000, 2)))
            .unwrap();
        // commit 里第 1 次 WRITE FILEMARKS 是开头 FM，第 2 次是索引的结束 FM：让后者失败
        dev.inject(Fault {
            opcode: opcode::WRITE_FILEMARKS_6,
            action: FaultAction::CheckCondition {
                sense_key: 0x03,
                asc: 0x0C,
                ascq: 0x00,
            },
            after: 1,
            remaining: 1,
        });
        let e = vol.commit().unwrap_err();
        assert!(
            matches!(e, TapeError::CommitFailed { ref stage, .. } if stage == "closing_fm"),
            "{e}"
        );
    }
    flush_all(&lib);
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert_eq!(
        r.dp.tail,
        TailKind::TruncatedIndex,
        "notes = {:?}",
        r.dp.notes
    );
    assert_eq!(vol.index().generation, 2, "末有效索引仍是第一次 commit");
    assert_eq!(names(&vol), vec!["first".to_string()]);
    assert!(!vol.writable());
}

#[test]
fn tp03_fake_index_with_wrong_self_pointer() {
    let (lib, dev) = setup();
    commit_file(&dev, "/first", 1);
    // 取 DP 上最新索引的 XML，复制一份到尾部形成结构完整但自指针不符的构造
    let xml = {
        let c = lib.cartridge(BARCODE).unwrap();
        let recs: Vec<&Vec<u8>> = c.partitions[1]
            .objects
            .iter()
            .filter_map(|o| match o {
                LogicalObject::Record(d) if d.starts_with(b"<?xml") => Some(d),
                _ => None,
            })
            .collect();
        recs.last().unwrap().to_vec()
    };
    lib.with_cartridge_mut(BARCODE, |c| {
        let p = &mut c.partitions[1];
        p.objects.push(LogicalObject::Filemark);
        p.objects.push(LogicalObject::Record(xml));
        p.objects.push(LogicalObject::Filemark);
        p.flushed = p.objects.len();
        c.vcr += 1; // 绕过驱动器改介质：手动模拟 VCR 变化
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::FakeIndex, "notes = {:?}", r.dp.notes);
    assert_eq!(
        r.dp.back_steps, 3,
        "假索引、相邻文件标记间的空区域、真索引各一步"
    );
    assert_eq!(vol.index().generation, 2);
    assert!(!vol.writable());
}

#[test]
fn tp06_broken_chain_is_restricted() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    commit_file(&dev, "/b", 2);
    // 破坏 DP 上代数 2 的索引记录
    lib.with_cartridge_mut(BARCODE, |c| {
        for o in c.partitions[1].objects.iter_mut() {
            if let LogicalObject::Record(d) = o
                && d.windows(20).any(|w| w == b"<generationnumber>2<")
            {
                *d = b"garbage".to_vec();
            }
        }
    });
    let e = mount_err(&dev, &RecoveryBudget::default());
    assert!(matches!(e, TapeError::RecoveryRestricted { .. }), "{e}");
}

#[test]
fn tp07_ip_lagging_dp_is_debt_not_error() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    let ip_gen2 = lib.cartridge(BARCODE).unwrap().partitions[0].clone();
    commit_file(&dev, "/b", 2);
    lib.with_cartridge_mut(BARCODE, |c| c.partitions[0] = ip_gen2);
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(r.ip_debt);
    assert!(r.history_checked);
    assert_eq!(vol.index().generation, 3, "视图来自 DP");
    assert_eq!(r.ip.last_index.as_ref().unwrap().index.generation, 2);
    assert!(vol.writable());
}

#[test]
fn ip_newer_than_dp_is_a_conflict() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    let dp_gen2 = lib.cartridge(BARCODE).unwrap().partitions[1].clone();
    commit_file(&dev, "/b", 2);
    lib.with_cartridge_mut(BARCODE, |c| c.partitions[1] = dp_gen2);
    let e = mount_err(&dev, &RecoveryBudget::default());
    assert!(matches!(e, TapeError::RecoveryRestricted { .. }), "{e}");
}

#[test]
fn tp08_locate_failure_at_append_position_blocks_write() {
    let (_lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    // 恢复协议末尾的 LOCATE(A) 失败：mount 成功但不可写
    dev.inject(Fault {
        opcode: opcode::LOCATE_16,
        action: FaultAction::CheckCondition {
            sense_key: 0x04,
            asc: 0x44,
            ascq: 0x00,
        },
        after: 100,
        remaining: 0,
    });
    // 先统计一次 mount 内部发出的 LOCATE 次数，再让最后一次（追加位置复核）失败
    dev.clear_faults();
    let before = dev
        .command_log()
        .iter()
        .filter(|&&o| o == opcode::LOCATE_16)
        .count();
    {
        let _vol = LtfsVolume::mount_with_budget(&dev, &RecoveryBudget::default()).unwrap();
    }
    let n = dev
        .command_log()
        .iter()
        .filter(|&&o| o == opcode::LOCATE_16)
        .count()
        - before;
    dev.inject(Fault {
        opcode: opcode::LOCATE_16,
        action: FaultAction::CheckCondition {
            sense_key: 0x04,
            asc: 0x44,
            ascq: 0x00,
        },
        after: (n - 1) as u32,
        remaining: 1,
    });
    let e = mount_err(&dev, &RecoveryBudget::default());
    assert!(
        matches!(
            e,
            TapeError::ScsiCommand {
                sense_key: 0x04,
                ..
            }
        ),
        "{e}"
    );
}

#[test]
fn budget_exhausted_without_index_is_restricted() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    // 在尾部堆两个假构造，预算只允许 1 步
    lib.with_cartridge_mut(BARCODE, |c| {
        let p = &mut c.partitions[1];
        for _ in 0..2 {
            p.objects.push(LogicalObject::Filemark);
            p.objects
                .push(LogicalObject::Record(b"not an index".to_vec()));
            p.objects.push(LogicalObject::Filemark);
        }
        p.flushed = p.objects.len();
        c.vcr += 1;
    });
    let budget = RecoveryBudget {
        max_back_steps: 1,
        ..Default::default()
    };
    let e = mount_err(&dev, &budget);
    assert!(matches!(e, TapeError::RecoveryRestricted { .. }), "{e}");
    // 预算足够时能越过假构造找到真索引，且尾部为 FakeIndex
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(vol.recovery().dp.tail, TailKind::FakeIndex);
    assert!(!vol.writable());
}

#[test]
fn tp01_vci_fast_path_hits_after_commit() {
    let (_lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    commit_file(&dev, "/b", 2);
    let before = dev.command_log().len();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(
        r.dp.hint_used && r.ip.hint_used,
        "notes = {:?} / {:?}",
        r.dp.notes,
        r.ip.notes
    );
    assert_eq!((r.dp.back_steps, r.ip.back_steps), (0, 0));
    assert_eq!(vol.index().generation, 3);
    assert!(vol.writable());
    // 快速路径不做反向 SPACE：本次 mount 内 SPACE 只应出现在 EOD 定位与尾部分类
    let log = &dev.command_log()[before..];
    let spaces = log.iter().filter(|&&o| o == opcode::SPACE_6).count();
    assert!(spaces <= 4, "SPACE 次数 {}", spaces);
}

#[test]
fn tp02_stale_vci_falls_back_to_standard_path() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/lost1", &mut Cursor::new(payload(30_000, 2)))
            .unwrap();
        // 构造已落带但未发布的尾部，不依赖追加过程中额外 LOCATE 的副作用。
        tape_rs::tape::commands::TapeDrive::new(&dev)
            .write_filemark(0)
            .unwrap();
        vol.append_file("/lost2", &mut Cursor::new(payload(30_000, 3)))
            .unwrap();
    }
    lib.power_cut();
    // 写入使介质 VCR 变化，两分区 VCI 都过期；标准路径仍找到 gen 2，尾部 T1
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(!r.dp.hint_used && !r.ip.hint_used);
    assert!(r.history_checked);
    assert!(
        r.notes.iter().any(|n| n.contains("过期")),
        "notes = {:?}",
        r.notes
    );
    assert_eq!(vol.index().generation, 2);
    assert_eq!(r.dp.tail, TailKind::UnindexedData);
    assert!(!vol.writable());
}

#[test]
fn vci_pointing_at_wrong_block_is_rejected_then_standard_path_succeeds() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    // 把 DP 的 VCI block 改成数据块位置（自指针必然不符），VCR 保持新鲜
    lib.with_cartridge_mut(BARCODE, |c| {
        if let Some((_, val)) = c.mam.get_mut(&(1u8, 0x080C)) {
            let vcr_len = val[0] as usize;
            let off = 1 + vcr_len + 8;
            val[off..off + 8].copy_from_slice(&5u64.to_be_bytes());
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(
        !r.dp.hint_used && r.ip.hint_used,
        "dp notes = {:?}",
        r.dp.notes
    );
    assert!(
        r.dp.notes.iter().any(|n| n.contains("VCI 提示")),
        "{:?}",
        r.dp.notes
    );
    assert_eq!(vol.index().generation, 2);
    assert!(vol.writable());
}

#[test]
fn mam_vcr_is_read_only_and_changes_on_write() {
    use tape_rs::ltfs::mam::{ATTR_VCR, AttributeFormat, Mam, MamAttribute};
    let (_lib, dev) = setup();
    let mam = Mam::new(&dev);
    let v1 = mam.read_vcr().unwrap().expect("sim 提供 VCR");
    commit_file(&dev, "/a", 1);
    let v2 = mam.read_vcr().unwrap().unwrap();
    assert_ne!(v1, v2);
    let e = mam
        .write_attribute(&MamAttribute {
            id: ATTR_VCR,
            format: AttributeFormat::Binary as u8,
            value: v2,
        })
        .unwrap_err();
    assert!(matches!(
        e,
        TapeError::ScsiCommand {
            sense_key: 0x05,
            ..
        }
    ));
}

/// 未修复的 Holo-VTL 偏差下，未索引数据尾仍被归为 T1 只读，而不是报错。
#[test]
fn tp05_under_holo_quirks_still_classifies_unindexed_tail() {
    use tape_rs::scsi::sim::SimQuirks;
    let (lib, dev) = setup();
    lib.set_quirks(SimQuirks::holo_vtl());
    commit_file(&dev, "/committed", 1);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/lost1", &mut Cursor::new(payload(30_000, 2)))
            .unwrap();
        // 构造已落带但未发布的尾部，不依赖追加过程中额外 LOCATE 的副作用。
        tape_rs::tape::commands::TapeDrive::new(&dev)
            .write_filemark(0)
            .unwrap();
        vol.append_file("/lost2", &mut Cursor::new(payload(30_000, 3)))
            .unwrap();
    }
    lib.power_cut();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(
        vol.recovery().dp.tail,
        TailKind::UnindexedData,
        "{:?}",
        vol.recovery().dp.notes
    );
    assert!(!vol.writable());
    assert_eq!(names(&vol), vec!["committed".to_string()]);
}

/// IBM LTFS 在只改元数据、或挂载时补写 IP 后留下的状态：IP 比 DP 新一代，
/// 且 IP 的回指针正好指向 DP 末索引。这是一致卷，视图取 IP，允许继续追加。
#[test]
fn ip_ahead_with_back_pointer_to_dp_last_is_consistent() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    lib.with_cartridge_mut(BARCODE, |c| {
        for o in c.partitions[0].objects.iter_mut() {
            if let LogicalObject::Record(d) = o {
                let text = String::from_utf8_lossy(d).to_string();
                if text.contains("<ltfsindex") {
                    assert!(text.contains("<generationnumber>2</generationnumber>"));
                    *d = text
                        .replace(
                            "<generationnumber>2</generationnumber>",
                            "<generationnumber>3</generationnumber>",
                        )
                        .into_bytes();
                }
            }
        }
        c.vcr += 1;
    });
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        assert!(vol.recovery().ip_ahead());
        assert_eq!(vol.index().generation, 3, "视图取 IP");
        assert!(vol.writable(), "{:?}", vol.recovery().notes);
        vol.append_file("/b", &mut Cursor::new(payload(1000, 9)))
            .unwrap();
        vol.commit().unwrap();
        assert_eq!(vol.index().generation, 4);
    }
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(!r.ip_ahead() && !r.ip_debt && r.chain_ok);
    assert_eq!(names(&vol).len(), 2);
    assert!(vol.writable());
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_rejects_an_ip_view_newer_than_its_observed_dp_head() {
    use tape_rs::ltfs::recovery::verify_ip_not_ahead;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (lib, dev) = setup();
    let dp_generation = commit_file(&dev, "/a", 1);
    let label = read_label_only(&dev).unwrap();
    let drive = TapeDrive::new(&dev);
    assert_eq!(
        verify_ip_not_ahead(&drive, &label, dp_generation, &RecoveryBudget::default()).unwrap(),
        dp_generation
    );
    lib.with_cartridge_mut(BARCODE, |c| {
        let before = format!("<generationnumber>{dp_generation}</generationnumber>");
        let after = format!("<generationnumber>{}</generationnumber>", dp_generation + 1);
        let mut changed = false;
        for object in &mut c.partitions[0].objects {
            if let LogicalObject::Record(record) = object {
                let text = String::from_utf8_lossy(record);
                if text.contains("<ltfsindex") {
                    assert!(text.contains(&before));
                    *record = text.replace(&before, &after).into_bytes();
                    changed = true;
                }
            }
        }
        assert!(changed);
        c.vcr += 1;
    });
    let error =
        verify_ip_not_ahead(&drive, &label, dp_generation, &RecoveryBudget::default()).unwrap_err();
    assert!(error.to_string().contains("领先"), "{error}");
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_rejects_unindexed_ip_tail() {
    use tape_rs::ltfs::recovery::verify_ip_not_ahead;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (lib, dev) = setup();
    let dp_generation = commit_file(&dev, "/a", 1);
    let label = read_label_only(&dev).unwrap();
    lib.with_cartridge_mut(BARCODE, |cartridge| {
        let ip = &mut cartridge.partitions[0];
        ip.objects
            .push(LogicalObject::Record(b"unindexed IP tail".to_vec()));
        ip.flushed = ip.objects.len();
    });
    let error = verify_ip_not_ahead(
        &TapeDrive::new(&dev),
        &label,
        dp_generation,
        &RecoveryBudget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("尾部不完整"), "{error}");
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_rejects_same_generation_ip_content_or_header_conflicts() {
    use tape_rs::ltfs::recovery::verify_dp_ip_head;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    for (before, after) in [
        ("<length>50000</length>", "<length>50001</length>"),
        ("<readonly>false</readonly>", "<readonly>true</readonly>"),
        (
            "<highestfileuid>2</highestfileuid>",
            "<highestfileuid>3</highestfileuid>",
        ),
        ("<fileoffset>0</fileoffset>", "<fileoffset>1</fileoffset>"),
    ] {
        let (lib, dev) = setup();
        commit_file(&dev, "/a", 1);
        let head = LtfsVolume::mount(&dev).unwrap().committed_index_construct();
        let label = read_label_only(&dev).unwrap();
        let drive = TapeDrive::new(&dev);
        assert_eq!(
            verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default()).unwrap(),
            head.generation
        );
        lib.with_cartridge_mut(BARCODE, |cartridge| {
            let mut changed = false;
            for object in &mut cartridge.partitions[0].objects {
                if let LogicalObject::Record(record) = object {
                    let text = String::from_utf8_lossy(record);
                    if text.contains(before) {
                        *record = text.replace(before, after).into_bytes();
                        changed = true;
                    }
                }
            }
            assert!(changed, "fixture missing {before}");
        });
        let error = verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default())
            .expect_err("同代 IP 变化不得凭 DP 摘要放行");
        assert!(error.to_string().contains("同代"), "{error}");
    }
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_accepts_lagging_ip_but_gates_same_generation_incremental_view() {
    use tape_rs::ltfs::recovery::verify_dp_ip_head;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (lib, dev) = setup();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/a", &mut Cursor::new(payload(50_000, 1)))
        .unwrap();
    volume.commit_incremental().unwrap();
    let head = volume.committed_index_construct();
    assert!(head.incremental);
    drop(volume);
    let label = read_label_only(&dev).unwrap();
    let drive = TapeDrive::new(&dev);
    assert!(
        verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default()).unwrap()
            < head.generation
    );
    let mut wrong_identity = head;
    wrong_identity.generation += 1;
    let error =
        verify_dp_ip_head(&drive, &label, wrong_identity, &RecoveryBudget::default()).unwrap_err();
    assert!(error.to_string().contains("身份"), "{error}");
    lib.with_cartridge_mut(BARCODE, |cartridge| {
        for object in &mut cartridge.partitions[0].objects {
            if let LogicalObject::Record(record) = object {
                let text = String::from_utf8_lossy(record);
                if text.contains("<ltfsindex") {
                    *record = text
                        .replace(
                            "<generationnumber>1</generationnumber>",
                            &format!("<generationnumber>{}</generationnumber>", head.generation),
                        )
                        .into_bytes();
                }
            }
        }
    });
    let error = verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default()).unwrap_err();
    assert!(error.to_string().contains("同代"), "{error}");
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_replays_delta_chain_for_same_generation_ip_read_view() {
    use tape_rs::ltfs::index::IndexLocation;
    use tape_rs::ltfs::recovery::verify_dp_ip_head;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (lib, dev) = setup();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/baseline", &mut Cursor::new(b"kept"))
        .unwrap();
    volume.commit().unwrap();
    volume
        .append_file("/dir/a", &mut Cursor::new(b"one"))
        .unwrap();
    volume.commit_incremental().unwrap();
    volume
        .append_file("/dir/b", &mut Cursor::new(b"two"))
        .unwrap();
    volume.commit_incremental().unwrap();
    volume.remove_file("/dir/a").unwrap();
    volume.rename_path("/dir/b", "/moved").unwrap();
    volume
        .set_node_xattr(
            "/moved",
            tape_rs::ltfs::index::Xattr {
                key: "source".into(),
                value: "disk replay".into(),
                base64: false,
            },
            false,
            false,
        )
        .unwrap();
    volume.commit_incremental().unwrap();
    let head = volume.committed_index_construct();
    let mut ip = volume.index().clone();
    ip.incremental = false;
    ip.previous_incremental_location = Some(head.location);
    ip.self_location = IndexLocation {
        partition: 'a',
        start_block: 5,
    };
    let xml = ip.to_xml().unwrap();
    drop(volume);
    lib.with_cartridge_mut(BARCODE, |cartridge| {
        let partition = &mut cartridge.partitions[0];
        partition.objects.truncate(4);
        partition.objects.push(LogicalObject::Filemark);
        for record in xml.chunks(64 * 1024) {
            partition
                .objects
                .push(LogicalObject::Record(record.to_vec()));
        }
        partition.objects.push(LogicalObject::Filemark);
        partition.flushed = partition.objects.len();
    });
    let label = read_label_only(&dev).unwrap();
    let drive = TapeDrive::new(&dev);
    assert_eq!(
        verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default()).unwrap(),
        head.generation
    );
    let error = verify_dp_ip_head(
        &drive,
        &label,
        head,
        &RecoveryBudget {
            max_chain_len: 1,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("预算"), "{error}");
    // A complete, parseable baseline with changed metadata must disagree with
    // the materialized IP view. The latest delta's exact hash stays unchanged.
    lib.with_cartridge_mut(BARCODE, |cartridge| {
        for object in &mut cartridge.partitions[1].objects {
            if let LogicalObject::Record(record) = object {
                let text = String::from_utf8_lossy(record);
                if text.contains("<ltfsindex") && text.contains("<name>baseline</name>") {
                    *record = text
                        .replace("<readonly>false</readonly>", "<readonly>true</readonly>")
                        .into_bytes();
                    break;
                }
            }
        }
    });
    let error = verify_dp_ip_head(&drive, &label, head, &RecoveryBudget::default()).unwrap_err();
    assert!(error.to_string().contains("同代"), "{error}");
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_accepts_full_checkpoint_with_a_historical_delta_back_pointer() {
    use tape_rs::ltfs::recovery::verify_dp_ip_head;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (_library, device) = setup();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.append_file("/a", &mut Cursor::new(b"data")).unwrap();
    volume.commit_incremental().unwrap();
    volume.unmount().unwrap();
    let volume = LtfsVolume::mount(&device).unwrap();
    let head = volume.committed_index_construct();
    assert!(!head.incremental);
    assert!(volume.index().previous_incremental_location.is_some());
    assert!(
        volume
            .recovery()
            .ip
            .last_index
            .as_ref()
            .unwrap()
            .index
            .previous_incremental_location
            .is_none()
    );
    drop(volume);
    let label = read_label_only(&device).unwrap();
    assert_eq!(
        verify_dp_ip_head(
            &TapeDrive::new(&device),
            &label,
            head,
            &RecoveryBudget::default()
        )
        .unwrap(),
        head.generation
    );
}

#[test]
fn normal_clean_full_mount_skips_history_but_explicit_audit_checks_it() {
    let (lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    commit_file(&dev, "/b", 2);
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.recovery().history_checked);
    assert_eq!(vol.recovery().chain_depth, 0);
    assert!(vol.writable());
    drop(vol);
    // 最新 Full 自包含；历史损坏由显式历史审计报告，不冒充已经扫描历史。
    lib.with_cartridge_mut(BARCODE, |c| {
        for o in &mut c.partitions[1].objects {
            if let LogicalObject::Record(d) = o
                && d.windows(20).any(|w| w == b"<generationnumber>2<")
            {
                *d = b"broken old history".to_vec();
            }
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.recovery().history_checked);
    for (path, seed) in [("a", 1), ("b", 2)] {
        let mut data = Vec::new();
        vol.read_file_to_writer(path, &mut data).unwrap();
        assert_eq!(data, payload(50_000, seed));
    }
    assert!(matches!(
        mount_err(&dev, &RecoveryBudget::default()),
        TapeError::RecoveryRestricted { .. }
    ));
}

#[test]
fn normal_mount_audits_history_when_vci_generation_disagrees() {
    let (_lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    let mam = tape_rs::ltfs::mam::Mam::with_partition(&dev, 1);
    let mut vci = mam.read_vci().unwrap().unwrap();
    vci.generation += 1;
    mam.write_vci(&vci).unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.recovery().history_checked);
    assert_eq!(vol.recovery().chain_depth, 1);
    assert_eq!(vol.index().generation, 2);
}

/// 小批次追加 DP Full、IP 留待检查点更新，与守护进程实际 sync_batch 路径一致。
#[test]
fn committed_dp_full_with_older_ip_has_bounded_mount_locates() {
    let (lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for i in 0..40 {
        vol.append_file(&format!("/file-{i}"), &mut Cursor::new(payload(32, i)))
            .unwrap();
        vol.sync_batch().unwrap();
        assert!(!vol.index().incremental, "此夹具必须生成自包含 Full");
    }
    let generation = vol.index().generation;
    drop(vol); // 已提交，但没有计划卸载检查点。
    lib.power_cut();
    let before = dev.command_log().len();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let locates = dev.command_log()[before..]
        .iter()
        .filter(|&&op| op == opcode::LOCATE_16)
        .count();
    let r = vol.recovery();
    eprintln!(
        "locates={locates}, history_checked={}, depth={}, hints={}/{}, ip_debt={}",
        r.history_checked, r.chain_depth, r.dp.hint_used, r.ip.hint_used, r.ip_debt
    );
    assert!(vol.writable());
    assert_eq!(vol.index().generation, generation);
    assert!(r.ip_debt && r.dp.hint_used && r.ip.hint_used);
    assert!(
        locates <= 12,
        "正常 Full 接管不应逐代随机定位：实际 {locates}"
    );
    for i in 0..40 {
        let mut bytes = Vec::new();
        vol.read_file_to_writer(&format!("/file-{i}"), &mut bytes)
            .unwrap();
        assert_eq!(bytes, payload(32, i));
    }
}

#[test]
fn dp_full_with_older_ip_still_audits_if_either_vci_is_stale_or_mismatched() {
    for partition in [0, 1] {
        for stale in [false, true] {
            let (_lib, dev) = setup();
            let mut vol = LtfsVolume::mount(&dev).unwrap();
            for i in 0..3 {
                vol.append_file(&format!("/file-{i}"), &mut Cursor::new(b"data"))
                    .unwrap();
                vol.sync_batch().unwrap();
            }
            drop(vol);
            let mam = tape_rs::ltfs::mam::Mam::with_partition(&dev, partition);
            let mut vci = mam.read_vci().unwrap().unwrap();
            if stale {
                vci.vcr[0] ^= 0x80;
            } else {
                vci.generation += 1;
            }
            mam.write_vci(&vci).unwrap();
            let vol = LtfsVolume::mount(&dev).unwrap();
            assert!(
                vol.recovery().history_checked,
                "partition={partition}, stale={stale}"
            );
            assert_eq!(vol.recovery().chain_depth, 3);
            assert_eq!(vol.index().generation, 4);
            assert!(vol.writable());
        }
    }
}

#[test]
fn dp_full_with_older_ip_keeps_explicit_history_audit() {
    let (lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for path in ["/first", "/second"] {
        vol.append_file(path, &mut Cursor::new(b"data")).unwrap();
        vol.sync_batch().unwrap();
    }
    drop(vol);
    lib.with_cartridge_mut(BARCODE, |c| {
        for object in &mut c.partitions[1].objects {
            if let LogicalObject::Record(bytes) = object
                && String::from_utf8_lossy(bytes).contains("<generationnumber>2<")
            {
                *bytes = b"broken historical Full".to_vec();
            }
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable() && vol.recovery().ip_debt);
    assert!(!vol.recovery().history_checked);
    for path in ["/first", "/second"] {
        let mut bytes = Vec::new();
        vol.read_file_to_writer(path, &mut bytes).unwrap();
        assert_eq!(bytes, b"data");
    }
    drop(vol);
    assert!(matches!(
        mount_err(&dev, &RecoveryBudget::default()),
        TapeError::RecoveryRestricted { .. }
    ));
}

#[test]
fn dp_full_does_not_skip_history_if_older_ip_points_beyond_it() {
    let (lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("/file", &mut Cursor::new(b"data")).unwrap();
    vol.sync_batch().unwrap();
    let end = vol.index().self_location.start_block + 100;
    drop(vol);
    lib.with_cartridge_mut(BARCODE, |c| {
        for object in &mut c.partitions[0].objects {
            if let LogicalObject::Record(bytes) = object {
                let xml = String::from_utf8_lossy(bytes);
                if xml.contains("<ltfsindex") {
                    let mut index = tape_rs::ltfs::index::LtfsIndex::parse(bytes).unwrap();
                    index.previous_location.as_mut().unwrap().start_block = end;
                    *bytes = index.to_xml().unwrap().to_vec();
                }
            }
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.recovery().history_checked);
    assert_eq!(vol.index().generation, 2);
}

#[cfg(feature = "fdb")]
#[test]
fn fdb_owner_denies_scratch_admission_before_full_or_delta_media_io() {
    use tape_rs::ltfs::recovery::verify_dp_ip_head;
    use tape_rs::ltfs::volume::read_label_only;
    use tape_rs::tape::commands::TapeDrive;

    let (_lib, dev) = setup();
    let label = read_label_only(&dev).unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/full", &mut Cursor::new(b"full"))
        .unwrap();
    volume.commit().unwrap();
    let full = volume.committed_index_construct();
    volume
        .append_file("/delta", &mut Cursor::new(b"delta"))
        .unwrap();
    volume.commit_incremental().unwrap();
    let delta = volume.committed_index_construct();
    drop(volume);
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.headroom_bytes = u64::MAX;
    for head in [full, delta] {
        let before = dev.command_log();
        let error = verify_dp_ip_head(&TapeDrive::new(&dev), &label, head, &budget).unwrap_err();
        assert!(error.to_string().contains("临时盘准入"), "{error}");
        assert_eq!(
            dev.command_log(),
            before,
            "no tape operation on denied admission"
        );
    }
    verify_dp_ip_head(
        &TapeDrive::new(&dev),
        &label,
        delta,
        &RecoveryBudget::default(),
    )
    .unwrap();
}

#[cfg(feature = "fdb")]
#[test]
fn streamed_full_recovery_captures_exact_source_and_reads_without_a_volume_tree() {
    use sha2::{Digest, Sha256};
    use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
    use tape_rs::ltfs::{recovery::capture_full_recovery, volume::read_label_only};
    use tape_rs::tape::commands::TapeDrive;

    let (_lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    commit_file(&dev, "/b", 2);
    let volume = LtfsVolume::mount_with_budget(&dev, &RecoveryBudget::default()).unwrap();
    let expected = volume.committed_index_construct();
    let depth = volume.recovery().chain_depth;
    let eod = volume.recovery().dp.eod;
    drop(volume);
    let label = read_label_only(&dev).unwrap();
    let before = dev.command_log().len();
    let mut xml = Vec::new();
    let source = capture_full_recovery(
        &TapeDrive::new(&dev),
        &label,
        &RecoveryBudget::default(),
        &mut xml,
    )
    .unwrap();
    assert_eq!(source.construct, expected);
    assert_eq!(source.history_depth, depth);
    assert!(!source.history_truncated);
    assert_eq!(source.dp_eod, eod);
    assert_eq!(source.ip_generation, expected.generation);
    assert_eq!(source.source_bytes, xml.len() as u64);
    assert_eq!(Sha256::digest(&xml).as_slice(), expected.sha256);
    let path = std::env::temp_dir().join(format!("streamed-recovery-{}.db", uuid::Uuid::new_v4()));
    let identity = ExpectedIndex {
        volume_uuid: expected.volume_uuid,
        generation: expected.generation,
        location: expected.location,
        sha256: expected.sha256,
    };
    VolumeExtentCache::build(&xml[..], &path, identity).unwrap();
    let cache = VolumeExtentCache::open(&path, identity).unwrap();
    let mut bytes = Vec::new();
    assert_eq!(
        cache
            .read_file_to_writer(&dev, &label, "a", &mut bytes)
            .unwrap(),
        50_000
    );
    assert_eq!(bytes, payload(50_000, 1));
    let mut wrong = label.clone();
    wrong.volume_uuid = uuid::Uuid::new_v4();
    let commands = dev.command_log().len();
    assert!(
        cache
            .read_file_to_writer(&dev, &wrong, "a", &mut Vec::new())
            .is_err()
    );
    assert_eq!(dev.command_log().len(), commands, "错卷在任何读带之前拒绝");
    assert!(
        !dev.command_log()[before..]
            .iter()
            .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x1b | 0x8d | 0xa5))
    );
    drop(cache);
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "fdb")]
#[test]
fn streamed_full_recovery_rejects_unclean_tail_conflicting_ip_and_history() {
    use tape_rs::ltfs::{recovery::capture_full_recovery, volume::read_label_only};
    use tape_rs::tape::commands::TapeDrive;
    for scenario in ["unindexed", "truncated", "ip", "history"] {
        let (lib, dev) = setup();
        commit_file(&dev, "/a", 1);
        commit_file(&dev, "/b", 2);
        let volume = LtfsVolume::mount(&dev).unwrap();
        let mut prior = volume.index().clone();
        let previous = prior.previous_location.unwrap();
        prior.self_location = previous;
        prior.previous_location = None;
        prior.root.files.pop();
        let prior_xml = prior.to_xml().unwrap().to_vec();
        drop(volume);
        let label = read_label_only(&dev).unwrap();
        lib.with_cartridge_mut(BARCODE, |cartridge| match scenario {
            "unindexed" => cartridge.partitions[1]
                .objects
                .push(LogicalObject::Record(b"unobserved bytes".to_vec())),
            "truncated" => {
                cartridge.partitions[1]
                    .objects
                    .push(LogicalObject::Filemark);
                cartridge.partitions[1]
                    .objects
                    .push(LogicalObject::Record(b"<ltfsindex>".to_vec()));
            }
            "ip" => {
                for object in &mut cartridge.partitions[0].objects {
                    if let LogicalObject::Record(bytes) = object {
                        let text = String::from_utf8_lossy(bytes);
                        if text.contains("<ltfsindex") {
                            *bytes = text
                                .replace("<length>50000</length>", "<length>50001</length>")
                                .into_bytes();
                        }
                    }
                }
            }
            "history" => {
                cartridge.partitions[1].objects[previous.start_block as usize] =
                    LogicalObject::Record(prior_xml)
            }
            _ => unreachable!(),
        });
        flush_all(&lib);
        let error = capture_full_recovery(
            &TapeDrive::new(&dev),
            &label,
            &RecoveryBudget::default(),
            &mut Vec::new(),
        )
        .expect_err(scenario);
        if scenario == "history" {
            assert!(error.to_string().contains("节点内容"), "{error}");
        }
    }
}

#[cfg(feature = "fdb")]
#[test]
fn streamed_full_recovery_preserves_budget_and_device_errors_and_rejects_deltas() {
    use tape_rs::ltfs::{recovery::capture_full_recovery, volume::read_label_only};
    use tape_rs::tape::commands::TapeDrive;
    let (_lib, dev) = setup();
    commit_file(&dev, "/a", 1);
    let label = read_label_only(&dev).unwrap();
    let tiny = RecoveryBudget {
        max_index_bytes: 1,
        ..Default::default()
    };
    let error =
        capture_full_recovery(&TapeDrive::new(&dev), &label, &tiny, &mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("超过预算"), "{error}");
    dev.inject(Fault {
        opcode: opcode::READ_6,
        action: FaultAction::HostError(3),
        after: 0,
        remaining: 1,
    });
    let error = capture_full_recovery(
        &TapeDrive::new(&dev),
        &label,
        &RecoveryBudget::default(),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(error, TapeError::ScsiHost(3)), "{error}");
    dev.clear_faults();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/delta", &mut Cursor::new(payload(50_000, 3)))
        .unwrap();
    volume.commit_incremental().unwrap();
    drop(volume);
    assert!(
        capture_full_recovery(
            &TapeDrive::new(&dev),
            &label,
            &RecoveryBudget::default(),
            &mut Vec::new()
        )
        .is_err()
    );
}
