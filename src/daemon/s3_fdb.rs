//! Anonymous S3 over the Active FDB catalog; writes require an explicit writable owner.
use super::*;
use crate::managed_catalog::{
    ActiveLibraryRoute, CatalogError, ContentDigest, ManagedCatalog, ObjectListRequest,
    ResolvedPath,
};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

#[path = "s3_fdb_rename.rs"]
mod rename;
#[path = "s3_fdb_write.rs"]
mod write;

type S3Result<T> = Result<T, S3Error>;

#[derive(Clone)]
struct FdbS3 {
    writable: bool,
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
    requests: Arc<Semaphore>,
    storage: super::super::fdb_read::ReadStorageBudget,
}

pub fn router(
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
    writable: bool,
) -> Router {
    Router::new().fallback(dispatch).with_state(FdbS3 {
        writable,
        catalog,
        routes,
        libraries,
        gateway,
        peers,
        requests: Arc::new(Semaphore::new(8)),
        storage: Default::default(),
    })
}
#[allow(clippy::too_many_arguments)]
pub fn serve(
    listen: &str,
    catalog: Arc<ManagedCatalog>,
    routes: BTreeMap<String, ActiveLibraryRoute>,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
    writable: bool,
    storage: super::super::fdb_read::ReadStorageBudget,
) -> std::io::Result<()> {
    http::serve_s3_router(
        listen,
        Router::new().fallback(dispatch).with_state(FdbS3 {
            writable,
            catalog,
            routes,
            libraries,
            gateway,
            peers,
            requests: Arc::new(Semaphore::new(8)),
            storage,
        }),
    )
}
fn unavailable(reason: impl Into<String>) -> S3Error {
    S3Error::new(503, "ServiceUnavailable", reason)
}
fn catalog_error(error: CatalogError) -> S3Error {
    match error {
        CatalogError::Invalid(reason) => S3Error::new(400, "InvalidArgument", reason),
        _ => unavailable(format!("FDB 目录暂不可用：{error}")),
    }
}

async fn buckets(s: &FdbS3) -> S3Result<BTreeMap<String, (String, Uuid, u64)>> {
    let mut all = BTreeMap::new();
    for (serial, route) in &s.routes {
        let mut after = None;
        loop {
            let page = s
                .catalog
                .list_active_pools_for_route(serial, *route, after.as_deref(), 100)
                .await
                .map_err(catalog_error)?;
            for pool in page.items {
                if all.len() >= 4096 {
                    return Err(unavailable("S3 bucket 列表超过本入口上限"));
                }
                all.insert(
                    bucket_name(serial, &pool.id.to_string()),
                    (serial.clone(), pool.id, pool.created_at_millis),
                );
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    check_routes(s).await?;
    Ok(all)
}
async fn check_routes(s: &FdbS3) -> S3Result<()> {
    for (serial, route) in &s.routes {
        if s.catalog
            .active_library_route(serial)
            .await
            .map_err(catalog_error)?
            != Some(*route)
        {
            return Err(unavailable("FDB Active 路由已改变，需要重新启动 S3 入口"));
        }
    }
    Ok(())
}
fn local_round(ctx: &HttpContext) -> Option<u64> {
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner());
    let owner = status.executor.as_ref()?;
    if status.shutting_down
        || status.catalog_pending
        || !status.executor_ready
        || !owner.fenced
        || owner.node != status.id
    {
        return None;
    }
    // The legacy catalog stays quiesced after Active cutover. FDB qualification
    // is the exact frozen route plus owner epoch, checked separately below.
    ctx.files.serving_deadline(owner.round).map(|_| owner.round)
}
async fn check_owner(s: &FdbS3, serial: &str, round: u64) -> S3Result<()> {
    let ctx = s
        .libraries
        .get(serial)
        .ok_or_else(|| unavailable("目标库未配置执行器"))?;
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let route = s.routes[serial];
    if status.catalog_pending
        || status.catalog_frozen_index != Some(route.frozen_index)
        || local_round(ctx) != Some(round)
        || s.catalog
            .active_library_owner(serial)
            .await
            .map_err(catalog_error)?
            != Some((route, round))
    {
        return Err(unavailable("FDB owner 或多数派 Serving 许可已改变"));
    }
    Ok(())
}

async fn dispatch(State(s): State<FdbS3>, req: Request) -> Response {
    let head = req.method() == "HEAD";
    let writable = s.writable;
    match dispatch_inner(s, req).await {
        Ok(response) => response,
        Err(error) => {
            let mut response = error.response(head);
            if error.status == 405 {
                response.headers_mut().insert(
                    header::ALLOW,
                    if writable {
                        "GET, HEAD, PUT, DELETE"
                    } else {
                        "GET, HEAD"
                    }
                    .parse()
                    .unwrap(),
                );
            }
            response
        }
    }
}
async fn dispatch_inner(s: FdbS3, req: Request) -> S3Result<Response> {
    let q = query(req.uri().query().unwrap_or(""))?;
    let is_rename = q.contains_key("renameObject");
    if !matches!(req.method().as_str(), "GET" | "HEAD")
        && !(matches!(req.method().as_str(), "PUT" | "DELETE") && s.writable)
    {
        return Err(S3Error::new(
            405,
            "MethodNotAllowed",
            "此 FDB S3 owner 不提供该写入操作",
        ));
    }
    if is_rename {
        if req.method() != "PUT" {
            return Err(S3Error::new(
                405,
                "MethodNotAllowed",
                "RenameObject 必须使用 PUT",
            ));
        }
        rename::validate(&req, &q)?;
    } else {
        supported_request_with_time(req.headers(), &q, true)?;
        if matches!(req.method().as_str(), "PUT" | "DELETE") {
            write::validate(&req, &q)?;
        }
    }
    let (bucket, key) = req
        .uri()
        .path()
        .strip_prefix('/')
        .unwrap_or(req.uri().path())
        .split_once('/')
        .unwrap_or((req.uri().path().trim_start_matches('/'), ""));
    let bucket = bucket.to_owned();
    let key = key.to_owned();
    let permit = s
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| S3Error::new(503, "SlowDown", "S3 并发请求已达上限"))?;
    let all = buckets(&s).await?;
    let scope = if bucket.is_empty() {
        None
    } else {
        Some(
            all.get(&bucket)
                .cloned()
                .ok_or_else(|| S3Error::new(404, "NoSuchBucket", "指定 bucket 不存在"))?,
        )
    };
    let ctx = match &scope {
        None => s.gateway.clone(),
        Some((serial, _, _)) => s
            .libraries
            .get(serial)
            .cloned()
            .ok_or_else(|| unavailable("目标带库未配置"))?,
    };
    let rename_input = if is_rename {
        let (serial, pool, _) = scope
            .as_ref()
            .ok_or_else(|| S3Error::new(400, "InvalidArgument", "RenameObject 缺少 bucket"))?;
        Some(rename::input(
            &req,
            &bucket,
            serial,
            s.routes[serial],
            *pool,
        )?)
    } else {
        None
    };
    let req = if is_rename {
        rename::empty_body(req, ctx.wait_timeout).await?
    } else {
        req
    };
    if let Some(input) = &rename_input
        && let Some(response) = rename::known(&s, input).await?
    {
        return Ok(response);
    }
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let owner = status
        .executor
        .as_ref()
        .filter(|e| {
            status.executor_ready
                && !status.shutting_down
                && !status.catalog_pending
                && e.fenced
                && e.node != 0
        })
        .ok_or_else(|| unavailable("S3 owner 正在恢复"))?;
    if owner.node != status.id {
        if req.headers().contains_key("x-tapers-forwarded") {
            return Err(unavailable("S3 owner 已变化，请重试"));
        }
        let address = s
            .peers
            .get(&owner.node)
            .ok_or_else(|| unavailable("未配置 owner S3 地址"))?;
        let response = tokio::time::timeout(
            ctx.wait_timeout,
            super::super::gateway::forward_request(req, address, permit, "s3-fdb"),
        )
        .await
        .map_err(|_| S3Error::new(504, "RequestTimeout", "S3 转发超时"))?;
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/json")
        {
            return Err(unavailable("S3 owner 转发失败"));
        }
        return Ok(response);
    }
    let round = owner.round;
    if let Some((serial, pool, _)) = scope {
        check_owner(&s, &serial, round).await?;
        if let Some(input) = &rename_input {
            return rename::execute(&s, input, round).await;
        }
        if matches!(req.method().as_str(), "PUT" | "DELETE") {
            if key.is_empty() {
                return Err(S3Error::new(
                    501,
                    "NotImplemented",
                    "请通过管理接口维护 pool",
                ));
            }
            let path = object_path(&key)?;
            return write::execute(s, serial, pool, path, round, req, permit).await;
        }
        let response = if key.is_empty() {
            if req.method() == "HEAD" {
                xml_response(200, "", true)
            } else if q.contains_key("location") {
                if q.keys().any(|k| !matches!(k.as_str(), "location" | "x-id")) {
                    return Err(S3Error::new(
                        400,
                        "InvalidArgument",
                        "location 不接受列举参数",
                    ));
                }
                xml_response(200, &format!("<LocationConstraint xmlns=\"{NS}\"/>"), false)
            } else {
                list(&s, &serial, pool, &bucket, &q).await?
            }
        } else {
            if q.keys().any(|k| k != "x-id") {
                return Err(S3Error::new(
                    501,
                    "NotImplemented",
                    "对象请求不支持此查询参数",
                ));
            }
            return get(
                &s,
                &serial,
                pool,
                &format!("/{pool}{}", object_path(&key)?),
                req.headers(),
                req.method() == "HEAD",
                round,
                permit,
            )
            .await;
        };
        check_owner(&s, &serial, round).await?;
        Ok(response)
    } else {
        if req.method() != "GET" || q.keys().any(|k| k != "x-id") {
            return Err(S3Error::new(501, "NotImplemented", "仅支持 ListBuckets"));
        }
        if local_round(&ctx) != Some(round) {
            return Err(unavailable("统一入口多数派许可已失效"));
        }
        let mut names = String::new();
        for (name, (_, _, created)) in &all {
            let time = chrono::DateTime::from_timestamp_millis(*created as i64)
                .ok_or_else(|| unavailable("池创建时间无效"))?
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            names.push_str(&format!(
                "<Bucket><Name>{}</Name><CreationDate>{time}</CreationDate></Bucket>",
                xml(name)
            ));
        }
        Ok(xml_response(
            200,
            &format!(
                "<ListAllMyBucketsResult xmlns=\"{NS}\"><Owner><ID>tape-rs</ID><DisplayName>tape-rs</DisplayName></Owner><Buckets>{names}</Buckets></ListAllMyBucketsResult>"
            ),
            false,
        ))
    }
}

fn tag(namespace: Uuid, id: Uuid, version: u64) -> String {
    format!("\"fdb-{}-{}-{version}\"", namespace.simple(), id.simple())
}
fn token(bucket: &str, namespace: Uuid, prefix: &str, delimiter: &str, last: &str) -> String {
    URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&(2, bucket, namespace.to_string(), prefix, delimiter, last)).unwrap(),
    )
}
fn cursor(
    value: &str,
    bucket: &str,
    namespace: Uuid,
    prefix: &str,
    delimiter: &str,
) -> S3Result<String> {
    let bad = || {
        S3Error::new(
            400,
            "InvalidArgument",
            "续页 token 不属于本次查询或 Active namespace",
        )
    };
    if value.len() > 16384 {
        return Err(bad());
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| bad())?;
    let (version, b, ns, p, d, last): (u8, String, String, String, String, String) =
        serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if version != 2
        || b != bucket
        || ns != namespace.to_string()
        || p != prefix
        || d != delimiter
        || last.len() > 2048
        || last.contains('\0')
    {
        return Err(bad());
    }
    Ok(last)
}
async fn list(
    s: &FdbS3,
    serial: &str,
    pool: Uuid,
    bucket: &str,
    q: &BTreeMap<String, String>,
) -> S3Result<Response> {
    let v2 = match q.get("list-type").map(String::as_str) {
        None => false,
        Some("2") => true,
        _ => return Err(S3Error::new(400, "InvalidArgument", "list-type 须为 2")),
    };
    if q.keys().any(|k| {
        !matches!(
            k.as_str(),
            "list-type"
                | "max-keys"
                | "prefix"
                | "delimiter"
                | "encoding-type"
                | "continuation-token"
                | "start-after"
                | "marker"
                | "fetch-owner"
                | "x-id"
        )
    }) || (v2 && q.contains_key("marker"))
        || (!v2 && (q.contains_key("continuation-token") || q.contains_key("start-after")))
        || q.get("fetch-owner")
            .is_some_and(|v| !matches!(v.as_str(), "true" | "false"))
    {
        return Err(S3Error::new(400, "InvalidArgument", "列举参数组合无效"));
    }
    let max = q
        .get("max-keys")
        .map(|s| {
            s.parse::<usize>()
                .ok()
                .filter(|n| *n <= 1000)
                .ok_or_else(|| S3Error::new(400, "InvalidArgument", "max-keys 须为 0..1000"))
        })
        .transpose()?
        .unwrap_or(1000);
    let prefix = q.get("prefix").map(String::as_str).unwrap_or("");
    let delimiter = q.get("delimiter").map(String::as_str).unwrap_or("");
    if !matches!(delimiter, "" | "/") {
        return Err(S3Error::new(501, "NotImplemented", "delimiter 只支持 /"));
    }
    let encoded = match q.get("encoding-type").map(String::as_str) {
        None => false,
        Some("url") => true,
        _ => {
            return Err(S3Error::new(
                400,
                "InvalidArgument",
                "encoding-type 须为 url",
            ));
        }
    };
    let namespace = s.routes[serial].route.namespace;
    let start = q
        .get(if v2 { "start-after" } else { "marker" })
        .map(String::as_str)
        .unwrap_or("");
    let after = q
        .get("continuation-token")
        .map(|t| cursor(t, bucket, namespace, prefix, delimiter))
        .transpose()?
        .unwrap_or_else(|| start.to_owned());
    if !encoded
        && [prefix, delimiter, start]
            .iter()
            .any(|v| !v.chars().all(xml_character))
    {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "列举参数含非法 XML 字符，请使用 encoding-type=url",
        ));
    }
    let page = s
        .catalog
        .list_active_objects_for_route(
            serial,
            s.routes[serial],
            ObjectListRequest {
                pool,
                prefix,
                delimiter,
                max_keys: max,
                start_after: &after,
            },
        )
        .await
        .map_err(catalog_error)?;
    let esc = |v: &str| {
        xml(&if encoded {
            crate::client::encode_query(v)
        } else {
            v.to_owned()
        })
    };
    let mut body = format!(
        "<ListBucketResult xmlns=\"{NS}\"><Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{max}</MaxKeys><IsTruncated>{}</IsTruncated>",
        xml(bucket),
        esc(prefix),
        page.next.is_some()
    );
    if !delimiter.is_empty() {
        body.push_str(&format!("<Delimiter>{}</Delimiter>", esc(delimiter)));
    }
    if encoded {
        body.push_str("<EncodingType>url</EncodingType>");
    }
    if v2 {
        body.push_str(&format!("<KeyCount>{}</KeyCount>", page.items.len()));
        if let Some(t) = q.get("continuation-token") {
            body.push_str(&format!(
                "<ContinuationToken>{}</ContinuationToken>",
                xml(t)
            ));
        }
        if !start.is_empty() {
            body.push_str(&format!("<StartAfter>{}</StartAfter>", esc(start)));
        }
        if let Some(next) = &page.next {
            let last = std::str::from_utf8(next).map_err(|_| unavailable("非 UTF-8 对象游标"))?;
            body.push_str(&format!(
                "<NextContinuationToken>{}</NextContinuationToken>",
                token(bucket, namespace, prefix, delimiter, last)
            ));
        }
    } else {
        body.push_str(&format!("<Marker>{}</Marker>", esc(start)));
        if let Some(next) = &page.next {
            body.push_str(&format!(
                "<NextMarker>{}</NextMarker>",
                esc(std::str::from_utf8(next).map_err(|_| unavailable("非 UTF-8 对象游标"))?)
            ));
        }
    }
    for item in page.items {
        if !encoded && !item.key.chars().all(xml_character) {
            return Err(S3Error::new(
                400,
                "InvalidArgument",
                "对象名含非法 XML 字符，请使用 encoding-type=url",
            ));
        }
        if let Some((id, head)) = item.file {
            body.push_str(&format!("<Contents><Key>{}</Key><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>",esc(&item.key),xml(&tag(namespace,id,head.version)),head.size));
            if let Some(time) = item
                .modified
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            {
                body.push_str(&format!(
                    "<LastModified>{}</LastModified>",
                    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                ));
            }
            if !v2 || q.get("fetch-owner").is_some_and(|v| v == "true") {
                body.push_str("<Owner><ID>tape-rs</ID><DisplayName>tape-rs</DisplayName></Owner>");
            }
            body.push_str("</Contents>");
        } else {
            body.push_str(&format!(
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                esc(&item.key)
            ));
        }
    }
    body.push_str("</ListBucketResult>");
    Ok(xml_response(200, &body, false))
}

async fn metadata(s: &FdbS3, serial: &str, pool: Uuid, path: &str) -> S3Result<ResolvedPath> {
    match s
        .catalog
        .resolve_active_metadata_for_route(serial, path.as_bytes(), s.routes[serial])
        .await
        .map_err(catalog_error)?
    {
        Some(value @ ResolvedPath::File { .. }) => {
            let ResolvedPath::File {
                ref placement,
                ref copy,
                ..
            } = value
            else {
                unreachable!()
            };
            if placement.pool_id != pool || placement.library_serial != serial {
                return Err(S3Error::new(404, "NoSuchKey", "指定池中不存在此对象"));
            }
            if copy.media.as_ref().is_some_and(|m| m.native_symlink) {
                return Err(S3Error::new(
                    409,
                    "InvalidObjectState",
                    "S3 不跟随原生符号链接",
                ));
            }
            Ok(value)
        }
        _ => Err(S3Error::new(404, "NoSuchKey", "对象不存在或是目录")),
    }
}

fn http_time(value: &str) -> Option<i64> {
    let time = match httpdate::parse_http_date(value) {
        Ok(time) => time,
        Err(_) => {
            return chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .filter(|date| date.timestamp() < 0)
                .map(|date| date.timestamp());
        }
    };
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(time) => i64::try_from(time.as_secs()).ok(),
        Err(error) => i64::try_from(error.duration().as_secs())
            .ok()
            .map(|time| -time),
    }
}

fn condition_status(headers: &HeaderMap, etag: &str, modified: Option<i64>) -> S3Result<u16> {
    let if_match = header_text(headers, "if-match")?;
    let if_none = header_text(headers, "if-none-match")?;
    let matches = |value: &str, weak: bool| {
        value.split(',').any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag == etag || (weak && tag.strip_prefix("W/") == Some(etag))
        })
    };
    if let Some(value) = if_match {
        if !matches(value, false) {
            return Ok(412);
        }
    } else if let Some(date) = header_text(headers, "if-unmodified-since")?.and_then(http_time)
        && modified.ok_or_else(|| unavailable("对象修改时间缺失"))? > date
    {
        return Ok(412);
    }
    if let Some(value) = if_none {
        if matches(value, true) {
            return Ok(304);
        }
    } else if let Some(date) = header_text(headers, "if-modified-since")?.and_then(http_time)
        && modified.ok_or_else(|| unavailable("对象修改时间缺失"))? <= date
    {
        return Ok(304);
    }
    Ok(200)
}
#[allow(clippy::too_many_arguments)]
async fn get(
    s: &FdbS3,
    serial: &str,
    pool: Uuid,
    path: &str,
    headers: &HeaderMap,
    head_only: bool,
    round: u64,
    permit: OwnedSemaphorePermit,
) -> S3Result<Response> {
    for name in [
        header::RANGE,
        header::IF_MATCH,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_UNMODIFIED_SINCE,
    ] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(S3Error::new(
                400,
                "InvalidArgument",
                "条件或 Range 请求头重复",
            ));
        }
    }
    if header_text(headers, "x-amz-checksum-mode")?.is_some_and(|v| v != "ENABLED") {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "checksum-mode 须为 ENABLED",
        ));
    }
    let before = metadata(s, serial, pool, path).await?;
    let ResolvedPath::File {
        id,
        ref head,
        ref object_metadata,
        ..
    } = before
    else {
        unreachable!()
    };
    let etag = tag(s.routes[serial].route.namespace, id, head.version);
    let modified = object_metadata.as_ref().and_then(|meta| meta.modified);
    let status = condition_status(headers, &etag, modified)?;
    let range = header_text(headers, "range")?;
    let parsed = range
        .filter(|_| status == 200)
        .map(|r| {
            http::parse_range(r, head.size)
                .ok_or_else(|| S3Error::new(400, "InvalidArgument", "只支持单个字节 Range"))
        })
        .transpose()?;
    let mut response = if status == 412 {
        S3Error::new(412, "PreconditionFailed", "对象不满足请求条件").response(head_only)
    } else if status == 304 {
        xml_response(304, "", true)
    } else if parsed == Some(Err(())) {
        let mut r = S3Error::new(416, "InvalidRange", "请求范围超出对象长度").response(head_only);
        r.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes */{}", head.size).parse().unwrap(),
        );
        r
    } else if head_only {
        let mut r = xml_response(200, "", true);
        let (start, end) = parsed.and_then(Result::ok).unwrap_or((0, head.size));
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, (end - start).into());
        if parsed.is_some() {
            *r.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
            r.headers_mut().insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{}", end - 1, head.size)
                    .parse()
                    .unwrap(),
            );
        }
        r
    } else {
        check_owner(s, serial, round).await?;
        let r = super::super::fdb_http::read_content(
            s.libraries[serial].clone(),
            path.into(),
            pool.to_string(),
            range.map(str::to_owned),
            permit,
        )
        .await;
        if !matches!(r.status().as_u16(), 200 | 206) {
            return Err(S3Error::new(
                if r.status().as_u16() == 504 { 504 } else { 503 },
                "ServiceUnavailable",
                "FDB owner 内容读取失败，请重试",
            ));
        }
        if metadata(s, serial, pool, path).await? != before {
            return Err(S3Error::new(
                409,
                "OperationAborted",
                "读取期间对象版本已改变，请重试",
            ));
        }
        let (start, end) = parsed.and_then(Result::ok).unwrap_or((0, head.size));
        if r.headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            != Some(end - start)
        {
            return Err(unavailable("读取暂存长度与 FDB 对象记录不符"));
        }
        r
    };
    check_owner(s, serial, round).await?;
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().unwrap());
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    response.headers_mut().insert(
        "x-amz-request-id",
        Uuid::new_v4().simple().to_string().parse().unwrap(),
    );
    if let Some(time) = modified.and_then(|time| chrono::DateTime::from_timestamp(time, 0)) {
        response.headers_mut().insert(
            header::LAST_MODIFIED,
            time.format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
                .parse()
                .expect("HTTP date"),
        );
    }
    if response.status().as_u16() == 304 {
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::CONTENT_TYPE);
        if let Some(metadata) = object_metadata {
            for name in ["cache-control", "expires"] {
                if let Some(value) = metadata.headers.get(name) {
                    response.headers_mut().insert(
                        axum::http::HeaderName::from_static(name),
                        value.parse().expect("validated metadata value"),
                    );
                }
            }
        }
    }
    if matches!(response.status().as_u16(), 200 | 206) {
        // The internal download policy is not stored S3 object metadata.
        response.headers_mut().remove(header::CACHE_CONTROL);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        if let Some(metadata) = object_metadata {
            for (name, value) in &metadata.headers {
                if name.starts_with("x-amz-checksum-") {
                    continue;
                }
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes())
                        .expect("validated metadata name"),
                    value.parse().expect("validated metadata value"),
                );
            }
        }
        if range.is_none()
            && header_text(headers, "x-amz-checksum-mode")? == Some("ENABLED")
            && let ResolvedPath::File { ref copy, .. } = before
            && let ContentDigest::Sha256(bytes) = copy.content_digest
        {
            if let Some(crc32) = object_metadata
                .as_ref()
                .and_then(|metadata| checksum::stored_crc32(&metadata.headers, &bytes))
            {
                response
                    .headers_mut()
                    .insert("x-amz-checksum-crc32", crc32.parse().expect("Base64"));
            } else {
                response.headers_mut().insert(
                    "x-amz-checksum-sha256",
                    STANDARD.encode(bytes).parse().unwrap(),
                );
            }
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn date_conditions_obey_etag_precedence_and_second_precision() {
        let tag = "\"opaque\"";
        let date = "Sat, 31 Oct 2026 01:02:03 GMT";
        let old = "Thu, 01 Jan 1970 00:00:00 GMT";
        let now = http_time(date).unwrap();
        for (headers, expected) in [
            (vec![("if-modified-since", date)], 304),
            (vec![("if-modified-since", old)], 200),
            (vec![("if-unmodified-since", date)], 200),
            (vec![("if-unmodified-since", old)], 412),
            (vec![("if-match", tag), ("if-unmodified-since", old)], 200),
            (
                vec![("if-match", "\"wrong\""), ("if-modified-since", date)],
                412,
            ),
            (
                vec![("if-none-match", tag), ("if-modified-since", old)],
                304,
            ),
            (
                vec![("if-none-match", "\"wrong\""), ("if-modified-since", date)],
                200,
            ),
            (vec![("if-none-match", "W/\"opaque\"")], 304),
            (vec![("if-match", "W/\"opaque\"")], 412),
            (vec![("if-modified-since", "invalid")], 200),
        ] {
            let map = headers
                .into_iter()
                .map(|(name, value)| {
                    (
                        axum::http::HeaderName::from_static(name),
                        value.parse().unwrap(),
                    )
                })
                .collect();
            assert_eq!(condition_status(&map, tag, Some(now)).unwrap(), expected);
        }
        let map = [(header::IF_MODIFIED_SINCE, date.parse().unwrap())]
            .into_iter()
            .collect();
        assert!(condition_status(&map, tag, None).is_err());
        assert_eq!(http_time("Sat Oct 31 01:02:03 2026"), Some(now));
        assert_eq!(http_time("Saturday, 31-Oct-26 01:02:03 GMT"), Some(now));
        assert_eq!(http_time("Wed, 31 Dec 1969 23:59:59 GMT"), Some(-1));
    }
    #[test]
    fn fdb_tokens_bind_namespace_bucket_prefix_and_delimiter() {
        let ns = Uuid::new_v4();
        let t = token("bucket", ns, "a", "/", "a/b");
        assert_eq!(cursor(&t, "bucket", ns, "a", "/").unwrap(), "a/b");
        assert!(cursor(&t, "other", ns, "a", "/").is_err());
        assert!(cursor(&t, "bucket", Uuid::new_v4(), "a", "/").is_err());
        assert!(cursor(&t, "bucket", ns, "b", "/").is_err());
        assert!(cursor(&t, "bucket", ns, "a", "").is_err());
        assert!(
            cursor(
                &encode_token("bucket", "a", "/", "a/b"),
                "bucket",
                ns,
                "a",
                "/"
            )
            .is_err()
        );
        assert!(cursor("broken", "bucket", ns, "a", "/").is_err());
        assert_ne!(tag(ns, Uuid::nil(), 1), tag(ns, Uuid::nil(), 2));
    }
}
