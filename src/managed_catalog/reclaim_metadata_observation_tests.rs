use super::*;
use crate::ltfs::index::FileNode;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{RecoveryBudget, capture_index_source, capture_recorded_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;
use metadata_plan::Plan;
use std::io::Cursor;

#[test]
fn full_observation_rejects_a_different_previous_sha_at_the_same_location() {
    observe(false, false);
}

#[test]
fn full_observation_binds_the_original_delta_chain() {
    observe(true, false);
}

#[test]
fn delta_observation_binds_the_original_delta_chain() {
    observe(true, true);
}

fn observe(previous_delta: bool, current_delta: bool) {
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("OBS000L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("OBS000L8", 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "OBS000".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume
        .append_file("/body", &mut Cursor::new(b"body"))
        .unwrap();
    volume.commit().unwrap();
    if previous_delta {
        volume.set_root_xattr("test.previous", "delta").unwrap();
        volume.commit_incremental().unwrap();
    }
    drop(volume);
    let label = read_label_only(&device).unwrap();
    let drive = TapeDrive::new(&device);
    let budget = RecoveryBudget::default();
    let before = capture_index_source(&drive, &label, &budget).unwrap();
    let donor_entry = before
        .entry_by_uid(before.header().highest_file_uid)
        .unwrap()
        .unwrap();
    let IndexEntry::File { node: donor, .. } = &donor_entry else {
        panic!("body")
    };
    let uid = before.header().highest_file_uid + 1;
    let expected = FileNode::reclaimed_reference("raw", uid, donor, &donor_entry).unwrap();
    let mut tree = TargetTree::new(label.volume_uuid);
    before
        .for_each_entry(|entry| {
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
        volume: label.volume_uuid.to_string(),
        previous,
        previous_dp_eod: current_proof(before.scope(), before.header()).unwrap().0,
        previous_source: before.identity().unwrap().to_bytes(),
        donor: Object::capture(donor_entry.clone()).unwrap(),
        expected: Object::capture(IndexEntry::File {
            path: "raw".into(),
            node: expected,
        })
        .unwrap(),
        physical_files: 1,
        target_objects: tree.objects,
        target_tree: tree.finish(),
    });
    plan.encode().unwrap();
    for eod in [
        0,
        before.construct().location.start_block,
        before.construct().location.start_block + 1,
        u64::MAX,
    ] {
        let mut invalid = plan.clone();
        invalid.0.previous_dp_eod = eod;
        assert!(invalid.encode().is_err(), "越界 EOD 必须拒绝: {eod}");
    }
    let mut legacy = plan.clone();
    legacy.0.version = 2;
    assert!(legacy.encode().is_err(), "首版计划直接升级，无旧格式兼容");
    let mut shifted = plan.clone();
    shifted.0.previous_dp_eod += 1;
    shifted.encode().unwrap();
    assert_ne!(
        shifted.fingerprint().unwrap(),
        plan.fingerprint().unwrap(),
        "原 EOD 必须绑定固定计划摘要"
    );
    assert!(shifted.observe_current(&before, None).is_err());
    let unchanged = plan.observe_current(&before, None).unwrap();
    assert_eq!(unchanged.kind(), ReclaimMetadataObservationKind::Unchanged);
    check_codec(&plan, &unchanged);
    let key = crate::scsi::reservation::ReservationKey::new(1, 7);
    crate::scsi::reservation::fence(&device, key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    let mut invalid = plan.clone();
    invalid.0.source.sha256[0] ^= 1;
    assert!(
        invalid
            .stage_reference(&mut volume, &before, &before)
            .is_err()
    );
    plan.stage_reference(&mut volume, &before, &before).unwrap();
    if current_delta {
        volume.commit_incremental().unwrap();
    } else {
        volume.commit().unwrap();
    }
    drop(volume);
    let actual = capture_index_source(&drive, &label, &budget).unwrap();
    assert_eq!(actual.construct().incremental, current_delta);
    let predecessor =
        capture_recorded_index_source(&drive, &label, &plan.previous_source().unwrap(), &budget)
            .unwrap();
    let commands = device.command_log().len();
    let committed = plan.observe_current(&actual, Some(&predecessor)).unwrap();
    assert!(
        shifted
            .observe_current(&actual, Some(&predecessor))
            .is_err(),
        "提交必须恰从原 EOD 追加，不能跳过不明尾部"
    );
    assert_eq!(committed.kind(), ReclaimMetadataObservationKind::Committed);
    check_codec(&plan, &committed);
    assert_eq!(plan.observe_current(&actual, None).is_ok(), current_delta);
    if !current_delta {
        assert!(
            plan.observe_current(&actual, Some(&before)).is_err(),
            "缓存的旧 Current 不能充当历史实读"
        );
    }
    let mut changed = plan.clone();
    let mut head = before.construct();
    head.sha256[0] ^= 1;
    changed.0.previous.clear();
    active_volume::encode_index(&mut changed.0.previous, head.into());
    changed.0.previous_source =
        crate::ltfs::recovery::IndexSourceIdentity::new(head, before.predecessors().to_vec())
            .unwrap()
            .to_bytes();
    changed.encode().unwrap();
    assert!(
        changed
            .observe_current(&actual, Some(&predecessor))
            .is_err(),
        "相同前驱位置不能替代原始 SHA 的实读证明"
    );
    if previous_delta {
        let mut changed = plan.clone();
        let mut chain = before.predecessors().to_vec();
        chain.last_mut().unwrap().sha256[0] ^= 1;
        changed.0.previous_source =
            crate::ltfs::recovery::IndexSourceIdentity::new(before.construct(), chain)
                .unwrap()
                .to_bytes();
        changed.encode().unwrap();
        assert!(changed.observe_current(&before, None).is_err());
        assert!(
            changed
                .observe_current(&actual, Some(&predecessor))
                .is_err(),
            "旧 delta 所需链的 SHA 也须绑定"
        );
    }
    assert_eq!(
        device.command_log().len(),
        commands,
        "判定本身不发送设备命令"
    );
}

fn check_codec(plan: &ReclaimMetadataPlan, observation: &ReclaimMetadataObservation) {
    let raw = observation.encode(plan).unwrap();
    assert_eq!(
        ReclaimMetadataObservation::decode(plan, &raw).unwrap(),
        *observation
    );
    for end in 0..raw.len() {
        assert!(ReclaimMetadataObservation::decode(plan, &raw[..end]).is_err());
    }
    let mut changed = raw.clone();
    changed.push(0);
    assert!(ReclaimMetadataObservation::decode(plan, &changed).is_err());
    for offset in std::iter::once(51).chain(0..raw.len()) {
        let mut changed = raw.clone();
        changed[offset] ^= 1;
        assert!(
            ReclaimMetadataObservation::decode(plan, &changed).is_err(),
            "完整观测快照损坏必须拒绝，offset={offset}"
        );
    }
    if observation.kind() == ReclaimMetadataObservationKind::Committed {
        let mut wrong = observation.clone();
        wrong.current.location.start_block += 1;
        // Keep a valid private checksum so semantic append-boundary validation,
        // rather than byte-integrity checking, must reject the retained fact.
        let mut wrong_raw = raw[..34].to_vec();
        active_volume::encode_index(&mut wrong_raw, wrong.current);
        wrong_raw.extend_from_slice(&wrong.physical_files.to_be_bytes());
        wrong_raw.extend_from_slice(&ReclaimMetadataObservation::checksum(&wrong_raw));
        assert!(
            ReclaimMetadataObservation::decode(plan, &wrong_raw).is_err(),
            "正确校验和不能使偏离原 EOD+1 的 Committed 合法"
        );
        assert!(wrong.encode(plan).is_err());
    }
    let mut changed = observation.clone();
    changed.current.volume_uuid = Uuid::new_v4();
    assert!(changed.encode(plan).is_err());
}
