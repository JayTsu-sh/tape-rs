use super::*;
use crate::managed_catalog::{LibraryHandoff, LibraryHandoffPhase};
use uuid::Uuid;

fn receipt(value: LibraryHandoff) -> Response {
    let status = match value.phase {
        LibraryHandoffPhase::Releasing | LibraryHandoffPhase::Loading => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    };
    reply(status, json!(value))
}

pub(super) async fn status(
    State(ctx): State<Arc<Context>>,
    Path((serial, operation)): Path<(String, Uuid)>,
    request: Request,
) -> Response {
    let route = match management::route(&ctx, &serial) {
        Ok(route) => route,
        Err(error) => return error,
    };
    if let Err(error) = management::header_scope(&request, route) {
        return error;
    }
    match ctx
        .catalog
        .library_handoff(&serial, route, Some(operation))
        .await
    {
        Ok(Some(value)) => reply(StatusCode::OK, json!(value)),
        Ok(None) => reply(
            StatusCode::NOT_FOUND,
            json!({"error":"带库交还或加载请求不存在"}),
        ),
        Err(error) => catalog_error(error),
    }
}

pub(super) async fn submit(
    State(ctx): State<Arc<Context>>,
    Path((serial, operation)): Path<(String, Uuid)>,
    request: Request,
) -> Response {
    let route = match management::route(&ctx, &serial) {
        Ok(route) => route,
        Err(error) => return error,
    };
    if let Err(error) = management::header_scope(&request, route) {
        return error;
    }
    if operation.is_nil() {
        return invalid("操作 ID 不能为零");
    }
    if ctx.upload_budget.is_none()
        || !ctx
            .owner
            .as_ref()
            .is_some_and(|o| o.libraries.contains_key(&serial))
    {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"带库交还与加载需要 FDB writer"}),
        );
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1024).await else {
        return invalid("交还或加载请求体过长");
    };
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Change {
        release: Option<Uuid>,
    }
    let Ok(change) = serde_json::from_slice::<Change>(&bytes) else {
        return invalid("请求必须是交还或加载 JSON 对象");
    };
    if change
        .release
        .is_some_and(|id| id.is_nil() || id == operation)
    {
        return invalid("加载须引用不同的有效交还回执 ID");
    }
    match ctx
        .catalog
        .library_handoff(&serial, route, Some(operation))
        .await
    {
        Ok(Some(value)) if value.release == change.release => {
            if value.phase != LibraryHandoffPhase::Blocked {
                return receipt(value);
            }
        }
        Ok(Some(_)) => return invalid("操作 ID 已由不同请求使用"),
        Ok(None) => {}
        Err(error) => return catalog_error(error),
    }
    if let Some(release) = change.release {
        return match ctx
            .catalog
            .begin_library_reload(&serial, route, operation, release)
            .await
        {
            Ok(value) => receipt(value),
            Err(error) => catalog_error_domain(error),
        };
    }
    let request = Request::from_parts(parts, Body::from(bytes));
    let local = match management::writer(&ctx, &serial, route, request).await {
        Ok(local) => local,
        Err(response) => return response,
    };
    let library = &local.library;
    let round = local.round;
    if library.files.device_busy(round) {
        return reply(
            StatusCode::CONFLICT,
            json!({"operation":operation,"status":"busy","detail":"带库正在执行设备操作"}),
        );
    }
    let timeout = library.wait_timeout;
    let deadline = std::time::Instant::now() + timeout;
    let (tx, rx) = std::sync::mpsc::channel();
    if library
        .exec
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(ExecRequest::LibraryHandoff {
            round,
            operation,
            deadline,
            reply: tx,
        })
        .is_err()
    {
        return unavailable("带库执行器不可用");
    }
    match tokio::task::spawn_blocking(move || rx.recv_timeout(timeout)).await {
        Ok(Ok(Ok(value))) => receipt(value),
        Ok(Ok(Err(ReadError::Busy(detail)))) => {
            if let Ok(Some(value)) = ctx
                .catalog
                .library_handoff(&serial, route, Some(operation))
                .await
                && value.release.is_none()
            {
                return receipt(value);
            }
            reply(
                StatusCode::CONFLICT,
                json!({"operation":operation,"status":"busy","detail":detail}),
            )
        }
        Ok(Ok(Err(error))) => unavailable(&error.to_string()),
        _ => reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"operation":operation,"status":"unknown","detail":"请查询原操作回执"}),
        ),
    }
}

fn catalog_error_domain(error: CatalogError) -> Response {
    match error.domain() {
        CatalogError::Invalid(detail) => invalid(detail),
        _ => catalog_error(error),
    }
}
