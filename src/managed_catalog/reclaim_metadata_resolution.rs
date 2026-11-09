//! Resolve an explicit recovery goal from fresh media evidence. Receipts are
//! immutable and independent of the mutable parent, manifest and attempt rows.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimMetadataRecoveryKind {
    Unwritten,
    Committed,
    Discarded,
    Checkpointed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Observed {
    Unwritten {
        observation: Vec<u8>,
        ip: u64,
    },
    Committed {
        observation: Vec<u8>,
    },
    Checkpointed {
        index: Vec<u8>,
        files: u64,
    },
    Discarded {
        index: Vec<u8>,
        files: u64,
        first: u64,
        end: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    kind: ReclaimMetadataRecoveryKind,
    current: ActiveIndexIdentity,
    files: u64,
}

impl Observed {
    fn validate(&self, plan: &ReclaimMetadataPlan) -> Result<View> {
        match self {
            Self::Unwritten { observation, .. } | Self::Committed { observation } => {
                let value = ReclaimMetadataObservation::decode(plan, observation)?;
                let (kind, expected) = match self {
                    Self::Unwritten { ip, .. } if (1..=value.current().generation).contains(ip) => {
                        (
                            ReclaimMetadataRecoveryKind::Unwritten,
                            ReclaimMetadataObservationKind::Unchanged,
                        )
                    }
                    Self::Committed { .. } => (
                        ReclaimMetadataRecoveryKind::Committed,
                        ReclaimMetadataObservationKind::Committed,
                    ),
                    _ => return Err(CatalogError::Corrupt("恢复未写结果缺少有效 IP 证明")),
                };
                if value.kind() != expected {
                    return Err(CatalogError::Corrupt("恢复结果类型与原观察不同"));
                }
                Ok(View {
                    kind,
                    current: value.current(),
                    files: value.physical_files(),
                })
            }
            Self::Checkpointed { index, files } => {
                let current = active_volume::decode_index(plan.volume(), index)?;
                let old = plan.previous_source()?.head();
                if !old.incremental
                    || current.incremental
                    || old.generation.checked_add(1) != Some(current.generation)
                    || current.location.partition != old.location.partition
                    || Some(current.location.start_block) != plan.previous_dp_eod().checked_add(1)
                    || *files != plan.0.physical_files
                {
                    return Err(CatalogError::Corrupt("完整检查点结果与原增量计划不同"));
                }
                Ok(View {
                    kind: ReclaimMetadataRecoveryKind::Checkpointed,
                    current,
                    files: *files,
                })
            }
            Self::Discarded {
                index,
                files,
                first,
                end,
            } => {
                let current = active_volume::decode_index(plan.volume(), index)?;
                let old = plan.previous_source()?.head();
                if current.incremental
                    || old.generation.checked_add(1) != Some(current.generation)
                    || current.location.partition != old.location.partition
                    || current.location.start_block.checked_sub(2) != Some(*end)
                    || *first != plan.previous_dp_eod()
                    || first > end
                    || *files != plan.0.physical_files
                {
                    return Err(CatalogError::Corrupt("恢复修尾结果与原计划边界不同"));
                }
                Ok(View {
                    kind: ReclaimMetadataRecoveryKind::Discarded,
                    current,
                    files: *files,
                })
            }
        }
    }
}

/// A live proof of the original durable recovery goal's actual outcome. A saved
/// observation or a former Writing token cannot manufacture this capability.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimMetadataRecoveryEvidence =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataRecoveryEvidence {
    task: ReclaimMetadataWrite,
    result: Observed,
    view: View,
}
impl ReclaimMetadataRecoveryEvidence {
    pub fn kind(&self) -> ReclaimMetadataRecoveryKind {
        self.view.kind
    }
    #[cfg(test)]
    pub(crate) fn from_current(
        task: &ReclaimMetadataWrite,
        owner: u64,
        current: &CapturedIndexSource,
        original: &CapturedIndexSource,
    ) -> Result<Self> {
        Self::from_current_with_raw_read(task, owner, current, original, None)
    }
    pub(crate) fn from_current_with_raw_read(
        task: &ReclaimMetadataWrite,
        owner: u64,
        current: &CapturedIndexSource,
        original: &CapturedIndexSource,
        bytes: Option<(u64, [u8; 32], Option<[u8; 16]>)>,
    ) -> Result<Self> {
        if !task.recovering() || task.owner() != owner {
            return Err(CatalogError::Invalid("恢复证明要求当前原持久恢复阶段"));
        }
        let IndexSourceScope::Current {
            ip_generation: Some(ip),
            ..
        } = current.scope()
        else {
            return Err(CatalogError::Invalid(
                "恢复结果要求有效完整 IP；缺失 IP 须显式检查点恢复",
            ));
        };
        if ip == 0 || ip > current.construct().generation {
            return Err(CatalogError::Invalid("恢复结果 IP 代次无效或超前"));
        }
        let plan = task.plan();
        plan.require_discard_source(original)?;
        let result = if let Ok(value) = plan.observe_current(current, Some(original)) {
            let observation = value.encode(&plan)?;
            match value.kind() {
                ReclaimMetadataObservationKind::Committed => {
                    plan.require_raw_readback(current, bytes)?;
                    Observed::Committed { observation }
                }
                ReclaimMetadataObservationKind::Unchanged => {
                    let IndexSourceScope::Current {
                        ip_generation: Some(ip),
                        ..
                    } = current.scope()
                    else {
                        return Err(CatalogError::Invalid("未写恢复要求完整且不超前的 IP"));
                    };
                    Observed::Unwritten { observation, ip }
                }
            }
        } else if task.checkpointing() {
            let value = plan.observe_checkpointed(current, original)?;
            let mut index = Vec::new();
            active_volume::encode_index(&mut index, value.current);
            Observed::Checkpointed {
                index,
                files: value.files,
            }
        } else {
            let value = plan.observe_discarded(current, original)?;
            let mut index = Vec::new();
            active_volume::encode_index(&mut index, value.current());
            let range = value.abandoned_range();
            Observed::Discarded {
                index,
                files: value.physical_files(),
                first: *range.start(),
                end: *range.end(),
            }
        };
        let view = result.validate(&plan)?;
        Ok(Self {
            task: task.clone(),
            result,
            view,
        })
    }
    fn require(&self, task: &ReclaimMetadataWrite, owner: u64) -> Result<()> {
        if self.task != *task
            || !task.recovering()
            || task.owner() != owner
            || self.result.validate(&task.plan())? != self.view
        {
            return Err(CatalogError::Invalid(
                "恢复证明改变原阶段、owner 或持久快照",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    namespace: String,
    library: String,
    pool: String,
    task: Record,
    goal: Phase,
    result: Observed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataResolution {
    wire: Wire,
    view: View,
}
impl ReclaimMetadataResolution {
    pub fn id(&self) -> Uuid {
        node_mutation::id(&self.wire.task.id).expect("validated recovery receipt")
    }
    pub fn kind(&self) -> ReclaimMetadataRecoveryKind {
        self.view.kind
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.view.current
    }
    pub fn physical_files(&self) -> u64 {
        self.view.files
    }
    pub fn plan(&self) -> ReclaimMetadataPlan {
        ReclaimMetadataPlan(self.wire.task.plan.clone())
    }
    pub fn observed_owner(&self) -> u64 {
        self.wire.task.owner
    }
    fn from_wire(wire: Wire) -> Result<Self> {
        node_mutation::id(&wire.namespace)?;
        node_mutation::id(&wire.pool)?;
        validate_serial(&wire.library)?;
        let task = ReclaimMetadataWrite(wire.task.clone());
        task.encode()?;
        if wire.version != 1
            || !task.resolved()
            || !matches!(wire.goal, Phase::Discarding | Phase::Checkpointing)
            || wire.goal == Phase::Checkpointing
                && matches!(wire.result, Observed::Discarded { .. })
            || wire.goal == Phase::Discarding
                && matches!(wire.result, Observed::Checkpointed { .. })
            || task.observation()?.is_some_and(|saved| {
                saved.kind() == ReclaimMetadataObservationKind::Committed
                    && match &wire.result {
                        Observed::Committed { observation } => {
                            task.0.observation.as_ref() != Some(observation)
                        }
                        _ => true,
                    }
            })
        {
            return Err(CatalogError::Corrupt("恢复回执缺少原终态"));
        }
        let view = wire.result.validate(&task.plan())?;
        Ok(Self { wire, view })
    }
    fn checksum(raw: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-resolution/1");
        hash.update(raw);
        hash.finalize().into()
    }
    fn encode(&self) -> Result<Vec<u8>> {
        if Self::from_wire(self.wire.clone())? != *self {
            return Err(CatalogError::Corrupt("恢复回执与结果不同"));
        }
        let mut raw = serde_json::to_vec(&self.wire).map_err(cache)?;
        if raw.len() > 65_000 {
            return Err(CatalogError::Invalid("恢复回执超出预算"));
        }
        raw.extend_from_slice(&Self::checksum(&raw));
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=65_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("恢复回执长度无效"));
        }
        let (body, sum) = raw.split_at(raw.len() - 32);
        if Self::checksum(body) != sum {
            return Err(CatalogError::Corrupt("恢复回执完整性校验失败"));
        }
        let value = Self::from_wire(serde_json::from_slice(body).map_err(cache)?)?;
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("恢复回执不是规范编码"));
        }
        Ok(value)
    }
}

fn key(ns: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = domain(ns, b'J');
    key.extend_from_slice(id.as_bytes());
    key
}
async fn at(tr: &Transaction, ns: Uuid, id: Uuid) -> Result<Option<ReclaimMetadataResolution>> {
    tr.get(&key(ns, id), false)
        .await?
        .map(|raw| ReclaimMetadataResolution::decode(&raw))
        .transpose()
}

/// Historical validation uses only immutable receipts, placement and history.
pub(in crate::managed_catalog) async fn require_history_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let value = at(tr, placement.namespace, id)
        .await?
        .ok_or(CatalogError::Corrupt("恢复历史缺少独立回执"))?;
    if value.id() != id
        || value.kind() == ReclaimMetadataRecoveryKind::Unwritten
        || value.current() != index
        || value.wire.namespace != placement.namespace.to_string()
        || value.wire.library != serial
        || value.wire.library != placement.library_serial
        || value.wire.pool != placement.pool_id.to_string()
        || value.wire.task.scan
            != require_volume_ready(tr, index.volume_uuid, None)
                .await?
                .id
                .to_string()
        || active_volume::stored_file_count_at(tr, index).await? != Some(value.physical_files())
    {
        return Err(CatalogError::Corrupt("恢复历史与独立回执或计数不同"));
    }
    Ok(())
}
pub(in crate::managed_catalog) async fn require_committed_history_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<bool> {
    require_history_at(tr, serial, id, index).await?;
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    Ok(at(tr, placement.namespace, id)
        .await?
        .ok_or(CatalogError::Incomplete)?
        .kind()
        == ReclaimMetadataRecoveryKind::Committed)
}

pub(super) async fn require_task_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataResolution> {
    let value = at(tr, reservation.namespace(), task.id())
        .await?
        .ok_or(CatalogError::Corrupt("恢复终态缺少独立回执"))?;
    let mut observed = ReclaimMetadataWrite(value.wire.task.clone());
    if observed.owner() > task.owner() {
        return Err(CatalogError::Corrupt("恢复回执 owner 超前"));
    }
    observed.0.owner = task.owner();
    if observed != *task
        || value.wire.namespace != reservation.namespace().to_string()
        || value.wire.library != reservation.serial()
        || value.wire.pool != reservation.spec().pool.to_string()
    {
        return Err(CatalogError::Corrupt("恢复终态改变原任务或归属"));
    }
    if value.kind() != ReclaimMetadataRecoveryKind::Unwritten
        && Box::pin(active_volume::history_at(
            tr,
            reservation.serial(),
            value.current().volume_uuid,
            value.current().generation,
        ))
        .await?
            != (
                active_volume::IndexKind::ReclaimRecovery(task.id()),
                value.current(),
            )
    {
        return Err(CatalogError::Corrupt("恢复终态缺少原物理历史"));
    }
    Ok(value)
}
pub(super) async fn require_state_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<()> {
    if task.resolved() {
        Box::pin(require_task_at(tr, reservation, task)).await?;
    } else if at(tr, reservation.namespace(), task.id()).await?.is_some() {
        return Err(CatalogError::Corrupt("活跃恢复任务已存在终态回执"));
    }
    Ok(())
}

impl ManagedCatalog {
    pub fn verify_reclaim_metadata_recovery<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,

        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            let task = context::claimed_task_at(&tr, reservation, owner, original).await?;
            if !task.recovering() || task != *original {
                return Err(CatalogError::Invalid("恢复执行要求原持久恢复快照"));
            }

            let input = context::input_for_task_at(&tr, reservation, owner, &task).await?;
            if target_context_policy_at(&tr, reservation, &task.plan(), input.as_ref(), false)
                .await?
                .to_string()
                != task.0.scan
                || active_volume::recovery_active(&tr, task.plan().volume()).await?
            {
                return Err(CatalogError::Corrupt("恢复执行改变原扫描或卷恢复状态"));
            }
            Ok(())
        })
    }
    pub fn reclaim_metadata_resolution<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,

        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<Option<ReclaimMetadataResolution>>> + 'a>,
    > {
        Box::pin(async move {
            original.encode()?;

            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let task = load_id_at(
                &tr,
                reservation,
                original.plan().source_uid(),
                original.id(),
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            require_phase_history(original, &task)?;

            context::input_for_task_at(&tr, reservation, owner, &task).await?;
            if original.plan() != task.plan() || original.owner() > owner || task.owner() > owner {
                return Err(CatalogError::OwnerFenced);
            }
            if !task.resolved() {
                return Ok(None);
            }
            require_task_at(&tr, reservation, &task).await.map(Some)
        })
    }
    pub fn resolve_reclaim_metadata_recovery<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,

        original: &'a ReclaimMetadataWrite,
        proof: &'a ReclaimMetadataRecoveryEvidence,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataResolution>> + 'a>>
    {
        Box::pin(async move {
            original.encode()?;
            proof.require(original, owner)?;
            self.db
                .run(|tr, _| {
                    Box::pin(async move {
                        requests::retry_result(
                            async {
                                parent_at(&tr, reservation, owner).await?;
                                let mut task = Box::pin(load_id_at(
                                    &tr,
                                    reservation,
                                    original.plan().source_uid(),
                                    original.id(),
                                ))
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                                publication::require_original(original, &task, owner)?;

                                if task.recovery_epoch() != original.recovery_epoch() {
                                    return Err(CatalogError::Invalid(
                                        "恢复证明已被新阶段序号取代",
                                    ));
                                }
                                context::input_for_task_at(&tr, reservation, owner, &task).await?;
                                if task.resolved() {
                                    let saved = require_task_at(&tr, reservation, &task).await?;
                                    if saved.wire.result != proof.result {
                                        return Err(CatalogError::Invalid("恢复重放改变首次结果"));
                                    }
                                    return Ok(saved);
                                }
                                if task != *original || !task.recovering() {
                                    return Err(CatalogError::Invalid("恢复结清改变原快照"));
                                }
                                let plan = task.plan();
                                let (scan, previous) = active_volume::observed_at(
                                    &tr,
                                    reservation.serial(),
                                    plan.volume(),
                                )
                                .await?;
                                if scan.id.to_string() != task.0.scan
                                    || previous != plan.previous_source()?.head().into()
                                    || active_volume::file_count_at(
                                        &tr,
                                        reservation.serial(),
                                        plan.volume(),
                                    )
                                    .await?
                                        != plan.0.physical_files
                                    || active_volume::recovery_active(&tr, plan.volume()).await?
                                {
                                    return Err(CatalogError::Corrupt(
                                        "恢复结清改变原卷头、计数或扫描",
                                    ));
                                }
                                let generation = previous
                                    .generation
                                    .checked_add(1)
                                    .ok_or(CatalogError::Corrupt("恢复代次溢出"))?;
                                publication::require_empty_result_at(
                                    &tr,
                                    reservation.namespace(),
                                    plan.volume(),
                                    generation,
                                )
                                .await?;
                                task.0.phase = Phase::Resolved;
                                let value = ReclaimMetadataResolution::from_wire(Wire {
                                    version: 1,
                                    namespace: reservation.namespace().to_string(),
                                    library: reservation.serial().into(),
                                    pool: reservation.spec().pool.to_string(),
                                    task: task.0.clone(),
                                    goal: original.0.phase,
                                    result: proof.result.clone(),
                                })?;
                                let receipt = value.encode()?;
                                let raw = task.encode()?;
                                let header = registry::encode(reservation, &task)?;
                                tr.set(&key(reservation.namespace(), task.id()), &receipt);
                                if value.kind() != ReclaimMetadataRecoveryKind::Unwritten {
                                    active_volume::set_reclaim_recovery_at(
                                        &tr,
                                        previous,
                                        value.current(),
                                        task.id(),
                                        value.physical_files(),
                                    );
                                }
                                tr.set(&task_key(reservation, &task), &raw);
                                tr.set(&registry::key(reservation), &header);
                                tr.clear(&ledger::own_key(reservation, b'A'));
                                tr.clear(&node_attempts::volume_key(plan.volume()));
                                Ok(value)
                            }
                            .await,
                        )
                    })
                })
                .await?
        })
    }
}

#[cfg(test)]
#[path = "reclaim_metadata_checkpoint_tests.rs"]
mod checkpoint_tests;
