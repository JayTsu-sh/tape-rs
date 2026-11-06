use super::super::physical_source::tests::fixture;
use super::super::volume_projection::tests::build;
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

fn prepare() -> (LtfsIndex, NodeRenameVolumeProjection, NodeRenameMediaPlan) {
    prepare_fixture(fixture())
}
fn prepare_fixture(
    mut f: super::super::physical_source::tests::Fixture,
) -> (LtfsIndex, NodeRenameVolumeProjection, NodeRenameMediaPlan) {
    let view = build(&mut f, 16 << 20).unwrap();
    let request = view.original_request();
    let object = NodePlanObject::for_bytes(
        request.library(),
        request.spec().source.namespace,
        &request.encode().unwrap(),
        8 << 20,
    )
    .unwrap();
    let facts = volume_plan::Facts::from_projection(
        &view,
        &URL_SAFE_NO_PAD.encode(object.encode()),
        [5; 32],
        [6; 32],
        1,
        10,
    )
    .unwrap();
    let recipe = NodeRenameVolumePlan {
        record: volume_plan::Record::new(facts, 1).unwrap(),
    };
    let plan = NodeRenameMediaPlan::new(recipe, Uuid::new_v4(), 1).unwrap();
    (f.index, view, plan)
}
fn staged(
    before: &LtfsIndex,
    view: &NodeRenameVolumeProjection,
    plan: &NodeRenameMediaPlan,
) -> (LtfsIndex, ActiveIndexIdentity) {
    plan.require_source(before, 16 << 20).unwrap();
    plan.require_projection(view).unwrap();
    let mut after = before.clone();
    after.root = Default::default();
    plan.stage_tree(&mut after, view).unwrap();
    after.generation += 1;
    after.self_location.start_block += 100;
    after.incremental = false;
    let current = ActiveIndexIdentity {
        volume_uuid: after.volume_uuid,
        generation: after.generation,
        location: after.self_location,
        sha256: [17; 32],
        incremental: false,
    };
    (after, current)
}
#[test]
fn media_projection_binds_complete_tree_original_attempt_and_preserves_shared_data() {
    let (before, view, plan) = prepare();
    let (mut after, current) = staged(&before, &view, &plan);
    let evidence = plan.evidence(&after, current, 16 << 20).unwrap();
    assert_eq!(evidence.current_index(), current);
    assert_eq!(evidence.plan(), &plan);
    for path in ["/.LTFSEE_DATA/object", "/other", "/native", "/high"] {
        assert_eq!(before.find_file(path), after.find_file(path));
    }
    assert!(after.find_file("/src/a").is_none());
    assert_eq!(after.find_file("/dst/a").unwrap().meta.file_uid, 22);
    assert_eq!(after.find_file("/dst/b").unwrap().meta.file_uid, 23);
    after.root.files.reverse();
    after.root.subdirs.reverse();
    plan.evidence(&after, current, 16 << 20).unwrap();
    assert!(NodeRenameMediaPlan::new(plan.recipe.clone(), Uuid::nil(), 1).is_err());
    assert!(NodeRenameMediaPlan::new(plan.recipe.clone(), Uuid::new_v4(), 0).is_err());
    for other in [
        NodeRenameMediaPlan::new(plan.recipe.clone(), plan.attempt, 2).unwrap(),
        NodeRenameMediaPlan::new(plan.recipe.clone(), Uuid::new_v4(), 1).unwrap(),
    ] {
        assert_ne!(other.fingerprint(), plan.fingerprint());
        assert!(other.evidence(&after, current, 16 << 20).is_err());
    }
    assert!(plan.evidence(&after, current, 1).is_err());
}
#[test]
fn media_evidence_rejects_wrong_header_marker_uid_and_unrelated_tree_changes() {
    let (before, view, plan) = prepare();
    let (after, current) = staged(&before, &view, &plan);
    let changes: [fn(&mut LtfsIndex); 13] = [
        |i| i.generation += 1,
        |i| i.volume_uuid = Uuid::new_v4(),
        |i| i.self_location.start_block += 1,
        |i| i.highest_file_uid -= 1,
        |i| i.incremental = true,
        |i| i.root.xattrs.retain(|a| a.key != RENAME_MARKER),
        |i| {
            i.root.xattrs.push(
                i.root
                    .xattrs
                    .iter()
                    .find(|a| a.key == RENAME_MARKER)
                    .unwrap()
                    .clone(),
            )
        },
        |i| {
            i.root
                .xattrs
                .iter_mut()
                .find(|a| a.key == RENAME_MARKER)
                .unwrap()
                .base64 = true
        },
        |i| {
            i.root
                .xattrs
                .iter_mut()
                .find(|a| a.key == RENAME_MARKER)
                .unwrap()
                .value = "1:".to_owned() + &"F".repeat(64)
        },
        |i| i.root.files[0].meta.file_uid = 22,
        |i| i.root.files[0].length += 1,
        |i| i.root.subdirs[0].meta.modify_time = "changed".into(),
        |i| i.unknown_elements.push("unknown".into()),
    ];
    for change in changes {
        let mut bad = after.clone();
        change(&mut bad);
        assert!(plan.evidence(&bad, current, 16 << 20).is_err());
    }
    let mut wrong = current;
    wrong.incremental = true;
    assert!(plan.evidence(&after, wrong, 16 << 20).is_err());
    let mut wrong = before.clone();
    wrong.root.files[0].length += 1;
    assert!(plan.require_source(&wrong, 16 << 20).is_err());
    let mut wrong = before.clone();
    wrong.root.xattrs.push(Xattr {
        key: RENAME_MARKER.into(),
        value: "bad".into(),
        base64: false,
    });
    assert!(plan.require_source(&wrong, 16 << 20).is_err());
}

#[test]
fn tree_rebuild_uses_component_order_and_preserves_root_name_and_prior_marker_facts() {
    use crate::ltfs::index::{DirectoryNode, FileNode, NodeMeta};
    let mut f = fixture();
    f.index.root.name = "original-root".into();
    f.index.root.xattrs.push(Xattr {
        key: RENAME_MARKER.into(),
        value: format!("1:{}", "a".repeat(64)),
        base64: false,
    });
    for (name, uid) in [("a-", 101), ("a", 102), ("a中", 103)] {
        f.index.root.subdirs.push(DirectoryNode {
            name: name.into(),
            meta: NodeMeta {
                file_uid: uid,
                ..Default::default()
            },
            files: vec![FileNode {
                name: "child".into(),
                meta: NodeMeta {
                    file_uid: uid + 10,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        });
    }
    let (before, view, plan) = prepare_fixture(f);
    let mut after = before.clone();
    after.root = DirectoryNode {
        name: before.root.name.clone(),
        ..Default::default()
    };
    plan.require_source(&before, 16 << 20).unwrap();
    plan.stage_tree(&mut after, &view).unwrap();
    assert_eq!(after.root.name, "original-root");
    for path in ["/a/child", "/a-/child", "/a中/child"] {
        assert_eq!(before.find_file(path), after.find_file(path));
    }
    assert_eq!(
        plan.summary(&after, true, 16 << 20).unwrap().0,
        plan.recipe.after_sha256()
    );
}

#[test]
fn actual_tree_summary_enforces_real_sqlite_page_budget_without_mutating_the_index() {
    use crate::ltfs::index::{FileNode, NodeMeta};
    let (mut index, _, plan) = prepare();
    for uid in 1000..4000 {
        index.root.files.push(FileNode {
            name: format!("unrelated-{uid}"),
            meta: NodeMeta {
                file_uid: uid,
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let original = index.to_xml().unwrap();
    assert!(plan.summary(&index, false, 64 << 10).is_err());
    assert!(plan.summary(&index, false, 16 << 20).is_ok());
    assert_eq!(index.to_xml().unwrap(), original);
}

#[test]
fn actual_tree_rejects_oversized_root_attributes_before_cloning_or_filtering() {
    let (mut before, _, plan) = prepare();
    before.root.xattrs.push(Xattr {
        key: "large-root".into(),
        value: "x".repeat(9 << 20),
        base64: false,
    });
    for after in [false, true] {
        assert!(plan.summary(&before, after, 16 << 20).is_err());
    }
}

#[test]
fn rename_absence_preserves_original_recovery_attribute_positions_and_full_source_hash() {
    use super::{NodeRenameAbsenceEvidence, NodeRenameAbsenceKind};
    use crate::ltfs::volume::{XATTR_ABANDONED, XATTR_EMPTY_RECOVERY, empty_recovery_marker};
    let mut f = fixture();
    let attrs = &mut f.index.root.xattrs;
    attrs.insert(
        0,
        Xattr {
            key: "root-before".into(),
            value: "first".into(),
            base64: false,
        },
    );
    attrs.insert(
        1,
        Xattr {
            key: XATTR_ABANDONED.into(),
            value: "b:1-2".into(),
            base64: false,
        },
    );
    attrs.push(Xattr {
        key: "root-between".into(),
        value: "middle".into(),
        base64: false,
    });
    attrs.push(Xattr {
        key: XATTR_EMPTY_RECOVERY.into(),
        value: "original-recovery".into(),
        base64: false,
    });
    attrs.push(Xattr {
        key: "root-after".into(),
        value: "last".into(),
        base64: false,
    });
    let (before, view, plan) = prepare_fixture(f);
    let previous = plan.recipe().source_index();
    let original = plan.absence_evidence(&before, previous, 16 << 20).unwrap();
    assert_eq!(original.kind(), NodeRenameAbsenceKind::Unchanged);
    let (committed, current) = staged(&before, &view, &plan);
    assert!(
        plan.absence_evidence(&committed, current, 16 << 20)
            .is_err()
    );
    let mut after = before.clone();
    after.generation += 1;
    after.self_location.start_block += 8;
    after.incremental = false;
    after.previous_location = Some(previous.location);
    let marker = empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
        volume_uuid: previous.volume_uuid,
        generation: previous.generation,
        location: previous.location,
        sha256: previous.sha256,
        incremental: previous.incremental,
    });
    for attr in &mut after.root.xattrs {
        if attr.key == XATTR_ABANDONED {
            attr.value = format!(
                "b:1-2;b:{}-{}",
                previous.location.start_block + 2,
                previous.location.start_block + 3
            );
        }
        if attr.key == XATTR_EMPTY_RECOVERY {
            attr.value = marker.clone();
        }
    }
    let index = ActiveIndexIdentity {
        generation: after.generation,
        location: after.self_location,
        sha256: [9; 32],
        incremental: false,
        ..previous
    };
    let proof: NodeRenameAbsenceEvidence = plan.absence_evidence(&after, index, 16 << 20).unwrap();
    assert_eq!(proof.kind(), NodeRenameAbsenceKind::Discarded);
    assert_eq!(proof.file_count(), original.file_count());
    assert_eq!(proof.plan(), &plan);
    let mut bad = after.clone();
    bad.root
        .xattrs
        .iter_mut()
        .find(|a| a.key == "root-between")
        .unwrap()
        .value = "changed".into();
    assert!(plan.absence_evidence(&bad, index, 16 << 20).is_err());
    bad = after.clone();
    bad.root
        .xattrs
        .iter_mut()
        .find(|a| a.key == XATTR_ABANDONED)
        .unwrap()
        .value = format!("b:1-2;b:1-{}", index.location.start_block);
    assert!(plan.absence_evidence(&bad, index, 16 << 20).is_err());
    bad = after;
    bad.previous_location = None;
    assert!(plan.absence_evidence(&bad, index, 16 << 20).is_err());
}
#[test]
fn rename_recovery_metadata_rejects_duplicates_base64_and_plan_budget_overflow() {
    use crate::ltfs::volume::XATTR_ABANDONED;
    let a = Xattr {
        key: XATTR_ABANDONED.into(),
        value: "b:1-2".into(),
        base64: false,
    };
    assert!(
        volume_plan::RecoveryAttribute::capture(&[a.clone(), a.clone()], XATTR_ABANDONED).is_err()
    );
    let mut bad = a.clone();
    bad.base64 = true;
    assert!(volume_plan::RecoveryAttribute::capture(&[bad], XATTR_ABANDONED).is_err());
    let mut bad = a;
    bad.value = "x".repeat(64_001);
    assert!(volume_plan::RecoveryAttribute::capture(&[bad], XATTR_ABANDONED).is_err());
    let bad = Xattr {
        key: XATTR_ABANDONED.into(),
        value: "\0".into(),
        base64: false,
    };
    assert!(volume_plan::RecoveryAttribute::capture(&[bad], XATTR_ABANDONED).is_err());
    let (before, _, mut plan) = prepare();
    let previous = plan.recipe().source_index();
    let mut oversized = plan.recipe.record.facts.clone();
    oversized.recovery_abandoned = Some(volume_plan::RecoveryAttribute {
        position: 0,
        value: "x".repeat(40_000),
    });
    oversized.recovery_marker = Some(volume_plan::RecoveryAttribute {
        position: 1,
        value: "y".repeat(40_000),
    });
    assert!(volume_plan::Record::new(oversized, plan.recipe.creator_owner_epoch()).is_err());
    plan.recipe.record.facts.before_files += 1;
    assert!(
        plan.require_absence_source(&before, previous, 16 << 20)
            .is_err()
    );
}
