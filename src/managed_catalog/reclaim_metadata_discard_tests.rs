use super::*;
use crate::ltfs::index::FileNode;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{
    RecoveryBudget, TailKind, capture_index_source, capture_recorded_index_source,
};
use crate::ltfs::volume::{LtfsVolume, TailPolicy, read_label_only};
use crate::scsi::reservation::{ReservationKey, fence};
use crate::scsi::sim::{LogicalObject, SimCartridge, SimLibrary, SimTransport};
use crate::tape::commands::TapeDrive;
use metadata_plan::Plan;

struct Scenario {
    library: SimLibrary,
    device: SimTransport,
    before: CapturedIndexSource,
    plan: ReclaimMetadataPlan,
}
fn capture(device: &SimTransport) -> CapturedIndexSource {
    capture_index_source(
        &TapeDrive::new(device),
        &read_label_only(device).unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap()
}
fn original(s: &Scenario) -> CapturedIndexSource {
    capture_recorded_index_source(
        &TapeDrive::new(&s.device),
        &read_label_only(&s.device).unwrap(),
        &s.plan.previous_source().unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap()
}
fn setup(delta: bool) -> Scenario {
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("DSC000L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("DSC000L8", 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "DSC000".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let key = ReservationKey::new(1, 7);
    fence(&device, key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    volume.append_bytes("/body", b"body").unwrap();
    volume.create_directory("/empty").unwrap();
    volume.add_symlink("/alias", "body").unwrap();
    volume.set_root_xattr("test.root", "retained-root").unwrap();
    volume.set_root_xattr(XATTR_ABANDONED, "b:4-4").unwrap();
    volume
        .set_root_xattr(XATTR_EMPTY_RECOVERY, "older-marker")
        .unwrap();
    volume
        .set_node_xattr(
            "/empty",
            Xattr {
                key: "test.dir".into(),
                value: "retained-dir".into(),
                base64: false,
            },
            false,
            false,
        )
        .unwrap();
    volume.commit().unwrap();
    if delta {
        volume
            .set_root_xattr("test.delta", "retained-delta")
            .unwrap();
        volume.commit_incremental().unwrap();
    }
    drop(volume);
    let before = capture(&device);
    assert_eq!(before.construct().incremental, delta);
    let donor_entry = before.entry_by_uid(2).unwrap().unwrap();
    let IndexEntry::File { node: donor, .. } = &donor_entry else {
        panic!("body")
    };
    let expected = FileNode::reclaimed_reference(
        "raw",
        before.header().highest_file_uid + 1,
        donor,
        &donor_entry,
    )
    .unwrap();
    let mut tree = TargetTree::new(before.construct().volume_uuid);
    let mut files = 0;
    before
        .for_each_entry(|entry| {
            if matches!(&entry, IndexEntry::File { .. }) {
                files += 1;
            }
            tree.add(&Object::capture(entry).map_err(callback)?)
                .map_err(callback)
        })
        .unwrap();
    let mut previous = Vec::new();
    active_volume::encode_index(&mut previous, before.construct().into());
    let plan = ReclaimMetadataPlan(Plan {
        version: 3,
        parent: "01".repeat(32),
        inventory: [1; 32],
        source: Object::capture(donor_entry.clone()).unwrap(),
        source_index: previous.clone(),
        input: [2; 32],
        volume: before.construct().volume_uuid.to_string(),
        previous,
        previous_dp_eod: current_proof(before.scope(), before.header()).unwrap().0,
        previous_source: before.identity().unwrap().to_bytes(),
        donor: Object::capture(donor_entry).unwrap(),
        expected: Object::capture(IndexEntry::File {
            path: "raw".into(),
            node: expected,
        })
        .unwrap(),
        physical_files: files,
        target_objects: tree.objects,
        target_tree: tree.finish(),
    });
    plan.encode().unwrap();
    Scenario {
        library,
        device,
        before,
        plan,
    }
}
fn discard(s: &Scenario, truncated: bool) -> CapturedIndexSource {
    let drive = TapeDrive::new(&s.device);
    drive.locate(1, s.plan.previous_dp_eod(), true).unwrap();
    if truncated {
        drive.write_filemark(1).unwrap();
        drive.write_block(b"<ltfsindex version=\"2.4.0\">").unwrap();
    } else {
        drive.write_block(b"unindexed-tail").unwrap();
    }
    drive.write_filemark(0).unwrap();
    let mut volume = LtfsVolume::mount_with_budget(&s.device, &RecoveryBudget::default()).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    assert_eq!(
        volume.recovery().dp.tail,
        if truncated {
            TailKind::TruncatedIndex
        } else {
            TailKind::UnindexedData
        }
    );
    assert_eq!(
        volume.recovery().dp.last_index.as_ref().unwrap().end_block + 1,
        s.plan.previous_dp_eod()
    );
    volume.close_tail(TailPolicy::Discard).unwrap();
    let mut body = Vec::new();
    volume.read_file_to_writer("/alias", &mut body).unwrap();
    assert_eq!(body, b"body");
    drop(volume);
    capture(&s.device)
}
fn replace_current(s: &Scenario, head: ActiveIndexIdentity, old: &str, new: &str) {
    s.library.with_cartridge_mut("DSC000L8", |cartridge| {
        for (partition, block) in [
            (1, head.location.start_block as usize),
            (0, crate::ltfs::volume::P1_DATA_START as usize + 1),
        ] {
            let LogicalObject::Record(raw) = &mut cartridge.partitions[partition].objects[block]
            else {
                panic!("当前 index record")
            };
            let text = std::str::from_utf8(raw).unwrap();
            assert_eq!(text.matches(old).count(), 1, "只改当前DP/IP各一个目标片段");
            *raw = text.replace(old, new).into_bytes();
        }
        cartridge.vcr += 1;
    });
}

#[test]
fn metadata_discard_observation_recognizes_full_and_delta_t1_t2_without_io() {
    for delta in [false, true] {
        for truncated in [false, true] {
            let s = setup(delta);
            let actual = discard(&s, truncated);
            let recorded = original(&s);
            let commands = s.device.command_log().len();
            let fact = s.plan.observe_discarded(&actual, &recorded).unwrap();
            assert_eq!(fact.plan_fingerprint(), s.plan.fingerprint().unwrap());
            assert_eq!(fact.current(), actual.construct().into());
            assert_eq!(fact.physical_files(), 2);
            assert_eq!(
                fact.abandoned_range(),
                s.plan.previous_dp_eod()..=actual.construct().location.start_block - 2
            );
            assert!(
                s.plan.observe_current(&actual, Some(&recorded)).is_err(),
                "修尾不是原元数据新增节点提交"
            );
            assert!(
                s.plan.observe_discarded(&actual, &s.before).is_err(),
                "旧Current缓存不是Recorded实读"
            );
            assert!(
                s.plan.observe_discarded(&recorded, &recorded).is_err(),
                "旧历史构造不能冒充当前结果"
            );
            let mut shifted = s.plan.clone();
            shifted.0.previous_dp_eod += 1;
            assert!(shifted.observe_discarded(&actual, &recorded).is_err());
            let mut wrong = s.plan.clone();
            wrong.0.target_tree[0] ^= 1;
            assert!(wrong.observe_discarded(&actual, &recorded).is_err());
            let mut wrong = s.plan.clone();
            wrong.0.donor.sha256[0] ^= 1;
            assert!(wrong.observe_discarded(&actual, &recorded).is_err());
            assert_eq!(s.device.command_log().len(), commands, "纯判定不得发送 CDB");
        }
    }
}

#[test]
fn metadata_discard_observation_rejects_actual_marker_tree_and_header_changes() {
    let s = setup(true);
    let actual = discard(&s, true);
    let recorded = original(&s);
    let head: ActiveIndexIdentity = actual.construct().into();
    let marker = empty_recovery_marker(s.before.construct());
    let range = format!(
        "b:4-4;b:{}-{}",
        s.plan.previous_dp_eod(),
        head.location.start_block - 2
    );
    let faults = [
        (marker.clone(), format!("{marker}0")),
        (
            range.clone(),
            format!(
                "b:4-4;b:0{}-{}",
                s.plan.previous_dp_eod(),
                head.location.start_block - 2
            ),
        ),
        ("retained-root".into(), "changed-root".into()),
        ("retained-dir".into(), "changed-dir".into()),
        (
            "<allowpolicyupdate>true</allowpolicyupdate>".into(),
            "<allowpolicyupdate>false</allowpolicyupdate>".into(),
        ),
    ];
    for (old, new) in faults {
        replace_current(&s, head, &old, &new);
        let changed = capture(&s.device);
        let commands = s.device.command_log().len();
        assert!(
            s.plan.observe_discarded(&changed, &recorded).is_err(),
            "实际当前构造篡改必须拒绝: {old}"
        );
        assert_eq!(s.device.command_log().len(), commands);
        replace_current(&s, head, &new, &old);
        assert!(
            s.plan
                .observe_discarded(&capture(&s.device), &recorded)
                .is_ok()
        );
    }
}

#[test]
fn metadata_discard_observation_rejects_resegmented_historical_index_boundaries() {
    let s = setup(false);
    let actual = discard(&s, true);
    let recorded = original(&s);
    s.library.with_cartridge_mut("DSC000L8", |cartridge| {
        let dp = &mut cartridge.partitions[1];
        let block = s.before.construct().location.start_block as usize;
        let LogicalObject::Record(raw) = &mut dp.objects[block] else {
            panic!("原历史索引 record")
        };
        let remainder = raw.split_off(raw.len() / 2);
        dp.objects
            .insert(block + 1, LogicalObject::Record(remainder));
        let removed = dp.objects.remove(s.plan.previous_dp_eod() as usize + 2);
        assert!(
            matches!(removed, LogicalObject::Record(_)),
            "删一条已放弃的截断 XML，保持新 Full 位置不变"
        );
        dp.flushed = dp.objects.len();
        cartridge.vcr += 1;
    });
    let changed = capture(&s.device);
    let historical = original(&s);
    assert_eq!(
        changed.identity().unwrap(),
        actual.identity().unwrap(),
        "新 Full XML/位置不变"
    );
    assert_eq!(
        historical.identity().unwrap(),
        recorded.identity().unwrap(),
        "原历史 XML/SHA/位置/链不变，仅结束 FM 后移"
    );
    assert_eq!(historical.construct_end(), s.plan.previous_dp_eod() + 1);
    let commands = s.device.command_log().len();
    assert!(
        s.plan.observe_discarded(&changed, &historical).is_err(),
        "实际历史构造闭合边界不同，不能只信恢复 marker 与 XML 身份"
    );
    assert_eq!(s.device.command_log().len(), commands);
}
