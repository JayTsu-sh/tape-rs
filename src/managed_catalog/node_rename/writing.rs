//! Whole-group admission and recovery of original held rename records.
use super::*;
pub(super) async fn require_held_original_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    manifest: &NodeRenameManifest,
) -> Result<NodeMutationTask> {
    let namespace =
        routing::require_runtime_active_owner(tr, manifest.request.library(), route, owner).await?;
    if namespace != manifest.task.namespace {
        return Err(invalid_attempt());
    }
    node_tasks::retained(tr, namespace, manifest.task.request_epoch).await?;
    let current = node_tasks::task_at(
        tr,
        &manifest.task.library_serial,
        namespace,
        manifest.task.request_epoch,
        manifest.task.operation,
    )
    .await?
    .ok_or_else(invalid_attempt)?;
    if current.kind != NodeMutationTaskKind::Rename
        || !matches!(
            current.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        )
        || current.library_serial != manifest.task.library_serial
        || current.namespace != manifest.task.namespace
        || current.pool != manifest.task.pool
        || current.request_epoch != manifest.task.request_epoch
        || current.operation != manifest.task.operation
        || current.object != manifest.task.object
        || current.fingerprint != manifest.task.fingerprint
        || current.accepted_owner_epoch != manifest.task.accepted_owner_epoch
        || current.created_at_millis != manifest.task.created_at_millis
        || current.prepared_owner_epoch != manifest.task.prepared_owner_epoch
        || current.prepared_at_millis != manifest.task.prepared_at_millis
        || manifest.request.fingerprint() != current.fingerprint
        || node_tasks::held_task_at(tr, namespace, current.pool)
            .await?
            .as_ref()
            != Some(&current)
    {
        return Err(invalid_attempt());
    }
    node_plan_objects::require_sealed_at(tr, &current.object).await?;
    require_header_at(tr, &current, &manifest.header).await?;
    Ok(current)
}
impl ManagedCatalog {
    /// Loads the original complete canonical descriptor under a held task.
    /// Detailed canonical-page validation remains in the original typed reader.
    pub async fn node_rename_held_manifest(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<NodeRenameManifest> {
        let (task, request) = self
            .node_rename_task_request(serial, route, owner, epoch, operation, PAGE_BYTES)
            .await?;
        if !matches!(
            task.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        ) {
            return Err(invalid_attempt());
        }
        let tr = self.db.create_trx()?;
        let raw = tr
            .get(&header_key(&task), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let header: Header = decode(&raw)?;
        validate_header(&header, &task)?;
        if !header.summary.complete || header.creator_owner > owner {
            return Err(invalid_attempt());
        }
        let manifest = NodeRenameManifest {
            task,
            request,
            header,
        };
        require_held_original_at(&tr, route, owner, &manifest).await?;
        Ok(manifest)
    }
}

/// No held_task_at call here: it dispatches to this verifier for Writing.
pub(in crate::managed_catalog) async fn require_writing_task_at(
    tr: &Transaction,
    task: &NodeMutationTask,
) -> Result<()> {
    if task.kind != NodeMutationTaskKind::Rename || task.state != NodeMutationTaskState::Writing {
        return Err(invalid_attempt());
    }
    let id = task.active_attempt.ok_or_else(invalid_attempt)?;
    let pointer = tr
        .get(&group_pointer(task), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    if raw_id(&pointer)? != id {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(&header_key(task), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let canonical: Header = decode(&raw)?;
    validate_header(&canonical, task)?;
    if !canonical.summary.complete {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(&global_key(task), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let global: GlobalHeader = decode_value(&raw)?;
    global.require_values(
        task.fingerprint,
        canonical.summary.sha256,
        canonical.summary.files,
    )?;
    let raw = tr
        .get(&group_key(task, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let group = GroupRecord::decode(&raw)?;
    group.require_task(task)?;
    if record_id(&group.id)? != id
        || group.global != global.summary.sha256
        || group.volumes != global.summary.volumes
        || group.pages != global.pages
    {
        return Err(invalid_attempt());
    }
    saved_ready_at(tr, task, &group, group.pages - 1).await?;
    node_plan_objects::require_sealed_at(tr, &task.object).await
}
impl ManagedCatalog {
    pub async fn node_rename_held_global_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
    ) -> Result<NodeRenameGlobalPlan> {
        let tr = self.db.create_trx()?;
        require_held_original_at(&tr, route, owner, manifest).await?;
        let raw = tr
            .get(&global_key(&manifest.task), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let header = GlobalHeader::decode(&raw, manifest)?;
        if header.creator > owner {
            return Err(invalid_attempt());
        }
        drop(tr);
        let mut summary = NodeRenameParticipantsSummary {
            volumes: 0,
            source_files: 0,
            target_volumes: 0,
            anchors: 0,
            sha256: initial(manifest, header.anchor, header.target),
        };
        let mut last = None;
        for index in 0..header.pages {
            let tr = self.db.create_trx()?;
            require_held_original_at(&tr, route, owner, manifest).await?;
            original_global_at(&tr, manifest, &header).await?;
            let page = member_page_at(&tr, manifest, &header, index).await?;
            if page.creator > owner
                || page.facts.previous != summary.sha256
                || index == 0 && page.creator != header.creator
            {
                return Err(invalid_attempt());
            }
            for member in &page.facts.members {
                let raw = tr
                    .get(&volume_plan_key(&manifest.task, member.volume), false)
                    .await?
                    .ok_or_else(invalid_attempt)?;
                let record = volume_plan::Record::decode(&raw)?;
                require_volume_plan_scope(&record, manifest, owner)?;
                if record.facts.binding()?.source.volume_uuid != member.volume
                    || record.facts.source_files() != member.source_files
                    || record.facts.fingerprint() != member.recipe
                {
                    return Err(invalid_attempt());
                }
            }
            add(&mut summary, &page, &mut last, header.anchor, header.target)?;
        }
        if summary != header.summary {
            return Err(invalid_attempt());
        }
        let tr = self.db.create_trx()?;
        require_held_original_at(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &header).await?;
        Ok(NodeRenameGlobalPlan {
            manifest: Arc::new(manifest.clone()),
            header,
        })
    }
    /// Discover the original group under Prepared or Writing, including after
    /// an ownership change. Returning its data is never permission to replay.
    pub async fn node_rename_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        global: &NodeRenameGlobalPlan,
    ) -> Result<Option<NodeRenameAttempt>> {
        let tr = self.db.create_trx()?;
        let task = require_held_original_at(&tr, route, owner, &global.manifest).await?;
        original_global_at(&tr, &global.manifest, &global.header).await?;
        let record = current_group_at(&tr, &global.manifest, &global.header).await?;
        if record.as_ref().is_some_and(|record| record.owner > owner)
            || task.state == NodeMutationTaskState::Writing && record.is_none()
        {
            return Err(invalid_attempt());
        }
        Ok(record.map(|record| NodeRenameAttempt {
            global: global.clone(),
            record,
        }))
    }
    /// Original immutable intents; no current name, policy or physical source
    /// is substituted during recovery. Actual I/O has a separate write gate.
    pub async fn node_rename_volume_intents(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        index: u64,
    ) -> Result<Vec<NodeRenameVolumeIntent>> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        require_held_original_at(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let record = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if record_id(&record.id)? != attempt.id() || record.owner > owner {
            return Err(invalid_attempt());
        }
        let page = saved_ready_at(&tr, &manifest.task, &record, index).await?;
        let claims_live = matches!(
            record.phase,
            NodeRenameAttemptPhase::Preparing
                | NodeRenameAttemptPhase::Ready
                | NodeRenameAttemptPhase::Writing
                | NodeRenameAttemptPhase::Publishing
        ) && record
            .completion
            .as_ref()
            .is_none_or(|c| index >= c.released_pages)
            || record.phase == NodeRenameAttemptPhase::ClosingPrepared
                && index >= record.closed_pages;
        let mut intents = Vec::new();
        for reference in &page.intents {
            intents.push(
                rebase::current_intent_at(
                    &tr,
                    manifest,
                    &global.header,
                    &record,
                    reference,
                    index,
                    claims_live,
                )
                .await?,
            );
        }
        Ok(intents)
    }
    /// Atomically arm a fully recorded original group. This does not move a
    /// cartridge, obtain PR, stage an index or publish the logical rename.
    pub async fn admit_node_rename_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &mut NodeRenameAttempt,
    ) -> Result<()> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let id = attempt.id();
        let tr = self.db.create_trx()?;
        let task = require_held_original_at(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let expected = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if record_id(&expected.id)? != id || expected.owner > owner {
            return Err(invalid_attempt());
        }
        if task.state != NodeMutationTaskState::Writing
            && expected.phase != NodeRenameAttemptPhase::Ready
        {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);
        let mut previous = expected.initial_chain()?;
        for index in 0..expected.ready_pages {
            let tr = self.db.create_trx()?;
            require_held_original_at(&tr, route, owner, manifest).await?;
            original_global_at(&tr, manifest, &global.header).await?;
            if current_group_at(&tr, manifest, &global.header)
                .await?
                .as_ref()
                != Some(&expected)
            {
                return Err(invalid_attempt());
            }
            let page = saved_ready_at(&tr, &manifest.task, &expected, index).await?;
            if page.previous != previous {
                return Err(invalid_attempt());
            }
            for reference in &page.intents {
                let intent = rebase::current_intent_at(
                    &tr,
                    manifest,
                    &global.header,
                    &expected,
                    reference,
                    index,
                    true,
                )
                .await?;
                observation::require_observation_budget(&intent)?;
                closure::require_budget(&intent)?;
            }
            previous = page.chain()?;
        }
        if previous != expected.ready_chain {
            return Err(invalid_attempt());
        }
        let record = self
            .db
            .run(|tr, _| {
                let expected = expected.clone();
                async move {
                    requests::retry_result(
                        async {
                            let task =
                                require_held_original_at(&tr, route, owner, manifest).await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let mut current = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if record_id(&current.id)? != id {
                                return Err(invalid_attempt());
                            }
                            if task.state == NodeMutationTaskState::Writing {
                                if current.fingerprint()? != expected.fingerprint()?
                                    || current.ready_chain != expected.ready_chain
                                {
                                    return Err(invalid_attempt());
                                }
                                return Ok(current);
                            }
                            if current != expected
                                || current.phase != NodeRenameAttemptPhase::Ready
                                || current.owner != owner
                            {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let baseline =
                                revalidate_at(&tr, route, owner, &manifest.request).await?;
                            publication::require_budget(&manifest.request, &baseline.source)?;
                            current.phase = NodeRenameAttemptPhase::Writing;
                            current.progress_revision = 1;
                            let current = current.seal()?;
                            let encoded = record_bytes(&current)?;
                            node_tasks::mark_rename_writing_at(&tr, &task, id)?;
                            tr.set(&group_key(&manifest.task, id), &encoded);
                            Ok(current)
                        }
                        .await,
                    )
                }
            })
            .await??;
        attempt.record = record;
        Ok(())
    }
    /// Check metadata ownership, current policy and the exact original intent
    /// immediately before a caller separately verifies its device PR/source.
    pub async fn check_node_rename_write_intent(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        intent: &NodeRenameVolumeIntent,
    ) -> Result<()> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        let task = require_held_original_at(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let group = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if task.state != NodeMutationTaskState::Writing
            || group.phase != NodeRenameAttemptPhase::Writing
            || record_id(&group.id)? != attempt.id()
            || group.owner > owner
            || intent.record.owner != owner
        {
            return Err(CatalogError::OwnerFenced);
        }
        let reference = intent.record.original_reference()?;
        let original = rebase::current_intent_at(
            &tr,
            manifest,
            &global.header,
            &group,
            &reference,
            intent.record.page,
            true,
        )
        .await?;
        if original != *intent {
            return Err(invalid_attempt());
        }
        if closure::at(&tr, intent).await?.is_some() {
            return Err(invalid("rename 原按卷尝试已关闭，不能重放"));
        }
        let binding = live_guard::binding_at(&tr, &manifest.request, intent.volume()).await?;
        if binding != intent.plan.recipe().record.facts.binding()? {
            return Err(invalid("rename 原卷来源或策略修订已改变"));
        }
        Ok(())
    }
}
