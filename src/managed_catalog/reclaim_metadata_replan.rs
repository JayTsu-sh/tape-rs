//! One request-bound successor after recovery, with independent fresh donor
//! facts. No original body proof, retirement row or previous plan is rewritten.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    version: u8,
    parent: String,
    first: String,
    previous: String,
    successor: String,
    scan: String,
    context: context::Context,
    original_plan: [u8; 32],
    plan: Plan,
    donor: donor::Fact,
}
impl Edge {
    fn checksum(raw: &[u8]) -> [u8; 32] {
        Sha256::new()
            .chain_update(b"tape-rs/reclaim-metadata-replan/1")
            .chain_update(raw)
            .finalize()
            .into()
    }
    fn encode(&self) -> Result<Vec<u8>> {
        for id in [&self.first, &self.previous, &self.successor, &self.scan] {
            node_mutation::id(id)?;
        }
        self.context.validate()?;
        let plan = ReclaimMetadataPlan(self.plan.clone());
        plan.encode()?;
        self.donor.require_plan(&plan)?;
        if self.version != 1
            || self.parent != self.plan.parent
            || self.previous == self.successor
            || self.first == self.successor
            || self.donor.attempt != self.previous
        {
            return Err(CatalogError::Corrupt("元数据重新规划后继身份无效"));
        }
        let mut raw = serde_json::to_vec(self).map_err(cache)?;
        if raw.len() > 65_000 {
            return Err(CatalogError::Invalid("元数据重新规划后继超出预算"));
        }
        raw.extend_from_slice(&Self::checksum(&raw));
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=65_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("元数据重新规划后继长度无效"));
        }
        let (body, sum) = raw.split_at(raw.len() - 32);
        if Self::checksum(body) != sum {
            return Err(CatalogError::Corrupt("元数据重新规划后继校验失败"));
        }
        let edge: Self = serde_json::from_slice(body).map_err(cache)?;
        if edge.encode()?.as_slice() != raw {
            return Err(CatalogError::Corrupt("元数据重新规划后继不是规范编码"));
        }
        Ok(edge)
    }
    fn require(
        &self,
        reservation: &ReclaimReservation,
        old: &ReclaimMetadataWrite,
        task: &ReclaimMetadataWrite,
    ) -> Result<()> {
        let before = old.plan();
        let after = task.plan();
        if !old.resolved()
            || self.parent != reservation.fingerprint()
            || self.first != old.0.first
            || self.previous != old.0.id
            || self.successor != task.0.id
            || task.previous_attempt() != Some(old.id())
            || task.plan_origin() != task.id()
            || task.0.first != old.0.first
            || self.context != old.0.context
            || self.context != task.0.context
            || self.scan != old.0.scan
            || self.scan != task.0.scan
            || self.plan != after.0
            || self.original_plan != before.fingerprint()?
            || self.donor.recovery_epoch != old.recovery_epoch()
            || self.donor.owner > old.owner()
            || self.donor.owner > task.owner()
            || before.0.input != after.0.input
            || before.0.source != after.0.source
            || before.0.source_index != after.0.source_index
            || before.0.inventory != after.0.inventory
            || before.0.volume != after.0.volume
            || before.0.donor != after.0.donor
            || before.path() != after.path()
        {
            return Err(CatalogError::Corrupt(
                "后继计划改变原请求、来源、donor 或恢复事实",
            ));
        }
        self.donor.require_plan(&after)
    }
}

pub(super) fn key(reservation: &ReclaimReservation, uid: u64, previous: Uuid) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'V');
    key.extend_from_slice(&uid.to_be_bytes());
    key.extend_from_slice(previous.as_bytes());
    key
}
async fn at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
    previous: Uuid,
) -> Result<Option<Edge>> {
    tr.get(&key(reservation, uid, previous), false)
        .await?
        .map(|raw| Edge::decode(&raw))
        .transpose()
}

pub(super) async fn require_previous_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    old: &ReclaimMetadataWrite,
    task: &ReclaimMetadataWrite,
) -> Result<()> {
    if old.plan().is_source_based() {
        return Box::pin(source_replan::require_previous_at(
            tr,
            reservation,
            old,
            task,
        ))
        .await;
    }
    let edge = at(tr, reservation, task.plan().source_uid(), old.id())
        .await?
        .ok_or(CatalogError::Corrupt("新计划缺少原请求绑定的后继边"))?;
    edge.require(reservation, old, task)?;
    let plan = task.plan();
    let body = tr
        .get(&relocation::body_key(reservation, plan.source_uid()), false)
        .await?
        .ok_or(CatalogError::Corrupt("后继缺少原正文迁移事实"))?;
    let expected = json!({"version":1,"parent":reservation.fingerprint(),"inventory":plan.0.inventory,
        "uid":plan.source_uid(),"object_sha256":Sha256::digest(plan.0.source.encode()?).to_vec(),
        "sha256":edge.donor.sha256,"length":plan.0.source.length});
    if body.as_ref() != expected.to_string().as_bytes() {
        return Err(CatalogError::Corrupt("后继 donor 摘要改变原已迁移正文"));
    }
    let result = Box::pin(resolution::require_task_at(tr, reservation, old)).await?;
    if result.kind() == ReclaimMetadataRecoveryKind::Committed {
        return Err(CatalogError::Corrupt("已提交元数据结果不能另建后继"));
    }
    let previous = active_volume::decode_index(task.plan().volume(), &edge.donor.index)?;
    if previous.generation < result.current().generation
        || previous.generation == result.current().generation && previous != result.current()
        || tr
            .get(
                &registry::edge_key(reservation, task.plan().source_uid(), old.id()),
                false,
            )
            .await?
            .is_some()
    {
        return Err(CatalogError::Corrupt("重新规划后继的原卷头或唯一边冲突"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Query the exact retained successor after an undelivered admission. This
    /// restores a historical handle, never a new owner or device capability.
    pub fn reclaim_metadata_successor<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
        operation: Uuid,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<Option<ReclaimMetadataWrite>>> + 'a>,
    > {
        Box::pin(async move {
            original.encode()?;
            if operation.is_nil() {
                return Err(CatalogError::Invalid("后继请求身份为空"));
            }
            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let old = load_id_at(
                &tr,
                reservation,
                original.plan().source_uid(),
                original.id(),
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            require_phase_history(original, &old)?;
            if old.plan() != original.plan() || old.owner() > owner {
                return Err(CatalogError::OwnerFenced);
            }
            if old.plan().is_source_based() {
                return source_replan::successor_at(&tr, reservation, &old, operation, owner).await;
            }
            let Some(edge) = at(&tr, reservation, old.plan().source_uid(), old.id()).await? else {
                return Ok(None);
            };
            if edge.successor != operation.to_string() {
                return Err(CatalogError::Invalid("原尝试已有不同请求的唯一后继"));
            }
            let task = load_id_at(&tr, reservation, old.plan().source_uid(), operation)
                .await?
                .ok_or(CatalogError::Corrupt("已登记后继缺少原任务"))?;
            edge.require(reservation, &old, &task)?;
            if task.owner() > owner {
                return Err(CatalogError::OwnerFenced);
            }
            Ok(Some(task))
        })
    }

    /// Atomically persist the unique request, donor fact, new fixed plan and
    /// its exact claims. A saved old plan or a saved donor row cannot call this.
    #[allow(clippy::too_many_arguments)]
    pub fn replan_reclaim_metadata_write<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
        original: &'a ReclaimMetadataWrite,
        operation: Uuid,
        plan: &'a ReclaimMetadataPlan,
        proof: &'a ReclaimMetadataDonorEvidence,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        Box::pin(async move {
            original.encode()?;
            plan.encode()?;
            request.validate()?;
            if !original.resolved()
                || operation.is_nil()
                || operation == original.id()
                || operation == original.first_attempt()
            {
                return Err(CatalogError::Invalid(
                    "重新规划要求原恢复终态和独立后继请求",
                ));
            }
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
                                publication::require_original(original, &old, owner)?;
                                if old != *original {
                                    return Err(CatalogError::Invalid("重新规划改变原终态快照"));
                                }
                                old.0.context.require(reservation, file, request)?;
                                let input =
                                    input_at(&tr, reservation, owner, file, request, &old.plan())
                                        .await?;
                                proof.require(&old, owner, &input)?;
                                if let Some(edge) =
                                    at(&tr, reservation, old.plan().source_uid(), old.id()).await?
                                {
                                    if edge.successor != operation.to_string()
                                        || edge.plan != plan.0
                                        || edge.donor != proof.fact
                                    {
                                        return Err(CatalogError::Invalid(
                                            "重复重新规划改变原后继请求、计划或 donor",
                                        ));
                                    }
                                    let task = load_id_at(
                                        &tr,
                                        reservation,
                                        old.plan().source_uid(),
                                        operation,
                                    )
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                    edge.require(reservation, &old, &task)?;
                                    if task.owner() != owner {
                                        return Err(CatalogError::OwnerFenced);
                                    }
                                    return Ok(task);
                                }
                                require_idle_at(&tr, reservation).await?;
                                let actual = Box::pin(donor::context_at(
                                    &tr,
                                    reservation,
                                    owner,
                                    file,
                                    request,
                                    &old,
                                ))
                                .await?;
                                proof.require_context(&actual, plan)?;
                                if tr
                                    .get(&latest_key(reservation, plan.source_uid()), false)
                                    .await?
                                    .as_deref()
                                    != Some(old.id().as_bytes())
                                    || tr
                                        .get(
                                            &attempt_key(reservation, plan.source_uid(), operation),
                                            false,
                                        )
                                        .await?
                                        .is_some()
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
                                {
                                    return Err(CatalogError::Corrupt(
                                        "重新规划的原最新索引、后继槽或唯一边冲突",
                                    ));
                                }
                                let scan = target_at(&tr, reservation, plan, &input).await?;
                                if scan.to_string() != old.0.scan {
                                    return Err(CatalogError::Corrupt("重新规划改变原目标扫描"));
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
                                    parent: reservation.fingerprint(),
                                    first: old.0.first.clone(),
                                    previous: old.id().to_string(),
                                    successor: operation.to_string(),
                                    scan: scan.to_string(),
                                    context: old.0.context.clone(),
                                    original_plan: old.plan().fingerprint()?,
                                    plan: plan.0.clone(),
                                    donor: proof.fact.clone(),
                                };
                                edge.require(reservation, &old, &task)?;
                                let raw = task.encode()?;
                                let edge_raw = edge.encode()?;
                                let held = claim(reservation, &task)?;
                                let header = registry::encode(reservation, &task)?;
                                tr.set(&key(reservation, plan.source_uid(), old.id()), &edge_raw);
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
