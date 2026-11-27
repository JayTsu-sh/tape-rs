//! Native multi-copy uploads and exact-version downloads.
use super::*;
use crate::replicated_pool::ReplicatedArchive;
use sha2::{Digest, Sha256};
use std::io::Seek;

const BASE: &str = "/v1/fdb/replicated-pools";

fn archive_route(parent: Uuid, operation: Uuid) -> String {
    format!("{BASE}/{parent}/archives/{operation}")
}

fn checked_job(response: Response, parent: Uuid, operation: Uuid) -> Result<Value> {
    let job = management::parsed(response, &[200, 202])?;
    if job["id"] != parent.to_string()
        || job["operation"] != operation.to_string()
        || job["members"]
            .as_array()
            .is_none_or(|members| !(2..=16).contains(&members.len()))
        || !matches!(
            job["status"].as_str(),
            Some("waiting" | "committed" | "cancelling" | "cancelled")
        )
    {
        return Err(ClientError::Verification("多副本归档回执身份无效".into()));
    }
    Ok(job)
}

fn check_input(job: &Value, input: &ReplicatedArchive) -> Result<()> {
    let digest = input
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if job["path"] != input.path
        || job["size"] != input.size
        || job["sha256"] != digest
        || Some(job["file_version"].as_u64().unwrap_or(0))
            != input.previous.unwrap_or(0).checked_add(1)
    {
        return Err(ClientError::Verification(
            "多副本归档回执与原版本、长度或摘要不符".into(),
        ));
    }
    Ok(())
}

impl FdbClient {
    /// Persist the original input before submitting it. A successful admission
    /// can still be waiting for members to commit.
    pub fn admit_replicated_archive(&self, pool: Uuid, input: &ReplicatedArchive) -> Result<Value> {
        validate_path(&input.path)?;
        let body = serde_json::to_vec(input).map_err(|_| invalid("归档请求无法编码"))?;
        let response = self.retry_request(
            "POST",
            &format!("{BASE}/{pool}/archives"),
            Some(&body),
            &[("content-type", "application/json")],
        )?;
        let job = checked_job(response, pool, input.operation)?;
        check_input(&job, input)?;
        Ok(job)
    }

    pub fn replicated_archive_status(&self, pool: Uuid, operation: Uuid) -> Result<Value> {
        checked_job(
            self.retry_request("GET", &archive_route(pool, operation), None, &[])?,
            pool,
            operation,
        )
    }

    pub fn cancel_replicated_archive(&self, pool: Uuid, operation: Uuid) -> Result<Value> {
        checked_job(
            self.retry_request("DELETE", &archive_route(pool, operation), None, &[])?,
            pool,
            operation,
        )
    }

    /// Retry only this original child, checking the parent before an uncertain
    /// response can be mistaken for an unsuccessful tape write.
    pub fn upload_replicated_member(
        &self,
        pool: Uuid,
        input: &ReplicatedArchive,
        member: Uuid,
        file: &File,
    ) -> Result<Value> {
        let route = format!("{}/members/{member}", archive_route(pool, input.operation));
        if !file.metadata()?.is_file() || file.metadata()?.len() != input.size {
            return Err(invalid("归档正文长度与原请求不同"));
        }
        let mut reason = "成员 owner 不可用".to_owned();
        for endpoint in &self.transport.endpoints {
            match self.transport.request_with(
                endpoint,
                "PUT",
                &route,
                Some(Body::OpenedFile(file, input.size)),
                &[],
                None,
            ) {
                Ok(response) if matches!(response.status, 200 | 202) => {
                    let job = checked_job(response, pool, input.operation)?;
                    check_input(&job, input)?;
                    if member_committed(&job, member) || job["status"] == "cancelled" {
                        return Ok(job);
                    }
                    break;
                }
                Ok(response) if matches!(response.status, 429 | 503 | 504) => {
                    reason =
                        String::from_utf8_lossy(&response.body[..response.body.len().min(4096)])
                            .into();
                    break;
                }
                Ok(response) => return Err(rejected(response)),
                Err((true, err)) => {
                    reason = err.to_string();
                    break;
                }
                Err((false, err)) => reason = err.to_string(),
            }
        }
        if let Ok(job) = self.replicated_archive_status(pool, input.operation) {
            check_input(&job, input)?;
            if member_committed(&job, member) || job["status"] == "cancelled" {
                return Ok(job);
            }
        }
        Err(ClientError::Indeterminate(format!(
            "底层池 {member} 的原归档操作 {} 尚未确认：{reason}",
            input.operation
        )))
    }

    /// Resume from an already committed copy when possible. The caller's
    /// immutable original body is needed only while no usable copy exists.
    pub fn archive_replicated(
        &self,
        pool: Uuid,
        input: &ReplicatedArchive,
        original: Option<&Path>,
    ) -> Result<Value> {
        let job = if original.is_some() {
            self.admit_replicated_archive(pool, input)?
        } else {
            self.replicated_archive_status(pool, input.operation)?
        };
        check_input(&job, input)?;
        if job["status"] == "committed"
            || job["status"] == "cancelled"
            || job["status"] == "cancelling"
        {
            return Ok(job);
        }
        let route = archive_route(pool, input.operation);
        let scratch =
            ScratchCopy(std::env::temp_dir().join(format!(".tape-replica-{}", Uuid::new_v4())));
        let mut scratch_file = None;
        let mut source = None;
        for member in job["members"].as_array().expect("checked job") {
            if member["commit"].is_null() {
                continue;
            }
            let id = reading::uuid_field(member, "pool_id", false)?;
            let mut file = match scratch_file.take() {
                Some(file) => file,
                None => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&scratch.0)?,
            };
            if self
                .download_replica_body(&format!("{route}/members/{id}"), &job, &mut file)
                .is_ok()
            {
                source = Some(file);
                break;
            }
            scratch_file = Some(file);
        }
        if source.is_none()
            && let Some(path) = original
        {
            match OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(path)
            {
                Ok(mut file) if file.metadata()?.is_file() => {
                    verify_body(&mut file, input)?;
                    source = Some(file);
                }
                Ok(_) => return Err(invalid("原归档正文须为普通文件")),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        let source = source.ok_or_else(|| {
            ClientError::Indeterminate(format!(
                "原归档操作 {} 等待原暂存或同摘要续传",
                input.operation
            ))
        })?;
        let mut current = job;
        let ids: Vec<_> = current["members"]
            .as_array()
            .expect("checked job")
            .iter()
            .map(|row| reading::uuid_field(row, "pool_id", false))
            .collect::<Result<_>>()?;
        let mut problem = None;
        for member in ids {
            if current["status"] != "waiting" {
                break;
            }
            if !member_committed(&current, member) {
                match self.upload_replicated_member(pool, input, member, &source) {
                    Ok(job) => current = job,
                    Err(err @ ClientError::Indeterminate(_)) => {
                        problem = Some(err);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        let current = self.replicated_archive_status(pool, input.operation)?;
        check_input(&current, input)?;
        if current["status"] == "waiting"
            && let Some(problem) = problem
        {
            return Err(problem);
        }
        Ok(current)
    }

    pub fn replicated_file_stat(&self, pool: Uuid, path: &str) -> Result<Option<Value>> {
        if path != "/" {
            validate_path(path)?;
        }
        let response = self.retry_request(
            "GET",
            &format!("{BASE}/{pool}/stat?path={}", URL_SAFE_NO_PAD.encode(path)),
            None,
            &[],
        )?;
        if response.status == 404 {
            return Ok(None);
        }
        let info = management::parsed(response, &[200])?;
        if info["id"] != pool.to_string()
            || info["path"] != path
            || !info["file_version"].as_u64().is_some_and(|v| v > 0)
            || !matches!(
                info["kind"].as_str(),
                Some("file" | "directory" | "symlink")
            )
        {
            return Err(ClientError::Verification(
                "多副本文件元数据与请求不一致".into(),
            ));
        }
        Ok(Some(info))
    }

    pub fn replicated_children(
        &self,
        pool: Uuid,
        path: &str,
        after: Option<&str>,
    ) -> Result<Value> {
        if path != "/" {
            validate_path(path)?;
        }
        let mut route = format!(
            "{BASE}/{pool}/children?path={}",
            URL_SAFE_NO_PAD.encode(path)
        );
        if let Some(after) = after {
            route.push_str(&format!("&after={}", URL_SAFE_NO_PAD.encode(after)));
        }
        let response = self.retry_request("GET", &route, None, &[])?;
        let page = management::parsed(response, &[200])?;
        if page["id"] != pool.to_string() || !page["items"].is_array() {
            return Err(ClientError::Verification("多副本目录页与原请求不符".into()));
        }
        Ok(page)
    }

    pub(crate) fn download_replicated_to(
        &self,
        pool: Uuid,
        path: &str,
        out: &mut File,
    ) -> Result<u64> {
        let info = self
            .replicated_file_stat(pool, path)?
            .ok_or(ClientError::NotFound)?;
        if info["kind"] != "file" {
            return Err(invalid("读取目标须为普通文件"));
        }
        let version = info["file_version"].as_u64().expect("validated version");
        self.download_replica_body(
            &format!(
                "{BASE}/{pool}/file?path={}&version={version}",
                URL_SAFE_NO_PAD.encode(path)
            ),
            &info,
            out,
        )
    }

    pub fn download_replicated_new(
        &self,
        pool: Uuid,
        path: &str,
        member: Option<Uuid>,
        output: &Path,
    ) -> Result<u64> {
        match fs::symlink_metadata(output) {
            Ok(_) => return Err(ClientError::Exists(output.display().to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let info = self
            .replicated_file_stat(pool, path)?
            .ok_or(ClientError::NotFound)?;
        if info["kind"] != "file" {
            return Err(invalid("读取目标须为普通文件"));
        }
        let version = info["file_version"].as_u64().expect("validated version");
        let mut route = format!(
            "{BASE}/{pool}/file?path={}&version={version}",
            URL_SAFE_NO_PAD.encode(path)
        );
        if let Some(member) = member {
            route.push_str(&format!("&member={member}"))
        }
        let directory = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let scratch = ScratchCopy(directory.join(format!(".fdb-download-{}", Uuid::new_v4())));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&scratch.0)?;
        let size = self.download_replica_body(&route, &info, &mut file)?;
        file.sync_all()?;
        fs::hard_link(&scratch.0, output)?;
        File::open(directory)?.sync_all()?;
        Ok(size)
    }

    fn download_replica_body(&self, route: &str, info: &Value, file: &mut File) -> Result<u64> {
        let size = info["size"]
            .as_u64()
            .ok_or_else(|| invalid("多副本文件长度无效"))?;
        let digest = info["sha256"]
            .as_str()
            .filter(|hex| hex.len() == 64)
            .ok_or_else(|| invalid("多副本文件摘要无效"))?;
        for endpoint in &self.transport.endpoints {
            file.set_len(0)?;
            file.rewind()?;
            let mut sink = BodySink {
                file,
                remaining: size,
                digest: Sha256::new(),
            };
            match self
                .transport
                .request_with(endpoint, "GET", route, None, &[], Some(&mut sink))
            {
                Ok(response) if matches!(response.status, 429 | 503 | 504) => {}
                Ok(response) if response.status == 200 => {
                    if sink.remaining != 0 || format!("{:x}", sink.digest.finalize()) != digest {
                        return Err(ClientError::Verification(
                            "多副本下载正文长度或摘要不符".into(),
                        ));
                    }
                    return Ok(size);
                }
                Ok(response) => return Err(rejected(response)),
                Err((true, err)) => return Err(err.into()),
                Err((false, _)) => {}
            }
        }
        Err(ClientError::NoLeader("没有可读取的已验证副本".into()))
    }
}

fn member_committed(job: &Value, member: Uuid) -> bool {
    job["members"]
        .as_array()
        .expect("checked job")
        .iter()
        .any(|row| row["pool_id"] == member.to_string() && !row["commit"].is_null())
}

fn verify_body(file: &mut File, input: &ReplicatedArchive) -> Result<()> {
    file.rewind()?;
    if file.metadata()?.len() != input.size {
        return Err(ClientError::Verification("原归档正文长度不符".into()));
    }
    let mut digest = Sha256::new();
    let length = std::io::copy(&mut file.take(input.size), &mut digest)?;
    if length != input.size || <[u8; 32]>::from(digest.finalize()) != input.sha256 {
        return Err(ClientError::Verification("原归档正文 SHA-256 不符".into()));
    }
    Ok(())
}

struct BodySink<'a> {
    file: &'a mut File,
    remaining: u64,
    digest: Sha256,
}

impl Write for BodySink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("副本正文超过已确认长度"));
        }
        let count = self.file.write(bytes)?;
        self.remaining -= count as u64;
        self.digest.update(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
