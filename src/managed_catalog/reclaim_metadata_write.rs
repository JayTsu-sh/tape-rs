//! Durable metadata intent, without device execution or erase authority.
//! Once admitted, unknown media results retain the original plan and claims.
use super::*;
use metadata_plan::Plan;
#[path = "reclaim_metadata_publication.rs"]
mod publication;
#[path = "reclaim_metadata_unwritten.rs"]
mod unwritten;
pub use publication::ReclaimMetadataPublication;
pub(in crate::managed_catalog) use publication::require_history_at as require_publication_at;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Writing,
    Closed,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    id: String,
    scan: String,
    plan: Plan,
    owner: u64,
    phase: Phase,
    observation: Option<Vec<u8>>,
}

/// A retained fixed plan, never a device I/O permit or migration receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataWrite(Record);

impl ReclaimMetadataWrite {
    pub fn id(&self) -> Uuid {
        Uuid::parse_str(&self.0.id).expect("validated metadata attempt")
    }
    pub(crate) fn scan_id(&self) -> Uuid {
        Uuid::parse_str(&self.0.scan).expect("validated metadata scan")
    }
    pub fn plan(&self) -> ReclaimMetadataPlan {
        ReclaimMetadataPlan(self.0.plan.clone())
    }
    pub fn owner(&self) -> u64 {
        self.0.owner
    }
    pub fn writing(&self) -> bool {
        self.0.phase == Phase::Writing
    }
    pub fn closed(&self) -> bool {
        self.0.phase == Phase::Closed
    }
    pub fn completed(&self) -> bool {
        self.0.phase == Phase::Committed
    }
    fn holds_claim(&self) -> bool {
        matches!(self.0.phase, Phase::Prepared | Phase::Writing)
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
        node_mutation::id(&self.0.id)?;
        node_mutation::id(&self.0.scan)?;
        if let Some(observed) = self.observation()?
            && !(self.writing()
                || self.completed()
                || self.closed() && observed.kind() == ReclaimMetadataObservationKind::Unchanged)
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
        if self.0.version != 3 || self.0.owner == 0 || self.0.owner > 0xFFFF_FFFF_FFFF {
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
    if original.id() != current.id() || original.0.scan != current.0.scan {
        return Err(CatalogError::Corrupt(
            "回收元数据任务改变原物理尝试或扫描身份",
        ));
    }
    if original.0.phase != Phase::Prepared
        && original.0.phase != current.0.phase
        && !(original.writing() && (current.completed() || current.closed()))
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
fn task_key(reservation: &ReclaimReservation, plan: &ReclaimMetadataPlan) -> Result<Vec<u8>> {
    let mut key = task_prefix(reservation, plan.0.source.uid);
    key.extend_from_slice(&plan.fingerprint()?);
    Ok(key)
}
fn claim(reservation: &ReclaimReservation, task: &ReclaimMetadataWrite) -> Result<Vec<u8>> {
    let plan = task.plan();
    node_claims::encode(&node_claims::NodeVolumeClaim::ReclaimMetadata(Box::new(
        node_claims::ReclaimMetadataVolumeClaim {
            namespace: reservation.namespace().to_string(),
            volume: plan.0.volume.clone(),
            attempt: task.0.id.clone(),
            task: task_key(reservation, &plan)?,
            fingerprint: plan.fingerprint()?,
        },
    )))
}

pub(in super::super) async fn require_idle_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    if tr
        .get(&ledger::own_key(reservation, b'A'), false)
        .await?
        .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

async fn parent_at(tr: &Transaction, reservation: &ReclaimReservation, owner: u64) -> Result<()> {
    if record_at(tr, reservation, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}

async fn load_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
) -> Result<Option<ReclaimMetadataWrite>> {
    if uid == 0 {
        return Err(CatalogError::Invalid("回收元数据源 UID 不能为零"));
    }
    let Some(fp) = tr.get(&latest_key(reservation, uid), false).await? else {
        if requests::prefix_has_rows(tr, &task_prefix(reservation, uid)).await? {
            return Err(CatalogError::Corrupt("回收元数据任务缺少原身份索引"));
        }
        return Ok(None);
    };
    if fp.len() != 32 {
        return Err(CatalogError::Corrupt("回收元数据任务身份长度无效"));
    }
    let mut key = task_prefix(reservation, uid);
    key.extend_from_slice(&fp);
    let raw = tr
        .get(&key, false)
        .await?
        .ok_or(CatalogError::Corrupt("回收元数据任务缺失"))?;
    let task = ReclaimMetadataWrite::decode(&raw)?;
    let plan = task.plan();
    if plan.0.parent != reservation.fingerprint()
        || plan.0.source.uid != uid
        || plan.fingerprint()?.as_slice() != fp.as_ref()
    {
        return Err(CatalogError::Corrupt("回收元数据任务与原父身份不符"));
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
    let expected = claim(reservation, &task)?;
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
    if task.completed() {
        Box::pin(publication::require_task_at(tr, reservation, &task)).await?;
    }
    Ok(Some(task))
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

async fn target_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    plan: &ReclaimMetadataPlan,
    input: &ReclaimMetadataInput,
) -> Result<Uuid> {
    require_pool_without_reclaim_at(tr, reservation.namespace(), reservation.spec().pool).await?;
    let (scan, index) = active_volume::observed_at(tr, reservation.serial(), plan.volume()).await?;
    let mut encoded = Vec::new();
    active_volume::encode_index(&mut encoded, index);
    let count = active_volume::file_count_at(tr, reservation.serial(), plan.volume()).await?;
    if encoded != plan.0.previous || count != plan.0.physical_files {
        return Err(CatalogError::Invalid(
            "回收元数据准入时目标卷头或物理文件计数改变",
        ));
    }
    let pool_raw = tr
        .get(
            &pool_key(reservation.namespace(), reservation.spec().pool),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let pool = decode_pool(reservation.spec().pool, &pool_raw)?;
    if count.checked_add(1).is_none_or(|n| n > pool.file_limit) {
        return Err(CatalogError::Incomplete);
    }
    let assignment = ManagedCatalog::verified_assignment(
        tr,
        reservation.serial(),
        &input.destination().placement().barcode,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if !assignment.allows_io() {
        return Err(CatalogError::MediaUnavailable);
    }
    tape_monitor::require_data_writable(
        tr,
        reservation.namespace(),
        reservation.serial(),
        &input.destination().placement().barcode,
    )
    .await?;
    Ok(scan.id)
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
        plan.encode()?;
        if plan.0.parent != reservation.fingerprint() {
            return Err(CatalogError::Invalid("回收元数据计划不属于原父任务"));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let input =
                                input_at(&tr, reservation, owner, file, request, plan).await?;
                            if let Some(saved) =
                                load_at(&tr, reservation, plan.0.source.uid).await?
                            {
                                if saved.plan() != *plan {
                                    return Err(CatalogError::Invalid(
                                        "回收元数据重试改变固定计划",
                                    ));
                                }
                                if saved.owner() != owner {
                                    return Err(CatalogError::OwnerFenced);
                                }
                                return Ok(saved);
                            }
                            require_idle_at(&tr, reservation).await?;
                            let scan = target_at(&tr, reservation, plan, &input).await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                            )
                            .await?;
                            let task = ReclaimMetadataWrite(Record {
                                version: 3,
                                id: Uuid::new_v4().to_string(),
                                scan: scan.to_string(),
                                plan: plan.0.clone(),
                                owner,
                                phase: Phase::Prepared,
                                observation: None,
                            });
                            let raw = task.encode()?;
                            let key = task_key(reservation, plan)?;
                            let fp = plan.fingerprint()?;
                            let held = claim(reservation, &task)?;
                            // All fallible validation/encoding precedes the first mutation.
                            tr.set(&key, &raw);
                            tr.set(&latest_key(reservation, plan.0.source.uid), &fp);
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
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataAdmission> {
        request.validate()?;
        original.encode()?;
        if original.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let mut task = load_at(&tr, reservation, plan.0.source.uid)
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
                                Phase::Closed | Phase::Committed => {
                                    return Err(CatalogError::Invalid(
                                        "已关闭或已发布的元数据任务不能重新准入",
                                    ));
                                }
                                Phase::Writing => true,
                                Phase::Prepared => false,
                            };
                            // Replays keep the same child/retirement identity;
                            // changed arguments cannot become a new plan.
                            let input =
                                input_at(&tr, reservation, owner, file, request, &plan).await?;
                            if needs_observation {
                                return Ok(ReclaimMetadataAdmission {
                                    task,
                                    needs_observation: true,
                                });
                            }
                            if target_at(&tr, reservation, &plan, &input)
                                .await?
                                .to_string()
                                != task.0.scan
                            {
                                return Err(CatalogError::Corrupt("准入改变原元数据目标扫描"));
                            }
                            task.0.phase = Phase::Writing;
                            let raw = task.encode()?;
                            let key = task_key(reservation, &plan)?;
                            tr.set(&key, &raw);
                            Ok(ReclaimMetadataAdmission {
                                task,
                                needs_observation: false,
                            })
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Read-only, same-version recheck for a fresh device execution. Losing an
    /// input, claim, writable policy or original head prevents the next CDB.
    /// Re-observing an uncertain result uses the original Writing status only.
    pub(crate) async fn verify_reclaim_metadata_execution(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
    ) -> Result<()> {
        request.validate()?;
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let plan = original.plan();
        let task = load_at(&tr, reservation, plan.source_uid())
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if task.owner() != owner || original.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        if task != *original || !task.writing() {
            return Err(CatalogError::Invalid("设备执行改变原 Writing 任务或观察"));
        }
        let input = input_at(&tr, reservation, owner, file, request, &plan).await?;
        if target_at(&tr, reservation, &plan, &input)
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
                            let mut task = load_at(&tr, reservation, plan.0.source.uid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != plan {
                                return Err(CatalogError::Invalid("观察改变原回收元数据计划"));
                            }
                            require_phase_history(original, &task)?;
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
                            tr.set(&task_key(reservation, &plan)?, &encoded);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
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
                            let mut task = load_at(&tr, reservation, source_uid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if task.owner() > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            task.0.owner = owner;
                            let raw = task.encode()?;
                            tr.set(&task_key(reservation, &task.plan())?, &raw);
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
                            let mut task = load_at(&tr, reservation, original.0.plan.source.uid)
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
                            if task.writing() {
                                return Err(CatalogError::PoolBusy);
                            }
                            task.0.phase = Phase::Closed;
                            let raw = task.encode()?;
                            let key = task_key(reservation, &task.plan())?;
                            tr.set(&key, &raw);
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
