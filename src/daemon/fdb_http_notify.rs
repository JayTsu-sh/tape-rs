//! External clients connect here; no callback addresses or hardware I/O.
use super::*;
use crate::managed_catalog::{NotifyConnectionAction, NotifyFilter};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn scope(ctx: &Context, request: &Request) -> Result<(String, ActiveLibraryRoute), Box<Response>> {
    let serial = match query_value(request, "library") {
        Ok(Some(s)) => http::percent_decode(s),
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
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    filter: NotifyFilter,
    from_start: bool,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    generation: String,
    abandon_unacknowledged: bool,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    generation: String,
    delivery_id: String,
    through: u64,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Inject {
    operation: String,
    event_type: String,
    data: Value,
}
async fn body<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, Box<Response>> {
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(request.into_body(), 16 * 1024),
    )
    .await
    .map_err(|_| {
        Box::new(reply(
            StatusCode::REQUEST_TIMEOUT,
            json!({"error":"通知请求体读取超时"}),
        ))
    })?
    .map_err(|_| bad("通知请求体超过上限或读取失败"))?;
    serde_json::from_slice(&bytes).map_err(|_| bad("通知请求JSON字段无效或重复"))
}
fn uuid(value: &str) -> Result<Uuid, Box<Response>> {
    Uuid::parse_str(value)
        .ok()
        .filter(|v| !v.is_nil() && v.to_string() == value)
        .ok_or_else(|| bad("消费者实例或批次UUID必须是规范的非零UUID"))
}
fn permit(ctx: &Context) -> Result<OwnedSemaphorePermit, Box<Response>> {
    ctx.notify_requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| Box::new(unavailable("通知请求数已达上限，请稍后重试")))
}
fn domain_error(mut error: &CatalogError) -> &CatalogError {
    while let CatalogError::Fdb(foundationdb::FdbBindingError::CustomError(inner)) = error {
        match inner.downcast_ref::<CatalogError>() {
            Some(next) => error = next,
            None => break,
        }
    }
    error
}
fn retryable(error: &CatalogError) -> bool {
    matches!(
        domain_error(error),
        CatalogError::Fdb(_) | CatalogError::Client(_) | CatalogError::Incomplete
    )
}
fn notify_error(error: CatalogError) -> Response {
    let error = domain_error(&error);
    let status = if matches!(error, CatalogError::Invalid(_)) {
        StatusCode::BAD_REQUEST
    } else if retryable(error) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    reply(
        status,
        json!({"error":error.to_string(),"retryable":retryable(error)}),
    )
}
fn generation(request: &Request) -> Result<Uuid, Box<Response>> {
    let value = query_value(request, "generation")
        .map_err(bad)?
        .ok_or_else(|| bad("必须指定消费者generation"))?;
    uuid(value)
}
fn bad(reason: &str) -> Box<Response> {
    Box::new(invalid(reason))
}
pub(super) async fn create(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body::<Create>(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .create_notify_consumer(&serial, route, &id, &value.filter, value.from_start)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => notify_error(e),
    }
}
pub(super) async fn show(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx.catalog.notify_consumer(&serial, route, &id).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => notify_error(e),
    }
}
pub(super) async fn list(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
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
        Err(e) => notify_error(e),
    }
}
pub(super) async fn delete(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body::<Delete>(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if !value.abandon_unacknowledged {
        return invalid("删除订阅会放弃其未确认通知，必须显式指定 abandon_unacknowledged=true");
    }
    let generation = match uuid(&value.generation) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .delete_notify_consumer(&serial, route, &id, generation)
        .await
    {
        Ok(()) => reply(StatusCode::OK, json!({"deleted":true})),
        Err(e) => notify_error(e),
    }
}
pub(super) async fn ack(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body::<Ack>(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let generation = match uuid(&value.generation) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let delivery = match uuid(&value.delivery_id) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let through = value.through;
    match ctx
        .catalog
        .acknowledge_notify(&serial, route, &id, generation, delivery, through)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => notify_error(e),
    }
}
pub(super) async fn batch(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
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
        Err(e) => notify_error(e),
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
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
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
            return notify_error(error);
        }
    }
    let initial = match ctx
        .catalog
        .notify_batch(&serial, route, &id, generation)
        .await
    {
        Ok(v) => v,
        Err(e) => return notify_error(e),
    };
    let token = (generation, Uuid::new_v4());
    match ctx
        .catalog
        .record_notify_connection(&serial, route, &id, token, NotifyConnectionAction::Open)
        .await
    {
        Ok(true) => {}
        Ok(false) => return invalid("消费者已删除或重建"),
        Err(e) => return notify_error(e),
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
                            json!({"error":error.to_string(),"retryable":retryable(&error)})
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
    let _request_permit = match permit(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let (serial, route) = match scope(&ctx, &request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let value = match body::<Inject>(request).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let operation = match uuid(&value.operation) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .inject_notification(&serial, route, operation, &value.event_type, &value.data)
        .await
    {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => notify_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notify_protocol_classifies_atomic_abort_as_permanent_domain_error() {
        for (inner, status) in [
            (CatalogError::Corrupt("测试损坏"), 500),
            (CatalogError::Invalid("测试无效"), 400),
        ] {
            let error =
                CatalogError::Fdb(foundationdb::FdbBindingError::CustomError(Box::new(inner)));
            let response = notify_error(error);
            assert_eq!(response.status().as_u16(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["retryable"], false);
        }
    }
    #[test]
    fn notify_protocol_rejects_noncanonical_generation_query() {
        let id = Uuid::new_v4();
        for value in [
            id.simple().to_string(),
            id.to_string().to_uppercase(),
            id.urn().to_string(),
        ] {
            let request = Request::builder()
                .uri(format!("/stream?generation={value}"))
                .body(Body::empty())
                .unwrap();
            assert!(generation(&request).is_err());
        }
    }
}
