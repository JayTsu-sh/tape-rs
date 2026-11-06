//! Strict loaded-volume discovery before generic single-node/file recovery.
use super::*;
impl ManagedCatalog {
    /// Return the original held rename task affecting this assigned pool. The
    /// typed shared slot, when present, must agree; released Publishing slots
    /// are validated as absent by the original group, never recreated.
    pub async fn pending_node_rename_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        volume: Uuid,
    ) -> Result<Option<NodeMutationTask>> {
        if volume.is_nil() {
            return Err(invalid("rename 待恢复卷身份为空"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let Some(placement) = verified_placement(&tr, volume).await? else {
            return Ok(None);
        };
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(invalid("rename 待恢复卷与活动带库不符"));
        }
        let held = node_tasks::held_task_at(&tr, namespace, placement.pool_id).await?;
        match tr.get(&node_attempts::volume_key(volume), false).await? {
            Some(raw) => match node_claims::decode(&raw)? {
                node_claims::NodeVolumeClaim::Rename(value) => {
                    require_prepared_volume_claim_at(&tr, &placement, &value).await?;
                    let task = held.as_ref().ok_or_else(invalid_attempt)?;
                    if task.kind != NodeMutationTaskKind::Rename
                        || task.request_epoch != value.epoch
                        || task.operation != value.operation
                    {
                        return Err(invalid_attempt());
                    }
                }
                node_claims::NodeVolumeClaim::Single(_) => {
                    if held
                        .as_ref()
                        .is_some_and(|task| task.kind == NodeMutationTaskKind::Rename)
                    {
                        return Err(invalid_attempt());
                    }
                    return Ok(None);
                }
            },
            None => {
                if let Some(task) = &held {
                    match require_no_missing_prepared_claim_at(&tr, &placement, task).await {
                        Ok(()) | Err(CatalogError::PoolBusy) => {}
                        Err(error) => return Err(error),
                    }
                    // PoolBusy here is the verifier's exact valid released-page
                    // case. This returns original task data, never an I/O permit.
                }
            }
        }
        let result = held.filter(|task| task.kind == NodeMutationTaskKind::Rename);
        if let Some(task) = &result {
            node_tasks::retained(&tr, namespace, task.request_epoch).await?;
        }
        Ok(result)
    }
}
