use super::*;

/// A completed logical operation owns exactly one typed result. This also
/// rejects stray results for missing, queued, prepared, writing, cancelled or rejected
/// tasks, before any retry path can mistake them for successful completion.
pub(in crate::managed_catalog) async fn task_outcome_at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    epoch: u64,
    operation: Uuid,
    task: Option<&NodeMutationTask>,
) -> Result<Option<NodeMutationTaskOutcome>> {
    let (published, deleted) = tokio::try_join!(
        publication::at(tr, namespace, serial, epoch, operation),
        deletion::at(tr, namespace, serial, epoch, operation),
    )?;
    node_rename::require_rename_task_outcome_at(tr, task, (namespace, serial, epoch, operation))
        .await?;
    if task.is_some_and(|task| task.kind == NodeMutationTaskKind::Rename) {
        if published.is_some() || deleted.is_some() {
            return Err(CatalogError::Corrupt("rename 不能持有单节点发布结果"));
        }
        return Ok(None);
    }
    let Some(task) = task.filter(|task| task.state == NodeMutationTaskState::Completed) else {
        if published.is_some() || deleted.is_some() {
            return Err(CatalogError::Corrupt("节点发布结果缺少对应的已完成任务"));
        }
        return Ok(None);
    };
    match (published, deleted) {
        (Some(value), None) => {
            publication::require_task_match(&value, task)?;
            Ok(Some(NodeMutationTaskOutcome {
                node: value.node(),
                version: value.version(),
                parent_version: value.parent_version(),
                deleted: false,
            }))
        }
        (None, Some(value)) => {
            deletion::require_task_match(&value, task)?;
            Ok(Some(NodeMutationTaskOutcome {
                node: value.node(),
                version: value.version(),
                parent_version: value.parent_version(),
                deleted: true,
            }))
        }
        _ => Err(CatalogError::Corrupt(
            "已完成节点任务要求唯一的发布或删除结果",
        )),
    }
}
