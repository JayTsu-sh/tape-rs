//! External clients connect here; no callback addresses or hardware I/O.
use super::*;
use crate::managed_catalog::{NotifyConnectionAction, NotifyFilter};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn scope(ctx: &Context, request: &Request) -> Result<(String, ActiveLibraryRoute), Box<Response>> {
    let serial = match query_value(request, "library") {
        Ok(Some(s)) => s.to_owned(),
        Ok(None) if ctx.routes.len() == 1 => ctx.routes.keys().next().unwrap().clone(),
        Ok(None) => return Err(bad("多个带库时通知接口必须指定 library")),
        Err(e) => return Err(bad(e)),
    };
    let route = ctx
        .routes
        .get(&serial)
        .copied()
        .ok_or_else(|| Box::new(reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}))))?;
    Ok((serial, route))
}
async fn body(request: Request) -> Result<Value, Box<Response>> {
    let bytes = axum::body::to_bytes(request.into_body(), 8192)
        .await
        .map_err(|_| bad("通知请求体超过上限"))?;
    serde_json::from_slice(&bytes).map_err(|_| bad("通知请求必须为 JSON"))
}
fn uuid(value: &Value, key: &str) -> Result<Uuid, Box<Response>> {
    value[key]
        .as_str()
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|v| !v.is_nil())
        .ok_or_else(|| bad("消费者实例或批次 UUID 无效"))
}
fn generation(request: &Request) -> Result<Uuid, Box<Response>> {
    query_value(request, "generation")
        .map_err(bad)?
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|v| !v.is_nil())
        .ok_or_else(|| bad("必须指定有效的消费者 generation"))
}
fn bad(reason: &str) -> Box<Response> {
    Box::new(invalid(reason))
}
fn fields(value: &Value, names: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|v| v.len() == names.len() && v.keys().all(|k| names.contains(&k.as_str())))
}

pub(super) async fn create(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if !fields(&value, &["filter", "from_start"]) || !value["from_start"].is_boolean() {
        return invalid("创建订阅须明确指定 filter 和 from_start，不接受外部回调地址");
    }
    let filter: NotifyFilter = match serde_json::from_value(value["filter"].clone()) {
        Ok(v) => v,
        Err(_) => return invalid("通知过滤条件无效"),
    };
    match ctx
        .catalog
        .create_notify_consumer(&serial, route, &id, &filter, value["from_start"] == true)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn show(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx.catalog.notify_consumer(&serial, route, &id).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn list(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let after = match query_value(&request, "after") {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(v)) => match v.parse() {
            Ok(v) => v,
            Err(_) => return invalid("limit 无效"),
        },
        Err(e) => return invalid(e),
    };
    match ctx
        .catalog
        .list_notify_consumers(&serial, route, after, limit)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn delete(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if !fields(&value, &["generation", "abandon_unacknowledged"])
        || value["abandon_unacknowledged"] != true
    {
        return invalid("删除订阅会放弃其未确认通知，必须显式指定 abandon_unacknowledged=true");
    }
    let generation = match uuid(&value, "generation") {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .delete_notify_consumer(&serial, route, &id, generation)
        .await
    {
        Ok(()) => reply(StatusCode::OK, json!({"deleted":true})),
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn ack(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if !fields(&value, &["generation", "delivery_id", "through"]) {
        return invalid("ACK 字段无效");
    }
    let generation = match uuid(&value, "generation") {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let delivery = match uuid(&value, "delivery_id") {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let Some(through) = value["through"].as_u64() else {
        return invalid("ACK through 无效");
    };
    match ctx
        .catalog
        .acknowledge_notify(&serial, route, &id, generation, delivery, through)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn batch(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let generation = match generation(&request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .notify_batch(&serial, route, &id, generation)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}

async fn send(writer: &mut tokio::io::DuplexStream, frame: &str) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            writer.write_all(frame.as_bytes())
        )
        .await,
        Ok(Ok(()))
    )
}
pub(super) async fn stream(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let generation = match generation(&request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let permit = match ctx.notify_streams.clone().try_acquire_owned() {
        Ok(v) => v,
        Err(_) => return unavailable("通知连接数已达上限，请稍后重连"),
    };
    if let Some(types) = match query_value(&request, "event_types") {
        Ok(v) => v,
        Err(e) => return invalid(e),
    } {
        let types = http::percent_decode(types);
        let types: Vec<String> = if types.is_empty() {
            Vec::new()
        } else {
            types.split(',').map(str::to_owned).collect()
        };
        if let Err(error) = ctx
            .catalog
            .expand_notify_types(&serial, route, &id, generation, &types)
            .await
        {
            return catalog_error(error);
        }
    }
    let initial = match ctx
        .catalog
        .notify_batch(&serial, route, &id, generation)
        .await
    {
        Ok(v) => v,
        Err(e) => return catalog_error(e),
    };
    let token = (generation, Uuid::new_v4());
    match ctx
        .catalog
        .record_notify_connection(&serial, route, &id, token, NotifyConnectionAction::Open)
        .await
    {
        Ok(true) => {}
        Ok(false) => return invalid("消费者已删除或重建"),
        Err(e) => return catalog_error(e),
    }
    let (mut writer, reader) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _permit = permit;
        let mut batch = initial;
        let mut last = Value::Null;
        let mut ticks = 0_u8;
        if !send(&mut writer, "retry: 1000\n: connected\n\n").await {
            let _ = ctx
                .catalog
                .record_notify_connection(
                    &serial,
                    route,
                    &id,
                    token,
                    NotifyConnectionAction::Closed,
                )
                .await;
            return;
        }
        loop {
            if !batch["delivery_id"].is_null() && batch["delivery_id"] != last {
                let frame = format!(
                    "event: notify\nid: {}\ndata: {batch}\n\n",
                    batch["delivery_id"].as_str().unwrap()
                );
                if !send(&mut writer, &frame).await {
                    break;
                }
                if !matches!(
                    ctx.catalog
                        .record_notify_connection(
                            &serial,
                            route,
                            &id,
                            token,
                            NotifyConnectionAction::Sent
                        )
                        .await,
                    Ok(true)
                ) {
                    break;
                }
                last = batch["delivery_id"].clone();
            }
            if ticks.is_multiple_of(10)
                && (!matches!(
                    ctx.catalog
                        .record_notify_connection(
                            &serial,
                            route,
                            &id,
                            token,
                            NotifyConnectionAction::Heartbeat
                        )
                        .await,
                    Ok(true)
                ) || !send(&mut writer, ": heartbeat\n\n").await)
            {
                break;
            }
            ticks = ticks.wrapping_add(1);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match ctx
                .catalog
                .notify_batch(&serial, route, &id, generation)
                .await
            {
                Ok(v) => batch = v,
                Err(error) => {
                    let _ = send(
                        &mut writer,
                        &format!(
                            "event: notify-error\ndata: {}\n\n",
                            json!({"error":error.to_string()})
                        ),
                    )
                    .await;
                    break;
                }
            }
        }
        let _ = ctx
            .catalog
            .record_notify_connection(&serial, route, &id, token, NotifyConnectionAction::Closed)
            .await;
    });
    let mut response = Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        "x-accel-buffering",
        axum::http::HeaderValue::from_static("no"),
    );
    response
}

pub(super) async fn inject(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let bytes = match axum::body::to_bytes(request.into_body(), 16 * 1024).await {
        Ok(v) => v,
        Err(_) => return invalid("注入请求体超过上限"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return invalid("注入请求必须为JSON"),
    };
    if !fields(&value, &["operation", "event_type", "data"]) {
        return invalid("注入仅接受operation、event_type和data");
    }
    let operation = match uuid(&value, "operation") {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let Some(kind) = value["event_type"].as_str() else {
        return invalid("事件类型无效");
    };
    match ctx
        .catalog
        .inject_notification(&serial, route, operation, kind, &value["data"])
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => catalog_error(e),
    }
}
