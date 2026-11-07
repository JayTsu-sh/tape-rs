//! Immutable per-volume absence results; the global Writing hold stays intact.
use super::*;
const MAX_CLOSURE: usize = 64_000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameIntentClosure {
    intent: NodeRenameVolumeIntent,
    kind: NodeRenameAbsenceKind,
    current: ActiveIndexIdentity,
    file_count: u64,
    owner: u64,
    time: u64,
}
impl NodeRenameIntentClosure {
    pub fn intent(&self) -> &NodeRenameVolumeIntent {
        &self.intent
    }
    pub fn kind(&self) -> NodeRenameAbsenceKind {
        self.kind
    }
    pub fn current_index(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn closed_owner(&self) -> u64 {
        self.owner
    }
    pub fn closed_at_millis(&self) -> u64 {
        self.time
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&encode(self).expect("validated rename closure"))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    recipe: String,
    intent: String,
    kind: NodeRenameAbsenceKind,
    current: String,
    file_count: u64,
    owner: u64,
    time: u64,
    checksum: [u8; 32],
}
fn key(namespace: Uuid, volume: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'C');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}
fn fingerprint(raw: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-closure-reference/1")
        .chain_update(raw)
        .finalize()
        .into()
}
impl Wire {
    fn checksum(&self) -> Result<[u8; 32]> {
        let mut copy = self.clone();
        copy.checksum = [0; 32];
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-intent-closure/1")
            .chain_update(node_mutation::canonical(&copy)?)
            .finalize()
            .into())
    }
}
fn validate(value: &NodeRenameIntentClosure) -> Result<()> {
    observation::require_original_intent(&value.intent)?;
    let record = &value.intent.record;
    let recipe = value.intent.plan.recipe();
    let before = recipe.source_index();
    let valid = match value.kind {
        NodeRenameAbsenceKind::Unchanged => value.current == before,
        NodeRenameAbsenceKind::Discarded => {
            value.current.volume_uuid == before.volume_uuid
                && before.generation.checked_add(1) == Some(value.current.generation)
                && value.current.location.partition == before.location.partition
                && value.current.location.start_block > before.location.start_block
                && !value.current.incremental
        }
    };
    if !valid
        || value.file_count != recipe.record.facts.before_files
        || value.owner < record.owner
        || !owner_valid(value.owner)
        || !(record.created..=MAX_MILLIS).contains(&value.time)
    {
        return Err(invalid_attempt());
    }
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(before.volume_uuid, &current)?;
    Ok(())
}
fn encode(value: &NodeRenameIntentClosure) -> Result<Vec<u8>> {
    validate(value)?;
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    let mut wire = Wire {
        version: 1,
        recipe: URL_SAFE_NO_PAD.encode(value.intent.plan.recipe().record.encode()?),
        intent: URL_SAFE_NO_PAD.encode(record_bytes(&value.intent.record)?),
        kind: value.kind,
        current: URL_SAFE_NO_PAD.encode(current),
        file_count: value.file_count,
        owner: value.owner,
        time: value.time,
        checksum: [0; 32],
    };
    wire.checksum = wire.checksum()?;
    let bytes = node_mutation::canonical(&wire)?;
    if bytes.len() > MAX_CLOSURE {
        return Err(invalid("rename 原关闭记录超过封口预算"));
    }
    Ok(bytes)
}
fn decode(raw: &[u8]) -> Result<NodeRenameIntentClosure> {
    if raw.len() > MAX_CLOSURE {
        return Err(invalid_attempt());
    }
    let wire: Wire = serde_json::from_slice(raw).map_err(|_| invalid_attempt())?;
    if wire.version != 1
        || wire.checksum()? != wire.checksum
        || node_mutation::canonical(&wire)? != raw
    {
        return Err(invalid_attempt());
    }
    let bytes = |s: &str| URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid_attempt());
    let recipe = NodeRenameVolumePlan {
        record: volume_plan::Record::decode(&bytes(&wire.recipe)?)?,
    };
    let record = IntentRecord::decode(&bytes(&wire.intent)?)?;
    let plan = NodeRenameMediaPlan::new(recipe, record_id(&record.id)?, record.owner)?;
    let current = active_volume::decode_index(record_id(&record.volume)?, &bytes(&wire.current)?)?;
    let value = NodeRenameIntentClosure {
        intent: NodeRenameVolumeIntent { record, plan },
        kind: wire.kind,
        current,
        file_count: wire.file_count,
        owner: wire.owner,
        time: wire.time,
    };
    if encode(&value)? != raw {
        return Err(invalid_attempt());
    }
    Ok(value)
}
pub(super) fn require_budget(intent: &NodeRenameVolumeIntent) -> Result<()> {
    let wire = Wire {
        version: 1,
        recipe: URL_SAFE_NO_PAD.encode(intent.plan.recipe().record.encode()?),
        intent: URL_SAFE_NO_PAD.encode(record_bytes(&intent.record)?),
        kind: NodeRenameAbsenceKind::Discarded,
        current: "_".repeat(67),
        file_count: u64::MAX,
        owner: 0xFFFF_FFFF_FFFF,
        time: MAX_MILLIS,
        checksum: [255; 32],
    };
    if node_mutation::canonical(&wire)?.len() > MAX_CLOSURE {
        return Err(invalid("rename 原意图无法封存关闭证据"));
    }
    Ok(())
}
pub(super) async fn at(
    tr: &Transaction,
    intent: &NodeRenameVolumeIntent,
) -> Result<Option<NodeRenameIntentClosure>> {
    let raw = tr
        .get(
            &key(
                record_id(&intent.record.namespace)?,
                intent.volume(),
                intent.id(),
            ),
            false,
        )
        .await?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = decode(&raw)?;
    if value.intent != *intent {
        return Err(invalid_attempt());
    }
    Ok(Some(value))
}
/// A closed shared claim and its immutable result must always agree.
pub(super) async fn require_claim_at(
    tr: &Transaction,
    value: &node_claims::RenameVolumeClaim,
) -> Result<()> {
    let raw = tr
        .get(&key(value.namespace, value.volume, value.intent), false)
        .await?;
    match (value.closed, raw) {
        (None, None) => Ok(()),
        (Some(expected), Some(raw)) => {
            let closed = decode(&raw)?;
            let mut original = claim(&closed.intent.record)?;
            original.closed = Some(expected);
            if fingerprint(&raw) != expected || original != *value {
                return Err(invalid_attempt());
            }
            Ok(())
        }
        _ => Err(invalid_attempt()),
    }
}
pub(super) async fn require_cached_claim_at(
    tr: &Transaction,
    intent: &NodeRenameVolumeIntent,
) -> Result<()> {
    record_id(&intent.record.namespace)?;
    let Some(raw) = tr
        .get(&node_attempts::volume_key(intent.volume()), false)
        .await?
    else {
        return Ok(());
    };
    let node_claims::NodeVolumeClaim::Rename(value) = node_claims::decode(&raw)? else {
        return Ok(());
    };
    if value.intent != intent.id() {
        return Ok(());
    }
    let mut expected = claim(&intent.record)?;
    expected.closed = value.closed;
    if *value != expected {
        return Err(invalid_attempt());
    }
    require_claim_at(tr, &value).await
}

async fn require_correlation_at(tr: &Transaction, value: &NodeRenameIntentClosure) -> Result<()> {
    let recipe = value.intent.plan.recipe();
    let binding = recipe.record.facts.binding()?;
    if verified_placement(tr, value.intent.volume())
        .await?
        .as_ref()
        != Some(&binding.placement)
        || require_volume_ready(tr, value.intent.volume(), None).await? != binding.scan
    {
        return Err(invalid_attempt());
    }
    if value.kind == NodeRenameAbsenceKind::Discarded {
        let (kind, index) = active_volume::history_at(
            tr,
            &value.intent.record.library,
            value.intent.volume(),
            value.current.generation,
        )
        .await?;
        if kind != active_volume::IndexKind::NodeRenameDiscard(value.intent.id())
            || index != value.current
        {
            return Err(invalid_attempt());
        }
    }
    Ok(())
}
pub(in crate::managed_catalog) async fn require_rename_discard_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let raw = tr
        .get(&key(placement.namespace, index.volume_uuid, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let value = decode(&raw)?;
    let binding = value.intent.plan.recipe().record.facts.binding()?;
    if value.kind != NodeRenameAbsenceKind::Discarded
        || value.intent.id() != id
        || value.current != index
        || value.intent.record.library != serial
        || placement != binding.placement
        || require_volume_ready(tr, index.volume_uuid, None).await? != binding.scan
        || active_volume::stored_file_count_at(tr, index).await? != Some(value.file_count)
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
pub(super) async fn known_at(
    tr: &Transaction,
    intent: &NodeRenameVolumeIntent,
) -> Result<Option<NodeRenameIntentClosure>> {
    require_cached_claim_at(tr, intent).await?;
    let saved = at(tr, intent).await?;
    if let Some(value) = &saved {
        require_correlation_at(tr, value).await?;
    }
    Ok(saved)
}
impl ManagedCatalog {
    pub async fn node_rename_intent_closure(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        intent: &NodeRenameVolumeIntent,
    ) -> Result<Option<NodeRenameIntentClosure>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, &intent.record.library, route, owner)
            .await?;
        if ns != record_id(&intent.record.namespace)? {
            return Err(invalid_attempt());
        }
        known_at(&tr, intent).await
    }
    /// Close this original physical attempt only. Keep its shared exclusion and
    /// all successful participants until a separately admitted physical retry.
    pub async fn close_node_rename_intent(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        intent: &NodeRenameVolumeIntent,
        evidence: &NodeRenameAbsenceEvidence,
    ) -> Result<NodeRenameIntentClosure> {
        if evidence.plan() != intent.media_plan()
            || intent.group() != attempt.id()
            || intent.record.global != attempt.global.fingerprint()
            || intent.record.logical != attempt.global.request().fingerprint()
        {
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
                        let ns = routing::require_runtime_active_owner(
                            &tr,
                            &intent.record.library,
                            route,
                            owner,
                        )
                        .await?;
                        if ns != record_id(&intent.record.namespace)? {
                            return Err(invalid_attempt());
                        }
                        if let Some(saved) = known_at(&tr, intent).await? {
                            if saved.current != evidence.current_index()
                                || saved.kind != evidence.kind()
                                || saved.file_count != evidence.file_count()
                            {
                                return Err(invalid_attempt());
                            }
                            return Ok(saved);
                        }
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
                        if observation::observation_at(&tr, intent).await?.is_some() {
                            return Err(invalid("rename 原尝试已有 S4，不能关闭为未发生"));
                        }
                        let binding = intent.plan.recipe().record.facts.binding()?;
                        if verified_placement(&tr, intent.volume()).await?.as_ref()
                            != Some(&binding.placement)
                            || require_volume_ready(&tr, intent.volume(), None).await?
                                != binding.scan
                        {
                            return Err(invalid_attempt());
                        }
                        let (scan, current) =
                            active_volume::observed_at(&tr, &task.library_serial, intent.volume())
                                .await?;
                        if scan != binding.scan
                            || current != binding.source
                            || active_volume::file_count_at(
                                &tr,
                                &task.library_serial,
                                intent.volume(),
                            )
                            .await?
                                != evidence.file_count()
                            || active_volume::recovery_active(&tr, intent.volume()).await?
                        {
                            return Err(invalid_attempt());
                        }
                        let value = NodeRenameIntentClosure {
                            intent: intent.clone(),
                            kind: evidence.kind(),
                            current: evidence.current_index(),
                            file_count: evidence.file_count(),
                            owner,
                            time: now,
                        };
                        let encoded = encode(&value)?;
                        if value.kind == NodeRenameAbsenceKind::Discarded
                            && (tr
                                .get(
                                    &active_volume::generation_key(
                                        intent.volume(),
                                        value.current.generation,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                                || tr
                                    .get(
                                        &active_batch::batch_key(
                                            intent.volume(),
                                            value.current.generation,
                                        ),
                                        false,
                                    )
                                    .await?
                                    .is_some()
                                || active_volume::stored_file_count_at(&tr, value.current)
                                    .await?
                                    .is_some())
                        {
                            return Err(invalid_attempt());
                        }
                        let mut closed = claim(&intent.record)?;
                        closed.closed = Some(fingerprint(&encoded));
                        let claim_bytes = node_claims::encode(
                            &node_claims::NodeVolumeClaim::Rename(Box::new(closed)),
                        )?;
                        let target = key(ns, intent.volume(), intent.id());
                        let next_group = group.advance_writing()?;
                        let group_bytes = record_bytes(&next_group)?;
                        let group_target = group_key(&task, attempt.id());
                        // Every proof, read and encoding precedes these infallible sets.
                        if value.kind == NodeRenameAbsenceKind::Discarded {
                            active_volume::set_rename_discard_at(
                                &tr,
                                binding.source,
                                value.current,
                                intent.id(),
                                value.file_count,
                            );
                        }
                        tr.set(&group_target, &group_bytes);
                        tr.set(&target, &encoded);
                        tr.set(&node_attempts::volume_key(intent.volume()), &claim_bytes);
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
#[path = "closure_tests.rs"]
mod tests;

pub(super) async fn by_reference_at(
    tr: &Transaction,
    namespace: Uuid,
    volume: Uuid,
    reference: &ClosureRef,
) -> Result<NodeRenameIntentClosure> {
    let id = record_id(&reference.id)?;
    let raw = tr
        .get(&key(namespace, volume, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let value = decode(&raw)?;
    if fingerprint(&raw) != reference.fingerprint
        || value.intent.id() != id
        || value.intent.volume() != volume
        || record_id(&value.intent.record.namespace)? != namespace
    {
        return Err(invalid_attempt());
    }
    require_correlation_at(tr, &value).await?;
    Ok(value)
}
