//! Continue only the original metadata attempt and its durable successor.
use super::*;
use crate::daemon::fdb_reclaim_media::{ReclaimMetadataRecovery, ReclaimMetadataWriter};
use crate::managed_catalog::{ReclaimMetadataRecoveryKind, ReclaimMetadataWrite};
use std::io::Seek;

/// A single bounded anonymous file. The retained task is only an exact local
/// handoff binding; all execution/unknown-result authority remains in FDB.
pub(super) struct RawBodySpool {
    file: std::fs::File,
    length: u64,
    written: u64,
    original: Option<Box<ReclaimMetadataWrite>>,
}
impl RawBodySpool {
    pub(super) fn new(
        directory: &std::path::Path,
        length: u64,
        budget: crate::local_read_storage::ReadStorageBudget,
    ) -> std::result::Result<Self, ReadError> {
        Ok(Self {
            file: crate::local_read_storage::download_file(directory, length, budget)
                .map_err(lost)?,
            length,
            written: 0,
            original: None,
        })
    }
    fn fresh(task: &ReclaimMetadataWrite) -> bool {
        task.plan().is_raw_body()
            && task.previous_attempt().is_none()
            && task.id() == task.first_attempt()
            && task.id() == task.plan_origin()
            && task.recovery_epoch() == 0
            && !task.writing()
            && !task.closed()
            && !task.resolved()
            && !task.recovering()
            && !task.completed()
    }
    pub(super) fn bind(
        mut self,
        task: &ReclaimMetadataWrite,
        proof: &crate::managed_catalog::ReclaimRawSourceRead,
    ) -> std::result::Result<Option<Self>, ReadError> {
        if !Self::fresh(task) {
            return Ok(None);
        }
        if proof.observed_owner() != task.owner()
            || proof.source_node().uid() != task.plan().source_uid()
            || self.written != self.length
            || task.plan().raw_body_length().map_err(parent_error)? != self.length
            || self.file.metadata().map_err(lost)?.len() != self.length
        {
            return Err(lost("原始正文暂存与原实读证明/新固定任务不符"));
        }
        self.original = Some(Box::new(task.clone()));
        Ok(Some(self))
    }
    fn into_original_file(
        mut self,
        task: &ReclaimMetadataWrite,
    ) -> std::result::Result<Option<std::fs::File>, ReadError> {
        if self.original.as_deref() != Some(task) || !Self::fresh(task) {
            return Ok(None);
        }
        self.file.rewind().map_err(lost)?;
        Ok(Some(self.file))
    }
}
impl std::io::Write for RawBodySpool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.length - self.written {
            return Err(std::io::Error::other("原始正文超过已预留暂存长度"));
        }
        let count = std::io::Write::write(&mut self.file, bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

// The resolved-only branch is mutually exclusive with write admission.
// Construct its future here so its planning temporaries do not enlarge the
// ordinary writer's poll frame. All original recovery facts remain unchanged.
fn replan_resolved<'a, 'device: 'a>(
    writer: &'a mut ReclaimMetadataWriter<'device>,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    task: &'a ReclaimMetadataWrite,
    source: &'a CapturedIndexSource,
    owner_round: u64,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    Box::pin(async move {
        let operation = uuid::Uuid::new_v4();
        if task.plan().is_source_based() {
            let proof = writer
                .read_source_replan(parent, task, source)
                .await
                .map_err(lost)?;
            control
                .catalog
                .replan_reclaim_metadata_source(parent, owner_round, task, operation, &proof)
                .await
                .map_err(parent_error)?;
        } else {
            let (file, request) = control
                .catalog
                .reclaim_metadata_file_context(parent, owner_round, task)
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("原正文重新规划上下文缺失"))?;
            let input = control
                .catalog
                .reclaim_metadata_input(parent, owner_round, &file, &request)
                .await
                .map_err(parent_error)?;
            let donor = writer
                .read_donor(parent, &file, &request, task)
                .await
                .map_err(lost)?;
            let inventory = crate::managed_catalog::ReclaimSourceInventory::from_captured(source)
                .map_err(parent_error)?;
            let plan = inventory
                .plan_reclaimed_body_after_recovery(
                    &input,
                    donor.target(),
                    task.plan().path(),
                    donor.evidence(),
                )
                .map_err(parent_error)?;
            control
                .catalog
                .replan_reclaim_metadata_write(
                    parent,
                    owner_round,
                    &file,
                    &request,
                    task,
                    operation,
                    &plan,
                    donor.evidence(),
                )
                .await
                .map_err(parent_error)?;
        }
        Ok(false)
    })
}

fn recover_original<'a, 'device: 'a>(
    writer: &'a mut ReclaimMetadataWriter<'device>,
    owner: &'a Active,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    task: &'a ReclaimMetadataWrite,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>>
{
    Box::pin(async move {
        if let ReclaimMetadataRecovery::Observed(proof) =
            writer.recover(parent, task).await.map_err(lost)?
        {
            control
                .catalog
                .resolve_reclaim_metadata_recovery(parent, owner.round, task, &proof)
                .await
                .map_err(parent_error)?;
        }
        Ok(())
    })
}

fn prepare_raw_file<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    task: &'a ReclaimMetadataWrite,
    raw_spool: Option<RawBodySpool>,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<Option<std::fs::File>, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        // A changed/recovered task cannot inherit a prior local file. Existing
        // tasks use the original observe/re-read path, never a blind replay.
        let fresh_file = raw_spool
            .map(|spool| spool.into_original_file(task))
            .transpose()?
            .flatten();
        let raw = if task.plan().is_raw_body() && !task.writing() {
            if fresh_file.is_some() {
                fresh_file
            } else {
                let source_drive = device(owner, &layout.source.drive_serial)?;
                let mut file = crate::local_read_storage::download_file(
                    owner.files.spool_dir(),
                    task.plan().raw_body_length().map_err(parent_error)?,
                    control.storage,
                )
                .map_err(lost)?;
                reader(owner, permit, control, source_drive)
                    .read_reclaim_raw_source_to(
                        parent,
                        task.plan().source_uid(),
                        owner.files.spool_dir(),
                        &mut file,
                    )
                    .await
                    .map_err(lost)?;
                file.rewind().map_err(lost)?;
                Some(file)
            }
        } else {
            None
        };
        Ok(raw)
    })
}

fn settle_execution<'a, 'device: 'a>(
    writer: &'a mut ReclaimMetadataWriter<'device>,
    owner: &'a Active,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    task: &'a ReclaimMetadataWrite,
    executed: crate::error::Result<crate::daemon::fdb_reclaim_media::ReclaimMetadataExecution>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    Box::pin(async move {
        let executed = match executed {
            Ok(executed) => executed,
            Err(error @ TapeError::RecoveryRestricted { .. }) => {
                // The guarded actor must independently prove the original tail;
                // failure to prove it retains the original Writing/claims.
                writer
                    .begin_discard(parent, task)
                    .await
                    .map_err(|_| lost(error))?;
                return Ok(false);
            }
            Err(error) => return Err(lost(error)),
        };
        if let Some(proof) = executed.commit_evidence() {
            control
                .catalog
                .publish_reclaim_metadata_write(parent, owner.round, executed.task(), proof)
                .await
                .map_err(parent_error)?;
            return Ok(true);
        }
        if let Some(proof) = executed.unwritten_evidence() {
            control
                .catalog
                .close_unwritten_reclaim_metadata_write(parent, owner.round, executed.task(), proof)
                .await
                .map_err(parent_error)?;
        } else if executed.needs_checkpoint() {
            writer
                .begin_checkpoint(parent, executed.task())
                .await
                .map_err(lost)?;
        }
        Ok(false)
    })
}

pub(super) fn step<'a>(
    owner: &'a mut Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    source: &'a CapturedIndexSource,
    original: &'a ReclaimMetadataWrite,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    step_with_spool(
        owner,
        permit,
        control,
        parent,
        layout,
        (source, None),
        original,
    )
}

pub(super) fn step_with_spool<'a>(
    owner: &'a mut Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    source: (&'a CapturedIndexSource, Option<RawBodySpool>),
    original: &'a ReclaimMetadataWrite,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    let (source, raw_spool) = source;
    Box::pin(async move {
        let task = control
            .catalog
            .claim_reclaim_metadata_attempt(parent, owner.round, original)
            .await
            .map_err(parent_error)?;
        if control
            .catalog
            .reclaim_metadata_publication(parent, owner.round, &task)
            .await
            .map_err(parent_error)?
            .is_some()
        {
            return Ok(true);
        }
        if let Some(resolution) = control
            .catalog
            .reclaim_metadata_resolution(parent, owner.round, &task)
            .await
            .map_err(parent_error)?
            && resolution.kind() == ReclaimMetadataRecoveryKind::Committed
        {
            return Ok(true);
        }
        let target = device(owner, &layout.destination.drive_serial)?;
        let budget = budget(owner, control);
        let mut writer = ReclaimMetadataWriter {
            catalog: &control.catalog,
            device: owner.devices[target].dev.as_ref(),
            route: control.route,
            key: owner.key,
            permission: permit,
            budget: &budget,
        };
        if task.closed() {
            control
                .catalog
                .retry_reclaim_metadata_write(parent, owner.round, &task)
                .await
                .map_err(parent_error)?;
            return Ok(false);
        }
        if task.resolved() {
            return replan_resolved(&mut writer, control, parent, &task, source, owner.round).await;
        }
        if task.recovering() {
            recover_original(&mut writer, owner, control, parent, &task).await?;
            owner.write_session = None;
            owner.fdb_read_session = None;
            return Ok(false);
        }
        let mut raw =
            prepare_raw_file(owner, permit, control, parent, layout, &task, raw_spool).await?;
        let executed = if let Some(file) = raw.as_mut() {
            writer
                .execute_raw_body(parent, &task, Some(source), file)
                .await
        } else {
            writer.execute(parent, &task, Some(source)).await
        };
        let published =
            settle_execution(&mut writer, owner, control, parent, &task, executed).await?;
        if published {
            owner.write_session = None;
            owner.fdb_read_session = None;
        }
        Ok(published)
    })
}

#[cfg(test)]
#[path = "executor_fdb_reclaim_spool_unit_tests.rs"]
mod spool_tests;
