#![allow(clippy::result_large_err)]
//! Durable metadata operations. Admission only queues; the device actor applies
//! changes between media requests and resumes pending work after takeover.
use super::*;
use crate::managed_catalog::ManagementRequest;

fn scope(serial: &str, route: ActiveLibraryRoute) -> Value {
    json!({"library_serial":serial,"namespace":route.route.namespace.to_string()})
}
pub(super) fn route(ctx: &Context, serial: &str) -> Result<ActiveLibraryRoute, Response> {
    ctx.routes
        .get(serial)
        .copied()
        .ok_or_else(|| reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"})))
}
pub(super) fn header_scope(request: &Request, route: ActiveLibraryRoute) -> Result<(), Response> {
    if request
        .headers()
        .get_all("x-tapers-catalog-namespace")
        .iter()
        .count()
        != 1
        || request
            .headers()
            .get("x-tapers-catalog-namespace")
            .and_then(|v| v.to_str().ok())
            != Some(route.route.namespace.to_string().as_str())
    {
        return Err(reply(
            StatusCode::CONFLICT,
            json!({"error":"请求 namespace 缺失、重复或已改变"}),
        ));
    }
    Ok(())
}
pub(super) async fn body(request: Request) -> Result<ManagementRequest, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), 4096)
        .await
        .map_err(|_| invalid("管理请求体无效或超过 4 KiB"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid("管理请求不是 JSON"))?;
    if value.to_string().as_bytes() != bytes.as_ref() {
        return Err(invalid("管理请求须为规范 JSON，禁止重复字段"));
    }
    ManagementRequest::parse(value).map_err(catalog_error)
}
pub(super) struct Local {
    pub(super) library: Arc<HttpContext>,
    pub(super) round: u64,
    _permit: OwnedSemaphorePermit,
    pub(super) request: Request,
}
pub(super) async fn writer(
    ctx: &Context,
    serial: &str,
    route: ActiveLibraryRoute,
    request: Request,
) -> Result<Local, Response> {
    header_scope(&request, route)?;
    if ctx.upload_budget.is_none() {
        return Err(reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"请求写入需要 FDB writer"}),
        ));
    }
    let owner = ctx
        .owner
        .as_ref()
        .ok_or_else(|| unavailable("未配置 owner"))?;
    let library = owner
        .libraries
        .get(serial)
        .ok_or_else(|| unavailable("带库 owner 未配置"))?
        .clone();
    let permit = ctx
        .admin_requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| unavailable("http_busy"))?;
    let status = library
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let executor = status
        .executor
        .as_ref()
        .filter(|e| {
            !status.shutting_down
                && !status.catalog_pending
                && status.executor_ready
                && e.fenced
                && e.node != 0
                && status.catalog_frozen_index == Some(route.frozen_index)
        })
        .ok_or_else(|| unavailable("带库 owner 尚未完成恢复"))?;
    if executor.node != status.id {
        if request.headers().contains_key("x-tapers-forwarded") {
            return Err(unavailable("owner 已变化，请查询原请求"));
        }
        let address = owner
            .peers
            .get(&executor.node)
            .ok_or_else(|| unavailable("owner HTTP 地址未配置"))?;
        return Err(
            match tokio::time::timeout(
                library.wait_timeout,
                gateway::forward_request(request, address, permit, serial),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => reply(
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"error":"请求结果未定，请查询原请求身份"}),
                ),
            },
        );
    }
    let round = executor.round;
    if library.files.serving_deadline(round).is_none() {
        return Err(unavailable("多数派 Serving 许可已失效"));
    }
    Ok(Local {
        library,
        round,
        _permit: permit,
        request,
    })
}

pub(super) async fn prepare(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(route) => route,
        Err(r) => return r,
    };
    if let Err(error) = header_scope(&request, route) {
        return error;
    }
    let request = match body(request).await {
        Ok(request) => request,
        Err(r) => return r,
    };
    if let Err(error) = ManagementRequest::parse(request.value().clone()) {
        return catalog_error(error);
    }
    if request.namespace() != route.route.namespace || request.value()["library"] != serial {
        return invalid("管理请求与带库或 namespace 不符");
    }
    let window = match ctx.catalog.active_management_window(&serial, route).await {
        Ok(window) => window,
        Err(e) => return catalog_error(e),
    };
    if request.epoch() != window.accepting {
        return reply(StatusCode::CONFLICT, json!({"error":"管理请求代次已改变"}));
    }
    let mut value = scope(&serial, route);
    value["request"] = request.value().clone();
    reply(StatusCode::OK, value)
}
pub(super) async fn admit(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(route) => route,
        Err(r) => return r,
    };
    let local = match writer(&ctx, &serial, route, request).await {
        Ok(local) => local,
        Err(r) => return r,
    };
    let request = match body(local.request).await {
        Ok(request) => request,
        Err(r) => return r,
    };
    if local.library.files.serving_deadline(local.round).is_none() {
        return unavailable("Serving 许可已失效");
    }
    match ctx
        .catalog
        .admit_management(&serial, route, local.round, &request)
        .await
    {
        Ok(value) => reply(
            if value["status"] == "queued" {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            value,
        ),
        Err(e) => catalog_error(e),
    }
}

pub(super) async fn policy(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let route = match route(&ctx, &serial) {
        Ok(route) => route,
        Err(r) => return r,
    };
    let kind = match query_value(&request, "kind") {
        Ok(Some(kind)) => kind,
        _ => return invalid("缺少策略 kind"),
    };
    let target = match query_value(&request, "target") {
        Ok(Some(raw)) if raw.len() <= 512 => match URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
        {
            Some(target) => target,
            None => return invalid("策略 target 无效"),
        },
        _ => return invalid("缺少策略 target"),
    };
    match ctx
        .catalog
        .active_resource_policy(&serial, route, kind, &target)
        .await
    {
        Ok(policy) => {
            let mut value = scope(&serial, route);
            value["kind"] = json!(kind);
            value["target"] = json!(target);
            value["policy"] = policy;
            reply(StatusCode::OK, value)
        }
        Err(e) => catalog_error(e),
    }
}
