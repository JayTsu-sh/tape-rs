use super::*;
use sha2::Digest;
use std::io::Seek;

fn verification(reason: &str) -> ClientError {
    ClientError::Verification(reason.into())
}

pub(super) fn read_route(operation: &str, library: &str, path: &str) -> Result<String> {
    if library.is_empty()
        || library.len() > 64
        || !library
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("带库标识无效"));
    }
    // Reads retain the existing UTF-8 path contract, including imported names.
    if path != "/"
        && (!path.starts_with('/')
            || path.len() > 2048
            || path[1..]
                .split('/')
                .any(|p| p.is_empty() || matches!(p, "." | "..") || p.contains('\0')))
    {
        return Err(invalid("读取路径必须是规范绝对路径"));
    }
    Ok(format!(
        "/v1/fdb/{operation}/{library}?path={}",
        URL_SAFE_NO_PAD.encode(path)
    ))
}

pub(super) fn uuid_field(value: &Value, field: &str, allow_nil: bool) -> Result<Uuid> {
    value[field]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| allow_nil || !id.is_nil())
        .ok_or_else(|| verification("FDB 元数据 UUID 无效"))
}

pub(super) fn check_scope(value: &Value, library: &str) -> Result<()> {
    if value["library_serial"] != library {
        return Err(verification("FDB 应答带库与请求不一致"));
    }
    uuid_field(value, "namespace", false)?;
    Ok(())
}

fn check_digest(value: &Value) -> Result<()> {
    let size = match value["algorithm"].as_str() {
        Some("sha256") => 64,
        Some("md5") => 32,
        _ => return Err(verification("文件缺少可校验的 SHA-256 或 MD5")),
    };
    if !value["hex"].as_str().is_some_and(|s| {
        s.len() == size
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(verification("FDB 文件摘要无效"));
    }
    Ok(())
}

fn check_stat(value: &Value, library: &str, pool: Uuid) -> Result<()> {
    check_scope(value, library)?;
    if value["pool_id"] != pool.to_string() {
        return Err(verification("FDB 应答池与请求不一致"));
    }
    uuid_field(value, "id", false)?;
    match value["kind"].as_str() {
        Some("directory") if value["version"].as_u64().is_some_and(|v| v > 0) => Ok(()),
        Some("directory") => Err(verification("FDB 目录属性版本无效")),
        Some("file" | "symlink") => {
            uuid_field(value, "pool_id", false)?;
            uuid_field(value, "volume_id", false)?;
            if value["size"].as_u64().is_none()
                || value["generation"].as_u64().is_none()
                || !value["version"].as_u64().is_some_and(|v| v > 0)
                || !value["barcode"].as_str().is_some_and(|s| !s.is_empty())
            {
                return Err(verification("FDB 文件元数据无效"));
            }
            if !value["digest"].is_null() {
                check_digest(&value["digest"])?;
            }
            Ok(())
        }
        _ => Err(verification("FDB 路径类型无效")),
    }
}

impl FdbClient {
    /// Pool-scoped metadata; this does not mount or read a tape. A missing
    /// digest is reported as null; verified exports still require a digest.
    pub fn stat(&self, library: &str, pool: Uuid, path: &str) -> Result<Option<Value>> {
        if pool.is_nil() {
            return Err(invalid("pool 必须是非空 UUID"));
        }
        let response = self.retry_request(
            "GET",
            &format!("{}&pool={pool}", read_route("stat", library, path)?),
            None,
            &[],
        )?;
        match response.status {
            200 => {
                let value = response.json();
                check_stat(&value, library, pool)?;
                Ok(Some(value))
            }
            404 => Ok(None),
            _ => Err(rejected(response)),
        }
    }

    /// One bounded page in the pool namespace. `after` is the opaque
    /// Base64URL `next` returned by the previous call for this exact directory.
    /// Pages use independent read versions; they are not a directory snapshot.
    pub fn list_children(
        &self,
        library: &str,
        pool: Uuid,
        path: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Value> {
        if pool.is_nil() || !(1..=100).contains(&limit) {
            return Err(invalid("limit 必须在 1..=100"));
        }
        let mut route = format!(
            "{}&limit={limit}&pool={pool}",
            read_route("children", library, path)?
        );
        let decode_cursor = |s: &str| -> Result<Vec<u8>> {
            let bytes = URL_SAFE_NO_PAD
                .decode(s)
                .map_err(|_| invalid("目录游标不是有效的 Base64URL"))?;
            if s.is_empty() || s.len() > 1024 || URL_SAFE_NO_PAD.encode(&bytes) != s {
                return Err(invalid("目录游标长度或编码无效"));
            }
            Ok(bytes)
        };
        let previous = after.map(decode_cursor).transpose()?;
        if let Some(after) = after {
            route.push_str("&after=");
            route.push_str(after);
        }
        let response = self.retry_request("GET", &route, None, &[])?;
        if response.status == 404 {
            return Err(ClientError::NotFound);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        check_scope(&value, library)?;
        if value["pool_id"] != pool.to_string() {
            return Err(verification("FDB 应答池与请求不一致"));
        }
        let items = value["items"]
            .as_array()
            .filter(|items| items.len() <= limit)
            .ok_or_else(|| verification("FDB 目录分页无效"))?;
        let mut last_name = None;
        for item in items {
            uuid_field(item, "id", false)?;
            let name = item["name_b64"]
                .as_str()
                .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
                .ok_or_else(|| verification("FDB 子项名称编码无效"))?;
            if name.is_empty()
                || name.len() > 255
                || name.contains(&b'/')
                || name.contains(&0)
                || matches!(name.as_slice(), b"." | b"..")
                || last_name.as_ref().is_some_and(|last| last >= &name)
                || item["name"] != serde_json::json!(std::str::from_utf8(&name).ok())
            {
                return Err(verification("FDB 子项名称或顺序无效"));
            }
            last_name = Some(name);
        }
        if !value["next"].is_null() {
            let next = value["next"]
                .as_str()
                .ok_or_else(|| verification("FDB 下一页游标无效"))?;
            let decoded = decode_cursor(next)?;
            if previous
                .as_ref()
                .is_some_and(|previous| previous >= &decoded)
            {
                return Err(verification("FDB 目录游标没有前进"));
            }
        } else if value.get("next").is_none() {
            return Err(verification("FDB 目录缺少分页终态"));
        }
        Ok(value)
    }

    /// Stream one ordinary file into a private temporary file, verify size and
    /// its recorded digest, then recheck namespace/inode/version/placement.
    /// Publish with no replacement only after all remote checks pass.
    pub fn download_new(
        &self,
        library: &str,
        pool: Uuid,
        path: &str,
        output: &Path,
    ) -> Result<u64> {
        if pool.is_nil() {
            return Err(invalid("pool 必须是非空 UUID"));
        }
        match fs::symlink_metadata(output) {
            Ok(_) => return Err(ClientError::Exists(output.display().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let before = self
            .stat(library, pool, path)?
            .ok_or(ClientError::NotFound)?;
        if before["kind"] != "file" || before["pool_id"] != pool.to_string() {
            return Err(verification("下载目标必须是指定池的普通文件"));
        }
        check_digest(&before["digest"])?;
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let scratch = ScratchCopy(parent.join(format!(".fdb-download-{}", Uuid::new_v4())));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&scratch.0)?;
        let size = self.download_exact_to(library, pool, path, &before, &mut file, true)?;
        file.sync_all()?;
        fs::hard_link(&scratch.0, output)?;
        File::open(parent)?.sync_all()?;
        Ok(size)
    }
    /// 读取指定命名空间的普通文件到空的私有缓存文件。
    /// 失败时目标可能含部分数据，调用方必须丢弃，不得发布或继续读取。
    /// 不创建路径，不修改已有非空文件；存在 SHA-256/MD5 时严格核验。
    /// 外部 LTFS 文件可无摘要，此时仍核对完整长度、前后身份及版本。
    pub fn download_to_scoped(
        &self,
        library: &str,
        namespace: Uuid,
        pool: Uuid,
        path: &str,
        file: &mut File,
    ) -> Result<u64> {
        if namespace.is_nil() || !file.metadata()?.is_file() || file.metadata()?.len() != 0 {
            return Err(invalid("命名空间须非空，下载目标须为空的私有普通文件"));
        }
        let before = self
            .stat(library, pool, path)?
            .ok_or(ClientError::NotFound)?;
        if before["namespace"] != namespace.to_string() {
            return Err(verification("下载应答命名空间与固定目标不一致"));
        }
        file.rewind()?;
        self.download_exact_to(library, pool, path, &before, file, false)
    }

    fn download_exact_to(
        &self,
        library: &str,
        pool: Uuid,
        path: &str,
        before: &Value,
        file: &mut File,
        require_digest: bool,
    ) -> Result<u64> {
        if before["kind"] != "file" || before["pool_id"] != pool.to_string() {
            return Err(verification("下载目标必须是指定池的普通文件"));
        }
        let digest = if before["digest"].is_null() && !require_digest {
            DownloadDigest::Absent
        } else {
            check_digest(&before["digest"])?;
            if before["digest"]["algorithm"] == "sha256" {
                DownloadDigest::Sha256(sha2::Sha256::new())
            } else {
                DownloadDigest::Md5(md5::Md5::new())
            }
        };
        let size = before["size"]
            .as_u64()
            .ok_or_else(|| verification("文件长度无效"))?;
        let mut sink = DownloadSink {
            file,
            remaining: size,
            digest,
        };
        let route = format!("{}&pool={pool}", read_route("files", library, path)?);
        let deadline = Instant::now() + self.transport.retry_for;
        let response = 'retry: loop {
            for endpoint in &self.transport.endpoints {
                match self.transport.request_with(
                    endpoint,
                    "GET",
                    &route,
                    None,
                    &[],
                    Some(&mut sink),
                ) {
                    Ok(response) if matches!(response.status, 503 | 504) => {}
                    Ok(response) => break 'retry response,
                    Err((false, _)) => {}
                    // A partial body never continues in a different connection.
                    Err((true, error)) => return Err(error.into()),
                }
            }
            if Instant::now() >= deadline {
                return Err(ClientError::NoLeader("FDB owner 暂不可用".into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if response.status != 200 {
            return Err(rejected(response));
        }
        let digest = match sink.digest {
            DownloadDigest::Sha256(hash) => Some(format!("{:x}", hash.finalize())),
            DownloadDigest::Md5(hash) => Some(format!("{:x}", hash.finalize())),
            DownloadDigest::Absent => None,
        };
        if sink.remaining != 0 || digest.is_some_and(|digest| before["digest"]["hex"] != digest) {
            return Err(verification("下载内容长度或摘要不匹配"));
        }
        if self.stat(library, pool, path)?.as_ref() != Some(before) {
            return Err(verification("下载期间路径、命名空间或文件版本已改变"));
        }
        Ok(size)
    }
}

struct DownloadSink<'a> {
    file: &'a mut File,
    remaining: u64,
    digest: DownloadDigest,
}

enum DownloadDigest {
    Sha256(sha2::Sha256),
    Md5(md5::Md5),
    Absent,
}

impl Write for DownloadSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("下载内容超过 FDB 记录长度"));
        }
        let n = self.file.write(bytes)?;
        match &mut self.digest {
            DownloadDigest::Sha256(hash) => hash.update(&bytes[..n]),
            DownloadDigest::Md5(hash) => hash.update(&bytes[..n]),
            DownloadDigest::Absent => {}
        }
        self.remaining -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;

    fn metadata(pool: Uuid, content: &[u8]) -> Value {
        serde_json::json!({"library_serial":"SIM", "namespace":Uuid::new_v4().to_string(), "id":Uuid::new_v4().to_string(),
            "kind":"file", "generation":1, "size":content.len(), "version":1, "pool_id":pool.to_string(), "volume_id":Uuid::new_v4().to_string(),
            "barcode":"READ01L8", "digest":{"algorithm":"sha256", "hex":format!("{:x}",sha2::Sha256::digest(content))}})
    }

    fn response(body: &[u8]) -> Vec<u8> {
        [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body.to_vec(),
        ]
        .concat()
    }

    fn server(
        steps: Vec<(&'static str, Vec<u8>)>,
        race: Option<std::path::PathBuf>,
    ) -> (FdbClient, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let join = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for (i, (route, response)) in steps.into_iter().enumerate() {
                let (mut conn, _) = loop {
                    match listener.accept() {
                        Ok(conn) => break conn,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "未收到请求 {route}");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with(route), "{first}");
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                if i == 2
                    && let Some(path) = &race
                {
                    fs::write(path, b"existing").unwrap();
                }
                let _ = conn.write_all(&response);
            }
        });
        (
            FdbClient::new([address])
                .unwrap()
                .with_timeouts(Duration::from_millis(100), Duration::from_secs(2)),
            join,
        )
    }

    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("fdb-download-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn scoped_private_reads_check_optional_digest_and_original_identity() {
        let root = root();
        let pool = Uuid::new_v4();
        for (i, (algorithm, bytes, changed)) in [
            ("sha256", &b"abc"[..], false),
            ("md5", &b"abc"[..], false),
            ("none", &b"abc"[..], false),
            ("none", &b""[..], false),
            ("none", &b"abc"[..], true),
            ("sha256", &b""[..], false),
            ("sha256", &b"abc"[..], true),
        ]
        .into_iter()
        .enumerate()
        {
            let mut before = metadata(pool, bytes);
            if algorithm == "md5" {
                before["digest"] = serde_json::json!({"algorithm":"md5","hex":format!("{:x}",md5::Md5::digest(bytes))});
            }
            if algorithm == "none" {
                before["digest"] = Value::Null;
            }
            let namespace = Uuid::parse_str(before["namespace"].as_str().unwrap()).unwrap();
            let mut after = before.clone();
            if changed {
                after["id"] = serde_json::json!(Uuid::new_v4().to_string());
            }
            let (client, join) = server(
                vec![
                    ("GET /v1/fdb/stat/", response(before.to_string().as_bytes())),
                    ("GET /v1/fdb/files/", response(bytes)),
                    ("GET /v1/fdb/stat/", response(after.to_string().as_bytes())),
                ],
                None,
            );
            let path = root.join(i.to_string());
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            let result = client.download_to_scoped("SIM", namespace, pool, "/file", &mut file);
            if changed {
                assert!(matches!(result, Err(ClientError::Verification(_))));
            } else {
                assert_eq!(result.unwrap(), bytes.len() as u64);
                assert_eq!(fs::read(&path).unwrap(), bytes);
            }
            join.join().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scoped_unhashed_reads_reject_incomplete_bodies_and_changed_identity() {
        let root = root();
        let pool = Uuid::new_v4();
        for failure in [
            "short",
            "long",
            "version",
            "namespace",
            "volume_id",
            "generation",
        ] {
            let mut before = metadata(pool, b"abc");
            before["digest"] = Value::Null;
            let namespace = Uuid::parse_str(before["namespace"].as_str().unwrap()).unwrap();
            let mut after = before.clone();
            match failure {
                "version" | "generation" => after[failure] = serde_json::json!(2),
                "namespace" | "volume_id" => {
                    after[failure] = serde_json::json!(Uuid::new_v4().to_string())
                }
                _ => {}
            }
            let body = match failure {
                "short" => b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na".to_vec(),
                "long" => response(b"abcd"),
                _ => response(b"abc"),
            };
            let mut steps = vec![
                ("GET /v1/fdb/stat/", response(before.to_string().as_bytes())),
                ("GET /v1/fdb/files/", body),
            ];
            if !matches!(failure, "short" | "long") {
                steps.push(("GET /v1/fdb/stat/", response(after.to_string().as_bytes())));
            }
            let (client, join) = server(steps, None);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(root.join(failure))
                .unwrap();
            assert!(
                client
                    .download_to_scoped("SIM", namespace, pool, "/file", &mut file)
                    .is_err(),
                "{failure}"
            );
            join.join().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scoped_private_reads_reject_wrong_namespace_before_body_and_preserve_nonempty_targets() {
        let root = root();
        let pool = Uuid::new_v4();
        let before = metadata(pool, b"abc");
        let (client, join) = server(
            vec![("GET /v1/fdb/stat/", response(before.to_string().as_bytes()))],
            None,
        );
        let path = root.join("cache");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(matches!(
            client.download_to_scoped("SIM", Uuid::new_v4(), pool, "/file", &mut file),
            Err(ClientError::Verification(_))
        ));
        join.join().unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        file.write_all(b"keep").unwrap();
        let no_server = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::ZERO);
        assert!(
            matches!(no_server.download_to_scoped("SIM",Uuid::new_v4(),pool,"/file",&mut file),Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn downloads_verify_sha256_md5_and_empty_content_without_replacement() {
        let root = root();
        let pool = Uuid::new_v4();
        for (i, (algorithm, bytes)) in [
            ("sha256", &b"abc"[..]),
            ("md5", &b"abc"[..]),
            ("sha256", &b""[..]),
        ]
        .into_iter()
        .enumerate()
        {
            let mut meta = metadata(pool, bytes);
            if algorithm == "md5" {
                meta["digest"] = serde_json::json!({"algorithm":"md5", "hex":format!("{:x}", md5::Md5::digest(bytes))});
            }
            let stat = response(meta.to_string().as_bytes());
            let (client, join) = server(
                vec![
                    ("GET /v1/fdb/stat/", stat.clone()),
                    ("GET /v1/fdb/files/", response(bytes)),
                    ("GET /v1/fdb/stat/", stat),
                ],
                None,
            );
            let output = root.join(i.to_string());
            assert_eq!(
                client
                    .download_new("SIM", pool, "/中文 &?", &output)
                    .unwrap(),
                bytes.len() as u64
            );
            assert_eq!(fs::read(&output).unwrap(), bytes);
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
            join.join().unwrap();
            assert!(matches!(
                client.download_new("SIM", pool, "/a", &output),
                Err(ClientError::Exists(_))
            ));
            assert_eq!(fs::read(&output).unwrap(), bytes);
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3, "不能遗留暂存文件");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_downloads_never_publish_partial_or_changed_files() {
        let root = root();
        let pool = Uuid::new_v4();
        for failure in [
            "short",
            "long",
            "digest",
            "version",
            "namespace",
            "inode",
            "pool",
            "race",
        ] {
            let meta = metadata(pool, b"abc");
            let mut after = meta.clone();
            match failure {
                "version" => after["version"] = serde_json::json!(2),
                "namespace" => after["namespace"] = serde_json::json!(Uuid::new_v4().to_string()),
                "inode" => after["id"] = serde_json::json!(Uuid::new_v4().to_string()),
                "pool" => after["pool_id"] = serde_json::json!(Uuid::new_v4().to_string()),
                _ => {}
            }
            let content = match failure {
                "short" => b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na".to_vec(),
                "long" => response(b"abcd"),
                "digest" => response(b"xyz"),
                _ => response(b"abc"),
            };
            let output = root.join(failure);
            let mut steps = vec![
                ("GET /v1/fdb/stat/", response(meta.to_string().as_bytes())),
                ("GET /v1/fdb/files/", content),
            ];
            if !matches!(failure, "short" | "long" | "digest") {
                steps.push(("GET /v1/fdb/stat/", response(after.to_string().as_bytes())));
            }
            let (client, join) = server(steps, (failure == "race").then(|| output.clone()));
            assert!(
                client.download_new("SIM", pool, "/a", &output).is_err(),
                "{failure}"
            );
            join.join().unwrap();
            if failure == "race" {
                assert_eq!(fs::read(&output).unwrap(), b"existing");
                fs::remove_file(output).unwrap();
            } else {
                assert!(!output.exists(), "{failure}");
            }
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0, "{failure}");
        }
        let target = root.join("target");
        fs::write(&target, b"existing").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let client = FdbClient::new(["127.0.0.1:1"]).unwrap();
        assert!(matches!(
            client.download_new("SIM", pool, "/a", &link),
            Err(ClientError::Exists(_))
        ));
        assert_eq!(fs::read(target).unwrap(), b"existing");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_only_queries_and_download_preflight_fail_closed() {
        let root = root();
        let pool = Uuid::new_v4();
        for field in ["pool_id", "kind", "digest"] {
            let mut meta = metadata(pool, b"abc");
            meta[field] = match field {
                "pool_id" => serde_json::json!(Uuid::new_v4().to_string()),
                "kind" => serde_json::json!("symlink"),
                _ => Value::Null,
            };
            let (client, join) = server(
                vec![("GET /v1/fdb/stat/", response(meta.to_string().as_bytes()))],
                None,
            );
            assert!(
                client
                    .download_new("SIM", pool, "/a", &root.join("out"))
                    .is_err()
            );
            join.join().unwrap();
        }
        let directory = serde_json::json!({"kind":"directory", "version":1, "id":Uuid::new_v4().to_string(), "namespace":Uuid::new_v4().to_string(), "library_serial":"SIM", "pool_id":pool.to_string()});
        let mut old = directory.clone();
        old.as_object_mut().unwrap().remove("namespace");
        let (client, join) = server(
            vec![
                (
                    "GET /v1/fdb/stat/",
                    response(directory.to_string().as_bytes()),
                ),
                ("GET /v1/fdb/stat/", response(old.to_string().as_bytes())),
            ],
            None,
        );
        assert_eq!(
            client.stat("SIM", pool, "/").unwrap(),
            Some(directory.clone())
        );
        assert!(
            client.stat("SIM", pool, "/").is_err(),
            "应答缺少 namespace 必须拒绝"
        );
        for version in [
            Value::Null,
            serde_json::json!(0),
            serde_json::json!("1"),
            serde_json::json!(-1),
        ] {
            let mut bad = directory.clone();
            bad["version"] = version;
            assert!(
                check_stat(&bad, "SIM", pool).is_err(),
                "目录版本不得缺失或靠类型转换补值"
            );
        }
        join.join().unwrap();
        for path in ["", "a", "/a/", "/a//b", "/../b", "/a\0b"] {
            assert!(read_route("stat", "SIM", path).is_err());
        }
        assert!(read_route("stat", "SIM?pool=x", "/").is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pagination_keeps_opaque_cursor_and_rejects_regression_or_bad_names() {
        let ns = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let cursor = URL_SAFE_NO_PAD.encode(b"opaque-c-key");
        let page = serde_json::json!({"library_serial":"SIM", "namespace":ns.to_string(), "pool_id":pool.to_string(),
            "items":[{"name":"a", "name_b64":"YQ", "id":Uuid::new_v4().to_string()}], "next":cursor});
        let mut bad = page.clone();
        bad["items"][0]["name"] = serde_json::json!("different");
        let (client, join) = server(
            vec![
                (
                    "GET /v1/fdb/children/SIM?path=Lw&limit=1&pool=",
                    response(page.to_string().as_bytes()),
                ),
                (
                    "GET /v1/fdb/children/",
                    response(page.to_string().as_bytes()),
                ),
                (
                    "GET /v1/fdb/children/",
                    response(bad.to_string().as_bytes()),
                ),
            ],
            None,
        );
        assert_eq!(
            client.list_children("SIM", pool, "/", 1, None).unwrap(),
            page
        );
        assert!(
            client
                .list_children("SIM", pool, "/", 1, Some(&cursor))
                .is_err(),
            "游标不能原地循环"
        );
        assert!(client.list_children("SIM", pool, "/", 1, None).is_err());
        join.join().unwrap();
        assert!(client.list_children("SIM", pool, "/", 101, None).is_err());
        assert!(
            client
                .list_children("SIM", pool, "/", 1, Some("bad&path=other"))
                .is_err()
        );
    }
}
