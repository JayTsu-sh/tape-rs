//! One durable Node task per writable Serving iteration; no-media waits do
//! not take the Prepared pool hold or prevent the management queue progressing.
use super::*;
use crate::daemon::fdb_node_media::execute_node_mutation;
use crate::managed_catalog::{
    ActiveIndexIdentity, CatalogError, NodeMutation, NodeMutationBaseline, NodeMutationRequest,
    NodeMutationTask, NodeMutationTaskKind, NodeMutationTaskState, ResolvedPath, VolumePlacement,
};

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("FDB 节点队列需恢复确认: {error}"))
}

pub(super) fn waiting(error: &CatalogError) -> bool {
    matches!(
        error.domain(),
        CatalogError::Incomplete
            | CatalogError::PoolBusy
            | CatalogError::PoolImporting
            | CatalogError::MediaUnavailable
    )
}

pub(super) async fn reject(
    control: &FdbOwnerControl,
    owner: u64,
    task: &NodeMutationTask,
    error: impl std::fmt::Display,
) -> std::result::Result<bool, ReadError> {
    let mut reason = String::new();
    for character in error.to_string().chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if reason.len() + character.len_utf8() > 500 {
            break;
        }
        reason.push(character);
    }
    let terminal = if task.kind == NodeMutationTaskKind::Rename {
        control
            .catalog
            .reject_node_rename_task(
                &control.serial,
                control.route,
                owner,
                task.request_epoch,
                task.operation,
                &reason,
            )
            .await
    } else {
        control
            .catalog
            .reject_node_mutation_task(
                &control.serial,
                control.route,
                owner,
                task.request_epoch,
                task.operation,
                &reason,
            )
            .await
    }
    .map_err(lost)?;
    info!(
        "FDB 节点任务 {}/{} 已结束: {:?} {}",
        task.request_epoch,
        task.operation,
        terminal.state,
        terminal.rejection_reason.as_deref().unwrap_or("")
    );
    Ok(true)
}

// This is only a selection hint. The admitted physical plan remains the
// authority for complete source/target/ancestor identity and UID allocation.
fn source_shape_allows(
    index: &crate::ltfs::index::LtfsIndex,
    request: &NodeMutationRequest,
    baseline: &NodeMutationBaseline,
) -> bool {
    let path = &baseline.path;
    let directory = index.find_directory(path);
    let file = index.find_file(path);
    let mut needed = match request.mutation() {
        NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. } => {
            if directory.is_some() || file.is_some() {
                return false;
            }
            1u64
        }
        NodeMutation::RemoveDirectory => {
            if file.is_some()
                || directory.is_some_and(|dir| !dir.files.is_empty() || !dir.subdirs.is_empty())
            {
                return false;
            }
            0
        }
        _ if matches!(baseline.current, Some(ResolvedPath::Directory { .. })) => {
            if file.is_some() {
                return false;
            }
            u64::from(directory.is_none())
        }
        _ => {
            if file.is_none() || directory.is_some() {
                return false;
            }
            0
        }
    };
    for (position, _) in path.match_indices('/').skip(1) {
        let parent = &path[..position];
        if index.find_file(parent).is_some() {
            return false;
        }
        if index.find_directory(parent).is_none() {
            needed += 1;
        }
    }
    index.highest_file_uid.checked_add(needed).is_some()
}

pub(super) async fn candidates(
    owner: &Active,
    control: &FdbOwnerControl,
    task: &NodeMutationTask,
    target: Option<uuid::Uuid>,
    new_link: bool,
) -> std::result::Result<Vec<(VolumePlacement, ActiveIndexIdentity)>, ReadError> {
    let inventory = inventory_inner(&owner.devices, true).map_err(lost)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut barcodes = Vec::new();
    for barcode in inventory
        .drives
        .iter()
        .filter_map(|slot| slot.loaded.as_ref().map(|(barcode, _)| barcode))
        .chain(inventory.slots.keys())
    {
        if seen.insert(barcode.clone()) {
            barcodes.push(barcode.clone());
        }
    }
    let pool = control
        .catalog
        .active_pool_for_route(&control.serial, control.route, task.pool)
        .await
        .map_err(lost)?
        .ok_or_else(|| lost("节点任务池已不存在"))?;
    let mut result = Vec::new();
    for barcode in barcodes {
        if inventory.cleaning.contains(&barcode) {
            continue;
        }
        let Some(head) = control
            .catalog
            .active_loaded_volume_head(&control.serial, &barcode, control.route, owner.round)
            .await
            .map_err(lost)?
        else {
            continue;
        };
        if head.pool_id != task.pool
            || !head.online
            || head.blacklisted
            || target.is_some_and(|volume| volume != head.volume_id)
        {
            continue;
        }
        if new_link
            && control
                .catalog
                .active_volume_file_count(
                    &control.serial,
                    control.route,
                    owner.round,
                    head.volume_id,
                )
                .await
                .map_err(lost)?
                >= pool.file_limit
        {
            continue;
        }
        let placement = control
            .catalog
            .volume_placement(head.volume_id)
            .await
            .map_err(lost)?
            .ok_or_else(|| lost("节点候选卷归属缺失"))?;
        if placement.namespace != task.namespace
            || placement.pool_id != task.pool
            || placement.library_serial != control.serial
            || placement.barcode != barcode
        {
            return Err(lost("节点候选卷归属不一致"));
        }
        result.push((placement, head.index));
    }
    Ok(result)
}

pub(super) async fn advance(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    after: &mut Option<Vec<u8>>,
) -> std::result::Result<bool, ReadError> {
    fdb_loading::guard(owner, permit, control).await?;
    let mut page = control
        .catalog
        .pending_node_mutation_tasks(
            &control.serial,
            control.route,
            owner.round,
            after.as_deref(),
            1,
        )
        .await;
    if after.is_some() && matches!(page, Err(CatalogError::Invalid(_))) {
        *after = None;
        page = control
            .catalog
            .pending_node_mutation_tasks(&control.serial, control.route, owner.round, None, 1)
            .await;
    }
    let page = page.map_err(lost)?;
    *after = page.next;
    let Some(task) = page.items.into_iter().next() else {
        return Ok(false);
    };
    owner.write_session = None;
    // The fair cursor advances for both protocols before dispatch. Rename
    // keeps its original two-path ledger and multi-volume execution boundary.
    if task.kind == NodeMutationTaskKind::Rename {
        return Box::pin(fdb_rename::advance(owner, permit, control, status, &task)).await;
    }
    let budget = usize::try_from(control.storage.index_bytes).map_err(lost)?;
    let (task, request) = match control
        .catalog
        .node_mutation_task_request(
            &control.serial,
            control.route,
            owner.round,
            task.request_epoch,
            task.operation,
            budget,
        )
        .await
    {
        Ok(value) => value,
        Err(error) if waiting(&error) => return Ok(false),
        Err(CatalogError::Invalid(reason)) if task.state != NodeMutationTaskState::Writing => {
            return reject(control, owner.round, &task, reason).await;
        }
        Err(error) => return Err(lost(error)),
    };
    if matches!(
        task.state,
        NodeMutationTaskState::Completed
            | NodeMutationTaskState::Cancelled
            | NodeMutationTaskState::Rejected
    ) {
        return Ok(true);
    }
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    let (baseline, target) = if task.state == NodeMutationTaskState::Writing {
        let attempt = control
            .catalog
            .node_mutation_attempt(
                &control.serial,
                control.route,
                owner.round,
                task.request_epoch,
                task.operation,
            )
            .await
            .map_err(lost)?
            .ok_or_else(|| lost("Writing 节点任务缺少原尝试"))?;
        (None, Some(attempt.previous().volume_uuid))
    } else {
        let baseline = match control
            .catalog
            .check_node_mutation_request(control.route, owner.round, &request)
            .await
        {
            Ok(value) => value,
            Err(error) if waiting(&error) => return Ok(false),
            Err(CatalogError::Invalid(reason)) => {
                return reject(control, owner.round, &task, reason).await;
            }
            Err(error) => return Err(lost(error)),
        };
        if task.state == NodeMutationTaskState::Queued {
            match control
                .catalog
                .check_active_pool_gate(&control.serial, control.route, owner.round, task.pool)
                .await
            {
                Ok(()) => {}
                Err(error) if waiting(&error) => return Ok(false),
                Err(CatalogError::Invalid(reason)) => {
                    return reject(control, owner.round, &task, reason).await;
                }
                Err(error) => return Err(lost(error)),
            }
        }
        let target = match &baseline.current {
            Some(ResolvedPath::File { copy, .. }) => Some(copy.volume_id),
            _ => None,
        };
        (Some(baseline), target)
    };
    for (placement, index) in candidates(
        owner,
        control,
        &task,
        target,
        task.state != NodeMutationTaskState::Writing
            && matches!(request.mutation(), NodeMutation::CreateSymlink { .. }),
    )
    .await?
    {
        let drive = match fdb_loading::select_write(
            owner, permit, control, status, &placement, index,
        )
        .await
        {
            Ok(drive) => drive,
            Err(ReadError::Busy(_) | ReadError::Failed(_)) => continue,
            Err(error) => return Err(error),
        };
        fdb_loading::guard(owner, permit, control).await?;
        let original_task = task.clone();
        let checked = fdb_loading::node_operation_guard(
            owner,
            permit,
            control,
            owner.devices[drive].dev.as_ref(),
            &original_task,
        );
        let tracked = fdb_monitor::Tracked::new(&checked, "write");
        let mounted = LtfsVolume::mount(&tracked);
        if mounted.is_err() {
            fdb_monitor::io_error(
                owner,
                permit,
                control,
                &placement.barcode,
                placement.volume_id,
                tracked.error(),
            )
            .await;
        }
        let mut volume = mounted.map_err(lost)?;
        volume.set_reservation_guard(Some(owner.key));
        let actual: ActiveIndexIdentity = volume
            .verified_recovery_index_construct()
            .map_err(lost)?
            .into();
        if actual.volume_uuid != placement.volume_id {
            return Err(lost("节点介质标签与所选目标卷不一致"));
        }
        let needs_recovery = task.state == NodeMutationTaskState::Writing
            || actual != index
            || volume.recovery().dp.tail != TailKind::Complete;
        let key = owner.key;
        let establish = |volume| {
            super::super::fdb_media::establish_active_media_owner(
                &control.catalog,
                volume,
                &control.serial,
                key,
                permit,
                budget,
            )
        };
        if needs_recovery {
            volume = Box::pin(establish(volume))
                .await
                .map_err(lost)?
                .into_volume();
        }
        if let Some(baseline) = &baseline
            && !source_shape_allows(volume.index(), &request, baseline)
        {
            continue;
        }
        if task.state != NodeMutationTaskState::Writing {
            // On a clean medium acquire the namespace-quiet Prepared hold
            // before generic cleanup can cancel any accepted file reservation.
            // An existing Prepared task returns before the global quiet check.
            match control
                .catalog
                .prepare_node_mutation_task(
                    &control.serial,
                    control.route,
                    owner.round,
                    task.request_epoch,
                    task.operation,
                    budget,
                )
                .await
            {
                Ok(current) => match current.state {
                    NodeMutationTaskState::Prepared => {}
                    NodeMutationTaskState::Queued | NodeMutationTaskState::Writing => {
                        return Ok(false);
                    }
                    _ => return Ok(true),
                },
                Err(error) if waiting(&error) => return Ok(false),
                Err(CatalogError::Invalid(reason)) => {
                    return reject(control, owner.round, &task, reason).await;
                }
                Err(error) => return Err(lost(error)),
            }
        }
        if !needs_recovery {
            volume = Box::pin(establish(volume))
                .await
                .map_err(lost)?
                .into_volume();
        }
        let committed = Box::pin(execute_node_mutation(
            &control.catalog,
            volume,
            control.route,
            owner.key,
            &task,
            budget,
        ))
        .await;
        if committed.is_err() {
            fdb_monitor::io_error(
                owner,
                permit,
                control,
                &placement.barcode,
                placement.volume_id,
                tracked.error(),
            )
            .await;
        }
        match committed {
            Ok(value) => drop(value.into_volume()),
            Err(
                TapeError::PoolBusy { .. }
                | TapeError::MediaUnavailable { .. }
                | TapeError::NotReady(_),
            ) => return Ok(false),
            Err(error @ TapeError::Refused { .. }) => {
                let current = control
                    .catalog
                    .node_mutation_task(
                        &control.serial,
                        control.route,
                        task.request_epoch,
                        task.operation,
                    )
                    .await
                    .map_err(lost)?
                    .ok_or_else(|| lost("节点任务消失"))?;
                if current.state == NodeMutationTaskState::Writing {
                    return Err(lost(error));
                }
                return reject(control, owner.round, &task, error).await;
            }
            Err(error) => return Err(lost(error)),
        }
        fdb_loading::guard(owner, permit, control).await?;
        owner.fdb_reads.insert(
            drive,
            fdb_loading::Mount {
                barcode: placement.barcode,
                volume: placement.volume_id,
                pool: placement.pool_id,
                last_used: Instant::now(),
            },
        );
        info!(
            "FDB 节点任务 {}/{} 自动执行完成",
            task.request_epoch, task.operation
        );
        return Ok(true);
    }
    Ok(false)
}
