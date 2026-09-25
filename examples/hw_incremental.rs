//! 既有 Holo 测试卷上的 LTFS 2.5 增量验证：不格式化；追加并删除独立测试目录。
//! 先停止所有使用该库的服务、核对库存，再设置 TAPE_RS_HW_DRIVE、
//! TAPE_RS_HW_SERIAL、TAPE_RS_HW_UUID 和 TAPE_RS_HW_POOL。拒绝已有预留。
//! 出错时保留预留和现场；成功时清洁卸载并释放。不能用于断电持久性证明。
//! TAPE_RS_HW_CHECK_ONLY=1 仅核验清洁 DP/IP Full，不注册预留或写入。
use std::io::Cursor;
use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::volume::{HashVerdict, LtfsVolume, XATTR_POOL_UUID};
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::read_unit_serial;
use tape_rs::scsi::reservation::{self, FenceOutcome, ReservationKey};
use tape_rs::tape::commands::TapeDrive;

fn main() {
    let required = |key| std::env::var(key).expect("必须显式设置设备、序列号、卷 UUID 和池 UUID");
    let dev = ScsiDevice::open(&required("TAPE_RS_HW_DRIVE")).unwrap();
    assert_eq!(
        read_unit_serial(&dev).unwrap(),
        required("TAPE_RS_HW_SERIAL")
    );
    let status = reservation::read_status(&dev).unwrap();
    assert!(
        status.holder.is_none() && status.keys.is_empty(),
        "设备仍有预留或注册"
    );
    let initial = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(
        initial.label().volume_uuid.to_string(),
        required("TAPE_RS_HW_UUID")
    );
    assert!(
        initial
            .index()
            .root
            .xattrs
            .iter()
            .any(|x| x.key == XATTR_POOL_UUID && x.value == required("TAPE_RS_HW_POOL"))
    );
    assert!(initial.writable() && !initial.index().incremental);
    if std::env::var("TAPE_RS_HW_CHECK_ONLY").as_deref() == Ok("1") {
        let ip = &initial.recovery().ip.last_index.as_ref().unwrap().index;
        assert!(!ip.incremental);
        assert_eq!(ip.generation, initial.index().generation);
        assert_eq!(ip.previous_location, Some(initial.index().self_location));
        println!(
            "CHECK clean_gen={} dp_block={} ip_block={} files={}",
            initial.index().generation,
            initial.index().self_location.start_block,
            ip.self_location.start_block,
            initial.list().len()
        );
        return;
    }
    let mut existing = Vec::new();
    initial
        .index()
        .walk_files(|p, f| existing.push((p.to_string(), f.clone())));
    let prefix = format!("rc/codex-incremental-{}", uuid::Uuid::new_v4());
    println!(
        "START uuid={} gen={} prefix={prefix}",
        initial.label().volume_uuid,
        initial.index().generation
    );
    drop(initial);
    let key = ReservationKey::new(250, 1);
    assert!(matches!(
        reservation::fence(&dev, key).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    for n in 1..=6 {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.set_reservation_guard(Some(key));
        vol.append_file(&format!("{prefix}/f{n}"), &mut Cursor::new(vec![n; 512]))
            .unwrap();
        vol.commit_incremental().unwrap();
        let pos = vol.index().self_location.start_block;
        let generation = vol.index().generation;
        drop(vol);
        // 独立读取实际 DP XML，不能只检查 writer 内存标记。
        let drive = TapeDrive::new(&dev);
        drive.locate(1, pos, true).unwrap();
        let mut xml = vec![0; 512 * 1024];
        let len = drive.read_block(&mut xml).unwrap();
        let raw = LtfsIndex::parse(&xml[..len]).unwrap();
        assert_eq!(raw.incremental, n != 6);
        let vol = LtfsVolume::mount(&dev).unwrap();
        assert_eq!(
            vol.recovery().incremental_depth,
            if n == 6 { 0 } else { n as u32 }
        );
        for k in 1..=n {
            let mut bytes = Vec::new();
            vol.read_file_to_writer(&format!("{prefix}/f{k}"), &mut bytes)
                .unwrap();
            assert_eq!(bytes, vec![k; 512]);
        }
        println!(
            "COMMIT gen={generation} dp_block={pos} incremental={} depth={} xml_bytes={len}",
            raw.incremental,
            vol.recovery().incremental_depth
        );
    }
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.set_reservation_guard(Some(key));
    for n in 1..=6 {
        vol.remove_file(&format!("{prefix}/f{n}")).unwrap();
    }
    vol.remove_directory(&prefix).unwrap();
    vol.commit_incremental().unwrap();
    println!("DELETE gen={}", vol.index().generation);
    drop(vol);
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.set_reservation_guard(Some(key));
    assert!(vol.index().find_file(&format!("{prefix}/f1")).is_none());
    vol.unmount().unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let ip = &vol.recovery().ip.last_index.as_ref().unwrap().index;
    assert!(!vol.index().incremental && !ip.incremental);
    assert_eq!(ip.generation, vol.index().generation);
    assert_eq!(ip.previous_location, Some(vol.index().self_location));
    assert_eq!(vol.list().len(), existing.len());
    for (path, original) in &existing {
        assert_eq!(vol.index().find_file(path), Some(original));
        assert!(
            matches!(vol.verify_file(path).unwrap(), HashVerdict::Match { .. }),
            "{path} 校验失败或缺少哈希"
        );
    }
    println!(
        "PASS clean_gen={} original_files_verified={}",
        vol.index().generation,
        existing.len()
    );
    vol.unmount().unwrap();
    TapeDrive::new(&dev).unload().unwrap();
    reservation::release_and_unregister(&dev, key).unwrap();
}
