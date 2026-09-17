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
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert_eq!(vol.index().generation, 3);
    assert_eq!(r.chain_depth, 2, "gen3 → gen2 → gen1");
    assert!(r.chain_ok && !r.ip_debt && vol.writable());
    assert_eq!(names(&vol).len(), 2);
}

#[test]
fn tp05_unindexed_data_tail_is_read_only() {
    let (lib, dev) = setup();
    commit_file(&dev, "/committed", 1);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        // 第一个文件的记录被第二次 LOCATE 刷入介质，第二个文件仍在缓冲；然后掉电。
        vol.append_file("/lost1", &mut Cursor::new(payload(30_000, 2)))
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
        let _vol = LtfsVolume::mount(&dev).unwrap();
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
        vol.append_file("/lost2", &mut Cursor::new(payload(30_000, 3)))
            .unwrap();
    }
    lib.power_cut();
    // 写入使介质 VCR 变化，两分区 VCI 都过期；标准路径仍找到 gen 2，尾部 T1
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(!r.dp.hint_used && !r.ip.hint_used);
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
        vol.append_file("/b", &mut Cursor::new(payload(1000, 9))).unwrap();
        vol.commit().unwrap();
        assert_eq!(vol.index().generation, 4);
    }
    let vol = LtfsVolume::mount(&dev).unwrap();
    let r = vol.recovery();
    assert!(!r.ip_ahead() && !r.ip_debt && r.chain_ok);
    assert_eq!(names(&vol).len(), 2);
    assert!(vol.writable());
}
