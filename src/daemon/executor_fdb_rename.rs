//! Serial orchestration of the original multi-volume rename ledger.
use super::*;
use crate::daemon::fdb_rename_media::{NodeRenameExecution, NodeRenameVolumeOutcome};
use crate::managed_catalog::{
    ActiveIndexIdentity, CatalogError, NodeMutationTask, NodeMutationTaskKind,
    NodeMutationTaskState, NodeRenameAttempt, NodeRenameAttemptPhase, NodeRenameGlobalPlan,
    NodeRenameManifest, NodeRenameProjectionSpool, NodeRenameRequest, NodeRenameVolumeProjection,
    VolumePlacement,
};

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("FDB rename 自动执行需恢复确认: {error}"))
}
fn catalog_error(error: CatalogError) -> ReadError {
    if fdb_nodes::waiting(&error) {
        ReadError::Busy(error.to_string())
    } else if let CatalogError::Invalid(reason) = error {
        ReadError::Failed(reason.into())
    } else {
        lost(error)
    }
}
fn media_error(error: TapeError) -> ReadError {
    match error {
        TapeError::PoolBusy { .. }
        | TapeError::MediaUnavailable { .. }
        | TapeError::NotReady(_) => ReadError::Busy(error.to_string()),
        TapeError::Refused { reason } => ReadError::Failed(reason),
        error => lost(error),
    }
}
struct Rename<'a> {
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    budget: usize,
}
impl Rename<'_> {
    async fn guard(&self, owner: &Active) -> std::result::Result<(), ReadError> {
        fdb_loading::guard(owner, self.permit, self.control).await
    }
    async fn select(
        &self,
        owner: &mut Active,
        task: &NodeMutationTask,
        volume: Option<uuid::Uuid>,
    ) -> std::result::Result<(usize, VolumePlacement), ReadError> {
        for (placement, index) in
            fdb_nodes::candidates(owner, self.control, task, volume, false).await?
        {
            match fdb_loading::select_write(
                owner,
                self.permit,
                self.control,
                self.status,
                &placement,
                index,
            )
            .await
            {
                Ok(drive) => return Ok((drive, placement)),
                Err(ReadError::Busy(_) | ReadError::Failed(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(ReadError::Busy(
            "rename 原参与卷暂无合格介质或驱动器".into(),
        ))
    }
    fn remember(&self, owner: &mut Active, drive: usize, placement: VolumePlacement) {
        owner.fdb_reads.insert(
            drive,
            fdb_loading::Mount {
                barcode: placement.barcode,
                volume: placement.volume_id,
                pool: placement.pool_id,
                last_used: Instant::now(),
            },
        );
    }
    async fn projection(
        &self,
        round: u64,
        manifest: &NodeRenameManifest,
        spool: &mut NodeRenameProjectionSpool,
        request: &NodeRenameRequest,
        index: &crate::ltfs::index::LtfsIndex,
    ) -> std::result::Result<NodeRenameVolumeProjection, ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        let budget = self.control.storage.index_bytes;
        let baseline = catalog
            .check_node_rename_request(route, round, request)
            .await
            .map_err(catalog_error)?;
        let mut guard = Box::pin(catalog.build_node_rename_volume_guard(
            route,
            round,
            manifest,
            spool,
            index.volume_uuid,
            budget,
        ))
        .await
        .map_err(catalog_error)?;
        let mut source = guard
            .physical_source(&baseline, spool, index, budget)
            .map_err(catalog_error)?;
        Box::pin(catalog.build_node_rename_volume_projection(
            route,
            round,
            manifest,
            spool,
            &mut source,
            budget,
        ))
        .await
        .map_err(catalog_error)
    }
    async fn recipe(
        &self,
        owner: &mut Active,
        task: &NodeMutationTask,
        manifest: &NodeRenameManifest,
        spool: &mut NodeRenameProjectionSpool,
        request: &NodeRenameRequest,
        id: uuid::Uuid,
    ) -> std::result::Result<(), ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        if catalog
            .node_rename_volume_plan(route, owner.round, manifest, id)
            .await
            .map_err(catalog_error)?
            .is_some()
        {
            return Ok(());
        }
        let (drive, placement) = self.select(owner, task, Some(id)).await?;
        self.guard(owner).await?;
        let mounted = LtfsVolume::mount(owner.devices[drive].dev.as_ref()).map_err(media_error)?;
        // No original rename group is admitted yet. Generic recovery is legal
        // here, before participant recipes and shared claims are complete.
        let mut mounted = Box::pin(super::super::fdb_media::establish_active_media_owner(
            catalog,
            mounted,
            &self.control.serial,
            owner.key,
            self.permit,
            self.budget,
        ))
        .await
        .map_err(media_error)?
        .into_volume();
        let actual: ActiveIndexIdentity = mounted
            .verified_committed_index_construct()
            .map_err(media_error)?
            .into();
        if actual.volume_uuid != id {
            return Err(lost("rename 准备卷标签与原参与身份不符"));
        }
        let mut view = self
            .projection(owner.round, manifest, spool, request, mounted.index())
            .await?;
        catalog
            .save_node_rename_volume_plan(route, owner.round, manifest, &mut view)
            .await
            .map_err(catalog_error)?;
        drop(mounted);
        self.guard(owner).await?;
        self.remember(owner, drive, placement);
        Ok(())
    }
    async fn canonical(
        &self,
        owner: &Active,
        task: &NodeMutationTask,
    ) -> std::result::Result<NodeRenameManifest, ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        if task.state == NodeMutationTaskState::Writing {
            return catalog
                .node_rename_held_manifest(
                    &self.control.serial,
                    route,
                    owner.round,
                    task.request_epoch,
                    task.operation,
                )
                .await
                .map_err(catalog_error);
        }
        let inputs = Box::pin(catalog.capture_prepared_node_rename_inputs(
            &self.control.serial,
            route,
            owner.round,
            task.request_epoch,
            task.operation,
            self.budget,
        ))
        .await
        .map_err(catalog_error)?;
        let mut capture =
            Box::pin(catalog.resume_node_rename_manifest_capture(route, owner.round, &inputs))
                .await
                .map_err(catalog_error)?;
        while !capture.summary().complete {
            self.guard(owner).await?;
            Box::pin(catalog.next_node_rename_manifest_page(route, owner.round, &mut capture))
                .await
                .map_err(catalog_error)?;
        }
        catalog
            .finish_node_rename_manifest_capture(route, owner.round, &capture)
            .await
            .map_err(catalog_error)
    }
    async fn participants(
        &self,
        owner: &mut Active,
        task: &NodeMutationTask,
        request: &NodeRenameRequest,
        manifest: &NodeRenameManifest,
        spool: &mut NodeRenameProjectionSpool,
    ) -> std::result::Result<NodeRenameGlobalPlan, ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        if task.state == NodeMutationTaskState::Writing {
            return Box::pin(catalog.node_rename_held_global_plan(route, owner.round, manifest))
                .await
                .map_err(catalog_error);
        }
        if let Some(global) =
            Box::pin(catalog.node_rename_global_plan(route, owner.round, manifest))
                .await
                .map_err(catalog_error)?
        {
            return Ok(global);
        }
        let mut capture = match catalog
            .resume_node_rename_participants(route, owner.round, manifest, spool)
            .await
        {
            Ok(capture) => capture,
            Err(CatalogError::Incomplete) => {
                // Only pristine empty input can lack an original first anchor.
                let (_, placement) = self.select(owner, task, None).await?;
                catalog
                    .begin_node_rename_participants(
                        route,
                        owner.round,
                        manifest,
                        spool,
                        Some(placement.volume_id),
                    )
                    .await
                    .map_err(catalog_error)?
            }
            Err(error) => return Err(catalog_error(error)),
        };
        while !capture.complete() {
            self.guard(owner).await?;
            let ids = catalog
                .node_rename_next_participant_volumes(route, owner.round, &capture, spool)
                .await
                .map_err(catalog_error)?;
            for id in ids {
                Box::pin(self.recipe(owner, task, manifest, spool, request, id)).await?;
            }
            catalog
                .next_node_rename_participants_page(route, owner.round, &mut capture, spool)
                .await
                .map_err(catalog_error)?;
        }
        Box::pin(catalog.finish_node_rename_participants(route, owner.round, &capture))
            .await
            .map_err(catalog_error)
    }
    async fn arm(
        &self,
        owner: &Active,
        global: &NodeRenameGlobalPlan,
    ) -> std::result::Result<NodeRenameAttempt, ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        let mut prior = catalog
            .node_rename_attempt(route, owner.round, global)
            .await
            .map_err(catalog_error)?;
        if let Some(attempt) = &mut prior {
            if matches!(
                attempt.phase(),
                NodeRenameAttemptPhase::Writing | NodeRenameAttemptPhase::Publishing
            ) {
                return Ok(attempt.clone());
            }
            if attempt.original_owner_epoch() != owner.round
                || attempt.phase() == NodeRenameAttemptPhase::ClosingPrepared
            {
                self.guard(owner).await?;
                catalog
                    .close_prepared_node_rename_attempt(route, owner.round, attempt)
                    .await
                    .map_err(catalog_error)?;
            }
        }
        let mut attempt = match prior {
            Some(attempt) if attempt.phase() != NodeRenameAttemptPhase::ClosedPrepared => attempt,
            _ => catalog
                .begin_node_rename_attempt_preparation(
                    route,
                    owner.round,
                    global,
                    uuid::Uuid::new_v4(),
                )
                .await
                .map_err(catalog_error)?,
        };
        while attempt.phase() == NodeRenameAttemptPhase::Preparing {
            self.guard(owner).await?;
            catalog
                .next_node_rename_attempt_preparation_page(route, owner.round, &mut attempt)
                .await
                .map_err(catalog_error)?;
        }
        self.guard(owner).await?;
        Box::pin(catalog.admit_node_rename_attempt(route, owner.round, &mut attempt))
            .await
            .map_err(catalog_error)?;
        Ok(attempt)
    }
    async fn media(
        &self,
        owner: &mut Active,
        task: &NodeMutationTask,
        request: &NodeRenameRequest,
        manifest: &NodeRenameManifest,
        spool: &mut NodeRenameProjectionSpool,
        attempt: &NodeRenameAttempt,
    ) -> std::result::Result<(), ReadError> {
        let catalog = &self.control.catalog;
        let route = self.control.route;
        for page in 0..attempt.prepared_pages() {
            let intents =
                Box::pin(catalog.node_rename_volume_intents(route, owner.round, attempt, page))
                    .await
                    .map_err(catalog_error)?;
            for mut intent in intents {
                self.guard(owner).await?;
                if catalog
                    .node_rename_observation(route, owner.round, &intent)
                    .await
                    .map_err(catalog_error)?
                    .is_some()
                {
                    continue;
                }
                let (drive, placement) = self.select(owner, task, Some(intent.volume())).await?;
                self.guard(owner).await?;
                let tracked = fdb_monitor::Tracked::new(owner.devices[drive].dev.as_ref(), "write");
                let mounted = LtfsVolume::mount(&tracked);
                if mounted.is_err() {
                    fdb_monitor::io_error(
                        owner,
                        self.permit,
                        self.control,
                        &placement.barcode,
                        placement.volume_id,
                        tracked.error(),
                    )
                    .await;
                }
                let mounted = mounted.map_err(media_error)?;
                let execution = NodeRenameExecution::new(
                    catalog,
                    route,
                    owner.key,
                    self.permit,
                    self.control.storage.index_bytes,
                )
                .map_err(media_error)?;
                let result = Box::pin(execution.execute(mounted, attempt, &intent, None)).await;
                if result.is_err() {
                    fdb_monitor::io_error(
                        owner,
                        self.permit,
                        self.control,
                        &placement.barcode,
                        placement.volume_id,
                        tracked.error(),
                    )
                    .await;
                }
                let result = result.map_err(media_error)?;
                let outcome = result.outcome().clone();
                let mut mounted = result.into_volume();
                match outcome {
                    NodeRenameVolumeOutcome::Observed(_) => {}
                    NodeRenameVolumeOutcome::NeedsProjection => {
                        let view = self
                            .projection(owner.round, manifest, spool, request, mounted.index())
                            .await?;
                        let result =
                            Box::pin(execution.execute(mounted, attempt, &intent, Some(view)))
                                .await;
                        if result.is_err() {
                            fdb_monitor::io_error(
                                owner,
                                self.permit,
                                self.control,
                                &placement.barcode,
                                placement.volume_id,
                                tracked.error(),
                            )
                            .await;
                        }
                        mounted = result.map_err(media_error)?.into_volume();
                    }
                    NodeRenameVolumeOutcome::Closed(closed) => {
                        let mut view = self
                            .projection(owner.round, manifest, spool, request, mounted.index())
                            .await?;
                        intent = Box::pin(catalog.admit_node_rename_rebased_intent(
                            route,
                            owner.round,
                            attempt,
                            &closed,
                            &mut view,
                            uuid::Uuid::new_v4(),
                        ))
                        .await
                        .map_err(catalog_error)?;
                        let result =
                            Box::pin(execution.execute(mounted, attempt, &intent, Some(view)))
                                .await;
                        if result.is_err() {
                            fdb_monitor::io_error(
                                owner,
                                self.permit,
                                self.control,
                                &placement.barcode,
                                placement.volume_id,
                                tracked.error(),
                            )
                            .await;
                        }
                        mounted = result.map_err(media_error)?.into_volume();
                    }
                }
                drop(mounted);
                self.guard(owner).await?;
                self.remember(owner, drive, placement);
            }
        }
        Ok(())
    }
    async fn run(
        &self,
        owner: &mut Active,
        task: &NodeMutationTask,
    ) -> std::result::Result<bool, ReadError> {
        self.guard(owner).await?;
        let catalog = &self.control.catalog;
        let route = self.control.route;
        let (mut task, request) = Box::pin(catalog.node_rename_task_request(
            &self.control.serial,
            route,
            owner.round,
            task.request_epoch,
            task.operation,
            self.budget,
        ))
        .await
        .map_err(catalog_error)?;
        if task.state == NodeMutationTaskState::Queued {
            let baseline = catalog
                .check_node_rename_request(route, owner.round, &request)
                .await
                .map_err(catalog_error)?;
            catalog
                .check_active_pool_gate(&self.control.serial, route, owner.round, task.pool)
                .await
                .map_err(catalog_error)?;
            let mut fixed = std::collections::BTreeSet::new();
            if let crate::managed_catalog::ResolvedPath::File { copy, .. } = &baseline.source {
                fixed.insert(copy.volume_id);
            }
            if let Some(crate::managed_catalog::ResolvedPath::File { copy, .. }) = &baseline.target
            {
                fixed.insert(copy.volume_id);
            }
            if fixed.is_empty() {
                let _ = self.select(owner, &task, None).await?;
            } else {
                for id in fixed {
                    let _ = self.select(owner, &task, Some(id)).await?;
                }
            }
            task = Box::pin(catalog.prepare_node_rename_task(
                &self.control.serial,
                route,
                owner.round,
                task.request_epoch,
                task.operation,
                self.budget,
            ))
            .await
            .map_err(catalog_error)?;
        }
        if !matches!(
            task.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        ) {
            return Ok(true);
        }
        let manifest = Box::pin(self.canonical(owner, &task)).await?;
        let mut spool = Box::pin(catalog.build_node_rename_projection_spool(
            route,
            owner.round,
            &manifest,
            self.control.storage.index_bytes,
        ))
        .await
        .map_err(catalog_error)?;
        let global =
            Box::pin(self.participants(owner, &task, &request, &manifest, &mut spool)).await?;
        let mut attempt = Box::pin(self.arm(owner, &global)).await?;
        if attempt.phase() != NodeRenameAttemptPhase::Publishing {
            Box::pin(self.media(owner, &task, &request, &manifest, &mut spool, &attempt)).await?;
            attempt = catalog
                .node_rename_attempt(route, owner.round, &global)
                .await
                .map_err(catalog_error)?
                .ok_or_else(|| lost("rename 原组消失"))?;
            self.guard(owner).await?;
            Box::pin(catalog.seal_node_rename_s4s(route, owner.round, &mut attempt))
                .await
                .map_err(catalog_error)?;
        }
        loop {
            self.guard(owner).await?;
            if !catalog
                .release_node_rename_publication_page(route, owner.round, &mut attempt)
                .await
                .map_err(catalog_error)?
            {
                break;
            }
        }
        self.guard(owner).await?;
        Box::pin(catalog.publish_node_rename_attempt(route, owner.round, &attempt, self.budget))
            .await
            .map_err(catalog_error)?;
        self.guard(owner).await?;
        info!(
            "FDB rename 任务 {}/{} 自动执行完成",
            task.request_epoch, task.operation
        );
        Ok(true)
    }
}

pub(super) async fn advance(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    task: &NodeMutationTask,
) -> std::result::Result<bool, ReadError> {
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    let driver = Rename {
        permit,
        control,
        status,
        budget: usize::try_from(control.storage.index_bytes).map_err(lost)?,
    };
    match Box::pin(driver.run(owner, task)).await {
        Err(ReadError::Busy(_)) => Ok(false),
        Err(ReadError::Failed(reason)) => {
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
                .ok_or_else(|| lost("rename 原任务消失"))?;
            if current.state == NodeMutationTaskState::Writing {
                return Err(lost(reason));
            }
            fdb_nodes::reject(control, owner.round, &current, reason).await
        }
        result => result,
    }
}

/// Finish only original held renames before the generic loaded-volume pass.
/// Take a fresh inventory afterwards: serial rename execution may move media.
pub(super) async fn recover(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
) -> std::result::Result<(), ReadError> {
    let mut after = None;
    loop {
        fdb_loading::guard(owner, permit, control).await?;
        let page = control
            .catalog
            .pending_node_mutation_tasks(
                &control.serial,
                control.route,
                owner.round,
                after.as_deref(),
                32,
            )
            .await
            .map_err(lost)?;
        for task in page.items {
            if task.kind == NodeMutationTaskKind::Rename
                && matches!(
                    task.state,
                    NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
                )
                && !Box::pin(advance(owner, permit, control, status, &task)).await?
            {
                return Err(ReadError::Busy(
                    "rename 原任务等待合格介质，尚不能完成接任".into(),
                ));
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// Recover only the already loaded original intent before the startup DP/IP
/// proof. Keep the original group and hold; orchestration follows that proof.
pub(super) async fn recover_loaded<'v>(
    control: &FdbOwnerControl,
    permit: &Permission,
    key: ReservationKey,
    mut volume: LtfsVolume<'v>,
    task: &NodeMutationTask,
) -> crate::error::Result<LtfsVolume<'v>> {
    let catalog = &control.catalog;
    let route = control.route;
    let round = key.round();
    let error =
        |error: CatalogError| TapeError::Catalog(format!("FDB 原 rename 装载卷恢复: {error}"));
    volume.set_reservation_guard(Some(key));
    let actual: ActiveIndexIdentity = volume.verified_recovery_index_construct()?.into();
    if task.state != NodeMutationTaskState::Writing {
        return Ok(volume);
    }
    let manifest = catalog
        .node_rename_held_manifest(
            &control.serial,
            route,
            round,
            task.request_epoch,
            task.operation,
        )
        .await
        .map_err(error)?;
    let global = Box::pin(catalog.node_rename_held_global_plan(route, round, &manifest))
        .await
        .map_err(error)?;
    let attempt = catalog
        .node_rename_attempt(route, round, &global)
        .await
        .map_err(error)?
        .ok_or_else(|| TapeError::Catalog("rename Writing 原组缺失".into()))?;
    if attempt.phase() == NodeRenameAttemptPhase::Publishing {
        return Ok(volume);
    }
    if attempt.phase() != NodeRenameAttemptPhase::Writing {
        return Err(TapeError::Catalog("rename Writing 原组阶段不符".into()));
    }
    for page in 0..attempt.prepared_pages() {
        for intent in Box::pin(catalog.node_rename_volume_intents(route, round, &attempt, page))
            .await
            .map_err(error)?
        {
            if intent.volume() == actual.volume_uuid {
                let execution = NodeRenameExecution::new(
                    catalog,
                    route,
                    key,
                    permit,
                    control.storage.index_bytes,
                )?;
                return Ok(Box::pin(execution.execute(volume, &attempt, &intent, None))
                    .await?
                    .into_volume());
            }
        }
    }
    // A held pool can contain a loaded medium outside the original participants.
    // The normal exact DP/IP proof still runs before it becomes an eviction candidate.
    Ok(volume)
}
