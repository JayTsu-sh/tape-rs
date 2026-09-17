//! 设备层隔离协议（持久预留 + 软件纪律）在模拟器上的验证。节点 = 发起方。
//! 对应研究笔记 pr-fencing-protocol.md 的 PF03—PF06。

use std::io::Cursor;

use tape_rs::error::TapeError;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::reservation::{
    FenceOutcome, PR_TYPE_EXCLUSIVE_ACCESS, ReservationKey, fence, ownership_lost, read_status,
    release_and_unregister, verify_holder,
};
use tape_rs::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary};

const BARCODE: &str = "FENCE1L8";
const NODE_A: u32 = 1;
const NODE_B: u32 = 2;

fn setup() -> SimLibrary {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let opts = MkltfsOptions { volume_id: "FENCE1".into(), block_size: 64 * 1024, ..Default::default() };
    mkltfs(&lib.drive_as(0, NODE_A), &opts).unwrap();
    lib
}

fn write_file(dev: &dyn tape_rs::scsi::transport::TapeTransport, key: ReservationKey, name: &str) -> Result<(), TapeError> {
    let mut vol = LtfsVolume::mount(dev)?;
    vol.set_reservation_guard(Some(key));
    vol.append_file(name, &mut Cursor::new(vec![7u8; 1000]))?;
    vol.commit()?;
    Ok(())
}

#[test]
fn key_encodes_node_and_round() {
    let k = ReservationKey::new(2, 0x1234_5678_9abc);
    assert_eq!(k.0, 0x4C02_1234_5678_9abc);
    assert_eq!((k.node(), k.round(), k.is_ours()), (2, 0x1234_5678_9abc, true));
    assert!(!ReservationKey(0x4000_0000_0a83_0948).is_ours(), "IBM LTFS 的 IPv4 键");
    assert_ne!(ReservationKey::new(2, 7), ReservationKey::new(2, 8), "同节点新一轮得到新键");
}

#[test]
fn pf03_fence_takes_free_device_and_confirms_by_readback() {
    let lib = setup();
    let a = lib.drive_as(0, NODE_A);
    let ka = ReservationKey::new(1, 1);
    match fence(&a, ka).unwrap() {
        FenceOutcome::Fenced { before, after } => {
            assert!(before.keys.is_empty() && before.holder.is_none());
            assert_eq!(after.keys, vec![ka.0]);
            assert_eq!(after.holder, Some((ka.0, PR_TYPE_EXCLUSIVE_ACCESS)));
            assert!(after.generation > before.generation);
        }
        other => panic!("{other:?}"),
    }
    verify_holder(&a, ka).unwrap();
    write_file(&a, ka, "/a.bin").unwrap();
}

#[test]
fn pf01_preempted_owner_is_rejected_by_the_device_and_nothing_lands() {
    let lib = setup();
    let (a, b) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B));
    let (ka, kb) = (ReservationKey::new(1, 1), ReservationKey::new(2, 2));
    assert!(matches!(fence(&a, ka).unwrap(), FenceOutcome::Fenced { .. }));
    write_file(&a, ka, "/before.bin").unwrap();

    // B 未隔离前连挂载都做不到
    assert!(matches!(LtfsVolume::mount(&b), Err(TapeError::ReservationConflict { .. })));
    // 但可以只读查询谁持有
    assert_eq!(read_status(&b).unwrap().holder, Some((ka.0, PR_TYPE_EXCLUSIVE_ACCESS)));

    // B 接管：抢占 + 回读确认，注册表里不再有 A 的键
    match fence(&b, kb).unwrap() {
        FenceOutcome::Fenced { before, after } => {
            assert_eq!(before.holder, Some((ka.0, PR_TYPE_EXCLUSIVE_ACCESS)));
            assert_eq!(after.keys, vec![kb.0]);
        }
        other => panic!("{other:?}"),
    }

    // A 不知道自己已下台，继续写：先 2A/03，之后一律冲突；两者都是"失去资格"
    let e1 = write_file(&a, ka, "/after-fence.bin").unwrap_err();
    assert!(ownership_lost(&e1), "{e1:?}");
    assert!(matches!(e1, TapeError::ScsiCommand { sense_key: 0x06, asc: 0x2A, ascq: 0x03, .. }), "{e1:?}");
    let e2 = write_file(&a, ka, "/after-fence.bin").unwrap_err();
    assert!(matches!(e2, TapeError::ReservationConflict { .. }), "{e2:?}");
    // A 的自检同样失败，且不会去恢复预留
    assert!(ownership_lost(&verify_holder(&a, ka).unwrap_err()));
    assert_eq!(read_status(&a).unwrap().keys, vec![kb.0], "A 没有重新注册");

    // B 看到的卷只有抢占前已提交的内容，并且可写
    let vol = LtfsVolume::mount(&b).unwrap();
    assert_eq!(vol.list().len(), 1);
    assert!(vol.writable());
    drop(vol);
    write_file(&b, kb, "/by-b.bin").unwrap();
}

#[test]
fn pf06_same_node_new_round_gets_a_new_key_and_old_round_is_stale() {
    let lib = setup();
    let a = lib.drive_as(0, NODE_A);
    let old = ReservationKey::new(1, 5);
    let new = ReservationKey::new(1, 6);
    fence(&a, old).unwrap();
    assert!(matches!(fence(&a, new).unwrap(), FenceOutcome::Fenced { .. }));
    verify_holder(&a, new).unwrap();
    // 旧轮次的实例拿旧键自检必然失败
    assert!(ownership_lost(&verify_holder(&a, old).unwrap_err()));
    // 设备不会拒绝它（同一发起方），所以必须在写第一个字节之前就停下：什么也不落带
    let before = lib.cartridge(BARCODE).unwrap().partitions[1].objects.len();
    let e = write_file(&a, old, "/stale-round.bin").unwrap_err();
    assert!(ownership_lost(&e), "{e:?}");
    assert_eq!(lib.cartridge(BARCODE).unwrap().partitions[1].objects.len(), before);
    write_file(&a, new, "/new-round.bin").unwrap();
}

#[test]
fn pf05_commit_guard_catches_reservation_lost_by_drive_reset() {
    let lib = setup();
    let (a, b) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B));
    let (ka, kb) = (ReservationKey::new(1, 1), ReservationKey::new(2, 2));
    fence(&a, ka).unwrap();
    write_file(&a, ka, "/committed.bin").unwrap();

    // 驱动器复位清掉了全部预留。A 挂载时的定位类命令会越过 29/00（它们与位置无关，允许重试），
    // 此后设备接受 A 的一切命令。防线是写入前自检：持有者已不是本轮键，追加在写第一个字节之前失败。
    lib.reset_drive(0);
    match write_file(&a, ka, "/x.bin").unwrap_err() {
        e @ TapeError::OwnershipLost { .. } => assert!(ownership_lost(&e)),
        other => panic!("{other:?}"),
    }
    // 自检失败后 A 不会自己去重新预留
    assert!(read_status(&a).unwrap().holder.is_none());
    assert!(matches!(fence(&b, kb).unwrap(), FenceOutcome::Fenced { .. }));
    assert!(matches!(write_file(&a, ka, "/x.bin"), Err(TapeError::ReservationConflict { .. })));
}

#[test]
fn pf05_commit_guard_fails_commit_as_indeterminate_when_nobody_holds_the_reservation() {
    let lib = setup();
    let a = lib.drive_as(0, NODE_A);
    let ka = ReservationKey::new(1, 1);
    fence(&a, ka).unwrap();
    let mut vol = LtfsVolume::mount(&a).unwrap();
    vol.set_reservation_guard(Some(ka));
    vol.append_file("/y.bin", &mut Cursor::new(vec![1u8; 10])).unwrap();
    // 预留在提交过程中消失（例如另一条路径上的复位），A 的命令仍会被设备接受
    release_and_unregister(&a, ka).unwrap();
    match vol.commit().unwrap_err() {
        TapeError::CommitFailed { stage, .. } => assert_eq!(stage, "guard_pre"),
        other => panic!("{other:?}"),
    }
    assert!(!vol.writable(), "卷冻结");
    assert!(vol.list().is_empty(), "已发布视图不变");
}

#[test]
fn pf03_unknown_result_during_fence_is_an_error_not_a_takeover() {
    let lib = setup();
    let (a, b) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B));
    let (ka, kb) = (ReservationKey::new(1, 1), ReservationKey::new(2, 2));
    fence(&a, ka).unwrap();
    // 抢占命令传输失败：fence 返回错误，调用方据此不接管
    b.inject(Fault { opcode: opcode::PERSISTENT_RESERVE_OUT, action: FaultAction::HostError(0x07), after: 1, remaining: 1 });
    assert!(fence(&b, kb).is_err());
    // 重新执行一遍（从读现状开始）可以完成
    assert!(matches!(fence(&b, kb).unwrap(), FenceOutcome::Fenced { .. }));
}

#[test]
fn pf07_device_without_persistent_reservations_reports_unsupported() {
    let lib = setup();
    assert_eq!(fence(&lib.changer(), ReservationKey::new(1, 1)).unwrap(), FenceOutcome::Unsupported);
}

#[test]
fn planned_handover_releases_but_successor_still_confirms() {
    let lib = setup();
    let (a, b) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B));
    let (ka, kb) = (ReservationKey::new(1, 1), ReservationKey::new(2, 2));
    fence(&a, ka).unwrap();
    release_and_unregister(&a, ka).unwrap();
    let st = read_status(&b).unwrap();
    assert!(st.keys.is_empty() && st.holder.is_none());
    assert!(matches!(fence(&b, kb).unwrap(), FenceOutcome::Fenced { .. }));
}

/// 三节点演练里发现的缺陷：被抢占后一直没碰过该设备的节点，重新隔离时第一条命令
/// 会先收到历史遗留的 2A/03；隔离流程必须越过它，按读到的现状行事。
#[test]
fn fence_works_through_a_stale_preempted_unit_attention() {
    let lib = setup();
    let (a, b) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B));
    fence(&a, ReservationKey::new(1, 1)).unwrap();
    fence(&b, ReservationKey::new(2, 2)).unwrap();
    // A 此后没有发过任何命令，2A/03 仍在排队
    let k = ReservationKey::new(1, 3);
    assert!(matches!(fence(&a, k).unwrap(), FenceOutcome::Fenced { .. }));
    verify_holder(&a, k).unwrap();
}

/// B（轮次 2）开始接管后 C（轮次 3）当选并完成隔离；B 迟到的隔离不能把预留抢走。
#[test]
fn late_fence_from_an_older_round_cannot_steal_from_a_newer_round() {
    let lib = setup();
    let (a, b, c) = (lib.drive_as(0, NODE_A), lib.drive_as(0, NODE_B), lib.drive_as(0, 3));
    let kc = ReservationKey::new(3, 3);
    fence(&a, ReservationKey::new(1, 1)).unwrap();
    assert!(matches!(fence(&c, kc).unwrap(), FenceOutcome::Fenced { .. }));
    let before = read_status(&c).unwrap();
    assert_eq!(fence(&b, ReservationKey::new(2, 2)).unwrap(), FenceOutcome::Superseded { holder: kc });
    assert_eq!(read_status(&c).unwrap(), before, "B 什么也没改");
    verify_holder(&c, kc).unwrap();
    write_file(&c, kc, "/c.bin").unwrap();
    // 非本系统的键（例如 IBM LTFS 的）不受轮次规则保护，照常抢占
    release_and_unregister(&c, kc).unwrap();
    fence(&a, ReservationKey(0x4000_0000_0a83_0948)).unwrap();
    assert!(matches!(fence(&b, ReservationKey::new(2, 2)).unwrap(), FenceOutcome::Fenced { .. }));
}
