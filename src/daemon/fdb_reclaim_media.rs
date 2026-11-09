//! One serialized metadata attempt on an already selected/loaded target drive.
//! The executor selects the original placement; this primitive never moves,
//! fences, repairs or formats media. Errors retain Writing and all claims.
use super::executor::fdb_operation_guard::Guard;
use super::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::{
    CapturedIndexSource, RecoveryBudget, capture_complete_index_source,
    capture_recorded_index_source,
};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::managed_catalog::{
    ActiveFileRequest, ActiveLibraryRoute, ManagedCatalog, ReclaimFile,
    ReclaimMetadataCommitEvidence, ReclaimMetadataObservationKind,
    ReclaimMetadataUnwrittenEvidence, ReclaimMetadataWrite, ReclaimReservation,
};
use crate::scsi::reservation::{ReservationKey, verify_holder};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

#[path = "fdb_reclaim_metadata_recovery.rs"]
mod recovery;
pub use recovery::ReclaimMetadataRecovery;
#[path = "fdb_reclaim_donor.rs"]
mod donor;
pub use donor::ReclaimMetadataDonorRead;

type BodyReadback = (u64, [u8; 32], Option<[u8; 16]>);

pub struct ReclaimMetadataWriter<'a> {
    pub catalog: &'a ManagedCatalog,
    pub device: &'a dyn TapeTransport,
    pub route: ActiveLibraryRoute,
    pub key: ReservationKey,
    pub permission: &'a Permission,
    pub budget: &'a RecoveryBudget,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataExecution {
    task: ReclaimMetadataWrite,
    commit: Option<ReclaimMetadataCommitEvidence>,
    unwritten: Option<ReclaimMetadataUnwrittenEvidence>,
    needs_checkpoint: bool,
}
impl ReclaimMetadataExecution {
    pub fn needs_checkpoint(&self) -> bool {
        self.needs_checkpoint
    }
    pub fn task(&self) -> &ReclaimMetadataWrite {
        &self.task
    }
    pub fn unwritten_evidence(&self) -> Option<&ReclaimMetadataUnwrittenEvidence> {
        self.unwritten.as_ref()
    }
    pub fn commit_evidence(&self) -> Option<&ReclaimMetadataCommitEvidence> {
        self.commit.as_ref()
    }
}

impl ReclaimMetadataWriter<'_> {
    fn recovery_budget(&self) -> RecoveryBudget {
        self.budget.clone().with_complete_full_history()
    }

    /// A directory/link needs a new current target-index proof after repair,
    /// without inventing a zero-byte file donor or changing its original path.
    pub fn read_source_replan<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        task: &'a ReclaimMetadataWrite,
        source: &'a CapturedIndexSource,
    ) -> std::pin::Pin<
        Box<
            impl std::future::Future<
                Output = Result<crate::managed_catalog::ReclaimMetadataSourceReplanEvidence>,
            > + 'a,
        >,
    > {
        Box::pin(async move {
            self.require_scope(reservation).await?;
            task.plan().validate_source(source).map_err(catalog)?;
            let context = self
                .catalog
                .reclaim_metadata_source_replan_context(reservation, self.key.round(), task)
                .await
                .map_err(catalog)?;
            let _activity = self.permission.begin_device_activity(self.key.round())?;
            let checked = Guard::new(self.device, || {
                futures_executor::block_on(self.require_scope(reservation))?;
                let current = futures_executor::block_on(
                    self.catalog.reclaim_metadata_source_replan_context(
                        reservation,
                        self.key.round(),
                        task,
                    ),
                )
                .map_err(catalog)?;
                if current != context {
                    return Err(catalog("原始节点重新规划期间目的状态改变"));
                }
                verify_holder(self.device, self.key)
            });
            let label = read_label_only(&checked)?;
            if label.volume_uuid != task.plan().volume() {
                return Err(catalog("节点重新规划装入了其他介质"));
            }
            let target =
                capture_complete_index_source(&TapeDrive::new(&checked), &label, self.budget)?;
            let inventory = crate::managed_catalog::ReclaimSourceInventory::from_captured(source)
                .map_err(catalog)?;
            let plan = task
                .replan_source(reservation, &inventory, &target, &label)
                .map_err(catalog)?;
            checked.verify_proof()?;
            crate::managed_catalog::ReclaimMetadataSourceReplanEvidence::from_capture(
                context, &target, plan,
            )
            .map_err(catalog)
        })
    }
    /// Read the destination's actual extents without a payload cache. Every
    /// CDB still runs under the caller's original task/owner/PR guard.
    fn read_raw_result(
        &self,
        checked: &dyn TapeTransport,
        label: &crate::ltfs::label::LtfsLabel,
        task: &ReclaimMetadataWrite,
        current: &CapturedIndexSource,
    ) -> Result<Option<BodyReadback>> {
        use sha2::{Digest, Sha256};
        if !task.plan().is_raw_body() {
            return Ok(None);
        }
        let crate::ltfs::index::IndexEntry::File { node, .. } = current
            .entry_by_uid(task.plan().uid())?
            .ok_or_else(|| catalog("原始正文目的节点缺失"))?
        else {
            return Err(catalog("原始正文目的节点类型改变"));
        };
        let mut body = donor::BodyHash {
            sha: Sha256::new(),
            md5: node
                .xattrs
                .iter()
                .any(|attr| attr.key == crate::ltfs::index::XATTR_MD5)
                .then(md5::Md5::new),
            length: 0,
            expected: node.length,
        };
        let descriptor = crate::catalog::volume_extents::CachedFile {
            path: task.plan().path().into(),
            length: node.length,
            symlink: node.symlink,
            extents: node.extents,
        };
        let length = crate::ltfs::volume::read_cached_file_to_writer(
            checked,
            label,
            &descriptor,
            &mut body,
        )?;
        if length != body.length || length != body.expected {
            return Err(catalog("原始正文目的实读残缺"));
        }
        let after = capture_complete_index_source(&TapeDrive::new(checked), label, self.budget)?;
        if after.identity()? != current.identity()?
            || after.scope() != current.scope()
            || after.construct_end() != current.construct_end()
        {
            return Err(catalog("原始正文目的实读期间索引或闭合边界改变"));
        }
        Ok(Some((
            length,
            body.sha.finalize().into(),
            body.md5.map(|h| h.finalize().into()),
        )))
    }
    async fn require_scope(&self, reservation: &ReclaimReservation) -> Result<()> {
        let round = self.key.round();
        if self.device.execution_guard()
            != Some((round, self.permission as *const Permission as usize))
        {
            return Err(TapeError::NotReady(
                "回收元数据设备未绑定当前代次和多数派许可实例".into(),
            ));
        }
        if reservation.namespace() != self.route.route.namespace
            || !(self.permission.allows(round, Phase::Serving)
                || self.permission.allows(round, Phase::Execution))
            || self
                .catalog
                .active_library_owner(reservation.serial())
                .await
                .map_err(catalog)?
                != Some((self.route, round))
        {
            return Err(TapeError::OwnershipLost {
                device: self.device.identity().into(),
                detail: "回收元数据原任务、route、owner 或多数派许可已改变".into(),
            });
        }
        Ok(())
    }

    async fn require_original(
        &self,
        reservation: &ReclaimReservation,
        task: &ReclaimMetadataWrite,
    ) -> Result<()> {
        self.require_scope(reservation).await?;
        if task.owner() != self.key.round()
            || self
                .catalog
                .reclaim_metadata_attempt_status(reservation, self.key.round(), task)
                .await
                .map_err(catalog)?
                != *task
        {
            return Err(catalog("回收元数据原 Writing 任务或占用改变"));
        }
        Ok(())
    }

    /// Only a fresh Prepared→Writing transition may commit a new index. A
    /// replay (including an undelivered admission reply) only observes media.
    /// The returned Writing snapshot never publishes or releases anything.
    pub async fn execute(
        &mut self,
        reservation: &ReclaimReservation,

        original: &ReclaimMetadataWrite,
        source: Option<&CapturedIndexSource>,
    ) -> Result<ReclaimMetadataExecution> {
        self.execute_body(reservation, original, source, None).await
    }

    pub async fn execute_raw_body<R: std::io::Read>(
        &mut self,
        reservation: &ReclaimReservation,
        original: &ReclaimMetadataWrite,
        source: Option<&CapturedIndexSource>,
        reader: &mut R,
    ) -> Result<ReclaimMetadataExecution> {
        if !original.plan().is_raw_body() {
            return Err(catalog("流式正文执行要求原始正文计划"));
        }
        self.execute_body(reservation, original, source, Some(reader))
            .await
    }

    async fn execute_body(
        &mut self,
        reservation: &ReclaimReservation,
        original: &ReclaimMetadataWrite,
        source: Option<&CapturedIndexSource>,
        mut reader: Option<&mut dyn std::io::Read>,
    ) -> Result<ReclaimMetadataExecution> {
        self.require_scope(reservation).await?;
        if self
            .catalog
            .reclaim_metadata_publication(reservation, self.key.round(), original)
            .await
            .map_err(catalog)?
            .is_some()
        {
            let task = self
                .catalog
                .reclaim_metadata_attempt_status(reservation, self.key.round(), original)
                .await
                .map_err(catalog)?;
            return Ok(ReclaimMetadataExecution {
                task,
                commit: None,
                unwritten: None,
                needs_checkpoint: false,
            });
        }
        if let Some(source) = source {
            original.plan().validate_source(source).map_err(catalog)?;
        }
        if !original.writing() && source.is_none() {
            let current = self
                .catalog
                .reclaim_metadata_attempt_status(reservation, self.key.round(), original)
                .await
                .map_err(catalog)?;
            if !(current.writing() && current.plan() == original.plan()) {
                return Err(catalog("首次元数据执行缺少原源节点实读"));
            }
        }
        if original.plan().is_raw_body() && !original.writing() && reader.is_none() {
            let current = self
                .catalog
                .reclaim_metadata_attempt_status(reservation, self.key.round(), original)
                .await
                .map_err(catalog)?;
            if !current.writing() {
                return Err(catalog("首次原始正文执行缺少输入流"));
            }
        }
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let admission = self
            .catalog
            .admit_reclaim_metadata_write(reservation, self.key.round(), original)
            .await
            .map_err(catalog)?;
        let writing = admission.task();
        let plan = writing.plan();
        let checked = Guard::new(self.device, || {
            futures_executor::block_on(self.require_original(reservation, writing))?;
            if !admission.needs_observation() {
                futures_executor::block_on(self.catalog.verify_reclaim_metadata_execution(
                    reservation,
                    self.key.round(),
                    writing,
                ))
                .map_err(catalog)?;
            }
            verify_holder(self.device, self.key)
        });
        let label = read_label_only(&checked)?;
        if label.volume_uuid != plan.volume() {
            return Err(catalog("回收元数据已装入介质不是原目标卷"));
        }
        let drive = TapeDrive::new(&checked);
        let before = capture_complete_index_source(&drive, &label, self.budget)?;
        if !admission.needs_observation() {
            let source = source.ok_or_else(|| catalog("首次元数据执行缺少原来源"))?;
            let mut volume = LtfsVolume::mount_with_budget(&checked, &self.recovery_budget())?;
            volume.set_reservation_guard(Some(self.key));
            if plan.is_raw_body() {
                let stream = reader
                    .as_mut()
                    .ok_or_else(|| catalog("首次原始正文执行缺少输入流"))?;
                plan.stage_raw_body(&mut volume, source, &before, stream)
                    .map_err(catalog)?;
            } else {
                plan.stage_reference(&mut volume, source, &before)
                    .map_err(catalog)?;
            }
            // A failed commit consumes the mount and stops this guard. A new
            // invocation must observe the original media; no inline retry.
            volume.commit()?;
        }
        let actual = if admission.needs_observation() {
            before
        } else {
            capture_complete_index_source(&drive, &label, self.budget)?
        };
        let predecessor = if actual.construct().incremental
            || actual.identity()? == plan.previous_source().map_err(catalog)?
        {
            None
        } else {
            Some(capture_recorded_index_source(
                &drive,
                &label,
                &plan.previous_source().map_err(catalog)?,
                self.budget,
            )?)
        };
        let observed = plan
            .observe_current(&actual, predecessor.as_ref())
            .map_err(catalog)?;
        if !admission.needs_observation()
            && observed.kind() != ReclaimMetadataObservationKind::Committed
        {
            return Err(catalog("元数据提交后未观察到原固定节点"));
        }
        let raw_read = if observed.kind() == ReclaimMetadataObservationKind::Committed {
            self.read_raw_result(&checked, &label, writing, &actual)?
        } else {
            None
        };
        checked.verify_proof()?;
        let task = self
            .catalog
            .record_reclaim_metadata_observation(reservation, self.key.round(), writing, &observed)
            .await
            .map_err(catalog)?;
        let needs_checkpoint = !matches!(actual.scope(), crate::ltfs::recovery::IndexSourceScope::Current { ip_generation: Some(ip), .. } if (1..=actual.construct().generation).contains(&ip));
        let commit =
            if observed.kind() == ReclaimMetadataObservationKind::Committed && !needs_checkpoint {
                Some(
                    ReclaimMetadataCommitEvidence::from_current_with_raw_read(
                        &task,
                        self.key.round(),
                        &actual,
                        predecessor.as_ref(),
                        raw_read,
                    )
                    .map_err(catalog)?,
                )
            } else {
                None
            };
        let unwritten = if observed.kind() == ReclaimMetadataObservationKind::Unchanged {
            ReclaimMetadataUnwrittenEvidence::from_clean_current(&task, self.key.round(), &actual)
                .map_err(catalog)?
        } else {
            None
        };
        Ok(ReclaimMetadataExecution {
            task,
            commit,
            unwritten,
            needs_checkpoint,
        })
    }
}

fn catalog(error: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!("FDB 回收元数据执行: {error}"))
}
