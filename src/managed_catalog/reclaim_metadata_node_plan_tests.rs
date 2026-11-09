use super::super::metadata_discard::tests::{capture, original, setup};
use super::*;
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::reservation::ReservationKey;

fn reservation(source: &CapturedIndexSource) -> ReclaimReservation {
    let ns = Uuid::new_v4();
    let mut head = Vec::new();
    active_volume::encode_index(&mut head, source.construct().into());
    ReclaimReservation {
        route: ActiveLibraryRoute {
            route: PreparedLibraryRoute {
                namespace: ns,
                revision: 1,
            },
            frozen_index: 1,
            certificate_sha256: [0; 32],
            initial_owner_epoch: 7,
        },
        intent: json!({"version":1,"library":"SIM-NODE-RECLAIM","namespace":ns.to_string(),
            "epoch":1,"operation":Uuid::new_v4().to_string(),"scan":Uuid::new_v4().to_string(),
            "scan_generation":source.construct().generation,"index":head,
            "change":{"kind":"media-reclaim","pool":Uuid::new_v4().to_string(),
                "barcode":"DSC000L8","volume":source.construct().volume_uuid.to_string(),
                "revision":1,"confirm_format":true,"unassign":false}}),
    }
}

#[test]
fn node_plan_binds_root_directory_and_native_link_across_full_and_delta_commits() {
    for old_delta in [false, true] {
        for new_delta in [false, true] {
            for uid in [1, 3, 4] {
                for path in ["reclaimed-node", "missing/parent/reclaimed-node"] {
                    let source = setup(false);
                    let target = setup(old_delta);
                    let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
                    let source_node = ReclaimSourceNode(
                        Object::capture(source.before.entry_by_uid(uid).unwrap().unwrap()).unwrap(),
                    );
                    let parent = reservation(&source.before);
                    let plan = inventory
                        .plan_reclaimed_node(&parent, &source_node, &target.before, path)
                        .unwrap();
                    assert!(plan.is_node_metadata());
                    assert_eq!(plan.added_files(), u64::from(uid == 4));
                    assert_eq!(
                        plan.observe_current(&target.before, None).unwrap().kind(),
                        ReclaimMetadataObservationKind::Unchanged
                    );
                    let before = target.device.command_log().len();
                    assert!(
                        inventory
                            .plan_reclaimed_node(
                                &parent,
                                &source_node,
                                &target.before,
                                "body/child"
                            )
                            .is_err()
                    );
                    assert!(
                        inventory
                            .plan_reclaimed_node(&parent, &source_node, &target.before, "empty")
                            .is_err()
                    );
                    assert!(
                        inventory
                            .plan_reclaimed_node(
                                &parent,
                                &source_node,
                                &source.before,
                                "reclaimed-node"
                            )
                            .is_err()
                    );
                    assert_eq!(target.device.command_log().len(), before, "计划阶段不发CDB");
                    let mut volume = LtfsVolume::mount(&target.device).unwrap();
                    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
                    plan.stage_reference(&mut volume, &source.before, &target.before)
                        .unwrap();
                    assert!(volume.index().find_directory(path).is_none());
                    assert!(volume.index().find_file(path).is_none());
                    if new_delta {
                        volume.commit_incremental().unwrap();
                    } else {
                        volume.commit().unwrap();
                    }
                    drop(volume);
                    target.library.power_cut();
                    let actual = capture(&target.device);
                    let previous = original(&target);
                    let result = plan.observe_current(&actual, Some(&previous)).unwrap();
                    assert_eq!(result.kind(), ReclaimMetadataObservationKind::Committed);
                    assert_eq!(
                        result.physical_files(),
                        plan.0.physical_files + u64::from(uid == 4)
                    );
                    assert_eq!(
                        Object::capture(actual.entry_by_uid(plan.uid()).unwrap().unwrap()).unwrap(),
                        plan.0.expected
                    );
                    let mut wrong = plan.clone();
                    wrong.0.input[0] ^= 1;
                    assert!(
                        wrong.encode().is_err(),
                        "目录/链接不能伪装成正文 donor 计划"
                    );
                }
            }
        }
    }
}

#[test]
fn node_plan_rejects_unrelated_metadata_changes_in_the_same_commit() {
    let source = setup(false);
    let target = setup(false);
    let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
    let node = ReclaimSourceNode(
        Object::capture(source.before.entry_by_uid(3).unwrap().unwrap()).unwrap(),
    );
    let plan = inventory
        .plan_reclaimed_node(
            &reservation(&source.before),
            &node,
            &target.before,
            "reclaimed-node",
        )
        .unwrap();
    let mut volume = LtfsVolume::mount(&target.device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    plan.stage_reference(&mut volume, &source.before, &target.before)
        .unwrap();
    volume.set_root_xattr("unrelated", "changed").unwrap();
    volume.commit().unwrap();
    drop(volume);
    assert!(
        plan.observe_current(&capture(&target.device), Some(&original(&target)))
            .is_err()
    );
}

#[test]
fn native_link_collision_preserves_original_target_file_and_all_attributes() {
    for delta in [false, true] {
        let source = setup(false);
        let target = setup(false);
        let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
        let node = ReclaimSourceNode(
            Object::capture(source.before.entry_by_uid(4).unwrap().unwrap()).unwrap(),
        );
        let parent = reservation(&source.before);
        let old_link = target.before.entry_by_uid(4).unwrap().unwrap();
        let IndexEntry::File {
            path: link_path, ..
        } = old_link
        else {
            panic!("link")
        };
        let commands = target.device.command_log().len();
        assert!(
            inventory
                .plan_reclaimed_node(&parent, &node, &target.before, &link_path)
                .is_err(),
            "相对链接不能换父目录后假装仍保持语义"
        );
        assert_eq!(target.device.command_log().len(), commands);
        let old = target.before.entry_by_path("body").unwrap().unwrap();
        let plan = inventory
            .plan_reclaimed_node(&parent, &node, &target.before, "body")
            .unwrap();
        let mut volume = LtfsVolume::mount(&target.device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
        plan.stage_reference(&mut volume, &source.before, &target.before)
            .unwrap();
        if delta {
            volume.commit_incremental().unwrap();
        } else {
            volume.commit().unwrap();
        }
        drop(volume);
        target.library.power_cut();
        let actual = capture(&target.device);
        assert_eq!(
            plan.observe_current(&actual, Some(&original(&target)))
                .unwrap()
                .kind(),
            ReclaimMetadataObservationKind::Committed
        );
        let crate::ltfs::index::IndexEntry::File { node: mut old, .. } = old else {
            panic!("body")
        };
        let relocated = actual.entry_by_uid(old.meta.file_uid).unwrap().unwrap();
        let crate::ltfs::index::IndexEntry::File {
            path,
            node: preserved,
        } = relocated
        else {
            panic!("old body")
        };
        assert_ne!(path, "body");
        old.name = preserved.name.clone();
        assert_eq!(
            preserved, old,
            "原 UID、正文位置和全部属性保持，只有归档名称改变"
        );
        assert_eq!(
            Object::capture(actual.entry_by_path("body").unwrap().unwrap()).unwrap(),
            plan.0.expected
        );
    }
}
