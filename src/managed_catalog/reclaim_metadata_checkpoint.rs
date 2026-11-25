//! A distinct DP-only precondition for repairing a missing/truncated IP.
//! Recorded sources remain historical facts, never ordinary Current evidence.
use super::*;
use crate::ltfs::recovery::{RecoveryReport, TailKind};
use crate::ltfs::volume::CommittedIndexConstruct;
use metadata_plan::TargetTree;

pub(super) struct CheckpointObservation {
    pub current: ActiveIndexIdentity,
    pub files: u64,
}

impl ReclaimMetadataPlan {
    pub(super) fn observe_checkpointed(
        &self,
        target: &CapturedIndexSource,
        original: &CapturedIndexSource,
    ) -> Result<CheckpointObservation> {
        current_proof(target.scope(), target.header())?;
        self.observe_checkpointed_dp(target, original)
    }

    // An Incremental DP requires a Full DP checkpoint as well as Full IP for
    // a consistent volume (SNIA LTFS 2.5.1 §§5.4.3,H.1). No raw node is added.
    pub(super) fn observe_checkpointed_dp(
        &self,
        target: &CapturedIndexSource,
        original: &CapturedIndexSource,
    ) -> Result<CheckpointObservation> {
        self.require_discard_source(original)?;
        let old = self.previous_source()?.head();
        let current = target.construct();
        let before = original.header();
        let head = target.header();
        if !old.incremental
            || current.incremental
            || head.incremental
            || current.volume_uuid != self.volume()
            || head.volume_uuid != current.volume_uuid
            || current.generation
                != old
                    .generation
                    .checked_add(1)
                    .ok_or(CatalogError::Invalid("检查点代次溢出"))?
            || head.generation != current.generation
            || head.self_location != current.location
            || current.location.partition != old.location.partition
            || Some(current.location.start_block) != self.previous_dp_eod().checked_add(1)
            || head.previous_location != before.previous_location
            || head.previous_incremental_location != Some(old.location)
            || head.highest_file_uid != before.highest_file_uid
            || head.creator != before.creator
            || head.comment != before.comment
            || head.allow_policy_update != before.allow_policy_update
            || head.volume_lock_state != before.volume_lock_state
            || head.version != "2.5.0"
        {
            return Err(CatalogError::Invalid(
                "完整检查点偏离原增量卷头、边界或 UID",
            ));
        }
        let mut tree = TargetTree::new(self.volume());
        let mut files = 0u64;
        target
            .for_each_entry(|entry| {
                if matches!(entry, IndexEntry::File { .. }) {
                    files = files
                        .checked_add(1)
                        .ok_or_else(|| callback(CatalogError::Invalid("检查点文件计数溢出")))?;
                }
                tree.add(&Object::capture(entry).map_err(callback)?)
                    .map_err(callback)
            })
            .map_err(cache)?;
        if files != self.0.physical_files
            || tree.objects != self.0.target_objects
            || tree.finish() != self.0.target_tree
        {
            return Err(CatalogError::Invalid("完整检查点改变原完整对象树或文件数"));
        }
        Ok(CheckpointObservation {
            current: current.into(),
            files,
        })
    }
}

/// Fresh original-task evidence for an controlled checkpoint. It is neither a
/// saved observation nor permission to append another DP metadata node.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimMetadataCheckpointEvidence =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataCheckpointEvidence {
    task: ReclaimMetadataWrite,
    current: CommittedIndexConstruct,
    dp_eod: u64,
    outcome: ReclaimMetadataRecoveryKind,
}

impl ReclaimMetadataCheckpointEvidence {
    pub(crate) fn from_report(
        task: &ReclaimMetadataWrite,
        owner: u64,
        target: &CapturedIndexSource,
        original: &CapturedIndexSource,
        report: &RecoveryReport,
        opening_only: bool,
    ) -> Result<Self> {
        let plan = task.plan();
        plan.require_discard_source(original)?;
        let current = target.construct();
        let last = report
            .dp
            .last_index
            .as_ref()
            .ok_or(CatalogError::Incomplete)?;
        let ip_damaged = match report.ip.tail {
            TailKind::EmptyData | TailKind::TruncatedIndex => true,
            TailKind::UnindexedData => {
                opening_only && report.ip.last_index.is_none() && report.ip.eod == 5
            }
            _ => false,
        };
        if task.owner() != owner
            || !(task.writing() || task.recovering())
            || target.scope() != IndexSourceScope::Recorded
            || target.construct_end() != report.dp.eod
            || report.dp.tail != TailKind::Complete
            || last.end_block.checked_add(1) != Some(report.dp.eod)
            || last.construct_sha256 != current.sha256
            || last.index.generation != current.generation
            || last.index.volume_uuid != current.volume_uuid
            || last.index.self_location != current.location
            || !report.chain_ok
            || !report.history_checked
            || report.chain_truncated_by_budget
            || report.dp.budget_exhausted
            || report.ip.budget_exhausted
            || !report.append_ok
            || report.ip_ahead()
            || report.restricted_reason().is_some()
            || report
                .ip
                .last_index
                .as_ref()
                .is_some_and(|ip| ip.index.generation >= current.generation)
            || !ip_damaged
            || !target.header().unknown_elements.is_empty()
            || target
                .header()
                .volume_lock_state
                .as_deref()
                .is_some_and(|lock| lock != "unlocked")
        {
            return Err(CatalogError::Invalid(
                "IP 恢复要求原实际闭合 DP、完整历史及已识别的 IP 损坏",
            ));
        }
        target
            .for_each_entry(|entry| {
                if let IndexEntry::File { node, .. } = entry
                    && node
                        .extents
                        .iter()
                        .any(|extent| extent.partition != current.location.partition)
                {
                    return Err(callback(CatalogError::Invalid("IP 检查点不能覆盖文件正文")));
                }
                Ok(())
            })
            .map_err(cache)?;
        let outcome = match plan.observe_verified_dp(target, Some(original), report.dp.eod) {
            Ok(observed) => {
                if let Some(saved) = task.observation()?
                    && saved.kind() == ReclaimMetadataObservationKind::Committed
                    && saved != observed
                {
                    return Err(CatalogError::Invalid("IP 恢复改变原已观察的 DP 提交"));
                }
                if observed.kind() == ReclaimMetadataObservationKind::Committed {
                    ReclaimMetadataRecoveryKind::Committed
                } else {
                    ReclaimMetadataRecoveryKind::Unwritten
                }
            }
            Err(error) => {
                if task.checkpointing() {
                    plan.observe_checkpointed_dp(target, original)?;
                    ReclaimMetadataRecoveryKind::Checkpointed
                } else if task.discarding() {
                    plan.observe_discarded_dp(target, original)?;
                    ReclaimMetadataRecoveryKind::Discarded
                } else {
                    return Err(error);
                }
            }
        };
        Ok(Self {
            task: task.clone(),
            current,
            dp_eod: report.dp.eod,
            outcome,
        })
    }

    pub(crate) fn current(&self) -> CommittedIndexConstruct {
        self.current
    }
    pub(crate) fn dp_eod(&self) -> u64 {
        self.dp_eod
    }
    pub(crate) fn require(&self, task: &ReclaimMetadataWrite, owner: u64) -> Result<()> {
        if self.task != *task || task.owner() != owner || !(task.writing() || task.recovering()) {
            return Err(CatalogError::Invalid(
                "IP 检查点证明改变原任务、阶段或 owner",
            ));
        }
        Ok(())
    }
}

impl ManagedCatalog {
    /// Journal before overwriting IP. The Writing mount/guard must be dropped
    /// and the dedicated actor must independently classify media again.
    pub async fn begin_reclaim_metadata_checkpoint(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,

        original: &ReclaimMetadataWrite,
        evidence: &ReclaimMetadataCheckpointEvidence,
    ) -> Result<ReclaimMetadataWrite> {
        original.encode()?;
        evidence.require(original, owner)?;
        if !(original.writing() || original.discarding())
            || !matches!(
                evidence.outcome,
                ReclaimMetadataRecoveryKind::Unwritten | ReclaimMetadataRecoveryKind::Committed
            )
        {
            return Err(CatalogError::Invalid(
                "检查点准入要求原 Writing 或 Discarding 的正常 DP 结果",
            ));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            isolation::require_normal_mutation_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let mut task =
                                load_id_at(&tr, reservation, plan.source_uid(), original.id())
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                            publication::require_original(original, &task, owner)?;

                            let input =
                                context::input_for_task_at(&tr, reservation, owner, &task).await?;
                            if task.checkpointing() {
                                return Ok(task);
                            }
                            if task != *original || !(task.writing() || task.discarding()) {
                                return Err(CatalogError::Invalid("检查点恢复改变原阶段快照"));
                            }
                            if target_context_policy_at(
                                &tr,
                                reservation,
                                &plan,
                                input.as_ref(),
                                false,
                            )
                            .await?
                            .to_string()
                                != task.0.scan
                                || active_volume::recovery_active(&tr, plan.volume()).await?
                            {
                                return Err(CatalogError::Corrupt("IP 恢复改变原扫描、卷头或占用"));
                            }
                            let next = plan
                                .previous_source()?
                                .head()
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("IP 恢复代次溢出"))?;
                            publication::require_empty_result_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                                next,
                            )
                            .await?;
                            task.0.phase = Phase::Checkpointing;
                            task.0.recovery_epoch = task
                                .0
                                .recovery_epoch
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("元数据恢复序号溢出"))?;
                            let raw = task.encode()?;
                            let header = registry::encode(reservation, &task)?;
                            tr.set(&task_key(reservation, &task), &raw);
                            tr.set(&registry::key(reservation), &header);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
