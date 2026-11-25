//! Actual Full/Delta physical captures at the pure final-tree observer seam.
//! Synthetic recipe data grants no group admission, I/O or publication scope.
use super::*;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;
fn plan_for(capture: &CapturedIndexSource) -> ReclaimIndexAugmentation {
    let construct = capture.construct();
    let mut previous = Vec::new();
    active_volume::encode_index(&mut previous, construct.into());
    let mut tree = source::TargetTree::new(construct.volume_uuid);
    let mut files = 0;
    capture
        .for_each_entry(|entry| {
            files += u64::from(matches!(entry, IndexEntry::File { .. }));
            tree.add_entry(entry)
                .map_err(|error| crate::error::TapeError::Catalog(error.to_string()))
        })
        .unwrap();
    let objects = tree.objects();
    let target_tree = tree.finish();
    let group = Uuid::new_v4().to_string();
    let intent = ReclaimIndexIntent {
        intent: Intent {
            version: 2,
            checkpoint_policy: GroupCheckpointPolicy::FullDp,
            parent: "a".repeat(64),
            group: group.clone(),
            batch: group,
            members: [1; 32],
            source: capture.identity().unwrap().to_bytes(),
            destination: ReclaimDriveBinding {
                barcode: "FULLPOL8".into(),
                volume: construct.volume_uuid,
                revision: 1,
                drive_serial: "SIM-POLICY".into(),
                address: 1,
                home: 2,
            },
            scan: Uuid::new_v4().to_string(),
            previous,
            previous_source: capture.identity().unwrap().to_bytes(),
            previous_dp_eod: capture.construct_end(),
            additional_files: 0,
            payloads: Vec::new(),
            root: None,
        },
        members: Vec::new(),
    };
    ReclaimIndexAugmentation {
        actual: Actual {
            version: 1,
            intent: intent.fingerprint().unwrap(),
            receipts: [2; 32],
            members: Vec::new(),
            generation: construct.generation,
            index_start: construct.location.start_block,
            highest_uid: capture.header().highest_file_uid,
            target_tree,
            target_objects: objects,
            target_files: files,
            root: None,
        },
        intent,
        members: Vec::new(),
    }
}
#[test]
fn same_complete_tree_delta_cannot_be_group_final_observation() {
    let root = std::env::temp_dir().join(format!("reclaim-full-policy-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("FULLPOL8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("FULLPOL8", 0).unwrap();
    let device = library.drive(0);
    TapeDrive::new(&device).load().unwrap();
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "FULLPO".into(),
            block_size: 4096,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    for at in 0..32 {
        volume.create_directory(&format!("/node-{at:02}")).unwrap();
    }
    volume.sync().unwrap();
    volume.create_directory("/last-node").unwrap();
    volume.sync_batch().unwrap();
    assert!(
        volume.index().incremental,
        "nonempty adaptive control must genuinely produce Delta"
    );
    let label = read_label_only(&device).unwrap();
    let mut budget = crate::ltfs::recovery::RecoveryBudget::default().with_complete_full_history();
    budget.index_scratch.directory = root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let delta = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&device),
        &label,
        &budget,
    )
    .unwrap();
    assert!(delta.construct().incremental);
    let delta_plan = plan_for(&delta);
    // Same materialized root/all entries/UIDs/counts match the recipe; only
    // the raw checkpoint kind should disqualify it as a group final proof.
    assert!(
        delta_plan.observe(&delta).is_err(),
        "group final observer accepted a same-tree adaptive Delta"
    );
    let delta_identity = delta.identity().unwrap();
    let recorded_delta = crate::ltfs::recovery::capture_recorded_index_source(
        &TapeDrive::new(&device),
        &label,
        &delta_identity,
        &budget,
    )
    .unwrap();
    assert!(
        delta_plan
            .verify_dp_repair_candidate(&recorded_delta)
            .is_err(),
        "DP-only repair candidate accepted raw Delta"
    );
    let delta_base = *delta_identity.predecessors().last().unwrap();
    let delta_end = delta.construct_end();
    drop(recorded_delta);
    drop(delta);
    volume.commit().unwrap();
    let full = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&device),
        &label,
        &budget,
    )
    .unwrap();
    assert!(!full.construct().incremental);
    let mut full_plan = plan_for(&full);
    // Full policy governs the new checkpoint, not its valid Delta baseline.
    full_plan.intent.intent.previous.clear();
    active_volume::encode_index(
        &mut full_plan.intent.intent.previous,
        delta_identity.head().into(),
    );
    full_plan.intent.intent.previous_source = delta_identity.to_bytes();
    full_plan.intent.intent.previous_dp_eod = delta_end;
    full_plan.actual.intent = full_plan.intent.fingerprint().unwrap();
    assert_eq!(full_plan.actual.target_tree, delta_plan.actual.target_tree);
    assert!(
        full_plan.observe(&full).is_ok(),
        "actual Full tree control must remain accepted"
    );
    let observed = full_plan.observe(&full).unwrap().0;
    observed.require_full_dp(&full_plan).unwrap();
    let mut forged = observed.clone();
    let mut head = full.construct();
    head.incremental = true;
    forged.source = IndexSourceIdentity::new(head, vec![delta_base])
        .unwrap()
        .to_bytes();
    forged.index.clear();
    active_volume::encode_index(&mut forged.index, head.into());
    let decoded: Observed = native_decode(&native_bytes(&forged).unwrap()).unwrap();
    assert!(
        decoded.require_full_dp(&full_plan).is_err(),
        "stored same-head proof with forgedDelta must reject"
    );
    let recorded_full = crate::ltfs::recovery::capture_recorded_index_source(
        &TapeDrive::new(&device),
        &label,
        &full.identity().unwrap(),
        &budget,
    )
    .unwrap();
    assert!(
        full_plan.verify_dp_repair_candidate(&recorded_full).is_ok(),
        "Recorded Full repair candidate must remain accepted without minting S4"
    );
    drop(recorded_full);
    drop(full);
    drop(volume);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn intent2_policy_is_required_canonical_and_old_private_intent1_is_not_migrated() {
    // Pure codec data, not a member/admission/capture capability.
    let volume = Uuid::new_v4();
    let source = IndexSourceIdentity::new(
        crate::ltfs::volume::CommittedIndexConstruct {
            volume_uuid: volume,
            generation: 1,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 5,
            },
            sha256: [1; 32],
            incremental: false,
        },
        Vec::new(),
    )
    .unwrap();
    let mut previous = Vec::new();
    active_volume::encode_index(&mut previous, source.head().into());
    let group = Uuid::new_v4().to_string();
    let intent = Intent {
        version: 2,
        checkpoint_policy: GroupCheckpointPolicy::FullDp,
        parent: "a".repeat(64),
        group: group.clone(),
        batch: group,
        members: [2; 32],
        source: source.to_bytes(),
        destination: ReclaimDriveBinding {
            barcode: "POLICYL8".into(),
            volume,
            revision: 1,
            drive_serial: "SIM-POLICY".into(),
            address: 1,
            home: 2,
        },
        scan: Uuid::new_v4().to_string(),
        previous,
        previous_source: source.to_bytes(),
        previous_dp_eod: 8,
        additional_files: 0,
        payloads: Vec::new(),
        root: None,
    };
    let raw = native_bytes(&intent).unwrap();
    assert_eq!(decode_index_intent(&raw).unwrap(), intent);
    let canonical = serde_json::to_value(&intent).unwrap();
    assert_eq!(canonical["checkpoint_policy"], "full_dp");
    for mutation in 0..6 {
        let mut value = canonical.clone();
        match mutation {
            0 => {
                value["version"] = json!(1);
                value.as_object_mut().unwrap().remove("checkpoint_policy");
            }
            1 => {
                value.as_object_mut().unwrap().remove("checkpoint_policy");
            }
            2 => value["checkpoint_policy"] = json!("delta"),
            3 => value["version"] = json!(3),
            4 => value["checkpoint_policy"] = json!(true),
            _ => value["unexpected"] = json!(true),
        };
        assert!(
            decode_index_intent(&serde_json::to_vec(&value).unwrap()).is_err(),
            "policy mutation{mutation}"
        );
    }
    let data = ReclaimIndexIntent {
        intent,
        members: Vec::new(),
    };
    data.require_full_dp_checkpoint_policy().unwrap();
    data.require_full_dp_construct(source.head()).unwrap();
    let mut legacy = data.clone();
    legacy.intent.version = 1;
    assert!(legacy.require_full_dp_checkpoint_policy().is_err());
    assert_ne!(
        legacy.fingerprint().unwrap(),
        data.fingerprint().unwrap(),
        "policy/version is bound into attempt/Actual seal"
    );
}
