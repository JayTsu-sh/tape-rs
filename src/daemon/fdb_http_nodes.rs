#![allow(clippy::result_large_err)]
//! Logical requests only. The Serving executor selects media and owns all
//! physical attempts; HTTP never accepts a physical plan or a commit proof.
use super::management::{header_scope, route, writer};
use super::*;
use crate::managed_catalog::{
    NodeMutation, NodeMutationRequest, NodeMutationSpec, NodeMutationTaskKind,
    NodeMutationTaskState, NodeMutationTaskStatus,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// The index budget is disk capacity and may be much larger than RAM. Bound
// logical JSON independently; do not buffer an index-sized request in HTTP.
const MAX_REQUEST_BYTES: usize = 8 << 20;
const MAX_ATTRIBUTE_BYTES: usize = 8 << 20;
const MAX_ATTRIBUTE_JSON: usize = 64 << 20;

pub(super) async fn attributes(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if request.uri().query().unwrap_or("").split('&').any(|item| {
        !matches!(
            item.split_once('=').map(|(key, _)| key),
            Some("pool" | "path")
        )
    }) {
        return invalid("节点属性查询只接受 pool 和 path");
    }
    let (path, pool, _) = match content_scope(&request) {
        Ok(value) => value,
        Err(error) => return invalid(error),
    };
    let catalog_path = match crate::managed_catalog::pool_catalog_path(
        Uuid::parse_str(&pool).expect("validated pool"),
        path.as_bytes(),
    ) {
        Ok(value) => value,
        Err(error) => return catalog_error(error),
    };
    let resolved = match ctx
        .catalog
        .resolve_active_attributes_for_route(&serial, &catalog_path, route, MAX_ATTRIBUTE_BYTES)
        .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return reply(StatusCode::NOT_FOUND, json!({"error":"路径不存在"})),
        Err(error) => return catalog_error(error),
    };
    let (id, version, kind, attributes) = match resolved {
        ResolvedPath::Directory {
            id,
            version,
            attributes,
        } => (id, version, "directory", attributes),
        ResolvedPath::File {
            id,
            head,
            copy,
            attributes,
            ..
        } => (
            id,
            head.version,
            if copy
                .media
                .as_ref()
                .is_some_and(|media| media.native_symlink)
            {
                "symlink"
            } else {
                "file"
            },
            attributes,
        ),
    };
    let Some(attributes) = attributes else {
        return unavailable("节点属性尚未完整登记");
    };
    let (Some(metadata), Some(xattrs)) = (attributes.meta(), attributes.xattrs()) else {
        return unavailable("节点属性尚未完整读取");
    };
    // An EE data alias remains an ordinary logical file. Its physical target
    // is not a POSIX link exposed by the logical filesystem.
    let target = if kind == "symlink" {
        match attributes.symlink() {
            Some(value) if !value.is_empty() => Some(value),
            _ => return unavailable("原生链接缺少目标"),
        }
    } else {
        None
    };
    let value = json!({"schema_version":1,"library_serial":serial,"namespace":route.route.namespace.to_string(),
        "pool_id":pool,"path":path,"id":id.to_string(),"version":version,"kind":kind,
        "attributes_digest":attributes.digest().iter().map(|b|format!("{b:02x}")).collect::<String>(),
        "metadata":metadata,"xattrs":xattrs,"symlink_target":target});
    let bytes = value.to_string();
    if bytes.len() > MAX_ATTRIBUTE_JSON {
        return unavailable("节点属性 JSON 超过响应预算");
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        bytes,
    )
        .into_response()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    version: u8,
    pool: String,
    operation: String,
    path: String,
    expected_version: Option<u64>,
    time: String,
    mutation: NodeMutation,
}

pub(super) fn limit(ctx: &Context) -> Result<usize, Response> {
    let budget = ctx.upload_budget.as_ref().ok_or_else(|| {
        reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"节点写入需要 FDB writer"}),
        )
    })?;
    Ok(usize::try_from(budget.index_bytes)
        .unwrap_or(usize::MAX)
        .min(MAX_REQUEST_BYTES))
}

pub(super) fn scope(request: &Request, route: ActiveLibraryRoute) -> Result<(), Response> {
    if request.uri().query().is_some() {
        return Err(invalid("节点请求不接受查询参数"));
    }
    header_scope(request, route)
}

pub(super) async fn body(
    request: Request,
    limit: usize,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, Response> {
    match tokio::time::timeout(timeout, axum::body::to_bytes(request.into_body(), limit)).await {
        Ok(Ok(bytes)) => Ok(bytes.to_vec()),
        _ => Err(invalid("节点请求正文超过限制、被截断或超时")),
    }
}

pub(super) fn identity(epoch: u64, operation: &str) -> Result<Uuid, Response> {
    Uuid::parse_str(operation)
        .ok()
        .filter(|id| epoch > 0 && !id.is_nil() && id.to_string() == operation)
        .ok_or_else(|| invalid("节点请求代次或 UUID 无效"))
}

fn receipt(status: NodeMutationTaskStatus) -> Value {
    let task = status.task;
    let outcome = status.outcome.map(|value| {
        json!({"node":value.node.to_string(),"version":value.version,
            "parent_version":value.parent_version,"deleted":value.deleted})
    });
    json!({"version":1,"library_serial":task.library_serial,"namespace":task.namespace.to_string(),
        "pool":task.pool.to_string(),"epoch":task.request_epoch,"operation":task.operation.to_string(),
        "fingerprint":task.fingerprint.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "status":task.state,"accepted_owner_epoch":task.accepted_owner_epoch,
        "created_at_millis":task.created_at_millis,"prepared_owner_epoch":task.prepared_owner_epoch,
        "prepared_at_millis":task.prepared_at_millis,"completed_owner_epoch":task.completed_owner_epoch,
        "completed_at_millis":task.completed_at_millis,"rejection_reason":task.rejection_reason,
        "result":outcome})
}

async fn read_status(
    ctx: &Context,
    serial: &str,
    route: ActiveLibraryRoute,
    epoch: u64,
    operation: Uuid,
    accepting: bool,
) -> Response {
    match ctx
        .catalog
        .node_mutation_task_status(serial, route, epoch, operation)
        .await
    {
        Ok(Some(value)) => {
            if value.task.kind != NodeMutationTaskKind::SingleNode {
                return invalid("原任务不是单节点修改，请使用对应请求类型");
            }
            let pending = matches!(
                value.task.state,
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
                receipt(value),
            )
        }
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"节点请求尚无记录"})),
        Err(error) => catalog_error(error),
    }
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
        Err(_) => return invalid("节点准备字段或格式无效"),
    };
    if input.version != 1
        || serde_json::to_value(&input)
            .expect("logical JSON")
            .to_string()
            .as_bytes()
            != bytes
    {
        return invalid("节点准备需要完整的规范 JSON，禁止重复字段");
    }
    if local.library.files.serving_deadline(local.round).is_none() {
        return unavailable("Serving 许可已失效");
    }
    let pool = match identity(1, &input.pool) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let operation = match identity(1, &input.operation) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let spec = NodeMutationSpec {
        pool,
        operation,
        path: input.path,
        expected_version: input.expected_version,
        mutation: input.mutation,
    };
    let prepared = match ctx
        .catalog
        .prepare_node_mutation_request(&serial, route, local.round, &spec, &input.time)
        .await
    {
        Ok(value) => value,
        Err(error) => return catalog_error(error),
    };
    let bytes = match prepared.encode() {
        Ok(value) if value.len() <= max => value,
        Ok(_) => return invalid("规范节点请求超过正文或索引预算"),
        Err(error) => return catalog_error(error),
    };
    reply(
        StatusCode::OK,
        json!({"version":1,"library_serial":serial,"namespace":route.route.namespace.to_string(),
        "request":serde_json::from_slice::<Value>(&bytes).expect("validated request"),
        "fingerprint":prepared.fingerprint().iter().map(|byte| format!("{byte:02x}")).collect::<String>()}),
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
    let request = match NodeMutationRequest::decode(&bytes) {
        Ok(value) => value,
        Err(error) => return catalog_error(error),
    };
    let c = request.context();
    if c.library_serial != serial || c.namespace != route.route.namespace {
        return invalid("节点请求与带库或 namespace 不符");
    }
    if local.library.files.serving_deadline(local.round).is_none() {
        return unavailable("Serving 许可已失效");
    }
    match ctx
        .catalog
        .queue_node_mutation_request(route, local.round, &request, max)
        .await
    {
        Ok(_) => read_status(&ctx, &serial, route, c.request_epoch, c.operation, true).await,
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
        .cancel_node_mutation_task(&serial, route, local.round, epoch, operation)
        .await
    {
        Ok(_) => read_status(&ctx, &serial, route, epoch, operation, false).await,
        Err(error) => catalog_error(error),
    }
}
