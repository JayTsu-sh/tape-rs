//! FDB metadata and explicitly enabled owner reads. Metadata-only mode never
//! opens a tape; owner content is released only after complete media verification.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, body::Body};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use crate::managed_catalog::{
    ActiveLibraryRoute, CatalogError, ContentDigest, ManagedCatalog, ResolvedPath,
};

use super::{
    executor::{ExecRequest, ReadError},
    gateway,
    http::{self, HttpContext},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
struct Context {
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    owner: Option<OwnerReads>,
}

#[derive(Clone)]
struct OwnerReads {
    libraries: BTreeMap<String, Arc<HttpContext>>,
    peers: HashMap<u64, String>,
    requests: Arc<Semaphore>,
}

fn reply(status: StatusCode, value: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        value.to_string(),
    )
        .into_response()
}

fn invalid(reason: &str) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({"error": reason}))
}

fn unavailable(reason: &str) -> Response {
    reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error": reason}))
}

fn catalog_error(error: CatalogError) -> Response {
    match error {
        CatalogError::Invalid(_) => invalid(&error.to_string()),
        _ => unavailable(&format!("FDB 目录不可用：{error}")),
    }
}

fn query_value<'a>(request: &'a Request, key: &str) -> Result<Option<&'a str>, &'static str> {
    let mut found = None;
    for item in request.uri().query().unwrap_or("").split('&') {
        if item.is_empty() {
            continue;
        }
        let (name, value) = item.split_once('=').ok_or("查询参数格式无效")?;
        if name == key && found.replace(value).is_some() {
            return Err("查询参数重复");
        }
    }
    Ok(found)
}

fn decoded_path(request: &Request) -> Result<Vec<u8>, &'static str> {
    let encoded = query_value(request, "path")?.ok_or("缺少 path")?;
    if encoded.len() > 4096 {
        return Err("路径过长");
    }
    let path = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "path 不是有效的 Base64URL")?;
    if path.len() > 2048 || std::str::from_utf8(&path).is_err() {
        return Err("路径必须是长度不超过 2048 的 UTF-8");
    }
    Ok(path)
}

async fn ready(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    // 内部单库探测只检查该库，避免双库 owner 分散时互相探测整个集群。
    let scope = match request.uri().query() {
        None => None,
        Some(query) => match query.strip_prefix("library=") {
            Some(serial) if !serial.is_empty() && !serial.contains('&') => Some(serial),
            _ => return invalid("readyz 查询只支持单个 library 序列号"),
        },
    };
    if scope.is_some_and(|serial| !ctx.routes.contains_key(serial)) {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    }
    for (serial, expected) in ctx
        .routes
        .iter()
        .filter(|(serial, _)| scope.is_none_or(|id| id == serial.as_str()))
    {
        match ctx.catalog.active_library_route(serial).await {
            Ok(Some(actual)) if actual == *expected => {}
            Ok(None) => return unavailable("带库尚未激活 FDB 路由"),
            Ok(Some(_)) => return unavailable("带库 FDB 路由已改变，需要重新启动只读服务"),
            Err(error) => return unavailable(&format!("FDB 路由不可用：{error}")),
        }
    }
    if let Some(owner) = &ctx.owner {
        for (serial, route) in ctx
            .routes
            .iter()
            .filter(|(serial, _)| scope.is_none_or(|id| id == serial.as_str()))
        {
            let Some(library) = owner.libraries.get(serial) else {
                return unavailable("带库 owner 未配置");
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
            if executor.node == status.id {
                if library.files.serving_deadline(executor.round).is_none() {
                    return unavailable("带库 owner 多数派 Serving 许可已失效");
                }
            } else {
                if request.headers().contains_key("x-tapers-forwarded") {
                    return unavailable("带库 owner 已变化，请稍后重试");
                }
                let Some(address) = owner.peers.get(&executor.node) else {
                    return unavailable("owner HTTP 地址未配置");
                };
                let Ok(permit) = owner.requests.clone().try_acquire_owned() else {
                    return unavailable("http_busy");
                };
                let Ok(probe) = Request::builder()
                    .uri(format!("/readyz?library={serial}"))
                    .header(header::HOST, address)
                    .body(Body::empty())
                else {
                    return unavailable("owner HTTP 地址无效");
                };
                let result = tokio::time::timeout(
                    library.wait_timeout.min(std::time::Duration::from_secs(5)),
                    gateway::forward_request(probe, address, permit, serial),
                )
                .await;
                if !matches!(result, Ok(response) if response.status() == StatusCode::OK) {
                    return unavailable("带库 owner 未确认可服务");
                }
            }
            match ctx.catalog.active_library_owner(serial).await {
                Ok(Some(actual)) if actual == (*route, executor.round) => {}
                _ => return unavailable("FDB owner epoch 尚未就绪"),
            }
        }
    }
    reply(
        StatusCode::OK,
        json!({"ready": true, "mode": if ctx.owner.is_some() { "fdb-owner-readonly" } else { "fdb-metadata-readonly" }}),
    )
}

async fn stat(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let Some(&expected) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error": "带库未配置"}));
    };
    let path = match decoded_path(&request) {
        Ok(path) => path,
        Err(error) => return invalid(error),
    };
    match ctx
        .catalog
        .resolve_active_metadata_for_route(&serial, &path, expected)
        .await
    {
        Ok(Some(ResolvedPath::Directory { id, .. })) => reply(
            StatusCode::OK,
            json!({"kind":"directory", "id":id.to_string(), "library_serial":serial}),
        ),
        Ok(Some(ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            ..
        })) => {
            let digest = match copy.content_digest {
                ContentDigest::Unknown => json!(null),
                ContentDigest::Md5(bytes) => json!({"algorithm":"md5", "hex":hex(&bytes)}),
                ContentDigest::Sha256(bytes) => {
                    json!({"algorithm":"sha256", "hex":hex(&bytes)})
                }
            };
            reply(
                StatusCode::OK,
                json!({"kind":if copy.media.as_ref().is_some_and(|media| media.native_symlink) {"symlink"} else {"file"}, "id":id.to_string(), "library_serial":serial,
                    "size":head.size, "version":head.version,
                    "volume_id":copy.volume_id.to_string(), "barcode":placement.barcode,
                    "digest":digest}),
            )
        }
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"路径不存在"})),
        Err(error) => catalog_error(error),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("写入 String");
    }
    result
}

async fn children(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let Some(&expected) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error": "带库未配置"}));
    };
    let path = match decoded_path(&request) {
        Ok(path) => path,
        Err(error) => return invalid(error),
    };
    let limit = match query_value(&request, "limit") {
        Ok(Some(value)) => match value.parse::<usize>() {
            Ok(value @ 1..=100) => value,
            _ => return invalid("limit 必须在 1..=100"),
        },
        Ok(None) => 100,
        Err(error) => return invalid(error),
    };
    let after = match query_value(&request, "after") {
        Ok(Some(value)) if value.len() <= 1024 => match URL_SAFE_NO_PAD.decode(value) {
            Ok(value) => Some(value),
            Err(_) => return invalid("after 不是有效的 Base64URL"),
        },
        Ok(Some(_)) => return invalid("after 过长"),
        Ok(None) => None,
        Err(error) => return invalid(error),
    };
    match ctx
        .catalog
        .list_active_children_for_route(&serial, &path, after.as_deref(), limit, expected)
        .await
    {
        Ok(Some(page)) => reply(
            StatusCode::OK,
            json!({"library_serial":serial,
                "items":page.items.iter().map(|child| json!({
                    "name":std::str::from_utf8(&child.name).ok(),
                    "name_b64":URL_SAFE_NO_PAD.encode(&child.name),
                    "id":child.id.to_string(),
                })).collect::<Vec<_>>(),
                "next":page.next.map(|cursor| URL_SAFE_NO_PAD.encode(cursor))}),
        ),
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"目录不存在"})),
        Err(error) => catalog_error(error),
    }
}

fn metadata_router(ctx: Arc<Context>) -> Router {
    Router::new()
        .route("/readyz", get(ready))
        .route("/v1/fdb/stat/{serial}", get(stat))
        .route("/v1/fdb/children/{serial}", get(children))
        .with_state(ctx)
}

pub fn serve(
    listen: &str,
    catalog: ManagedCatalog,
    routes: BTreeMap<String, ActiveLibraryRoute>,
) -> std::io::Result<()> {
    http::serve_router(
        listen,
        metadata_router(Arc::new(Context {
            catalog: Arc::new(catalog),
            routes,
            owner: None,
        })),
    )
}

/// 阶段性 owner 只读入口；不开放旧 FileService 的目录或写入 API。
pub fn owner_router(
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    peers: HashMap<u64, String>,
) -> Router {
    let ctx = Arc::new(Context {
        catalog,
        routes,
        owner: Some(OwnerReads {
            libraries,
            peers,
            requests: Arc::new(Semaphore::new(32)),
        }),
    });
    metadata_router(ctx.clone()).merge(
        Router::new()
            .route("/v1/fdb/files/{serial}", get(content))
            .with_state(ctx),
    )
}

pub fn serve_owner(
    listen: &str,
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    peers: HashMap<u64, String>,
) -> std::io::Result<()> {
    http::serve_router(listen, owner_router(catalog, routes, libraries, peers))
}

fn content_scope(request: &Request) -> Result<(String, String, Option<String>), &'static str> {
    let path = String::from_utf8(decoded_path(request)?).map_err(|_| "路径不是 UTF-8")?;
    let pool = query_value(request, "pool")?.ok_or("缺少池 UUID pool")?;
    let pool = uuid::Uuid::parse_str(pool)
        .map_err(|_| "pool 必须是 UUID")?
        .to_string();
    if request.headers().get_all(header::RANGE).iter().count() > 1 {
        return Err("Range 重复");
    }
    // 无 validator 可供核对 If-Range 时，安全地返回完整内容。
    let range = if request.headers().contains_key(header::IF_RANGE) {
        None
    } else {
        request
            .headers()
            .get(header::RANGE)
            .map(|value| value.to_str().map(str::to_owned).map_err(|_| "Range 无效"))
            .transpose()?
    };
    Ok((path, pool, range))
}

fn held_file(file: std::fs::File, range: Option<&str>, permit: OwnedSemaphorePermit) -> Response {
    let mut response = Response::new(Body::empty());
    if let Err(error) = http::respond_file(&mut response, file, range) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"read_failed", "detail":error.to_string()}),
        );
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(http::LimitedBody {
            body,
            _permit: permit,
        }),
    )
}

async fn content(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    // axum 的 get 会自动接受 HEAD；此阶段不因 HEAD 触发整盘索引核验或读带。
    if request.method() != axum::http::Method::GET {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"method_not_allowed"}),
        );
    }
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    };
    let (path, pool, range) = match content_scope(&request) {
        Ok(scope) => scope,
        Err(error) => return invalid(error),
    };
    let Some(owner) = &ctx.owner else {
        return unavailable("未启用 owner 内容读取");
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
            Err(_) => reply(StatusCode::GATEWAY_TIMEOUT, json!({"error":"read_timeout"})),
        };
    }
    // FDB 目录负责区分缺失、目录和错池；执行线程会在读带前后重新解析并核对副本。
    match ctx
        .catalog
        .resolve_active_metadata_for_route(&serial, path.as_bytes(), route)
        .await
    {
        Ok(Some(ResolvedPath::File {
            placement, copy, ..
        })) if placement.pool_id.to_string() == pool => {
            if copy
                .media
                .as_ref()
                .is_some_and(|media| media.native_symlink)
            {
                return reply(StatusCode::CONFLICT, json!({"error":"is_symlink"}));
            }
        }
        Ok(Some(ResolvedPath::Directory { .. })) => {
            return reply(StatusCode::CONFLICT, json!({"error":"is_directory"}));
        }
        Ok(_) => return reply(StatusCode::NOT_FOUND, json!({"error":"not_found"})),
        Err(error) => return catalog_error(error),
    }
    read_content(library.clone(), path, pool, range, permit).await
}

async fn read_content(
    library: Arc<HttpContext>,
    path: String,
    pool: String,
    range: Option<String>,
    permit: OwnedSemaphorePermit,
) -> Response {
    let timeout = library.wait_timeout;
    // 超时或断开不取消已经执行的 CDB。阻塞任务仍持有许可直到执行器回执，
    // 防止慢设备使 HTTP 超时后不断向无界 mpsc 队列追加请求。
    let pending = tokio::task::spawn_blocking(move || {
        let result = (|| {
            let file = library
                .files
                .read_spool()
                .map_err(|error| ReadError::Busy(format!("读取暂存不可用: {error}")))?;
            let (reply, received) = std::sync::mpsc::channel();
            library
                .exec
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .send(ExecRequest::Scoped {
                    pool,
                    lane: 0,
                    request: Box::new(ExecRequest::ReadTo { path, file, reply }),
                })
                .map_err(|_| ReadError::Busy("owner 执行线程已停止".into()))?;
            received
                .recv()
                .map_err(|_| ReadError::Busy("owner 读取回执已断开".into()))?
        })();
        (result, permit)
    });
    match tokio::time::timeout(timeout, pending).await {
        Ok(Ok((Ok(file), permit))) => held_file(file, range.as_deref(), permit),
        Ok(Ok((Err(error), _permit))) => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"read_unavailable", "detail":error.to_string()}),
        ),
        Ok(Err(_)) => unavailable("owner 读取任务异常"),
        Err(_) => reply(StatusCode::GATEWAY_TIMEOUT, json!({"error":"read_timeout"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    #[test]
    fn content_requires_explicit_pool_and_unambiguous_path_and_range() {
        let pool = uuid::Uuid::new_v4();
        let request = |query: &str| {
            Request::builder()
                .uri(format!("/v1/fdb/files/S?{query}"))
                .body(Body::empty())
                .unwrap()
        };
        let query = format!("pool={pool}&path=L2E");
        assert_eq!(
            content_scope(&request(&query)).unwrap(),
            ("/a".into(), pool.to_string(), None)
        );
        for query in [
            "path=L2E".into(),
            "pool=bad&path=L2E".into(),
            format!("{query}&path=L2I"),
            format!("{query}&pool={pool}"),
            format!("pool={pool}&path=_w"),
        ] {
            assert!(content_scope(&request(&query)).is_err(), "{query}");
        }
        let mut repeated = request(&query);
        repeated
            .headers_mut()
            .append(header::RANGE, "bytes=0-1".parse().unwrap());
        repeated
            .headers_mut()
            .append(header::RANGE, "bytes=2-3".parse().unwrap());
        assert!(content_scope(&repeated).is_err());
        repeated.headers_mut().remove(header::RANGE);
        repeated
            .headers_mut()
            .insert(header::RANGE, "bytes=0-1".parse().unwrap());
        repeated
            .headers_mut()
            .insert(header::IF_RANGE, "unverifiable".parse().unwrap());
        assert_eq!(content_scope(&repeated).unwrap().2, None);
    }

    #[test]
    fn timed_out_read_keeps_its_queue_slot_until_executor_finishes() {
        use std::sync::{Mutex, mpsc};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root =
                std::env::temp_dir().join(format!("fdb-http-timeout-{}", uuid::Uuid::new_v4()));
            let files = super::super::files::FileService::new(root.clone()).unwrap();
            let (exec, received) = mpsc::channel();
            let (node, _) = mpsc::channel();
            let library = Arc::new(HttpContext {
                files,
                status: Arc::new(Mutex::new(Default::default())),
                exec: Mutex::new(exec),
                node: Mutex::new(node),
                client_addrs: HashMap::new(),
                wait_timeout: std::time::Duration::from_millis(30),
            });
            let slots = Arc::new(Semaphore::new(1));
            let response = read_content(
                library,
                "/a".into(),
                uuid::Uuid::new_v4().to_string(),
                None,
                slots.clone().try_acquire_owned().unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
            assert_eq!(
                slots.available_permits(),
                0,
                "HTTP 超时不能释放尚在执行器中的请求名额"
            );
            let request = received
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let ExecRequest::Scoped { request, .. } = request else {
                panic!("没有池作用域");
            };
            let ExecRequest::ReadTo { reply, .. } = *request else {
                panic!("没有返回文件的读取");
            };
            reply.send(Err(ReadError::Busy("完成".into()))).unwrap();
            let returned = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                slots.clone().acquire_owned(),
            )
            .await
            .unwrap()
            .unwrap();
            drop(returned);
            assert_eq!(slots.available_permits(), 1);
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    fn verified_file_range_holds_admission_through_download() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root =
                std::env::temp_dir().join(format!("fdb-http-range-{}", uuid::Uuid::new_v4()));
            let files = super::super::files::FileService::new(root.clone()).unwrap();
            for (range, code, expected, content_range) in [
                (None, 200, &b"abcdef"[..], None),
                (Some("bytes=1-3"), 206, &b"bcd"[..], Some("bytes 1-3/6")),
                (Some("bytes=-2"), 206, &b"ef"[..], Some("bytes 4-5/6")),
                (Some("bytes=6-"), 416, &b""[..], Some("bytes */6")),
            ] {
                use std::io::Write as _;
                let mut file = files.read_spool().unwrap();
                file.write_all(b"abcdef").unwrap();
                let slots = Arc::new(Semaphore::new(1));
                let permit = slots.clone().try_acquire_owned().unwrap();
                let response = held_file(file, range, permit);
                assert_eq!(response.status().as_u16(), code);
                assert_eq!(
                    response
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .map(|v| v.to_str().unwrap()),
                    content_range
                );
                assert_eq!(slots.available_permits(), 0);
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                if code != 416 {
                    assert_eq!(&bytes[..], expected);
                }
                assert_eq!(slots.available_permits(), 1);
            }
            std::fs::remove_dir_all(root).unwrap();
        });
    }
}
