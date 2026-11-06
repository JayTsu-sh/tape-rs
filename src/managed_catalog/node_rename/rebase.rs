//! New physical identities descend from a retained original absence result.
use super::*;
fn recipe_key(task: &NodeMutationTask, group: Uuid, volume: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = attempt_prefix(task, 9);
    key.extend_from_slice(group.as_bytes());
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}
fn same_logical_scope(current: &IntentRecord, root: &IntentRecord) -> Result<()> {
    current.validate()?;
    root.validate()?;
    if current.library != root.library
        || current.namespace != root.namespace
        || current.pool != root.pool
        || current.epoch != root.epoch
        || current.operation != root.operation
        || current.group != root.group
        || current.volume != root.volume
        || current.logical != root.logical
        || current.global != root.global
        || current.page != root.page
        || current.owner < root.owner
        || current.created < root.created
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
async fn require_parent_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    root: &NodeRenameVolumeIntent,
    record: &IntentRecord,
) -> Result<NodeRenameIntentClosure> {
    let IntentLineage::Rebased {
        root: reference,
        parent,
    } = &record.lineage
    else {
        return Err(invalid_attempt());
    };
    if *reference != root.record.original_reference()? {
        return Err(invalid_attempt());
    }
    same_logical_scope(record, &root.record)?;
    let value = closure::by_reference_at(tr, task.namespace, root.volume(), parent).await?;
    if value.intent().record.original_reference()? != *reference
        || record.owner < value.closed_owner()
        || record.created < value.closed_at_millis()
    {
        return Err(invalid_attempt());
    }
    same_logical_scope(record, &value.intent().record)?;
    Ok(value)
}
pub(super) async fn current_intent_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    global: &GlobalHeader,
    group: &GroupRecord,
    reference: &IntentRef,
    page: u64,
    require_claim: bool,
) -> Result<NodeRenameVolumeIntent> {
    let ready = saved_ready_at(tr, &manifest.task, group, page).await?;
    if !ready.intents.contains(reference) {
        return Err(invalid_attempt());
    }
    let root = original_intent_at(tr, manifest, global, group, reference, page, false).await?;
    let id = raw_id(
        &tr.get(
            &intent_pointer(&manifest.task, root.group(), root.volume()),
            false,
        )
        .await?
        .ok_or_else(invalid_attempt)?,
    )?;
    let current = if id == root.id() {
        root.clone()
    } else {
        let raw = tr
            .get(
                &intent_key(&manifest.task, root.group(), root.volume(), id),
                false,
            )
            .await?
            .ok_or_else(invalid_attempt)?;
        let record = IntentRecord::decode(&raw)?;
        require_nonce_at(tr, &record).await?;
        if record_id(&record.id)? != id {
            return Err(invalid_attempt());
        }
        let closed = require_parent_at(tr, &manifest.task, &root, &record).await?;
        let raw = tr
            .get(
                &recipe_key(&manifest.task, root.group(), root.volume(), id),
                false,
            )
            .await?
            .ok_or_else(invalid_attempt)?;
        let recipe = NodeRenameVolumePlan {
            record: volume_plan::Record::decode(&raw)?,
        };
        require_volume_plan_scope(&recipe.record, manifest, record.owner)?;
        recipe.require_rebase_root(root.media_plan().recipe())?;
        if recipe.creator_owner_epoch() != record.owner
            || recipe.source_index() != closed.current_index()
            || recipe.record.facts.before_files != closed.file_count()
        {
            return Err(invalid_attempt());
        }
        let plan = NodeRenameMediaPlan::new(recipe, id, record.owner)?;
        let intent = NodeRenameVolumeIntent { record, plan };
        observation::require_original_intent(&intent)?;
        intent
    };
    if require_claim {
        let raw = tr
            .get(&node_attempts::volume_key(current.volume()), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let node_claims::NodeVolumeClaim::Rename(value) = node_claims::decode(&raw)? else {
            return Err(invalid_attempt());
        };
        let mut expected = claim(&current.record)?;
        expected.closed = value.closed;
        if *value != expected
            || value.closed.is_some() && group.phase != NodeRenameAttemptPhase::Writing
        {
            return Err(invalid_attempt());
        }
        closure::require_claim_at(tr, &value).await?;
    } else if group
        .completion
        .as_ref()
        .is_some_and(|c| page < c.released_pages)
        && tr
            .get(&node_attempts::volume_key(current.volume()), false)
            .await?
            .is_some()
    {
        return Err(invalid_attempt());
    }
    Ok(current)
}
/// Shared-slot validation blocks other work without reconstructing an index.
pub(super) async fn require_lineage_claim_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    group: &GroupRecord,
    record: &IntentRecord,
) -> Result<()> {
    let root_ref = record.original_reference()?;
    let page = saved_ready_at(tr, task, group, record.page).await?;
    if !page.intents.contains(&root_ref) {
        return Err(invalid_attempt());
    }
    if record.lineage == IntentLineage::Original {
        if record.owner != group.owner || record.created != group.created {
            return Err(invalid_attempt());
        }
    } else {
        let raw = tr
            .get(
                &intent_key(
                    task,
                    record_id(&record.group)?,
                    record_id(&record.volume)?,
                    record_id(&root_ref.id)?,
                ),
                false,
            )
            .await?
            .ok_or_else(invalid_attempt)?;
        let root = IntentRecord::decode(&raw)?;
        if root.lineage != IntentLineage::Original
            || root.original_reference()? != root_ref
            || root.owner != group.owner
            || root.created != group.created
        {
            return Err(invalid_attempt());
        }
        same_logical_scope(record, &root)?;
        let IntentLineage::Rebased { parent, .. } = &record.lineage else {
            unreachable!()
        };
        let closed =
            closure::by_reference_at(tr, task.namespace, record_id(&record.volume)?, parent)
                .await?;
        same_logical_scope(record, &closed.intent().record)?;
        if closed.intent().record.original_reference()? != root_ref
            || record.owner < closed.closed_owner()
            || record.created < closed.closed_at_millis()
        {
            return Err(invalid_attempt());
        }
    }
    Ok(())
}
impl ManagedCatalog {
    /// Atomically replace only a proven-absent current physical intent. The
    /// original global member, recipe, logical operation and time remain fixed.
    pub async fn admit_node_rename_rebased_intent(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        closed: &NodeRenameIntentClosure,
        projection: &mut NodeRenameVolumeProjection,
        id: Uuid,
    ) -> Result<NodeRenameVolumeIntent> {
        if id.is_nil() || id == closed.intent().id() || closed.intent().group() != attempt.id() {
            return Err(invalid_attempt());
        }
        let global = &attempt.global;
        let manifest = &global.manifest;
        if projection.original_request() != &manifest.request {
            return Err(invalid_attempt());
        }
        let facts = volume_plan::Facts::from_projection(
            projection,
            &manifest.header.request,
            manifest.header.summary.sha256,
            manifest.header.initial_rows_sha256,
            manifest.header.prepared_owner,
            manifest.header.prepared_time,
        )?;
        let binding = facts.binding()?;
        let created = management::now().max(closed.closed_at_millis());
        let reference = closed.intent().record.original_reference()?;
        self.db
            .run(|tr, _| {
                let facts = facts.clone();
                let binding = binding.clone();
                let reference = reference.clone();
                async move {
                    requests::retry_result(
                        async {
                            let task =
                                writing::require_held_original_at(&tr, route, owner, manifest)
                                    .await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let group = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if task.state != NodeMutationTaskState::Writing
                                || group.phase != NodeRenameAttemptPhase::Writing
                                || record_id(&group.id)? != attempt.id()
                                || group.fingerprint()? != attempt.fingerprint()
                                || group.owner > owner
                            {
                                return Err(invalid_attempt());
                            }
                            let current = current_intent_at(
                                &tr,
                                manifest,
                                &global.header,
                                &group,
                                &reference,
                                closed.intent().record.page,
                                true,
                            )
                            .await?;
                            if current.id() == id {
                                let IntentLineage::Rebased { parent, .. } = &current.record.lineage
                                else {
                                    return Err(invalid_attempt());
                                };
                                if parent.id != closed.intent().record.id
                                    || parent.fingerprint != closed.fingerprint()
                                    || current.plan.recipe().record.facts != facts
                                {
                                    return Err(invalid_attempt());
                                }
                                return Ok(current);
                            }
                            if current != *closed.intent()
                                || closure::known_at(&tr, &current).await?.as_ref() != Some(closed)
                            {
                                return Err(invalid_attempt());
                            }
                            if binding.source != closed.current_index()
                                || binding.source.volume_uuid != current.volume()
                                || facts.before_files != closed.file_count()
                                || live_guard::binding_at(&tr, &manifest.request, current.volume())
                                    .await?
                                    != binding
                            {
                                return Err(invalid("rename 重试投影与原关闭后的当前卷不符"));
                            }
                            revalidate_at(&tr, route, owner, &manifest.request).await?;
                            if active_volume::file_count_at(
                                &tr,
                                &task.library_serial,
                                current.volume(),
                            )
                            .await?
                                != closed.file_count()
                                || active_volume::recovery_active(&tr, current.volume()).await?
                            {
                                return Err(invalid_attempt());
                            }
                            let root = original_intent_at(
                                &tr,
                                manifest,
                                &global.header,
                                &group,
                                &reference,
                                current.record.page,
                                false,
                            )
                            .await?;
                            let recipe = NodeRenameVolumePlan {
                                record: volume_plan::Record::new(facts, owner)?,
                            };
                            recipe.require_rebase_root(root.media_plan().recipe())?;
                            let plan = NodeRenameMediaPlan::new(recipe.clone(), id, owner)?;
                            let record = IntentRecord {
                                version: 1,
                                library: current.record.library.clone(),
                                namespace: current.record.namespace.clone(),
                                pool: current.record.pool.clone(),
                                epoch: current.record.epoch,
                                operation: current.record.operation.clone(),
                                group: current.record.group.clone(),
                                volume: current.record.volume.clone(),
                                id: id.to_string(),
                                owner,
                                created,
                                logical: current.record.logical,
                                global: current.record.global,
                                recipe: recipe.fingerprint(),
                                physical: plan.fingerprint(),
                                page: current.record.page,
                                pool_revision: binding.pool_revision,
                                assignment_revision: binding.assignment_revision,
                                lineage: IntentLineage::Rebased {
                                    root: reference,
                                    parent: ClosureRef {
                                        id: current.record.id.clone(),
                                        fingerprint: closed.fingerprint(),
                                    },
                                },
                                checksum: [0; 32],
                            }
                            .seal()?;
                            let intent = NodeRenameVolumeIntent { record, plan };
                            observation::require_original_intent(&intent)?;
                            observation::require_observation_budget(&intent)?;
                            closure::require_budget(&intent)?;
                            let target = intent_key(&task, attempt.id(), intent.volume(), id);
                            let recipe_target =
                                recipe_key(&task, attempt.id(), intent.volume(), id);
                            let nonce = nonce_key(task.namespace, intent.volume(), id);
                            if tr.get(&nonce, false).await?.is_some()
                                || tr.get(&target, false).await?.is_some()
                                || tr.get(&recipe_target, false).await?.is_some()
                            {
                                return Err(invalid("rename 已使用或孤立物理意图 UUID 不能复用"));
                            }
                            let intent_bytes = record_bytes(&intent.record)?;
                            let recipe_bytes = recipe.record.encode()?;
                            let shared = claim_bytes(&intent.record)?;
                            let nonce_bytes = nonce_value(&intent.record)?;
                            let next_group = group.advance_writing()?;
                            let group_bytes = record_bytes(&next_group)?;
                            let group_target = group_key(&task, attempt.id());
                            // All fallible checks and encodings precede pointer/claim changes.
                            tr.set(&group_target, &group_bytes);
                            tr.set(&nonce, &nonce_bytes);
                            tr.set(&recipe_target, &recipe_bytes);
                            tr.set(&target, &intent_bytes);
                            tr.set(
                                &intent_pointer(&task, attempt.id(), intent.volume()),
                                id.as_bytes(),
                            );
                            tr.set(&node_attempts::volume_key(intent.volume()), &shared);
                            Ok(intent)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}
