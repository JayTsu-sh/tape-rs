//! 匿名、path-style 的 S3 HTTP 入口。bucket 是 (带库序列号, pool UUID) 的稳定投影。
//! 数据写入复用 FileService；只有落带且任务记录获多数派确认才回复成功。
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::mpsc::channel;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, header, request::Parts},
    response::Response,
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use md5::Md5;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::executor::{ExecRequest, ReadError};
use super::files::{FileService, ServiceError, Stat, TaskStatus, is_directory};
use super::http::{self, HttpContext};
use crate::ltfs::index::{XATTR_MD5, Xattr};
use crate::ltfs::volume::FileMetadata;

const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const HEADERS_XATTR: &str = "tapers.s3.headers";
const REGION: &str = "us-east-1";

#[derive(Clone)]
struct S3 {
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
    requests: Arc<Semaphore>,
}

/// 每个节点都监听同一 S3 端口；VIP 与原 HTTP 入口共用已有的多数派许可。
pub fn serve(
    listen: &str,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
) -> std::io::Result<()> {
    http::serve_s3_router(
        listen,
        Router::new().fallback(dispatch).with_state(S3 {
            libraries,
            gateway,
            peers,
            requests: Arc::new(Semaphore::new(32)),
        }),
    )
}

/// 不依赖池名称，改名、owner 接管及多库同名/同 UUID 均不改变或混淆身份。
pub fn bucket_name(library: &str, pool: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((library.len() as u64).to_be_bytes());
    hash.update(library.as_bytes());
    hash.update(pool.as_bytes());
    format!("pool-{}", &format!("{:x}", hash.finalize())[..56])
}

fn buckets(s: &S3) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for (library, ctx) in &s.libraries {
        let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner());
        for pool in &status.pools {
            out.insert(
                bucket_name(library, &pool.uuid),
                (library.clone(), pool.uuid.clone()),
            );
        }
    }
    out
}

async fn dispatch(State(s): State<S3>, req: Request) -> Response {
    let head = req.method() == "HEAD";
    let query = match query(req.uri().query().unwrap_or("")) {
        Ok(q) => q,
        Err(e) => return e.response(head),
    };
    if let Err(e) = supported_request(req.headers(), &query) {
        return e.response(head);
    }
    let path = req.uri().path();
    let (name, key) = path
        .strip_prefix('/')
        .unwrap_or(path)
        .split_once('/')
        .unwrap_or((path.trim_start_matches('/'), ""));
    let root = name.is_empty();
    let all = buckets(&s);
    let (ctx, pool) = if root {
        if req.method() != "GET" || query.keys().any(|k| k != "x-id") {
            return error_response(501, "NotImplemented", "此 S3 操作尚未实现", head);
        }
        (s.gateway.clone(), None)
    } else {
        if key.is_empty() && !matches!(req.method().as_str(), "GET" | "HEAD") {
            return error_response(
                501,
                "NotImplemented",
                "请通过 HTTP 管理接口创建或删除 pool",
                head,
            );
        }
        let Some((library, pool)) = all.get(name) else {
            return error_response(404, "NoSuchBucket", "指定 bucket 不存在", head);
        };
        (s.libraries[library].clone(), Some(pool.clone()))
    };
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let owner = status
        .executor
        .as_ref()
        .filter(|_| status.executor_ready && !status.shutting_down && !status.catalog_pending)
        .map(|e| e.node)
        .unwrap_or(0);
    if owner == 0 {
        return error_response(503, "ServiceUnavailable", "入口或目标带库正在恢复", head);
    }
    if owner != status.id {
        if req.headers().contains_key("x-tapers-forwarded") {
            return error_response(503, "ServiceUnavailable", "owner 已变化，请稍后重试", head);
        }
        let Some(address) = s.peers.get(&owner) else {
            return error_response(503, "ServiceUnavailable", "未配置目标节点 S3 地址", head);
        };
        let Ok(permit) = s.requests.clone().try_acquire_owned() else {
            return error_response(503, "SlowDown", "S3 并发请求已达上限", head);
        };
        let response = super::gateway::forward_request(req, address, permit, "s3").await;
        if (response.status().is_client_error() || response.status().is_server_error())
            && response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v == "application/json")
        {
            return error_response(
                response.status().as_u16(),
                if response.status().as_u16() == 504 {
                    "RequestTimeout"
                } else {
                    "ServiceUnavailable"
                },
                "S3 转发失败；写入结果可能未定，请核对对象",
                head,
            );
        }
        return response;
    }
    if pool.is_none() {
        if http::ready_owner(&status, &ctx.files).is_none() {
            return error_response(503, "ServiceUnavailable", "多数派服务许可已失效", false);
        }
        // 既有 pool 未记录创建时间；epoch 表示未知，供要求该字段的客户端解析。
        let names = all
            .keys()
            .map(|n| format!("<Bucket><Name>{}</Name><CreationDate>1970-01-01T00:00:00Z</CreationDate></Bucket>", xml(n)))
            .collect::<String>();
        return xml_response(
            200,
            &format!(
                "<ListAllMyBucketsResult xmlns=\"{NS}\"><Owner><ID>tape-rs</ID><DisplayName>tape-rs</DisplayName></Owner><Buckets>{names}</Buckets></ListAllMyBucketsResult>"
            ),
            false,
        );
    }
    http::dispatch_s3_context(ctx, s.requests, req, pool.unwrap()).await
}

#[derive(Debug)]
struct S3Error {
    status: u16,
    code: &'static str,
    message: String,
}
impl S3Error {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn response(&self, head: bool) -> Response {
        error_response(self.status, self.code, &self.message, head)
    }
}
impl From<ServiceError> for S3Error {
    fn from(e: ServiceError) -> Self {
        let (status, code) = match &e {
            ServiceError::NotFound(_) | ServiceError::IsDirectory(_) => (404, "NoSuchKey"),
            ServiceError::Exists(_) | ServiceError::VersionMismatch(_) => {
                (412, "PreconditionFailed")
            }
            ServiceError::PathBusy(_) => (409, "OperationAborted"),
            ServiceError::BadPath(_) | ServiceError::NotDirectory(_) => (400, "InvalidArgument"),
            ServiceError::TooLarge { .. } => (400, "EntityTooLarge"),
            ServiceError::NoTape(_) | ServiceError::InsufficientCapacity { .. } => {
                (503, "ServiceUnavailable")
            }
            ServiceError::NotServing(_) | ServiceError::SwitchingTape(_) => {
                (503, "ServiceUnavailable")
            }
            ServiceError::NotWritable(_) => (403, "AccessDenied"),
            _ => (500, "InternalError"),
        };
        Self::new(status, code, e.to_string())
    }
}

pub(super) fn error_response(status: u16, code: &str, message: &str, head: bool) -> Response {
    xml_response(
        status,
        &format!(
            "<Error><Code>{}</Code><Message>{}</Message></Error>",
            xml(code),
            xml(message)
        ),
        head,
    )
}
fn xml_response(status: u16, value: &str, head: bool) -> Response {
    let body = if head {
        String::new()
    } else {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{value}")
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CONNECTION, "close")
        .header(
            "x-amz-request-id",
            uuid::Uuid::new_v4().simple().to_string(),
        )
        .header("x-amz-bucket-region", REGION)
        .body(Body::from(body))
        .unwrap()
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn xml_character(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}'
        | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}
fn decode(value: &str) -> Result<String, S3Error> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let digit = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            let a = bytes.next().and_then(digit);
            let b = bytes.next().and_then(digit);
            let (Some(a), Some(b)) = (a, b) else {
                return Err(S3Error::new(400, "InvalidURI", "百分号编码非法"));
            };
            out.push(a * 16 + b);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).map_err(|_| S3Error::new(400, "InvalidURI", "对象名须为 UTF-8"))
}
fn query(value: &str) -> Result<BTreeMap<String, String>, S3Error> {
    let mut out = BTreeMap::new();
    for field in value.split('&').filter(|v| !v.is_empty()) {
        let (k, v) = field.split_once('=').unwrap_or((field, ""));
        if out.insert(decode(k)?, decode(v)?).is_some() {
            return Err(S3Error::new(400, "InvalidArgument", "查询参数重复"));
        }
    }
    Ok(out)
}
fn supported_request(headers: &HeaderMap, query: &BTreeMap<String, String>) -> Result<(), S3Error> {
    if headers.contains_key(header::IF_MODIFIED_SINCE)
        || headers.contains_key(header::IF_UNMODIFIED_SINCE)
        || headers.contains_key(header::IF_RANGE)
        || headers
            .keys()
            .any(|k| k.as_str().starts_with("x-amz-if-match-"))
    {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "暂不支持时间、大小或 If-Range 条件，请勿依赖这些条件执行操作",
        ));
    }
    if query.keys().any(|k| {
        !matches!(
            k.as_str(),
            "x-id"
                | "list-type"
                | "prefix"
                | "delimiter"
                | "max-keys"
                | "continuation-token"
                | "start-after"
                | "encoding-type"
                | "fetch-owner"
                | "marker"
                | "location"
        )
    }) {
        return Err(S3Error::new(501, "NotImplemented", "此 S3 子操作尚未实现"));
    }
    if headers.contains_key(header::TRANSFER_ENCODING)
        || headers.contains_key("x-amz-trailer")
        || headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|s| s.trim().eq_ignore_ascii_case("aws-chunked"))
            })
        || headers
            .get("x-amz-content-sha256")
            .is_some_and(|v| v.as_bytes().starts_with(b"STREAMING-"))
    {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "上传须使用 Content-Length；不支持 aws-chunked 或 trailer",
        ));
    }
    if headers.keys().any(|k| {
        k.as_str().starts_with("x-amz-server-side-encryption")
            || k.as_str().starts_with("x-amz-grant-")
            || matches!(
                k.as_str(),
                "x-amz-acl"
                    | "x-amz-copy-source"
                    | "x-amz-tagging"
                    | "x-amz-website-redirect-location"
                    | "x-amz-write-offset-bytes"
                    | "x-amz-expected-bucket-owner"
                    | "x-amz-request-payer"
                    | "x-amz-object-lock-mode"
                    | "x-amz-object-lock-retain-until-date"
                    | "x-amz-object-lock-legal-hold"
            )
    }) {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "不支持 ACL、复制、标签、加密或存储策略",
        ));
    }
    if headers
        .get("x-amz-storage-class")
        .is_some_and(|v| v != "STANDARD")
    {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "仅支持 STANDARD 存储类别",
        ));
    }
    Ok(())
}
fn object_path(key: &str) -> Result<String, S3Error> {
    let key = decode(key)?;
    if key.is_empty()
        || key.chars().any(|c| c.is_control() || !xml_character(c))
        || key
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || key.split('/').next() == Some(".tapers")
    {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "对象名必须能无损映射到普通相对文件路径",
        ));
    }
    Ok(format!("/{key}"))
}

pub(super) fn handle(
    parts: Parts,
    reader: &mut dyn Read,
    response: &mut Response,
    ctx: &HttpContext,
    pool: &str,
) -> std::io::Result<()> {
    let head = parts.method == "HEAD";
    *response = match handle_inner(&parts, reader, ctx, pool) {
        Ok(r) => r,
        Err(e) => e.response(head),
    };
    Ok(())
}
fn handle_inner(
    parts: &Parts,
    reader: &mut dyn Read,
    ctx: &HttpContext,
    pool: &str,
) -> Result<Response, S3Error> {
    let q = query(parts.uri.query().unwrap_or(""))?;
    supported_request(&parts.headers, &q)?;
    let rest = parts
        .uri
        .path()
        .strip_prefix('/')
        .unwrap_or(parts.uri.path());
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    let head = parts.method == "HEAD";
    let path = if key.is_empty() {
        None
    } else {
        Some(object_path(key)?)
    };
    let write = matches!(parts.method.as_str(), "PUT" | "DELETE");
    if !matches!(parts.method.as_str(), "GET" | "HEAD" | "PUT" | "DELETE") {
        return Err(S3Error::new(501, "NotImplemented", "此 S3 操作尚未实现"));
    }
    if path.is_none() && write {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "请通过管理接口维护 pool",
        ));
    }
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if status.shutting_down
        || status.catalog_pending
        || !status.executor_ready
        || status
            .executor
            .as_ref()
            .is_none_or(|e| e.node != status.id || !e.fenced)
    {
        return Err(S3Error::new(503, "ServiceUnavailable", "目标带库不在服务"));
    }
    let files = ctx
        .files
        .select_pool(pool, path.as_deref(), parts.method != "PUT", write)
        .filter(|f| f.target_pool().as_deref() == Some(pool))
        .ok_or_else(|| S3Error::new(503, "ServiceUnavailable", "目标池尚未恢复"))?;
    files.check_permission()?;
    if path.is_none() {
        if head {
            return Ok(xml_response(200, "", true));
        }
        if q.contains_key("location") {
            if q.len() > 2 {
                return Err(S3Error::new(
                    400,
                    "InvalidArgument",
                    "location 不接受列举参数",
                ));
            }
            return Ok(xml_response(
                200,
                &format!("<LocationConstraint xmlns=\"{NS}\"/>"),
                false,
            ));
        }
        return list_objects(&files, bucket, &q);
    }
    if q.keys().any(|k| k != "x-id") {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "对象请求不支持此查询参数",
        ));
    }
    let path = path.unwrap();
    match parts.method.as_str() {
        "PUT" => put(&files, &path, &parts.headers, reader, ctx),
        "DELETE" => {
            if parts.headers.contains_key(header::IF_MATCH)
                || parts.headers.contains_key(header::IF_NONE_MATCH)
            {
                return Err(S3Error::new(501, "NotImplemented", "暂不支持条件删除"));
            }
            match files.delete(&path) {
                Ok(task) => {
                    wait_task(&files, task, ctx)?;
                }
                Err(ServiceError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
            Ok(xml_response(204, "", true))
        }
        _ => get(&files, &path, &parts.headers, head, ctx),
    }
}
fn wait_task(files: &FileService, task: u64, ctx: &HttpContext) -> Result<(), S3Error> {
    let state = files
        .wait_task(task, ctx.wait_timeout)
        .unwrap_or(TaskStatus::Staged);
    let result = match &state {
        TaskStatus::Committed { .. } => Ok(()),
        TaskStatus::Failed { reason } => Err(S3Error::new(500, "InternalError", reason)),
        TaskStatus::Indeterminate { reason } => Err(S3Error::new(
            500,
            "InternalError",
            format!("操作结果未定：{reason}；任务 {task}"),
        )),
        TaskStatus::Staged => Err(S3Error::new(
            504,
            "RequestTimeout",
            format!("等待提交超时，结果未定；任务 {task}"),
        )),
    };
    if !files.task_recorded(task, &state, std::time::Duration::from_secs(10)) {
        return Err(S3Error::new(
            504,
            "RequestTimeout",
            format!("任务记录尚未获多数派确认，结果未定；任务 {task}"),
        ));
    }
    result
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, S3Error> {
    headers
        .get(name)
        .map(|v| {
            v.to_str()
                .map_err(|_| S3Error::new(400, "InvalidArgument", format!("请求头 {name} 非法")))
        })
        .transpose()
}
fn put(
    files: &FileService,
    path: &str,
    headers: &HeaderMap,
    reader: &mut dyn Read,
    ctx: &HttpContext,
) -> Result<Response, S3Error> {
    let length = header_text(headers, "content-length")?
        .ok_or_else(|| S3Error::new(411, "MissingContentLength", "上传必须提供 Content-Length"))?
        .parse::<u64>()
        .map_err(|_| S3Error::new(400, "InvalidArgument", "Content-Length 非法"))?;
    let create = match header_text(headers, "if-none-match")? {
        None => false,
        Some("*") => true,
        _ => {
            return Err(S3Error::new(
                501,
                "NotImplemented",
                "写入只支持 If-None-Match: *",
            ));
        }
    };
    if headers.contains_key(header::IF_MATCH) {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "暂不支持 If-Match 写入",
        ));
    }
    let expected_md5 = header_text(headers, "content-md5")?
        .map(|v| {
            STANDARD
                .decode(v)
                .ok()
                .filter(|b| b.len() == 16)
                .ok_or_else(|| S3Error::new(400, "InvalidDigest", "Content-MD5 非法"))
        })
        .transpose()?;
    let expected_sha =
        header_text(headers, "x-amz-content-sha256")?.filter(|v| *v != "UNSIGNED-PAYLOAD");
    if expected_sha.is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(S3Error::new(400, "InvalidDigest", "SHA-256 非法"));
    }
    let checksum = header_text(headers, "x-amz-checksum-sha256")?
        .map(|v| {
            STANDARD
                .decode(v)
                .ok()
                .filter(|b| b.len() == 32)
                .ok_or_else(|| S3Error::new(400, "InvalidDigest", "SHA-256 checksum 非法"))
        })
        .transpose()?;
    for name in headers.keys().map(|k| k.as_str()) {
        if name.starts_with("x-amz-checksum-") && name != "x-amz-checksum-sha256" {
            return Err(S3Error::new(
                501,
                "NotImplemented",
                "暂只支持 SHA-256 checksum，请关闭自动 CRC 校验和",
            ));
        }
    }
    if header_text(headers, "x-amz-sdk-checksum-algorithm")?.is_some_and(|v| v != "SHA256") {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "暂只支持 SHA256 checksum",
        ));
    }
    let mut saved = BTreeMap::<String, String>::new();
    for (name, value) in headers {
        if name.as_str().starts_with("x-amz-meta-")
            || matches!(
                name.as_str(),
                "content-type"
                    | "content-encoding"
                    | "content-language"
                    | "content-disposition"
                    | "cache-control"
                    | "expires"
            )
        {
            let value = value
                .to_str()
                .map_err(|_| S3Error::new(400, "InvalidArgument", "对象元数据请求头非法"))?;
            saved.insert(name.to_string(), value.to_owned());
        }
    }
    if serde_json::to_vec(&saved).unwrap().len() > 8192 {
        return Err(S3Error::new(
            400,
            "MetadataTooLarge",
            "对象元数据超过 8 KiB",
        ));
    }
    let h = files.begin_with(path, length, create)?;
    let spooled = (|| -> Result<_, S3Error> {
        let mut file = std::fs::File::create(&h.spool)
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
        let mut left = length;
        let mut buf = vec![0u8; 256 * 1024];
        let mut md5 = Md5::new();
        let mut sha = Sha256::new();
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = reader
                .read(&mut buf[..want])
                .map_err(|e| S3Error::new(400, "IncompleteBody", e.to_string()))?;
            if n == 0 {
                return Err(S3Error::new(
                    400,
                    "IncompleteBody",
                    "正文短于 Content-Length",
                ));
            }
            file.write_all(&buf[..n])
                .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
            md5.update(&buf[..n]);
            sha.update(&buf[..n]);
            files.ingest(&h, n as u64)?;
            left -= n as u64;
        }
        let md5 = md5.finalize();
        let sha = sha.finalize();
        if expected_md5.as_deref().is_some_and(|v| v != md5.as_slice())
            || expected_sha.is_some_and(|v| !v.eq_ignore_ascii_case(&format!("{sha:x}")))
            || checksum.as_deref().is_some_and(|v| v != sha.as_slice())
        {
            return Err(S3Error::new(400, "BadDigest", "正文摘要不匹配，未提交上传"));
        }
        file.sync_all()
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
        Ok((format!("{md5:x}"), sha))
    })();
    let (md5, sha) = match spooled {
        Ok(v) => v,
        Err(e) => {
            files.abort(h);
            return Err(e);
        }
    };
    let metadata = FileMetadata::replacement(vec![
        Xattr {
            key: XATTR_MD5.into(),
            value: md5.clone(),
            base64: false,
        },
        Xattr {
            key: HEADERS_XATTR.into(),
            value: STANDARD.encode(serde_json::to_vec(&saved).unwrap()),
            base64: true,
        },
    ]);
    let task = files.finish_with_metadata(h, length, Some(metadata))?;
    wait_task(files, task, ctx)?;
    let mut response = xml_response(200, "", true);
    response
        .headers_mut()
        .insert(header::ETAG, format!("\"{md5}\"").parse().unwrap());
    if checksum.is_some() {
        response.headers_mut().insert(
            "x-amz-checksum-sha256",
            STANDARD.encode(sha).parse().unwrap(),
        );
    }
    Ok(response)
}
fn xattr<'a>(st: &'a Stat, key: &str) -> Option<&'a serde_json::Value> {
    st.metadata["xattrs"]
        .as_array()?
        .iter()
        .find(|a| a["key"].as_str() == Some(key))
}
fn etag(st: &Stat) -> String {
    let md5 = xattr(st, XATTR_MD5)
        .and_then(|v| v["value"].as_str())
        .filter(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()));
    format!(
        "\"{}\"",
        md5.map(str::to_owned)
            .unwrap_or_else(|| if !st.sha256.is_empty() {
                format!("sha256-{}", st.sha256)
            } else {
                format!("v{}-{}", st.version.0, st.version.1)
            })
    )
}
fn headers_for(st: &Stat, response: &mut Response) {
    response
        .headers_mut()
        .insert(header::ETAG, etag(st).parse().unwrap());
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    if let Some(date) = st.metadata["modify_time"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
    {
        response.headers_mut().insert(
            header::LAST_MODIFIED,
            date.with_timezone(&chrono::Utc)
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
                .parse()
                .unwrap(),
        );
    }
    if let Some(value) = xattr(st, HEADERS_XATTR)
        .and_then(|v| v["value"].as_str())
        .and_then(|v| STANDARD.decode(v).ok())
        .and_then(|v| serde_json::from_slice::<BTreeMap<String, String>>(&v).ok())
    {
        for (name, value) in value {
            if (name.starts_with("x-amz-meta-")
                || matches!(
                    name.as_str(),
                    "content-type"
                        | "content-encoding"
                        | "content-language"
                        | "content-disposition"
                        | "cache-control"
                        | "expires"
                ))
                && let (Ok(name), Ok(value)) = (
                    axum::http::HeaderName::try_from(name),
                    axum::http::HeaderValue::try_from(value),
                )
            {
                response.headers_mut().insert(name, value);
            }
        }
    }
}
fn get(
    files: &FileService,
    path: &str,
    headers: &HeaderMap,
    head: bool,
    ctx: &HttpContext,
) -> Result<Response, S3Error> {
    let st = files
        .stat(path)?
        .ok_or_else(|| S3Error::new(404, "NoSuchKey", "对象不存在或尚未提交"))?;
    if is_directory(&st.metadata) {
        return Err(S3Error::new(404, "NoSuchKey", "目录不是 S3 对象"));
    }
    if st.metadata["symlink"].as_str().is_some() {
        return Err(S3Error::new(
            409,
            "InvalidObjectState",
            "S3 不跟随原生符号链接",
        ));
    }
    let matches = |value: &str| {
        value
            .split(',')
            .any(|v| v.trim() == "*" || v.trim() == etag(&st))
    };
    if header_text(headers, "if-match")?.is_some_and(|v| !matches(v)) {
        return Err(S3Error::new(412, "PreconditionFailed", "对象 ETag 不匹配"));
    }
    if header_text(headers, "if-none-match")?.is_some_and(matches) {
        return Ok(xml_response(304, "", true));
    }
    let range = header_text(headers, "range")?;
    let parsed = if let Some(value) = range {
        Some(
            http::parse_range(value, st.len)
                .ok_or_else(|| S3Error::new(400, "InvalidArgument", "只支持单个字节 Range"))?,
        )
    } else {
        None
    };
    if parsed == Some(Err(())) {
        let mut response = error_response(416, "InvalidRange", "请求范围超出对象长度", head);
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes */{}", st.len).parse().unwrap(),
        );
        return Ok(response);
    }
    let mut response = xml_response(200, "", true);
    if head {
        let (start, end) = parsed.and_then(Result::ok).unwrap_or((0, st.len));
        if parsed.is_some() {
            *response.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{}", end - 1, st.len)
                    .parse()
                    .unwrap(),
            );
        }
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, (end - start).into());
    } else {
        let file = files
            .read_spool()
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
        let (tx, rx) = channel();
        ctx.exec
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(http::scoped_request(
                files,
                ExecRequest::ReadTo {
                    path: path.into(),
                    file,
                    reply: tx,
                },
            ))
            .map_err(|_| S3Error::new(503, "ServiceUnavailable", "设备执行者已退出"))?;
        let mut file = match rx.recv_timeout(ctx.wait_timeout) {
            Ok(Ok(file)) => file,
            Ok(Err(ReadError::Busy(e))) => return Err(S3Error::new(503, "SlowDown", e)),
            Ok(Err(ReadError::Lost(e))) => return Err(S3Error::new(503, "ServiceUnavailable", e)),
            Ok(Err(e)) => return Err(S3Error::new(500, "InternalError", e.to_string())),
            Err(_) => return Err(S3Error::new(504, "RequestTimeout", "读取等待超时")),
        };
        let current = files.stat(path)?;
        if current.as_ref().is_none_or(|v| {
            v.version != st.version || v.sha256 != st.sha256 || v.barcode != st.barcode
        }) || file
            .metadata()
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?
            .len()
            != st.len
        {
            return Err(S3Error::new(
                409,
                "OperationAborted",
                "读取期间对象版本已改变，请重试",
            ));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
        http::respond_file(&mut response, file, range)
            .map_err(|e| S3Error::new(500, "InternalError", e.to_string()))?;
        response.headers_mut().insert(
            "x-amz-request-id",
            uuid::Uuid::new_v4().simple().to_string().parse().unwrap(),
        );
    }
    headers_for(&st, &mut response);
    Ok(response)
}

fn list_objects(
    files: &FileService,
    bucket: &str,
    q: &BTreeMap<String, String>,
) -> Result<Response, S3Error> {
    let v2 = match q.get("list-type").map(String::as_str) {
        None => false,
        Some("2") => true,
        _ => return Err(S3Error::new(400, "InvalidArgument", "list-type 须为 2")),
    };
    let max = q
        .get("max-keys")
        .map(|v| {
            v.parse::<usize>()
                .ok()
                .filter(|v| *v <= 1000)
                .ok_or_else(|| S3Error::new(400, "InvalidArgument", "max-keys 须为 0..1000"))
        })
        .transpose()?
        .unwrap_or(1000);
    let prefix = q.get("prefix").map(String::as_str).unwrap_or("");
    let delimiter = q.get("delimiter").map(String::as_str).unwrap_or("");
    if !delimiter.is_empty() && delimiter != "/" {
        return Err(S3Error::new(501, "NotImplemented", "delimiter 暂只支持 /"));
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
    if prefix.chars().any(char::is_control) || delimiter.chars().any(char::is_control) {
        return Err(S3Error::new(400, "InvalidArgument", "列举参数含控制字符"));
    }
    if q.get("fetch-owner")
        .is_some_and(|v| v != "true" && v != "false")
        || (v2 && q.contains_key("marker"))
        || (!v2 && (q.contains_key("continuation-token") || q.contains_key("start-after")))
    {
        return Err(S3Error::new(400, "InvalidArgument", "列举参数组合非法"));
    }
    let token = q.get("continuation-token");
    let start = q
        .get(if v2 { "start-after" } else { "marker" })
        .map(String::as_str)
        .unwrap_or("");
    if !encoded
        && [prefix, delimiter, start]
            .iter()
            .any(|v| !v.chars().all(xml_character))
    {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "列举参数含 XML 1.0 不支持的字符，请使用 encoding-type=url",
        ));
    }
    let mut cursor = if let Some(token) = token {
        decode_token(token, bucket, prefix, delimiter)?
    } else {
        start.to_owned()
    };
    let floor = cursor.clone();
    let path_prefix = format!("/{prefix}");
    let mut items = Vec::new();
    let mut last = cursor.clone();
    let mut more = false;
    // 每次只收集一页。目录、墓碑和 CommonPrefixes 的后代通过扫描游标跳过。
    loop {
        if max == 0 {
            break;
        }
        let rows = files.object_page(&path_prefix, &format!("/{cursor}"), 1001)?;
        if rows.is_empty() {
            break;
        }
        let count = rows.len();
        for (path, st) in rows {
            let key = &path[1..];
            if key <= cursor.as_str() {
                continue;
            }
            cursor = key.to_owned();
            if st.deleted || is_directory(&st.metadata) || st.metadata["symlink"].as_str().is_some()
            {
                continue;
            }
            if !encoded && !key.chars().all(xml_character) {
                return Err(S3Error::new(
                    400,
                    "InvalidArgument",
                    "已有对象名含 XML 1.0 不支持的字符，请使用 encoding-type=url",
                ));
            }
            let common = if delimiter.is_empty() {
                None
            } else {
                key[prefix.len()..]
                    .find('/')
                    .map(|at| key[..prefix.len() + at + 1].to_owned())
            };
            if common.as_ref().is_some_and(|v| {
                v <= &floor
                    || items
                        .last()
                        .is_some_and(|i: &(String, Option<Stat>)| &i.0 == v)
            }) {
                continue;
            }
            if items.len() == max {
                more = true;
                break;
            }
            last = common.clone().unwrap_or_else(|| key.to_owned());
            items.push((last.clone(), if common.is_some() { None } else { Some(st) }));
        }
        if more || count < 1001 {
            break;
        }
    }
    let esc = |s: &str| {
        xml(&if encoded {
            crate::client::encode_query(s)
        } else {
            s.to_owned()
        })
    };
    let mut body = format!(
        "<ListBucketResult xmlns=\"{NS}\"><Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{max}</MaxKeys><IsTruncated>{more}</IsTruncated>",
        xml(bucket),
        esc(prefix)
    );
    if !delimiter.is_empty() {
        body.push_str(&format!("<Delimiter>{}</Delimiter>", esc(delimiter)));
    }
    if encoded {
        body.push_str("<EncodingType>url</EncodingType>");
    }
    if v2 {
        body.push_str(&format!("<KeyCount>{}</KeyCount>", items.len()));
        if let Some(token) = token {
            body.push_str(&format!(
                "<ContinuationToken>{}</ContinuationToken>",
                xml(token)
            ));
        }
        if !start.is_empty() {
            body.push_str(&format!("<StartAfter>{}</StartAfter>", esc(start)));
        }
        if more {
            body.push_str(&format!(
                "<NextContinuationToken>{}</NextContinuationToken>",
                encode_token(bucket, prefix, delimiter, &last)
            ));
        }
    } else {
        body.push_str(&format!("<Marker>{}</Marker>", esc(start)));
        if more {
            body.push_str(&format!("<NextMarker>{}</NextMarker>", esc(&last)));
        }
    }
    for (key, st) in items {
        if let Some(st) = st {
            body.push_str(&format!("<Contents><Key>{}</Key><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>",esc(&key),xml(&etag(&st)),st.len));
            if let Some(time) = st.metadata["modify_time"]
                .as_str()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            {
                body.push_str(&format!(
                    "<LastModified>{}</LastModified>",
                    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ));
            }
            if !v2 || q.get("fetch-owner").is_some_and(|v| v == "true") {
                body.push_str("<Owner><ID>tape-rs</ID><DisplayName>tape-rs</DisplayName></Owner>");
            }
            body.push_str("</Contents>");
        } else {
            body.push_str(&format!(
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                esc(&key)
            ));
        }
    }
    body.push_str("</ListBucketResult>");
    Ok(xml_response(200, &body, false))
}
fn encode_token(bucket: &str, prefix: &str, delimiter: &str, last: &str) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1, bucket, prefix, delimiter, last)).unwrap())
}
fn decode_token(
    token: &str,
    bucket: &str,
    prefix: &str,
    delimiter: &str,
) -> Result<String, S3Error> {
    let fail = || {
        S3Error::new(
            400,
            "InvalidArgument",
            "continuation-token 非法或不属于本次查询",
        )
    };
    if token.len() > 16 * 1024 {
        return Err(fail());
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| fail())?;
    let (version, b, p, d, last): (u8, String, String, String, String) =
        serde_json::from_slice(&bytes).map_err(|_| fail())?;
    if version != 1 || b != bucket || p != prefix || d != delimiter {
        return Err(fail());
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::files::{TapeIdent, TapeLimits};
    use crate::daemon::node::NodeStatus;
    use crate::daemon::state::Executor;
    use crate::ltfs::index::{FileNode, LtfsIndex};
    use http_body_util::BodyExt;
    use std::sync::Mutex;

    struct Fixture {
        dir: std::path::PathBuf,
        ctx: HttpContext,
    }
    impl Fixture {
        fn new(names: &[&str]) -> Self {
            let dir = std::env::temp_dir().join(format!("s3-unit-{}", uuid::Uuid::new_v4()));
            let files = FileService::new(dir.clone()).unwrap();
            files.bind_pool("P");
            let mut index = LtfsIndex::empty(uuid::Uuid::new_v4(), "S3".into(), 'b');
            for name in names {
                index.root.files.push(FileNode {
                    name: (*name).into(),
                    length: 3,
                    ..Default::default()
                });
            }
            files.open(
                1,
                TapeIdent {
                    barcode: "T1".into(),
                    pool_uuid: "P".into(),
                },
                TapeLimits {
                    file_limit: 10000,
                    usable_capacity: 1 << 20,
                },
                &index,
                1 << 20,
                true,
            );
            let (exec, _) = channel();
            let (node, _) = channel();
            let ctx = HttpContext {
                files,
                status: Arc::new(Mutex::new(NodeStatus {
                    id: 1,
                    executor: Some(Executor {
                        node: 1,
                        term: 1,
                        round: 1,
                        fenced: true,
                    }),
                    executor_ready: true,
                    ..Default::default()
                })),
                exec: Mutex::new(exec),
                node: Mutex::new(node),
                client_addrs: HashMap::new(),
                wait_timeout: std::time::Duration::from_millis(10),
            };
            Self { dir, ctx }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.ctx.files.close_all("done");
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }
    fn body(response: Response) -> String {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                String::from_utf8(
                    response
                        .into_body()
                        .collect()
                        .await
                        .unwrap()
                        .to_bytes()
                        .to_vec(),
                )
                .unwrap()
            })
    }
    #[test]
    fn bucket_identity_includes_library_and_is_dns_compatible() {
        let a = bucket_name("A", "P");
        assert_eq!(a, bucket_name("A", "P"));
        assert_ne!(a, bucket_name("B", "P"));
        assert_ne!(bucket_name("AB", "C"), bucket_name("A", "BC"));
        assert!((3..=63).contains(&a.len()));
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
    }
    #[test]
    fn keys_and_queries_are_decoded_once_without_path_aliases() {
        assert_eq!(
            object_path("%E4%B8%AD%E6%96%87/a%20b+%252F").unwrap(),
            "/中文/a b+%2F"
        );
        for key in [
            "/a",
            "a//b",
            "a/",
            "a/../b",
            "%2Etapers/a",
            "%00",
            "%FF",
            "%2",
            "a%0Ab",
            "%EF%BF%BE",
            "%EF%BF%BF",
        ] {
            assert!(object_path(key).is_err(), "{key}");
        }
        assert!(query("prefix=a&prefix=b").is_err());
        assert_eq!(query("prefix=a+b").unwrap()["prefix"], "a+b");
    }
    #[test]
    fn unsupported_requests_are_rejected_before_upload() {
        for route in ["uploads", "uploadId=x", "acl", "versionId=x", "delete"] {
            assert!(supported_request(&HeaderMap::new(), &query(route).unwrap()).is_err());
        }
        for (name, value) in [
            ("content-encoding", "gzip, aws-chunked"),
            ("transfer-encoding", "chunked"),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
            ("x-amz-copy-source", "/b/k"),
            ("x-amz-acl", "public-read"),
            ("if-unmodified-since", "Thu, 01 Jan 1970 00:00:00 GMT"),
            ("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT"),
            ("if-range", "\"old-etag\""),
            ("x-amz-if-match-last-modified-time", "1970-01-01T00:00:00Z"),
            ("x-amz-if-match-size", "123"),
        ] {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            assert!(supported_request(&h, &BTreeMap::new()).is_err());
        }
    }
    #[test]
    fn bad_digest_aborts_admission_and_never_enqueues() {
        let f = Fixture::new(&[]);
        let mut h = HeaderMap::new();
        h.insert("content-length", "3".parse().unwrap());
        h.insert("content-md5", STANDARD.encode([0u8; 16]).parse().unwrap());
        let error = put(&f.ctx.files, "/bad", &h, &mut &b"abc"[..], &f.ctx)
            .err()
            .unwrap();
        assert_eq!(error.code, "BadDigest");
        assert_eq!(f.ctx.files.stat_full("/bad").unwrap().in_flight, None);
        assert_eq!(f.ctx.files.queued(1), Some((0, 0)));
        f.ctx.files.abort(f.ctx.files.begin("/bad", 3).unwrap());
        let error = put(&f.ctx.files, "/short", &h, &mut &b"a"[..], &f.ctx)
            .err()
            .unwrap();
        assert_eq!(error.code, "IncompleteBody");
        assert_eq!(f.ctx.files.stat_full("/short").unwrap().in_flight, None);
    }
    #[test]
    fn delimiter_pagination_does_not_repeat_prefixes_on_next_page() {
        // 索引路径由 catalog_of 转为文件路径，此夹具不执行任何磁带命令。
        let f = Fixture::new(&["a/one", "a/two", "b/one", "b/two", "c"]);
        let q = query("list-type=2&delimiter=%2F&max-keys=1").unwrap();
        let one = body(list_objects(&f.ctx.files, "bucket", &q).unwrap());
        assert!(one.contains("<Prefix>a/</Prefix>"));
        let token = one
            .split("<NextContinuationToken>")
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap();
        let two = body(
            list_objects(
                &f.ctx.files,
                "bucket",
                &query(&format!(
                    "list-type=2&delimiter=%2F&max-keys=1&continuation-token={token}"
                ))
                .unwrap(),
            )
            .unwrap(),
        );
        assert!(two.contains("<Prefix>b/</Prefix>"));
        assert!(!two.contains("<Prefix>a/</Prefix>"));
        assert!(decode_token(token, "other", "", "/").is_err());
        assert!(decode_token(token, "bucket", "new", "/").is_err());
        let zero = body(
            list_objects(
                &f.ctx.files,
                "bucket",
                &query("list-type=2&max-keys=0").unwrap(),
            )
            .unwrap(),
        );
        assert!(zero.contains("<KeyCount>0</KeyCount>"));
        let after = body(
            list_objects(
                &f.ctx.files,
                "bucket",
                &query("list-type=2&delimiter=%2F&start-after=a%2Fzzz").unwrap(),
            )
            .unwrap(),
        );
        assert!(!after.contains("<Prefix>a/</Prefix>"));
    }
    #[test]
    fn object_scanning_is_bounded_and_merges_recent_tombstones_with_sqlite() {
        let dir = std::env::temp_dir().join(format!("s3-page-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.db");
        let _db = super::super::directory::Directory::open(&path).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        for n in 0..2500 {
            db.execute("INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,metadata) VALUES('P',?1,'T1',1,3,'','null')",[format!("/a{n:04}")]).unwrap();
        }
        db.execute("INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,metadata) VALUES('OTHER','/a0000','T1',1,9,'','null')",[]).unwrap();
        let large = "x".repeat(6 * 1024 * 1024);
        let md5 = "900150983cd24fb0d6963f7d28e17f72";
        let metadata = serde_json::json!({
            "modify_time":"2026-10-29T00:00:00.000000000Z",
            "xattrs":[{"key":"large","value":large,"base64":false},
                {"key":XATTR_MD5,"value":md5,"base64":false}]
        });
        db.execute(
            "UPDATE files SET metadata=?1 WHERE pool_uuid='P' AND path='/a1100'",
            [metadata.to_string()],
        )
        .unwrap();
        let f =
            FileService::with_options(dir.join("spool"), Default::default(), Some(path)).unwrap();
        f.bind_pool("P");
        let mut index = LtfsIndex::empty(uuid::Uuid::new_v4(), "S3".into(), 'b');
        for name in ["a0000", "a2500"] {
            index.root.files.push(FileNode {
                name: name.into(),
                length: 3,
                xattrs: vec![
                    Xattr {
                        key: "large".into(),
                        value: large.clone(),
                        base64: false,
                    },
                    Xattr {
                        key: XATTR_MD5.into(),
                        value: md5.into(),
                        base64: false,
                    },
                ],
                ..Default::default()
            });
        }
        f.open(
            1,
            TapeIdent {
                barcode: "T1".into(),
                pool_uuid: "P".into(),
            },
            TapeLimits {
                file_limit: 10000,
                usable_capacity: 1 << 20,
            },
            &index,
            1 << 20,
            true,
        );
        f.delete("/a0000").unwrap();
        let (batch, uploads) = f.take_batch_now(1).unwrap();
        let deleted = crate::daemon::state::FileRec {
            path: "/a0000".into(),
            length: 0,
            sha256: String::new(),
            version: (1, 1),
            deleted: true,
            metadata: serde_json::Value::Null,
        };
        f.batch_done(
            1,
            &batch,
            &uploads,
            Ok((2, [(deleted.path.clone(), deleted)].into())),
        );
        let mut after = "/".to_owned();
        let mut seen = Vec::new();
        loop {
            let page = f.object_page("/a", &after, 17).unwrap();
            assert!(page.len() <= 17);
            for (path, stat) in &page {
                assert!(
                    stat.metadata.to_string().len() < 512,
                    "列举复制了完整属性: {path}"
                );
                if path == "/a1100" || path == "/a2500" {
                    assert_eq!(etag(stat), format!("\"{md5}\""));
                }
                if path == "/a1100" {
                    assert_eq!(stat.metadata["modify_time"], metadata["modify_time"]);
                }
            }
            if page.is_empty() {
                break;
            }
            after = page.last().unwrap().0.clone();
            seen.extend(page);
        }
        assert_eq!(seen.len(), 2501);
        assert!(seen.windows(2).all(|p| p[0].0 < p[1].0));
        assert!(seen[0].1.deleted);
        assert_eq!(seen.last().unwrap().0, "/a2500");
        let listed =
            body(list_objects(&f, "bucket", &query("list-type=2&prefix=a000").unwrap()).unwrap());
        assert!(!listed.contains("<Key>a0000</Key>"));
        drop(db);
        drop(_db);
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn legacy_keys_outside_xml_10_require_url_encoding_for_listing() {
        let f = Fixture::new(&["bad\u{fffe}"]);
        assert_eq!(
            list_objects(&f.ctx.files, "bucket", &query("list-type=2").unwrap())
                .err()
                .unwrap()
                .code,
            "InvalidArgument"
        );
        let listed = body(
            list_objects(
                &f.ctx.files,
                "bucket",
                &query("list-type=2&encoding-type=url").unwrap(),
            )
            .unwrap(),
        );
        assert!(listed.contains("<Key>bad%EF%BF%BE</Key>"));
        assert!(!listed.contains('\u{fffe}'));
        assert!(
            list_objects(
                &f.ctx.files,
                "bucket",
                &query("list-type=2&prefix=%EF%BF%BE").unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn head_and_invalid_ranges_do_not_issue_device_requests() {
        let f = Fixture::new(&["a"]);
        let response = get(&f.ctx.files, "/a", &HeaderMap::new(), true, &f.ctx).unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-length"], "3");
        assert!(body(response).is_empty());
        let mut headers = HeaderMap::new();
        headers.insert("range", "bytes=3-".parse().unwrap());
        let response = get(&f.ctx.files, "/a", &headers, false, &f.ctx).unwrap();
        assert_eq!(response.status(), 416);
        assert_eq!(response.headers()["content-range"], "bytes */3");
        assert!(body(response).contains("<Code>InvalidRange</Code>"));
        headers.insert("range", "bytes=0-0,2-2".parse().unwrap());
        assert_eq!(
            get(&f.ctx.files, "/a", &headers, false, &f.ctx)
                .err()
                .unwrap()
                .status,
            400
        );
    }
}
