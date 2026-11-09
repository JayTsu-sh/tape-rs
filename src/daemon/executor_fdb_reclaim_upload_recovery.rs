//! Reconcile old-owner Q/I under a dedicated original-child authority. Ordinary
//! write admission stays strict; recovery never creates a new physical attempt.
use super::*;
use crate::managed_catalog::PublishOutcome;

pub(super) async fn step(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
) -> std::result::Result<bool, ReadError> {
    let mut after = None;
    loop {
        let page = control
            .catalog
            .reclaim_uploads_page(parent, owner.round, after.as_deref(), 16)
            .await
            .map_err(parent_error)?;
        for request in page.items {
            let pending = control
                .catalog
                .reclaim_upload_recovery_pending(parent, owner.round, &request)
                .await
                .map_err(parent_error)?;
            let terminal = control
                .catalog
                .active_upload_result(&request, control.route)
                .await
                .map_err(parent_error)?;
            if terminal.is_some() && !pending {
                if matches!(terminal, Some(PublishOutcome::RequestCancelled)) {
                    control
                        .catalog
                        .seal_reclaim_upload_cancellation(parent, owner.round, &request)
                        .await
                        .map_err(parent_error)?;
                }
                continue;
            }
            let physical = control
                .catalog
                .active_file_attempt(&request, control.route)
                .await
                .map_err(parent_error)?;
            let original = if let Some(original) = physical {
                original
            } else if pending {
                control
                    .catalog
                    .reclaim_upload_attempt(control.route, owner.round, &request)
                    .await
                    .map_err(parent_error)?
                    .ok_or_else(|| lost("原恢复收尾缺少封存目标"))?
            } else {
                continue;
            };
            if original.volume_id != layout.destination.volume {
                return Err(lost("原复制恢复目标不在固定布局中"));
            }
            let placement = control
                .catalog
                .volume_placement(original.volume_id)
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("原复制恢复归属缺失"))?;
            let head = control
                .catalog
                .active_loaded_volume_head(
                    &control.serial,
                    &placement.barcode,
                    control.route,
                    owner.round,
                )
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("原复制恢复卷头缺失"))?;
            let drive = fdb_loading::select_reclaim_recovery(
                owner,
                permit,
                control,
                &placement,
                head.index,
                (parent, &request),
            )
            .await?;
            control
                .catalog
                .begin_reclaim_upload_recovery(parent, owner.round, &request)
                .await
                .map_err(parent_error)?;
            let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
            let checked = fdb_loading::operation_guard(
                owner,
                permit,
                owner.devices[drive].dev.as_ref(),
                || {
                    futures_executor::block_on(
                        control.catalog.require_reclaim_upload_recovery_lane(
                            parent,
                            owner.round,
                            &request,
                        ),
                    )
                    .map_err(|e| TapeError::Catalog(e.to_string()))
                },
            );
            let tracked = fdb_monitor::Tracked::new(&checked, "reclaim_original_upload_recovery");
            let mounted =
                LtfsVolume::mount_with_budget(&tracked, &budget(owner, control)).map_err(lost)?;
            let recovered = crate::daemon::fdb_media::establish_active_media_owner(
                &control.catalog,
                mounted,
                &control.serial,
                owner.key,
                permit,
                control.storage.index_bytes.try_into().map_err(lost)?,
            )
            .await
            .map_err(lost)?;
            let mut volume = recovered.into_volume();
            let index = crate::managed_catalog::ActiveIndexIdentity::from(
                volume.verified_recovery_index_construct().map_err(lost)?,
            );
            checked.verify_proof().map_err(lost)?;
            let terminal = control
                .catalog
                .active_upload_result(&request, control.route)
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("原复制恢复后仍缺少确切终态"))?;
            match terminal {
                PublishOutcome::AlreadyApplied(_) | PublishOutcome::RequestCancelled => {}
                other => return Err(lost(format!("原复制恢复终态冲突: {other:?}"))),
            }
            // The actual index and exact original batch have now been verified,
            // not merely a transport retry or a fresh capacity sample.
            fdb_monitor::index_state(owner, control, &placement.barcode, index, false).await?;
            control
                .catalog
                .complete_reclaim_upload_recovery(parent, owner.round, &request, index)
                .await
                .map_err(parent_error)?;
            if matches!(terminal, PublishOutcome::RequestCancelled) {
                control
                    .catalog
                    .seal_reclaim_upload_cancellation(parent, owner.round, &request)
                    .await
                    .map_err(parent_error)?;
            }
            drop(volume);
            owner.write_session = None;
            owner.fdb_read_session = None;
            owner.fdb_reads.insert(
                drive,
                fdb_loading::Mount {
                    barcode: placement.barcode,
                    volume: original.volume_id,
                    pool: placement.pool_id,
                    last_used: Instant::now(),
                },
            );
            return Ok(true);
        }
        let Some(next) = page.next else {
            return Ok(false);
        };
        after = Some(next);
    }
}
