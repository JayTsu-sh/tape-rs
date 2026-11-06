//! An explicit upload carries stable identity across retries and owner changes.
use super::*;
use crate::managed_catalog::{ActiveFileRequest, PublishOutcome};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const INTENT_HEADER: &str = "x-tapers-fdb-write";

/// Encode the immutable upload fields. The route, path and pool are supplied
/// by the URL; the execution epoch is always selected by the receiving owner.
pub fn encode_intent(intent: &ActiveFileRequest) -> Result<String, &'static str> {
    let ContentDigest::Sha256(digest) = intent.expected_digest else {
        return Err("上传必须提供 SHA-256");
    };
    intent.ltfs_path().map_err(|_| "上传身份或路径无效")?;
    Ok(URL_SAFE_NO_PAD.encode(
        json!({
            "version":2, "namespace":intent.namespace.to_string(), "operation":intent.operation_id.to_string(),
            "epoch":intent.request_epoch, "parent":intent.parent.to_string(),
            "file":intent.file_id.to_string(), "previous":intent.expected_version,
            "barcode":intent.barcode, "size":intent.expected_size,
            "sha256":hex(&digest),
        })
        .to_string(),
    ))
}

fn parse(
    request: &Request,
    serial: &str,
    route: ActiveLibraryRoute,
) -> Result<ActiveFileRequest, &'static str> {
    if request
        .headers()
        .get_all("x-tapers-fdb-namespace")
        .iter()
        .count()
        > 1
    {
        return Err("namespace 请求头重复");
    }
    if let Some(value) = request.headers().get("x-tapers-fdb-namespace")
        && value.to_str().ok().and_then(|s| Uuid::parse_str(s).ok()) != Some(route.route.namespace)
    {
        return Err("保存的上传命名空间与当前路由不一致");
    }
    if request.uri().query().unwrap_or("").split('&').any(|item| {
        !matches!(
            item.split_once('=').map(|(key, _)| key),
            Some("pool" | "path")
        )
    }) {
        return Err("上传只支持 pool 和 path 查询参数");
    }
    let (path, pool, _) = content_scope(request)?;
    let path = path.trim_start_matches('/');
    let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
    if request.headers().get_all(INTENT_HEADER).iter().count() != 1 {
        return Err("上传必须包含唯一 x-tapers-fdb-write");
    }
    let raw = request.headers()[INTENT_HEADER]
        .to_str()
        .map_err(|_| "上传身份无效")?;
    if raw.len() > 4096 {
        return Err("上传身份过长");
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| "上传身份不是 Base64URL")?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| "上传身份不是 JSON")?;
    let object = value.as_object().ok_or("上传身份必须是对象")?;
    if decoded != value.to_string().as_bytes() {
        return Err("上传身份必须使用规范 JSON，禁止重复字段");
    }
    let fields = [
        "version",
        "namespace",
        "operation",
        "epoch",
        "parent",
        "file",
        "previous",
        "barcode",
        "size",
        "sha256",
    ];
    if value["version"].as_u64() != Some(2) {
        return Err("上传身份版本无效");
    }
    if value["namespace"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        != Some(route.route.namespace)
    {
        return Err("上传命名空间与当前路由不一致");
    }
    if object.len() != fields.len() || object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err("上传身份字段不匹配");
    }
    let id = |field: &str| {
        value[field]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .ok_or("上传 UUID 无效")
    };
    let number = |field: &str| value[field].as_u64().ok_or("上传数值无效");
    let sha = value["sha256"].as_str().ok_or("上传 SHA-256 无效")?;
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("上传 SHA-256 无效");
    }
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&sha[i * 2..i * 2 + 2], 16).map_err(|_| "上传 SHA-256 无效")?;
    }
    let intent = ActiveFileRequest {
        namespace: route.route.namespace,
        parent: id("parent")?,
        parent_path: format!("/{parent_path}").into_bytes(),
        name: name.as_bytes().to_vec(),
        file_id: id("file")?,
        expected_version: if value["previous"].is_null() {
            None
        } else {
            Some(number("previous")?)
        },
        operation_id: id("operation")?,
        request_epoch: number("epoch")?,
        library_serial: serial.into(),
        pool_id: Some(Uuid::parse_str(&pool).map_err(|_| "池 UUID 无效")?),
        barcode: match &value["barcode"] {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            _ => return Err("上传条码约束无效"),
        },
        expected_size: number("size")?,
        expected_digest: ContentDigest::Sha256(digest),
    };
    intent.ltfs_path().map_err(|_| "上传路径或身份无效")?;
    // Require canonical path bytes, including the leading slash; never let
    // distinct URLs silently name the same on-medium operation.
    if intent.ltfs_path().unwrap().as_bytes() != decoded_path(request)? {
        return Err("上传路径必须是规范绝对路径");
    }
    Ok(intent)
}

pub(super) async fn write(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    dispatch(ctx, serial, request).await
}

pub(super) async fn status(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"method_not_allowed"}),
        );
    }
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    };
    let intent = match parse(&request, &serial, route) {
        Ok(intent) => intent,
        Err(error) => return invalid(error),
    };
    match ctx.catalog.active_upload_result(&intent, route).await {
        Ok(Some(PublishOutcome::AlreadyApplied(version))) => reply(
            StatusCode::OK,
            json!({"status":"committed", "version":version}),
        ),
        Ok(Some(result)) => reply(StatusCode::CONFLICT, json!({"error":format!("{result:?}")})),
        Ok(None) => reply(StatusCode::ACCEPTED, json!({"status":"unconfirmed"})),
        Err(error) => catalog_error(error),
    }
}

/// Read immutable original S4 evidence without an executor or current path.
pub(super) async fn receipt(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    use crate::managed_catalog::UploadReceiptOutcome;
    if request.method() != axum::http::Method::GET {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"method_not_allowed"}),
        );
    }
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    };
    let intent = match parse(&request, &serial, route) {
        Ok(value) => value,
        Err(error) => return invalid(error),
    };
    let original = match ctx
        .catalog
        .active_upload_receipt(&intent, route, 8 << 20)
        .await
    {
        Ok(UploadReceiptOutcome::Committed(value)) => value,
        Ok(UploadReceiptOutcome::Unconfirmed) => {
            return reply(StatusCode::ACCEPTED, json!({"status":"unconfirmed"}));
        }
        Ok(UploadReceiptOutcome::Rejected(result)) => {
            return reply(StatusCode::CONFLICT, json!({"error":format!("{result:?}")}));
        }
        Err(error) => return catalog_error(error),
    };
    let r = original.receipt;
    let attributes = original.attributes;
    let path = intent.ltfs_path().expect("validated");
    let kind = if original.native_symlink {
        "symlink"
    } else {
        "file"
    };
    let exact = json!({"schema_version":1,"library_serial":serial,"namespace":r.namespace.to_string(),
        "pool_id":r.pool_id.map(|id|id.to_string()),"path":path,"id":r.file_id.to_string(),
        "version":original.version,"kind":kind,"attributes_digest":hex(&attributes.digest()),
        "metadata":attributes.meta(),"xattrs":attributes.xattrs(),
        "symlink_target":if original.native_symlink { attributes.symlink() } else { None }});
    let value = json!({"schema_version":1,"status":"committed","library_serial":serial,
        "namespace":r.namespace.to_string(),"pool_id":r.pool_id.map(|id|id.to_string()),"path":path,
        "operation":r.operation_id.to_string(),"epoch":r.request_epoch,"parent":r.parent.to_string(),
        "id":r.file_id.to_string(),"version":original.version,"copy":r.copy_id.to_string(),
        "volume":r.volume_id.to_string(),"generation":r.volume_generation,"scan":r.volume_scan_id.to_string(),
        "file_uid":r.ltfs_file_uid,"barcode":original.barcode,"size":r.size,
        "sha256":match r.content_digest { ContentDigest::Sha256(value)=>hex(&value), _=>return unavailable("上传回执缺少 SHA-256") },
        "attributes":exact});
    let bytes = value.to_string();
    if bytes.len() > 64 << 20 {
        return unavailable("原上传回执超过 JSON 响应预算");
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        bytes,
    )
        .into_response()
}

async fn dispatch(ctx: Arc<Context>, serial: String, request: Request) -> Response {
    let Some(budget) = ctx.upload_budget else {
        return unavailable("未启用 FDB writer");
    };
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    };
    let intent = match parse(&request, &serial, route) {
        Ok(intent) => intent,
        Err(error) => return invalid(error),
    };
    if request.headers().contains_key(header::TRANSFER_ENCODING)
        || request
            .headers()
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .count()
            != 1
        || request.headers()[header::CONTENT_LENGTH]
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            != Some(intent.expected_size)
    {
        return invalid("上传需要与身份大小一致的唯一 Content-Length");
    }
    if request.headers().contains_key(header::IF_MATCH)
        || request.headers().contains_key(header::IF_NONE_MATCH)
        || request.headers().contains_key(header::IF_MODIFIED_SINCE)
        || request.headers().contains_key(header::IF_UNMODIFIED_SINCE)
        || request.headers().contains_key(header::IF_RANGE)
        || request.headers().contains_key(header::CONTENT_RANGE)
        || request.headers().contains_key(header::RANGE)
    {
        return invalid("上传条件请使用身份中的 previous，不支持 Range 或条件请求头");
    }
    if intent.expected_size > budget.download_bytes {
        return reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"upload_too_large"}),
        );
    }
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
    let Some(executor) = status.executor.as_ref().filter(|executor| {
        !status.shutting_down
            && status.executor_ready
            && executor.fenced
            && executor.node != 0
            && status.catalog_frozen_index == Some(route.frozen_index)
    }) else {
        return unavailable("带库 owner 尚未完成恢复");
    };
    if executor.node != status.id {
        if request.headers().contains_key("x-tapers-forwarded") {
            return unavailable("带库 owner 已变化，请稍后重试");
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
                json!({"error":"upload_indeterminate"}),
            ),
        };
    }
    let spool = match crate::local_read_storage::download_file(
        library.files.spool_dir(),
        intent.expected_size,
        budget,
    ) {
        Ok(file) => file,
        Err(error) => return unavailable(&format!("上传暂存不可用: {error}")),
    };
    let timeout = library.wait_timeout;
    let library = library.clone();
    // The task owns its queue slot and anonymous spool even if the HTTP peer
    // disconnects or times out. No interrupted upload reaches the executor.
    let pending = tokio::spawn(async move {
        let mut file = tokio::fs::File::from_std(spool);
        let mut body = request.into_body();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut received = 0u64;
        while let Some(frame) = tokio::time::timeout_at(deadline, body.frame())
            .await
            .map_err(|_| "上传正文超时".to_string())?
        {
            let frame = frame.map_err(|error| error.to_string())?;
            if let Ok(data) = frame.into_data() {
                received = received
                    .checked_add(data.len() as u64)
                    .ok_or("上传长度溢出")?;
                if received > intent.expected_size {
                    return Err("上传正文超过声明长度".into());
                }
                file.write_all(&data)
                    .await
                    .map_err(|error| error.to_string())?;
            } else {
                return Err("上传不支持 trailers".into());
            }
        }
        if received != intent.expected_size {
            return Err("上传正文长度不足".into());
        }
        file.flush().await.map_err(|error| error.to_string())?;
        let file = file.into_std().await;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (reply, received) = std::sync::mpsc::channel();
            library
                .exec
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .send(ExecRequest::FdbWrite {
                    intent: Box::new(intent),
                    file,
                    reply,
                })
                .map_err(|_| "owner 执行线程已停止".to_string())?;
            received
                .recv()
                .map_err(|_| "上传回执已断开".to_string())?
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?
    });
    match tokio::time::timeout(timeout, pending).await {
        Ok(Ok(Ok(version))) => reply(
            StatusCode::CREATED,
            json!({"status":"committed", "version":version}),
        ),
        Ok(Ok(Err(error))) => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"upload_unconfirmed", "detail":error}),
        ),
        Ok(Err(_)) => unavailable("上传任务异常，请查询原请求结果"),
        Err(_) => reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"error":"upload_indeterminate"}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_catalog::PreparedLibraryRoute;

    #[test]
    fn upload_identity_roundtrip_binds_path_pool_and_original_request() {
        let namespace = Uuid::new_v4();
        let route = ActiveLibraryRoute {
            route: PreparedLibraryRoute {
                namespace,
                revision: 1,
            },
            frozen_index: 10,
            certificate_sha256: [1; 32],
            initial_owner_epoch: 1,
        };
        let intent = crate::managed_catalog::ActiveFileIntent {
            barcode_constraint: None,
            namespace,
            parent: Uuid::nil(),
            parent_path: b"/".to_vec(),
            name: b"a".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 2,
            volume_scan_id: Uuid::new_v4(),
            expected_size: 3,
            expected_digest: ContentDigest::Sha256([7; 32]),
        };
        let encoded = encode_intent(&intent.request()).unwrap();
        let request = |path: &[u8], metadata: &str| {
            Request::builder()
                .uri(format!(
                    "/v1/fdb/files/SIM?pool={}&path={}",
                    intent.pool_id.unwrap(),
                    URL_SAFE_NO_PAD.encode(path)
                ))
                .header(INTENT_HEADER, metadata)
                .body(Body::empty())
                .unwrap()
        };
        let original = intent.request();
        assert_eq!(
            parse(&request(b"/a", &encoded), "SIM", route).unwrap(),
            original
        );
        let mut mismatched: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).unwrap()).unwrap();
        mismatched["namespace"] = json!(Uuid::new_v4().to_string());
        assert!(
            parse(
                &request(b"/a", &URL_SAFE_NO_PAD.encode(mismatched.to_string())),
                "SIM",
                route
            )
            .is_err()
        );
        mismatched["namespace"] = json!(namespace.to_string());
        mismatched["version"] = json!(99);
        assert!(
            parse(
                &request(b"/a", &URL_SAFE_NO_PAD.encode(mismatched.to_string())),
                "SIM",
                route
            )
            .is_err(),
            "只接受当前协议版本"
        );
        for path in [b"//a".as_slice(), b"/a/../b", b"/", b"a"] {
            assert!(parse(&request(path, &encoded), "SIM", route).is_err());
        }
        let mut duplicate = request(b"/a", &encoded);
        duplicate
            .headers_mut()
            .append(INTENT_HEADER, encoded.parse().unwrap());
        assert!(parse(&duplicate, "SIM", route).is_err());
        let mut json: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).unwrap()).unwrap();
        json["ignored"] = json!(true);
        assert!(
            parse(
                &request(b"/a", &URL_SAFE_NO_PAD.encode(json.to_string())),
                "SIM",
                route
            )
            .is_err()
        );
        let raw = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        let raw = String::from_utf8(raw)
            .unwrap()
            .replace("{", "{\"version\":1,");
        assert!(parse(&request(b"/a", &URL_SAFE_NO_PAD.encode(raw)), "SIM", route).is_err());
    }
}
