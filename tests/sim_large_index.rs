//! 大构造/小输入分段回归：不需要真实设备，不把大 XML 复制到解析输入缓冲。
use sha2::{Digest, Sha256};
use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use tape_rs::error::TapeError;
use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::RecoveryBudget;
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::sim::{Fault, FaultAction, LogicalObject, SimCartridge, SimLibrary};
use tape_rs::tape::commands::TapeDrive;

#[test]
fn recovery_streams_index_over_64m_and_preserves_explicit_budget() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("LARGE1L8", 512 << 20), 0)
        .unwrap();
    sim.load_into_drive("LARGE1L8", 0).unwrap();
    let dev = sim.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "LARGE1".into(),
            block_size: 512 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    // 保留原标签、自指针和两个分区回指，在 XML 根元素内插入大量小事件。
    sim.with_cartridge_mut("LARGE1L8", |c| {
        for part in &mut c.partitions {
            let LogicalObject::Record(xml) = &part.objects[5] else {
                panic!("缺少索引");
            };
            let xml = xml.clone();
            let end = xml
                .windows(b"</ltfsindex>".len())
                .position(|w| w == b"</ltfsindex>")
                .unwrap();
            let mut large = xml[..end].to_vec();
            let comment = format!("<!--{}-->", "x".repeat(1017));
            for _ in 0..(65 << 10) {
                large.extend_from_slice(comment.as_bytes());
            }
            large.extend_from_slice(&xml[end..]);
            assert!(large.len() > 64 << 20);
            part.objects.truncate(5);
            part.objects.extend(
                large
                    .chunks(512 << 10)
                    .map(|v| LogicalObject::Record(v.to_vec())),
            );
            part.objects.push(LogicalObject::Filemark);
            part.flushed = part.objects.len();
        }
    });
    let restricted = RecoveryBudget {
        max_index_bytes: 64 << 20,
        ..Default::default()
    };
    assert!(LtfsVolume::mount_with_budget(&dev, &restricted).is_err());
    let before = dev.command_log().len();
    let volume = LtfsVolume::mount(&dev).unwrap();
    assert!(volume.writable());
    assert!(volume.list().is_empty());
    assert!(volume.recovery().append_ok);
    assert!(
        !dev.command_log()[before..]
            .iter()
            .any(|op| matches!(*op, 0x0a | 0x10 | 0x8d))
    );
}

#[test]
fn stream_parser_matches_slice_parser_and_rejects_truncated_full() {
    let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "分段回归".into(), 'b');
    let xml = index.to_xml().unwrap();
    let fragmented = std::io::BufReader::with_capacity(3, std::io::Cursor::new(xml.as_ref()));
    let parsed = LtfsIndex::parse_reader(fragmented).unwrap();
    assert_eq!(
        parsed.to_xml().unwrap(),
        LtfsIndex::parse(&xml).unwrap().to_xml().unwrap()
    );
    let end = xml
        .windows(b"</ltfsindex>".len())
        .position(|w| w == b"</ltfsindex>")
        .unwrap();
    assert!(LtfsIndex::parse_reader(std::io::Cursor::new(&xml[..end])).is_err());
}

#[test]
fn index_prefix_and_xml_can_cross_short_tape_records() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("SHORT1L8", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("SHORT1L8", 0).unwrap();
    let dev = sim.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "SHORT1".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let mut data_index_sha = [0; 32];
    sim.with_cartridge_mut("SHORT1L8", |c| {
        for (partition, part) in c.partitions.iter_mut().enumerate() {
            let LogicalObject::Record(xml) = &part.objects[5] else {
                panic!("缺少索引");
            };
            let xml = xml.clone();
            if partition == 1 {
                data_index_sha = Sha256::digest(&xml).into();
            }
            part.objects.truncate(5);
            part.objects
                .extend(xml.chunks(7).map(|v| LogicalObject::Record(v.to_vec())));
            part.objects.push(LogicalObject::Filemark);
            part.flushed = part.objects.len();
        }
    });
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable());
    assert_eq!(vol.index().generation, 1);
    assert_eq!(
        vol.recovery().view().unwrap().construct_sha256,
        data_index_sha
    );
    let candidate = vol.recovery().view().unwrap();
    let path = std::env::temp_dir().join(format!(
        "tape-rs-confirmed-index-{}.db",
        uuid::Uuid::new_v4()
    ));
    let drive = TapeDrive::new(&dev);
    let mut wrong_boundary = candidate.clone();
    wrong_boundary.end_block += 1;
    assert!(
        VolumeExtentCache::build_confirmed(
            &drive,
            vol.label(),
            &wrong_boundary,
            &RecoveryBudget::default(),
            &path
        )
        .is_err()
    );
    assert!(!path.exists());
    let stats = VolumeExtentCache::build_confirmed(
        &drive,
        vol.label(),
        candidate,
        &RecoveryBudget::default(),
        &path,
    )
    .unwrap();
    assert_eq!(stats.files, 0);
    let expected = ExpectedIndex {
        volume_uuid: candidate.index.volume_uuid,
        generation: candidate.index.generation,
        location: candidate.index.self_location,
        sha256: data_index_sha,
    };
    VolumeExtentCache::open(&path, expected).unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn confirmed_index_cache_keeps_committed_file_extents() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("CACHE1L8", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("CACHE1L8", 0).unwrap();
    let dev = sim.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "CACHE1".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/dir/data.bin", &mut std::io::Cursor::new(b"evidence"))
            .unwrap();
        vol.commit().unwrap();
    }
    let vol = LtfsVolume::mount(&dev).unwrap();
    let file = vol.index().find_file("dir/data.bin").unwrap();
    let candidate = vol.recovery().view().unwrap();
    let path =
        std::env::temp_dir().join(format!("tape-rs-extent-file-{}.db", uuid::Uuid::new_v4()));
    let drive = TapeDrive::new(&dev);
    let stats = VolumeExtentCache::build_confirmed(
        &drive,
        vol.label(),
        candidate,
        &RecoveryBudget::default(),
        &path,
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    let expected = ExpectedIndex {
        volume_uuid: candidate.index.volume_uuid,
        generation: candidate.index.generation,
        location: candidate.index.self_location,
        sha256: candidate.construct_sha256,
    };
    let cache = VolumeExtentCache::open(&path, expected).unwrap();
    let cached = cache.file_by_uid(file.meta.file_uid).unwrap().unwrap();
    assert_eq!(cached.path, "dir/data.bin");
    assert_eq!(cached.length, file.length);
    assert_eq!(cached.extents, file.extents);
    drop(cache);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn confirmed_cache_read_failure_preserves_scsi_diagnostics() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("CACHE2L8", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("CACHE2L8", 0).unwrap();
    let dev = sim.drive(0);
    mkltfs(&dev, &MkltfsOptions::default()).unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    let candidate = vol.recovery().view().unwrap();
    dev.inject(Fault {
        opcode: opcode::READ_6,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x11,
            ascq: 0,
        },
        after: 0,
        remaining: 1,
    });
    let path =
        std::env::temp_dir().join(format!("tape-rs-extent-error-{}.db", uuid::Uuid::new_v4()));
    let err = VolumeExtentCache::build_confirmed(
        &TapeDrive::new(&dev),
        vol.label(),
        candidate,
        &RecoveryBudget::default(),
        &path,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        TapeError::ScsiCommand {
            sense_key: 0x03,
            asc: 0x11,
            ..
        }
    ));
    assert!(!path.exists());
}

/// 原报告规模的文件树回归；不构造整份输入XML，不接触设备。
#[test]
#[ignore = "百万文件内存/时间基线；显式运行，不访问硬件"]
fn stream_parser_materializes_one_million_files() {
    use std::io::Read;
    struct FilesXml {
        header: Option<Vec<u8>>,
        tail: Option<Vec<u8>>,
        frame: std::io::Cursor<Vec<u8>>,
        next: u64,
    }
    impl Read for FilesXml {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if out.is_empty() {
                return Ok(0);
            }
            loop {
                let n = self.frame.read(out)?;
                if n != 0 {
                    return Ok(n);
                }
                let next = if let Some(h) = self.header.take() {
                    h
                } else if self.next < 1_000_000 {
                    let i = self.next;
                    self.next += 1;
                    format!("<file><name>f{i:07}</name><length>0</length><fileuid>{}</fileuid><readonly>false</readonly><modifytime>2026-10-14T00:00:00.000000000Z</modifytime><extentinfo/></file>",i+2).into_bytes()
                } else if let Some(t) = self.tail.take() {
                    t
                } else {
                    return Ok(0);
                };
                self.frame = std::io::Cursor::new(next);
            }
        }
    }
    let mut index = LtfsIndex::empty(
        uuid::Uuid::new_v4(),
        "million file stream regression".into(),
        'b',
    );
    index.highest_file_uid = 1_000_001;
    let xml = index.to_xml().unwrap();
    let marker = b"<contents>";
    let split = xml.windows(marker.len()).position(|w| w == marker).unwrap() + marker.len();
    let source = FilesXml {
        header: Some(xml[..split].to_vec()),
        tail: Some(xml[split..].to_vec()),
        frame: std::io::Cursor::new(Vec::new()),
        next: 0,
    };
    let started = std::time::Instant::now();
    let parsed = LtfsIndex::parse_reader(std::io::BufReader::with_capacity(8192, source)).unwrap();
    assert_eq!(parsed.root.files.len(), 1_000_000);
    assert_eq!(parsed.root.files[999_999].name, "f0999999");
    assert_eq!(parsed.root.files[999_999].meta.file_uid, 1_000_001);
    assert_eq!(
        parsed.root.files[0].meta.modify_time,
        "2026-10-14T00:00:00.000000000Z"
    );
    println!("百万文件流式解析: {:?}", started.elapsed());
}
