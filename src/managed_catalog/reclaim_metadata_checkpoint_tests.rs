use super::super::super::metadata_discard::tests::{Scenario, capture, original, setup};
use super::*;
use crate::ltfs::volume::{LtfsVolume, P1_DATA_START};
use crate::scsi::reservation::ReservationKey;
use crate::scsi::sim::LogicalObject;

fn task(s: &Scenario, phase: Phase) -> ReclaimMetadataWrite {
    let id = Uuid::new_v4().to_string();
    let context=serde_json::from_value(json!({"file":{"path":"/a","file":Uuid::new_v4().to_string(),
        "version":1,"size":4,"operation":Uuid::new_v4().to_string()},"epoch":1,"request":vec![7u8;32]})).unwrap();
    let task = ReclaimMetadataWrite(Record {
        version: 7,
        id: id.clone(),
        first: id.clone(),
        plan_origin: id,
        previous: None,
        scan: Uuid::new_v4().to_string(),
        plan: s.plan.0.clone(),
        context,
        owner: 7,
        phase,
        recovery_epoch: u64::from(matches!(phase, Phase::Discarding | Phase::Checkpointing)),
        observation: None,
    });
    task.encode().unwrap();
    task
}

fn damage_ip(s: &Scenario, records: &[LogicalObject]) {
    s.library.with_cartridge_mut("DSC000L8", |cartridge| {
        let ip = &mut cartridge.partitions[0];
        ip.objects.truncate(P1_DATA_START as usize);
        ip.objects.extend_from_slice(records);
        ip.flushed = ip.objects.len();
        cartridge.vcr += 1;
    });
}

#[test]
fn checkpoint_repairs_empty_opening_and_truncated_ip_with_exact_full_or_delta_dp() {
    for delta in [false, true] {
        for suffix in [
            vec![],
            vec![LogicalObject::Filemark],
            vec![
                LogicalObject::Filemark,
                LogicalObject::Record(b"<ltfsindex version=\"2.5.0\">".to_vec()),
            ],
        ] {
            let s = setup(delta);
            let baseline = s.device.command_log().len();
            let retained = original(&s);
            let writing = task(&s, Phase::Writing);
            damage_ip(&s, &suffix);
            let before = s.library.cartridge("DSC000L8").unwrap();
            let mut volume = LtfsVolume::mount_with_budget(
                &s.device,
                &crate::ltfs::recovery::RecoveryBudget::default(),
            )
            .unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
            let proof = volume
                .reclaim_metadata_checkpoint_evidence(
                    &writing,
                    &retained,
                    &crate::ltfs::recovery::RecoveryBudget::default(),
                )
                .unwrap();
            assert_eq!(proof.current(), s.before.construct());
            assert_eq!(proof.dp_eod(), s.plan.previous_dp_eod());
            let commands = s.device.command_log().len();
            assert!(
                volume
                    .checkpoint_reclaim_metadata(
                        &writing,
                        &retained,
                        &crate::ltfs::recovery::RecoveryBudget::default()
                    )
                    .is_err(),
                "Writing 不具有检查点写权限"
            );
            assert!(
                !s.device.command_log()[commands..]
                    .iter()
                    .any(|op| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
            );
            let mut recovering = writing.clone();
            recovering.0.phase = Phase::Checkpointing;
            recovering.0.recovery_epoch = 1;
            assert!(
                proof.require(&recovering, 7).is_err(),
                "Writing 旧证明不能执行新检查点阶段"
            );
            drop(volume);
            let mut volume = LtfsVolume::mount_with_budget(
                &s.device,
                &crate::ltfs::recovery::RecoveryBudget::default(),
            )
            .unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
            volume
                .checkpoint_reclaim_metadata(
                    &recovering,
                    &retained,
                    &crate::ltfs::recovery::RecoveryBudget::default(),
                )
                .unwrap();
            drop(volume);
            let current = capture(&s.device);
            let after = s.library.cartridge("DSC000L8").unwrap();
            if delta {
                assert_eq!(
                    &after.partitions[1].objects[..before.partitions[1].objects.len()],
                    before.partitions[1].objects.as_slice()
                );
                assert!(!current.construct().incremental);
                assert_eq!(
                    current.construct().generation,
                    s.before.construct().generation + 1
                );
            } else {
                assert_eq!(
                    after.partitions[1].objects, before.partitions[1].objects,
                    "Full DP 的 IP 恢复不能写 DP"
                );
                assert_eq!(current.construct(), s.before.construct());
            }
            let result =
                ReclaimMetadataRecoveryEvidence::from_current(&recovering, 7, &current, &retained)
                    .unwrap();
            assert_eq!(
                result.kind(),
                if delta {
                    ReclaimMetadataRecoveryKind::Checkpointed
                } else {
                    ReclaimMetadataRecoveryKind::Unwritten
                }
            );
            assert!(
                ReclaimMetadataRecoveryEvidence::from_current(&writing, 7, &current, &retained)
                    .is_err()
            );
            let mut newer = recovering.clone();
            newer.0.recovery_epoch += 1;
            assert!(
                result.require(&newer, 7).is_err(),
                "旧阶段序号不能成为新恢复结果"
            );
            assert!(
                !s.device.command_log()[baseline..]
                    .iter()
                    .any(|op| matches!(op, 0x04 | 0x19)),
                "测试准备后不格式化或擦除"
            );
        }
    }
}

#[test]
fn checkpoint_rejects_unrecognized_ip_data_fake_construct_and_wrong_owner_without_writes() {
    for suffix in [
        vec![LogicalObject::Record(b"foreign-ip-data".to_vec())],
        vec![
            LogicalObject::Filemark,
            LogicalObject::Record(b"<ltfsindex>malformed</ltfsindex>".to_vec()),
            LogicalObject::Filemark,
        ],
    ] {
        let s = setup(false);
        let retained = original(&s);
        let recovering = task(&s, Phase::Checkpointing);
        damage_ip(&s, &suffix);
        let mut volume = LtfsVolume::mount_with_budget(
            &s.device,
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
        let commands = s.device.command_log().len();
        assert!(
            volume
                .checkpoint_reclaim_metadata(
                    &recovering,
                    &retained,
                    &crate::ltfs::recovery::RecoveryBudget::default()
                )
                .is_err()
        );
        assert!(
            !s.device.command_log()[commands..]
                .iter()
                .any(|op| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
        );
    }
}

#[test]
fn checkpoint_rejects_a_different_task_owner_without_writes() {
    let s = setup(false);
    let retained = original(&s);
    let mut recovering = task(&s, Phase::Checkpointing);
    recovering.0.owner = 8;
    damage_ip(&s, &[]);
    let mut volume =
        LtfsVolume::mount_with_budget(&s.device, &crate::ltfs::recovery::RecoveryBudget::default())
            .unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    let commands = s.device.command_log().len();
    assert!(
        volume
            .checkpoint_reclaim_metadata(
                &recovering,
                &retained,
                &crate::ltfs::recovery::RecoveryBudget::default()
            )
            .is_err()
    );
    assert!(
        !s.device.command_log()[commands..]
            .iter()
            .any(|op| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );
}

#[test]
fn checkpoint_rejects_an_ip_ahead_view_and_never_reclassifies_recorded_as_current() {
    let s = setup(false);
    let retained = original(&s);
    let recovering = task(&s, Phase::Checkpointing);
    assert!(s.plan.observe_current(&retained, Some(&retained)).is_err());
    let mut ahead = s.before.header().clone();
    ahead.generation += 1;
    ahead.self_location = crate::ltfs::index::IndexLocation {
        partition: 'a',
        start_block: P1_DATA_START + 1,
    };
    ahead.previous_location = Some(s.before.construct().location);
    ahead.previous_incremental_location = None;
    damage_ip(
        &s,
        &[
            LogicalObject::Filemark,
            LogicalObject::Record(ahead.to_xml().unwrap().to_vec()),
            LogicalObject::Filemark,
        ],
    );
    let mut volume =
        LtfsVolume::mount_with_budget(&s.device, &crate::ltfs::recovery::RecoveryBudget::default())
            .unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
    assert!(volume.recovery().ip_ahead());
    let commands = s.device.command_log().len();
    assert!(
        volume
            .checkpoint_reclaim_metadata(
                &recovering,
                &retained,
                &crate::ltfs::recovery::RecoveryBudget::default()
            )
            .is_err()
    );
    assert!(
        !s.device.command_log()[commands..]
            .iter()
            .any(|op| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );
}
