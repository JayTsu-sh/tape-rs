use super::super::metadata_discard::tests::{capture, setup};
use super::*;
use crate::ltfs::recovery::{RecoveryBudget, capture_recorded_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::reservation::ReservationKey;
use crate::tape::commands::TapeDrive;
use std::io::Cursor;

#[test]
fn unmapped_ee_alias_materializes_its_verified_data_and_preserves_user_attributes() {
    let source = setup(false);
    let target = setup(false);
    let mut volume = LtfsVolume::mount(&source.device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    volume
        .append_bytes("/.LTFSEE_DATA/raw", b"alias body bytes")
        .unwrap();
    volume.create_directory("/docs").unwrap();
    volume
        .add_symlink("/docs/ee", "../.LTFSEE_DATA/raw")
        .unwrap();
    volume
        .set_node_xattr(
            "/docs/ee",
            crate::ltfs::index::Xattr {
                key: "user.original".into(),
                value: "AAEC/w==".into(),
                base64: true,
            },
            false,
            false,
        )
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    let actual_source = capture(&source.device);
    let alias = actual_source.entry_by_path("docs/ee").unwrap().unwrap();
    let object = Object::capture(alias.clone()).unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&actual_source).unwrap();
    let parent = reservation(&actual_source);
    let (dp_eod, ip_generation, history_depth) =
        current_proof(actual_source.scope(), actual_source.header()).unwrap();
    let context = ReclaimRawReadContext {
        parent: parent.fingerprint(),
        inventory: inventory.summary.digest,
        index: inventory.summary.head.clone(),
        identity: inventory.summary.identity,
        dp_eod,
        ip_generation,
        history_depth,
        node: ReclaimSourceNode(object.clone()),
    };
    assert_eq!(
        context.node().length(),
        Some(0),
        "EE 别名长度不能冒充其正文长度"
    );
    assert!(inventory.is_ee_alias(context.node()).unwrap());
    let sha = Sha256::digest(b"alias body bytes").into();
    assert!(ReclaimRawSourceRead::from_read(&context, 7, &actual_source, [0; 32], None).is_err());
    let proof = ReclaimRawSourceRead::from_read(&context, 7, &actual_source, sha, None).unwrap();
    assert_ne!(proof.fact.body.uid, proof.fact.object.uid);
    let label = read_label_only(&target.device).unwrap();
    let plan = inventory
        .plan_reclaimed_raw_body(&parent, &proof, &target.before, &label, "materialized")
        .unwrap();
    assert!(plan.is_raw_body() && !plan.is_node_metadata());
    assert_eq!(
        plan.raw_body_length().unwrap(),
        b"alias body bytes".len() as u64
    );
    assert_eq!(plan.index_start().unwrap(), plan.previous_dp_eod() + 2);
    proof.fact.require_plan(&plan).unwrap();
    let mut volume = LtfsVolume::mount(&target.device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    let before = target.device.command_log().len();
    let mut wrong = plan.clone();
    wrong.0.raw_body.as_mut().unwrap().body.uid += 1;
    assert!(
        wrong
            .stage_raw_body(
                &mut volume,
                &actual_source,
                &target.before,
                &mut Cursor::new(b"alias body bytes")
            )
            .is_err()
    );
    assert_eq!(before, target.device.command_log().len());
    plan.stage_raw_body(
        &mut volume,
        &actual_source,
        &target.before,
        &mut Cursor::new(b"alias body bytes"),
    )
    .unwrap();
    volume.commit_incremental().unwrap();
    drop(volume);
    target.library.power_cut();
    let actual = capture(&target.device);
    let previous = capture_recorded_index_source(
        &TapeDrive::new(&target.device),
        &label,
        &plan.previous_source().unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap();
    assert_eq!(
        plan.observe_current(&actual, Some(&previous))
            .unwrap()
            .kind(),
        ReclaimMetadataObservationKind::Committed
    );
    let IndexEntry::File { node, .. } = actual.entry_by_uid(plan.uid()).unwrap().unwrap() else {
        panic!("file")
    };
    assert!(node.symlink.is_none());
    assert_eq!(node.length, b"alias body bytes".len() as u64);
    let IndexEntry::File { node: original, .. } = alias else {
        panic!("alias")
    };
    let mut meta = node.meta.clone();
    meta.file_uid = original.meta.file_uid;
    assert_eq!(meta, original.meta);
    assert!(
        node.xattrs
            .iter()
            .any(|a| a.key == "user.original" && a.value == "AAEC/w==" && a.base64)
    );
    let volume = LtfsVolume::mount(&target.device).unwrap();
    let mut bytes = Vec::new();
    volume.read_file_to_writer(plan.path(), &mut bytes).unwrap();
    assert_eq!(bytes, b"alias body bytes");
    plan.require_raw_readback(
        &actual,
        Some((bytes.len() as u64, Sha256::digest(bytes).into(), None)),
    )
    .unwrap();
    assert!(
        plan.require_raw_readback(&actual, Some((0, sha, None)))
            .is_err()
    );
    assert!(
        source_body(&actual_source, 4).is_err(),
        "普通原生链接不能当作 EE 正文读取"
    );
}

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
        intent: json!({"version":1,"library":"SIM-RAW-RECLAIM","namespace":ns.to_string(),
            "epoch":1,"operation":Uuid::new_v4().to_string(),"scan":Uuid::new_v4().to_string(),
            "scan_generation":source.construct().generation,"index":head,
            "change":{"kind":"media-reclaim","pool":Uuid::new_v4().to_string(),
                "barcode":"DSC000L8","volume":source.construct().volume_uuid.to_string(),
                "revision":1,"confirm_format":true,"unassign":false}}),
    }
}

#[test]
fn raw_body_plan_binds_data_blocks_and_requires_actual_readback_for_publication() {
    for old_delta in [false, true] {
        for new_delta in [false, true] {
            let source = setup(false);
            let target = setup(old_delta);
            let inventory = ReclaimSourceInventory::from_captured(&source.before).unwrap();
            let parent = reservation(&source.before);
            let node = Object::capture(source.before.entry_by_uid(2).unwrap().unwrap()).unwrap();
            let (dp_eod, ip_generation, history_depth) =
                current_proof(source.before.scope(), source.before.header()).unwrap();
            let context = ReclaimRawReadContext {
                parent: parent.fingerprint(),
                inventory: inventory.summary.digest,
                index: inventory.summary.head.clone(),
                identity: inventory.summary.identity,
                dp_eod,
                ip_generation,
                history_depth,
                node: ReclaimSourceNode(node),
            };
            let sha: [u8; 32] = Sha256::digest(b"body").into();
            let proof =
                ReclaimRawSourceRead::from_read(&context, 7, &source.before, sha, None).unwrap();
            let label = read_label_only(&target.device).unwrap();
            let commands = target.device.command_log().len();
            let plan = inventory
                .plan_reclaimed_raw_body(&parent, &proof, &target.before, &label, "raw-body")
                .unwrap();
            assert!(plan.is_raw_body() && plan.is_source_based() && !plan.is_node_metadata());
            assert_eq!(plan.index_start().unwrap(), plan.previous_dp_eod() + 2);
            proof.fact.require_plan(&plan).unwrap();
            let mut foreign = proof.fact.clone();
            foreign.sha256[0] ^= 1;
            assert!(foreign.require_plan(&plan).is_err());
            assert_eq!(target.device.command_log().len(), commands);
            let mut volume = LtfsVolume::mount(&target.device).unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
            assert!(
                plan.stage_reference(&mut volume, &source.before, &target.before)
                    .is_err()
            );
            let mut wrong = plan.clone();
            wrong.0.raw_body.as_mut().unwrap().block_size *= 2;
            let commands = target.device.command_log().len();
            assert!(
                wrong
                    .stage_raw_body(
                        &mut volume,
                        &source.before,
                        &target.before,
                        &mut Cursor::new(b"body")
                    )
                    .is_err()
            );
            assert_eq!(target.device.command_log().len(), commands);
            plan.stage_raw_body(
                &mut volume,
                &source.before,
                &target.before,
                &mut Cursor::new(b"body"),
            )
            .unwrap();
            if new_delta {
                volume.commit_incremental().unwrap();
            } else {
                volume.commit().unwrap();
            }
            drop(volume);
            target.library.power_cut();
            let actual = capture(&target.device);
            let previous = capture_recorded_index_source(
                &TapeDrive::new(&target.device),
                &label,
                &plan.previous_source().unwrap(),
                &RecoveryBudget::default(),
            )
            .unwrap();
            let observation = plan.observe_current(&actual, Some(&previous)).unwrap();
            assert_eq!(
                observation.kind(),
                ReclaimMetadataObservationKind::Committed
            );
            assert_eq!(
                ReclaimMetadataObservation::decode(&plan, &observation.encode(&plan).unwrap())
                    .unwrap(),
                observation
            );
            assert!(
                plan.require_raw_readback(&actual, None).is_err(),
                "XML 正确不等于正文读回正确"
            );
            assert!(
                plan.require_raw_readback(&actual, Some((4, [0; 32], None)))
                    .is_err()
            );
            assert!(
                plan.require_raw_readback(&actual, Some((3, sha, None)))
                    .is_err()
            );
            assert!(
                plan.require_raw_readback(&actual, Some((4, sha, Some([0; 16]))))
                    .is_err()
            );
            let volume = LtfsVolume::mount(&target.device).unwrap();
            let mut bytes = Vec::new();
            volume.read_file_to_writer(plan.path(), &mut bytes).unwrap();
            plan.require_raw_readback(
                &actual,
                Some((bytes.len() as u64, Sha256::digest(&bytes).into(), None)),
            )
            .unwrap();
        }
    }
}
