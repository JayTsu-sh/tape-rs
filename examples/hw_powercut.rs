//! Holo VM 硬断电探针。人工停妥所有客户端、核对库存后使用。
//! 显式门控：TAPE_RS_HW_DRIVE/SERIAL/UUID/POOL；manifest 为客户端本地路径。
//! prepare 保留预留和增量尾部，不卸载；外部硬断电后 check 验证并写清洁 Full。
//! cleanup 只删除 manifest 中随机前缀的测试文件。出错保留现场，不自动抢占。
use std::io::{Cursor, Write};
use tape_rs::ltfs::index::{LtfsIndex, Xattr};
use tape_rs::ltfs::volume::{HashVerdict, LtfsVolume, XATTR_POOL_UUID};
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::read_unit_serial;
use tape_rs::scsi::reservation::{self, FenceOutcome, ReservationKey};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3, "prepare|check|cleanup manifest.json");
    let mode = args[1].as_str();
    assert!(["prepare", "check", "cleanup"].contains(&mode));
    let required = |key| std::env::var(key).expect("缺少显式硬件门控变量");
    let dev = ScsiDevice::open(&required("TAPE_RS_HW_DRIVE")).unwrap();
    assert_eq!(
        read_unit_serial(&dev).unwrap(),
        required("TAPE_RS_HW_SERIAL")
    );
    let status = reservation::read_status(&dev).unwrap();
    assert!(
        status.holder.is_none() && status.keys.is_empty(),
        "设备仍被预留，必须核对后显式移交"
    );
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(
        vol.label().volume_uuid.to_string(),
        required("TAPE_RS_HW_UUID")
    );
    assert!(
        vol.index()
            .root
            .xattrs
            .iter()
            .any(|x| x.key == XATTR_POOL_UUID && x.value == required("TAPE_RS_HW_POOL"))
    );
    assert!(vol.writable(), "{:?}", vol.recovery().notes);
    let key = ReservationKey::new(250, 2);
    assert!(matches!(
        reservation::fence(&dev, key).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    vol.set_reservation_guard(Some(key));
    if mode == "prepare" {
        assert!(
            !std::path::Path::new(&args[2]).exists(),
            "不得覆盖之前的验证凭证"
        );
        assert!(!vol.index().incremental, "从清洁 Full 开始");
        let prefix = format!("rc/powercut-{}", uuid::Uuid::new_v4());
        let baseline = String::from_utf8(vol.index().to_xml().unwrap().to_vec()).unwrap();
        let manifest = serde_json::json!({"prefix": prefix, "baseline": baseline, "expected_generation": vol.index().generation + 3});
        std::fs::write(&args[2], serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        for n in 1..=3 {
            vol.append_file(&format!("{prefix}/f{n}"), &mut Cursor::new(vec![n; 65536]))
                .unwrap();
            if n == 3 {
                vol.create_directory(&format!("{prefix}/empty")).unwrap();
                vol.add_symlink(&format!("{prefix}/link"), "f1").unwrap();
                vol.set_node_xattr(
                    &format!("{prefix}/f1"),
                    Xattr {
                        key: "powercut.binary".into(),
                        value: "AP8=".into(),
                        base64: true,
                    },
                    true,
                    false,
                )
                .unwrap();
            }
            vol.commit_incremental().unwrap();
            println!(
                "ACK gen={} incremental={} prefix={prefix}",
                vol.index().generation,
                vol.index().incremental
            );
            std::io::stdout().flush().unwrap();
        }
        // 特意不 unmount、不释放 PR。随后由 PVE 硬停 VM，不向 Holo 发送 sync。
        return;
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&args[2]).unwrap()).unwrap();
    let prefix = manifest["prefix"].as_str().unwrap();
    let baseline = LtfsIndex::parse(manifest["baseline"].as_str().unwrap().as_bytes()).unwrap();
    if mode == "check" {
        assert_eq!(
            vol.index().generation,
            manifest["expected_generation"].as_u64().unwrap()
        );
        assert!(vol.index().incremental);
        assert_eq!(vol.recovery().incremental_depth, 3);
        for n in 1..=3 {
            let mut data = Vec::new();
            vol.read_file_to_writer(&format!("{prefix}/f{n}"), &mut data)
                .unwrap();
            assert_eq!(data, vec![n; 65536]);
        }
        assert_eq!(
            vol.index()
                .find_file(&format!("{prefix}/f1"))
                .unwrap()
                .xattr("powercut.binary"),
            Some("AP8=")
        );
        assert_eq!(
            vol.index()
                .find_file(&format!("{prefix}/link"))
                .unwrap()
                .symlink
                .as_deref(),
            Some("f1")
        );
        println!(
            "RECOVERED gen={} incremental_depth=3 payloads=3 metadata=ok",
            vol.index().generation
        );
    } else {
        for n in 1..=3 {
            vol.remove_file(&format!("{prefix}/f{n}")).unwrap();
        }
        vol.remove_file(&format!("{prefix}/link")).unwrap();
        vol.remove_directory(&format!("{prefix}/empty")).unwrap();
        vol.remove_directory(prefix).unwrap();
        vol.commit_incremental().unwrap();
    }
    let mut count = 0;
    baseline.walk_files(|p, f| {
        assert_eq!(vol.index().find_file(p), Some(f), "原有文件元数据变化: {p}");
        assert!(
            matches!(vol.verify_file(p).unwrap(), HashVerdict::Match { .. }),
            "原有内容校验失败: {p}"
        );
        count += 1;
    });
    vol.commit().unwrap();
    println!(
        "FULL gen={} original_verified={count}",
        vol.index().generation
    );
    vol.unmount().unwrap();
    reservation::release_and_unregister(&dev, key).unwrap();
}
