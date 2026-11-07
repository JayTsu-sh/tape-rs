//! Serial, owner-fenced execution of one durable logical node task. The actor
//! selects and mounts the medium and supplies its current PR/Permission; this
//! module never fences, moves, formats or salvages a cartridge. Every error
//! consumes the volume so dirty or indeterminate state cannot be reused.
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::TailKind;
use crate::ltfs::volume::LtfsVolume;
use crate::managed_catalog::{
    ActiveIndexIdentity, ActiveLibraryRoute, CatalogError, ManagedCatalog, NodeMutation,
    NodeMutationAttempt, NodeMutationDeletion, NodeMutationPlan, NodeMutationPublication,
    NodeMutationRequest, NodeMutationTask, NodeMutationTaskState, VolumePlacement,
};
use crate::scsi::reservation::ReservationKey;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeMediaResult {
    Published(NodeMutationPublication),
    Deleted(NodeMutationDeletion),
}

pub struct NodeMediaCommit<'a> {
    volume: LtfsVolume<'a>,
    result: NodeMediaResult,
}

impl<'a> NodeMediaCommit<'a> {
    pub fn result(&self) -> &NodeMediaResult {
        &self.result
    }
    pub fn volume(&self) -> &LtfsVolume<'a> {
        &self.volume
    }
    pub fn into_volume(self) -> LtfsVolume<'a> {
        self.volume
    }
}

fn catalog_error(task: &NodeMutationTask, stage: &str, error: CatalogError) -> TapeError {
    let detail = format!(
        "FDB 节点任务 {}/{} {stage}: {error}",
        task.request_epoch, task.operation
    );
    if matches!(error, CatalogError::OwnerFenced) {
        TapeError::OwnershipLost {
            device: task.library_serial.clone(),
            detail,
        }
    } else {
        TapeError::Catalog(detail)
    }
}

fn same_request(expected: &NodeMutationTask, current: &NodeMutationTask) -> bool {
    expected.library_serial == current.library_serial
        && expected.namespace == current.namespace
        && expected.pool == current.pool
        && expected.request_epoch == current.request_epoch
        && expected.operation == current.operation
        && expected.object == current.object
        && expected.fingerprint == current.fingerprint
        && expected.accepted_owner_epoch == current.accepted_owner_epoch
        && expected.created_at_millis == current.created_at_millis
}

// Only pre-admission failures can be classified as an unwritten refusal.
// Writing/recovery/publication continue to preserve their original errors.
fn prewrite_error(
    task: &NodeMutationTask,
    stage: &str,
    barcode: &str,
    error: CatalogError,
) -> TapeError {
    match error {
        CatalogError::Invalid(reason) => TapeError::Refused {
            reason: format!(
                "FDB 节点任务 {}/{} {stage}: {reason}",
                task.request_epoch, task.operation
            ),
        },
        CatalogError::PoolBusy | CatalogError::PoolImporting => TapeError::PoolBusy {
            pool: task.pool.to_string(),
        },
        CatalogError::MediaUnavailable => TapeError::MediaUnavailable {
            barcode: barcode.into(),
        },
        CatalogError::Incomplete => TapeError::NotReady("节点规范输入尚未就绪".into()),
        error => catalog_error(task, stage, error),
    }
}

async fn media_scope(
    catalog: &ManagedCatalog,
    task: &NodeMutationTask,
    route: ActiveLibraryRoute,
    owner: u64,
    source: ActiveIndexIdentity,
) -> Result<VolumePlacement> {
    let placement = catalog
        .volume_placement(source.volume_uuid)
        .await
        .map_err(|e| catalog_error(task, "核对介质归属", e))?
        .ok_or_else(|| TapeError::Catalog("节点执行介质尚未登记".into()))?;
    if placement.namespace != task.namespace
        || placement.pool_id != task.pool
        || placement.library_serial != task.library_serial
    {
        return Err(TapeError::Catalog("节点执行介质不属于任务带库和池".into()));
    }
    let head = catalog
        .active_loaded_volume_head(&task.library_serial, &placement.barcode, route, owner)
        .await
        .map_err(|e| catalog_error(task, "核对介质策略", e))?
        .filter(|head| head.volume_id == source.volume_uuid && head.pool_id == task.pool)
        .ok_or_else(|| TapeError::Catalog("节点执行介质归属已改变".into()))?;
    if !head.online || head.blacklisted {
        return Err(TapeError::MediaUnavailable {
            barcode: placement.barcode,
        });
    }
    Ok(placement)
}

struct NodeExecution<'a> {
    catalog: &'a ManagedCatalog,
    route: ActiveLibraryRoute,
    owner: u64,
    budget: usize,
}

impl NodeExecution<'_> {
    async fn publish(
        &self,
        task: &NodeMutationTask,
        attempt: &NodeMutationAttempt,
        plan: &NodeMutationPlan,
        request: &NodeMutationRequest,
    ) -> Result<NodeMediaResult> {
        match plan.mutation() {
            NodeMutation::Unlink | NodeMutation::RemoveDirectory => self
                .catalog
                .publish_node_mutation_deletion(
                    self.route,
                    self.owner,
                    attempt,
                    plan,
                    request,
                    self.budget,
                )
                .await
                .map(NodeMediaResult::Deleted)
                .map_err(|e| catalog_error(task, "发布删除", e)),
            _ => self
                .catalog
                .publish_node_mutation_attempt(
                    self.route,
                    self.owner,
                    attempt,
                    plan,
                    request,
                    self.budget,
                )
                .await
                .map(NodeMediaResult::Published)
                .map_err(|e| catalog_error(task, "发布节点", e)),
        }
    }
}

/// Execute or resume the original request. The actor must serialize device
/// work and provide a fenced key and live GuardedTransport Permission. Recovery
/// requires a fresh mount after takeover; never put a new key on an old cached
/// view and use it as recovery evidence. Known
/// completion returns the first result without touching the supplied volume;
/// that result's historical S4 need not equal the volume's current head.
pub async fn execute_node_mutation<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    route: ActiveLibraryRoute,
    key: ReservationKey,
    selected: &NodeMutationTask,
    budget: usize,
) -> Result<NodeMediaCommit<'a>> {
    let serial = selected.library_serial.as_str();
    let owner = key.round();
    let execution = NodeExecution {
        catalog,
        route,
        owner,
        budget,
    };
    if selected.namespace != route.route.namespace || budget == 0 {
        return Err(TapeError::Catalog("节点执行作用域或字节预算无效".into()));
    }
    // At most one recovery closure is followed by a fresh physical attempt.
    // We never spin on an indeterminate device operation or re-append an S4.
    for _ in 0..2 {
        let task = catalog
            .node_mutation_task(serial, route, selected.request_epoch, selected.operation)
            .await
            .map_err(|e| catalog_error(selected, "读取任务", e))?
            .ok_or_else(|| TapeError::Catalog("节点执行任务尚未登记".into()))?;
        if !same_request(selected, &task) {
            return Err(TapeError::Catalog("节点执行身份与原始任务不符".into()));
        }
        if task.state == NodeMutationTaskState::Completed {
            let (published, deleted) = tokio::try_join!(
                catalog.node_mutation_publication(
                    serial,
                    route,
                    owner,
                    task.request_epoch,
                    task.operation
                ),
                catalog.node_mutation_deletion(
                    serial,
                    route,
                    owner,
                    task.request_epoch,
                    task.operation
                ),
            )
            .map_err(|e| catalog_error(&task, "读取原完成结果", e))?;
            let result = match (published, deleted) {
                (Some(value), None) => NodeMediaResult::Published(value),
                (None, Some(value)) => NodeMediaResult::Deleted(value),
                _ => return Err(TapeError::Catalog("节点完成任务缺少唯一结果".into())),
            };
            return Ok(NodeMediaCommit { volume, result });
        }
        if task.state == NodeMutationTaskState::Rejected {
            return Err(TapeError::Refused {
                reason: format!(
                    "节点任务已拒绝: {}",
                    task.rejection_reason
                        .as_deref()
                        .ok_or_else(|| TapeError::Catalog("节点拒绝任务缺少原原因".into()))?
                ),
            });
        }
        if task.state == NodeMutationTaskState::Cancelled {
            return Err(TapeError::Refused {
                reason: "节点任务已取消".into(),
            });
        }
        let (_, request) = catalog
            .node_mutation_task_request(
                serial,
                route,
                owner,
                task.request_epoch,
                task.operation,
                budget,
            )
            .await
            .map_err(|e| catalog_error(&task, "加载原请求", e))?;
        if volume
            .guarded_execution()
            .is_some_and(|(round, _)| round != owner)
        {
            return Err(TapeError::OwnershipLost {
                device: serial.into(),
                detail: "卷执行许可与当前代次不符".into(),
            });
        }
        volume.set_reservation_guard(Some(key));
        let cached_source = volume.committed_index_construct().into();
        let placement = media_scope(catalog, &task, route, owner, cached_source).await?;
        let mut task = task;
        if task.state == NodeMutationTaskState::Queued {
            volume.verified_committed_index_construct()?;
            task = catalog
                .prepare_node_mutation_task(
                    serial,
                    route,
                    owner,
                    task.request_epoch,
                    task.operation,
                    budget,
                )
                .await
                .map_err(|e| prewrite_error(&task, "准备任务", &placement.barcode, e))?;
        }
        match task.state {
            NodeMutationTaskState::Prepared => {
                let source = volume.verified_committed_index_construct()?.into();
                let inputs = catalog
                    .capture_prepared_node_inputs(
                        serial,
                        route,
                        owner,
                        task.request_epoch,
                        task.operation,
                        budget,
                    )
                    .await
                    .map_err(|e| prewrite_error(&task, "捕获规范输入", &placement.barcode, e))?;
                let plan = catalog
                    .prepare_node_mutation_plan(route, &inputs, source, volume.index())
                    .await
                    .map_err(|e| prewrite_error(&task, "构建物理计划", &placement.barcode, e))?;
                let attempt = catalog
                    .admit_node_mutation_attempt(
                        route,
                        owner,
                        Uuid::new_v4(),
                        &inputs,
                        &plan,
                        budget,
                    )
                    .await
                    .map_err(|e| prewrite_error(&task, "准入物理尝试", &placement.barcode, e))?;
                volume.stage_node_mutation(&plan)?;
                let saved = catalog
                    .pending_node_mutation_attempt(serial, route, owner, source.volume_uuid)
                    .await
                    .map_err(|e| catalog_error(&task, "提交前复核尝试", e))?;
                if saved.as_ref() != Some(&attempt) {
                    return Err(TapeError::Catalog("节点提交前的原物理尝试已改变".into()));
                }
                media_scope(catalog, &task, route, owner, source).await?;
                volume.sync_batch()?;
                let evidence = volume.node_mutation_evidence(&plan)?;
                catalog
                    .observe_node_mutation_attempt(route, owner, &attempt, &plan, &evidence)
                    .await
                    .map_err(|e| catalog_error(&task, "观察实际节点 S4", e))?;
                let result = execution.publish(&task, &attempt, &plan, &request).await?;
                log::info!(
                    "FDB 节点任务 {}/{} 已提交并发布",
                    task.request_epoch,
                    task.operation
                );
                return Ok(NodeMediaCommit { volume, result });
            }
            NodeMutationTaskState::Writing => {
                let source: ActiveIndexIdentity =
                    volume.verified_recovery_index_construct()?.into();
                let attempt = catalog
                    .pending_node_mutation_attempt(serial, route, owner, source.volume_uuid)
                    .await
                    .map_err(|e| catalog_error(&task, "发现原待恢复尝试", e))?
                    .ok_or_else(|| TapeError::Catalog("Writing 节点任务缺少本卷原尝试".into()))?;
                let plan = catalog
                    .load_node_mutation_attempt_plan(route, owner, &attempt, budget)
                    .await
                    .map_err(|e| catalog_error(&task, "加载原物理计划", e))?;
                if plan.context().request_epoch != task.request_epoch
                    || plan.context().operation != task.operation
                    || plan.context().pool != task.pool
                    || plan.context().namespace != task.namespace
                {
                    return Err(TapeError::Catalog(
                        "本卷待恢复尝试不属于所选节点任务".into(),
                    ));
                }
                if let Some(observed) = catalog
                    .node_mutation_observation(route, owner, &attempt)
                    .await
                    .map_err(|e| catalog_error(&task, "读取原节点观察", e))?
                {
                    if ActiveIndexIdentity::from(volume.verified_committed_index_construct()?)
                        != observed.current()
                    {
                        return Err(TapeError::Catalog("原节点观察与当前实际介质头不符".into()));
                    }
                    let result = execution.publish(&task, &attempt, &plan, &request).await?;
                    return Ok(NodeMediaCommit { volume, result });
                }
                let absence = match volume.recovery().dp.tail {
                    TailKind::Complete => match volume.node_mutation_evidence(&plan) {
                        Ok(evidence) => {
                            catalog
                                .observe_node_mutation_attempt(
                                    route, owner, &attempt, &plan, &evidence,
                                )
                                .await
                                .map_err(|e| catalog_error(&task, "恢复实际节点 S4", e))?;
                            let result =
                                execution.publish(&task, &attempt, &plan, &request).await?;
                            return Ok(NodeMediaCommit { volume, result });
                        }
                        Err(TapeError::Catalog(_)) => {
                            volume.node_mutation_absence_evidence(&plan)?
                        }
                        Err(error) => return Err(error),
                    },
                    TailKind::UnindexedData | TailKind::TruncatedIndex => {
                        media_scope(catalog, &task, route, owner, source).await?;
                        volume.discard_node_mutation_tail(&plan)?
                    }
                    tail => {
                        return Err(TapeError::RecoveryRestricted {
                            reason: format!("节点恢复拒绝未知 DP 尾部 {tail:?}"),
                        });
                    }
                };
                catalog
                    .close_node_mutation_attempt(route, owner, &attempt, &plan, &absence)
                    .await
                    .map_err(|e| catalog_error(&task, "关闭未发生的原尝试", e))?;
            }
            _ => {
                return Err(TapeError::Refused {
                    reason: "节点任务状态已改变，请查询原结果".into(),
                });
            }
        }
    }
    Err(TapeError::Catalog(
        "节点执行任务反复变化，保留原任务等待恢复".into(),
    ))
}
