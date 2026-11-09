//! Source-only replanning requires an owner-fenced current index read, not
//! a fictitious data donor. Its RV edge retains the original resolution.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataSourceReplanContext {
    task: ReclaimMetadataWrite,
    current: ActiveIndexIdentity,
    files: u64,
}
impl ReclaimMetadataSourceReplanContext {
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataSourceReplanEvidence {
    context: ReclaimMetadataSourceReplanContext,
    plan: ReclaimMetadataPlan,
    fact: Fact,
}
impl ReclaimMetadataSourceReplanEvidence {
    pub fn plan(&self) -> &ReclaimMetadataPlan {
        &self.plan
    }
    pub(crate) fn from_capture(
        context: ReclaimMetadataSourceReplanContext,
        current: &CapturedIndexSource,
        plan: ReclaimMetadataPlan,
    ) -> Result<Self> {
        if context.current != ActiveIndexIdentity::from(current.construct())
            || plan.observe_current(current, None)?.kind()
                != ReclaimMetadataObservationKind::Unchanged
            || plan.0.physical_files != context.files
            || !plan.is_source_based()
            || context.task.owner() == 0
        {
            return Err(CatalogError::Invalid(
                "原始来源重新规划缺少本轮精确索引实读",
            ));
        }
        let fact = Fact {
            attempt: context.task.id().to_string(),
            owner: context.task.owner(),
            recovery_epoch: context.task.recovery_epoch(),
            plan: plan.fingerprint()?,
            source: current.identity().map_err(cache)?.fingerprint(),
            end: current_proof(current.scope(), current.header())?.0,
        };
        fact.require_plan(&plan)?;
        Ok(Self {
            context,
            plan,
            fact,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    attempt: String,
    owner: u64,
    recovery_epoch: u64,
    plan: [u8; 32],
    source: [u8; 32],
    end: u64,
}
impl Fact {
    fn require_plan(&self, plan: &ReclaimMetadataPlan) -> Result<()> {
        node_mutation::id(&self.attempt)?;
        if self.owner == 0
            || self.recovery_epoch == 0
            || !plan.is_source_based()
            || self.plan != plan.fingerprint()?
            || self.source != plan.previous_source()?.fingerprint()
            || self.end != plan.previous_dp_eod()
        {
            return Err(CatalogError::Corrupt("原始来源重新规划观察与原计划不同"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    version: u8,
    source: bool,
    parent: String,
    previous: Record,
    successor: Record,
    observation: Fact,
}
impl Edge {
    fn checksum(raw: &[u8]) -> [u8; 32] {
        Sha256::new()
            .chain_update(b"tape-rs/reclaim-source-replan/1")
            .chain_update(raw)
            .finalize()
            .into()
    }
    fn encode(&self) -> Result<Vec<u8>> {
        let old = ReclaimMetadataWrite(self.previous.clone());
        let new = ReclaimMetadataWrite(self.successor.clone());
        old.encode()?;
        new.encode()?;
        let before = old.plan();
        let after = new.plan();
        if self.version != 1
            || !self.source
            || self.parent != before.0.parent
            || self.parent != after.0.parent
            || !old.resolved()
            || !new.0.context.is_source_based()
            || !before.is_source_based()
            || !after.is_source_based()
            || !matches!(new.0.phase, Phase::Prepared)
            || new.recovery_epoch() != 0
            || new.id() == old.id()
            || new.id() == old.first_attempt()
            || new.previous_attempt() != Some(old.id())
            || new.plan_origin() != new.id()
            || new.first_attempt() != old.first_attempt()
            || new.0.context != old.0.context
            || new.0.scan != old.0.scan
            || self.observation.attempt != old.id().to_string()
            || self.observation.owner != old.owner()
            || self.observation.owner != new.owner()
            || self.observation.recovery_epoch != old.recovery_epoch()
            || before.0.raw_body != after.0.raw_body
            || before.0.source != after.0.source
            || before.0.source_index != after.0.source_index
            || before.0.input != after.0.input
            || before.0.inventory != after.0.inventory
            || before.0.volume != after.0.volume
            || before.path() != after.path()
            || before.0.donor.uid != after.0.donor.uid
            || before.0.donor.path != after.0.donor.path
        {
            return Err(CatalogError::Corrupt("原始来源后继改变原来源、目标或阶段"));
        }
        self.observation.require_plan(&after)?;
        let mut raw = serde_json::to_vec(self).map_err(cache)?;
        if raw.len() > 65_000 {
            return Err(CatalogError::Invalid("原始来源后继证明超出预算"));
        }
        raw.extend_from_slice(&Self::checksum(&raw));
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=65_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("原始来源后继证明长度"));
        }
        let (body, sum) = raw.split_at(raw.len() - 32);
        if Self::checksum(body) != sum {
            return Err(CatalogError::Corrupt("原始来源后继证明摘要"));
        }
        let edge: Self = serde_json::from_slice(body).map_err(cache)?;
        if edge.encode()? != raw {
            return Err(CatalogError::Corrupt("原始来源后继证明不是规范编码"));
        }
        Ok(edge)
    }
    fn require(
        &self,
        reservation: &ReclaimReservation,
        old: &ReclaimMetadataWrite,
        new: &ReclaimMetadataWrite,
    ) -> Result<()> {
        self.encode()?;
        let mut before = ReclaimMetadataWrite(self.previous.clone());
        let mut after = ReclaimMetadataWrite(self.successor.clone());
        if before.owner() > old.owner() || after.owner() > new.owner() {
            return Err(CatalogError::OwnerFenced);
        }
        before.0.owner = old.owner();
        after.0.owner = new.owner();
        // The edge seals its initial Prepared task; later phase progress is
        // validated separately, without rewriting this independent edge.
        require_phase_history(&after, new)?;
        if self.parent != reservation.fingerprint()
            || before != *old
            || after.id() != new.id()
            || after.plan() != new.plan()
            || after.0.context != new.0.context
            || after.0.scan != new.0.scan
            || after.0.plan_origin != new.0.plan_origin
            || after.0.first != new.0.first
            || after.0.previous != new.0.previous
        {
            return Err(CatalogError::Corrupt("原始来源后继与原尝试行不同"));
        }
        Ok(())
    }
}
async fn at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    old: &ReclaimMetadataWrite,
) -> Result<Option<Edge>> {
    tr.get(
        &replan::key(reservation, old.plan().source_uid(), old.id()),
        false,
    )
    .await?
    .map(|raw| Edge::decode(&raw))
    .transpose()
}
pub(super) async fn require_previous_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    old: &ReclaimMetadataWrite,
    new: &ReclaimMetadataWrite,
) -> Result<()> {
    let edge = at(tr, reservation, old)
        .await?
        .ok_or(CatalogError::Corrupt("原始来源新计划缺少独立后继证明"))?;
    edge.require(reservation, old, new)?;
    let result = Box::pin(resolution::require_task_at(tr, reservation, old)).await?;
    let previous = new.plan().previous_source()?.head();
    if result.kind() == ReclaimMetadataRecoveryKind::Committed
        || previous.generation < result.current().generation
        || previous.generation == result.current().generation
            && ActiveIndexIdentity::from(previous) != result.current()
        || tr
            .get(
                &registry::edge_key(reservation, new.plan().source_uid(), old.id()),
                false,
            )
            .await?
            .is_some()
    {
        return Err(CatalogError::Corrupt(
            "原始来源后继与原恢复结果或唯一边冲突",
        ));
    }
    Ok(())
}
pub(super) async fn successor_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    old: &ReclaimMetadataWrite,
    operation: Uuid,
    owner: u64,
) -> Result<Option<ReclaimMetadataWrite>> {
    let Some(edge) = at(tr, reservation, old).await? else {
        return Ok(None);
    };
    if edge.successor.id != operation.to_string() {
        return Err(CatalogError::Invalid("原节点已有不同请求的唯一后继"));
    }
    let task = Box::pin(load_id_at(
        tr,
        reservation,
        old.plan().source_uid(),
        operation,
    ))
    .await?
    .ok_or(CatalogError::Incomplete)?;
    edge.require(reservation, old, &task)?;
    if task.owner() > owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(Some(task))
}

async fn context_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    original: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataSourceReplanContext> {
    let task = Box::pin(context::claimed_task_at(tr, reservation, owner, original)).await?;
    if task != *original
        || !task.resolved()
        || !task.plan().is_source_based()
        || registry::read_at(tr, reservation).await?.as_ref() != Some(&task)
        || Box::pin(context::input_for_task_at(tr, reservation, owner, &task))
            .await?
            .is_some()
    {
        return Err(CatalogError::Invalid("来源重新规划要求原当前已结清尝试"));
    }
    let result = Box::pin(resolution::require_task_at(tr, reservation, &task)).await?;
    let (scan, current) =
        active_volume::observed_at(tr, reservation.serial(), task.plan().volume()).await?;
    if result.kind() == ReclaimMetadataRecoveryKind::Committed
        || scan.id.to_string() != task.0.scan
        || current.generation < result.current().generation
        || current.generation == result.current().generation && current != result.current()
    {
        return Err(CatalogError::Invalid("节点原提交已完成或目的历史改变"));
    }
    let files = active_volume::file_count_at(tr, reservation.serial(), current.volume_uuid).await?;
    native_device::require_volume_quiescent_at(tr, reservation.namespace(), current.volume_uuid)
        .await?;
    require_parent_pool_at(tr, reservation).await?;
    let placement = verified_placement(tr, current.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.namespace != reservation.namespace()
        || placement.pool_id != reservation.spec().pool
        || placement.library_serial != reservation.serial()
    {
        return Err(CatalogError::Invalid("来源重新规划的目的归属改变"));
    }
    target_barcode_policy_at(
        tr,
        reservation,
        &placement.barcode,
        Some(current.volume_uuid),
        files,
        false,
    )
    .await?;
    Ok(ReclaimMetadataSourceReplanContext {
        task,
        current,
        files,
    })
}

impl ManagedCatalog {
    pub fn reclaim_metadata_source_replan_context<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        task: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<ReclaimMetadataSourceReplanContext>> + 'a>,
    > {
        Box::pin(async move { context_at(&self.db.create_trx()?, reservation, owner, task).await })
    }

    pub fn replan_reclaim_metadata_source<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
        operation: Uuid,
        proof: &'a ReclaimMetadataSourceReplanEvidence,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        Box::pin(async move {
            if operation.is_nil()
                || operation == original.id()
                || operation == original.first_attempt()
                || proof.context.task != *original
                || original.owner() != owner
            {
                return Err(CatalogError::Invalid("原始来源后继请求或本轮观察无效"));
            }
            let plan = proof.plan();
            plan.encode()?;
            proof.fact.require_plan(plan)?;
            self.db
                .run(|tr, _| {
                    Box::pin(async move {
                        requests::retry_result(
                            async {
                                parent_at(&tr, reservation, owner).await?;
                                let old = Box::pin(load_id_at(
                                    &tr,
                                    reservation,
                                    original.plan().source_uid(),
                                    original.id(),
                                ))
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                                if old != *original {
                                    return Err(CatalogError::Invalid("原始来源恢复快照已改变"));
                                }
                                if let Some(saved) =
                                    Box::pin(successor_at(&tr, reservation, &old, operation, owner))
                                        .await?
                                {
                                    if saved.plan() != *plan {
                                        return Err(CatalogError::Invalid(
                                            "原始来源后继重试改变计划",
                                        ));
                                    }
                                    return Ok(saved);
                                }
                                if Box::pin(context_at(&tr, reservation, owner, &old)).await?
                                    != proof.context
                                {
                                    return Err(CatalogError::Invalid(
                                        "原始来源目的状态在实读后改变",
                                    ));
                                }
                                require_idle_at(&tr, reservation).await?;
                                let scan = target_context_policy_at(
                                    &tr,
                                    reservation,
                                    plan,
                                    None,
                                    plan.added_files() > 0,
                                )
                                .await?;
                                if scan.to_string() != old.0.scan
                                    || tr
                                        .get(
                                            &registry::edge_key(
                                                reservation,
                                                plan.source_uid(),
                                                old.id(),
                                            ),
                                            false,
                                        )
                                        .await?
                                        .is_some()
                                    || tr
                                        .get(
                                            &attempt_key(reservation, plan.source_uid(), operation),
                                            false,
                                        )
                                        .await?
                                        .is_some()
                                {
                                    return Err(CatalogError::Corrupt(
                                        "原始来源后继扫描或唯一槽冲突",
                                    ));
                                }
                                let task = ReclaimMetadataWrite(Record {
                                    version: 7,
                                    id: operation.to_string(),
                                    first: old.0.first.clone(),
                                    plan_origin: operation.to_string(),
                                    previous: Some(old.id().to_string()),
                                    scan: scan.to_string(),
                                    plan: plan.0.clone(),
                                    context: old.0.context.clone(),
                                    owner,
                                    phase: Phase::Prepared,
                                    recovery_epoch: 0,
                                    observation: None,
                                });
                                let edge = Edge {
                                    version: 1,
                                    source: true,
                                    parent: reservation.fingerprint(),
                                    previous: old.0.clone(),
                                    successor: task.0.clone(),
                                    observation: proof.fact.clone(),
                                };
                                edge.require(reservation, &old, &task)?;
                                let edge_raw = edge.encode()?;
                                let raw = task.encode()?;
                                let held = claim(reservation, &task)?;
                                let header = registry::encode(reservation, &task)?;
                                tr.set(
                                    &replan::key(reservation, plan.source_uid(), old.id()),
                                    &edge_raw,
                                );
                                tr.set(&task_key(reservation, &task), &raw);
                                tr.set(
                                    &latest_key(reservation, plan.source_uid()),
                                    operation.as_bytes(),
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
        })
    }
}
