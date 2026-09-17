//! DV02（索引与最终屏障各故障点）与 04 的"首个贯通场景"在 SimTransport 上的验证。
//!
//! 每个分支：文件甲已确认归档；文件乙在指定故障点失败；随后用不带任何旧状态的新
//! 实例从介质恢复，核对甲完整、乙按介质证据判定、读写资格分别报告。

use std::io::Cursor;

use tape_rs::error::TapeError;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::TailKind;
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};

const BARCODE: &str = "DV0001L8";
const BLOCK: usize = 64 * 1024;

fn setup() -> (SimLibrary, SimTransport) {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "DV0001".into(),
            block_size: BLOCK as u32,
            ..Default::default()
        },
    )
    .unwrap();
    (lib, dev)
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(17).wrapping_add(seed))
        .collect()
}

fn medium_error(opcode: u8, after: u32) -> Fault {
    Fault {
        opcode,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0C,
            ascq: 0x00,
        },
        after,
        remaining: 1,
    }
}

/// 甲已确认归档；返回甲的内容。
fn archive_alpha(dev: &SimTransport) -> Vec<u8> {
    let data = payload(150_000, 1);
    let mut vol = LtfsVolume::mount(dev).unwrap();
    vol.append_file("/alpha.bin", &mut Cursor::new(&data))
        .unwrap();
    vol.commit().expect("甲 归档成功");
    data
}

fn read(vol: &LtfsVolume<'_>, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    vol.read_file_to_writer(path, &mut out).unwrap();
    out
}

/// 写乙并在注入故障下提交，返回 (提交错误阶段, 乙内容)。
fn write_beta_with_fault(dev: &SimTransport, fault: Fault) -> (String, Vec<u8>) {
    let beta = payload(90_000, 2);
    let mut vol = LtfsVolume::mount(dev).unwrap();
    vol.append_file("/beta.bin", &mut Cursor::new(&beta))
        .unwrap();
    assert_eq!(vol.pending_files(), vec!["beta.bin".to_string()]);
    dev.inject(fault);
    let err = vol.commit().unwrap_err();
    let TapeError::CommitFailed { stage, .. } = &err else {
        panic!("期望 CommitFailed，得到 {err}")
    };
    // 失败后本卷冻结：不能再追加，也不能再提交；已发布视图仍是甲
    assert!(!vol.writable());
    assert!(matches!(
        vol.append_file("/x", &mut Cursor::new(b"z".to_vec())),
        Err(TapeError::RecoveryRestricted { .. })
    ));
    assert_eq!(vol.list().len(), 1);
    assert_eq!(read(&vol, "alpha.bin"), payload(150_000, 1));
    dev.clear_faults();
    (stage.clone(), beta)
}

/// 新实例从介质恢复：甲必须完整可读。
fn recover_fresh<'a>(dev: &'a SimTransport, alpha: &[u8]) -> LtfsVolume<'a> {
    let vol = LtfsVolume::mount(dev).expect("从介质恢复");
    assert_eq!(read(&vol, "alpha.bin"), alpha, "甲 内容不变");
    vol
}

#[test]
fn dv02_fault_before_index_records_data_written_index_not_started() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    // commit 的第一条 WRITE FILEMARKS 是开头 FM
    let (stage, _) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_FILEMARKS_6, 0));
    assert_eq!(stage, "opening_fm");
    let vol = recover_fresh(&dev, &alpha);
    // 乙的数据已落带但没有索引构造：T1，只读，乙不可见
    assert_eq!(vol.recovery().dp.tail, TailKind::UnindexedData);
    assert!(vol.list().iter().all(|(p, _)| !p.ends_with("beta.bin")));
    assert!(!vol.writable());
}

#[test]
fn dv02_fault_in_index_records_leaves_truncated_index() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    // 故障在 append 之后注入：commit 内第 1 次 WRITE(6) 就是 DP 索引记录
    let (stage, _) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_6, 0));
    assert_eq!(stage, "index_records");
    let vol = recover_fresh(&dev, &alpha);
    // 开头 FM 已写、记录未写：区域 [FM] 后直接 EOD，按协议归为未索引数据
    assert!(matches!(
        vol.recovery().dp.tail,
        TailKind::UnindexedData | TailKind::TruncatedIndex
    ));
    assert_eq!(vol.index().generation, 2);
    assert!(!vol.writable());
}

#[test]
fn dv02_fault_on_closing_filemark_is_truncated_index() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    let (stage, _) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_FILEMARKS_6, 1));
    assert_eq!(stage, "closing_fm");
    let vol = recover_fresh(&dev, &alpha);
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::TruncatedIndex, "{:?}", r.dp.notes);
    assert_eq!(vol.index().generation, 2, "乙 的索引残缺，不得发布");
    assert!(!vol.writable());
}

#[test]
fn dv02_fault_on_index_partition_dp_already_committed() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    // commit 内第 1 次 WRITE(6) 是 DP 索引记录，第 2 次是 IP 索引记录
    let (stage, beta) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_6, 1));
    assert_eq!(stage, "index_partition");
    let vol = recover_fresh(&dev, &alpha);
    let r = vol.recovery();
    // 应用得到"结果未定"，但介质证据证明乙已可靠提交：DP gen 3 完整。
    // IP 的开头文件标记已写下、索引没写成，IP 上暂时没有索引；卷内没有 IP 数据，下次提交会整段重写 IP
    assert_eq!(r.dp.tail, TailKind::Complete);
    assert!(r.ip_debt, "IP 仍是 gen 2");
    assert_eq!(vol.index().generation, 3);
    assert_eq!(read(&vol, "beta.bin"), beta);
    assert!(vol.writable(), "ip_tail={:?} ip_notes={:?} notes={:?}", r.ip.tail, r.ip.notes, r.notes);
}

#[test]
fn dv02_fault_on_final_barrier_is_indeterminate_but_committed() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    // WRITE FILEMARKS：#0 开头 FM，#1 结束 FM，#2 IP 结束 FM，#3 最终屏障 count=0
    let (stage, beta) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_FILEMARKS_6, 4));
    assert_eq!(stage, "barrier");
    let vol = recover_fresh(&dev, &alpha);
    let r = vol.recovery();
    assert_eq!(vol.index().generation, 3);
    assert_eq!(read(&vol, "beta.bin"), beta);
    assert!(
        !r.dp.hint_used && !r.ip.hint_used,
        "VCI 未更新，快速路径不可用"
    );
    assert!(vol.writable());
}

#[test]
fn dv02_fault_on_vci_write_keeps_commit_and_partial_fast_path() {
    let (_lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    // 两次 WRITE ATTRIBUTE：P0 VCI、P1 VCI；让第二次失败
    let (stage, beta) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_ATTRIBUTE, 1));
    assert_eq!(stage, "vci");
    let vol = recover_fresh(&dev, &alpha);
    let r = vol.recovery();
    assert_eq!(vol.index().generation, 3);
    assert_eq!(read(&vol, "beta.bin"), beta);
    assert!(r.ip.hint_used, "P0 的 VCI 已写且新鲜");
    assert!(!r.dp.hint_used, "P1 的 VCI 过期，走标准路径");
    assert!(vol.writable());
}

#[test]
fn dv02_power_cut_after_index_records_before_closing_filemark() {
    let (lib, dev) = setup();
    let alpha = archive_alpha(&dev);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/beta.bin", &mut Cursor::new(payload(90_000, 2)))
            .unwrap();
        dev.inject(medium_error(opcode::WRITE_FILEMARKS_6, 1));
        let err = vol.commit().unwrap_err();
        assert!(matches!(err, TapeError::CommitFailed { ref stage, .. } if stage == "closing_fm"));
        // 失败后不再发任何会改变位置（从而刷缓冲）的命令，直接掉电：
        // 开头 FM 已刷入，索引记录仍在驱动器缓冲
    }
    lib.power_cut();
    dev.clear_faults();
    let vol = recover_fresh(&dev, &alpha);
    assert_eq!(
        vol.recovery().dp.tail,
        TailKind::UnindexedData,
        "{:?}",
        vol.recovery().dp.notes
    );
    assert_eq!(vol.index().generation, 2);
    assert!(!vol.writable());
}

#[test]
fn threading_scenario_three_branches_do_not_mix() {
    // 04 的"首个贯通场景"：同一份甲，三种乙故障分支各自独立判定
    for (fault, expect_beta_visible, expect_writable) in [
        (medium_error(opcode::WRITE_FILEMARKS_6, 0), false, false), // 数据已写、索引未完成
        (medium_error(opcode::WRITE_FILEMARKS_6, 1), false, false), // 索引结果未知（残缺）
        (medium_error(opcode::WRITE_FILEMARKS_6, 4), true, true),   // S4 成立、响应未送达
    ] {
        let (_lib, dev) = setup();
        let alpha = archive_alpha(&dev);
        let (_, beta) = write_beta_with_fault(&dev, fault);
        let vol = recover_fresh(&dev, &alpha);
        let beta_visible = vol.list().iter().any(|(p, _)| p.ends_with("beta.bin"));
        assert_eq!(beta_visible, expect_beta_visible);
        if beta_visible {
            assert_eq!(read(&vol, "beta.bin"), beta);
        }
        assert_eq!(vol.writable(), expect_writable);
    }
}

/// VCR 不变化的设备上（未修复的 Holo-VTL），DP 索引已写完但 VCI 未更新时，
/// 快速路径的提示指向旧索引；恢复必须发现提示之后还有内容并找到更新的已提交索引。
#[test]
fn constant_vcr_does_not_hide_newer_committed_index() {
    use tape_rs::scsi::sim::SimQuirks;
    let (lib, dev) = setup();
    lib.set_quirks(SimQuirks {
        vcr_never_changes: true,
        ..Default::default()
    });
    let alpha = archive_alpha(&dev);
    // 屏障失败：DP/IP 索引都已写完，VCI 未更新，仍指向 gen 2
    let (stage, beta) = write_beta_with_fault(&dev, medium_error(opcode::WRITE_FILEMARKS_6, 4));
    assert_eq!(stage, "barrier");
    let vol = recover_fresh(&dev, &alpha);
    let r = vol.recovery();
    assert!(
        !r.dp.hint_used,
        "提示之后还有内容，必须不采信: {:?}",
        r.dp.notes
    );
    assert_eq!(vol.index().generation, 3, "不得漏掉已可靠提交的乙");
    assert_eq!(read(&vol, "beta.bin"), beta);
    assert!(vol.writable());
}
