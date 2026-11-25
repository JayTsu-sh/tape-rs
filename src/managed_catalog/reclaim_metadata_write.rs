//! Durable metadata intent, without device execution or erase authority.
//! Once admitted, unknown media results retain the original plan and claims.
use super::*;
#[cfg(test)]
#[path = "reclaim_metadata_isolation_test_hook.rs"]
mod isolation_test_hook;
pub(super) use publication::require_directory_history_link_at;
use metadata_plan::Plan;
#[path = "reclaim_metadata_checkpoint.rs"]
mod checkpoint;
#[path = "reclaim_metadata_context.rs"]
mod context;
#[path = "reclaim_metadata_discard_intent.rs"]
mod discard_intent;
#[path = "reclaim_metadata_donor.rs"]
mod donor;
pub use checkpoint::ReclaimMetadataCheckpointEvidence;
pub use donor::{ReclaimMetadataDonorContext, ReclaimMetadataDonorEvidence};
#[path = "reclaim_metadata_resolution.rs"]
mod resolution;
pub use resolution::{
    ReclaimMetadataRecoveryEvidence, ReclaimMetadataRecoveryKind, ReclaimMetadataResolution,
};
pub(in crate::managed_catalog) use resolution::{
    require_committed_history_at as require_resolution_commit_at,
    require_history_at as require_resolution_at,
};
#[path = "reclaim_metadata_publication.rs"]
mod publication;
#[path = "reclaim_metadata_registry.rs"]
mod registry;
#[path = "reclaim_metadata_replan.rs"]
mod replan;
#[path = "reclaim_metadata_source_replan.rs"]
mod source_replan;
pub use source_replan::{ReclaimMetadataSourceReplanContext, ReclaimMetadataSourceReplanEvidence};
#[path = "reclaim_metadata_retry.rs"]
mod retry;
#[path = "reclaim_metadata_unwritten.rs"]
mod unwritten;
pub use publication::ReclaimMetadataPublication;
pub(in crate::managed_catalog) use publication::require_history_at as require_publication_at;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Writing,
    Discarding,
    Checkpointing,
    Resolved,
    Closed,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    id: String,
    first: String,
    plan_origin: String,
    previous: Option<String>,
    scan: String,
    plan: Plan,
    context: context::Context,
    owner: u64,
    phase: Phase,
    recovery_epoch: u64,
    observation: Option<Vec<u8>>,
}

/// A retained fixed plan, never a device I/O permit or migration receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataWrite(Record);

impl ReclaimMetadataWrite {
    pub fn id(&self) -> Uuid {
        Uuid::parse_str(&self.0.id).expect("validated metadata attempt")
    }
    pub fn first_attempt(&self) -> Uuid {
        Uuid::parse_str(&self.0.first).expect("validated first attempt")
    }
    pub fn plan_origin(&self) -> Uuid {
        Uuid::parse_str(&self.0.plan_origin).expect("validated plan origin")
    }
    pub fn previous_attempt(&self) -> Option<Uuid> {
        self.0
            .previous
            .as_deref()
            .map(|id| Uuid::parse_str(id).expect("validated previous attempt"))
    }
    pub(crate) fn scan_id(&self) -> Uuid {
        Uuid::parse_str(&self.0.scan).expect("validated metadata scan")
    }
    pub fn plan(&self) -> ReclaimMetadataPlan {
        ReclaimMetadataPlan(self.0.plan.clone())
    }
    pub(crate) fn replan_source(
        &self,
        reservation: &ReclaimReservation,
        inventory: &ReclaimSourceInventory<'_>,
        target: &CapturedIndexSource,
        label: &crate::ltfs::label::LtfsLabel,
    ) -> Result<ReclaimMetadataPlan> {
        self.0.context.require_plan(&self.plan())?;
        match &self.0.context {
            context::Context::Raw { raw } => {
                inventory.plan_raw_body_at(reservation, raw, target, label, self.plan().path())
            }
            context::Context::Node { .. } => inventory.plan_reclaimed_node(
                reservation,
                &ReclaimSourceNode(self.plan().0.source),
                target,
                self.plan().path(),
            ),
            _ => Err(CatalogError::Invalid(
                "正文 donor 计划需要实际 donor 重新规划",
            )),
        }
    }
    pub fn recovery_epoch(&self) -> u64 {
        self.0.recovery_epoch
    }
    pub fn owner(&self) -> u64 {
        self.0.owner
    }
    pub fn writing(&self) -> bool {
        self.0.phase == Phase::Writing
    }
    pub fn discarding(&self) -> bool {
        self.0.phase == Phase::Discarding
    }
    pub fn checkpointing(&self) -> bool {
        self.0.phase == Phase::Checkpointing
    }
    pub(crate) fn recovering(&self) -> bool {
        self.discarding() || self.checkpointing()
    }
    pub fn resolved(&self) -> bool {
        self.0.phase == Phase::Resolved
    }
    pub fn closed(&self) -> bool {
        self.0.phase == Phase::Closed
    }
    pub fn completed(&self) -> bool {
        self.0.phase == Phase::Committed
    }
    fn holds_claim(&self) -> bool {
        matches!(
            self.0.phase,
            Phase::Prepared | Phase::Writing | Phase::Discarding | Phase::Checkpointing
        )
    }
    /// Retained snapshot evidence, never permission to replay or release claims.
    pub fn observation(&self) -> Result<Option<ReclaimMetadataObservation>> {
        self.0
            .observation
            .as_deref()
            .map(|raw| ReclaimMetadataObservation::decode(&self.plan(), raw))
            .transpose()
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.plan().encode()?;
        self.0.context.validate()?;
        self.0.context.require_plan(&self.plan())?;
        node_mutation::id(&self.0.id)?;
        node_mutation::id(&self.0.scan)?;
        let first = node_mutation::id(&self.0.first)?;
        let origin = node_mutation::id(&self.0.plan_origin)?;
        if self.0.previous.is_none() && origin != self.id() {
            return Err(CatalogError::Corrupt("首次元数据尝试的计划来源不同"));
        }
        if self.0.previous.is_none() != (first == self.id()) {
            return Err(CatalogError::Corrupt("元数据首次尝试与前身身份不一致"));
        }
        if let Some(previous) = &self.0.previous
            && node_mutation::id(previous)? == self.id()
        {
            return Err(CatalogError::Corrupt("元数据尝试不能以前身指向自身"));
        }
        if let Some(observed) = self.observation()?
            && !(self.writing()
                // A DP commit can be observed before IP repair is admitted.
                // Checkpointing retains that exact fact without publishing it.
                || self.checkpointing()
                || self.completed()
                || self.resolved()
                || (self.closed() || self.recovering())
                    && observed.kind() == ReclaimMetadataObservationKind::Unchanged)
        {
            return Err(CatalogError::Corrupt("未开写任务不能保留介质结果观察"));
        }
        if self.completed()
            && self
                .observation()?
                .is_none_or(|value| value.kind() != ReclaimMetadataObservationKind::Committed)
        {
            return Err(CatalogError::Corrupt("已发布元数据任务缺少原提交观察"));
        }
        if self.0.version != 7
            || (self.recovering() || self.resolved()) != (self.0.recovery_epoch > 0)
            || self.0.owner == 0
            || self.0.owner > 0xFFFF_FFFF_FFFF
        {
            return Err(CatalogError::Corrupt("回收元数据任务版本或 owner 无效"));
        }
        let raw = serde_json::to_vec(&self.0).map_err(cache)?;
        if raw.len() > 60_000 {
            return Err(CatalogError::Invalid("回收元数据任务超出预算"));
        }
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() > 60_000 {
            return Err(CatalogError::Corrupt("回收元数据任务过大"));
        }
        let value = Self(serde_json::from_slice(raw).map_err(cache)?);
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("回收元数据任务编码不规范"));
        }
        Ok(value)
    }
}

/// A committed state transition, never permission to issue a CDB.
/// A replay or ambiguous commit requires observing the original media result.
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataAdmission {
    task: ReclaimMetadataWrite,
    needs_observation: bool,
}
impl ReclaimMetadataAdmission {
    pub fn task(&self) -> &ReclaimMetadataWrite {
        &self.task
    }
    pub fn needs_observation(&self) -> bool {
        self.needs_observation
    }
}

fn require_phase_history(
    original: &ReclaimMetadataWrite,
    current: &ReclaimMetadataWrite,
) -> Result<()> {
    // Prepared branches into unwritten Closed or Writing→Committed.
    // A retained reply must never be reinterpreted as the other branch.
    if original.id() != current.id()
        || original.0.scan != current.0.scan
        || original.0.previous != current.0.previous
        || original.0.first != current.0.first
        || original.0.plan_origin != current.0.plan_origin
        || original.0.context != current.0.context
        || original.0.recovery_epoch > current.0.recovery_epoch
    {
        return Err(CatalogError::Corrupt(
            "回收元数据任务改变原物理尝试或扫描身份",
        ));
    }
    if original.0.phase != Phase::Prepared
        && original.0.phase != current.0.phase
        && !(original.writing()
            && (current.completed()
                || current.closed()
                || current.recovering()
                || current.resolved()))
        && !(original.recovering()
            && (current.resolved()
                || current.recovering() && original.recovery_epoch() < current.recovery_epoch()))
    {
        return Err(CatalogError::Corrupt("回收元数据阶段与原回执冲突"));
    }
    if let Some(before) = original.observation()? {
        let after = current
            .observation()?
            .ok_or(CatalogError::Corrupt("回收元数据任务丢失已持久化的原观察"))?;
        if before != after && before.kind() == ReclaimMetadataObservationKind::Committed {
            return Err(CatalogError::Corrupt("回收元数据任务改变已提交观察"));
        }
    }
    Ok(())
}

fn latest_key(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'T');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn task_prefix(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'W');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn attempt_key(reservation: &ReclaimReservation, uid: u64, id: Uuid) -> Vec<u8> {
    let mut key = task_prefix(reservation, uid);
    key.extend_from_slice(id.as_bytes());
    key
}
fn task_key(reservation: &ReclaimReservation, task: &ReclaimMetadataWrite) -> Vec<u8> {
    attempt_key(reservation, task.0.plan.source.uid, task.id())
}

fn claim(reservation: &ReclaimReservation, task: &ReclaimMetadataWrite) -> Result<Vec<u8>> {
    let plan = task.plan();
    node_claims::encode(&node_claims::NodeVolumeClaim::ReclaimMetadata(Box::new(
        node_claims::ReclaimMetadataVolumeClaim {
            namespace: reservation.namespace().to_string(),
            volume: plan.0.volume.clone(),
            attempt: task.0.id.clone(),
            task: task_key(reservation, task),
            fingerprint: plan.fingerprint()?,
        },
    )))
}

pub(in super::super) async fn require_idle_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    if registry::read_at(tr, reservation)
        .await?
        .is_some_and(|task| task.holds_claim())
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

async fn parent_at(tr: &Transaction, reservation: &ReclaimReservation, owner: u64) -> Result<()> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["phase"] == "reformatting" {
        return Err(CatalogError::Invalid(
            "源卷已进入原持久格式化阶段，不能再写迁移元数据",
        ));
    }
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}

async fn read_body_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
    id: Uuid,
) -> Result<Option<ReclaimMetadataWrite>> {
    if uid == 0 || id.is_nil() {
        return Err(CatalogError::Invalid("回收元数据 UID 或尝试身份为空"));
    }
    let Some(raw) = tr.get(&attempt_key(reservation, uid, id), false).await? else {
        return Ok(None);
    };
    let task = ReclaimMetadataWrite::decode(&raw)?;
    let plan = task.plan();
    if plan.0.parent != reservation.fingerprint() || plan.0.source.uid != uid || task.id() != id {
        return Err(CatalogError::Corrupt("回收元数据任务与原父或尝试身份不符"));
    }
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    if plan.0.inventory != manifest.summary.digest
        || plan.0.source_index != manifest.summary.head
        || reservation.intent["index"] != json!(plan.0.source_index)
        || tr
            .get(&ledger::object_key(reservation, uid), false)
            .await?
            .as_deref()
            != Some(plan.0.source.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("回收元数据任务与原始封存清单不同"));
    }
    Ok(Some(task))
}

async fn raw_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
    id: Uuid,
) -> Result<Option<ReclaimMetadataWrite>> {
    let value = read_body_at(tr, reservation, uid, id).await?;
    if let Some(task) = &value {
        registry::require_previous_at(tr, reservation, task).await?;
    }
    Ok(value)
}

async fn require_claims_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<()> {
    let plan = task.plan();
    let expected = claim(reservation, task)?;
    let active = tr.get(&ledger::own_key(reservation, b'A'), false).await?;
    let held = tr
        .get(&node_attempts::volume_key(plan.volume()), false)
        .await?;
    if (task.holds_claim()
        && (active.as_deref() != Some(expected.as_slice())
            || held.as_deref() != Some(expected.as_slice())))
        || (!task.holds_claim()
            && (active.as_deref() == Some(expected.as_slice())
                || held.as_deref() == Some(expected.as_slice())))
    {
        return Err(CatalogError::Corrupt("回收元数据任务与卷占用不一致"));
    }
    if task.holds_claim()
        && require_volume_ready(tr, plan.volume(), None)
            .await?
            .id
            .to_string()
            != task.0.scan
    {
        return Err(CatalogError::Corrupt("回收元数据原目标扫描身份改变"));
    }
    Ok(())
}

async fn load_id_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
    id: Uuid,
) -> Result<Option<ReclaimMetadataWrite>> {
    let Some(task) = raw_at(tr, reservation, uid, id).await? else {
        return Ok(None);
    };
    require_claims_at(tr, reservation, &task).await?;
    if task.holds_claim() {
        registry::require_current_at(tr, reservation, &task).await?;
    }
    if task.completed() {
        Box::pin(publication::require_task_at(tr, reservation, &task)).await?;
    }
    Box::pin(resolution::require_state_at(tr, reservation, &task)).await?;
    Ok(Some(task))
}

async fn load_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
) -> Result<Option<ReclaimMetadataWrite>> {
    if uid == 0 {
        return Err(CatalogError::Invalid("回收元数据源 UID 不能为零"));
    }
    registry::read_at(tr, reservation).await?;
    let Some(raw) = tr.get(&latest_key(reservation, uid), false).await? else {
        if requests::prefix_has_rows(tr, &task_prefix(reservation, uid)).await? {
            return Err(CatalogError::Corrupt("回收元数据任务缺少原身份索引"));
        }
        return Ok(None);
    };
    let id = uuid_from(&raw)?;
    if id.is_nil() {
        return Err(CatalogError::Corrupt("元数据最新尝试身份为空"));
    }
    load_id_at(tr, reservation, uid, id)
        .await?
        .map(Some)
        .ok_or(CatalogError::Corrupt("元数据最新尝试缺少原记录"))
}

async fn input_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
    plan: &ReclaimMetadataPlan,
) -> Result<ReclaimMetadataInput> {
    let input = Box::pin(relocation::metadata_input_at(
        tr,
        reservation,
        owner,
        file,
        request,
    ))
    .await?;
    if input.fingerprint()? != plan.0.input
        || input.source().byte_proof().object() != &plan.0.source
    {
        return Err(CatalogError::Invalid("回收元数据计划改变原正文或目标证明"));
    }
    Ok(input)
}

// Coverage accepts only the original raw node's committed outcome. A Discard,
// Checkpoint or merely successful body upload cannot cover its attributes.
pub(super) async fn coverage_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    uid: u64,
    id: Uuid,
) -> Result<Object> {
    let (plan, _, _) = destination_at(tr, reservation, owner, uid, id).await?;
    if plan.0.source.kind != ReclaimSourceNodeKind::File && !plan.is_raw_body() {
        return Err(CatalogError::Invalid("原始文件覆盖要求普通文件任务"));
    }
    Ok(plan.0.source)
}

pub(super) async fn destination_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    uid: u64,
    id: Uuid,
) -> Result<(ReclaimMetadataPlan, Uuid, ActiveIndexIdentity)> {
    parent_at(tr, reservation, owner).await?;
    let task = load_id_at(tr, reservation, uid, id)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if task.owner() > owner {
        return Err(CatalogError::OwnerFenced);
    }
    context::input_for_task_at(tr, reservation, owner, &task).await?;
    let index = if task.completed() {
        publication::require_task_at(tr, reservation, &task)
            .await?
            .observation()
            .current()
    } else if task.resolved() {
        let result = resolution::require_task_at(tr, reservation, &task).await?;
        if result.kind() != ReclaimMetadataRecoveryKind::Committed {
            return Err(CatalogError::Incomplete);
        }
        result.current()
    } else {
        return Err(CatalogError::Incomplete);
    };
    let plan = task.plan();
    let placement = verified_placement(tr, plan.volume())
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let assignment =
        ManagedCatalog::verified_assignment(tr, reservation.serial(), &placement.barcode)
            .await?
            .ok_or(CatalogError::Incomplete)?;
    let (scan, head) = active_volume::observed_at(tr, reservation.serial(), plan.volume()).await?;
    if placement.namespace != reservation.namespace()
        || placement.pool_id != reservation.spec().pool
        || placement.library_serial != reservation.serial()
        || assignment.volume_id != Some(plan.volume())
        || !assignment.allows_io()
        || scan.id != task.scan_id()
        || head.generation < index.generation
        || head.generation == index.generation && head != index
    {
        return Err(CatalogError::Invalid("原始文件覆盖的目的卷或索引链改变"));
    }
    Ok((plan, task.scan_id(), index))
}

async fn target_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    plan: &ReclaimMetadataPlan,
    input: &ReclaimMetadataInput,
) -> Result<Uuid> {
    target_policy_at(tr, reservation, plan, input, true).await
}

// A cleanup Discard adds no file; it must still pass the same current media,
// original scan/head/count/input and pool checks without reserving another slot.
async fn target_policy_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    plan: &ReclaimMetadataPlan,
    input: &ReclaimMetadataInput,
    new_file: bool,
) -> Result<Uuid> {
    target_context_policy_at(tr, reservation, plan, Some(input), new_file).await
}

async fn target_context_policy_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    plan: &ReclaimMetadataPlan,
    input: Option<&ReclaimMetadataInput>,
    new_file: bool,
) -> Result<Uuid> {
    require_parent_pool_at(tr, reservation).await?;
    let (scan, index) = active_volume::observed_at(tr, reservation.serial(), plan.volume()).await?;
    let mut encoded = Vec::new();
    active_volume::encode_index(&mut encoded, index);
    let count = active_volume::file_count_at(tr, reservation.serial(), plan.volume()).await?;
    if encoded != plan.0.previous || count != plan.0.physical_files {
        return Err(CatalogError::Invalid(
            "回收元数据准入时目标卷头或物理文件计数改变",
        ));
    }
    if let Some(input) = input {
        target_placement_policy_at(tr, reservation, input, count, new_file).await?;
    } else {
        let placement = verified_placement(tr, plan.volume())
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if !plan.is_source_based()
            || placement.namespace != reservation.namespace()
            || placement.pool_id != reservation.spec().pool
            || placement.library_serial != reservation.serial()
            || plan.volume() == reservation.spec().volume()
        {
            return Err(CatalogError::Invalid("原始节点目的卷改变原池或带库"));
        }
        target_barcode_policy_at(
            tr,
            reservation,
            &placement.barcode,
            Some(plan.volume()),
            count,
            new_file,
        )
        .await?;
    }
    Ok(scan.id)
}

async fn target_placement_policy_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    input: &ReclaimMetadataInput,
    count: u64,
    new_file: bool,
) -> Result<()> {
    target_barcode_policy_at(
        tr,
        reservation,
        &input.destination().placement().barcode,
        Some(input.destination().copy().volume_id),
        count,
        new_file,
    )
    .await
}
async fn target_barcode_policy_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    barcode: &str,
    volume: Option<Uuid>,
    count: u64,
    new_file: bool,
) -> Result<()> {
    let pool_raw = tr
        .get(
            &pool_key(reservation.namespace(), reservation.spec().pool),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let pool = decode_pool(reservation.spec().pool, &pool_raw)?;
    if new_file && count.checked_add(1).is_none_or(|n| n > pool.file_limit) {
        return Err(CatalogError::Incomplete);
    }
    let assignment = ManagedCatalog::verified_assignment(tr, reservation.serial(), barcode)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if !assignment.allows_io() || assignment.volume_id != volume {
        return Err(CatalogError::MediaUnavailable);
    }
    tape_monitor::require_data_writable(tr, reservation.namespace(), reservation.serial(), barcode)
        .await?;
    Ok(())
}

pub(in crate::managed_catalog::control::reclaim) async fn isolation_checkpoint_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<Option<ReclaimMetadataWrite>> {
    let task = registry::read_at(tr, reservation).await?;
    if task.as_ref().is_some_and(|task| task.owner() > owner) {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(task)
}

impl ManagedCatalog {
    /// Fix the new raw node and one physical-file slot atomically, with no CDB.
    /// A duplicate returns the retained original phase; it never retries I/O.
    pub async fn prepare_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        plan: &ReclaimMetadataPlan,
    ) -> Result<ReclaimMetadataWrite> {
        request.validate()?;
        let context = context::Context::new(reservation, file, request)?;
        self.prepare_reclaim_metadata_context(reservation, owner, &context, plan)
            .await
    }

    pub async fn prepare_reclaim_metadata_node(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimMetadataPlan,
    ) -> Result<ReclaimMetadataWrite> {
        if !plan.is_node_metadata() {
            return Err(CatalogError::Invalid("原始节点准备要求目录或原生链接计划"));
        }
        self.prepare_reclaim_metadata_context(reservation, owner, &context::Context::node(), plan)
            .await
    }

    /// Persist actual source-byte facts with the exact raw append recipe.
    pub async fn prepare_reclaim_raw_body(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimMetadataPlan,
        proof: &ReclaimRawSourceRead,
    ) -> Result<ReclaimMetadataWrite> {
        if proof.observed_owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let context = context::Context::Raw {
            raw: Box::new(proof.fact.clone()),
        };
        context.require_plan(plan)?;
        self.prepare_reclaim_metadata_context(reservation, owner, &context, plan)
            .await
    }

    async fn prepare_reclaim_metadata_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &context::Context,
        plan: &ReclaimMetadataPlan,
    ) -> Result<ReclaimMetadataWrite> {
        plan.encode()?;
        context.require_plan(plan)?;
        if plan.0.parent != reservation.fingerprint() {
            return Err(CatalogError::Invalid("回收元数据计划不属于原父任务"));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            group_only::require_ready_at(&tr,reservation,owner,None).await?;
                            isolation::require_metadata_successor_at(&tr, reservation, owner, plan)
                                .await?;
                            registry::read_at(&tr, reservation).await?;
                            let input =
                                context::input_for_plan_at(&tr, reservation, owner, context, plan)
                                    .await?;
                            if let Some(saved) =
                                load_at(&tr, reservation, plan.0.source.uid).await?
                            {
                                let saved = load_id_at(
                                    &tr,
                                    reservation,
                                    plan.source_uid(),
                                    saved.first_attempt(),
                                )
                                .await?
                                .ok_or(CatalogError::Corrupt("元数据准备缺少原首次尝试"))?;
                                if saved.0.context != *context || saved.plan() != *plan {
                                    return Err(CatalogError::Invalid(
                                        "回收元数据重试改变固定计划",
                                    ));
                                }
                                if saved.owner() > owner
                                    || saved.holds_claim() && saved.owner() != owner
                                {
                                    return Err(CatalogError::OwnerFenced);
                                }
                                return Ok(saved);
                            }
                            require_idle_at(&tr, reservation).await?;
                            let scan = target_context_policy_at(
                                &tr,
                                reservation,
                                plan,
                                input.as_ref(),
                                plan.added_files() > 0,
                            )
                            .await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                            )
                            .await?;
                            let id = Uuid::new_v4().to_string();
                            let task = ReclaimMetadataWrite(Record {
                                version: 7,
                                id: id.clone(),
                                first: id.clone(),
                                plan_origin: id,
                                previous: None,
                                scan: scan.to_string(),
                                plan: plan.0.clone(),
                                context: context.clone(),
                                owner,
                                phase: Phase::Prepared,
                                recovery_epoch: 0,
                                observation: None,
                            });
                            let raw = task.encode()?;
                            let key = task_key(reservation, &task);
                            let held = claim(reservation, &task)?;
                            let header = registry::encode(reservation, &task)?;
                            let parent = if context.is_source_based() {
                                let mut value = record_at(&tr, reservation, owner).await?;
                                value["phase"] = json!("copying");
                                Some(value.to_string())
                            } else {
                                None
                            };
                            // All fallible validation/encoding precedes the first mutation.
                            if let Some(parent) = parent {
                                tr.set(&reservation.journal_key(), parent.as_bytes());
                            }
                            tr.set(&key, &raw);
                            tr.set(
                                &latest_key(reservation, plan.0.source.uid),
                                task.id().as_bytes(),
                            );
                            tr.set(&registry::key(reservation), &header);
                            tr.set(&ledger::own_key(reservation, b'A'), &held);
                            tr.set(&node_attempts::volume_key(plan.volume()), &held);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Atomically mark the original plan as potentially written before I/O.
    /// Only a fresh transition can proceed to the guarded device actor. Replays,
    /// including a commit whose reply was lost, must observe instead of replaying.
    /// Device execution/recovery is not implemented by this catalog kernel.
    pub async fn admit_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,

        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataAdmission> {
        original.encode()?;
        if original.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let admitted = self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            group_only::require_ready_at(&tr,reservation,owner,None).await?;
                            let plan = original.plan();
                            isolation::require_metadata_successor_at(&tr, reservation, owner, &plan)
                                .await?;
                            let mut task =
                                load_id_at(&tr, reservation, plan.0.source.uid, original.id())
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != plan {
                                return Err(CatalogError::Invalid("准入改变原回收元数据计划"));
                            }
                            require_phase_history(original, &task)?;

                            let needs_observation = match task.0.phase {
                                Phase::Closed
                                | Phase::Committed
                                | Phase::Discarding
                                | Phase::Checkpointing
                                | Phase::Resolved => {
                                    return Err(CatalogError::Invalid(
                                        "已关闭、已发布或已转入修尾的元数据任务不能重新准入",
                                    ));
                                }
                                Phase::Writing if reservation.spec().group_only() => {
                                    return Err(CatalogError::Invalid("BlockedUnknown：原group-only MetadataTask已Writing，禁止重放观察"));
                                }
                                Phase::Writing => true,
                                Phase::Prepared => false,
                            };
                            // Replays keep the same child/retirement identity;
                            // changed arguments cannot become a new plan.
                            let input =
                                context::input_for_task_at(&tr, reservation, owner, &task).await?;
                            if needs_observation {
                                return Ok(ReclaimMetadataAdmission {
                                    task,
                                    needs_observation: true,
                                });
                            }
                            if target_context_policy_at(
                                &tr,
                                reservation,
                                &plan,
                                input.as_ref(),
                                plan.added_files() > 0,
                            )
                            .await?
                            .to_string()
                                != task.0.scan
                            {
                                return Err(CatalogError::Corrupt("准入改变原元数据目标扫描"));
                            }
                            task.0.phase = Phase::Writing;
                            let raw = task.encode()?;
                            let key = task_key(reservation, &task);
                            let header = registry::encode(reservation, &task)?;
                            tr.set(&key, &raw);
                            tr.set(&registry::key(reservation), &header);
                            Ok(ReclaimMetadataAdmission {
                                task,
                                needs_observation: false,
                            })
                        }
                        .await,
                    )
                })
            })
            .await??;
        #[cfg(test)]
        isolation_test_hook::after_committed(reservation)?;
        Ok(admitted)
    }

    /// Read-only, same-version recheck for a fresh device execution. Losing an
    /// input, claim, writable policy or original head prevents the next CDB.
    /// Re-observing an uncertain result uses the original Writing status only.
    pub(crate) async fn verify_reclaim_metadata_execution(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,

        original: &ReclaimMetadataWrite,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let plan = original.plan();
        isolation::require_metadata_successor_at(&tr, reservation, owner, &plan).await?;
        group_only::require_seal_at(&tr,reservation).await?;
        let task = load_id_at(&tr, reservation, plan.source_uid(), original.id())
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if task.owner() != owner || original.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        if task != *original || !task.writing() {
            return Err(CatalogError::Invalid("设备执行改变原 Writing 任务或观察"));
        }
        let input = context::input_for_task_at(&tr, reservation, owner, &task).await?;
        if target_context_policy_at(
            &tr,
            reservation,
            &plan,
            input.as_ref(),
            plan.added_files() > 0,
        )
        .await?
        .to_string()
            != task.0.scan
        {
            return Err(CatalogError::Corrupt("设备执行改变原元数据目标扫描"));
        }
        Ok(())
    }

    /// Retain actual media observations on the original Writing task. This does
    /// not publish a volume head, close the task or release any of its claims.
    /// A committed observation cannot be replaced by an older clean snapshot.
    pub async fn record_reclaim_metadata_observation(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataWrite,
        observation: &ReclaimMetadataObservation,
    ) -> Result<ReclaimMetadataWrite> {
        original.encode()?;
        if original.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        if !original.writing() {
            return Err(CatalogError::Invalid(
                "只有原 Writing 任务可保留介质结果观察",
            ));
        }
        let raw = observation.encode(&original.plan())?;
        let raw = raw.as_slice();
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let mut task =
                                load_id_at(&tr, reservation, plan.0.source.uid, original.id())
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != plan {
                                return Err(CatalogError::Invalid("观察改变原回收元数据计划"));
                            }
                            require_phase_history(original, &task)?;
                            if task.recovering() || task.resolved() {
                                return Err(CatalogError::Invalid(
                                    "原任务已转入修尾，不能再记录普通写入观察",
                                ));
                            }
                            if task.0.observation.as_deref() == Some(raw) {
                                return Ok(task);
                            }
                            if task.observation()?.is_some_and(|saved| {
                                saved.kind() == ReclaimMetadataObservationKind::Committed
                            }) {
                                return Err(CatalogError::Invalid(
                                    "已提交介质结果观察不能回退或改变",
                                ));
                            }
                            task.0.observation = Some(raw.to_vec());
                            let encoded = task.encode()?;
                            let header = registry::encode(reservation, &task)?;
                            let key = task_key(reservation, &task);
                            tr.set(&key, &encoded);
                            tr.set(&registry::key(reservation), &header);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Read the parent's last retained checkpoint, including a terminal attempt.
    /// This is a fact query, not pending-work classification or an I/O permit.
    /// An old owner is retained until explicitly claiming the exact handle.
    pub fn reclaim_metadata_checkpoint<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<Option<ReclaimMetadataWrite>>> + 'a>,
    > {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let task = registry::read_at(&tr, reservation).await?;
            if task.as_ref().is_some_and(|task| task.owner() > owner) {
                return Err(CatalogError::OwnerFenced);
            }
            Ok(task)
        })
    }

    /// Discover only an active attempt; a Closed checkpoint is not execution.
    pub async fn pending_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimMetadataWrite>> {
        Ok(self
            .reclaim_metadata_checkpoint(reservation, owner)
            .await?
            .filter(|task| task.holds_claim()))
    }

    /// Discover a retained source task before explicitly claiming its original
    /// handle. An older recorded owner is a fact, never execution permission.
    pub async fn discover_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
    ) -> Result<Option<ReclaimMetadataWrite>> {
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let task = load_at(&tr, reservation, source_uid).await?;
        if task.as_ref().is_some_and(|t| t.owner() > owner) {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(task)
    }

    /// Read the retained original task. Missing/corrupt slots are never repaired.
    pub async fn reclaim_metadata_write_status(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
    ) -> Result<Option<ReclaimMetadataWrite>> {
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let task = load_at(&tr, reservation, source_uid).await?;
        if task.as_ref().is_some_and(|t| t.owner() != owner) {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(task)
    }

    /// HA may claim only after the same parent has already been claimed.
    /// Claiming Writing preserves uncertainty; it does not retry device I/O.
    pub async fn claim_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
    ) -> Result<ReclaimMetadataWrite> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            isolation::require_normal_mutation_at(&tr, reservation, owner).await?;
                            let mut task = load_at(&tr, reservation, source_uid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if task.owner() > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            task.0.owner = owner;
                            let raw = task.encode()?;
                            let header =
                                registry::encode_if_current_at(&tr, reservation, &task).await?;
                            let key = task_key(reservation, &task);
                            tr.set(&key, &raw);
                            if let Some(header) = header {
                                tr.set(&registry::key(reservation), &header);
                            }
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Close only Prepared; Writing retains all claims until media recovery.
    /// The original plan and exact own claims remain bound throughout.
    /// Keep the original plan/receipt; clear only its exact verified own claims.
    pub async fn close_prepared_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataWrite> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let mut task = load_id_at(
                                &tr,
                                reservation,
                                original.0.plan.source.uid,
                                original.id(),
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner || original.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != original.plan() {
                                return Err(CatalogError::Invalid("关闭改变原元数据计划"));
                            }
                            require_phase_history(original, &task)?;
                            if task.closed() {
                                return Ok(task);
                            }
                            if task.completed() {
                                return Err(CatalogError::Invalid("已发布任务不能改成未写关闭"));
                            }
                            if task.0.phase != Phase::Prepared {
                                return Err(CatalogError::PoolBusy);
                            }
                            task.0.phase = Phase::Closed;
                            let raw = task.encode()?;
                            let key = task_key(reservation, &task);
                            let header = registry::encode(reservation, &task)?;
                            tr.set(&key, &raw);
                            tr.set(&registry::key(reservation), &header);
                            tr.clear(&ledger::own_key(reservation, b'A'));
                            tr.clear(&node_attempts::volume_key(task.plan().volume()));
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

pub(in super::super) async fn require_planning_absent_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
) -> Result<()> {
    require_idle_at(tr, reservation).await?;
    if load_at(tr, reservation, uid).await?.is_some() {
        return Err(CatalogError::Invalid("原UID已有元数据尝试，必须恢复原目标"));
    }
    Ok(())
}

pub(in super::super) async fn history_attempt_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    original: &ReclaimMetadataWrite,
) -> Result<(Uuid, Option<Uuid>, [u8; 32])> {
    original.encode()?;
    parent_at(tr, reservation, owner).await?;
    let task = load_id_at(tr, reservation, original.plan().source_uid(), original.id())
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if task.plan() != original.plan() || task.owner() > owner {
        return Err(CatalogError::Invalid("目的历史读取改变原尝试计划或owner"));
    }
    require_phase_history(original, &task)?;
    context::input_for_task_at(tr, reservation, owner, &task).await?;
    Ok((
        task.plan().volume(),
        Some(task.scan_id()),
        Sha256::digest(task.encode()?).into(),
    ))
}
