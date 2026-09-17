//! 接管后的自动收尾：T1/T2 尾部在证明独占后追加一份索引，卷重新一致、可写。

use std::io::Cursor;

use tape_rs::error::TapeError;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::TailKind;
use tape_rs::ltfs::volume::{LOST_AND_FOUND, LtfsVolume, TailPolicy, XATTR_ABANDONED};
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::reservation::{FenceOutcome, ReservationKey, fence};
use tape_rs::scsi::sim::{Fault, FaultAction, LogicalObject, SimCartridge, SimLibrary, SimTransport};

const BARCODE: &str = "CLOSE1L8";
const OLD: u32 = 1;
const NEW: u32 = 2;

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn read(vol: &LtfsVolume<'_>, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    vol.read_file_to_writer(path, &mut out).unwrap();
    out
}

fn flush_all(lib: &SimLibrary) {
    lib.with_cartridge_mut(BARCODE, |c| {
        for p in c.partitions.iter_mut() {
            p.flushed = p.objects.len();
        }
    });
}

/// 旧 Leader 格式化、提交一个文件，然后在写第二个文件途中失去执行资格。
/// 返回库、新 Leader 的传输、它的键，以及留在带上未索引的字节。
fn takeover_with_unindexed_tail() -> (SimLibrary, SimTransport, ReservationKey, Vec<u8>) {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let old = lib.drive_as(0, OLD);
    let k_old = ReservationKey::new(1, 1);
    fence(&old, k_old).unwrap();
    let opts = MkltfsOptions { volume_id: "CLOSE1".into(), block_size: 64 * 1024, ..Default::default() };
    mkltfs(&old, &opts).unwrap();
    let lost = payload(150_000, 9);
    {
        let mut vol = LtfsVolume::mount(&old).unwrap();
        vol.set_reservation_guard(Some(k_old));
        vol.append_file("/committed.bin", &mut Cursor::new(payload(100_000, 1))).unwrap();
        vol.commit().unwrap();
        vol.append_file("/in-flight.bin", &mut Cursor::new(&lost)).unwrap();
        // 没有提交；数据已进驱动器并落带
        std::mem::forget(vol);
    }
    flush_all(&lib);
    let new = lib.drive_as(0, NEW);
    let k_new = ReservationKey::new(2, 2);
    assert!(matches!(fence(&new, k_new).unwrap(), FenceOutcome::Fenced { .. }));
    (lib, new, k_new, lost)
}

#[test]
fn unindexed_tail_is_closed_after_takeover_and_volume_is_writable_again() {
    let (_lib, new, key, _) = takeover_with_unindexed_tail();
    let mut vol = LtfsVolume::mount(&new).unwrap();
    assert_eq!(vol.recovery().dp.tail, TailKind::UnindexedData);
    assert!(!vol.writable());
    let gen_before = vol.index().generation;
    let eod = vol.recovery().dp.eod;

    vol.set_reservation_guard(Some(key));
    let rep = vol.close_tail(TailPolicy::Discard).unwrap();
    assert_eq!(rep.tail, TailKind::UnindexedData);
    assert_eq!(rep.abandoned_end_block, eod);
    assert!(rep.abandoned_first_block < eod);
    assert_eq!(rep.generation, gen_before + 1);
    assert_eq!(rep.salvaged, None);
    assert!(vol.writable());
    vol.append_file("/after-close.bin", &mut Cursor::new(payload(5000, 4))).unwrap();
    vol.commit().unwrap();
    drop(vol);

    // 重新挂载：完整尾部、回指链通过、已提交文件完好、放弃的范围有记录
    let vol = LtfsVolume::mount(&new).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::Complete, "{:?}", r.notes);
    assert!(r.chain_ok && !r.ip_debt);
    assert!(vol.writable());
    assert_eq!(read(&vol, "committed.bin"), payload(100_000, 1));
    assert_eq!(read(&vol, "after-close.bin"), payload(5000, 4));
    assert!(vol.index().find_file("in-flight.bin").is_none());
    let note = vol.index().root.xattrs.iter().find(|x| x.key == XATTR_ABANDONED).unwrap();
    assert_eq!(note.value, format!("b:{}-{}", rep.abandoned_first_block, eod - 1));
}

#[test]
fn salvage_registers_the_unindexed_bytes_under_lost_and_found() {
    let (_lib, new, key, lost) = takeover_with_unindexed_tail();
    let mut vol = LtfsVolume::mount(&new).unwrap();
    vol.set_reservation_guard(Some(key));
    let rep = vol.close_tail(TailPolicy::Salvage).unwrap();
    let (path, len) = rep.salvaged.clone().unwrap();
    assert!(path.starts_with(LOST_AND_FOUND));
    assert_eq!(len, lost.len() as u64);
    drop(vol);
    let vol = LtfsVolume::mount(&new).unwrap();
    assert_eq!(read(&vol, &path), lost, "打捞出的内容就是旧 Leader 写到一半的数据");
    assert!(vol.index().find_file(&path).unwrap().meta.readonly);
}

#[test]
fn truncated_index_tail_is_closed_too() {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let old = lib.drive_as(0, OLD);
    let opts = MkltfsOptions { volume_id: "CLOSE1".into(), block_size: 64 * 1024, ..Default::default() };
    mkltfs(&old, &opts).unwrap();
    {
        let mut vol = LtfsVolume::mount(&old).unwrap();
        vol.append_file("/first.bin", &mut Cursor::new(payload(1000, 1))).unwrap();
        vol.commit().unwrap();
        vol.append_file("/second.bin", &mut Cursor::new(payload(20_000, 2))).unwrap();
        old.inject(Fault {
            opcode: opcode::WRITE_FILEMARKS_6,
            action: FaultAction::CheckCondition { sense_key: 0x03, asc: 0x0C, ascq: 0x00 },
            after: 1,
            remaining: 1,
        });
        assert!(matches!(vol.commit(), Err(TapeError::CommitFailed { .. })));
    }
    flush_all(&lib);
    let new = lib.drive_as(0, NEW);
    let key = ReservationKey::new(2, 2);
    fence(&new, key).unwrap();
    let mut vol = LtfsVolume::mount(&new).unwrap();
    assert_eq!(vol.recovery().dp.tail, TailKind::TruncatedIndex);
    vol.set_reservation_guard(Some(key));
    // 打捞只取文件标记之前的数据，不把写了一半的索引当文件内容
    let rep = vol.close_tail(TailPolicy::Salvage).unwrap();
    assert_eq!(rep.salvaged.as_ref().unwrap().1, 20_000);
    drop(vol);
    let vol = LtfsVolume::mount(&new).unwrap();
    assert_eq!(vol.recovery().dp.tail, TailKind::Complete, "{:?}", vol.recovery().dp.notes);
    assert!(vol.writable());
    assert_eq!(read(&vol, &rep.salvaged.unwrap().0), payload(20_000, 2));
}

#[test]
fn closing_requires_proven_exclusivity() {
    let (lib, new, key, _) = takeover_with_unindexed_tail();
    let objects_before = lib.cartridge(BARCODE).unwrap().partitions[1].objects.len();

    // 没设自检键
    let mut vol = LtfsVolume::mount(&new).unwrap();
    assert!(matches!(vol.close_tail(TailPolicy::Discard), Err(TapeError::RecoveryRestricted { .. })));
    // 键不是设备上的持有者（例如陈旧轮次）
    vol.set_reservation_guard(Some(ReservationKey::new(2, 1)));
    assert!(matches!(vol.close_tail(TailPolicy::Discard), Err(TapeError::OwnershipLost { .. })));
    assert!(!vol.writable());
    assert_eq!(lib.cartridge(BARCODE).unwrap().partitions[1].objects.len(), objects_before, "介质未被触碰");

    // 正确的键可以
    vol.set_reservation_guard(Some(key));
    vol.close_tail(TailPolicy::Discard).unwrap();
}

#[test]
fn fake_index_and_complete_tails_are_not_closed_automatically() {
    // 完整尾部：没什么可收的
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive_as(0, NEW);
    let key = ReservationKey::new(2, 2);
    fence(&dev, key).unwrap();
    let opts = MkltfsOptions { volume_id: "CLOSE1".into(), block_size: 64 * 1024, ..Default::default() };
    mkltfs(&dev, &opts).unwrap();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.set_reservation_guard(Some(key));
    assert!(matches!(vol.close_tail(TailPolicy::Discard), Err(TapeError::RecoveryRestricted { .. })));
    drop(vol);

    // T3：末索引之后有一份结构完整但自指针不符的索引，来历不明，不自动处理
    let xml = {
        let c = lib.cartridge(BARCODE).unwrap();
        c.partitions[1]
            .objects
            .iter()
            .rev()
            .find_map(|o| match o {
                LogicalObject::Record(d) if d.starts_with(b"<?xml") => Some(d.clone()),
                _ => None,
            })
            .unwrap()
    };
    lib.with_cartridge_mut(BARCODE, |c| {
        let p = &mut c.partitions[1];
        p.objects.push(LogicalObject::Filemark);
        p.objects.push(LogicalObject::Record(xml));
        p.objects.push(LogicalObject::Filemark);
        p.flushed = p.objects.len();
        c.vcr += 1;
    });
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(vol.recovery().dp.tail, TailKind::FakeIndex);
    vol.set_reservation_guard(Some(key));
    match vol.close_tail(TailPolicy::Discard) {
        Err(TapeError::RecoveryRestricted { reason }) => assert!(reason.contains("FakeIndex"), "{reason}"),
        other => panic!("{other:?}"),
    }
}
