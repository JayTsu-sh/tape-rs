//! Pool-scoped upload preparation. No reservation or medium move is performed.
use super::*;
use crate::managed_catalog::ActiveUploadSpec;
use uuid::Uuid;

pub(super) async fn prepare(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    if ctx.upload_budget.is_none() {
        return unavailable("未启用 FDB writer");
    }
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    };
    if request.uri().query().unwrap_or("").split('&').any(|item| {
        !matches!(
            item.split_once('=').map(|(key, _)| key),
            Some("pool" | "path")
        )
    }) {
        return invalid("准备上传只支持 pool 和 path 查询参数");
    }
    let (path, pool, _) = match content_scope(&request) {
        Ok(scope) => scope,
        Err(error) => return invalid(error),
    };
    let Some(owner) = &ctx.owner else {
        return unavailable("未启用 owner");
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
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Some(executor) = status.executor.as_ref().filter(|e| {
        !status.shutting_down
            && status.executor_ready
            && e.fenced
            && e.node != 0
            && status.catalog_frozen_index == Some(route.frozen_index)
    }) else {
        return unavailable("带库 owner 尚未完成恢复");
    };
    if executor.node != status.id {
        if request.headers().contains_key("x-tapers-forwarded") {
            return unavailable("owner 已变化，请重试准备请求");
        }
        let Some(address) = owner.peers.get(&executor.node) else {
            return unavailable("owner 地址未配置");
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
                json!({"error":"prepare_timeout"}),
            ),
        };
    }
    let body = match tokio::time::timeout(
        library.wait_timeout,
        axum::body::to_bytes(request.into_body(), 8192),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return invalid("上传准备正文超过限制、被截断或超时"),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return invalid("上传准备需要 JSON"),
    };
    let fields = ["operation", "size", "sha256", "previous", "barcode"];
    if !value.as_object().is_some_and(|obj| {
        obj.len() == fields.len() && obj.keys().all(|k| fields.contains(&k.as_str()))
    }) || body.as_ref() != value.to_string().as_bytes()
    {
        return invalid("上传准备字段不匹配或不是规范 JSON");
    }
    let Some(operation) = value["operation"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
    else {
        return invalid("operation UUID 无效");
    };
    let Some(size) = value["size"].as_u64() else {
        return invalid("size 无效");
    };
    let Some(sha) = value["sha256"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return invalid("SHA-256 无效");
    };
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&sha[i * 2..i * 2 + 2], 16).expect("validated hex");
    }
    let previous = if value["previous"].is_null() {
        None
    } else {
        match value["previous"].as_u64() {
            Some(v) if v > 0 && v != u64::MAX => Some(v),
            _ => return invalid("previous 无效"),
        }
    };
    let barcode = match &value["barcode"] {
        Value::Null => None,
        Value::String(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| (b'!'..=b'~').contains(&b) && b != b'/') =>
        {
            Some(s.clone())
        }
        _ => return invalid("barcode 无效"),
    };
    let spec = ActiveUploadSpec {
        pool_id: Uuid::parse_str(&pool).expect("validated pool"),
        path: path.into_bytes(),
        operation_id: operation,
        expected_version: previous,
        size,
        digest: ContentDigest::Sha256(digest),
    };
    let timeout = library.wait_timeout;
    let library = library.clone();
    let pending = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (reply, received) = std::sync::mpsc::channel();
        library
            .exec
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send(ExecRequest::FdbPrepare {
                spec: Box::new(spec),
                barcode,
                reply,
            })
            .map_err(|_| ReadError::Busy("owner 执行线程已停止".into()))?;
        received
            .recv()
            .map_err(|_| ReadError::Busy("准备回执已断开".into()))?
    });
    match tokio::time::timeout(timeout, pending).await {
        Ok(Ok(Ok(intent))) => reply(
            StatusCode::OK,
            json!({
                "version":2, "library":serial, "pool":pool,
                "path":URL_SAFE_NO_PAD.encode(intent.ltfs_path().expect("validated path")),
            "namespace":intent.namespace.to_string(), "metadata":upload::encode_intent(&intent.request()).expect("validated intent"),
            }),
        ),
        Ok(Ok(Err(ReadError::Failed(reason)))) => {
            reply(StatusCode::CONFLICT, json!({"error":reason}))
        }
        Ok(Ok(Err(error))) => unavailable(&error.to_string()),
        Ok(Err(_)) => unavailable("准备任务异常"),
        Err(_) => reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"error":"prepare_timeout"}),
        ),
    }
}
