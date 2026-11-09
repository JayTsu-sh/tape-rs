//! Continue only the original metadata attempt and its durable successor.
use super::*;
use crate::daemon::fdb_reclaim_media::{ReclaimMetadataRecovery, ReclaimMetadataWriter};
use crate::managed_catalog::{ReclaimMetadataRecoveryKind, ReclaimMetadataWrite};
use std::io::Seek;

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
            let operation = uuid::Uuid::new_v4();
            if task.plan().is_source_based() {
                let proof = writer
                    .read_source_replan(parent, &task, source)
                    .await
                    .map_err(lost)?;
                control
                    .catalog
                    .replan_reclaim_metadata_source(parent, owner.round, &task, operation, &proof)
                    .await
                    .map_err(parent_error)?;
            } else {
                let (file, request) = control
                    .catalog
                    .reclaim_metadata_file_context(parent, owner.round, &task)
                    .await
                    .map_err(parent_error)?
                    .ok_or_else(|| lost("原正文重新规划上下文缺失"))?;
                let input = control
                    .catalog
                    .reclaim_metadata_input(parent, owner.round, &file, &request)
                    .await
                    .map_err(parent_error)?;
                let donor = writer
                    .read_donor(parent, &file, &request, &task)
                    .await
                    .map_err(lost)?;
                let inventory =
                    crate::managed_catalog::ReclaimSourceInventory::from_captured(source)
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
                        owner.round,
                        &file,
                        &request,
                        &task,
                        operation,
                        &plan,
                        donor.evidence(),
                    )
                    .await
                    .map_err(parent_error)?;
            }
            return Ok(false);
        }
        if task.recovering() {
            if let ReclaimMetadataRecovery::Observed(proof) =
                writer.recover(parent, &task).await.map_err(lost)?
            {
                control
                    .catalog
                    .resolve_reclaim_metadata_recovery(parent, owner.round, &task, &proof)
                    .await
                    .map_err(parent_error)?;
            }
            owner.write_session = None;
            owner.fdb_read_session = None;
            return Ok(false);
        }
        let mut raw = if task.plan().is_raw_body() && !task.writing() {
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
        } else {
            None
        };
        let executed = if let Some(file) = raw.as_mut() {
            writer
                .execute_raw_body(parent, &task, Some(source), file)
                .await
        } else {
            writer.execute(parent, &task, Some(source)).await
        };
        let executed = match executed {
            Ok(executed) => executed,
            Err(error @ TapeError::RecoveryRestricted { .. }) => {
                // The guarded actor must independently prove the original tail;
                // failure to prove it retains the original Writing/claims.
                writer
                    .begin_discard(parent, &task)
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
            owner.write_session = None;
            owner.fdb_read_session = None;
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
