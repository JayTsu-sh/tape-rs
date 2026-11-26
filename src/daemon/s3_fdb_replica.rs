//! S3 projection of the shared parent archive and verified read path.
use super::*;
use crate::daemon::fdb_http::replica_files::{BASE, read_file, upload_member};
use crate::managed_catalog::ReplicaObjectMetadata;
use crate::replicated_pool::{ReplicatedArchive, inode};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use std::os::fd::AsRawFd;
use tokio_util::io::ReaderStream;

pub(super) const OPERATION: &str = "x-tapers-operation";

pub(super) async fn dispatch(
    s: &FdbS3,
    pool: Uuid,
    bucket: &str,
    key: &str,
    q: &BTreeMap<String, String>,
    req: Request,
    permit: OwnedSemaphorePermit,
) -> S3Result<Response> {
    let head = req.method() == "HEAD";
    if req.headers().contains_key(OPERATION) && req.method() != "PUT" {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "归档操作 UUID 需要 PUT",
        ));
    }
    if req.method() == "DELETE"
        || q.contains_key("renameObject")
        || (req.method() == "PUT" && key.is_empty())
    {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "多副本池支持文件归档和读取",
        ));
    }
    if key.is_empty() {
        if head {
            return Ok(xml_response(200, "", true));
        }
        if q.contains_key("location") {
            if q.keys().any(|k| !matches!(k.as_str(), "location" | "x-id")) {
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
        return list(s, None, pool, bucket, q).await;
    }
    if q.keys().any(|k| k != "x-id") {
        return Err(S3Error::new(
            501,
            "NotImplemented",
            "对象请求不支持此查询参数",
        ));
    }
    let path = object_path(key)?;
    if req.method() == "PUT" {
        let operation = header_text(req.headers(), OPERATION)?
            .map(|value| {
                Uuid::parse_str(value)
                    .ok()
                    .filter(|id| !id.is_nil())
                    .ok_or_else(|| S3Error::new(400, "InvalidArgument", "归档操作 UUID 无效"))
            })
            .transpose()?
            .unwrap_or_else(Uuid::new_v4);
        let mut response = put(s, pool, path, operation, req)
            .await
            .unwrap_or_else(|err| err.response(false));
        response
            .headers_mut()
            .insert(OPERATION, operation.to_string().parse().expect("UUID"));
        return Ok(response);
    }
    get(s, pool, &path, req.headers(), head, permit).await
}

fn archive_error(err: CatalogError) -> S3Error {
    match err.domain() {
        CatalogError::IdempotencyMismatch | CatalogError::PoolBusy | CatalogError::Invalid(_) => {
            S3Error::new(409, "OperationAborted", err.to_string())
        }
        _ => catalog_error(err),
    }
}

async fn put(
    s: &FdbS3,
    pool: Uuid,
    path: String,
    operation: Uuid,
    req: Request,
) -> S3Result<Response> {
    let before = s
        .catalog
        .replica_archive_status(pool, operation, false, &s.routes)
        .await
        .map_err(archive_error)?;
    let id = inode(pool, &path);
    let previous = if let Some(job) = before {
        job["file_version"]
            .as_u64()
            .and_then(|n| n.checked_sub(1))
            .filter(|n| *n > 0)
    } else {
        let current = s
            .catalog
            .replica_file(pool, &path, &s.routes)
            .await
            .map_err(catalog_error)?;
        if current
            .as_ref()
            .is_some_and(|file| file.info["kind"] != "file")
        {
            return Err(S3Error::new(
                409,
                "OperationAborted",
                "归档目标须为普通文件",
            ));
        }
        let version = current.and_then(|file| file.info["file_version"].as_u64());
        write::conditions(req.headers(), pool, version.map(|n| (id, n)))?;
        version
    };
    let if_match = header_text(req.headers(), "if-match")?.map(str::to_owned);
    let if_none_match = header_text(req.headers(), "if-none-match")?.map(str::to_owned);
    let staged = write::stage(&s.gateway, s.storage, req).await?;
    let input = ReplicatedArchive {
        operation,
        path,
        previous,
        size: staged.length,
        sha256: staged.digest,
    };
    let object = ReplicaObjectMetadata {
        headers: staged.saved,
        if_match,
        if_none_match,
    };
    let job = s
        .catalog
        .admit_replica_object(pool, &input, Some(&object), &s.routes)
        .await
        .map_err(archive_error)?;
    let mut timed_out = false;
    if job["status"] == "waiting" {
        for member in job["members"].as_array().expect("archive members") {
            if !member["commit"].is_null() {
                continue;
            }
            let member = Uuid::parse_str(member["pool_id"].as_str().expect("member id"))
                .expect("catalog UUID");
            // A detached owner request must keep its own read offset.
            let source =
                tokio::fs::File::open(format!("/proc/self/fd/{}", staged.file.as_raw_fd()))
                    .await
                    .map_err(|err| unavailable(err.to_string()))?;
            let request = Request::builder()
                .method("PUT")
                .uri(format!(
                    "{BASE}/{pool}/archives/{operation}/members/{member}"
                ))
                .header(header::CONTENT_LENGTH, input.size)
                .body(Body::from_stream(ReaderStream::new(source)))
                .expect("native archive request");
            let response = upload_member(
                State(s.replica.clone()),
                Path((pool, operation, member)),
                request,
            )
            .await;
            timed_out |= response.status() == StatusCode::GATEWAY_TIMEOUT;
        }
    }
    let job = s
        .catalog
        .replica_archive_status(pool, operation, false, &s.routes)
        .await
        .map_err(archive_error)?
        .ok_or_else(|| unavailable("原归档回执暂不可用"))?;
    match job["status"].as_str() {
        Some("committed")
            if job["members"].as_array().is_some_and(|members| {
                !members.is_empty() && members.iter().all(|member| !member["commit"].is_null())
            }) => {}
        Some("cancelled" | "cancelling") => {
            return Err(S3Error::new(
                409,
                "OperationAborted",
                "原归档正在取消或已经取消",
            ));
        }
        _ if timed_out => {
            return Err(S3Error::new(
                504,
                "RequestTimeout",
                "原归档结果未知，请查询或重试原操作",
            ));
        }
        _ => return Err(unavailable("原归档等待副本，请查询或重试原操作")),
    }
    let version = job["file_version"].as_u64().expect("catalog version");
    let mut response = xml_response(200, "", true);
    response
        .headers_mut()
        .insert(header::ETAG, tag(pool, id, version).parse().expect("ETag"));
    if let Some((name, value)) = staged.checksum.header() {
        response
            .headers_mut()
            .insert(name, value.parse().expect("checksum"));
    }
    Ok(response)
}

async fn get(
    s: &FdbS3,
    pool: Uuid,
    path: &str,
    headers: &HeaderMap,
    head: bool,
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
    let checksum_mode = header_text(headers, "x-amz-checksum-mode")?;
    if checksum_mode.is_some_and(|v| v != "ENABLED") {
        return Err(S3Error::new(
            400,
            "InvalidArgument",
            "checksum-mode 须为 ENABLED",
        ));
    }
    let file = s
        .catalog
        .replica_file(pool, path, &s.routes)
        .await
        .map_err(catalog_error)?
        .filter(|file| file.info["kind"] == "file")
        .ok_or_else(|| S3Error::new(404, "NoSuchKey", "指定对象不存在"))?;
    let version = file.info["file_version"].as_u64().expect("catalog version");
    let size = file.info["size"].as_u64().expect("catalog size");
    let digest = file.copies.first().map(|copy| copy.digest);
    let modified = file.info["modified"].as_i64();
    let saved = file.info["object_headers"].as_object().map(|object| {
        object
            .iter()
            .filter_map(|(key, val)| Some((key.clone(), val.as_str()?.to_owned())))
            .collect::<BTreeMap<_, _>>()
    });
    let etag = tag(pool, inode(pool, path), version);
    let status = condition_status(headers, &etag, modified)?;
    let range = header_text(headers, "range")?.filter(|_| status == 200);
    let parsed = range
        .map(|range| {
            http::parse_range(range, size)
                .ok_or_else(|| S3Error::new(400, "InvalidArgument", "只支持单个字节 Range"))
        })
        .transpose()?;
    let (start, end) = parsed.and_then(Result::ok).unwrap_or((0, size));
    let mut response = if status == 412 {
        S3Error::new(412, "PreconditionFailed", "对象不满足请求条件").response(head)
    } else if status == 304 {
        let mut response = xml_response(304, "", true);
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::CONTENT_TYPE);
        response
    } else if parsed == Some(Err(())) {
        let mut response = S3Error::new(416, "InvalidRange", "请求范围超出对象长度").response(head);
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes */{size}").parse().unwrap(),
        );
        response
    } else if head {
        let mut response = xml_response(if parsed.is_some() { 206 } else { 200 }, "", true);
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, (end - start).into());
        if parsed.is_some() {
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{size}", end - 1).parse().unwrap(),
            );
        }
        response
    } else {
        let result = read_file(&s.replica, file, None, false, range, |member| {
            format!(
                "{BASE}/{pool}/file?path={}&version={version}&member={member}",
                URL_SAFE_NO_PAD.encode(path)
            )
        })
        .await;
        if result.status().as_u16() != if parsed.is_some() { 206 } else { 200 }
            || result
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                != Some(end - start)
        {
            return Err(unavailable("未能读取指定版本的已验证副本"));
        }
        let (parts, body) = result.into_parts();
        Response::from_parts(
            parts,
            Body::new(http::LimitedBody {
                body,
                _permit: permit,
            }),
        )
    };
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().expect("ETag"));
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    if let Some(time) = modified.and_then(|time| chrono::DateTime::from_timestamp(time, 0)) {
        response.headers_mut().insert(
            header::LAST_MODIFIED,
            time.format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
                .parse()
                .expect("HTTP date"),
        );
    }
    if matches!(response.status().as_u16(), 200 | 206) {
        response.headers_mut().remove(header::CACHE_CONTROL);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        if let Some(saved) = &saved {
            for (name, value) in saved
                .iter()
                .filter(|(key, _)| !key.starts_with("x-amz-checksum-"))
            {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| unavailable("对象元数据名称无效"))?,
                    value.parse().map_err(|_| unavailable("对象元数据值无效"))?,
                );
            }
        }
        if range.is_none()
            && checksum_mode == Some("ENABLED")
            && let Some(digest) = digest
        {
            if let Some(crc32) = saved
                .as_ref()
                .and_then(|headers| checksum::stored_crc32(headers, &digest))
            {
                response
                    .headers_mut()
                    .insert("x-amz-checksum-crc32", crc32.parse().expect("checksum"));
            } else {
                response.headers_mut().insert(
                    "x-amz-checksum-sha256",
                    STANDARD.encode(digest).parse().unwrap(),
                );
            }
        }
    }
    Ok(response)
}
