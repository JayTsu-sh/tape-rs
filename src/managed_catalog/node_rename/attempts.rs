//! Strict original global-attempt records. Data alone never permits I/O.
use super::*;
const MAX_ATTEMPT_RECORD: usize = 8192;
const MAX_MILLIS: u64 = 253_402_300_799_999;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionSeal {
    revision: u64,
    chain: [u8; 32],
    released_pages: u64,
    released_chain: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRenameAttemptPhase {
    Preparing,
    Ready,
    Writing,
    Publishing,
    ClosingPrepared,
    ClosedPrepared,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupRecord {
    version: u8,
    library: String,
    namespace: String,
    pool: String,
    epoch: u64,
    operation: String,
    id: String,
    owner: u64,
    created: u64,
    logical: [u8; 32],
    global: [u8; 32],
    volumes: u64,
    pages: u64,
    phase: NodeRenameAttemptPhase,
    progress_revision: u64,
    completion: Option<CompletionSeal>,
    ready_pages: u64,
    ready_volumes: u64,
    ready_chain: [u8; 32],
    closed_pages: u64,
    closed_time: Option<u64>,
    closed_owner: Option<u64>,
    checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosureRef {
    id: String,
    fingerprint: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum IntentLineage {
    Original,
    Rebased { root: IntentRef, parent: ClosureRef },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentRecord {
    version: u8,
    library: String,
    namespace: String,
    pool: String,
    epoch: u64,
    operation: String,
    group: String,
    volume: String,
    id: String,
    owner: u64,
    created: u64,
    logical: [u8; 32],
    global: [u8; 32],
    recipe: [u8; 32],
    physical: [u8; 32],
    page: u64,
    pool_revision: u64,
    assignment_revision: u64,
    lineage: IntentLineage,
    checksum: [u8; 32],
}
fn invalid_attempt() -> CatalogError {
    CatalogError::Corrupt("rename 原全局尝试或按卷意图记录不符")
}
fn record_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let raw = node_mutation::canonical(value)?;
    if raw.len() > MAX_ATTEMPT_RECORD {
        return Err(invalid_attempt());
    }
    Ok(raw)
}
fn parse_record<T: DeserializeOwned + Serialize>(raw: &[u8]) -> Result<T> {
    if raw.len() > MAX_ATTEMPT_RECORD {
        return Err(invalid_attempt());
    }
    let record: T = serde_json::from_slice(raw).map_err(|_| invalid_attempt())?;
    if record_bytes(&record)? != raw {
        return Err(invalid_attempt());
    }
    Ok(record)
}
fn record_id(raw: &str) -> Result<Uuid> {
    node_mutation::id(raw).map_err(|_| invalid_attempt())
}
fn prefix_volumes(pages: u64, total_pages: u64, total_volumes: u64) -> Result<u64> {
    if pages > total_pages {
        return Err(invalid_attempt());
    }
    if pages == total_pages {
        Ok(total_volumes)
    } else {
        pages
            .checked_mul(MEMBERS_PER_PAGE as u64)
            .ok_or_else(invalid_attempt)
    }
}
impl GroupRecord {
    fn fingerprint(&self) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-global-attempt/1")
            .chain_update(record_bytes(&(
                self.version,
                &self.library,
                &self.namespace,
                &self.pool,
                self.epoch,
                &self.operation,
                &self.id,
                self.owner,
                self.created,
                self.logical,
                self.global,
                self.volumes,
                self.pages,
            ))?)
            .finalize()
            .into())
    }
    fn advance_writing(&self) -> Result<Self> {
        if self.phase != NodeRenameAttemptPhase::Writing {
            return Err(invalid_attempt());
        }
        let mut next = self.clone();
        next.progress_revision = next
            .progress_revision
            .checked_add(1)
            .ok_or_else(invalid_attempt)?;
        next.seal()
    }
    fn initial_chain(&self) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-ready-pages/1")
            .chain_update(self.fingerprint()?)
            .finalize()
            .into())
    }
    fn checksum(&self) -> Result<[u8; 32]> {
        let mut copy = self.clone();
        copy.checksum = [0; 32];
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-group-record/1")
            .chain_update(record_bytes(&copy)?)
            .finalize()
            .into())
    }
    fn validate(&self) -> Result<()> {
        validate_physical_serial(&self.library).map_err(|_| invalid_attempt())?;
        for id in [&self.namespace, &self.pool, &self.operation, &self.id] {
            record_id(id)?;
        }
        if self.version != 1
            || self.epoch == 0
            || !owner_valid(self.owner)
            || !(1..=MAX_MILLIS).contains(&self.created)
            || self.volumes == 0
            || self.pages != self.volumes.div_ceil(MEMBERS_PER_PAGE as u64)
            || self.ready_pages > self.pages
            || self.ready_volumes != prefix_volumes(self.ready_pages, self.pages, self.volumes)?
            || self.closed_pages > self.ready_pages
            || self.ready_pages == 0 && self.ready_chain != self.initial_chain()?
            || self.checksum()? != self.checksum
            || matches!(
                self.phase,
                NodeRenameAttemptPhase::Writing | NodeRenameAttemptPhase::Publishing
            ) != (self.progress_revision > 0)
            || (self.phase == NodeRenameAttemptPhase::Publishing) != self.completion.is_some()
            || self
                .completion
                .as_ref()
                .is_some_and(|value| value.revision != self.progress_revision)
        {
            return Err(invalid_attempt());
        }
        if let Some(completion) = &self.completion
            && (completion.released_pages > self.pages
                || completion.released_pages == 0
                    && completion.released_chain != sealing::initial_chain(self)?
                || completion.released_pages == self.pages
                    && completion.released_chain != completion.chain)
        {
            return Err(invalid_attempt());
        }
        match self.phase {
            NodeRenameAttemptPhase::Preparing
                if self.ready_pages < self.pages
                    && self.closed_pages == 0
                    && self.closed_time.is_none()
                    && self.closed_owner.is_none() => {}
            NodeRenameAttemptPhase::Ready
            | NodeRenameAttemptPhase::Writing
            | NodeRenameAttemptPhase::Publishing
                if self.ready_pages == self.pages
                    && self.closed_pages == 0
                    && self.closed_time.is_none()
                    && self.closed_owner.is_none() => {}
            NodeRenameAttemptPhase::ClosingPrepared
                if self.closed_time.is_none() && self.closed_owner.is_none() => {}
            NodeRenameAttemptPhase::ClosedPrepared
                if self.closed_pages == self.ready_pages
                    && self
                        .closed_owner
                        .is_some_and(|owner| owner >= self.owner && owner_valid(owner))
                    && self
                        .closed_time
                        .is_some_and(|time| time >= self.created && time <= MAX_MILLIS) => {}
            _ => return Err(invalid_attempt()),
        }
        Ok(())
    }
    fn seal(mut self) -> Result<Self> {
        self.checksum = self.checksum()?;
        self.validate()?;
        Ok(self)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        let value: Self = parse_record(raw)?;
        value.validate()?;
        Ok(value)
    }
    fn require_global(&self, manifest: &NodeRenameManifest, global: &GlobalHeader) -> Result<()> {
        self.validate()?;
        if self.library != manifest.task.library_serial
            || self.namespace != manifest.task.namespace.to_string()
            || self.pool != manifest.task.pool.to_string()
            || self.epoch != manifest.task.request_epoch
            || self.operation != manifest.task.operation.to_string()
            || self.logical != manifest.task.fingerprint
            || self.global != global.summary.sha256
            || self.owner < manifest.header.prepared_owner
            || self.created < manifest.header.prepared_time
            || self.volumes != global.summary.volumes
            || self.pages != global.pages
        {
            return Err(invalid_attempt());
        }
        Ok(())
    }
}
impl IntentRecord {
    fn original_reference(&self) -> Result<IntentRef> {
        self.validate()?;
        Ok(match &self.lineage {
            IntentLineage::Original => IntentRef {
                volume: self.volume.clone(),
                id: self.id.clone(),
                fingerprint: self.fingerprint()?,
            },
            IntentLineage::Rebased { root, .. } => root.clone(),
        })
    }
    fn checksum(&self) -> Result<[u8; 32]> {
        let mut copy = self.clone();
        copy.checksum = [0; 32];
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-intent-record/1")
            .chain_update(record_bytes(&copy)?)
            .finalize()
            .into())
    }
    fn validate(&self) -> Result<()> {
        validate_physical_serial(&self.library).map_err(|_| invalid_attempt())?;
        for id in [
            &self.namespace,
            &self.pool,
            &self.operation,
            &self.group,
            &self.volume,
            &self.id,
        ] {
            record_id(id)?;
        }
        if self.version != 1
            || self.epoch == 0
            || !owner_valid(self.owner)
            || !(1..=MAX_MILLIS).contains(&self.created)
            || self.pool_revision == 0
            || self.assignment_revision == 0
            || self.checksum()? != self.checksum
        {
            return Err(invalid_attempt());
        }
        if let IntentLineage::Rebased { root, parent } = &self.lineage {
            record_id(&root.id)?;
            record_id(&root.volume)?;
            record_id(&parent.id)?;
            if root.volume != self.volume || root.id == self.id || parent.id == self.id {
                return Err(invalid_attempt());
            }
        }
        Ok(())
    }
    fn seal(mut self) -> Result<Self> {
        self.checksum = self.checksum()?;
        self.validate()?;
        Ok(self)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        let value: Self = parse_record(raw)?;
        value.validate()?;
        Ok(value)
    }
    fn fingerprint(&self) -> Result<[u8; 32]> {
        self.validate()?;
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-volume-intent/1")
            .chain_update(record_bytes(self)?)
            .finalize()
            .into())
    }
    fn require_group(
        &self,
        group: &GroupRecord,
        member: &NodeRenameParticipant,
        recipe: &NodeRenameVolumePlan,
    ) -> Result<NodeRenameMediaPlan> {
        self.validate()?;
        if self.lineage != IntentLineage::Original
            || self.library != group.library
            || self.namespace != group.namespace
            || self.pool != group.pool
            || self.epoch != group.epoch
            || self.operation != group.operation
            || self.group != group.id
            || self.owner != group.owner
            || self.created != group.created
            || self.logical != group.logical
            || self.global != group.global
            || self.volume != member.volume.to_string()
            || self.page >= group.pages
            || self.recipe != member.recipe
            || self.recipe != recipe.fingerprint()
            || self.logical != recipe.record.facts.fingerprint
            || recipe.source_index().volume_uuid != member.volume
            || self.pool_revision != recipe.record.facts.binding()?.pool_revision
            || self.assignment_revision != recipe.record.facts.binding()?.assignment_revision
        {
            return Err(invalid_attempt());
        }
        let plan = NodeRenameMediaPlan::new(recipe.clone(), record_id(&self.id)?, self.owner)?;
        if plan.fingerprint() != self.physical {
            return Err(invalid_attempt());
        }
        Ok(plan)
    }
}

#[derive(Clone)]
pub struct NodeRenameAttempt {
    global: NodeRenameGlobalPlan,
    record: GroupRecord,
}
impl NodeRenameAttempt {
    pub fn global_plan(&self) -> &NodeRenameGlobalPlan {
        &self.global
    }
    pub fn id(&self) -> Uuid {
        record_id(&self.record.id).expect("validated group UUID")
    }
    pub fn original_owner_epoch(&self) -> u64 {
        self.record.owner
    }
    pub fn phase(&self) -> NodeRenameAttemptPhase {
        self.record.phase
    }
    pub fn progress_revision(&self) -> u64 {
        self.record.progress_revision
    }
    pub fn prepared_pages(&self) -> u64 {
        self.record.ready_pages
    }
    pub fn prepared_volumes(&self) -> u64 {
        self.record.ready_volumes
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.record.fingerprint().expect("validated group")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameVolumeIntent {
    record: IntentRecord,
    plan: NodeRenameMediaPlan,
}
impl NodeRenameVolumeIntent {
    pub fn preparation_page(&self) -> u64 {
        self.record.page
    }
    pub fn id(&self) -> Uuid {
        record_id(&self.record.id).expect("validated intent UUID")
    }
    pub fn group(&self) -> Uuid {
        record_id(&self.record.group).expect("validated group UUID")
    }
    pub fn volume(&self) -> Uuid {
        record_id(&self.record.volume).expect("validated volume UUID")
    }
    pub fn original_owner_epoch(&self) -> u64 {
        self.record.owner
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.record.fingerprint().expect("validated intent")
    }
    /// Data only. Prepared remains closed to media I/O.
    pub fn media_plan(&self) -> &NodeRenameMediaPlan {
        &self.plan
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentRef {
    volume: String,
    id: String,
    fingerprint: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyPage {
    version: u8,
    group: [u8; 32],
    index: u64,
    previous: [u8; 32],
    intents: Vec<IntentRef>,
    checksum: [u8; 32],
}
impl ReadyPage {
    fn checksum(&self) -> Result<[u8; 32]> {
        let mut copy = self.clone();
        copy.checksum = [0; 32];
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-ready-record/1")
            .chain_update(encode_value(&copy)?)
            .finalize()
            .into())
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.intents.is_empty()
            || self.intents.len() > MEMBERS_PER_PAGE
            || self.checksum()? != self.checksum
        {
            return Err(invalid_attempt());
        }
        let mut last = None;
        for intent in &self.intents {
            let volume = record_id(&intent.volume)?;
            record_id(&intent.id)?;
            if last.is_some_and(|id| id >= volume) {
                return Err(invalid_attempt());
            }
            last = Some(volume);
        }
        Ok(())
    }
    fn seal(mut self) -> Result<Self> {
        self.checksum = self.checksum()?;
        self.validate()?;
        Ok(self)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        let value: Self = decode_value(raw)?;
        value.validate()?;
        Ok(value)
    }
    fn chain(&self) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-ready-page/1")
            .chain_update(encode_value(self)?)
            .finalize()
            .into())
    }
    fn require_group(&self, group: &GroupRecord) -> Result<()> {
        self.validate()?;
        let count = prefix_volumes(
            self.index.checked_add(1).ok_or_else(invalid_attempt)?,
            group.pages,
            group.volumes,
        )?
        .checked_sub(prefix_volumes(self.index, group.pages, group.volumes)?)
        .ok_or_else(invalid_attempt)?;
        if self.group != group.fingerprint()?
            || self.index >= group.pages
            || self.intents.len() as u64 != count
            || self.index == 0 && self.previous != group.initial_chain()?
        {
            return Err(invalid_attempt());
        }
        Ok(())
    }
}
fn attempt_prefix(task: &NodeMutationTask, tag: u8) -> Vec<u8> {
    let mut k = prefix(task);
    k.push(tag);
    k
}
fn group_pointer(task: &NodeMutationTask) -> Vec<u8> {
    let mut k = attempt_prefix(task, 5);
    k.push(0);
    k
}
fn groups_prefix(task: &NodeMutationTask) -> Vec<u8> {
    let mut k = attempt_prefix(task, 5);
    k.push(1);
    k
}
fn group_key(task: &NodeMutationTask, id: Uuid) -> Vec<u8> {
    let mut k = groups_prefix(task);
    k.extend_from_slice(id.as_bytes());
    k
}
fn intent_key(task: &NodeMutationTask, group: Uuid, volume: Uuid, id: Uuid) -> Vec<u8> {
    let mut k = attempt_prefix(task, 6);
    k.extend_from_slice(group.as_bytes());
    k.extend_from_slice(volume.as_bytes());
    k.extend_from_slice(id.as_bytes());
    k
}
fn intent_pointer(task: &NodeMutationTask, group: Uuid, volume: Uuid) -> Vec<u8> {
    let mut k = attempt_prefix(task, 7);
    k.extend_from_slice(group.as_bytes());
    k.extend_from_slice(volume.as_bytes());
    k
}
fn ready_key(task: &NodeMutationTask, group: Uuid, index: u64) -> Vec<u8> {
    let mut k = attempt_prefix(task, 8);
    k.extend_from_slice(group.as_bytes());
    k.extend_from_slice(&index.to_be_bytes());
    k
}
fn raw_id(raw: &[u8]) -> Result<Uuid> {
    let id = Uuid::from_slice(raw).map_err(|_| invalid_attempt())?;
    if id.is_nil() {
        return Err(invalid_attempt());
    }
    Ok(id)
}
fn nonce_key(namespace: Uuid, volume: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'I');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}
fn nonce_value(record: &IntentRecord) -> Result<Vec<u8>> {
    let mut value = vec![1];
    value.extend_from_slice(&record.fingerprint()?);
    Ok(value)
}
async fn require_nonce_at(tr: &Transaction, record: &IntentRecord) -> Result<()> {
    let key = nonce_key(
        record_id(&record.namespace)?,
        record_id(&record.volume)?,
        record_id(&record.id)?,
    );
    if tr.get(&key, false).await?.as_deref() != Some(nonce_value(record)?.as_slice()) {
        return Err(invalid_attempt());
    }
    Ok(())
}
fn claim(record: &IntentRecord) -> Result<node_claims::RenameVolumeClaim> {
    Ok(node_claims::RenameVolumeClaim {
        namespace: record_id(&record.namespace)?,
        pool: record_id(&record.pool)?,
        library: record.library.clone(),
        epoch: record.epoch,
        operation: record_id(&record.operation)?,
        global: record_id(&record.group)?,
        volume: record_id(&record.volume)?,
        intent: record_id(&record.id)?,
        page: record.page,
        owner: record.owner,
        fingerprint: record.fingerprint()?,
        closed: None,
    })
}
fn claim_bytes(record: &IntentRecord) -> Result<Vec<u8>> {
    node_claims::encode(&node_claims::NodeVolumeClaim::Rename(Box::new(claim(
        record,
    )?)))
}
async fn original_global_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    expected: &GlobalHeader,
) -> Result<()> {
    let raw = tr
        .get(&global_key(&manifest.task), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    if GlobalHeader::decode(&raw, manifest)? != *expected {
        return Err(invalid_attempt());
    }
    Ok(())
}
async fn current_group_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    global: &GlobalHeader,
) -> Result<Option<GroupRecord>> {
    let Some(raw) = tr.get(&group_pointer(&manifest.task), false).await? else {
        if !read_page(tr, &groups_prefix(&manifest.task), None, 1, false)
            .await?
            .items
            .is_empty()
        {
            return Err(invalid_attempt());
        }
        return Ok(None);
    };
    let id = raw_id(&raw)?;
    let raw = tr
        .get(&group_key(&manifest.task, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let value = GroupRecord::decode(&raw)?;
    value.require_global(manifest, global)?;
    let task = node_tasks::task_at(
        tr,
        &manifest.task.library_serial,
        manifest.task.namespace,
        manifest.task.request_epoch,
        manifest.task.operation,
    )
    .await?
    .ok_or_else(invalid_attempt)?;
    value.require_task(&task)?;
    if record_id(&value.id)? != id {
        return Err(invalid_attempt());
    }
    Ok(Some(value))
}
async fn member_page_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    global: &GlobalHeader,
    index: u64,
) -> Result<ParticipantPage> {
    if index >= global.pages {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(&participant_key(&manifest.task, index), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let page = ParticipantPage::decode(&raw)?;
    if page.facts.index != index
        || page.facts.logical != global.logical
        || page.facts.manifest != global.manifest
        || index + 1 < global.pages && page.facts.members.len() != MEMBERS_PER_PAGE
        || index + 1 == global.pages && page.chain()? != global.summary.sha256
    {
        return Err(invalid_attempt());
    }
    Ok(page)
}
async fn saved_ready_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    group: &GroupRecord,
    index: u64,
) -> Result<ReadyPage> {
    if index >= group.ready_pages {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(&ready_key(task, record_id(&group.id)?, index), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let page = ReadyPage::decode(&raw)?;
    page.require_group(group)?;
    if page.index != index || index + 1 == group.ready_pages && page.chain()? != group.ready_chain {
        return Err(invalid_attempt());
    }
    Ok(page)
}
async fn original_intent_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    global: &GlobalHeader,
    group: &GroupRecord,
    reference: &IntentRef,
    page: u64,
    require_claim: bool,
) -> Result<NodeRenameVolumeIntent> {
    let volume = record_id(&reference.volume)?;
    let id = record_id(&reference.id)?;
    let group_id = record_id(&group.id)?;
    let raw = tr
        .get(&intent_key(&manifest.task, group_id, volume, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let record = IntentRecord::decode(&raw)?;
    require_nonce_at(tr, &record).await?;
    let members = member_page_at(tr, manifest, global, page).await?;
    let member = members
        .facts
        .members
        .iter()
        .find(|member| member.volume == volume)
        .ok_or_else(invalid_attempt)?;
    let raw = tr
        .get(&volume_plan_key(&manifest.task, volume), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let recipe_record = volume_plan::Record::decode(&raw)?;
    require_volume_plan_scope(&recipe_record, manifest, group.owner)?;
    let recipe = NodeRenameVolumePlan {
        record: recipe_record,
    };
    let plan = record.require_group(group, member, &recipe)?;
    if record.page != page
        || record_id(&record.id)? != id
        || record.fingerprint()? != reference.fingerprint
    {
        return Err(invalid_attempt());
    }
    if require_claim {
        let pointer = tr
            .get(&intent_pointer(&manifest.task, group_id, volume), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let held = tr
            .get(&node_attempts::volume_key(volume), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let node_claims::NodeVolumeClaim::Rename(held) = node_claims::decode(&held)? else {
            return Err(invalid_attempt());
        };
        let mut expected = claim(&record)?;
        expected.closed = held.closed;
        if raw_id(&pointer)? != id
            || *held != expected
            || held.closed.is_some() && group.phase != NodeRenameAttemptPhase::Writing
        {
            return Err(invalid_attempt());
        }
        closure::require_claim_at(tr, &held).await?;
    }
    Ok(NodeRenameVolumeIntent { record, plan })
}

impl ManagedCatalog {
    /// Persist a discoverable original preparation group, not an I/O permit.
    pub async fn begin_node_rename_attempt_preparation(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        global: &NodeRenameGlobalPlan,
        id: Uuid,
    ) -> Result<NodeRenameAttempt> {
        if id.is_nil() {
            return Err(invalid("rename 原尝试 UUID 为空"));
        }
        let manifest = &global.manifest;
        self.verify_participant_pages(route, owner, manifest, &global.header)
            .await?;
        let record = self
            .db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        require_original(&tr, route, owner, manifest).await?;
                        original_global_at(&tr, manifest, &global.header).await?;
                        if let Some(saved) = current_group_at(&tr, manifest, &global.header).await?
                        {
                            if record_id(&saved.id)? == id {
                                if saved.phase == NodeRenameAttemptPhase::ClosedPrepared {
                                    return Err(invalid("已关闭 rename 尝试 UUID 不能复用"));
                                }
                                return Ok(saved);
                            }
                            if saved.phase != NodeRenameAttemptPhase::ClosedPrepared {
                                return Err(CatalogError::PoolBusy);
                            }
                        }
                        revalidate_at(&tr, route, owner, &manifest.request).await?;
                        let key = group_key(&manifest.task, id);
                        if tr.get(&key, false).await?.is_some() {
                            return Err(invalid("rename 尝试 UUID 已存在"));
                        }
                        let mut record = GroupRecord {
                            version: 1,
                            library: manifest.task.library_serial.clone(),
                            namespace: manifest.task.namespace.to_string(),
                            pool: manifest.task.pool.to_string(),
                            epoch: manifest.task.request_epoch,
                            operation: manifest.task.operation.to_string(),
                            id: id.to_string(),
                            owner,
                            created: management::now().max(manifest.header.prepared_time),
                            logical: manifest.task.fingerprint,
                            global: global.header.summary.sha256,
                            volumes: global.header.summary.volumes,
                            pages: global.header.pages,
                            phase: NodeRenameAttemptPhase::Preparing,
                            progress_revision: 0,
                            completion: None,
                            ready_pages: 0,
                            ready_volumes: 0,
                            ready_chain: [0; 32],
                            closed_pages: 0,
                            closed_time: None,
                            closed_owner: None,
                            checksum: [0; 32],
                        };
                        record.ready_chain = record.initial_chain()?;
                        let record = record.seal()?;
                        record.require_global(manifest, &global.header)?;
                        let encoded = record_bytes(&record)?;
                        tr.set(&key, &encoded);
                        tr.set(&group_pointer(&manifest.task), id.as_bytes());
                        Ok(record)
                    }
                    .await,
                )
            })
            .await??;
        Ok(NodeRenameAttempt {
            global: global.clone(),
            record,
        })
    }
    pub async fn node_rename_prepared_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        global: &NodeRenameGlobalPlan,
    ) -> Result<Option<NodeRenameAttempt>> {
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let saved = current_group_at(&tr, manifest, &global.header).await?;
        if saved.as_ref().is_some_and(|record| record.owner > owner) {
            return Err(invalid_attempt());
        }
        Ok(saved.map(|record| NodeRenameAttempt {
            global: global.clone(),
            record,
        }))
    }
    /// Each bounded page atomically records intents and the existing shared Q/v
    /// exclusion slots. A lost response retries this exact page, not its successor.
    pub async fn next_node_rename_attempt_preparation_page(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &mut NodeRenameAttempt,
    ) -> Result<Vec<NodeRenameVolumeIntent>> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let index = attempt.record.ready_pages;
        let ids: Vec<Uuid> = (0..MEMBERS_PER_PAGE).map(|_| Uuid::new_v4()).collect();
        let expected_id = attempt.id();
        let (record, intents) = self
            .db
            .run(|tr, _| {
                let ids = ids.clone();
                async move {
                    requests::retry_result(
                        async {
                            require_original(&tr, route, owner, manifest).await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let mut record = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if record_id(&record.id)? != expected_id || record.owner != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if !matches!(
                                record.phase,
                                NodeRenameAttemptPhase::Preparing | NodeRenameAttemptPhase::Ready
                            ) {
                                return Err(invalid("rename 原尝试正在关闭或已经关闭"));
                            }
                            if index < record.ready_pages {
                                let page =
                                    saved_ready_at(&tr, &manifest.task, &record, index).await?;
                                let mut intents = Vec::new();
                                for reference in &page.intents {
                                    intents.push(
                                        original_intent_at(
                                            &tr,
                                            manifest,
                                            &global.header,
                                            &record,
                                            reference,
                                            index,
                                            true,
                                        )
                                        .await?,
                                    );
                                }
                                return Ok((record, intents));
                            }
                            if index != record.ready_pages {
                                return Err(invalid_attempt());
                            }
                            if record.phase == NodeRenameAttemptPhase::Ready {
                                return Ok((record, Vec::new()));
                            }
                            revalidate_at(&tr, route, owner, &manifest.request).await?;
                            let members =
                                member_page_at(&tr, manifest, &global.header, index).await?;
                            let mut intents = Vec::new();
                            let mut writes = Vec::new();
                            let mut references = Vec::new();
                            for (member, id) in members.facts.members.iter().zip(ids) {
                                let volume = member.volume;
                                let raw = tr
                                    .get(&volume_plan_key(&manifest.task, volume), false)
                                    .await?
                                    .ok_or_else(invalid_attempt)?;
                                let recipe_record = volume_plan::Record::decode(&raw)?;
                                require_volume_plan_scope(&recipe_record, manifest, owner)?;
                                let recipe = NodeRenameVolumePlan {
                                    record: recipe_record,
                                };
                                if recipe.fingerprint() != member.recipe {
                                    return Err(invalid_attempt());
                                }
                                let binding = recipe.record.facts.binding()?;
                                if live_guard::binding_at(&tr, &manifest.request, volume).await?
                                    != binding
                                {
                                    return Err(invalid("rename 原按卷来源或策略修订已改变"));
                                }
                                let slot = node_attempts::volume_key(volume);
                                if tr.get(&slot, false).await?.is_some() {
                                    node_attempts::require_no_pending_at(&tr, volume).await?;
                                }
                                let next = binding
                                    .source
                                    .generation
                                    .checked_add(1)
                                    .ok_or_else(invalid_attempt)?;
                                if active_volume::recovery_active(&tr, volume).await?
                                    || !reservation::volume_reservations_empty(&tr, volume, next)
                                        .await?
                                    || tr
                                        .get(&active_batch::batch_key(volume, next), false)
                                        .await?
                                        .is_some()
                                {
                                    return Err(CatalogError::Incomplete);
                                }
                                let pointer = intent_pointer(&manifest.task, expected_id, volume);
                                if tr.get(&pointer, false).await?.is_some() {
                                    return Err(invalid_attempt());
                                }
                                let key = intent_key(&manifest.task, expected_id, volume, id);
                                if tr.get(&key, false).await?.is_some() {
                                    return Err(invalid_attempt());
                                }
                                let nonce = nonce_key(manifest.task.namespace, volume, id);
                                if tr.get(&nonce, false).await?.is_some() {
                                    return Err(invalid("rename 逐卷物理 UUID 已使用"));
                                }
                                let plan = NodeRenameMediaPlan::new(recipe, id, owner)?;
                                let intent = IntentRecord {
                                    version: 1,
                                    library: record.library.clone(),
                                    namespace: record.namespace.clone(),
                                    pool: record.pool.clone(),
                                    epoch: record.epoch,
                                    operation: record.operation.clone(),
                                    group: record.id.clone(),
                                    volume: volume.to_string(),
                                    id: id.to_string(),
                                    owner: record.owner,
                                    created: record.created,
                                    logical: record.logical,
                                    global: record.global,
                                    recipe: member.recipe,
                                    physical: plan.fingerprint(),
                                    page: index,
                                    pool_revision: binding.pool_revision,
                                    assignment_revision: binding.assignment_revision,
                                    lineage: IntentLineage::Original,
                                    checksum: [0; 32],
                                }
                                .seal()?;
                                intent.require_group(&record, member, plan.recipe())?;
                                references.push(IntentRef {
                                    volume: volume.to_string(),
                                    id: id.to_string(),
                                    fingerprint: intent.fingerprint()?,
                                });
                                writes.push((nonce, nonce_value(&intent)?));
                                writes.push((key, record_bytes(&intent)?));
                                writes.push((pointer, id.as_bytes().to_vec()));
                                writes.push((slot, claim_bytes(&intent)?));
                                intents.push(NodeRenameVolumeIntent {
                                    record: intent,
                                    plan,
                                });
                            }
                            let page = ReadyPage {
                                version: 1,
                                group: record.fingerprint()?,
                                index,
                                previous: record.ready_chain,
                                intents: references,
                                checksum: [0; 32],
                            }
                            .seal()?;
                            page.require_group(&record)?;
                            record.ready_pages = record
                                .ready_pages
                                .checked_add(1)
                                .ok_or_else(invalid_attempt)?;
                            record.ready_volumes =
                                prefix_volumes(record.ready_pages, record.pages, record.volumes)?;
                            record.ready_chain = page.chain()?;
                            if record.ready_pages == record.pages {
                                record.phase = NodeRenameAttemptPhase::Ready;
                            }
                            let record = record.seal()?;
                            let header_bytes = record_bytes(&record)?;
                            let page_bytes = encode_value(&page)?;
                            let page_key = ready_key(&manifest.task, expected_id, index);
                            if tr.get(&page_key, false).await?.is_some() {
                                return Err(invalid_attempt());
                            }
                            for (key, bytes) in writes {
                                tr.set(&key, &bytes);
                            }
                            tr.set(&page_key, &page_bytes);
                            tr.set(&group_key(&manifest.task, expected_id), &header_bytes);
                            Ok((record, intents))
                        }
                        .await,
                    )
                }
            })
            .await??;
        attempt.record = record;
        Ok(intents)
    }
    pub async fn node_rename_prepared_volume_intents(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        index: u64,
    ) -> Result<Vec<NodeRenameVolumeIntent>> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let record = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if record_id(&record.id)? != attempt.id() || record.owner > owner {
            return Err(invalid_attempt());
        }
        let page = saved_ready_at(&tr, &manifest.task, &record, index).await?;
        let mut intents = Vec::new();
        let claims_live = matches!(
            record.phase,
            NodeRenameAttemptPhase::Preparing | NodeRenameAttemptPhase::Ready
        ) || record.phase == NodeRenameAttemptPhase::ClosingPrepared
            && index >= record.closed_pages;
        for reference in &page.intents {
            intents.push(
                original_intent_at(
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
}

impl GroupRecord {
    fn require_task(&self, task: &NodeMutationTask) -> Result<()> {
        self.validate()?;
        if task.kind != NodeMutationTaskKind::Rename
            || !matches!(
                (task.state, self.phase),
                (
                    NodeMutationTaskState::Prepared,
                    NodeRenameAttemptPhase::Preparing
                        | NodeRenameAttemptPhase::Ready
                        | NodeRenameAttemptPhase::ClosingPrepared
                        | NodeRenameAttemptPhase::ClosedPrepared
                ) | (
                    NodeMutationTaskState::Writing,
                    NodeRenameAttemptPhase::Writing | NodeRenameAttemptPhase::Publishing
                )
            )
            || task.state == NodeMutationTaskState::Writing
                && task.active_attempt != Some(record_id(&self.id)?)
            || self.library != task.library_serial
            || self.namespace != task.namespace.to_string()
            || self.pool != task.pool.to_string()
            || self.epoch != task.request_epoch
            || self.operation != task.operation.to_string()
            || self.logical != task.fingerprint
            || self.owner < task.prepared_owner_epoch.ok_or_else(invalid_attempt)?
            || self.created < task.prepared_at_millis.ok_or_else(invalid_attempt)?
        {
            return Err(invalid_attempt());
        }
        Ok(())
    }
}
pub(in crate::managed_catalog) async fn require_no_prepared_attempt_at(
    tr: &Transaction,
    task: &NodeMutationTask,
) -> Result<()> {
    if task.kind != NodeMutationTaskKind::Rename {
        return Ok(());
    }
    let Some(raw) = tr.get(&group_pointer(task), false).await? else {
        if !read_page(tr, &groups_prefix(task), None, 1, false)
            .await?
            .items
            .is_empty()
        {
            return Err(invalid_attempt());
        }
        return Ok(());
    };
    let id = raw_id(&raw)?;
    let raw = tr
        .get(&group_key(task, id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let record = GroupRecord::decode(&raw)?;
    record.require_task(task)?;
    if record_id(&record.id)? != id {
        return Err(invalid_attempt());
    }
    if record.phase != NodeRenameAttemptPhase::ClosedPrepared {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
/// Validate the shared slot before treating it as pending. This grants no I/O.
pub(in crate::managed_catalog) async fn require_prepared_volume_claim_at(
    tr: &Transaction,
    placement: &VolumePlacement,
    value: &node_claims::RenameVolumeClaim,
) -> Result<()> {
    if value.namespace != placement.namespace
        || value.pool != placement.pool_id
        || value.library != placement.library_serial
        || value.volume != placement.volume_id
    {
        return Err(invalid_attempt());
    }
    let task = node_tasks::task_at(
        tr,
        &value.library,
        value.namespace,
        value.epoch,
        value.operation,
    )
    .await?
    .ok_or_else(invalid_attempt)?;
    if node_tasks::held_task_at(tr, value.namespace, value.pool)
        .await?
        .as_ref()
        != Some(&task)
    {
        return Err(invalid_attempt());
    }
    let pointer = tr
        .get(&group_pointer(&task), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    if raw_id(&pointer)? != value.global {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(&group_key(&task, value.global), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let group = GroupRecord::decode(&raw)?;
    group.require_task(&task)?;
    if record_id(&group.id)? != value.global
        || group.owner > value.owner
        || group.phase == NodeRenameAttemptPhase::ClosedPrepared
        || value.page >= group.ready_pages
        || group
            .completion
            .as_ref()
            .is_some_and(|c| value.page < c.released_pages)
        || value.closed.is_some() && group.phase != NodeRenameAttemptPhase::Writing
        || group.phase == NodeRenameAttemptPhase::ClosingPrepared && value.page < group.closed_pages
    {
        return Err(invalid_attempt());
    }
    let raw = tr
        .get(
            &intent_key(&task, value.global, value.volume, value.intent),
            false,
        )
        .await?
        .ok_or_else(invalid_attempt)?;
    let intent = IntentRecord::decode(&raw)?;
    require_nonce_at(tr, &intent).await?;
    let mut expected = claim(&intent)?;
    expected.closed = value.closed;
    if expected != *value
        || value.closed.is_some() && task.state != NodeMutationTaskState::Writing
        || intent.library != group.library
        || intent.namespace != group.namespace
        || intent.pool != group.pool
        || intent.epoch != group.epoch
        || intent.operation != group.operation
        || intent.group != group.id
        || intent.logical != group.logical
        || intent.global != group.global
        || intent.created < group.created
    {
        return Err(invalid_attempt());
    }
    let pointer = tr
        .get(&intent_pointer(&task, value.global, value.volume), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    if raw_id(&pointer)? != value.intent {
        return Err(invalid_attempt());
    }
    rebase::require_lineage_claim_at(tr, &task, &group, &intent).await?;
    closure::require_claim_at(tr, value).await?;
    Ok(())
}
impl ManagedCatalog {
    /// Explicitly release only this never-armed group's metadata claims. A
    /// Prepared task has never received permission to alter a medium.
    pub async fn close_prepared_node_rename_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &mut NodeRenameAttempt,
    ) -> Result<()> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let expected_id = attempt.id();
        loop {
            let record = self
                .db
                .run(|tr, _| async move {
                    requests::retry_result(
                        async {
                            require_original(&tr, route, owner, manifest).await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let mut record = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if record_id(&record.id)? != expected_id || record.owner > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if record.phase == NodeRenameAttemptPhase::ClosedPrepared {
                                return Ok(record);
                            }
                            if record.phase == NodeRenameAttemptPhase::Writing {
                                return Err(invalid_attempt());
                            }
                            if record.phase != NodeRenameAttemptPhase::ClosingPrepared {
                                record.phase = NodeRenameAttemptPhase::ClosingPrepared;
                            }
                            let mut clear = Vec::new();
                            if record.closed_pages < record.ready_pages {
                                let index = record.closed_pages;
                                let page =
                                    saved_ready_at(&tr, &manifest.task, &record, index).await?;
                                for reference in &page.intents {
                                    let intent = original_intent_at(
                                        &tr,
                                        manifest,
                                        &global.header,
                                        &record,
                                        reference,
                                        index,
                                        true,
                                    )
                                    .await?;
                                    clear.push(node_attempts::volume_key(intent.volume()));
                                    clear.push(intent_pointer(
                                        &manifest.task,
                                        expected_id,
                                        intent.volume(),
                                    ));
                                }
                                record.closed_pages = record
                                    .closed_pages
                                    .checked_add(1)
                                    .ok_or_else(invalid_attempt)?;
                            }
                            if record.closed_pages == record.ready_pages {
                                record.phase = NodeRenameAttemptPhase::ClosedPrepared;
                                record.closed_time = Some(management::now().max(record.created));
                                record.closed_owner = Some(owner);
                            }
                            let record = record.seal()?;
                            let encoded = record_bytes(&record)?;
                            for key in clear {
                                tr.clear(&key);
                            }
                            tr.set(&group_key(&manifest.task, expected_id), &encoded);
                            Ok(record)
                        }
                        .await,
                    )
                })
                .await??;
            let closed = record.phase == NodeRenameAttemptPhase::ClosedPrepared;
            attempt.record = record;
            if closed {
                return Ok(());
            }
        }
    }
    /// Cancellation uses the associated original data, even when a changed
    /// logical precondition prevents any further preparation. No fresh target.
    pub(in crate::managed_catalog) async fn close_rename_preparation_for_task(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        task: &NodeMutationTask,
    ) -> Result<()> {
        if task.kind != NodeMutationTaskKind::Rename
            || task.state != NodeMutationTaskState::Prepared
        {
            return Ok(());
        }
        let tr = self.db.create_trx()?;
        if routing::require_runtime_active_owner(&tr, &task.library_serial, route, owner).await?
            != task.namespace
        {
            return Err(invalid_attempt());
        }
        if node_tasks::held_task_at(&tr, task.namespace, task.pool)
            .await?
            .as_ref()
            != Some(task)
        {
            return Err(invalid_attempt());
        }
        if tr.get(&group_pointer(task), false).await?.is_none() {
            return require_no_prepared_attempt_at(&tr, task).await;
        }
        let raw = tr
            .get(&header_key(task), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        let header: Header = decode(&raw)?;
        validate_header(&header, task)?;
        if !header.summary.complete {
            return Err(invalid_attempt());
        }
        let raw = tr
            .get(&global_key(task), false)
            .await?
            .ok_or_else(invalid_attempt)?;
        drop(tr);
        let request = self
            .load_node_rename_request(route, owner, &task.object, PAGE_BYTES)
            .await?;
        if request.fingerprint() != task.fingerprint {
            return Err(invalid_attempt());
        }
        let manifest = NodeRenameManifest {
            task: task.clone(),
            request,
            header,
        };
        let header = GlobalHeader::decode(&raw, &manifest)?;
        let global = NodeRenameGlobalPlan {
            manifest: Arc::new(manifest),
            header,
        };
        let mut attempt = self
            .node_rename_prepared_attempt(route, owner, &global)
            .await?
            .ok_or_else(invalid_attempt)?;
        self.close_prepared_node_rename_attempt(route, owner, &mut attempt)
            .await
    }
}

#[cfg(test)]
#[path = "attempts_tests.rs"]
mod tests;

/// Called only when Q/v is absent. A prepared group's retained ready pages
/// still prove which claims must exist, even if its reverse pointer was lost.
pub(in crate::managed_catalog) async fn require_no_missing_prepared_claim_at(
    tr: &Transaction,
    placement: &VolumePlacement,
    task: &NodeMutationTask,
) -> Result<()> {
    if task.kind != NodeMutationTaskKind::Rename
        || !matches!(
            task.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        )
    {
        return Ok(());
    }
    let Some(raw) = tr.get(&group_pointer(task), false).await? else {
        if !read_page(tr, &groups_prefix(task), None, 1, false)
            .await?
            .items
            .is_empty()
        {
            return Err(invalid_attempt());
        }
        return Ok(());
    };
    let group_id = raw_id(&raw)?;
    let raw = tr
        .get(&group_key(task, group_id), false)
        .await?
        .ok_or_else(invalid_attempt)?;
    let group = GroupRecord::decode(&raw)?;
    group.require_task(task)?;
    if record_id(&group.id)? != group_id {
        return Err(invalid_attempt());
    }
    if group.phase == NodeRenameAttemptPhase::ClosedPrepared {
        return Ok(());
    }
    let volume = placement.volume_id;
    let pointer_present = tr
        .get(&intent_pointer(task, group_id, volume), false)
        .await?
        .is_some();
    if pointer_present && group.phase != NodeRenameAttemptPhase::Publishing {
        return Err(invalid_attempt());
    }
    let (mut low, mut high) = (group.closed_pages, group.ready_pages);
    while low < high {
        let index = low + (high - low) / 2;
        let page = saved_ready_at(tr, task, &group, index).await?;
        let first = record_id(&page.intents.first().ok_or_else(invalid_attempt)?.volume)?;
        let last = record_id(&page.intents.last().ok_or_else(invalid_attempt)?.volume)?;
        if volume < first {
            high = index;
        } else if volume > last {
            low = index + 1;
        } else {
            if page
                .intents
                .iter()
                .any(|intent| record_id(&intent.volume).is_ok_and(|id| id == volume))
            {
                if pointer_present
                    && group
                        .completion
                        .as_ref()
                        .is_some_and(|c| index < c.released_pages)
                {
                    return Err(CatalogError::PoolBusy);
                }
                return Err(invalid_attempt());
            }
            break;
        }
    }
    if pointer_present {
        return Err(invalid_attempt());
    }
    Ok(())
}

#[path = "writing.rs"]
mod writing;
pub(in crate::managed_catalog) use writing::require_writing_task_at;

#[path = "observation.rs"]
mod observation;
pub use observation::NodeRenameObservation;
pub(in crate::managed_catalog) use observation::require_rename_observation_at;

#[path = "closure.rs"]
mod closure;
pub use closure::NodeRenameIntentClosure;
pub(in crate::managed_catalog) use closure::require_rename_discard_at;

#[path = "rebase.rs"]
mod rebase;

#[path = "sealing.rs"]
mod sealing;

#[path = "publication.rs"]
mod publication;
#[path = "release.rs"]
mod release;
pub use publication::NodeRenamePublication;
pub(in crate::managed_catalog) use publication::require_task_outcome_at as require_rename_task_outcome_at;

#[path = "discovery.rs"]
mod discovery;
