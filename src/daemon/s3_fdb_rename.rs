//! S3 RenameObject maps a durable client token to the existing rename ledger.
use super::*;
use crate::managed_catalog::{
    NodeMutationTaskKind, NodeMutationTaskState, NodeRenameRequest, NodeRenameRevision,
    NodeRenameSpec, S3RenameInput,
};

const CONDITIONS: [&str; 8] = [
    "x-amz-rename-source-if-match",
    "x-amz-rename-source-if-none-match",
    "x-amz-rename-source-if-modified-since",
    "x-amz-rename-source-if-unmodified-since",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
];
fn invalid(reason: &str) -> S3Error {
    S3Error::new(400, "InvalidArgument", reason)
}
fn precondition() -> S3Error {
    S3Error::new(412, "PreconditionFailed", "对象不满足原重命名条件")
}
fn error(error: CatalogError) -> S3Error {
    match error {
        CatalogError::IdempotencyMismatch => S3Error::new(
            400,
            "IdempotencyParameterMismatch",
            "同一 client token 的参数已改变",
        ),
        error => catalog_error(error),
    }
}

pub(super) fn validate(req: &Request, q: &BTreeMap<String, String>) -> S3Result<()> {
    if req.headers().contains_key(header::RANGE) {
        return Err(invalid("RenameObject 不接受 Range"));
    }
    if q.get("renameObject").is_none_or(|value| !value.is_empty())
        || q.keys()
            .any(|key| !matches!(key.as_str(), "renameObject" | "x-id"))
    {
        return Err(invalid("RenameObject 查询参数无效"));
    }
    let mut headers = req.headers().clone();
    for name in CONDITIONS.into_iter().chain([
        "x-amz-rename-source",
        "x-amz-client-token",
        "content-length",
    ]) {
        if headers.get_all(name).iter().count() > 1 {
            return Err(invalid("RenameObject 请求头重复"));
        }
    }
    if header_text(&headers, "content-length")?.is_some_and(|value| value != "0") {
        return Err(invalid("RenameObject 请求不能包含正文"));
    }
    for name in CONDITIONS
        .into_iter()
        .chain(["x-amz-rename-source", "x-amz-client-token"])
    {
        headers.remove(name);
    }
    supported_request_with_time(&headers, &BTreeMap::new(), true)
}

pub(super) fn input(
    req: &Request,
    bucket: &str,
    library: &str,
    route: ActiveLibraryRoute,
    pool: Uuid,
) -> S3Result<S3RenameInput> {
    let encoded = header_text(req.headers(), "x-amz-rename-source")?
        .ok_or_else(|| invalid("RenameObject 缺少 x-amz-rename-source"))?;
    let source = decode(encoded)?;
    let source = source.strip_prefix('/').unwrap_or(&source);
    let key = if let Some((prefix, key)) = source.split_once('/') {
        if prefix == bucket {
            key
        } else if prefix.strip_prefix("pool-").is_some_and(|suffix| {
            suffix.len() == 56 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(invalid("RenameObject 源和目标必须在同一 bucket"));
        } else {
            source
        }
    } else {
        source
    };
    let (_, destination) = req
        .uri()
        .path()
        .strip_prefix('/')
        .unwrap_or(req.uri().path())
        .split_once('/')
        .ok_or_else(|| invalid("RenameObject 缺少目标 key"))?;
    let from = decoded_object_path(key)?;
    let to = object_path(destination)?;
    if from == to || from.len() > 1025 || to.len() > 1025 {
        return Err(invalid("RenameObject 两端 key 必须不同且不超过 1024 字节"));
    }
    let token = header_text(req.headers(), "x-amz-client-token")?
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if token.is_empty() || token.len() > 64 || !token.bytes().all(|b| (33..=126).contains(&b)) {
        return Err(invalid(
            "client token 必须为 1 到 64 个可打印的非空格 ASCII 字符",
        ));
    }
    let mut conditions: [Option<String>; 8] = Default::default();
    for (index, name) in CONDITIONS.iter().enumerate() {
        let Some(value) = header_text(req.headers(), name)? else {
            continue;
        };
        if value.is_empty() || value.len() > 1024 {
            return Err(invalid("RenameObject 条件长度无效"));
        }
        if index % 4 >= 2 {
            if http_time(value).is_none() {
                return Err(invalid("RenameObject 时间条件无效"));
            }
        } else if value != "*" {
            let quoted = value.starts_with('"') && value.ends_with('"') && value.len() >= 3;
            let inner = if quoted {
                &value[1..value.len() - 1]
            } else {
                value
            };
            if value.starts_with("W/")
                || inner
                    .chars()
                    .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, ',' | '"'))
            {
                return Err(invalid("RenameObject 条件须为单个强 ETag 或 *"));
            }
        }
        conditions[index] = Some(value.into());
    }
    Ok(S3RenameInput {
        library: library.into(),
        namespace: route.route.namespace,
        pool,
        token,
        from,
        to,
        conditions,
    })
}

pub(super) async fn empty_body(req: Request, timeout: std::time::Duration) -> S3Result<Request> {
    let (parts, body) = req.into_parts();
    tokio::time::timeout(timeout, axum::body::to_bytes(body, 0))
        .await
        .map_err(|_| invalid("RenameObject 请求正文未结束"))?
        .map_err(|_| invalid("RenameObject 请求不能包含正文"))?;
    Ok(Request::from_parts(parts, Body::empty()))
}

fn check_conditions(
    conditions: &[Option<String>],
    current: Option<&ResolvedPath>,
    namespace: Uuid,
) -> S3Result<()> {
    let object = match current {
        Some(ResolvedPath::File {
            id,
            head,
            object_metadata,
            ..
        }) => Some((
            tag(namespace, *id, head.version),
            object_metadata.as_ref().and_then(|value| value.modified),
        )),
        None => None,
        _ => {
            return Err(S3Error::new(
                409,
                "InvalidObjectState",
                "RenameObject 仅接受普通对象",
            ));
        }
    };
    let matched = |value: &str| {
        object
            .as_ref()
            .is_some_and(|(tag, _)| value == "*" || value == tag || format!("\"{value}\"") == *tag)
    };
    if let Some(value) = &conditions[0] {
        if !matched(value) {
            return Err(precondition());
        }
    } else if let Some(value) = &conditions[3] {
        let date = http_time(value).ok_or_else(|| invalid("时间条件无效"))?;
        if object
            .as_ref()
            .ok_or_else(precondition)?
            .1
            .ok_or_else(|| unavailable("对象修改时间缺失"))?
            > date
        {
            return Err(precondition());
        }
    }
    if let Some(value) = &conditions[1] {
        if matched(value) {
            return Err(precondition());
        }
    } else if let Some(value) = &conditions[2] {
        let date = http_time(value).ok_or_else(|| invalid("时间条件无效"))?;
        if object
            .as_ref()
            .ok_or_else(precondition)?
            .1
            .ok_or_else(|| unavailable("对象修改时间缺失"))?
            <= date
        {
            return Err(precondition());
        }
    }
    Ok(())
}
fn revision(namespace: Uuid, value: &ResolvedPath) -> NodeRenameRevision {
    let (inode, version) = match value {
        ResolvedPath::File { id, head, .. } => (*id, head.version),
        ResolvedPath::Directory { id, version, .. } => (*id, *version),
    };
    NodeRenameRevision {
        namespace,
        inode,
        version,
    }
}
async fn resolve(s: &FdbS3, input: &S3RenameInput, path: &str) -> S3Result<Option<ResolvedPath>> {
    let path =
        crate::managed_catalog::pool_catalog_path(input.pool, path.as_bytes()).map_err(error)?;
    s.catalog
        .resolve_active_metadata_for_route(&input.library, &path, s.routes[&input.library])
        .await
        .map_err(error)
}
async fn parent(s: &FdbS3, input: &S3RenameInput, path: &str) -> S3Result<NodeRenameRevision> {
    let parent = path.rsplit_once('/').unwrap().0;
    match resolve(s, input, if parent.is_empty() { "/" } else { parent }).await? {
        Some(value @ ResolvedPath::Directory { .. }) => Ok(revision(input.namespace, &value)),
        _ => Err(S3Error::new(
            409,
            "InvalidRequest",
            "目标或源的父目录不存在",
        )),
    }
}
async fn prepare(s: &FdbS3, input: &S3RenameInput, round: u64) -> S3Result<NodeRenameRequest> {
    let source = metadata(
        s,
        &input.library,
        input.pool,
        &format!("/{}{}", input.pool, input.from),
    )
    .await?;
    let target = resolve(s, input, &input.to).await?;
    if let Some(ResolvedPath::File {
        copy, placement, ..
    }) = &target
        && (placement.pool_id != input.pool
            || placement.library_serial != input.library
            || copy.media.as_ref().is_some_and(|m| m.native_symlink))
    {
        return Err(S3Error::new(
            409,
            "InvalidObjectState",
            "目标不是此池的普通对象",
        ));
    }
    check_conditions(&input.conditions[..4], Some(&source), input.namespace)?;
    check_conditions(&input.conditions[4..], target.as_ref(), input.namespace)?;
    let source_parent = parent(s, input, &input.from).await?;
    let destination_parent =
        if input.from.rsplit_once('/').unwrap().0 == input.to.rsplit_once('/').unwrap().0 {
            source_parent
        } else {
            parent(s, input, &input.to).await?
        };
    let spec = NodeRenameSpec {
        pool: input.pool,
        operation: Uuid::new_v4(),
        from: input.from.clone(),
        to: input.to.clone(),
        source: revision(input.namespace, &source),
        source_parent,
        destination_parent,
        target: target
            .as_ref()
            .map(|value| revision(input.namespace, value)),
        no_replace: input.conditions[5].as_deref() == Some("*"),
    };
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    s.catalog
        .prepare_node_rename_request(
            &input.library,
            s.routes[&input.library],
            round,
            &spec,
            &time,
        )
        .await
        .map_err(|err| match err {
            CatalogError::Invalid(_) => precondition(),
            other => error(other),
        })
}

async fn status(
    s: &FdbS3,
    input: &S3RenameInput,
    request: &NodeRenameRequest,
) -> S3Result<Option<Response>> {
    let route = s.routes[&input.library];
    let Some(status) = s
        .catalog
        .node_mutation_task_status(
            &input.library,
            route,
            request.request_epoch(),
            request.spec().operation,
        )
        .await
        .map_err(error)?
    else {
        return Ok(None);
    };
    let task = status.task;
    if task.kind != NodeMutationTaskKind::Rename
        || task.fingerprint != request.fingerprint()
        || task.pool != input.pool
        || task.namespace != input.namespace
    {
        return Err(unavailable("S3 rename 原任务身份不一致"));
    }
    match task.state {
        NodeMutationTaskState::Completed => {
            let publication = s
                .catalog
                .node_rename_publication(
                    &input.library,
                    route,
                    request.request_epoch(),
                    request.spec().operation,
                )
                .await
                .map_err(error)?
                .ok_or_else(|| unavailable("S3 rename 缺少原完成回执"))?;
            if publication.request() != request
                || Some(publication.fingerprint()) != task.publication_digest
                || Some(publication.completed_owner()) != task.completed_owner_epoch
                || Some(publication.completed_at_millis()) != task.completed_at_millis
                || publication.is_directory()
            {
                return Err(unavailable("S3 rename 原任务与完成回执不一致"));
            }
            Ok(Some(xml_response(200, "", true)))
        }
        NodeMutationTaskState::Rejected => Err(precondition()),
        NodeMutationTaskState::Cancelled => {
            Err(S3Error::new(409, "OperationAborted", "原重命名任务已取消"))
        }
        _ => Ok(None),
    }
}
pub(super) async fn known(s: &FdbS3, input: &S3RenameInput) -> S3Result<Option<Response>> {
    match s
        .catalog
        .s3_rename_request(s.routes[&input.library], input)
        .await
        .map_err(error)?
    {
        Some(request) => status(s, input, &request).await,
        None => Ok(None),
    }
}
pub(super) async fn execute(s: &FdbS3, input: &S3RenameInput, round: u64) -> S3Result<Response> {
    let route = s.routes[&input.library];
    let request = match s
        .catalog
        .s3_rename_request(route, input)
        .await
        .map_err(error)?
    {
        Some(request) => request,
        None => {
            let candidate = match prepare(s, input, round).await {
                Ok(candidate) => candidate,
                Err(failed) => {
                    // Another request with the same token may have completed
                    // while this request was still resolving its original paths.
                    if let Some(request) = s
                        .catalog
                        .s3_rename_request(route, input)
                        .await
                        .map_err(error)?
                    {
                        if let Some(response) = status(s, input, &request).await? {
                            return Ok(response);
                        }
                        return Err(unavailable("同一 token 的原请求仍在处理，请重试"));
                    }
                    return Err(failed);
                }
            };
            check_owner(s, &input.library, round).await?;
            s.catalog
                .remember_s3_rename_request(route, round, input, &candidate)
                .await
                .map_err(error)?
        }
    };
    if let Some(response) = status(s, input, &request).await? {
        return Ok(response);
    }
    check_owner(s, &input.library, round).await?;
    s.catalog
        .queue_node_rename_request(route, round, &request, 8 << 20)
        .await
        .map_err(error)?;
    let deadline = tokio::time::Instant::now() + s.libraries[&input.library].wait_timeout;
    loop {
        if let Some(response) = status(s, input, &request).await? {
            return Ok(response);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(S3Error::new(
                504,
                "RequestTimeout",
                "原重命名结果尚未确认；使用同一 client token 重试",
            ));
        }
        check_owner(s, &input.library, round).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route() -> ActiveLibraryRoute {
        ActiveLibraryRoute {
            route: crate::managed_catalog::PreparedLibraryRoute {
                namespace: Uuid::new_v4(),
                revision: 1,
            },
            frozen_index: 1,
            certificate_sha256: [1; 32],
            initial_owner_epoch: 1,
        }
    }
    fn request(source: &str) -> Request {
        Request::builder()
            .method("PUT")
            .uri("/bucket/out%252F.txt?renameObject")
            .header("x-amz-rename-source", source)
            .header("x-amz-client-token", "stable")
            .body(Body::empty())
            .unwrap()
    }
    #[test]
    fn source_and_destination_are_decoded_once_and_bare_sources_are_pool_relative() {
        let route = route();
        let pool = Uuid::new_v4();
        for source in [
            "bucket%2Fspace%20%252F.txt",
            "/bucket/space%20%252F.txt",
            "space%20%252F.txt",
        ] {
            let parsed = input(&request(source), "bucket", "SIM", route, pool).unwrap();
            assert_eq!(parsed.from, "/space %2F.txt");
            assert_eq!(parsed.to, "/out%2F.txt");
            assert_eq!(parsed.token, "stable");
        }
        assert_eq!(
            input(&request("prefix/a"), "bucket", "SIM", route, pool)
                .unwrap()
                .from,
            "/prefix/a"
        );
        let foreign = format!("pool-{}/a", "a".repeat(56));
        assert!(input(&request(&foreign), "bucket", "SIM", route, pool).is_err());
        for source in [
            "a//b",
            "a/%2E%2E/b",
            "a%00b",
            ".tapers/private",
            "a/",
            "out%252F.txt",
        ] {
            assert!(
                input(&request(source), "bucket", "SIM", route, pool).is_err(),
                "{source}"
            );
        }
        assert!(input(&request(&"x".repeat(1025)), "bucket", "SIM", route, pool).is_err());
    }
    #[test]
    fn rename_validation_rejects_ignored_conditions_duplicates_and_nonempty_bodies() {
        let mut request = request("source");
        let query = BTreeMap::from([("renameObject".into(), "".into())]);
        validate(&request, &query).unwrap();
        request
            .headers_mut()
            .append("x-amz-client-token", "duplicate".parse().unwrap());
        assert!(validate(&request, &query).is_err());
        request.headers_mut().remove("x-amz-client-token");
        request
            .headers_mut()
            .insert("x-amz-rename-source-unknown", "x".parse().unwrap());
        assert_eq!(validate(&request, &query).unwrap_err().status, 501);
        request.headers_mut().remove("x-amz-rename-source-unknown");
        request
            .headers_mut()
            .insert("if-unmodified-since", "not a date".parse().unwrap());
        assert!(input(&request, "bucket", "SIM", route(), Uuid::new_v4()).is_err());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            rt.block_on(empty_body(
                Request::new(Body::from("x")),
                std::time::Duration::from_secs(1)
            ))
            .is_err()
        );
    }
}
