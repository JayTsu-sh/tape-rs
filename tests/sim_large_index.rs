//! 大构造/小输入分段回归：不需要真实设备，不把大 XML 复制到解析输入缓冲。
use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::RecoveryBudget;
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::sim::{LogicalObject, SimCartridge, SimLibrary};

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
    sim.with_cartridge_mut("SHORT1L8", |c| {
        for part in &mut c.partitions {
            let LogicalObject::Record(xml) = &part.objects[5] else {
                panic!("缺少索引");
            };
            let xml = xml.clone();
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
