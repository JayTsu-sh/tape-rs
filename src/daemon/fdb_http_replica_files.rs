//! Native file data plane. Only opaque catalog selections reach the executor.
use super::*;
use crate::managed_catalog::{ReplicaFile, ReplicaRead, ReplicaUpload};
use crate::replicated_pool::ReplicatedArchive;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub(in crate::daemon) const BASE: &str = "/v1/fdb/replicated-pools";

pub(super) fn router(ctx: Arc<Context>) -> Router {
    Router::new()
        .route("/v1/fdb/replicated-pools/{id}/stat", get(stat))
        .route("/v1/fdb/replicated-pools/{id}/children", get(children))
        .route("/v1/fdb/replicated-pools/{id}/file", get(content))
        .route(
            "/v1/fdb/replicated-pools/{id}/archives",
            axum::routing::post(admit),
        )
        .route(
            "/v1/fdb/replicated-pools/{id}/archives/{operation}",
            get(status).delete(cancel),
        )
        .route(
            "/v1/fdb/replicated-pools/{id}/archives/{operation}/members/{member}",
            get(recovery_content).put(upload_member),
        )
        .with_state(ctx)
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, json!({"error":"not_found"}))
}

fn error(error: CatalogError) -> Response {
    match error.domain() {
        CatalogError::IdempotencyMismatch | CatalogError::Invalid(_) => reply(
            StatusCode::CONFLICT,
            json!({"error":"archive_conflict","detail":error.domain().to_string()}),
        ),
        CatalogError::PoolBusy
        | CatalogError::PoolImporting
        | CatalogError::LogicalInventoryBlocked(_) => unavailable(&error.domain().to_string()),
        _ => catalog_error(error),
    }
}

async fn admit(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let Some(budget) = ctx.upload_budget else {
        return unavailable("未启用 FDB writer");
    };
    let Ok(_permit) = ctx.admin_requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    let Ok(Ok(body)) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(request.into_body(), 8192),
    )
    .await
    else {
        return invalid("多副本原请求过长或超时");
    };
    let Ok(input) = serde_json::from_slice::<ReplicatedArchive>(&body) else {
        return invalid("多副本原请求 JSON 无效");
    };
    if input.size > budget.download_bytes {
        return reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"upload_too_large"}),
        );
    }
    match ctx
        .catalog
        .admit_replica_archive(id, &input, &ctx.routes)
        .await
    {
        Ok(value) => archive_response(value),
        Err(problem) => error(problem),
    }
}

fn archive_response(value: Value) -> Response {
    let status = if matches!(value["status"].as_str(), Some("committed" | "cancelled")) {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    reply(status, value)
}

async fn archive_status(ctx: &Context, id: Uuid, operation: Uuid, cancel: bool) -> Response {
    let Ok(_permit) = ctx.admin_requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    match ctx
        .catalog
        .replica_archive_status(id, operation, cancel, &ctx.routes)
        .await
    {
        Ok(Some(value)) => archive_response(value),
        Ok(None) => not_found(),
        Err(problem) => error(problem),
    }
}

async fn status(
    State(ctx): State<Arc<Context>>,
    Path((id, operation)): Path<(Uuid, Uuid)>,
) -> Response {
    archive_status(&ctx, id, operation, false).await
}

async fn cancel(
    State(ctx): State<Arc<Context>>,
    Path((id, operation)): Path<(Uuid, Uuid)>,
) -> Response {
    if ctx.upload_budget.is_none() {
        return unavailable("未启用 FDB writer");
    }
    archive_status(&ctx, id, operation, true).await
}

enum Destination<'a> {
    Local(Arc<HttpContext>),
    Peer(&'a str, Arc<HttpContext>),
}

fn destination<'a>(ctx: &'a Context, serial: &str) -> Option<(&'a OwnerRuntime, Destination<'a>)> {
    let owner = ctx.owner.as_ref()?;
    let library = owner.libraries.get(serial)?;
    let route = ctx.routes.get(serial)?;
    let status = library.status.lock().unwrap_or_else(|err| err.into_inner());
    let executor = status.executor.as_ref().filter(|executor| {
        !status.shutting_down
            && status.executor_ready
            && executor.fenced
            && executor.node != 0
            && status.catalog_frozen_index == Some(route.frozen_index)
    })?;
    let dest = if executor.node == status.id {
        Destination::Local(library.clone())
    } else {
        Destination::Peer(owner.peers.get(&executor.node)?.as_str(), library.clone())
    };
    Some((owner, dest))
}

pub(in crate::daemon) async fn upload_member(
    State(ctx): State<Arc<Context>>,
    Path((id, operation, member)): Path<(Uuid, Uuid, Uuid)>,
    request: Request,
) -> Response {
    let Some(budget) = ctx.upload_budget else {
        return unavailable("未启用 FDB writer");
    };
    // Reconcile a possibly lost S4 response before considering this request body.
    match ctx
        .catalog
        .replica_archive_status(id, operation, false, &ctx.routes)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(problem) => return error(problem),
    }
    let planned = match ctx
        .catalog
        .replica_upload(id, operation, member, &ctx.routes)
        .await
    {
        Ok(Some(planned)) => planned,
        Ok(None) => return archive_status(&ctx, id, operation, false).await,
        Err(problem) => return error(problem),
    };
    if request
        .headers()
        .get_all(header::CONTENT_LENGTH)
        .iter()
        .count()
        != 1
        || request.headers()[header::CONTENT_LENGTH]
            .to_str()
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            != Some(planned.spec.size)
        || request.headers().contains_key(header::TRANSFER_ENCODING)
        || request.headers().contains_key(header::RANGE)
        || request.headers().contains_key(header::CONTENT_RANGE)
        || request.uri().query().is_some()
    {
        return invalid("副本上传需要与原请求一致的唯一 Content-Length");
    }
    if planned.spec.size > budget.download_bytes {
        return reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"upload_too_large"}),
        );
    }
    let Some((owner, dest)) = destination(&ctx, &planned.serial) else {
        return unavailable(&format!("底层池 {member} 的 owner 尚未就绪"));
    };
    let Ok(permit) = owner.requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    let library = match dest {
        Destination::Local(library) => library,
        Destination::Peer(addr, library) => {
            if request.headers().contains_key("x-tapers-forwarded") {
                return unavailable("底层池 owner 已变化");
            }
            return forward(request, addr, permit, &planned.serial, library.wait_timeout).await;
        }
    };
    let timeout = library.wait_timeout;
    let executor = library.clone();
    let catalog = ctx.catalog.clone();
    let pending = tokio::spawn(async move {
        let ContentDigest::Sha256(expected) = planned.spec.digest else {
            return Err(ReadError::Failed("原归档请求缺少 SHA-256".into()));
        };
        let file = receive(
            request.into_body(),
            &executor,
            planned.spec.size,
            expected,
            budget,
        )
        .await?;
        let ReplicaUpload { request, spec, .. } = planned;
        let identity = if let Some(request) = request {
            request
        } else {
            let managed = executor.clone();
            let intent = tokio::task::spawn_blocking(move || {
                execute(&managed, |reply| ExecRequest::FdbPrepare {
                    spec: Box::new(spec),
                    barcode: None,
                    reply,
                })
            })
            .await
            .map_err(|err| ReadError::Busy(err.to_string()))??;
            catalog
                .pin_replica_upload(id, operation, &intent.request())
                .await
                .map_err(|err| ReadError::Busy(err.to_string()))?
        };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            execute(&executor, |reply| ExecRequest::FdbWrite {
                intent: Box::new(identity),
                file,
                reply,
            })
        })
        .await
        .map_err(|err| ReadError::Busy(err.to_string()))?
    });
    match tokio::time::timeout(timeout, pending).await {
        Ok(Ok(Ok(_))) => archive_status(&ctx, id, operation, false).await,
        Ok(Ok(Err(ReadError::Failed(detail)))) => reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error":"member_rejected","member":member,"detail":detail}),
        ),
        Ok(Ok(Err(problem))) => {
            let status = archive_status(&ctx, id, operation, false).await;
            if status.status() == StatusCode::OK {
                status
            } else {
                unavailable(&format!("底层池 {member} 的原请求未确认：{problem}"))
            }
        }
        Ok(Err(_)) => unavailable("归档任务异常，请查询原操作"),
        Err(_) => reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"error":"archive_indeterminate","member":member}),
        ),
    }
}

async fn receive(
    mut body: Body,
    library: &HttpContext,
    size: u64,
    expected: [u8; 32],
    budget: super::super::fdb_read::ReadStorageBudget,
) -> Result<std::fs::File, ReadError> {
    let busy = |err: std::io::Error| ReadError::Busy(err.to_string());
    let file = crate::local_read_storage::download_file(library.files.spool_dir(), size, budget)
        .map_err(|err| ReadError::Busy(err.to_string()))?;
    let mut file = tokio::fs::File::from_std(file);
    let deadline = tokio::time::Instant::now() + library.wait_timeout;
    let mut remaining = size;
    let mut sha = Sha256::new();
    while let Some(frame) = tokio::time::timeout_at(deadline, body.frame())
        .await
        .map_err(|_| ReadError::Busy("副本上传正文超时".into()))?
    {
        let data = frame
            .ok()
            .and_then(|frame| frame.into_data().ok())
            .ok_or_else(|| ReadError::Failed("副本上传正文不完整".into()))?;
        remaining = remaining
            .checked_sub(data.len() as u64)
            .ok_or_else(|| ReadError::Failed("副本上传超过声明长度".into()))?;
        sha.update(&data);
        file.write_all(&data).await.map_err(busy)?;
    }
    if remaining != 0 {
        return Err(ReadError::Failed("副本上传正文长度不足".into()));
    }
    if <[u8; 32]>::from(sha.finalize()) != expected {
        return Err(ReadError::Failed(
            "副本上传 SHA-256 与原归档请求不符".into(),
        ));
    }
    file.flush().await.map_err(busy)?;
    Ok(file.into_std().await)
}

fn execute<T>(
    library: &HttpContext,
    make_request: impl FnOnce(std::sync::mpsc::Sender<Result<T, ReadError>>) -> ExecRequest,
) -> Result<T, ReadError> {
    let (tx, rx) = std::sync::mpsc::channel();
    library
        .exec
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .send(make_request(tx))
        .map_err(|_| ReadError::Busy("owner 执行线程已停止".into()))?;
    rx.recv()
        .map_err(|_| ReadError::Busy("owner 回执已断开".into()))?
}

async fn forward(
    request: Request,
    addr: &str,
    permit: OwnedSemaphorePermit,
    serial: &str,
    timeout: std::time::Duration,
) -> Response {
    match tokio::time::timeout(
        timeout,
        gateway::forward_request(request, addr, permit, serial),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => reply(
            StatusCode::GATEWAY_TIMEOUT,
            json!({"error":"owner_timeout"}),
        ),
    }
}

fn path(request: &Request) -> Result<String, &'static str> {
    decoded_path(request).map(|value| String::from_utf8(value).expect("validated UTF-8"))
}

async fn stat(State(ctx): State<Arc<Context>>, Path(id): Path<Uuid>, request: Request) -> Response {
    let Ok(_permit) = ctx.admin_requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    let path = match path(&request) {
        Ok(path) => path,
        Err(reason) => return invalid(reason),
    };
    match ctx.catalog.replica_file(id, &path, &ctx.routes).await {
        Ok(Some(mut file)) => {
            file.info["id"] = json!(id);
            reply(StatusCode::OK, file.info)
        }
        Ok(None) => not_found(),
        Err(problem) => error(problem),
    }
}

async fn children(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let Ok(_permit) = ctx.admin_requests.clone().try_acquire_owned() else {
        return unavailable("http_busy");
    };
    let path = match path(&request) {
        Ok(path) => path,
        Err(reason) => return invalid(reason),
    };
    let after = match query_value(&request, "after").and_then(|after| {
        after
            .map(|value| {
                if value.len() > 512 {
                    return Err("目录游标过长");
                }
                URL_SAFE_NO_PAD
                    .decode(value)
                    .ok()
                    .and_then(|raw| String::from_utf8(raw).ok())
                    .ok_or("目录游标无效")
            })
            .transpose()
    }) {
        Ok(after) => after,
        Err(reason) => return invalid(reason),
    };
    match ctx
        .catalog
        .replica_children(id, &path, after.as_deref(), &ctx.routes)
        .await
    {
        Ok(Some(value)) => reply(StatusCode::OK, value),
        Ok(None) => not_found(),
        Err(problem) => error(problem),
    }
}

async fn content(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET {
        return invalid("副本正文需要 GET");
    }
    let range = match request
        .headers()
        .get(header::RANGE)
        .map(|value| value.to_str())
        .transpose()
    {
        Ok(range) => range,
        Err(_) => return invalid("Range 无效"),
    };
    let path = match path(&request) {
        Ok(path) => path,
        Err(reason) => return invalid(reason),
    };
    let member = match query_value(&request, "member").and_then(|value| {
        value
            .map(|value| Uuid::parse_str(value).map_err(|_| "成员 UUID 无效"))
            .transpose()
    }) {
        Ok(member) => member,
        Err(reason) => return invalid(reason),
    };
    let expected = match query_value(&request, "version").and_then(|value| {
        value
            .map(|value| value.parse::<u64>().map_err(|_| "文件版本无效"))
            .transpose()
    }) {
        Ok(expected) => expected,
        Err(reason) => return invalid(reason),
    };
    let file = match ctx.catalog.replica_file(id, &path, &ctx.routes).await {
        Ok(Some(file)) => file,
        Ok(None) => return not_found(),
        Err(problem) => return error(problem),
    };
    let version = file.info["file_version"].as_u64().expect("catalog version");
    if expected.is_some_and(|expected| expected != version) || file.info["kind"] != "file" {
        return reply(
            StatusCode::CONFLICT,
            json!({"error":"file_version_or_kind_conflict"}),
        );
    }
    let forwarded = request.headers().contains_key("x-tapers-forwarded");
    let route = |pool| {
        format!(
            "{BASE}/{id}/file?path={}&version={version}&member={pool}",
            URL_SAFE_NO_PAD.encode(&path)
        )
    };
    read_file(&ctx, file, member, forwarded, range, route).await
}

async fn recovery_content(
    State(ctx): State<Arc<Context>>,
    Path((id, operation, member)): Path<(Uuid, Uuid, Uuid)>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET || request.headers().contains_key(header::RANGE)
    {
        return invalid("恢复副本需要完整 GET");
    }
    let file = match ctx
        .catalog
        .replica_recovery_file(id, operation, &ctx.routes)
        .await
    {
        Ok(Some(file)) => file,
        Ok(None) => return not_found(),
        Err(problem) => return error(problem),
    };
    let forwarded = request.headers().contains_key("x-tapers-forwarded");
    read_file(&ctx, file, Some(member), forwarded, None, |pool| {
        format!("{BASE}/{id}/archives/{operation}/members/{pool}")
    })
    .await
}

pub(in crate::daemon) async fn read_file(
    ctx: &Context,
    file: ReplicaFile,
    member: Option<Uuid>,
    forwarded: bool,
    range: Option<&str>,
    route: impl Fn(Uuid) -> String,
) -> Response {
    let mut proxied = std::collections::BTreeSet::new();
    let version = file_version(&file.info);
    let mut failures = Vec::new();
    for selection in file.copies {
        if member.is_some_and(|member| member != selection.pool) {
            continue;
        }
        let Some((owner, dest)) = destination(ctx, &selection.serial) else {
            failures.push(format!("{} 的 owner 尚未就绪", selection.pool));
            continue;
        };
        let Ok(permit) = owner.requests.clone().try_acquire_owned() else {
            return unavailable("http_busy");
        };
        match dest {
            Destination::Local(library) => {
                let pool = selection.pool;
                match read_member(library, selection, permit).await {
                    Ok((file, permit)) => {
                        return with_version(held_file(file, range, permit), &version);
                    }
                    Err(reason) => failures.push(format!("{pool}: {reason}")),
                }
            }
            Destination::Peer(addr, library) if !forwarded && proxied.insert(selection.pool) => {
                let mut request = Request::builder().uri(route(selection.pool));
                if let Some(range) = range {
                    request = request.header(header::RANGE, range);
                }
                let request = request.body(Body::empty()).expect("internal read URI");
                let response = forward(
                    request,
                    addr,
                    permit,
                    &selection.serial,
                    library.wait_timeout,
                )
                .await;
                if matches!(response.status().as_u16(), 200 | 206 | 416)
                    && response
                        .headers()
                        .get("x-tapers-file-version")
                        .is_some_and(|value| value == &version)
                {
                    return response;
                }
                failures.push(format!("{}: HTTP {}", selection.pool, response.status()));
            }
            Destination::Peer(..) => {}
        }
    }
    if failures.is_empty() {
        not_found()
    } else {
        reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"no_verified_replica","detail":failures}),
        )
    }
}

fn file_version(info: &Value) -> String {
    info["file_version"]
        .as_u64()
        .expect("catalog version")
        .to_string()
}

fn with_version(mut response: Response, version: &str) -> Response {
    response.headers_mut().insert(
        "x-tapers-file-version",
        version.parse().expect("numeric version"),
    );
    response
}

async fn read_member(
    library: Arc<HttpContext>,
    selection: ReplicaRead,
    permit: OwnedSemaphorePermit,
) -> Result<(std::fs::File, OwnedSemaphorePermit), String> {
    let timeout = library.wait_timeout;
    let pending = tokio::task::spawn_blocking(move || {
        execute(&library, |reply| ExecRequest::FdbReplicaRead {
            selection: Box::new(selection),
            reply,
        })
        .map(|file| (file, permit))
    });
    tokio::time::timeout(timeout, pending)
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}
