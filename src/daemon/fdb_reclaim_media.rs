//! One serialized metadata attempt on an already selected/loaded target drive.
//! The executor selects the original placement; this primitive never moves,
//! fences, repairs or formats media. Errors retain Writing and all claims.
use super::executor::fdb_operation_guard::Guard;
use super::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::{
    CapturedIndexSource, RecoveryBudget, capture_index_source, capture_recorded_index_source,
};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::managed_catalog::{
    ActiveFileRequest, ActiveLibraryRoute, ManagedCatalog, ReclaimFile,
    ReclaimMetadataCommitEvidence, ReclaimMetadataObservationKind, ReclaimMetadataWrite,
    ReclaimReservation,
};
use crate::scsi::reservation::{ReservationKey, verify_holder};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

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
}
impl ReclaimMetadataExecution {
    pub fn task(&self) -> &ReclaimMetadataWrite {
        &self.task
    }
    pub fn commit_evidence(&self) -> Option<&ReclaimMetadataCommitEvidence> {
        self.commit.as_ref()
    }
}

impl ReclaimMetadataWriter<'_> {
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
                .reclaim_metadata_write_status(
                    reservation,
                    self.key.round(),
                    task.plan().source_uid(),
                )
                .await
                .map_err(catalog)?
                .as_ref()
                != Some(task)
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
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
        source: Option<&CapturedIndexSource>,
    ) -> Result<ReclaimMetadataExecution> {
        self.require_scope(reservation).await?;
        if self
            .catalog
            .reclaim_metadata_publication(reservation, self.key.round(), file, request, original)
            .await
            .map_err(catalog)?
            .is_some()
        {
            let task = self
                .catalog
                .reclaim_metadata_write_status(
                    reservation,
                    self.key.round(),
                    original.plan().source_uid(),
                )
                .await
                .map_err(catalog)?
                .ok_or_else(|| catalog("已发布元数据任务缺失"))?;
            return Ok(ReclaimMetadataExecution { task, commit: None });
        }
        if let Some(source) = source {
            original.plan().validate_source(source).map_err(catalog)?;
        }
        if !original.writing() && source.is_none() {
            let current = self
                .catalog
                .reclaim_metadata_write_status(
                    reservation,
                    self.key.round(),
                    original.plan().source_uid(),
                )
                .await
                .map_err(catalog)?;
            if !current.is_some_and(|task| task.writing() && task.plan() == original.plan()) {
                return Err(catalog("首次元数据执行缺少原源节点实读"));
            }
        }
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let admission = self
            .catalog
            .admit_reclaim_metadata_write(reservation, self.key.round(), file, request, original)
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
                    file,
                    request,
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
        let before = capture_index_source(&drive, &label, self.budget)?;
        if !admission.needs_observation() {
            let source = source.ok_or_else(|| catalog("首次元数据执行缺少原来源"))?;
            let mut volume = LtfsVolume::mount_with_budget(&checked, self.budget)?;
            volume.set_reservation_guard(Some(self.key));
            plan.stage_reference(&mut volume, source, &before)
                .map_err(catalog)?;
            // A failed commit consumes the mount and stops this guard. A new
            // invocation must observe the original media; no inline retry.
            volume.commit()?;
        }
        let actual = if admission.needs_observation() {
            before
        } else {
            capture_index_source(&drive, &label, self.budget)?
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
        checked.verify_proof()?;
        let task = self
            .catalog
            .record_reclaim_metadata_observation(reservation, self.key.round(), writing, &observed)
            .await
            .map_err(catalog)?;
        let commit = if observed.kind() == ReclaimMetadataObservationKind::Committed {
            Some(
                ReclaimMetadataCommitEvidence::from_current(
                    &task,
                    self.key.round(),
                    &actual,
                    predecessor.as_ref(),
                )
                .map_err(catalog)?,
            )
        } else {
            None
        };
        Ok(ReclaimMetadataExecution { task, commit })
    }
}

fn catalog(error: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!("FDB 回收元数据执行: {error}"))
}
