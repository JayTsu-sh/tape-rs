//! Self-contained original rename S4 witnesses, independent of task retirement.
use super::*;
const MAX_OBSERVATION: usize = 64_000;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationWire {
    version: u8,
    recipe: String,
    intent: String,
    current: String,
    file_count: u64,
    owner: u64,
    time: u64,
    checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameObservation {
    intent: NodeRenameVolumeIntent,
    current: ActiveIndexIdentity,
    file_count: u64,
    owner: u64,
    time: u64,
}
impl NodeRenameObservation {
    pub fn intent(&self) -> &NodeRenameVolumeIntent {
        &self.intent
    }
    pub fn current_index(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn observed_owner(&self) -> u64 {
        self.owner
    }
}
fn observation_key(namespace: Uuid, volume: Uuid, generation: u64) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'R');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    key
}
impl ObservationWire {
    fn checksum(&self) -> Result<[u8; 32]> {
        let mut copy = self.clone();
        copy.checksum = [0; 32];
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-observation-record/1")
            .chain_update(node_mutation::canonical(&copy)?)
            .finalize()
            .into())
    }
}
pub(super) fn require_original_intent(value: &NodeRenameVolumeIntent) -> Result<()> {
    let intent = &value.record;
    let recipe = value.plan.recipe();
    intent.validate()?;
    recipe.require_intent_scope(&claim(intent)?, intent.logical, intent.created)?;
    let binding = recipe.record.facts.binding()?;
    if value.plan.fingerprint() != intent.physical
        || recipe.fingerprint() != intent.recipe
        || value.plan.attempt() != record_id(&intent.id)?
        || value.plan.owner_epoch() != intent.owner
        || recipe.record.facts.fingerprint != intent.logical
        || intent.pool_revision != binding.pool_revision
        || intent.assignment_revision != binding.assignment_revision
        || intent.volume != binding.source.volume_uuid.to_string()
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
fn validate_observation(value: &NodeRenameObservation) -> Result<()> {
    require_original_intent(&value.intent)?;
    let intent = &value.intent.record;
    let recipe = value.intent.plan.recipe();
    let before = recipe.source_index();
    if value.current.volume_uuid != before.volume_uuid
        || Some(value.current.generation) != before.generation.checked_add(1)
        || value.current.location.partition != before.location.partition
        || value.current.location.start_block <= before.location.start_block
        || value.current.incremental
        || value.file_count != recipe.record.facts.files
        || value.owner < intent.owner
        || !owner_valid(value.owner)
        || !(intent.created..=MAX_MILLIS).contains(&value.time)
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
pub(super) fn encode_observation(value: &NodeRenameObservation) -> Result<Vec<u8>> {
    validate_observation(value)?;
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(value.current.volume_uuid, &current)?;
    let mut wire = ObservationWire {
        version: 1,
        recipe: URL_SAFE_NO_PAD.encode(value.intent.plan.recipe().record.encode()?),
        intent: URL_SAFE_NO_PAD.encode(record_bytes(&value.intent.record)?),
        current: URL_SAFE_NO_PAD.encode(current),
        file_count: value.file_count,
        owner: value.owner,
        time: value.time,
        checksum: [0; 32],
    };
    wire.checksum = wire.checksum()?;
    let bytes = node_mutation::canonical(&wire)?;
    if bytes.len() > MAX_OBSERVATION {
        return Err(invalid("rename S4 历史记录超过封口预算"));
    }
    Ok(bytes)
}
fn decode_observation(bytes: &[u8]) -> Result<NodeRenameObservation> {
    if bytes.len() > MAX_OBSERVATION {
        return Err(invalid_attempt());
    }
    let wire: ObservationWire = serde_json::from_slice(bytes).map_err(|_| invalid_attempt())?;
    if wire.version != 1
        || wire.checksum()? != wire.checksum
        || node_mutation::canonical(&wire)? != bytes
    {
        return Err(invalid_attempt());
    }
    let raw_recipe = URL_SAFE_NO_PAD
        .decode(&wire.recipe)
        .map_err(|_| invalid_attempt())?;
    let recipe = NodeRenameVolumePlan {
        record: volume_plan::Record::decode(&raw_recipe)?,
    };
    let raw_intent = URL_SAFE_NO_PAD
        .decode(&wire.intent)
        .map_err(|_| invalid_attempt())?;
    let record = IntentRecord::decode(&raw_intent)?;
    let plan = NodeRenameMediaPlan::new(recipe, record_id(&record.id)?, record.owner)?;
    let raw = URL_SAFE_NO_PAD
        .decode(&wire.current)
        .map_err(|_| invalid_attempt())?;
    let current = active_volume::decode_index(record_id(&record.volume)?, &raw)?;
    let value = NodeRenameObservation {
        intent: NodeRenameVolumeIntent { record, plan },
        current,
        file_count: wire.file_count,
        owner: wire.owner,
        time: wire.time,
    };
    if encode_observation(&value)? != bytes {
        return Err(invalid_attempt());
    }
    Ok(value)
}
/// Called before Writing admission, not after a medium has been changed.
pub(super) fn require_observation_budget(intent: &NodeRenameVolumeIntent) -> Result<()> {
    let recipe = URL_SAFE_NO_PAD.encode(intent.plan.recipe().record.encode()?);
    let original = URL_SAFE_NO_PAD.encode(record_bytes(&intent.record)?);
    // Encoded index identity is fixed-width. Numeric audit/count fields and
    // checksum use their maximum width; this is a size preview, not S4 proof.
    let wire = ObservationWire {
        version: 1,
        recipe,
        intent: original,
        current: "_".repeat(67),
        file_count: u64::MAX,
        owner: 0xFFFF_FFFF_FFFF,
        time: MAX_MILLIS,
        checksum: [255; 32],
    };
    if node_mutation::canonical(&wire)?.len() > MAX_OBSERVATION {
        return Err(invalid("rename 原意图无法封存完整历史证据"));
    }
    Ok(())
}

pub(super) async fn observation_at(
    tr: &Transaction,
    intent: &NodeRenameVolumeIntent,
) -> Result<Option<NodeRenameObservation>> {
    let before = intent.plan.recipe().source_index();
    let next = before
        .generation
        .checked_add(1)
        .ok_or_else(invalid_attempt)?;
    let raw = tr
        .get(
            &observation_key(
                record_id(&intent.record.namespace)?,
                before.volume_uuid,
                next,
            ),
            false,
        )
        .await?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = decode_observation(&raw)?;
    if value.intent != *intent {
        return Err(invalid_attempt());
    }
    Ok(Some(value))
}

/// No live task, canonical page or recipe chunk is needed by historical reads.
pub(in crate::managed_catalog) async fn require_rename_observation_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let raw = tr
        .get(
            &observation_key(placement.namespace, index.volume_uuid, index.generation),
            false,
        )
        .await?
        .ok_or_else(invalid_attempt)?;
    let value = decode_observation(&raw)?;
    let binding = value.intent.plan.recipe().record.facts.binding()?;
    if value.current != index
        || value.intent.id() != id
        || value.intent.record.library != serial
        || placement != binding.placement
        || require_volume_ready(tr, index.volume_uuid, None).await? != binding.scan
        || active_volume::stored_file_count_at(tr, index).await? != Some(value.file_count)
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
impl ManagedCatalog {
    /// Accept an actual, opaque, PR-checked original S4 under current authority.
    /// The logical namespace and both the pool and volume holds remain intact.
    pub async fn observe_node_rename_intent(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        intent: &NodeRenameVolumeIntent,
        evidence: &NodeRenameS4Evidence,
    ) -> Result<NodeRenameObservation> {
        if evidence.plan() != intent.media_plan() {
            return Err(invalid_attempt());
        }
        let now = management::now().max(intent.record.created);
        let global = &attempt.global;
        let manifest = &global.manifest;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            256_000,
                        ))?;
                        let task =
                            writing::require_held_original_at(&tr, route, owner, manifest).await?;
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
                        let reference = intent.record.original_reference()?;
                        if rebase::current_intent_at(
                            &tr,
                            manifest,
                            &global.header,
                            &group,
                            &reference,
                            intent.record.page,
                            true,
                        )
                        .await?
                            != *intent
                        {
                            return Err(invalid_attempt());
                        }
                        if closure::at(&tr, intent).await?.is_some() {
                            return Err(invalid("rename 原按卷尝试已关闭，不能再接纳 S4"));
                        }
                        let binding = intent.plan.recipe().record.facts.binding()?;
                        let placement = verified_placement(&tr, intent.volume())
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if placement != binding.placement
                            || require_volume_ready(&tr, intent.volume(), None).await?
                                != binding.scan
                        {
                            return Err(invalid_attempt());
                        }
                        let (scan, current) =
                            active_volume::observed_at(&tr, &task.library_serial, intent.volume())
                                .await?;
                        if scan != binding.scan {
                            return Err(invalid_attempt());
                        }
                        if let Some(saved) = observation_at(&tr, intent).await? {
                            if saved.current != evidence.current_index()
                                || current != saved.current
                                || active_volume::history_at(
                                    &tr,
                                    &task.library_serial,
                                    intent.volume(),
                                    current.generation,
                                )
                                .await?
                                .0 != active_volume::IndexKind::NodeRename(intent.id())
                            {
                                return Err(invalid_attempt());
                            }
                            return Ok(saved);
                        }
                        if current != binding.source {
                            return Err(invalid_attempt());
                        }
                        let current = evidence.current_index();
                        if active_volume::recovery_active(&tr, intent.volume()).await?
                            || tr
                                .get(
                                    &active_batch::batch_key(intent.volume(), current.generation),
                                    false,
                                )
                                .await?
                                .is_some()
                            || tr
                                .get(
                                    &active_volume::generation_key(
                                        intent.volume(),
                                        current.generation,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                            || active_volume::stored_file_count_at(&tr, current)
                                .await?
                                .is_some()
                        {
                            return Err(invalid_attempt());
                        }
                        let value = NodeRenameObservation {
                            intent: intent.clone(),
                            current,
                            file_count: intent.plan.recipe().record.facts.files,
                            owner,
                            time: now,
                        };
                        let encoded = encode_observation(&value)?;
                        let key =
                            observation_key(task.namespace, intent.volume(), current.generation);
                        let next_group = group.advance_writing()?;
                        let group_bytes = record_bytes(&next_group)?;
                        let group_target = group_key(&task, attempt.id());
                        // No fallible operation is permitted after the first mutation.
                        active_volume::set_rename_observation_at(
                            &tr,
                            binding.source,
                            current,
                            intent.id(),
                            value.file_count,
                        );
                        tr.set(&group_target, &group_bytes);
                        tr.set(&key, &encoded);
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }
    /// Data discovery before replay, including after later task retirement.
    pub async fn node_rename_observation(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        intent: &NodeRenameVolumeIntent,
    ) -> Result<Option<NodeRenameObservation>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, &intent.record.library, route, owner)
            .await?;
        if ns != record_id(&intent.record.namespace)? {
            return Err(invalid_attempt());
        }
        if closure::known_at(&tr, intent).await?.is_some() {
            return Ok(None);
        }
        let saved = observation_at(&tr, intent).await?;
        if saved.is_none() {
            let generation = intent
                .plan
                .recipe()
                .source_index()
                .generation
                .checked_add(1)
                .ok_or_else(invalid_attempt)?;
            if tr
                .get(
                    &active_volume::generation_key(intent.volume(), generation),
                    false,
                )
                .await?
                .is_some()
            {
                return Err(invalid_attempt());
            }
        }
        if let Some(value) = &saved {
            let (kind, index) = active_volume::history_at(
                &tr,
                &intent.record.library,
                intent.volume(),
                value.current.generation,
            )
            .await?;
            if kind != active_volume::IndexKind::NodeRename(intent.id()) || index != value.current {
                return Err(invalid_attempt());
            }
        }
        Ok(saved)
    }
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
