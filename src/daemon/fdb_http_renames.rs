#![allow(clippy::result_large_err)]
//! Original two-path requests. Device work belongs exclusively to the owner.
use super::management::{route, writer};
use super::nodes::{body, identity, limit, scope};
use super::*;
use crate::managed_catalog::{
    NodeMutationTaskKind, NodeMutationTaskState, NodeRenameRequest, NodeRenameRevision,
    NodeRenameSpec,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    namespace: String,
    inode: String,
    version: u64,
}
impl Revision {
    fn revision(&self) -> Result<NodeRenameRevision, Response> {
        if self.version == 0 || self.version == u64::MAX {
            return Err(invalid("rename 原身份版本必须为可递增正整数"));
        }
        Ok(NodeRenameRevision {
            namespace: identity(1, &self.namespace)?,
            inode: identity(1, &self.inode)?,
            version: self.version,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    version: u8,
    pool: String,
    operation: String,
    from: String,
    to: String,
    source: Revision,
    source_parent: Revision,
    destination_parent: Revision,
    target: Option<Revision>,
    no_replace: bool,
    time: String,
}
fn revision_value(value: NodeRenameRevision) -> Value {
    json!({"namespace":value.namespace.to_string(),"inode":value.inode.to_string(),"version":value.version})
}
fn digest(value: [u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
async fn read_status(
    ctx: &Context,
    serial: &str,
    route: ActiveLibraryRoute,
    epoch: u64,
    operation: Uuid,
    accepting: bool,
) -> Response {
    let task = match ctx
        .catalog
        .node_mutation_task_status(serial, route, epoch, operation)
        .await
    {
        Ok(Some(status)) if status.task.kind == NodeMutationTaskKind::Rename => status.task,
        Ok(Some(_)) => return invalid("原任务不是 rename，请使用对应请求类型"),
        Ok(None) => {
            return reply(
                StatusCode::NOT_FOUND,
                json!({"error":"rename 请求尚无记录"}),
            );
        }
        Err(error) => return catalog_error(error),
    };
    let result = if task.state == NodeMutationTaskState::Completed {
        let publication = match ctx
            .catalog
            .node_rename_publication(serial, route, epoch, operation)
            .await
        {
            Ok(Some(value)) => value,
            Ok(None) => {
                return catalog_error(CatalogError::Corrupt("Completed rename 缺少原两路径回执"));
            }
            Err(error) => return catalog_error(error),
        };
        // A completed task is immutable. The second read must still agree
        // with the first task's original identity, result and first audit.
        let request = publication.request();
        if request.library() != serial
            || request.spec().source.namespace != task.namespace
            || request.spec().pool != task.pool
            || request.request_epoch() != epoch
            || request.spec().operation != operation
            || request.fingerprint() != task.fingerprint
            || Some(publication.fingerprint()) != task.publication_digest
            || Some(publication.completed_owner()) != task.completed_owner_epoch
            || Some(publication.completed_at_millis()) != task.completed_at_millis
        {
            return catalog_error(CatalogError::Corrupt("rename 原任务与两路径终态不符"));
        }
        Some(json!({"source":revision_value(publication.source()),
            "source_parent":revision_value(publication.source_parent()),
            "destination_parent":revision_value(publication.destination_parent()),
            "replaced":publication.replaced().map(revision_value),
            "directory":publication.is_directory(),"group":publication.group().to_string(),
            "attributes_digest":digest(publication.attributes_digest()),
            "source_parent_attributes_digest":digest(publication.source_parent_attributes_digest()),
            "destination_parent_attributes_digest":digest(publication.destination_parent_attributes_digest()),
            "publication_digest":digest(publication.fingerprint())}))
    } else {
        None
    };
    let pending = matches!(
        task.state,
        NodeMutationTaskState::Queued
            | NodeMutationTaskState::Prepared
            | NodeMutationTaskState::Writing
    );
    reply(
        if accepting && pending {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        json!({"version":1,"kind":"rename","library_serial":task.library_serial,"namespace":task.namespace.to_string(),
            "pool":task.pool.to_string(),"epoch":task.request_epoch,"operation":task.operation.to_string(),
            "fingerprint":digest(task.fingerprint),"status":task.state,
            "accepted_owner_epoch":task.accepted_owner_epoch,"created_at_millis":task.created_at_millis,
            "prepared_owner_epoch":task.prepared_owner_epoch,"prepared_at_millis":task.prepared_at_millis,
            "completed_owner_epoch":task.completed_owner_epoch,"completed_at_millis":task.completed_at_millis,
            "rejection_reason":task.rejection_reason,"result":result}),
    )
}

pub(super) async fn prepare(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = scope(&request, route) {
        return error;
    }
    let max = match limit(&ctx) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let local = match writer(&ctx, &serial, route, request).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    let bytes = match body(local.request, max, local.library.wait_timeout).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    let input: Prepare = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return invalid("rename 准备字段或格式无效"),
    };
    if input.version != 1
        || serde_json::to_value(&input)
            .expect("logical JSON")
            .to_string()
            .as_bytes()
            != bytes
    {
        return invalid("rename 准备需要完整规范 JSON，禁止重复字段");
    }
    if local.library.files.serving_deadline(local.round).is_none() {
        return unavailable("Serving 许可已失效");
    }
    let spec = match (
        identity(1, &input.pool),
        identity(1, &input.operation),
        input.source.revision(),
        input.source_parent.revision(),
        input.destination_parent.revision(),
        input.target.as_ref().map(Revision::revision).transpose(),
    ) {
        (
            Ok(pool),
            Ok(operation),
            Ok(source),
            Ok(source_parent),
            Ok(destination_parent),
            Ok(target),
        ) => NodeRenameSpec {
            pool,
            operation,
            from: input.from,
            to: input.to,
            source,
            source_parent,
            destination_parent,
            target,
            no_replace: input.no_replace,
        },
        _ => return invalid("rename 原双路径身份字段无效"),
    };
    if [spec.source, spec.source_parent, spec.destination_parent]
        .iter()
        .any(|revision| revision.namespace != route.route.namespace)
        || spec
            .target
            .is_some_and(|target| target.namespace != route.route.namespace)
    {
        return invalid("rename 原身份与当前 namespace 不符");
    }
    let prepared = match ctx
        .catalog
        .prepare_node_rename_request(&serial, route, local.round, &spec, &input.time)
        .await
    {
        Ok(value) => value,
        Err(error) => return catalog_error(error),
    };
    let bytes = match prepared.encode() {
        Ok(value) if value.len() <= max => value,
        Ok(_) => return invalid("rename 规范请求超过正文预算"),
        Err(error) => return catalog_error(error),
    };
    reply(
        StatusCode::OK,
        json!({"version":1,"kind":"rename","library_serial":serial,"namespace":route.route.namespace.to_string(),
        "request":serde_json::from_slice::<Value>(&bytes).expect("validated rename"),"fingerprint":digest(prepared.fingerprint())}),
    )
}

pub(super) async fn admit(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = scope(&request, route) {
        return error;
    }
    let max = match limit(&ctx) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let local = match writer(&ctx, &serial, route, request).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    let bytes = match body(local.request, max, local.library.wait_timeout).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    let original = match NodeRenameRequest::decode(&bytes) {
        Ok(value) => value,
        Err(error) => return catalog_error(error),
    };
    if original.library() != serial || original.spec().source.namespace != route.route.namespace {
        return invalid("rename 原请求与带库或 namespace 不符");
    }
    if local.library.files.serving_deadline(local.round).is_none() {
        return unavailable("Serving 许可已失效");
    }
    match ctx
        .catalog
        .queue_node_rename_request(route, local.round, &original, max)
        .await
    {
        Ok(_) => {
            read_status(
                &ctx,
                &serial,
                route,
                original.request_epoch(),
                original.spec().operation,
                true,
            )
            .await
        }
        Err(error) => catalog_error(error),
    }
}
pub(super) async fn status(
    State(ctx): State<Arc<Context>>,
    Path((serial, epoch, operation)): Path<(String, u64, String)>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = scope(&request, route) {
        return error;
    }
    let operation = match identity(epoch, &operation) {
        Ok(value) => value,
        Err(error) => return error,
    };
    read_status(&ctx, &serial, route, epoch, operation, false).await
}
pub(super) async fn cancel(
    State(ctx): State<Arc<Context>>,
    Path((serial, epoch, operation)): Path<(String, u64, String)>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = scope(&request, route) {
        return error;
    }
    let operation = match identity(epoch, &operation) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let local = match writer(&ctx, &serial, route, request).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    match ctx
        .catalog
        .cancel_node_rename_task(&serial, route, local.round, epoch, operation)
        .await
    {
        Ok(_) => read_status(&ctx, &serial, route, epoch, operation, false).await,
        Err(error) => catalog_error(error),
    }
}
