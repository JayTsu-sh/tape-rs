//! Re-enter the original durable recovery goal before deciding on any write.
use super::*;
use crate::ltfs::recovery::TailKind;
use crate::ltfs::volume::TailPolicy;
use crate::managed_catalog::{ReclaimMetadataRecoveryEvidence, ReclaimMetadataResolution};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Goal {
    Discard,
    Checkpoint,
}

enum Admission {
    Discard(crate::managed_catalog::ReclaimMetadataTailEvidence),
    Checkpoint(crate::managed_catalog::ReclaimMetadataCheckpointEvidence),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReclaimMetadataRecovery {
    Observed(ReclaimMetadataRecoveryEvidence),
    Resolved(ReclaimMetadataResolution),
}

impl ReclaimMetadataWriter<'_> {
    /// Observe a fresh original T1/T2, then persist the goal. The local Writing
    /// guard and volume are dropped before the caller can execute recovery.
    pub fn begin_discard<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,

        writing: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        self.begin_recovery(reservation, writing, Goal::Discard)
    }

    pub fn begin_checkpoint<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,

        task: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        self.begin_recovery(reservation, task, Goal::Checkpoint)
    }

    fn begin_recovery<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,

        writing: &'a ReclaimMetadataWrite,
        goal: Goal,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        Box::pin(async move {
            self.require_original(reservation, writing).await?;
            let allowed = match goal {
                Goal::Discard => {
                    writing.writing()
                        || writing.checkpointing()
                            && writing
                                .plan()
                                .previous_source()
                                .map_err(catalog)?
                                .head()
                                .incremental
                }
                Goal::Checkpoint => writing.writing() || writing.discarding(),
            };
            if !allowed {
                return Err(catalog("元数据恢复准入阶段不允许指定目标"));
            }
            let _activity = self.permission.begin_device_activity(self.key.round())?;
            let proof = {
                let checked = Guard::new(self.device, || {
                    futures_executor::block_on(self.require_original(reservation, writing))?;
                    verify_holder(self.device, self.key)
                });
                let label = read_label_only(&checked)?;
                if label.volume_uuid != writing.plan().volume() {
                    return Err(catalog("修尾前已装入其他介质"));
                }
                let original = capture_recorded_index_source(
                    &TapeDrive::new(&checked),
                    &label,
                    &writing.plan().previous_source().map_err(catalog)?,
                    self.budget,
                )?;
                let mut volume = LtfsVolume::mount_with_budget(&checked, &self.recovery_budget())?;
                volume.set_reservation_guard(Some(self.key));
                let proof = match goal {
                    Goal::Discard => Admission::Discard(
                        volume.reclaim_metadata_tail_evidence(writing, &original)?,
                    ),
                    Goal::Checkpoint => {
                        Admission::Checkpoint(volume.reclaim_metadata_checkpoint_evidence(
                            writing,
                            &original,
                            &self.recovery_budget(),
                        )?)
                    }
                };
                checked.verify_proof()?;
                proof
            };
            match proof {
                Admission::Discard(proof) => self
                    .catalog
                    .begin_reclaim_metadata_discard(reservation, self.key.round(), writing, &proof)
                    .await
                    .map_err(catalog),
                Admission::Checkpoint(proof) => self
                    .catalog
                    .begin_reclaim_metadata_checkpoint(
                        reservation,
                        self.key.round(),
                        writing,
                        &proof,
                    )
                    .await
                    .map_err(catalog),
            }
        })
    }

    /// A replay never assumes the previous recovery CDB failed. Completed
    /// original writes, lost buffered tails and completed Discards are read
    /// first; only a proven original T1/T2 may append one cleanup Full.
    pub fn recover<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,

        task: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataRecovery>> + 'a>>
    {
        Box::pin(async move {
            let goal = if task.checkpointing() {
                Goal::Checkpoint
            } else {
                Goal::Discard
            };
            self.require_scope(reservation).await?;
            if let Some(result) = self
                .catalog
                .reclaim_metadata_resolution(reservation, self.key.round(), task)
                .await
                .map_err(catalog)?
            {
                return Ok(ReclaimMetadataRecovery::Resolved(result));
            }
            if !(goal == Goal::Discard && task.discarding()
                || goal == Goal::Checkpoint && task.checkpointing())
            {
                return Err(catalog("元数据恢复要求原持久阶段"));
            }
            self.require_original(reservation, task).await?;
            let _activity = self.permission.begin_device_activity(self.key.round())?;
            let checked = Guard::new(self.device, || {
                futures_executor::block_on(self.require_original(reservation, task))?;
                futures_executor::block_on(self.catalog.verify_reclaim_metadata_recovery(
                    reservation,
                    self.key.round(),
                    task,
                ))
                .map_err(catalog)?;
                verify_holder(self.device, self.key)
            });
            let label = read_label_only(&checked)?;
            let plan = task.plan();
            if label.volume_uuid != plan.volume() {
                return Err(catalog("修尾已装入介质不是原目标卷"));
            }
            let original = capture_recorded_index_source(
                &TapeDrive::new(&checked),
                &label,
                &plan.previous_source().map_err(catalog)?,
                self.budget,
            )?;
            {
                let mut volume = LtfsVolume::mount_with_budget(&checked, &self.recovery_budget())?;
                volume.set_reservation_guard(Some(self.key));
                match volume.recovery().dp.tail {
                    TailKind::Complete => {}
                    TailKind::UnindexedData | TailKind::TruncatedIndex => {
                        if goal != Goal::Discard {
                            return Err(TapeError::RecoveryRestricted {
                                reason: "检查点 DP 尾部未完成，需要原任务显式修尾意图".into(),
                            });
                        }
                        // Verifies the original construct end, complete source
                        // tree and exact old EOD before any repair append.
                        volume.reclaim_metadata_tail_evidence(task, &original)?;
                        volume.close_tail(TailPolicy::Discard)?;
                        drop(volume);
                        volume = LtfsVolume::mount_with_budget(&checked, &self.recovery_budget())?;
                        volume.set_reservation_guard(Some(self.key));
                    }
                    _ => return Err(catalog("修尾重入未识别到原任务可恢复的尾部")),
                }
                if volume.recovery().ip.tail != TailKind::Complete
                    || volume.recovery().ip.last_index.is_none()
                {
                    // This routine independently recognizes the exact DP result
                    // and known IP damage before overwriting IP. An original
                    // delta requires the explicit Checkpointing goal.
                    volume.checkpoint_reclaim_metadata(task, &original, &self.recovery_budget())?;
                }
            }
            let current =
                capture_complete_index_source(&TapeDrive::new(&checked), &label, self.budget)?;
            let raw_read = if plan
                .observe_current(&current, Some(&original))
                .is_ok_and(|o| o.kind() == ReclaimMetadataObservationKind::Committed)
            {
                self.read_raw_result(&checked, &label, task, &current)?
            } else {
                None
            };
            let proof = ReclaimMetadataRecoveryEvidence::from_current_with_raw_read(
                task,
                self.key.round(),
                &current,
                &original,
                raw_read,
            )
            .map_err(catalog)?;
            checked.verify_proof()?;
            Ok(ReclaimMetadataRecovery::Observed(proof))
        })
    }
}
