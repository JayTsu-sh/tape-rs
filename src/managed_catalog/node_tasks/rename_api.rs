//! Kind-specific entry points over the common ledger. Prepared is not media authority.
use super::*;

pub(in crate::managed_catalog::control) async fn require_rename_prepared_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    expected: &NodeMutationTask,
    request: &NodeRenameRequest,
) -> Result<()> {
    let current = known_at(tr, route, owner, request, &expected.object).await?;
    if expected.kind != NodeMutationTaskKind::Rename
        || expected.state != NodeMutationTaskState::Prepared
        || current.as_ref() != Some(expected)
    {
        return Err(CatalogError::Invalid(
            "rename 规范输入要求原任务仍为 Prepared",
        ));
    }
    node_plan_objects::require_sealed_at(tr, &expected.object).await
}

impl ManagedCatalog {
    pub async fn queue_node_rename_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeRenameRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        self.queue_logical_node_request(route, owner, request, max_bytes)
            .await
    }
    pub async fn node_rename_task_request(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<(NodeMutationTask, NodeRenameRequest)> {
        validate_physical_serial(serial)?;
        valid_identity(epoch, operation)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        retained(&tr, namespace, epoch).await?;
        let task = task_at(&tr, serial, namespace, epoch, operation)
            .await?
            .ok_or(CatalogError::Invalid("rename 任务尚未登记"))?;
        if task.kind != NodeMutationTaskKind::Rename {
            return Err(CatalogError::Invalid("原任务不是 rename"));
        }
        drop(tr);
        let request = self
            .load_node_rename_request(route, owner, &task.object, max_bytes)
            .await?;
        if !matches_request(&task, &request, &task.object) {
            return Err(CatalogError::Corrupt("rename 任务与封口原请求不符"));
        }
        Ok((task, request))
    }
    pub async fn prepare_node_rename_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        let (_, request) = self
            .node_rename_task_request(serial, route, owner, epoch, operation, max_bytes)
            .await?;
        self.prepare_logical_node_request(route, owner, &request, max_bytes)
            .await
    }
    pub async fn cancel_node_rename_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<NodeMutationTask> {
        self.finish_unwritten_node_task(
            serial,
            route,
            owner,
            epoch,
            operation,
            (NodeMutationTaskKind::Rename, None),
        )
        .await
    }
    pub async fn reject_node_rename_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        reason: &str,
    ) -> Result<NodeMutationTask> {
        self.finish_unwritten_node_task(
            serial,
            route,
            owner,
            epoch,
            operation,
            (NodeMutationTaskKind::Rename, Some(reason)),
        )
        .await
    }
}

pub(in crate::managed_catalog::control) async fn require_rename_held_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    expected: &NodeMutationTask,
    request: &NodeRenameRequest,
) -> Result<()> {
    let current = known_at(tr, route, owner, request, &expected.object)
        .await?
        .ok_or(CatalogError::Invalid("rename 原任务尚未登记"))?;
    if expected.kind != NodeMutationTaskKind::Rename
        || current.kind != NodeMutationTaskKind::Rename
        || !matches!(
            current.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        )
        || expected.library_serial != current.library_serial
        || expected.namespace != current.namespace
        || expected.pool != current.pool
        || expected.request_epoch != current.request_epoch
        || expected.operation != current.operation
        || expected.object != current.object
        || expected.fingerprint != current.fingerprint
        || expected.accepted_owner_epoch != current.accepted_owner_epoch
        || expected.created_at_millis != current.created_at_millis
        || expected.prepared_owner_epoch != current.prepared_owner_epoch
        || expected.prepared_at_millis != current.prepared_at_millis
        || expected.prepared_owner_epoch.is_none()
        || held_task_at(tr, current.namespace, current.pool)
            .await?
            .as_ref()
            != Some(&current)
    {
        return Err(CatalogError::Invalid("rename 原持有任务与封口输入不符"));
    }
    node_plan_objects::require_sealed_at(tr, &expected.object).await
}
