//! S3 content writes use the same fenced FDB upload and node-mutation ledgers.
use super::*;
use crate::managed_catalog::{
    ActiveFileIntent, ActiveUploadSpec, NodeMutation, NodeMutationRequest, NodeMutationSpec,
    NodeMutationTaskKind, NodeMutationTaskState,
};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;

fn invalid(reason: &str) -> S3Error {
    S3Error::new(400, "InvalidArgument", reason)
}
fn conflict() -> S3Error {
    S3Error::new(
        409,
        "ConditionalRequestConflict",
        "操作期间对象身份或版本已改变",
    )
}
fn precondition() -> S3Error {
    S3Error::new(412, "PreconditionFailed", "对象不满足写入条件")
}

pub(super) fn validate(req: &Request, q: &BTreeMap<String, String>) -> S3Result<()> {
    if q.keys().any(|key| key != "x-id") {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "对象写入不支持此查询参数",
        ));
    }
    for name in req.headers().keys() {
        if req.headers().get_all(name).iter().count() != 1 {
            return Err(invalid("对象写入请求头重复"));
        }
    }
    if [
        header::RANGE,
        header::CONTENT_RANGE,
        header::IF_MODIFIED_SINCE,
        header::IF_UNMODIFIED_SINCE,
    ]
    .iter()
    .any(|name| req.headers().contains_key(name))
    {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "对象写入不支持 Range 或时间条件",
        ));
    }
    if let Some(value) = header_text(req.headers(), "if-none-match")?
        && (req.method() != "PUT" || value != "*")
    {
        return Err(invalid("写入仅支持 PUT If-None-Match: *"));
    }
    if let Some(value) = header_text(req.headers(), "if-match")?
        && value != "*"
        && !(value.len() >= 2
            && value.starts_with('"')
            && value.ends_with('"')
            && !value[1..value.len() - 1].contains(['"', ',']))
    {
        return Err(invalid("If-Match 必须为单个强 ETag 或 *"));
    }
    Ok(())
}

async fn resolve(
    s: &FdbS3,
    serial: &str,
    pool: Uuid,
    path: &str,
) -> S3Result<Option<ResolvedPath>> {
    let path =
        crate::managed_catalog::pool_catalog_path(pool, path.as_bytes()).map_err(catalog_error)?;
    s.catalog
        .resolve_active_metadata_for_route(serial, &path, s.routes[serial])
        .await
        .map_err(catalog_error)
}
fn file_version(
    value: Option<&ResolvedPath>,
    serial: &str,
    pool: Uuid,
) -> S3Result<Option<(Uuid, u64)>> {
    match value {
        None => Ok(None),
        Some(ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            ..
        }) if placement.pool_id == pool
            && placement.library_serial == serial
            && !copy.media.as_ref().is_some_and(|m| m.native_symlink) =>
        {
            Ok(Some((*id, head.version)))
        }
        _ => Err(S3Error::new(
            409,
            "InvalidObjectState",
            "对象写入仅接受本池普通文件",
        )),
    }
}
fn conditions(headers: &HeaderMap, namespace: Uuid, current: Option<(Uuid, u64)>) -> S3Result<()> {
    let etag = current.map(|(id, version)| tag(namespace, id, version));
    if header_text(headers, "if-none-match")? == Some("*") && current.is_some() {
        return Err(precondition());
    }
    if let Some(value) = header_text(headers, "if-match")?
        && etag
            .as_deref()
            .is_none_or(|tag| value != "*" && value != tag)
    {
        return Err(precondition());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn mutation(
    s: &FdbS3,
    serial: &str,
    pool: Uuid,
    path: &str,
    round: u64,
    change: NodeMutation,
    original: Option<(Uuid, u64)>,
) -> S3Result<()> {
    check_owner(s, serial, round).await?;
    let route = s.routes[serial];
    let spec = NodeMutationSpec {
        pool,
        operation: Uuid::new_v4(),
        path: path.into(),
        expected_version: original.map(|(_, version)| version),
        mutation: change,
    };
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let request = s
        .catalog
        .prepare_node_mutation_request(serial, route, round, &spec, &time)
        .await
        .map_err(|error| match error {
            CatalogError::Invalid(_) => conflict(),
            error => catalog_error(error),
        })?;
    if original.is_some_and(|(id, _)| request.context().node != id) {
        return Err(conflict());
    }
    s.catalog
        .queue_node_mutation_request(route, round, &request, 8 << 20)
        .await
        .map_err(catalog_error)?;
    wait_mutation(s, serial, round, &request).await
}
async fn wait_mutation(
    s: &FdbS3,
    serial: &str,
    round: u64,
    request: &NodeMutationRequest,
) -> S3Result<()> {
    let c = request.context();
    let deadline = tokio::time::Instant::now() + s.libraries[serial].wait_timeout;
    loop {
        let status = s
            .catalog
            .node_mutation_task_status(serial, s.routes[serial], c.request_epoch, c.operation)
            .await
            .map_err(catalog_error)?
            .ok_or_else(|| unavailable("原对象修改任务缺失"))?;
        if status.task.kind != NodeMutationTaskKind::SingleNode
            || status.task.fingerprint != request.fingerprint()
        {
            return Err(unavailable("原对象修改任务身份不符"));
        }
        match status.task.state {
            NodeMutationTaskState::Completed => {
                if status.outcome.as_ref().is_none_or(|outcome| {
                    outcome.node != c.node
                        || outcome.deleted != matches!(request.mutation(), NodeMutation::Unlink)
                }) {
                    return Err(unavailable("原对象修改缺少匹配的完成回执"));
                }
                return Ok(());
            }
            NodeMutationTaskState::Rejected => return Err(conflict()),
            NodeMutationTaskState::Cancelled => {
                return Err(S3Error::new(409, "OperationAborted", "原对象修改已取消"));
            }
            _ => {}
        }
        check_owner(s, serial, round).await?;
        if tokio::time::Instant::now() >= deadline {
            return Err(S3Error::new(
                504,
                "RequestTimeout",
                format!(
                    "原修改任务结果未定：{}:{}:{}",
                    c.namespace, c.request_epoch, c.operation
                ),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
async fn parents(s: &FdbS3, serial: &str, pool: Uuid, path: &str, round: u64) -> S3Result<()> {
    let mut prefix = String::new();
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    for part in &parts[..parts.len() - 1] {
        prefix.push('/');
        prefix.push_str(part);
        match resolve(s, serial, pool, &prefix).await? {
            Some(ResolvedPath::Directory { .. }) => continue,
            Some(_) => return Err(conflict()),
            None => {}
        }
        if let Err(error) = mutation(
            s,
            serial,
            pool,
            &prefix,
            round,
            NodeMutation::CreateDirectory,
            None,
        )
        .await
            && !matches!(
                resolve(s, serial, pool, &prefix).await?,
                Some(ResolvedPath::Directory { .. })
            )
        {
            return Err(error);
        }
    }
    Ok(())
}

struct UploadHeaders {
    length: u64,
    md5: Option<Vec<u8>>,
    sha: Option<String>,
    checksum: Option<Vec<u8>>,
    saved: BTreeMap<String, String>,
}
impl UploadHeaders {
    fn parse(headers: &HeaderMap) -> S3Result<Self> {
        let length = header_text(headers, "content-length")?
            .ok_or_else(|| {
                S3Error::new(411, "MissingContentLength", "上传必须提供 Content-Length")
            })?
            .parse()
            .map_err(|_| invalid("Content-Length 无效"))?;
        let decode = |name: &str, length| -> S3Result<Option<Vec<u8>>> {
            header_text(headers, name)?
                .map(|v| {
                    STANDARD
                        .decode(v)
                        .ok()
                        .filter(|b| b.len() == length)
                        .ok_or_else(|| S3Error::new(400, "InvalidDigest", "上传摘要格式无效"))
                })
                .transpose()
        };
        let md5 = decode("content-md5", 16)?;
        let checksum = decode("x-amz-checksum-sha256", 32)?;
        let sha = header_text(headers, "x-amz-content-sha256")?
            .filter(|v| *v != "UNSIGNED-PAYLOAD")
            .map(str::to_owned);
        if sha
            .as_ref()
            .is_some_and(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(S3Error::new(400, "InvalidDigest", "SHA-256 无效"));
        }
        if headers.keys().any(|name| {
            name.as_str().starts_with("x-amz-checksum-") && name != "x-amz-checksum-sha256"
        }) || header_text(headers, "x-amz-sdk-checksum-algorithm")?
            .is_some_and(|v| v != "SHA256")
        {
            return Err(S3Error::new(
                501,
                "NotImplemented",
                "仅支持 SHA256 checksum",
            ));
        }
        let mut saved = BTreeMap::new();
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
                saved.insert(
                    name.to_string(),
                    value
                        .to_str()
                        .map_err(|_| invalid("对象元数据不是有效文本"))?
                        .to_owned(),
                );
            }
        }
        if saved.len() > 128 || serde_json::to_vec(&saved).expect("string map").len() > 8192 {
            return Err(S3Error::new(400, "MetadataTooLarge", "对象元数据超过预算"));
        }
        Ok(Self {
            length,
            md5,
            sha,
            checksum,
            saved,
        })
    }
}

async fn put(
    s: &FdbS3,
    serial: &str,
    pool: Uuid,
    path: &str,
    round: u64,
    req: Request,
) -> S3Result<Response> {
    let headers = UploadHeaders::parse(req.headers())?;
    if headers.length > s.storage.download_bytes {
        return Err(S3Error::new(413, "EntityTooLarge", "上传超过本地暂存预算"));
    }
    let before = resolve(s, serial, pool, path).await?;
    let original = file_version(before.as_ref(), serial, pool)?;
    conditions(req.headers(), s.routes[serial].route.namespace, original)?;
    let file = crate::local_read_storage::download_file(
        s.libraries[serial].files.spool_dir(),
        headers.length,
        s.storage,
    )
    .map_err(|err| unavailable(format!("上传暂存不可用：{err}")))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut body = req.into_body();
    let mut length = 0u64;
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let deadline = tokio::time::Instant::now() + s.libraries[serial].wait_timeout;
    while let Some(frame) = tokio::time::timeout_at(deadline, body.frame())
        .await
        .map_err(|_| S3Error::new(408, "RequestTimeout", "上传正文超时"))?
    {
        let data = frame
            .map_err(|err| S3Error::new(400, "IncompleteBody", err.to_string()))?
            .into_data()
            .map_err(|_| invalid("不支持上传 trailer"))?;
        length = length
            .checked_add(data.len() as u64)
            .ok_or_else(|| invalid("上传长度溢出"))?;
        if length > headers.length {
            return Err(invalid("正文超过 Content-Length"));
        }
        sha.update(&data);
        md5.update(&data);
        file.write_all(&data)
            .await
            .map_err(|err| unavailable(err.to_string()))?;
    }
    if length != headers.length {
        return Err(S3Error::new(
            400,
            "IncompleteBody",
            "正文短于 Content-Length",
        ));
    }
    let digest: [u8; 32] = sha.finalize().into();
    let md5 = md5.finalize();
    if headers.md5.as_deref().is_some_and(|v| v != md5.as_slice())
        || headers.sha.as_ref().is_some_and(|v| {
            !v.eq_ignore_ascii_case(
                &digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
        })
        || headers.checksum.as_deref().is_some_and(|v| v != digest)
    {
        return Err(S3Error::new(400, "BadDigest", "上传正文摘要不匹配"));
    }
    file.flush()
        .await
        .map_err(|err| unavailable(err.to_string()))?;
    let file = file.into_std().await;
    parents(s, serial, pool, path, round).await?;
    check_owner(s, serial, round).await?;
    let spec = ActiveUploadSpec {
        pool_id: pool,
        path: path.as_bytes().to_vec(),
        operation_id: Uuid::new_v4(),
        expected_version: original.map(|(_, v)| v),
        size: length,
        digest: ContentDigest::Sha256(digest),
    };
    let library = s.libraries[serial].clone();
    let intent: ActiveFileIntent = tokio::task::spawn_blocking(move || {
        let (reply, received) = std::sync::mpsc::channel();
        library
            .exec
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ExecRequest::FdbPrepare {
                spec: Box::new(spec),
                barcode: None,
                reply,
            })
            .map_err(|_| ReadError::Busy("设备线程已停止".into()))?;
        received
            .recv()
            .map_err(|_| ReadError::Busy("准备回执已断开".into()))?
    })
    .await
    .map_err(|err| unavailable(err.to_string()))?
    .map_err(|err| match err {
        ReadError::Failed(_) => conflict(),
        err => unavailable(err.to_string()),
    })?;
    if original.is_some_and(|(id, _)| id != intent.file_id) {
        return Err(conflict());
    }
    let request = intent.request();
    let metadata = FileMetadata::replacement(vec![
        Xattr {
            key: XATTR_MD5.into(),
            value: format!("{md5:x}"),
            base64: false,
        },
        Xattr {
            key: HEADERS_XATTR.into(),
            value: STANDARD.encode(serde_json::to_vec(&headers.saved).expect("string map")),
            base64: true,
        },
    ]);
    s.catalog
        .remember_s3_upload_metadata(s.routes[serial], round, &request, &metadata)
        .await
        .map_err(catalog_error)?;
    let library = s.libraries[serial].clone();
    let file_id = intent.file_id;
    let version = tokio::task::spawn_blocking(move || {
        let (reply, received) = std::sync::mpsc::channel();
        library
            .exec
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ExecRequest::FdbWrite {
                intent: Box::new(request),
                file,
                reply,
            })
            .map_err(|_| ReadError::Busy("设备线程已停止".into()))?;
        received
            .recv()
            .map_err(|_| ReadError::Busy("写入回执已断开".into()))?
    })
    .await
    .map_err(|err| unavailable(err.to_string()))?
    .map_err(|err| {
        unavailable(format!(
            "原上传 {}:{}:{} 结果未确认：{err}",
            intent.namespace, intent.request_epoch, intent.operation_id
        ))
    })?;
    check_owner(s, serial, round).await?;
    let mut response = xml_response(200, "", true);
    response.headers_mut().insert(
        header::ETAG,
        tag(intent.namespace, file_id, version)
            .parse()
            .expect("ETag"),
    );
    if headers.checksum.is_some() {
        response.headers_mut().insert(
            "x-amz-checksum-sha256",
            STANDARD.encode(digest).parse().expect("digest"),
        );
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    s: FdbS3,
    serial: String,
    pool: Uuid,
    path: String,
    round: u64,
    req: Request,
    permit: OwnedSemaphorePermit,
) -> S3Result<Response> {
    let timeout = s.libraries[&serial].wait_timeout;
    let pending = tokio::spawn(async move {
        let _permit = permit;
        if req.method() == "PUT" {
            return put(&s, &serial, pool, &path, round, req).await;
        }
        let before = resolve(&s, &serial, pool, &path).await?;
        let current = file_version(before.as_ref(), &serial, pool)?;
        conditions(req.headers(), s.routes[&serial].route.namespace, current)?;
        rename::empty_body(req, timeout).await?;
        if let Some(original) = current {
            mutation(
                &s,
                &serial,
                pool,
                &path,
                round,
                NodeMutation::Unlink,
                Some(original),
            )
            .await?;
        }
        check_owner(&s, &serial, round).await?;
        Ok(xml_response(204, "", true))
    });
    match tokio::time::timeout(timeout, pending).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(unavailable(error.to_string())),
        Err(_) => Err(S3Error::new(
            504,
            "RequestTimeout",
            "原 S3 操作结果未定；核对对象及持久任务后处理",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_or_ambiguous_write_parameters_fail_before_spooling() {
        for (name, value) in [
            ("if-match", "\""),
            ("if-match", "W/\"tag\""),
            ("if-none-match", "tag"),
            ("range", "bytes=0-1"),
        ] {
            let request = Request::builder()
                .method("PUT")
                .header(name, value)
                .body(Body::empty())
                .unwrap();
            assert!(
                validate(&request, &BTreeMap::new()).is_err(),
                "{name}={value}"
            );
        }
        let mut request = Request::builder()
            .method("PUT")
            .body(Body::empty())
            .unwrap();
        request
            .headers_mut()
            .append("if-none-match", "*".parse().unwrap());
        request
            .headers_mut()
            .append("if-none-match", "*".parse().unwrap());
        assert!(validate(&request, &BTreeMap::new()).is_err());
        for (name, value) in [
            ("content-md5", "bad"),
            ("x-amz-content-sha256", "bad"),
            ("x-amz-checksum-sha256", "bad"),
            ("x-amz-sdk-checksum-algorithm", "CRC32"),
        ] {
            let request = Request::builder()
                .header("content-length", "3")
                .header(name, value)
                .body(Body::empty())
                .unwrap();
            assert!(UploadHeaders::parse(request.headers()).is_err(), "{name}");
        }
        assert!(UploadHeaders::parse(&HeaderMap::new()).is_err());
        let request = Request::builder()
            .header("content-length", "0")
            .body(Body::empty())
            .unwrap();
        assert_eq!(UploadHeaders::parse(request.headers()).unwrap().length, 0);
    }
}
