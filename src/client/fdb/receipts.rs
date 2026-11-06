//! Detailed results refer to the saved original upload, never today's path.
use super::*;
use sha2::{Digest, Sha256};

/// Verified complete result of one immutable original upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedUpload {
    version: FileVersion,
    operation: Uuid,
    details: Value,
}

impl CommittedUpload {
    pub(super) fn parse(details: Value, request: &PreparedUpload) -> Result<Self> {
        verify(&details, request)?;
        Ok(Self {
            version: FileVersion::Fdb {
                namespace: reading::uuid_field(&details, "namespace", false)?,
                inode: reading::uuid_field(&details, "id", false)?,
                version: details["version"]
                    .as_u64()
                    .ok_or_else(|| invalid("原回执版本无效"))?,
            },
            operation: reading::uuid_field(&details, "operation", false)?,
            details,
        })
    }

    pub fn version(&self) -> FileVersion {
        self.version
    }

    pub fn operation(&self) -> Uuid {
        self.operation
    }

    pub fn details(&self) -> &Value {
        &self.details
    }
}

pub(super) fn verify(value: &Value, request: &PreparedUpload) -> Result<()> {
    let bad = || ClientError::Verification("原上传回执与已保存身份不一致".into());
    let fields = [
        "schema_version",
        "status",
        "library_serial",
        "namespace",
        "pool_id",
        "path",
        "operation",
        "epoch",
        "parent",
        "id",
        "version",
        "copy",
        "volume",
        "generation",
        "scan",
        "file_uid",
        "barcode",
        "size",
        "sha256",
        "attributes",
    ];
    if !value
        .as_object()
        .is_some_and(|v| v.len() == fields.len() && v.keys().all(|k| fields.contains(&k.as_str())))
    {
        return Err(bad());
    }
    let identity = request.identity();
    let path = URL_SAFE_NO_PAD
        .decode(request.value["path"].as_str().unwrap())
        .map_err(|_| bad())?;
    let path = std::str::from_utf8(&path).map_err(|_| bad())?;
    let pool = Uuid::parse_str(request.value["pool"].as_str().unwrap()).map_err(|_| bad())?;
    let library = request.value["library"].as_str().unwrap();
    nodes::verify_attributes(&value["attributes"], library, pool, path)?;
    if value["schema_version"] != 1
        || value["status"] != "committed"
        || value["library_serial"] != library
        || value["pool_id"] != request.value["pool"]
        || value["path"] != path
        || value["id"] != identity["file"]
        || value["version"] != identity["previous"].as_u64().unwrap_or(0) + 1
        || [
            "namespace",
            "operation",
            "epoch",
            "parent",
            "size",
            "sha256",
        ]
        .iter()
        .any(|key| value[*key] != identity[*key])
        || !value["file_uid"].as_u64().is_some_and(|v| v > 0)
        || !value["barcode"].as_str().is_some_and(|s| {
            !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic() && b != b'/')
        })
        || ["namespace", "id", "version"]
            .iter()
            .any(|key| value[*key] != value["attributes"][*key])
        || value["file_uid"] != value["attributes"]["metadata"]["file_uid"]
        || value["attributes"]["kind"] != "file"
    {
        return Err(bad());
    }
    for field in ["volume", "scan"] {
        reading::uuid_field(value, field, false)?;
    }
    if !value["generation"].as_u64().is_some_and(|n| n > 0)
        || (!identity["barcode"].is_null() && value["barcode"] != identity["barcode"])
    {
        return Err(bad());
    }
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/copy-id/1");
    for key in ["namespace", "file"] {
        hash.update(
            Uuid::parse_str(identity[key].as_str().unwrap())
                .map_err(|_| bad())?
                .as_bytes(),
        );
    }
    hash.update(identity["epoch"].as_u64().unwrap().to_be_bytes());
    hash.update(
        Uuid::parse_str(identity["operation"].as_str().unwrap())
            .map_err(|_| bad())?
            .as_bytes(),
    );
    hash.update(identity["previous"].as_u64().unwrap_or(0).to_be_bytes());
    let digest = hash.finalize();
    if value["copy"] != Uuid::from_bytes(digest[..16].try_into().unwrap()).to_string() {
        return Err(bad());
    }
    Ok(())
}

impl FdbClient {
    pub(super) fn committed_upload(
        &self,
        request: &PreparedUpload,
        version: u64,
    ) -> Result<CommittedUpload> {
        let details = self.upload_receipt(request)?.ok_or_else(|| {
            ClientError::Verification(
                "原请求已报告提交，但缺少可验证的完整回执；请保留请求目录及正文".into(),
            )
        })?;
        if details["version"] != version {
            return Err(ClientError::Verification(
                "完整回执与原提交版本不一致".into(),
            ));
        }
        CommittedUpload::parse(details, request)
    }

    /// Original complete S4 result; None means this original upload is unconfirmed.
    pub fn upload_receipt(&self, request: &PreparedUpload) -> Result<Option<Value>> {
        let response = self.retry_request(
            "GET",
            &request.route("upload-receipts"),
            None,
            &request.headers(),
        )?;
        if response.body.len() > 64 << 20 {
            return Err(ClientError::Verification("原上传回执超过响应预算".into()));
        }
        let value = response.json();
        if response.status == 202 && value == serde_json::json!({"status":"unconfirmed"}) {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        verify(&value, request)?;
        Ok(Some(value))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filesystem_consumes_original_typed_receipt_and_advances_the_next_condition() {
        use crate::client::{FileStat, PathStat};
        use crate::tapefs::{
            Backend, DirectoryListing, ROOT_INO, Res, TapeFs, UploadOutcome, UploadState,
            UploadTarget,
        };
        use std::sync::Mutex;
        struct ReceiptBackend {
            root: FileVersion,
            receipt: CommittedUpload,
            targets: Mutex<Vec<UploadTarget>>,
        }
        impl Backend for ReceiptBackend {
            fn root_version(&self) -> Option<FileVersion> {
                Some(self.root)
            }
            fn stat(&self, path: &str) -> Res<Option<PathStat>> {
                if path != "/" {
                    return Ok(None);
                }
                Ok(Some(PathStat {
                    state: "committed".into(),
                    length: 0,
                    current: Some(FileStat {
                        version: self.root,
                        length: 0,
                        generation: None,
                        round: None,
                        barcode: String::new(),
                        sha256: String::new(),
                        metadata: json!({"kind":"directory"}),
                    }),
                }))
            }
            fn list_dir(&self, _: &str) -> Res<Option<DirectoryListing>> {
                Ok(None)
            }
            fn fetch(&self, _: &str, _: &mut File) -> Res<u64> {
                Err(nix::libc::ENOENT)
            }
            fn upload(
                &self,
                _: &str,
                _: &Path,
                state: &mut UploadState,
                _: bool,
            ) -> Res<UploadOutcome> {
                self.targets.lock().unwrap().push(state.target());
                Ok(UploadOutcome::Fdb(self.receipt.clone()))
            }
            fn delete(&self, _: &str) -> Res<()> {
                Err(nix::libc::EROFS)
            }
        }
        for wrong_body in [false, true] {
            let request = super::super::tests::request();
            let identity = request.identity();
            let namespace = Uuid::parse_str(identity["namespace"].as_str().unwrap()).unwrap();
            let parent = Uuid::parse_str(identity["parent"].as_str().unwrap()).unwrap();
            let receipt = CommittedUpload::parse(receipt(&request), &request).unwrap();
            let expected_version = receipt.version();
            let root = std::env::temp_dir().join(format!("fdb-core-receipt-{}", Uuid::new_v4()));
            let fs = TapeFs::new(
                ReceiptBackend {
                    root: FileVersion::Fdb {
                        namespace,
                        inode: parent,
                        version: 1,
                    },
                    receipt,
                    targets: Mutex::new(Vec::new()),
                },
                root.clone(),
            )
            .unwrap();
            let (attr, writer) = fs.create(ROOT_INO, "a", true).unwrap();
            fs.write(writer, 0, if wrong_body { b"xyz" } else { b"abc" })
                .unwrap();
            if wrong_body {
                assert_eq!(fs.fsync(writer), Err(nix::libc::EIO));
            } else {
                fs.fsync(writer).unwrap();
                assert_eq!(
                    fs.getxattr(attr.ino, "user.tape.version").unwrap(),
                    expected_version.to_string().as_bytes()
                );
                assert_eq!(fs.getxattr(attr.ino, "user.tape.generation").unwrap(), b"2");
                assert_eq!(
                    fs.getxattr(attr.ino, "user.tape.barcode").unwrap(),
                    b"TEST01L8"
                );
                assert_eq!(fs.getxattr(attr.ino, "user.original").unwrap(), [0, 255]);
                assert!(fs.getattr(attr.ino).unwrap().readonly);
                fs.write(writer, 0, b"abc").unwrap();
                assert_eq!(
                    fs.fsync(writer),
                    Err(nix::libc::EIO),
                    "the previous create receipt cannot confirm a new replacement"
                );
            }
            let mut expected = vec![UploadTarget::Fdb(UploadCondition::Create {
                namespace,
                parent,
            })];
            if !wrong_body {
                expected.push(UploadTarget::Fdb(UploadCondition::Replace(
                    expected_version,
                )));
            }
            assert_eq!(*fs.backend().targets.lock().unwrap(), expected);
            fs.release(writer);
            drop(fs);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn lite_commit_without_original_details_does_not_acknowledge_the_upload() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let root = std::env::temp_dir().join(format!("fdb-upload-ack-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        eprintln!("fdb_upload_ack_workspace={}", root.display());
        let source = root.join("source");
        let saved = root.join("request");
        fs::write(&source, b"abc").unwrap();
        let request = super::super::tests::request();
        request.save_fixture(&saved, &source).unwrap();
        let header = fs::read(saved.join("request.json")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut handled = 0;
            while handled < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut input = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert!(line.starts_with(if handled == 0 {
                    "GET /v1/fdb/uploads/"
                } else {
                    "GET /v1/fdb/upload-receipts/"
                }));
                loop {
                    let mut header = String::new();
                    input.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                }
                let (code, body) = if handled == 0 {
                    ("200 OK", "{\"status\":\"committed\",\"version\":1}")
                } else {
                    ("202 Accepted", "{\"status\":\"unconfirmed\"}")
                };
                write!(
                    stream,
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                handled += 1;
            }
            handled
        });
        let client = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(1));
        let result = client.upload_saved(&saved);
        let handled = server.join().unwrap();
        assert!(
            matches!(result, Err(ClientError::Verification(_))),
            "a numeric committed status alone must not release the original body: {result:?}"
        );
        assert_eq!(handled, 2);
        assert_eq!(fs::read(saved.join("request.json")).unwrap(), header);
        assert_eq!(fs::read(saved.join("payload")).unwrap(), b"abc");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn put_ack_waits_for_original_details_and_retry_never_resubmits_the_body() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let root = std::env::temp_dir().join(format!("fdb-upload-full-ack-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        eprintln!("fdb_full_upload_ack_workspace={}", root.display());
        let source = root.join("source");
        let saved = root.join("request");
        fs::write(&source, b"abc").unwrap();
        let request = super::super::tests::request();
        request.save_fixture(&saved, &source).unwrap();
        let original = receipt(&request);
        let complete = original.clone();
        let foreign = receipt(&super::super::tests::request());
        let header = fs::read(saved.join("request.json")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut number = 0;
            let mut puts = 0;
            while number < 5 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut input = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let mut size = 0;
                loop {
                    let mut header = String::new();
                    input.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = header.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; size];
                input.read_exact(&mut body).unwrap();
                number += 1;
                let (status, response) = match number {
                    1 => {
                        assert!(line.starts_with("GET /v1/fdb/uploads/"));
                        ("202 Accepted", json!({"status":"unconfirmed"}))
                    }
                    2 => {
                        assert!(line.starts_with("PUT /v1/fdb/files/"));
                        assert_eq!(body, b"abc");
                        puts += 1;
                        ("201 Created", json!({"status":"committed","version":1}))
                    }
                    3 => {
                        assert!(line.starts_with("GET /v1/fdb/upload-receipts/"));
                        ("200 OK", foreign.clone())
                    }
                    4 => {
                        assert!(line.starts_with("GET /v1/fdb/uploads/"));
                        ("200 OK", json!({"status":"committed","version":1}))
                    }
                    5 => {
                        assert!(line.starts_with("GET /v1/fdb/upload-receipts/"));
                        ("200 OK", complete.clone())
                    }
                    _ => unreachable!(),
                };
                let body = response.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            assert_eq!(number, 5);
            puts
        });
        let client = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(1));
        assert!(matches!(
            client.upload_saved(&saved),
            Err(ClientError::Verification(_))
        ));
        assert_eq!(fs::read(saved.join("request.json")).unwrap(), header);
        assert_eq!(fs::read(saved.join("payload")).unwrap(), b"abc");
        fs::remove_file(source).unwrap();
        fs::remove_file(saved.join("payload")).unwrap();
        let committed = client.upload_saved(&saved).unwrap();
        assert_eq!(committed.details(), &original);
        assert_eq!(
            committed.version(),
            FileVersion::Fdb {
                namespace: reading::uuid_field(&original, "namespace", false).unwrap(),
                inode: reading::uuid_field(&original, "id", false).unwrap(),
                version: 1,
            }
        );
        assert_eq!(
            committed.operation(),
            reading::uuid_field(&original, "operation", false).unwrap()
        );
        assert_eq!(server.join().unwrap(), 1);
        assert_eq!(fs::read(saved.join("request.json")).unwrap(), header);
        fs::remove_dir_all(root).unwrap();
    }

    pub(crate) fn receipt(request: &PreparedUpload) -> Value {
        let identity = request.identity();
        let mut hash = Sha256::new().chain_update(b"tape-rs/copy-id/1");
        for key in ["namespace", "file"] {
            hash.update(
                Uuid::parse_str(identity[key].as_str().unwrap())
                    .unwrap()
                    .as_bytes(),
            );
        }
        hash.update(1u64.to_be_bytes());
        hash.update(
            Uuid::parse_str(identity["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        hash.update(0u64.to_be_bytes());
        let hash = hash.finalize();
        json!({"schema_version":1,"status":"committed","library_serial":"SIM",
            "namespace":identity["namespace"],"pool_id":request.value["pool"],"path":"/a",
            "operation":identity["operation"],"epoch":1,"parent":identity["parent"],"id":identity["file"],
            "version":1,"copy":Uuid::from_bytes(hash[..16].try_into().unwrap()).to_string(),
            "volume":Uuid::new_v4().to_string(),"generation":2,"scan":Uuid::new_v4().to_string(),"file_uid":7,
            "barcode":"TEST01L8","size":3,"sha256":identity["sha256"],
            "attributes":{"schema_version":1,"library_serial":"SIM","namespace":identity["namespace"],
                "pool_id":request.value["pool"],"path":"/a","id":identity["file"],"version":1,"kind":"file",
                "attributes_digest":"7".repeat(64),"metadata":{"readonly":true,"file_uid":7,
                "creation_time":"","change_time":"raw original time","modify_time":"","access_time":"","backup_time":""},
                "xattrs":[{"key":"user.original","value":"AP8=","base64":true}],"symlink_target":null}})
    }

    #[test]
    fn complete_original_ack_survives_restart_without_server_or_payload() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let root = std::env::temp_dir().join(format!("fdb-local-ack-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        eprintln!("fdb_local_ack_workspace={}", root.display());
        let source = root.join("source");
        let saved = root.join("request");
        fs::write(&source, b"abc").unwrap();
        let request = super::super::tests::request();
        request.save_fixture(&saved, &source).unwrap();
        let original = receipt(&request);
        let reply = original.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut handled = 0;
            while handled < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut input = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert!(line.starts_with(if handled == 0 {
                    "GET /v1/fdb/uploads/"
                } else {
                    "GET /v1/fdb/upload-receipts/"
                }));
                loop {
                    let mut header = String::new();
                    input.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                }
                let value = if handled == 0 {
                    json!({"status":"committed","version":1})
                } else {
                    reply.clone()
                };
                let body = value.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                handled += 1;
            }
            assert_eq!(handled, 2);
        });
        let client = FdbClient::new([&address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(1));
        let committed = client.upload_saved(&saved).unwrap();
        assert_eq!(committed.details(), &original);
        server.join().unwrap();
        drop(client);
        assert!(
            saved.join("receipt.json").is_file(),
            "the complete original acknowledgement must be durable before releasing its body"
        );
        assert!(!saved.join("payload").exists());
        fs::remove_file(source).unwrap();
        let restarted = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(50));
        assert_eq!(restarted.upload_saved(&saved).unwrap(), committed);
        assert_eq!(
            restarted.status_saved(&saved).unwrap(),
            UploadStatus::Committed(1)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detailed_result_rejects_another_identity_or_inconsistent_attributes() {
        let request = super::super::tests::request();
        let mut value = receipt(&request);
        verify(&value, &request).unwrap();
        for field in [
            "namespace",
            "pool_id",
            "path",
            "operation",
            "epoch",
            "parent",
            "id",
            "version",
            "copy",
            "volume",
            "generation",
            "scan",
            "file_uid",
            "barcode",
            "size",
            "sha256",
        ] {
            let mut bad = value.clone();
            bad[field] = Value::Null;
            assert!(verify(&bad, &request).is_err(), "{field}");
        }
        for field in ["id", "namespace", "version", "kind", "metadata", "xattrs"] {
            let mut bad = value.clone();
            bad["attributes"][field] = Value::Null;
            assert!(verify(&bad, &request).is_err(), "attributes.{field}");
        }
        value["unexpected"] = json!(true);
        assert!(verify(&value, &request).is_err());
    }
}
