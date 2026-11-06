use super::*;
use crate::managed_catalog::{ActivePoolCreation, PoolPolicy, PoolRecord};
use uuid::Uuid;

fn pool_json(pool: &PoolRecord) -> Value {
    pool.to_json()
}

fn scope_json(serial: &str, route: ActiveLibraryRoute, pool: &PoolRecord) -> Value {
    json!({"library_serial":serial,"namespace":route.route.namespace.to_string(),"pool":pool_json(pool)})
}

fn only_query(request: &Request, fields: &[&str]) -> Result<(), &'static str> {
    for item in request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|item| !item.is_empty())
    {
        if !item
            .split_once('=')
            .is_some_and(|(key, _)| fields.contains(&key))
        {
            return Err("池查询参数无效");
        }
    }
    Ok(())
}

pub(super) async fn list(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}));
    };
    if let Err(e) = only_query(&request, &["limit", "after"]) {
        return invalid(e);
    }
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(value)) => match value.parse::<usize>() {
            Ok(n @ 1..=100) => n,
            _ => return invalid("池分页大小须在 1..=100"),
        },
        Err(e) => return invalid(e),
    };
    let after = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(value)) if value.len() <= 1024 => {
            let decoded = URL_SAFE_NO_PAD
                .decode(value)
                .ok()
                .and_then(|v| serde_json::from_slice::<(u8, String, String, Vec<u8>)>(&v).ok());
            match decoded {
                Some((1, s, n, cursor))
                    if s == serial && n == route.route.namespace.to_string() =>
                {
                    Some(cursor)
                }
                _ => return invalid("池分页游标与带库或命名空间不符"),
            }
        }
        _ => return invalid("池分页游标无效"),
    };
    match ctx
        .catalog
        .list_active_pools_for_route(&serial, route, after.as_deref(), limit)
        .await
    {
        Ok(page) => reply(
            StatusCode::OK,
            json!({"library_serial":serial,"namespace":route.route.namespace.to_string(),"limit":limit,
            "items":page.items.iter().map(pool_json).collect::<Vec<_>>(),
            "next":page.next.map(|cursor| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1,&serial,route.route.namespace.to_string(),cursor)).expect("cursor encodes")))}),
        ),
        Err(e) => catalog_error(e),
    }
}

fn pool_id(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == value)
}

pub(super) async fn show(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}));
    };
    if request.uri().query().is_some() {
        return invalid("池详情不接受查询参数");
    }
    let Some(id) = pool_id(&id) else {
        return invalid("池 UUID 无效或不规范");
    };
    match ctx.catalog.active_pool_for_route(&serial, route, id).await {
        Ok(Some(pool)) => reply(StatusCode::OK, scope_json(&serial, route, &pool)),
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"池不存在"})),
        Err(e) => catalog_error(e),
    }
}

pub(super) async fn create(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}));
    };
    let Some(id) = pool_id(&id) else {
        return invalid("池 UUID 无效或不规范");
    };
    if ctx.upload_budget.is_none() || request.uri().query().is_some() {
        return invalid("创建池须启用 FDB writer，且不接受查询参数");
    }
    let Some(owner) = &ctx.owner else {
        return unavailable("未配置 owner");
    };
    let Some(library) = owner.libraries.get(&serial) else {
        return unavailable("带库 owner 未配置");
    };
    let Ok(permit) = owner.requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    let status = library
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(executor) = status.executor.as_ref().filter(|e| {
        !status.shutting_down
            && !status.catalog_pending
            && status.executor_ready
            && e.fenced
            && e.node != 0
            && status.catalog_frozen_index == Some(route.frozen_index)
    }) else {
        return unavailable("带库 owner 尚未完成恢复");
    };
    if executor.node != status.id {
        if request.headers().contains_key("x-tapers-forwarded") {
            return unavailable("owner 已变化，请重试同一池请求");
        }
        let Some(address) = owner.peers.get(&executor.node) else {
            return unavailable("owner HTTP 地址未配置");
        };
        return match tokio::time::timeout(
            library.wait_timeout,
            gateway::forward_request(request, address, permit, &serial),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => reply(
                StatusCode::GATEWAY_TIMEOUT,
                json!({"error":"pool_create_indeterminate","detail":"请使用同一 namespace 与池 UUID 查询或重试"}),
            ),
        };
    }
    let round = executor.round;
    if library.files.serving_deadline(round).is_none() {
        return unavailable("多数派 Serving 许可已失效");
    }
    let bytes = match axum::body::to_bytes(request.into_body(), 4096).await {
        Ok(bytes) => bytes,
        Err(_) => return invalid("池创建请求体无效或超过 4 KiB"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return invalid("池创建请求不是 JSON"),
    };
    if value.to_string().as_bytes() != bytes.as_ref()
        || !value.as_object().is_some_and(|obj| {
            obj.len() == 3
                && obj
                    .keys()
                    .all(|k| ["namespace", "name", "file_limit"].contains(&k.as_str()))
        })
    {
        return invalid("池创建请求须为规范 JSON，且只含 namespace、name、file_limit");
    }
    if value["namespace"].as_str() != Some(route.route.namespace.to_string().as_str()) {
        return reply(
            StatusCode::CONFLICT,
            json!({"error":"namespace_mismatch","detail":"创建请求的命名空间已改变"}),
        );
    }
    let Some(name) = value["name"].as_str() else {
        return invalid("池名称无效");
    };
    let Some(file_limit) = value["file_limit"].as_u64() else {
        return invalid("池文件数上限无效");
    };
    let pool = PoolRecord {
        id,
        library_serial: serial.clone(),
        name: name.into(),
        file_limit,
        created_at_millis: chrono::Utc::now().timestamp_millis() as u64,
        policy: PoolPolicy::default(),
        revision: 1,
    };
    if library.files.serving_deadline(round).is_none() {
        return unavailable("多数派 Serving 许可已失效");
    }
    let result = ctx
        .catalog
        .create_active_pool(&serial, route, round, &pool)
        .await;
    if library.files.serving_deadline(round).is_none() {
        return reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"error":"pool_create_indeterminate","detail":"许可已失效，请查询同一池 UUID"}),
        );
    }
    match result {
        Ok(ActivePoolCreation::Created(pool)) => {
            reply(StatusCode::CREATED, scope_json(&serial, route, &pool))
        }
        Ok(ActivePoolCreation::AlreadyExists(pool)) => {
            reply(StatusCode::OK, scope_json(&serial, route, &pool))
        }
        Ok(ActivePoolCreation::IdConflict) => reply(
            StatusCode::CONFLICT,
            json!({"error":"pool_id_conflict","detail":"池 UUID 已由不同属性使用"}),
        ),
        Ok(ActivePoolCreation::NameConflict) => reply(
            StatusCode::CONFLICT,
            json!({"error":"pool_name_conflict","detail":"池名称已被使用"}),
        ),
        Ok(ActivePoolCreation::Retired) => reply(
            StatusCode::CONFLICT,
            json!({"error":"pool_retired","detail":"池 UUID 已退役，请使用新的 UUID"}),
        ),
        Err(e) => catalog_error(e),
    }
}

async fn change(
    ctx: Arc<Context>,
    serial: String,
    id: String,
    request: Request,
    kind: &str,
) -> Response {
    let Some(id) = pool_id(&id) else {
        return invalid("池 UUID 无效或不规范");
    };
    let (parts, body) = request.into_parts();
    let prepared = match management::body(Request::new(body)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if prepared.value()["library"] != serial
        || prepared.value()["change"]["kind"] != kind
        || prepared.value()["change"]["id"] != id.to_string()
    {
        return invalid("池管理原请求与 URL 的带库、池或操作类型不符");
    }
    management::admit(
        State(ctx),
        Path(serial),
        Request::from_parts(parts, Body::from(prepared.value().to_string())),
    )
    .await
}
pub(super) async fn delete(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    change(ctx, serial, id, request, "pool-delete").await
}
pub(super) async fn update(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    change(ctx, serial, id, request, "pool").await
}
