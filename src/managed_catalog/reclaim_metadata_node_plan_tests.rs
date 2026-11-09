use super::super::metadata_discard::tests::{Scenario, capture, original, setup};
use super::*;
use crate::ltfs::recovery::{RecoveryBudget, capture_recorded_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::reservation::ReservationKey;
use crate::tape::commands::TapeDrive;

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

fn append_unreferenced_target_file(target: &mut Scenario, delta: bool) {
    let mut volume = LtfsVolume::mount(&target.device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    volume.append_bytes("unused", b"old unused").unwrap();
    if delta {
        volume.commit_incremental().unwrap();
    } else {
        volume.commit().unwrap();
    }
    drop(volume);
    target.before = capture(&target.device);
}

fn plan_predecessor(target: &Scenario, plan: &ReclaimMetadataPlan) -> CapturedIndexSource {
    capture_recorded_index_source(
        &TapeDrive::new(&target.device),
        &read_label_only(&target.device).unwrap(),
        &plan.previous_source().unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap()
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
        let mut target = setup(false);
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
        append_unreferenced_target_file(&mut target, false);
        let old = target.before.entry_by_path("unused").unwrap().unwrap();
        let plan = inventory
            .plan_reclaimed_node(&parent, &node, &target.before, "unused")
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
            plan.observe_current(&actual, Some(&plan_predecessor(&target, &plan)))
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
        assert_ne!(path, "unused");
        old.name = preserved.name.clone();
        assert_eq!(
            preserved, old,
            "原 UID、正文位置和全部属性保持，只有归档名称改变"
        );
        assert_eq!(
            Object::capture(actual.entry_by_path("unused").unwrap().unwrap()).unwrap(),
            plan.0.expected
        );
        let volume = LtfsVolume::mount(&target.device).unwrap();
        for link in ["alias", "unused"] {
            let mut bytes = Vec::new();
            volume.read_file_to_writer(link, &mut bytes).unwrap();
            assert_eq!(bytes, b"body", "已有链接及新链接都保持正文解析");
        }
        let mut bytes = Vec::new();
        volume.read_file_to_writer(&path, &mut bytes).unwrap();
        assert_eq!(bytes, b"old unused");
    }
}

#[test]
fn native_link_collision_preserves_existing_incoming_link() {
    for delta in [false, true] {
        for (link_path, link_target) in [
            ("alias", "body"),
            ("alias", "./body"),
            ("nested/alias", "../body"),
        ] {
            let source = setup(false);
            let mut target = setup(delta);
            if (link_path, link_target) != ("alias", "body") {
                let mut volume = LtfsVolume::mount(&target.device).unwrap();
                volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
                volume.remove_file("alias").unwrap();
                volume.add_symlink(link_path, link_target).unwrap();
                volume.commit().unwrap();
                drop(volume);
                target.before = capture(&target.device);
            }
            let mut old_body = Vec::new();
            LtfsVolume::mount(&target.device)
                .unwrap()
                .read_file_to_writer(link_path, &mut old_body)
                .unwrap();
            assert_eq!(old_body, b"body");
            let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
            let node = ReclaimSourceNode(
                Object::capture(source.before.entry_by_uid(4).unwrap().unwrap()).unwrap(),
            );
            let before = target.device.command_log().len();
            let planned = inventory.plan_reclaimed_node(
                &reservation(&source.before),
                &node,
                &target.before,
                "body",
            );
            if let Ok(plan) = &planned {
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
                let mut actual = Vec::new();
                let read = LtfsVolume::mount(&target.device)
                    .unwrap()
                    .read_file_to_writer(link_path, &mut actual);
                assert!(read.is_ok(), "合法计划破坏已有相对链接: {read:?}");
                assert_eq!(actual, old_body);
            }
            assert!(planned.is_err(), "已有相对链接引用的文件不能被改名归档");
            assert_eq!(target.device.command_log().len(), before, "拒绝时不发CDB");
        }
    }
}

#[test]
fn legacy_displacement_cannot_stage_or_publish_a_broken_incoming_link() {
    for delta in [false, true] {
        let source = setup(false);
        let mut target = setup(false);
        append_unreferenced_target_file(&mut target, delta);
        let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
        let entry = source.before.entry_by_uid(4).unwrap().unwrap();
        let node = ReclaimSourceNode(Object::capture(entry.clone()).unwrap());
        let parent = reservation(&source.before);
        let mut legacy = inventory
            .plan_reclaimed_node(&parent, &node, &target.before, "unused")
            .unwrap();
        // Reconstruct the otherwise valid recipe produced before the incoming
        // link guard. Keep the same original target and exact tree fingerprint.
        let old = target.before.entry_by_path("body").unwrap().unwrap();
        let original = Object::capture(old.clone()).unwrap();
        let archive = format!(
            "_reclaim_displaced_{}_{}",
            parent.fingerprint(),
            original.uid
        );
        legacy.0.displaced = Some(Scaffold {
            original,
            expected: Object::capture(moved_file(old, &archive).unwrap()).unwrap(),
        });
        legacy.0.expected =
            Object::capture(entry.reclaimed_metadata_node("body", legacy.uid()).unwrap()).unwrap();
        legacy.encode().unwrap();
        let before = target.device.command_log().len();
        assert_eq!(
            legacy.observe_current(&target.before, None).unwrap().kind(),
            ReclaimMetadataObservationKind::Unchanged,
            "拒绝后续写入不应阻止实际 NoWrite 观察及旧意图结清"
        );
        assert_eq!(target.device.command_log().len(), before);
        let mut volume = LtfsVolume::mount(&target.device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
        let before = target.device.command_log().len();
        assert!(
            legacy
                .stage_reference(&mut volume, &source.before, &target.before)
                .is_err()
        );
        assert_eq!(target.device.command_log().len(), before);
        // Simulate an unsafe result already written by the old implementation.
        // Recovery must not publish it or count it toward source coverage.
        volume.displace_reclaimed_file("body", 2, &archive).unwrap();
        let relocated = node_metadata::relocation_source(&entry, "body").unwrap();
        volume
            .reference_reclaimed_node("body", legacy.uid(), &relocated)
            .unwrap();
        if delta {
            volume.commit_incremental().unwrap();
        } else {
            volume.commit().unwrap();
        }
        drop(volume);
        target.library.power_cut();
        let actual = capture(&target.device);
        let previous = plan_predecessor(&target, &legacy);
        let before = target.device.command_log().len();
        assert!(legacy.observe_current(&actual, Some(&previous)).is_err());
        assert_eq!(target.device.command_log().len(), before);
    }
}
