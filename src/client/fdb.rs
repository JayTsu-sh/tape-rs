//! FDB uploads preserve one target and original payload in a durable directory.
//! An uncertain PUT is resolved by operation identity, never by path contents
//! and never by preparing another upload or silently changing its tape.
use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use uuid::Uuid;

mod conditions;
pub use conditions::UploadCondition;
mod inventory;
mod listing;
mod management;
mod notifications;
pub use management::PreparedManagement;
mod nodes;
mod receipts;
pub use nodes::{NodeCondition, NodeJournal, PreparedNodeMutation};
mod renames;
pub use receipts::CommittedUpload;
pub use renames::{PreparedRename, RenameCondition, RenameJournal, RenameSpec};
mod pools;
mod reading;
mod upload_ack;
mod upload_intent;
mod upload_state;
pub use listing::ListObjectsOptions;
pub use upload_state::UploadJournal;

fn valid_management_barcode(barcode: &str) -> bool {
    !barcode.is_empty()
        && barcode.len() <= 64
        && barcode
            .bytes()
            .all(|b| (b'!'..=b'~').contains(&b) && b != b'/')
}

fn invalid(reason: &str) -> ClientError {
    ClientError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        reason,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStatus {
    /// 原逻辑意图与正文已保存，尚未固定服务端准备结果。
    NotPrepared,
    Committed(u64),
    Unconfirmed,
}

/// Opaque, validated request returned by the owner. Save before uploading.
#[derive(Debug, Clone)]
pub struct PreparedUpload {
    value: Value,
}

impl PreparedUpload {
    fn parse(value: Value) -> Result<Self> {
        let fields = [
            "version",
            "library",
            "pool",
            "path",
            "namespace",
            "metadata",
        ];
        if value["version"] != 2
            || !value.as_object().is_some_and(|obj| {
                obj.len() == fields.len() && obj.keys().all(|key| fields.contains(&key.as_str()))
            })
        {
            return Err(invalid("上传请求头字段或版本无效"));
        }
        let serial = value["library"]
            .as_str()
            .ok_or_else(|| invalid("缺少带库标识"))?;
        if serial.is_empty()
            || serial.len() > 64
            || !serial
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("上传请求带库标识无效"));
        }
        for field in ["pool", "namespace"] {
            if !value[field]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(|id| !id.is_nil())
            {
                return Err(invalid("上传请求池或命名空间 UUID 无效"));
            }
        }
        let encoded = value["path"]
            .as_str()
            .ok_or_else(|| invalid("缺少上传路径"))?;
        let path = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid("上传路径编码无效"))?;
        validate_path(std::str::from_utf8(&path).map_err(|_| invalid("上传路径不是 UTF-8"))?)?;
        let metadata = value["metadata"]
            .as_str()
            .filter(|s| s.len() <= 4096)
            .ok_or_else(|| invalid("上传身份长度无效"))?;
        let raw = URL_SAFE_NO_PAD
            .decode(metadata)
            .map_err(|_| invalid("上传身份编码无效"))?;
        let identity: Value =
            serde_json::from_slice(&raw).map_err(|_| invalid("上传身份不是 JSON"))?;
        let fields = [
            "version",
            "operation",
            "epoch",
            "parent",
            "file",
            "previous",
            "barcode",
            "size",
            "sha256",
            "namespace",
        ];
        if raw != identity.to_string().as_bytes()
            || identity["version"] != 2
            || identity["namespace"] != value["namespace"]
            || !identity.as_object().is_some_and(|obj| {
                obj.len() == fields.len() && obj.keys().all(|key| fields.contains(&key.as_str()))
            })
        {
            return Err(invalid("上传身份字段或版本无效"));
        }
        for field in ["operation", "parent", "file"] {
            if !identity[field]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(|id| !id.is_nil())
            {
                return Err(invalid("上传身份 UUID 无效"));
            }
        }
        if !identity["epoch"].as_u64().is_some_and(|v| v > 0)
            || identity["size"].as_u64().is_none()
            || !(identity["previous"].is_null()
                || identity["previous"]
                    .as_u64()
                    .is_some_and(|v| v > 0 && v < u64::MAX))
            || !identity["sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("上传身份大小、版本或摘要无效"));
        }
        if !identity["barcode"].is_null()
            && !identity["barcode"].as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes().all(|b| b.is_ascii_graphic() && b != b'/')
            })
        {
            return Err(invalid("上传条码约束无效"));
        }
        Ok(Self { value })
    }

    fn identity(&self) -> Value {
        serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(self.value["metadata"].as_str().expect("validated"))
                .expect("validated"),
        )
        .expect("validated")
    }

    fn route(&self, operation: &str) -> String {
        format!(
            "/v1/fdb/{operation}/{}?pool={}&path={}",
            self.value["library"].as_str().unwrap(),
            self.value["pool"].as_str().unwrap(),
            self.value["path"].as_str().unwrap()
        )
    }

    fn headers(&self) -> [(&str, &str); 2] {
        [
            (
                "x-tapers-fdb-write",
                self.value["metadata"].as_str().unwrap(),
            ),
            (
                "x-tapers-fdb-namespace",
                self.value["namespace"].as_str().unwrap(),
            ),
        ]
    }

    #[cfg(test)]
    fn save_fixture(&self, path: &Path, source: &Path) -> Result<()> {
        let original = upload_intent::OriginalUpload::fixture(self);
        upload_state::save_new(&original, path, source)?;
        upload_state::UploadJournal::open_locked(path, true)?.record_prepared(self.clone())
    }

    pub fn load(path: &Path) -> Result<Self> {
        upload_state::UploadJournal::open_locked(path, false)?
            .request
            .ok_or_else(|| invalid("原请求尚未完成 prepare，请保留目录并使用 upload 重试"))
    }
}

fn save_value_new(value: &Value, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".fdb-request-{}", Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{}", value)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let cleanup = fs::remove_file(&temporary);
    result?;
    cleanup?;
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || !crate::ltfs::xml_text_valid(path)
        || path.len() > 2048
        || path[1..]
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\0'))
    {
        return Err(invalid("上传路径必须是规范绝对文件路径"));
    }
    Ok(())
}

pub struct FdbClient {
    transport: Client,
}

impl FdbClient {
    pub fn new<S: Into<String>>(endpoints: impl IntoIterator<Item = S>) -> Result<Self> {
        let transport = Client::new(endpoints);
        if transport.endpoints.is_empty()
            || transport.endpoints.iter().any(|address| {
                address.is_empty() || address.contains('/') || address.chars().any(char::is_control)
            })
        {
            return Err(invalid("FDB 节点地址须为不带路径的 host:port"));
        }
        Ok(Self { transport })
    }

    pub fn with_timeouts(mut self, retry: Duration, io: Duration) -> Self {
        self.transport.retry_for = retry;
        self.transport.io_timeout = io;
        self
    }

    fn retry_request(
        &self,
        method: &str,
        route: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response> {
        let deadline = Instant::now() + self.transport.retry_for;
        loop {
            for endpoint in &self.transport.endpoints {
                match self.transport.request_with(
                    endpoint,
                    method,
                    route,
                    body.map(Body::Mem),
                    headers,
                    None,
                ) {
                    Ok(response) if !matches!(response.status, 503 | 504) => return Ok(response),
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(ClientError::NoLeader("FDB owner 暂不可用".into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Persist original intent and independent bytes before any prepare RPC.
    /// The destination must not exist; resume a saved directory with upload_saved.
    #[allow(clippy::too_many_arguments)] // Six distinct target/input fields plus the mandatory durable destination.
    pub fn prepare_file(
        &self,
        library: &str,
        pool: Uuid,
        path: &str,
        source: &Path,
        condition: UploadCondition,
        barcode: Option<&str>,
        directory: &Path,
    ) -> Result<PreparedUpload> {
        let mut saved =
            UploadJournal::create(library, pool, path, source, condition, barcode, directory)?;
        self.prepare_journal(&mut saved)
    }

    /// Prepare this held original request; an error leaves the same lease and identity intact.
    pub fn prepare_journal(&self, saved: &mut UploadJournal) -> Result<PreparedUpload> {
        saved.require_exclusive()?;
        if let Some(request) = &saved.request {
            return Ok(request.clone());
        }
        // An unprepared state cannot have been admitted. Verify and reestablish
        // local durability before its first (or resumed) read-only prepare.
        saved.payload()?;
        let original = &saved.original;
        let body =
            serde_json::json!({"operation":original.operation.to_string(),"size":original.size,
            "sha256":original.sha256,"previous":original.previous,"barcode":original.barcode})
            .to_string();
        let route = format!(
            "/v1/fdb/uploads/{}/prepare?pool={}&path={}",
            original.library,
            original.pool,
            URL_SAFE_NO_PAD.encode(&original.path)
        );
        let response = self.retry_request(
            "POST",
            &route,
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let request = PreparedUpload::parse(response.json())?;
        original.verify(&request)?;
        saved.record_prepared(request.clone())?;
        Ok(request)
    }

    /// Read durable original completion locally; otherwise query only the original identity.
    pub fn status_saved(&self, directory: &Path) -> Result<UploadStatus> {
        let saved = upload_state::UploadJournal::open_locked(directory, false)?;
        if let Some(committed) = saved.acknowledged()? {
            return Ok(UploadStatus::Committed(
                committed.details()["version"].as_u64().unwrap(),
            ));
        }
        match &saved.request {
            Some(request) => self.status(request),
            None => Ok(UploadStatus::NotPrepared),
        }
    }

    /// Read the complete original receipt without changing the saved directory.
    pub fn saved_receipt(&self, directory: &Path) -> Result<Option<CommittedUpload>> {
        let saved = upload_state::UploadJournal::open_locked(directory, false)?;
        if let Some(receipt) = saved.acknowledged()? {
            return Ok(Some(receipt));
        }
        let Some(request) = &saved.request else {
            return Ok(None);
        };
        self.upload_receipt(request)?
            .map(|value| CommittedUpload::parse(value, request))
            .transpose()
    }

    pub fn status(&self, request: &PreparedUpload) -> Result<UploadStatus> {
        let response =
            self.retry_request("GET", &request.route("uploads"), None, &request.headers())?;
        let value = response.json();
        match (
            response.status,
            value["status"].as_str(),
            value["version"].as_u64(),
        ) {
            (200, Some("committed"), Some(version))
                if version == request.identity()["previous"].as_u64().unwrap_or(0) + 1 =>
            {
                Ok(UploadStatus::Committed(version))
            }
            (202, Some("unconfirmed"), _) => Ok(UploadStatus::Unconfirmed),
            (200 | 202, _, _) => Err(ClientError::Verification("上传结果应答无效".into())),
            _ => Err(rejected(response)),
        }
    }

    /// Only a saved request can be submitted. Success requires the complete
    /// original receipt. A lost response triggers queries, never another PUT,
    /// another identity or another tape in this call.
    pub fn upload_saved(&self, state: &Path) -> Result<CommittedUpload> {
        let mut saved = UploadJournal::open(state)?;
        self.upload_journal(&mut saved)
    }

    /// Submit this held request and durably acknowledge its original complete result.
    pub fn upload_journal(&self, saved: &mut UploadJournal) -> Result<CommittedUpload> {
        saved.require_exclusive()?;
        if let Some(committed) = saved.acknowledged()? {
            saved.release_payload()?;
            return Ok(committed);
        }
        let committed = self.upload_open(saved)?;
        saved.record_acknowledged(&committed)?;
        saved.release_payload()?;
        Ok(committed)
    }

    fn upload_open(&self, saved: &mut upload_state::UploadJournal) -> Result<CommittedUpload> {
        let request = self.prepare_journal(saved)?;
        let identity = request.identity();
        let size = identity["size"].as_u64().unwrap();
        if let UploadStatus::Committed(version) = self.status(&request)? {
            return self.committed_upload(&request, version);
        }
        let payload = saved.payload()?;
        let mut problem = "尚未收到上传回执".to_owned();
        for endpoint in &self.transport.endpoints {
            match self.transport.request_with(
                endpoint,
                "PUT",
                &request.route("files"),
                Some(Body::OpenedFile(&payload, size)),
                &request.headers(),
                None,
            ) {
                Ok(response) if response.status == 201 => {
                    let value = response.json();
                    if let Some(version) = value["version"].as_u64().filter(|v| {
                        *v == identity["previous"].as_u64().unwrap_or(0) + 1
                            && value["status"] == "committed"
                    }) {
                        return self.committed_upload(&request, version);
                    }
                    problem = "上传成功应答缺少原版本或提交状态".into();
                    break;
                }
                Ok(response)
                    if (400..500).contains(&response.status)
                        && !matches!(response.status, 408 | 429) =>
                {
                    return Err(rejected(response));
                }
                Ok(response) => {
                    problem = format!(
                        "上传 HTTP {}：{}",
                        response.status,
                        String::from_utf8_lossy(&response.body[..response.body.len().min(4096)])
                    );
                    break;
                }
                Err((true, error)) => {
                    problem = error.to_string();
                    break;
                }
                Err((false, error)) => {
                    problem = error.to_string();
                    continue;
                }
            }
        }
        match self.status(&request) {
            Ok(UploadStatus::Committed(version)) => self.committed_upload(&request, version),
            _ => Err(ClientError::Indeterminate(format!(
                "{problem}；请保留原请求目录及正文，查询或重试同一目录；不要重新 prepare"
            ))),
        }
    }
}

fn rejected(response: Response) -> ClientError {
    ClientError::Rejected {
        status: response.status,
        body: String::from_utf8_lossy(&response.body).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assert_upload_committed(committed: CommittedUpload, directory: &Path, version: u64) {
        let request = PreparedUpload::load(directory).unwrap();
        let identity = request.identity();
        assert_eq!(
            committed.version(),
            FileVersion::Fdb {
                namespace: Uuid::parse_str(identity["namespace"].as_str().unwrap()).unwrap(),
                inode: Uuid::parse_str(identity["file"].as_str().unwrap()).unwrap(),
                version,
            }
        );
        assert_eq!(
            committed.operation(),
            Uuid::parse_str(identity["operation"].as_str().unwrap()).unwrap()
        );
    }

    pub(super) fn request() -> PreparedUpload {
        let namespace = Uuid::new_v4().to_string();
        let metadata = serde_json::json!({
            "version":2,"namespace":namespace,"operation":Uuid::new_v4().to_string(),"epoch":1,"parent":Uuid::new_v4().to_string(),
            "file":Uuid::new_v4().to_string(),"previous":null,"barcode":null,"size":3,"sha256":format!("{:x}", sha2::Sha256::digest(b"abc")),
        });
        PreparedUpload::parse(serde_json::json!({
            "version":2,"library":"SIM","pool":Uuid::new_v4().to_string(),"path":URL_SAFE_NO_PAD.encode("/a"),
            "namespace":namespace,"metadata":URL_SAFE_NO_PAD.encode(metadata.to_string()),
        })).unwrap()
    }

    #[test]
    fn preparation_persists_before_rpc_and_resumes_the_same_operation_without_source() {
        preparation_workflow(false);
    }

    #[test]
    fn held_journal_keeps_original_operation_after_lost_reply_and_directory_replacement() {
        preparation_workflow(true);
    }

    #[test]
    fn shared_journal_rejects_mutation_before_rpc_and_exclusive_lease_moves_between_threads() {
        let root = std::env::temp_dir().join(format!("fdb-prepare-journal-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source = root.join("source");
        let state = root.join("request");
        fs::write(&source, b"abc").unwrap();
        request().save_fixture(&state, &source).unwrap();
        let header = fs::read(state.join("request.json")).unwrap();
        let client = FdbClient::new(["127.0.0.1:0"]).unwrap();
        let mut shared = UploadJournal::open_locked(&state, false).unwrap();
        for result in [
            client.prepare_journal(&mut shared).map(|_| ()),
            client.upload_journal(&mut shared).map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
            );
        }
        drop(shared);
        let journal = UploadJournal::open(&state).unwrap();
        let operation = journal.operation();
        let gate = Arc::new(std::sync::Barrier::new(2));
        let other_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            other_gate.wait();
            other_gate.wait();
            journal
        });
        gate.wait();
        let occupied = UploadJournal::open(&state).is_err();
        gate.wait();
        let mut journal = worker.join().unwrap();
        assert!(occupied);
        assert_eq!(journal.operation(), operation);
        fs::write(state.join("request.json"), b"foreign header").unwrap();
        assert!(matches!(
            client.upload_journal(&mut journal),
            Err(ClientError::Verification(_))
        ));
        assert_eq!(fs::read(state.join("payload")).unwrap(), b"abc");
        fs::write(state.join("request.json"), header).unwrap();
        drop(journal);
        assert!(UploadJournal::open(&state).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    fn preparation_workflow(keep_handle: bool) {
        let root = std::env::temp_dir().join(format!("fdb-prepare-journal-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        eprintln!("fdb_prepare_journal_workspace={}", root.display());
        let source = root.join("source");
        let mut state = root.join("request");
        fs::write(&source, b"abc").unwrap();
        let response = request();
        let namespace = Uuid::parse_str(response.value["namespace"].as_str().unwrap()).unwrap();
        let pool = Uuid::parse_str(response.value["pool"].as_str().unwrap()).unwrap();
        let parent = Uuid::parse_str(response.identity()["parent"].as_str().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let server_state = Arc::new(std::sync::Mutex::new(state.clone()));
        let current_location = server_state.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut first_operation = Value::Null;
            let mut selected = response.clone();
            while requests.load(Ordering::SeqCst) < 5 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = header.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let number = requests.fetch_add(1, Ordering::SeqCst) + 1;
                let (status, answer) = match number {
                    1 | 2 => {
                        assert!(line.starts_with("POST /v1/fdb/uploads/SIM/prepare?"));
                        let input: Value = serde_json::from_slice(&body).unwrap();
                        let saved: Value = serde_json::from_slice(
                            &fs::read(server_state.lock().unwrap().join("request.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(saved["version"], 2);
                        assert_eq!(saved["original"]["version"], 2);
                        assert_eq!(saved["original"]["parent"], response.identity()["parent"]);
                        assert!(saved["prepared"].is_null());
                        assert_eq!(
                            fs::read(server_state.lock().unwrap().join("payload")).unwrap(),
                            b"abc"
                        );
                        assert_eq!(input["operation"], saved["original"]["operation"]);
                        if number == 1 {
                            first_operation = input["operation"].clone();
                            // Lose the read-only reply after observing a complete durable local state.
                            continue;
                        }
                        assert_eq!(input["operation"], first_operation);
                        let mut prepared = response.value.clone();
                        let mut identity = response.identity();
                        identity["operation"] = first_operation.clone();
                        prepared["metadata"] =
                            serde_json::json!(URL_SAFE_NO_PAD.encode(identity.to_string()));
                        selected = PreparedUpload::parse(prepared.clone()).unwrap();
                        ("200 OK", prepared.to_string())
                    }
                    3 => {
                        assert!(line.starts_with("GET /v1/fdb/uploads/SIM?"));
                        let saved: Value = serde_json::from_slice(
                            &fs::read(server_state.lock().unwrap().join("request.json")).unwrap(),
                        )
                        .unwrap();
                        assert!(
                            saved["prepared"].is_object(),
                            "selection must be durable before admission"
                        );
                        ("202 Accepted", "{\"status\":\"unconfirmed\"}".into())
                    }
                    4 => {
                        assert!(line.starts_with("PUT /v1/fdb/files/SIM?"));
                        assert_eq!(body, b"abc");
                        (
                            "201 Created",
                            "{\"status\":\"committed\",\"version\":1}".into(),
                        )
                    }
                    5 => {
                        assert!(line.starts_with("GET /v1/fdb/upload-receipts/SIM?"));
                        ("200 OK", receipts::tests::receipt(&selected).to_string())
                    }
                    _ => unreachable!(),
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .unwrap();
            }
            assert_eq!(requests.load(Ordering::SeqCst), 5);
        });
        let client = FdbClient::new([&address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(2));
        let condition = UploadCondition::Create { namespace, parent };
        let mut journal = if keep_handle {
            Some(
                UploadJournal::create("SIM", pool, "/a", &source, condition, None, &state).unwrap(),
            )
        } else {
            None
        };
        if let Some(journal) = &journal {
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert_eq!(journal.library(), "SIM");
            assert_eq!(journal.namespace(), namespace);
            assert_eq!(journal.pool(), pool);
            assert_eq!(journal.path(), "/a");
            assert_eq!(journal.size(), 3);
            assert_eq!(
                journal.sha256(),
                format!("{:x}", sha2::Sha256::digest(b"abc"))
            );
            assert_eq!(journal.condition().unwrap(), condition);
            assert!(journal.prepared().is_none());
        }
        let result = match &mut journal {
            Some(journal) => client.prepare_journal(journal),
            None => client.prepare_file("SIM", pool, "/a", &source, condition, None, &state),
        };
        assert!(matches!(result, Err(ClientError::NoLeader(_))));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        if let Some(journal) = &journal {
            assert!(journal.prepared().is_none());
            assert!(journal.acknowledged().unwrap().is_none());
        } else {
            assert_eq!(
                client.status_saved(&state).unwrap(),
                UploadStatus::NotPrepared
            );
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "local draft status must not issue a query with invented physical fields"
        );
        let foreign = if let Some(journal) = &journal {
            let original_path = state.clone();
            let held = root.join("held");
            fs::rename(&state, &held).unwrap();
            *current_location.lock().unwrap() = held.clone();
            let foreign_source = root.join("foreign-source");
            fs::write(&foreign_source, b"xyz").unwrap();
            let other = UploadJournal::create(
                "SIM",
                pool,
                "/a",
                &foreign_source,
                condition,
                None,
                &original_path,
            )
            .unwrap();
            assert_ne!(other.operation(), journal.operation());
            let snapshot = (
                original_path.clone(),
                fs::read(original_path.join("request.json")).unwrap(),
                fs::read(original_path.join("payload")).unwrap(),
            );
            drop(other);
            state = held;
            Some(snapshot)
        } else {
            None
        };
        fs::write(&source, b"xyz").unwrap();
        fs::remove_file(&source).unwrap();
        drop(client);
        let client = FdbClient::new([&address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(2));
        let committed = match &mut journal {
            Some(journal) => {
                let original_operation = journal.operation();
                let committed = client.upload_journal(journal).unwrap();
                assert_eq!(committed.operation(), original_operation);
                assert_eq!(journal.acknowledged().unwrap(), Some(committed.clone()));
                assert_eq!(client.upload_journal(journal).unwrap(), committed);
                committed
            }
            None => client.upload_saved(&state).unwrap(),
        };
        drop(journal);
        assert_upload_committed(committed, &state, 1);
        if let Some((path, header, payload)) = foreign {
            assert_eq!(fs::read(path.join("request.json")).unwrap(), header);
            assert_eq!(fs::read(path.join("payload")).unwrap(), payload);
            assert!(!path.join("receipt.json").exists());
        }

        server.join().unwrap();
        let selected = PreparedUpload::load(&state).unwrap();
        let saved = upload_state::UploadJournal::open_locked(&state, false).unwrap();
        assert_eq!(
            selected.identity()["operation"],
            saved.original.operation.to_string()
        );
        drop(saved);
        fs::remove_file(state.join("request.json")).unwrap();
        assert!(
            matches!(client.upload_saved(&state),Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::NotFound)
        );
        assert_eq!(count.load(Ordering::SeqCst), 5);
        assert!(!state.join("request.json").exists());
        assert!(!state.join("payload").exists());
        assert!(state.join("receipt.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zero_previous_cannot_alias_a_create_only_upload() {
        let original = request();
        let mut value = original.value.clone();
        let mut identity = original.identity();
        identity["previous"] = serde_json::json!(0);
        value["metadata"] = serde_json::json!(URL_SAFE_NO_PAD.encode(identity.to_string()));
        assert!(PreparedUpload::parse(value).is_err());
        let client = FdbClient::new(["127.0.0.1:0"]).unwrap();
        let missing = std::env::temp_dir().join(format!("not-read-{}", Uuid::new_v4()));
        assert!(
            matches!(client.prepare_file("SIM",Uuid::new_v4(),"/file",&missing,UploadCondition::Replace(FileVersion::Fdb {namespace:Uuid::new_v4(),inode:Uuid::new_v4(),version:0}),None,&missing.with_extension("request")),Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn request_directory_is_private_atomic_and_never_replaced() {
        for invalid in ["/bad\u{1}", "/bad\u{fffe}", "/bad\u{ffff}"] {
            assert!(validate_path(invalid).is_err());
        }
        let root = std::env::temp_dir().join(format!("fdb-client-state-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("request");
        let source = root.join("source");
        fs::write(&source, b"abc").unwrap();
        let request = request();
        request.save_fixture(&path, &source).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in [upload_state::HEADER, upload_state::PAYLOAD] {
            assert_eq!(
                fs::metadata(path.join(name)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(PreparedUpload::load(&path).unwrap().value, request.value);
        let before = fs::read(path.join(upload_state::HEADER)).unwrap();
        assert_eq!(fs::read(path.join(upload_state::PAYLOAD)).unwrap(), b"abc");
        assert!(request.save_fixture(&path, &source).is_err());
        assert_eq!(fs::read(path.join(upload_state::HEADER)).unwrap(), before);
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(PreparedUpload::load(&link).is_err());
        assert!(request.save_fixture(&link, &source).is_err());
        fs::write(path.join(upload_state::HEADER), b"{\"version\":").unwrap();
        assert!(PreparedUpload::load(&path).is_err());
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            3,
            "失败不能遗留临时请求目录"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncertain_put_queries_original_request_and_never_retransmits() {
        let root = std::env::temp_dir().join(format!("fdb-client-retry-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        eprintln!("fdb_frozen_source_workspace={}", root.display());
        let state = root.join("request");
        let source = root.join("source");
        let prepared = request();
        fs::write(&source, b"abc").unwrap();
        prepared.save_fixture(&state, &source).unwrap();
        fs::write(&source, b"xyz").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut handled = 0;
            while handled != 4 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let put = line.starts_with("PUT ");
                let mut length = 0;
                let mut namespace = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                        if key.eq_ignore_ascii_case("x-tapers-fdb-namespace") {
                            namespace = true;
                        }
                    }
                }
                assert!(namespace, "每次提交和查询都必须绑定原 namespace");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                handled += 1;
                if put {
                    assert_eq!(body, b"abc");
                    count.fetch_add(1, Ordering::SeqCst);
                    // Commit succeeds but the TCP connection loses the reply.
                    continue;
                }
                let committed = count.load(Ordering::SeqCst) > 0;
                let (code, body) = if line.starts_with("GET /v1/fdb/upload-receipts/") {
                    assert!(committed);
                    (200, receipts::tests::receipt(&prepared).to_string())
                } else if committed {
                    (200, "{\"status\":\"committed\",\"version\":1}".to_owned())
                } else {
                    (202, "{\"status\":\"unconfirmed\"}".to_owned())
                };
                write!(
                    stream,
                    "HTTP/1.1 {code} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            assert_eq!(handled, 4);
        });
        let client = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::from_secs(1), Duration::from_secs(2));
        assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
        assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        fs::remove_file(&source).unwrap();
        assert!(!state.join(upload_state::PAYLOAD).exists());
        assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
