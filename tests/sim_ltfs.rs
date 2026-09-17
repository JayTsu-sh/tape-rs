//! 在 SimTransport 上跑通 mkltfs → mount → append → commit → 重新 mount 的完整链路，
//! 以及换带器库存/搬运和故障注入。不接触任何 /dev/sg*。

use std::io::Cursor;

use tape_rs::changer::commands::MediumChanger;
use tape_rs::changer::element::ElementType;
use tape_rs::error::TapeError;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::sim::{
    DT_START, Fault, FaultAction, STORAGE_START, SimCartridge, SimLibrary, SimQuirks,
};

const BARCODE: &str = "SIM001L8";

fn formatted_drive() -> (SimLibrary, tape_rs::scsi::sim::SimTransport) {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive(0);
    let opts = MkltfsOptions {
        volume_id: "SIM001".into(),
        block_size: 64 * 1024,
        ..Default::default()
    };
    mkltfs(&dev, &opts).expect("mkltfs on sim");
    (lib, dev)
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn mkltfs_append_commit_remount_roundtrip() {
    let (_lib, dev) = formatted_drive();
    let data = payload(200_000, 7);

    let gen_after_commit = {
        let mut vol = LtfsVolume::mount(&dev).expect("mount fresh volume");
        assert_eq!(vol.index().generation, 1);
        vol.append_file("/dir/a.bin", &mut Cursor::new(&data))
            .unwrap();
        vol.commit().unwrap();
        vol.index().generation
    };
    assert_eq!(gen_after_commit, 2);

    let vol = LtfsVolume::mount(&dev).expect("remount");
    assert_eq!(vol.index().generation, 2);
    let files = vol.list();
    assert_eq!(files.len(), 1);
    let (path, len) = &files[0];
    assert!(path.ends_with("a.bin"), "path = {}", path);
    assert_eq!(*len, data.len() as u64);

    let mut out = Vec::new();
    vol.read_file_to_writer(path, &mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn power_cut_keeps_committed_and_drops_unflushed() {
    let (lib, dev) = formatted_drive();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/first.bin", &mut Cursor::new(payload(10_000, 1)))
            .unwrap();
        vol.commit().unwrap();
        // 第二个文件只写数据、不 commit：数据落在驱动器缓冲
        vol.append_file("/second.bin", &mut Cursor::new(payload(10_000, 2)))
            .unwrap();
    }
    lib.power_cut();

    let vol = LtfsVolume::mount(&dev).expect("remount after power cut");
    let names: Vec<String> = vol.list().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names.len(), 1, "names = {:?}", names);
    assert!(names[0].ends_with("first.bin"));
}

#[test]
fn injected_write_fault_surfaces_as_scsi_error() {
    let (_lib, dev) = formatted_drive();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    dev.inject(Fault {
        opcode: opcode::WRITE_6,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0C,
            ascq: 0x00,
        },
        after: 0,
        remaining: 1,
    });
    let err = vol
        .append_file("/bad.bin", &mut Cursor::new(payload(100, 3)))
        .unwrap_err();
    assert!(
        matches!(
            err,
            TapeError::ScsiCommand {
                sense_key: 0x03,
                asc: 0x0C,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn changer_inventory_and_move_to_drive() {
    let lib = SimLibrary::new(2, 5, 2);
    lib.insert_cartridge(SimCartridge::blank("AAA001L8", 1 << 20), 0)
        .unwrap();
    lib.insert_cartridge(SimCartridge::blank("AAA002L8", 1 << 20), 3)
        .unwrap();
    let chg = lib.changer();
    let mut changer = MediumChanger::new(&chg);
    let map = changer.load_address_map().unwrap().clone();
    assert_eq!((map.dt_start, map.dt_count), (DT_START, 2));
    assert_eq!((map.storage_start, map.storage_count), (STORAGE_START, 5));

    let status = changer.read_all_status().unwrap();
    let tags: Vec<(u16, String)> = status
        .iter()
        .filter(|e| e.element_type == ElementType::Storage)
        .filter_map(|e| e.volume_tag.clone().map(|t| (e.address, t)))
        .collect();
    assert_eq!(
        tags,
        vec![
            (STORAGE_START, "AAA001L8".into()),
            (STORAGE_START + 3, "AAA002L8".into())
        ]
    );
    let drives: Vec<&tape_rs::changer::element::ElementStatus> = status
        .iter()
        .filter(|e| e.element_type == ElementType::DataTransfer)
        .collect();
    assert_eq!(drives.len(), 2);
    assert!(drives.iter().all(|d| !d.full));
    assert_eq!(drives[0].drive_id.as_deref(), Some("SIMDRV0000"));

    changer
        .move_medium(STORAGE_START + 3, DT_START + 1)
        .unwrap();
    let status = changer.read_all_status().unwrap();
    let drive1 = status.iter().find(|e| e.address == DT_START + 1).unwrap();
    assert!(drive1.full);
    assert_eq!(drive1.volume_tag.as_deref(), Some("AAA002L8"));
    let slot3 = status
        .iter()
        .find(|e| e.address == STORAGE_START + 3)
        .unwrap();
    assert!(!slot3.full);

    // 源空 / 目标满 各自报 ILLEGAL REQUEST
    let e = changer
        .move_medium(STORAGE_START + 3, DT_START)
        .unwrap_err();
    assert!(matches!(
        e,
        TapeError::ScsiCommand {
            sense_key: 0x05,
            asc: 0x3B,
            ascq: 0x0E,
            ..
        }
    ));
    let e = changer
        .move_medium(STORAGE_START, DT_START + 1)
        .unwrap_err();
    assert!(matches!(
        e,
        TapeError::ScsiCommand {
            sense_key: 0x05,
            asc: 0x3B,
            ascq: 0x0D,
            ..
        }
    ));

    // 驱动器里的磁带对 drive(1) 可见并可读 label 前置命令
    let d1 = lib.drive(1);
    let drive = tape_rs::tape::commands::TapeDrive::new(&d1);
    assert!(drive.test_unit_ready().unwrap());
    let d0 = lib.drive(0);
    let empty = tape_rs::tape::commands::TapeDrive::new(&d0);
    assert!(!empty.test_unit_ready().unwrap());
}

/// Holo-VTL 实测偏差下（文件标记不报 residual、BOP 反向 SPACE 返回 ILLEGAL REQUEST）
/// 全链路仍须正确：靠 sense 的 FM/ILI 位与 INFORMATION 字段判定，而不是 residual。
#[test]
fn roundtrip_under_holo_vtl_quirks() {
    let lib = SimLibrary::new(1, 4, 1);
    lib.set_quirks(SimQuirks::holo_vtl());
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive(0);
    let opts = MkltfsOptions {
        volume_id: "SIM001".into(),
        block_size: 64 * 1024,
        ..Default::default()
    };
    mkltfs(&dev, &opts).unwrap();
    let data = payload(200_000, 9);
    {
        let mut vol = LtfsVolume::mount(&dev).expect("mount under quirks");
        assert!(vol.writable(), "{:?}", vol.recovery().notes);
        vol.append_file("/q/a.bin", &mut Cursor::new(&data))
            .unwrap();
        vol.commit().unwrap();
    }
    // 第二次挂载走标准路径也要正确：让 VCI 过期
    lib.with_cartridge_mut(BARCODE, |c| c.vcr += 1);
    let vol = LtfsVolume::mount(&dev).expect("remount under quirks");
    assert!(!vol.recovery().dp.hint_used);
    assert_eq!(vol.index().generation, 2);
    let mut out = Vec::new();
    vol.read_file_to_writer("q/a.bin", &mut out).unwrap();
    assert_eq!(out, data);
    assert!(vol.writable());
}

#[test]
fn append_records_md5_and_sha256_and_verify_detects_corruption() {
    use tape_rs::ltfs::index::{XATTR_MD5, XATTR_SHA256};
    use tape_rs::ltfs::volume::{HashPolicy, HashVerdict};
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted_drive();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/default.bin", &mut Cursor::new(b"hello".to_vec())).unwrap();
        vol.set_hash_policy(HashPolicy { md5: true, sha256: true });
        vol.append_file_with_xattrs(
            "/a.bin",
            &mut Cursor::new(b"hello".to_vec()),
            &[("user.origin", "/src/a.bin"), (XATTR_MD5, "caller cannot override")],
        )
        .unwrap();
        vol.commit().unwrap();
    }
    let vol = LtfsVolume::mount(&dev).unwrap();
    let f = vol.index().find_file("a.bin").unwrap();
    // RFC 1321 / FIPS 180-4 对 "hello" 的公开值
    assert_eq!(f.xattr(XATTR_MD5), Some("5d41402abc4b2a76b9719d911017c592"));
    assert_eq!(
        f.xattr(XATTR_SHA256),
        Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
    );
    assert_eq!(f.xattr("user.origin"), Some("/src/a.bin"));
    // 默认策略只写 sha256sum
    let d = vol.index().find_file("default.bin").unwrap();
    assert!(d.xattr(XATTR_SHA256).is_some());
    assert_eq!(d.xattr(XATTR_MD5), None);
    assert_eq!(vol.verify_file("a.bin").unwrap(), HashVerdict::Match { algo: "sha256sum" });
    drop(vol);

    // 绕过驱动器改一个数据字节：长度不变，只有哈希能发现
    lib.with_cartridge_mut(BARCODE, |c| {
        let last = c.partitions[1]
            .objects
            .iter_mut()
            .rev()
            .find_map(|o| match o {
                LogicalObject::Record(d) if d.as_slice() == b"hello" => Some(d),
                _ => None,
            })
            .unwrap();
        last[0] = b'j';
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    match vol.verify_file("a.bin").unwrap() {
        HashVerdict::Mismatch { algo, .. } => assert_eq!(algo, "sha256sum"),
        other => panic!("expected mismatch, got {:?}", other),
    }
    assert_eq!(vol.verify_file("default.bin").unwrap(), HashVerdict::Match { algo: "sha256sum" });
}

#[test]
fn symlink_is_followed_on_read_like_ee_layout() {
    use tape_rs::ltfs::volume::HashVerdict;

    let (_lib, dev) = formatted_drive();
    let data = payload(100_000, 3);
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/.LTFSEE_DATA/obj-1", &mut Cursor::new(&data)).unwrap();
        vol.add_symlink("/gpfs/fs1/a.bin", "../../.LTFSEE_DATA/obj-1").unwrap();
        vol.add_symlink("/gpfs/escape", "../../../etc/passwd").unwrap();
        vol.commit().unwrap();
    }
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable(), "{:?}", vol.restricted_reason());
    let mut out = Vec::new();
    vol.read_file_to_writer("gpfs/fs1/a.bin", &mut out).unwrap();
    assert_eq!(out, data);
    assert_eq!(vol.verify_file("gpfs/fs1/a.bin").unwrap(), HashVerdict::NoHash);
    let err = vol.read_file_to_writer("gpfs/escape", &mut Vec::new()).unwrap_err();
    assert!(matches!(err, TapeError::Ltfs(_)), "{:?}", err);
}

#[test]
fn index_with_unknown_elements_mounts_read_only() {
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted_drive();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/a.bin", &mut Cursor::new(payload(10, 1))).unwrap();
        vol.commit().unwrap();
    }
    // 在两个分区的索引里都塞一个本实现不回写的元素。位置和记录数不变。
    lib.with_cartridge_mut(BARCODE, |c| {
        for p in c.partitions.iter_mut() {
            for o in p.objects.iter_mut() {
                if let LogicalObject::Record(d) = o {
                    let text = String::from_utf8_lossy(d).to_string();
                    if text.contains("<ltfsindex") {
                        *d = text
                            .replace("</ltfsindex>", "<futurefeature>1</futurefeature></ltfsindex>")
                            .into_bytes();
                    }
                }
            }
        }
    });
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(vol.list().len(), 1, "读不受影响");
    assert!(!vol.writable());
    assert!(vol.restricted_reason().unwrap().contains("futurefeature"));
    let err = vol.append_file("/b.bin", &mut Cursor::new(vec![1u8])).unwrap_err();
    assert!(matches!(err, TapeError::RecoveryRestricted { .. }), "{:?}", err);
}

/// 反向校准（IBM LTFS 2.4.8.3 读 tape-rs 写的带）暴露的四个格式要求，逐条钉住。
#[test]
fn on_tape_layout_matches_what_ibm_ltfs_requires() {
    use tape_rs::ltfs::index::LtfsIndex;
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted_drive();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/d/a.bin", &mut Cursor::new(payload(100, 1))).unwrap();
        vol.commit().unwrap();
    }
    let c = lib.cartridge(BARCODE).unwrap();
    let text = |o: &LogicalObject| match o {
        LogicalObject::Record(d) => String::from_utf8_lossy(d).to_string(),
        LogicalObject::Filemark => String::new(),
    };
    let field = |xml: &str, tag: &str| {
        let open = format!("<{tag}>");
        let start = xml.find(&open).unwrap() + open.len();
        xml[start..start + xml[start..].find('<').unwrap()].to_string()
    };

    // 1. 两份卷标的 formattime 相同，且带 9 位小数（LTFS11184E / LTFS17034E）
    let labels: Vec<String> = c.partitions.iter().map(|p| text(&p.objects[2])).collect();
    let t0 = field(&labels[0], "formattime");
    assert_eq!(t0, field(&labels[1], "formattime"));
    assert_eq!(t0.len(), "2026-09-17T05:34:36.305334065Z".len(), "{t0}");
    assert_eq!(&t0[19..20], ".");

    // 2. Index Construct = FM + 索引 + FM：块 3 是卷标构造的 FM，块 4 是索引的开头 FM，索引在块 5
    for p in &c.partitions {
        assert!(matches!(p.objects[3], LogicalObject::Filemark));
        assert!(matches!(p.objects[4], LogicalObject::Filemark));
        assert!(text(&p.objects[5]).contains("<ltfsindex"));
        assert!(matches!(p.objects.last().unwrap(), LogicalObject::Filemark));
    }

    // 3. IP 末索引回指 DP 上同代的那份（一致卷的定义），否则 IBM 挂载时会补写 IP
    let ip = LtfsIndex::parse(text(&c.partitions[0].objects[5]).as_bytes()).unwrap();
    let dp_objs = &c.partitions[1].objects;
    let dp_last_block = dp_objs
        .iter()
        .rposition(|o| text(o).contains("<ltfsindex"))
        .unwrap() as u64;
    let dp = LtfsIndex::parse(text(&dp_objs[dp_last_block as usize]).as_bytes()).unwrap();
    assert_eq!(ip.generation, dp.generation);
    let back = ip.previous_location.unwrap();
    assert_eq!((back.partition, back.start_block), ('b', dp_last_block));
    assert_eq!(dp.self_location.start_block, dp_last_block);

    // 4. fileuid 0 是保留值：自动建的目录也要有号，且全卷唯一（LTFS17106E）
    let mut uids = vec![dp.root.meta.file_uid];
    fn collect(d: &tape_rs::ltfs::index::DirectoryNode, out: &mut Vec<u64>) {
        for f in &d.files {
            out.push(f.meta.file_uid);
        }
        for s in &d.subdirs {
            out.push(s.meta.file_uid);
            collect(s, out);
        }
    }
    collect(&dp.root, &mut uids);
    assert!(uids.iter().all(|&u| u != 0), "{uids:?}");
    let n = uids.len();
    uids.sort();
    uids.dedup();
    assert_eq!(uids.len(), n, "fileuid 重复");
    assert!(dp.highest_file_uid >= *uids.last().unwrap());
}

/// 视图里有文件数据放在 IP 上（IBM 的放置策略会这样做）时必须只读：提交会整段重写 IP。
#[test]
fn volume_with_file_data_on_index_partition_mounts_read_only() {
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted_drive();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/a.bin", &mut Cursor::new(payload(10, 1))).unwrap();
        vol.commit().unwrap();
    }
    lib.with_cartridge_mut(BARCODE, |c| {
        for p in c.partitions.iter_mut() {
            for o in p.objects.iter_mut() {
                if let LogicalObject::Record(d) = o {
                    let t = String::from_utf8_lossy(d).to_string();
                    if let Some(ext) = t.find("<extent>") {
                        // 把第一个 extent 改成位于 IP（分区 a）
                        let (head, tail) = t.split_at(ext);
                        assert!(tail.contains("<partition>b</partition>"));
                        let tail = tail.replacen("<partition>b</partition>", "<partition>a</partition>", 1);
                        *d = format!("{head}{tail}").into_bytes();
                    }
                }
            }
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.writable());
    assert!(vol.recovery().notes.iter().any(|n| n.contains("位于 IP")), "{:?}", vol.recovery().notes);
}

/// IBM LTFS 的 <volumelockstate>：locked / permlocked 的卷不得写入，解锁后保留该元素。
#[test]
fn locked_volume_mounts_read_only() {
    use tape_rs::scsi::sim::LogicalObject;

    let (lib, dev) = formatted_drive();
    let set_state = |state: &str| {
        lib.with_cartridge_mut(BARCODE, |c| {
            for p in c.partitions.iter_mut() {
                for o in p.objects.iter_mut() {
                    if let LogicalObject::Record(d) = o {
                        let t = String::from_utf8_lossy(d).to_string();
                        if t.contains("<ltfsindex") {
                            let t = match (t.find("<volumelockstate>"), t.find("</volumelockstate>")) {
                                (Some(a), Some(b)) => format!("{}{}", &t[..a], &t[b + 18..]),
                                _ => t,
                            };
                            *d = t
                                .replace(
                                    "<highestfileuid>",
                                    &format!("<volumelockstate>{state}</volumelockstate><highestfileuid>"),
                                )
                                .into_bytes();
                        }
                    }
                }
            }
        });
    };
    for state in ["locked", "permlocked", "something-new"] {
        set_state(state);
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        assert!(!vol.writable(), "{state}");
        assert!(vol.restricted_reason().unwrap().contains(state));
        let err = vol.append_file("/x", &mut Cursor::new(vec![1u8])).unwrap_err();
        assert!(matches!(err, TapeError::RecoveryRestricted { .. }));
    }
    set_state("unlocked");
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable(), "{:?}", vol.restricted_reason());
    vol.append_file("/x", &mut Cursor::new(vec![1u8])).unwrap();
    vol.commit().unwrap();
    assert_eq!(vol.index().volume_lock_state.as_deref(), Some("unlocked"));
}
