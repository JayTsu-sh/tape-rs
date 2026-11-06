//! Close an absent physical attempt without losing its logical pool barrier.
use super::*;
use crate::managed_catalog::{NodeMutationAbsenceEvidence, NodeMutationAbsenceKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationAttemptClosure {
    attempt: NodeMutationAttempt,
    kind: NodeMutationAbsenceKind,
    current: ActiveIndexIdentity,
    file_count: u64,
    closed_owner: u64,
    closed_at_millis: u64,
}
impl NodeMutationAttemptClosure {
    pub fn attempt(&self) -> &NodeMutationAttempt {
        &self.attempt
    }
    pub fn kind(&self) -> NodeMutationAbsenceKind {
        self.kind
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn closed_owner(&self) -> u64 {
        self.closed_owner
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    attempt: String,
    kind: NodeMutationAbsenceKind,
    current: String,
    file_count: u64,
    closed_owner: u64,
    closed_at_millis: u64,
}
fn key(namespace: Uuid, volume: Uuid, source_generation: u64, attempt: Uuid) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'c');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&source_generation.to_be_bytes());
    key.extend_from_slice(attempt.as_bytes());
    key
}
fn encode(value: &NodeMutationAttemptClosure) -> Result<Vec<u8>> {
    let before = value.attempt.previous;
    let valid_index = match value.kind {
        NodeMutationAbsenceKind::Unchanged => value.current == before,
        NodeMutationAbsenceKind::Discarded => {
            value.current.volume_uuid == before.volume_uuid
                && before.generation.checked_add(1) == Some(value.current.generation)
                && value.current.location.partition == 'b'
                && value.current.location.start_block > before.location.start_block
                && !value.current.incremental
        }
    };
    if !valid_index
        || value.closed_owner < value.attempt.owner
        || !(value.attempt.created_at_millis..=253_402_300_799_999)
            .contains(&value.closed_at_millis)
    {
        return Err(CatalogError::Corrupt("节点尝试关闭身份、位置或时间不符"));
    }
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(before.volume_uuid, &current)?;
    let bytes = node_mutation::canonical(&Wire {
        version: 1,
        attempt: URL_SAFE_NO_PAD.encode(super::encode(&value.attempt)?),
        kind: value.kind,
        current: URL_SAFE_NO_PAD.encode(current),
        file_count: value.file_count,
        closed_owner: value.closed_owner,
        closed_at_millis: value.closed_at_millis,
    })?;
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点关闭记录过长"));
    }
    Ok(bytes)
}
fn decode(bytes: &[u8]) -> Result<NodeMutationAttemptClosure> {
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点关闭记录过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点关闭记录编码"))?;
    let raw = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| CatalogError::Corrupt("节点关闭二进制字段"))
    };
    let attempt = super::decode(&raw(&wire.attempt)?)?;
    let value = NodeMutationAttemptClosure {
        current: active_volume::decode_index(attempt.previous.volume_uuid, &raw(&wire.current)?)?,
        attempt,
        kind: wire.kind,
        file_count: wire.file_count,
        closed_owner: wire.closed_owner,
        closed_at_millis: wire.closed_at_millis,
    };
    if wire.version != 1 || encode(&value)? != bytes {
        return Err(CatalogError::Corrupt("节点关闭记录必须为规范版本1"));
    }
    Ok(value)
}
pub(super) async fn at(
    tr: &Transaction,
    attempt: &NodeMutationAttempt,
) -> Result<Option<NodeMutationAttemptClosure>> {
    let Some(raw) = tr
        .get(
            &key(
                attempt.namespace,
                attempt.previous.volume_uuid,
                attempt.previous.generation,
                attempt.id,
            ),
            false,
        )
        .await?
    else {
        return Ok(None);
    };
    let value = decode(&raw)?;
    if value.attempt != *attempt {
        return Err(CatalogError::Corrupt("节点关闭记录与原尝试不符"));
    }
    Ok(Some(value))
}

pub(super) async fn require_unused_at(
    tr: &Transaction,
    attempt: &NodeMutationAttempt,
) -> Result<()> {
    let target = key(
        attempt.namespace,
        attempt.previous.volume_uuid,
        attempt.previous.generation,
        attempt.id,
    );
    if let Some(raw) = tr.get(&target, false).await? {
        let saved = decode(&raw)?;
        if key(
            saved.attempt.namespace,
            saved.attempt.previous.volume_uuid,
            saved.attempt.previous.generation,
            saved.attempt.id,
        ) != target
        {
            return Err(CatalogError::Corrupt("节点关闭记录与键作用域不符"));
        }
        return Err(CatalogError::Invalid("已关闭节点尝试身份不能复用"));
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn require_discard_at(
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
            &key(
                placement.namespace,
                index.volume_uuid,
                index
                    .generation
                    .checked_sub(1)
                    .ok_or(CatalogError::Corrupt("节点修尾代数无前代"))?,
                id,
            ),
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("节点修尾历史缺少关闭记录"))?;
    let value = decode(&raw)?;
    if value.kind != NodeMutationAbsenceKind::Discarded
        || value.current != index
        || value.attempt.id != id
        || value.attempt.library != serial
        || value.attempt.library != placement.library_serial
        || value.attempt.namespace != placement.namespace
        || value.attempt.pool != placement.pool_id
        || require_volume_ready(tr, index.volume_uuid, None).await?.id != value.attempt.scan
        || active_volume::stored_file_count_at(tr, index).await? != Some(value.file_count)
    {
        return Err(CatalogError::Corrupt(
            "节点修尾历史与关闭记录、扫描或文件数不符",
        ));
    }
    Ok(())
}

pub(super) async fn require_correlation_at(
    tr: &Transaction,
    value: &NodeMutationAttemptClosure,
) -> Result<()> {
    if value.kind == NodeMutationAbsenceKind::Discarded
        && (active_volume::history_at(
            tr,
            &value.attempt.library,
            value.current.volume_uuid,
            value.current.generation,
        )
        .await?
            != (
                active_volume::IndexKind::NodeDiscard(value.attempt.id),
                value.current,
            )
            || active_volume::observed_at(tr, &value.attempt.library, value.current.volume_uuid)
                .await?
                .1
                .generation
                < value.current.generation)
    {
        return Err(CatalogError::Corrupt("节点关闭记录与修尾历史或游标不符"));
    }
    Ok(())
}

impl ManagedCatalog {
    pub async fn node_mutation_attempt_closure(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
    ) -> Result<Option<NodeMutationAttemptClosure>> {
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_runtime_active_owner(&tr, &attempt.library, route, owner).await?;
        if namespace != attempt.namespace {
            return Err(CatalogError::Invalid("节点关闭记录与活动路由不符"));
        }
        node_tasks::retained(&tr, namespace, attempt.epoch).await?;
        let value = at(&tr, attempt).await?;
        if value.is_none() && observation::at(&tr, attempt).await?.is_none() {
            let task = node_tasks::task_at(
                &tr,
                &attempt.library,
                namespace,
                attempt.epoch,
                attempt.operation,
            )
            .await?
            .ok_or(CatalogError::Corrupt("节点尝试缺少任务及关闭结果"))?;
            if task.active_attempt != Some(attempt.id) {
                return Err(CatalogError::Corrupt("节点尝试退出 Writing 却缺少关闭记录"));
            }
            require_task_at(&tr, &task).await?;
        }
        if let Some(value) = &value {
            require_correlation_at(&tr, value).await?;
        }
        Ok(value)
    }

    /// Only opaque absence evidence can close Writing. Keep the logical pool
    /// barrier and original request; a retry returns the first immutable result
    /// even if another physical attempt has since been admitted.
    pub async fn close_node_mutation_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
        plan: &NodeMutationPlan,
        evidence: &NodeMutationAbsenceEvidence,
    ) -> Result<NodeMutationAttemptClosure> {
        if !attempt.matches_plan(plan)
            || evidence.fingerprint() != attempt.object.digest()
            || evidence.context() != plan.context()
        {
            return Err(CatalogError::Invalid("节点未发生证明与原物理尝试不符"));
        }
        let now = management::now().max(attempt.created_at_millis);
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            256_000,
                        ))?;
                        let task = current_task_at(&tr, route, owner, plan).await?;
                        if let Some(saved) = at(&tr, attempt).await? {
                            if saved.current != evidence.current()
                                || saved.file_count != evidence.file_count()
                                || saved.kind != evidence.kind()
                            {
                                return Err(CatalogError::Corrupt("节点关闭重试与原结果不符"));
                            }
                            require_correlation_at(&tr, &saved).await?;
                            return Ok(saved);
                        }
                        if require_task_at(&tr, &task).await? != *attempt {
                            return Err(CatalogError::Invalid("节点关闭要求原 Writing 尝试"));
                        }
                        let placement = verified_placement(&tr, attempt.previous.volume_uuid)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if placement.namespace != attempt.namespace
                            || placement.pool_id != attempt.pool
                            || placement.library_serial != attempt.library
                        {
                            return Err(CatalogError::Corrupt("节点关闭卷的归属已改变"));
                        }
                        let (scan, current) = active_volume::observed_at(
                            &tr,
                            &attempt.library,
                            attempt.previous.volume_uuid,
                        )
                        .await?;
                        if scan.id != attempt.scan || current != attempt.previous {
                            return Err(CatalogError::Invalid("节点关闭的扫描或前代游标已改变"));
                        }
                        if observation::at(&tr, attempt).await?.is_some() {
                            return Err(CatalogError::Invalid(
                                "节点尝试已有 S4 观察，不能作为未发生关闭",
                            ));
                        }
                        let count = active_volume::file_count_at(
                            &tr,
                            &attempt.library,
                            current.volume_uuid,
                        )
                        .await?;
                        if count != evidence.file_count() {
                            return Err(CatalogError::Corrupt("节点关闭的实际文件数已改变"));
                        }
                        let next = current.generation + 1;
                        let mut count_key =
                            active_volume::generation_key(current.volume_uuid, next);
                        count_key.push(b'f');
                        if active_volume::recovery_active(&tr, current.volume_uuid).await?
                            || tr
                                .get(&active_batch::batch_key(current.volume_uuid, next), false)
                                .await?
                                .is_some()
                            || tr
                                .get(
                                    &active_volume::generation_key(current.volume_uuid, next),
                                    false,
                                )
                                .await?
                                .is_some()
                            || tr.get(&count_key, false).await?.is_some()
                        {
                            return Err(CatalogError::Corrupt("节点关闭的下一代已有冲突记录"));
                        }
                        let value = NodeMutationAttemptClosure {
                            attempt: attempt.clone(),
                            kind: evidence.kind(),
                            current: evidence.current(),
                            file_count: count,
                            closed_owner: owner,
                            closed_at_millis: now,
                        };
                        let encoded = encode(&value)?;
                        let closed = key(
                            attempt.namespace,
                            current.volume_uuid,
                            attempt.previous.generation,
                            attempt.id,
                        );
                        // This helper encodes before its first write. No fallible work follows.
                        node_tasks::return_prepared_at(&tr, &task, attempt.id)?;
                        if value.kind == NodeMutationAbsenceKind::Discarded {
                            active_volume::set_node_discard_at(
                                &tr,
                                attempt.previous,
                                value.current,
                                attempt.id,
                                count,
                            );
                        }
                        tr.set(&closed, &encoded);
                        tr.clear(&volume_key(current.volume_uuid));
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
