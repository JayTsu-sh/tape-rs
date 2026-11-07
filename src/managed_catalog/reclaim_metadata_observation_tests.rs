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
        version: 2,
        parent: "01".repeat(32),
        inventory: [1; 32],
        source: Object::capture(donor_entry.clone()).unwrap(),
        source_index: previous.clone(),
        input: [2; 32],
        volume: label.volume_uuid.to_string(),
        previous,
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
    assert_eq!(
        plan.observe_current(&before, None).unwrap().kind(),
        ReclaimMetadataObservationKind::Unchanged
    );
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume
        .reference_reclaimed_body("raw", uid, "body", donor.meta.file_uid, &donor_entry)
        .unwrap();
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
    assert_eq!(
        plan.observe_current(&actual, Some(&predecessor))
            .unwrap()
            .kind(),
        ReclaimMetadataObservationKind::Committed
    );
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
