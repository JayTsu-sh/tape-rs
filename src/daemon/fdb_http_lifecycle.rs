//! Operational device commands bind the HTTP hierarchy to one durable request.
use super::*;

async fn action(
    ctx: Arc<Context>,
    serial: String,
    pool: Option<String>,
    barcode: Option<String>,
    kind: &str,
    request: Request,
) -> Response {
    if ctx.upload_budget.is_none() {
        return reply(StatusCode::METHOD_NOT_ALLOWED, json!({"error":"read_only"}));
    }
    if !ctx.routes.contains_key(&serial) {
        return reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}));
    }
    let pool = if let Some(pool) = pool {
        match inventory::pool_scope(&ctx, &serial, &pool).await {
            Ok(id) => Some(id),
            Err(e) => return *e,
        }
    } else {
        None
    };
    let (parts, body) = request.into_parts();
    let prepared = match management::body(Request::new(body)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let value = prepared.value();
    if value["library"] != serial
        || value["change"]["kind"] != kind
        || barcode
            .as_deref()
            .is_some_and(|barcode| value["change"]["barcode"] != barcode)
    {
        return invalid("设备管理请求的库、条码或操作与 URL 不符");
    }
    if let Some(pool) = pool {
        if value["change"]["pool"] != pool.to_string() {
            return invalid("设备管理请求与 URL 的池不符");
        }
    } else if barcode.is_some() && !value["change"]["pool"].is_null() {
        return invalid("已分池介质必须使用其 pool 层级管理入口");
    }
    management::admit(
        State(ctx),
        Path(serial),
        Request::from_parts(parts, Body::from(value.to_string())),
    )
    .await
}
pub(super) async fn online(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "media-online",
        request,
    )
    .await
}
pub(super) async fn set(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(ctx, serial, Some(pool), Some(barcode), "tape", request).await
}
pub(super) async fn offline(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let prepared = match management::body(Request::new(body)).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    if prepared.value()["change"]["patch"]["online"] != false {
        return invalid("offline 入口必须固定 online=false");
    }
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "tape",
        Request::from_parts(parts, Body::from(prepared.value().to_string())),
    )
    .await
}
pub(super) async fn rescan(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    action(ctx, serial, None, None, "library-rescan", request).await
}
pub(super) async fn assign(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "media-assign",
        request,
    )
    .await
}
pub(super) async fn unassign(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "media-unassign",
        request,
    )
    .await
}
pub(super) async fn validate(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "media-validate",
        request,
    )
    .await
}
pub(super) async fn move_tape(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    action(
        ctx,
        serial,
        Some(pool),
        Some(barcode),
        "media-move",
        request,
    )
    .await
}
pub(super) async fn move_unassigned(
    State(ctx): State<Arc<Context>>,
    Path((serial, barcode)): Path<(String, String)>,
    request: Request,
) -> Response {
    action(ctx, serial, None, Some(barcode), "media-move", request).await
}
