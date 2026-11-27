//! Management-only endpoints for replicated pool creation and import evidence.
use super::*;
use crate::replicated_pool::{CreateReplicatedPool, ResolveReplicatedPoolConflict};
use uuid::Uuid;

fn admin_error(error: CatalogError) -> Response {
    match error.domain() {
        CatalogError::ReplicatedSelectionBlocked(details) => reply(
            StatusCode::CONFLICT,
            json!({"error":"member_selection_blocked","member":details}),
        ),
        CatalogError::IdempotencyMismatch => reply(
            StatusCode::CONFLICT,
            json!({"error":"request_conflict","detail":"多副本池已保存同一身份的不同请求"}),
        ),
        CatalogError::PoolImporting => reply(
            StatusCode::CONFLICT,
            json!({"error":"replicated_pool_pending","detail":error.domain().to_string()}),
        ),
        _ => pools::failure(error),
    }
}

fn value_response(result: crate::managed_catalog::Result<Option<Value>>) -> Response {
    match result {
        Ok(Some(value)) => reply(StatusCode::OK, value),
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"多副本池不存在"})),
        Err(error) => admin_error(error),
    }
}

fn operation(raw: &str) -> Result<Uuid, &'static str> {
    Uuid::parse_str(raw)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == raw)
        .ok_or("多副本池 UUID 须规范且非零")
}

pub(super) async fn create(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    let _permit = match pools::budget(&ctx) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    if ctx.owner.is_none() {
        return unavailable("当前服务未配置多副本池创建");
    }
    let input: CreateReplicatedPool = match pools::body_with_limit(request, 16 * 1024).await {
        Ok(input) => input,
        Err(response) => return *response,
    };
    match ctx.catalog.admit_replicated_pool(&input, &ctx.routes).await {
        Ok(value) => reply(
            if matches!(
                value["status"].as_str(),
                Some("running" | "waiting_library" | "result_unknown")
            ) {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            value,
        ),
        Err(error) => admin_error(error),
    }
}

pub(super) async fn status(State(ctx): State<Arc<Context>>, Path(id): Path<String>) -> Response {
    let _permit = match pools::budget(&ctx) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    match operation(&id) {
        Ok(id) => value_response(ctx.catalog.replicated_pool_status(id, &ctx.routes).await),
        Err(error) => invalid(error),
    }
}

pub(super) async fn files(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _permit = match pools::budget(&ctx) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let id = match operation(&id) {
        Ok(id) => id,
        Err(error) => return invalid(error),
    };
    let limit =
        match query_value(&request, "limit").map(|value| value.unwrap_or("100").parse::<usize>()) {
            Ok(Ok(limit)) => limit,
            _ => return invalid("导入文件分页大小无效"),
        };
    let after = match query_value(&request, "after") {
        Ok(Some(value)) if value.len() <= 4096 => match URL_SAFE_NO_PAD
            .decode(value)
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
        {
            Some(path) => Some(path),
            None => return invalid("导入文件游标无效"),
        },
        Ok(None) => None,
        _ => return invalid("导入文件游标无效"),
    };
    value_response(
        ctx.catalog
            .replicated_pool_files(id, after.as_deref(), limit, &ctx.routes)
            .await,
    )
}

pub(super) async fn resolve(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _permit = match pools::budget(&ctx) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    if ctx.owner.is_none() {
        return unavailable("当前服务未配置多副本池冲突处理");
    }
    let id = match operation(&id) {
        Ok(id) => id,
        Err(error) => return invalid(error),
    };
    let input: ResolveReplicatedPoolConflict =
        match pools::body_with_limit(request, 16 * 1024).await {
            Ok(input) => input,
            Err(response) => return *response,
        };
    value_response(
        ctx.catalog
            .resolve_replicated_pool_conflict(id, &input, &ctx.routes)
            .await,
    )
}
