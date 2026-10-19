//! 真实 RC17 DP54/57/62 原始 XML 的公开 API 回归；默认不访问硬件。
//! 来源：2026-10-16 EE/LE Recovery 后 Full14副本→Rust Incremental15。
use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::label::LtfsLabel;
use tape_rs::ltfs::recovery::{IndexCandidate, RecoveryBudget, verify_chain};
use tape_rs::scsi::sim::{LogicalObject, SimCartridge, SimLibrary};
use tape_rs::tape::commands::TapeDrive;

const DP54: &[u8] = include_bytes!("fixtures/ee-recovery-dp-54.xml");
const DP57: &[u8] = include_bytes!("fixtures/ee-recovery-dp-57.xml");
const DP62: &[u8] = include_bytes!("fixtures/ee-recovery-dp-62.xml");

#[test]
fn actual_delta_replays_over_ee_recovery_full() {
    let base = LtfsIndex::parse(DP57).unwrap();
    assert_eq!(
        LtfsIndex::parse(DP54).unwrap().root,
        base.root,
        "实际两份 Full 的所有节点元数据、extent、xattr 与顺序应一致"
    );
    let delta = LtfsIndex::parse(DP62).unwrap();
    let replayed = base.apply_incremental(&delta).unwrap();
    assert_eq!(replayed.generation, 15);
    assert_eq!(
        replayed
            .find_file("interop-completion-20261016/incremental.bin")
            .unwrap()
            .length,
        716800
    );
    base.walk_files(|path, file| assert_eq!(replayed.find_file(path), Some(file)));
}

#[test]
fn ee_same_generation_recovery_full_is_not_a_broken_chain() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("EE0017L9", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("EE0017L9", 0).unwrap();
    sim.with_cartridge_mut("EE0017L9", |cart| {
        let partition = cart.partitions[0].clone();
        cart.partitions = vec![partition.clone(), partition];
        let dp = &mut cart.partitions[1];
        dp.objects = vec![LogicalObject::Record(vec![]); 59];
        dp.objects[53] = LogicalObject::Filemark;
        dp.objects[54] = LogicalObject::Record(DP54.to_vec());
        dp.objects[55] = LogicalObject::Filemark;
        dp.objects[56] = LogicalObject::Filemark;
        dp.objects[57] = LogicalObject::Record(DP57.to_vec());
        dp.objects[58] = LogicalObject::Filemark;
        dp.flushed = dp.objects.len();
    });
    let dev = sim.drive(0);
    let index = LtfsIndex::parse(DP57).unwrap();
    let label = LtfsLabel::new(index.volume_uuid, 'a', 512 << 10, "real EE fixture".into());
    let candidate = IndexCandidate {
        start_block: 57,
        end_block: 58,
        index,
    };
    let mut notes = Vec::new();
    // 只保留触发故障的相邻两份真实 Full，较旧历史由预算明确截断。
    let result = verify_chain(
        &TapeDrive::new(&dev),
        &label,
        &candidate,
        &RecoveryBudget {
            max_chain_len: 1,
            ..Default::default()
        },
        &mut notes,
    )
    .unwrap();
    assert!(result.0, "真实 EE 同代 Recovery Full 应可通过：{notes:?}");
    assert_eq!((result.1, result.2), (1, true));
}

#[test]
fn same_generation_does_not_accept_changed_views_unknown_fields_or_deltas() {
    let original = std::str::from_utf8(DP54).unwrap();
    let variants = [
        original.replace(
            "<highestfileuid>41</highestfileuid>",
            "<highestfileuid>42</highestfileuid>",
        ),
        original.replace("<length>1048576</length>", "<length>1048575</length>"),
        original.replace("</ltfsindex>", "<unknownfuture/></ltfsindex>"),
        std::str::from_utf8(DP62)
            .unwrap()
            .replace(
                "<generationnumber>15</generationnumber>",
                "<generationnumber>14</generationnumber>",
            )
            .replace("<startblock>62</startblock>", "<startblock>54</startblock>"),
    ];
    for xml in variants {
        let sim = SimLibrary::new(1, 1, 0);
        sim.insert_cartridge(SimCartridge::blank("EE0017L9", 64 << 20), 0)
            .unwrap();
        sim.load_into_drive("EE0017L9", 0).unwrap();
        sim.with_cartridge_mut("EE0017L9", |cart| {
            cart.partitions.push(cart.partitions[0].clone());
            let dp = &mut cart.partitions[1];
            dp.objects = vec![LogicalObject::Record(vec![]); 56];
            dp.objects[53] = LogicalObject::Filemark;
            dp.objects[54] = LogicalObject::Record(xml.into_bytes());
            dp.objects[55] = LogicalObject::Filemark;
            dp.flushed = dp.objects.len();
        });
        let dev = sim.drive(0);
        let index = LtfsIndex::parse(DP57).unwrap();
        let label = LtfsLabel::new(index.volume_uuid, 'a', 512 << 10, "real EE fixture".into());
        let mut notes = Vec::new();
        let result = verify_chain(
            &TapeDrive::new(&dev),
            &label,
            &IndexCandidate {
                start_block: 57,
                end_block: 58,
                index,
            },
            &RecoveryBudget::default(),
            &mut notes,
        )
        .unwrap();
        assert!(!result.0, "有差异的同代链不能放行: {notes:?}");
    }
}

#[test]
fn forward_and_self_references_are_rejected_before_reading() {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("EE0017L9", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("EE0017L9", 0).unwrap();
    let dev = sim.drive(0);
    for block in [57, 58] {
        let mut index = LtfsIndex::parse(DP57).unwrap();
        index.previous_location.as_mut().unwrap().start_block = block;
        let label = LtfsLabel::new(index.volume_uuid, 'a', 512 << 10, "real EE fixture".into());
        let before = dev.command_log().len();
        let mut notes = Vec::new();
        let result = verify_chain(
            &TapeDrive::new(&dev),
            &label,
            &IndexCandidate {
                start_block: 57,
                end_block: 58,
                index,
            },
            &RecoveryBudget::default(),
            &mut notes,
        )
        .unwrap();
        assert!(!result.0);
        assert_eq!(dev.command_log().len(), before);
    }
}
