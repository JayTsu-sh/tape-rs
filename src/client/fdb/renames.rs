//! Original rename protocol; no FoundationDB SDK linkage is needed.
use super::nodes::{fields, uuid};
use super::*;
use sha2::{Digest, Sha256};
mod conditions;
mod journal;
pub use conditions::{RenameCondition, RenameSpec};
pub use journal::RenameJournal;
const MAX_REQUEST: usize = 8 << 20;
const MAX_SAVED: usize = MAX_REQUEST + 4096;
const MAX_TIME: u64 = 253_402_300_799_999;
fn fingerprint(request: &Value) -> String {
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"tape-rs/node-rename-request/1")
            .chain_update(request.to_string().as_bytes())
            .finalize()
    )
}
fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        text.len() == 64
            && text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}
#[derive(Clone, Debug)]
pub struct PreparedRename {
    value: Value,
}
impl PreparedRename {
    fn parse(value: Value) -> Result<Self> {
        if !fields(
            &value,
            &[
                "version",
                "kind",
                "library_serial",
                "namespace",
                "request",
                "fingerprint",
            ],
        ) || value["version"] != 1
            || value["kind"] != "rename"
            || value.to_string().len() > MAX_SAVED
        {
            return Err(invalid("rename 准备结果字段、版本或长度无效"));
        }
        let library = value["library_serial"]
            .as_str()
            .ok_or_else(|| invalid("rename 准备结果缺少带库"))?;
        Client::new(Vec::<String>::new()).with_library(library)?;
        let namespace = uuid(&value, "namespace")?;
        let request = &value["request"];
        if !fields(
            request,
            &[
                "version",
                "library",
                "pool",
                "operation",
                "request_epoch",
                "from",
                "to",
                "source",
                "source_parent",
                "destination_parent",
                "target",
                "no_replace",
                "time",
                "baseline_sha256",
            ],
        ) || request["version"] != 1
            || request["library"] != library
            || request.to_string().len() > MAX_REQUEST
            || !request["request_epoch"]
                .as_u64()
                .is_some_and(|epoch| epoch > 0)
            || !request["baseline_sha256"].as_array().is_some_and(|bytes| {
                bytes.len() == 32
                    && bytes
                        .iter()
                        .all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255))
            })
            || value["fingerprint"] != fingerprint(request)
        {
            return Err(invalid("rename 规范请求身份、字段或摘要无效"));
        }
        uuid(request, "operation")?;
        let spec = RenameSpec::from_request(request)?;
        if spec.condition.namespace()? != namespace {
            return Err(invalid("rename 准备结果改变了 namespace"));
        }
        let time = request["time"]
            .as_str()
            .ok_or_else(|| invalid("rename 原时间缺失"))?;
        crate::ltfs::index::MetadataChange {
            modify_time: Some(time.into()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid("rename 原时间不是规范 LTFS UTC 时间"))?;
        Ok(Self { value })
    }
    fn request(&self) -> &Value {
        &self.value["request"]
    }
    fn library(&self) -> &str {
        self.value["library_serial"].as_str().unwrap()
    }
    fn namespace(&self) -> &str {
        self.value["namespace"].as_str().unwrap()
    }
    fn base(&self) -> String {
        format!("/v1/fdb/renames/{}", self.library())
    }
    fn route(&self) -> String {
        format!(
            "{}/requests/{}/{}",
            self.base(),
            self.request()["request_epoch"],
            self.request()["operation"].as_str().unwrap()
        )
    }
    fn verify(&self, value: Value) -> Result<Value> {
        let bad = || ClientError::Verification("rename 回执与原双路径请求或保留状态不符".into());
        let request = self.request();
        if !fields(
            &value,
            &[
                "version",
                "kind",
                "library_serial",
                "namespace",
                "pool",
                "epoch",
                "operation",
                "fingerprint",
                "status",
                "accepted_owner_epoch",
                "created_at_millis",
                "prepared_owner_epoch",
                "prepared_at_millis",
                "completed_owner_epoch",
                "completed_at_millis",
                "rejection_reason",
                "result",
            ],
        ) || value["version"] != 1
            || value["kind"] != "rename"
            || value["library_serial"] != self.library()
            || value["namespace"] != self.namespace()
            || value["pool"] != request["pool"]
            || value["epoch"] != request["request_epoch"]
            || value["operation"] != request["operation"]
            || value["fingerprint"] != self.value["fingerprint"]
        {
            return Err(bad());
        }
        let accepted = value["accepted_owner_epoch"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(bad)?;
        let created = value["created_at_millis"]
            .as_u64()
            .filter(|n| (1..=MAX_TIME).contains(n))
            .ok_or_else(bad)?;
        let prepared = match (&value["prepared_owner_epoch"], &value["prepared_at_millis"]) {
            (Value::Null, Value::Null) => None,
            (owner, time) => Some((
                owner.as_u64().filter(|n| *n >= accepted).ok_or_else(bad)?,
                time.as_u64()
                    .filter(|n| (created..=MAX_TIME).contains(n))
                    .ok_or_else(bad)?,
            )),
        };
        let state = value["status"].as_str().ok_or_else(bad)?;
        let terminal = matches!(state, "completed" | "cancelled" | "rejected");
        if !matches!(
            state,
            "queued" | "prepared" | "writing" | "completed" | "cancelled" | "rejected"
        ) || (state == "queued" && prepared.is_some())
            || (matches!(state, "prepared" | "writing" | "completed") && prepared.is_none())
        {
            return Err(bad());
        }
        if terminal {
            let (owner, time) = prepared.unwrap_or((accepted, created));
            if !value["completed_owner_epoch"]
                .as_u64()
                .is_some_and(|n| n >= owner)
                || !value["completed_at_millis"]
                    .as_u64()
                    .is_some_and(|n| (time..=MAX_TIME).contains(&n))
            {
                return Err(bad());
            }
        } else if !value["completed_owner_epoch"].is_null()
            || !value["completed_at_millis"].is_null()
        {
            return Err(bad());
        }
        if state == "rejected" {
            if !value["rejection_reason"].as_str().is_some_and(|s| {
                !s.trim().is_empty() && s.len() <= 500 && !s.chars().any(char::is_control)
            }) {
                return Err(bad());
            }
        } else if !value["rejection_reason"].is_null() {
            return Err(bad());
        }
        if state == "completed" {
            let result = &value["result"];
            let spec = RenameSpec::from_request(request)?;
            let (source, left, right, target) = spec.condition.revisions()?;
            if !fields(
                result,
                &[
                    "source",
                    "source_parent",
                    "destination_parent",
                    "replaced",
                    "directory",
                    "group",
                    "attributes_digest",
                    "source_parent_attributes_digest",
                    "destination_parent_attributes_digest",
                    "publication_digest",
                ],
            ) || result["source"] != source.after()
                || result["source_parent"] != left.after()
                || result["destination_parent"] != right.after()
                || result["replaced"]
                    != target
                        .map(|revision| revision.after())
                        .unwrap_or(Value::Null)
                || !result["directory"].is_boolean()
                || uuid(result, "group").is_err()
                || [
                    "attributes_digest",
                    "source_parent_attributes_digest",
                    "destination_parent_attributes_digest",
                    "publication_digest",
                ]
                .iter()
                .any(|key| !digest(&result[*key]))
            {
                return Err(bad());
            }
        } else if !value["result"].is_null() {
            return Err(bad());
        }
        Ok(value)
    }
}
fn uncertain(error: ClientError) -> ClientError {
    match error {
        ClientError::NoLeader(_) | ClientError::Io(_) => ClientError::Indeterminate(
            "rename 原结果暂不可查询；保留原日志目录，按同一操作查询或重试".into(),
        ),
        error => error,
    }
}

impl FdbClient {
    /// Persist the original intent before read-only preparation. Retry with the saved directory.
    pub fn prepare_rename(
        &self,
        library: &str,
        spec: &RenameSpec,
        directory: &Path,
    ) -> Result<PreparedRename> {
        let mut saved = RenameJournal::create(library, spec, directory)?;
        self.prepare_rename_journal(&mut saved)
    }

    pub fn prepare_rename_journal(&self, saved: &mut RenameJournal) -> Result<PreparedRename> {
        saved.durable()?;
        saved.terminal()?;
        if let Some(prepared) = &saved.prepared {
            return Ok(prepared.clone());
        }
        let input = saved.original.input();
        let namespace = saved.namespace().to_string();
        let answer = self.retry_request(
            "POST",
            &format!("/v1/fdb/renames/{}/prepare", saved.library()),
            Some(input.to_string().as_bytes()),
            &[
                ("Content-Type", "application/json"),
                ("x-tapers-catalog-namespace", &namespace),
            ],
        )?;
        if answer.status != 200 {
            return Err(rejected(answer));
        }
        let prepared = PreparedRename::parse(answer.json())
            .map_err(|error| ClientError::Verification(format!("rename准备应答无效：{error}")))?;
        saved.original.verify(&prepared)?;
        saved.record_prepared(prepared.clone())?;
        Ok(prepared)
    }

    /// Read-only original status. A Draft returns None without an RPC.
    pub fn rename_status_saved(&self, directory: &Path) -> Result<Option<Value>> {
        let saved = RenameJournal::open_shared(directory)?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(Some(terminal));
        }
        match saved.prepared() {
            Some(prepared) => self.rename_status(prepared),
            None => Ok(None),
        }
    }

    pub fn rename_status(&self, request: &PreparedRename) -> Result<Option<Value>> {
        let response = self.retry_request(
            "GET",
            &request.route(),
            None,
            &[("x-tapers-catalog-namespace", request.namespace())],
        )?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        request.verify(response.json()).map(Some)
    }

    /// Resolve the original operation under the same journal lease; cache any terminal result.
    pub fn run_rename_journal(&self, saved: &mut RenameJournal) -> Result<Value> {
        saved.durable()?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(terminal);
        }
        let prepared = self.prepare_rename_journal(saved)?;
        let terminal = self.run_rename_request(&prepared)?;
        saved.record_terminal(&terminal)?;
        Ok(terminal)
    }

    pub fn run_rename_saved(&self, path: &Path) -> Result<Value> {
        let mut saved = RenameJournal::open(path)?;
        let terminal = self.run_rename_journal(&mut saved)?;
        if terminal["status"] != "completed" {
            return Err(ClientError::Rejected {
                status: 409,
                body: terminal.to_string(),
            });
        }
        Ok(terminal)
    }

    fn run_rename_request(&self, request: &PreparedRename) -> Result<Value> {
        let deadline = Instant::now() + self.transport.retry_for;
        let mut status = self.rename_status(request).map_err(uncertain)?;
        if status.is_none() {
            let bytes = request.request().to_string();
            match self.retry_request(
                "POST",
                &format!("{}/requests", request.base()),
                Some(bytes.as_bytes()),
                &[
                    ("Content-Type", "application/json"),
                    ("x-tapers-catalog-namespace", request.namespace()),
                ],
            ) {
                Ok(response) if matches!(response.status, 200 | 202) => {
                    status = Some(request.verify(response.json())?)
                }
                Ok(response) => return Err(rejected(response)),
                Err(_) => {}
            }
        }
        loop {
            if let Some(value) = status
                && matches!(
                    value["status"].as_str(),
                    Some("completed" | "rejected" | "cancelled")
                )
            {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(ClientError::Indeterminate(
                    "重命名尚未确认；保留原日志目录，按同一操作查询或重试".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
            status = self.rename_status(request).map_err(uncertain)?;
        }
    }

    pub fn cancel_rename_saved(&self, path: &Path) -> Result<Value> {
        let mut saved = RenameJournal::open(path)?;
        self.cancel_rename_journal(&mut saved)
    }

    pub fn cancel_rename_journal(&self, saved: &mut RenameJournal) -> Result<Value> {
        saved.durable()?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(terminal);
        }
        let request = saved
            .prepared()
            .ok_or_else(|| invalid("原rename请求尚未准备或登记，不发送取消"))?;
        if let Some(known) = self.rename_status(request).map_err(uncertain)?
            && matches!(
                known["status"].as_str(),
                Some("completed" | "cancelled" | "rejected")
            )
        {
            saved.record_terminal(&known)?;
            return Ok(known);
        }
        let response = self
            .retry_request(
                "DELETE",
                &request.route(),
                None,
                &[("x-tapers-catalog-namespace", request.namespace())],
            )
            .map_err(uncertain)?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let terminal = request.verify(response.json())?;
        saved.record_terminal(&terminal)?;
        Ok(terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> RenameSpec {
        let namespace = Uuid::new_v4();
        let version = |inode, version| FileVersion::Fdb {
            namespace,
            inode,
            version,
        };
        let parent = version(Uuid::new_v4(), 7);
        RenameSpec {
            pool: Uuid::new_v4(),
            from: "/old".into(),
            to: "/new".into(),
            no_replace: true,
            condition: RenameCondition {
                source: version(Uuid::new_v4(), 3),
                source_parent: parent,
                destination_parent: parent,
                target: None,
            },
        }
    }
    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rename-journal-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        root
    }
    fn prepared(saved: &RenameJournal) -> PreparedRename {
        let original = &saved.original;
        let mut request = original.input();
        request["library"] = original.library.clone().into();
        request["request_epoch"] = 1.into();
        request["baseline_sha256"] = serde_json::json!(vec![0u8; 32]);
        PreparedRename::parse(serde_json::json!({"version":1,"kind":"rename","library_serial":original.library,
            "namespace":saved.namespace().to_string(),"fingerprint":fingerprint(&request),"request":request})).unwrap()
    }
    fn terminal(prepared: &PreparedRename) -> Value {
        let request = prepared.request();
        let condition = RenameCondition::from_request(request).unwrap();
        let (source, left, right, target) = condition.revisions().unwrap();
        serde_json::json!({"version":1,"kind":"rename","library_serial":prepared.library(),"namespace":prepared.namespace(),
            "pool":request["pool"],"epoch":request["request_epoch"],"operation":request["operation"],"fingerprint":prepared.value["fingerprint"],
            "status":"completed","accepted_owner_epoch":12,"created_at_millis":100,"prepared_owner_epoch":12,"prepared_at_millis":110,
            "completed_owner_epoch":13,"completed_at_millis":120,"rejection_reason":null,
            "result":{"source":source.after(),"source_parent":left.after(),"destination_parent":right.after(),"replaced":target.map(|r|r.after()),
                "directory":false,"group":Uuid::new_v4().to_string(),"attributes_digest":"01".repeat(32),"source_parent_attributes_digest":"02".repeat(32),
                "destination_parent_attributes_digest":"03".repeat(32),"publication_digest":"04".repeat(32)}})
    }
    #[test]
    fn conditions_pin_both_full_parents_and_explicit_target_absence() {
        let original = spec();
        original.validate().unwrap();
        for altered in [
            {
                let mut s = original.clone();
                s.condition.source = FileVersion::Raft {
                    round: 1,
                    sequence: 1,
                };
                s
            },
            {
                let mut s = original.clone();
                let namespace = s.condition.namespace().unwrap();
                s.condition.target = Some(FileVersion::Fdb {
                    namespace,
                    inode: Uuid::new_v4(),
                    version: 1,
                });
                s
            },
            {
                let mut s = original.clone();
                s.to = s.from.clone();
                s
            },
            {
                let mut s = original.clone();
                s.to = "/old/child".into();
                s
            },
            {
                let mut s = original.clone();
                s.from = "/".into();
                s
            },
            {
                let mut s = original.clone();
                s.condition.destination_parent = s.condition.source;
                s
            },
            {
                let mut s = original.clone();
                s.condition.target = Some(s.condition.source);
                s
            },
            {
                let mut s = original.clone();
                s.condition.destination_parent = FileVersion::Fdb {
                    namespace: Uuid::new_v4(),
                    inode: Uuid::new_v4(),
                    version: 7,
                };
                s
            },
            {
                let mut s = original.clone();
                let FileVersion::Fdb {
                    namespace, inode, ..
                } = s.condition.source
                else {
                    unreachable!()
                };
                s.condition.source = FileVersion::Fdb {
                    namespace,
                    inode,
                    version: u64::MAX,
                };
                s
            },
        ] {
            assert!(altered.validate().is_err());
        }
    }
    #[test]
    fn receipt_validates_original_two_path_versions_kind_and_audit() {
        let root = root();
        let saved = RenameJournal::create("LIB", &spec(), &root.join("op")).unwrap();
        let prepared = prepared(&saved);
        let good = terminal(&prepared);
        prepared.verify(good.clone()).unwrap();
        for altered in [
            {
                let mut v = good.clone();
                v["kind"] = "single-node".into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["source"]["inode"] = Uuid::new_v4().to_string().into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["source_parent"]["version"] = 9.into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["destination_parent"]["version"] = 9.into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["replaced"] = v["result"]["source"].clone();
                v
            },
            {
                let mut v = good.clone();
                v["completed_owner_epoch"] = 11.into();
                v
            },
            {
                let mut v = good.clone();
                v["completed_at_millis"] = 109.into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["publication_digest"] = "AB".repeat(32).into();
                v
            },
            {
                let mut v = good.clone();
                v["result"]["extra"] = true.into();
                v
            },
        ] {
            assert!(prepared.verify(altered).is_err());
        }
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn draft_reopen_keeps_operation_time_and_never_recaptures_conditions() {
        let root = root();
        let path = root.join("op");
        let original = spec();
        let saved = RenameJournal::create("LIB", &original, &path).unwrap();
        let operation = saved.operation();
        let time = saved.time().to_owned();
        assert!(
            RenameJournal::open(&path).is_err(),
            "competing exclusive lease"
        );
        drop(saved);
        let saved = RenameJournal::open(&path).unwrap();
        assert_eq!(saved.spec(), &original);
        assert_eq!(saved.operation(), operation);
        assert_eq!(saved.time(), time);
        assert!(saved.prepared().is_none());
        drop(saved);
        let offline = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(20));
        assert!(offline.rename_status_saved(&path).unwrap().is_none());
        assert!(RenameJournal::create("LIB", &original, &path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cached_original_terminal_survives_no_service_and_required_header_cannot_be_lost() {
        let root = root();
        let path = root.join("op");
        let mut saved = RenameJournal::create("LIB", &spec(), &path).unwrap();
        let prepared = prepared(&saved);
        let good = terminal(&prepared);
        saved.record_prepared(prepared).unwrap();
        saved.record_terminal(&good).unwrap();
        drop(saved);
        let offline = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(20));
        assert_eq!(offline.run_rename_saved(&path).unwrap(), good);
        assert_eq!(
            offline.rename_status_saved(&path).unwrap(),
            Some(good.clone())
        );
        assert_eq!(offline.cancel_rename_saved(&path).unwrap(), good);
        fs::rename(path.join("request.json"), path.join("required-held")).unwrap();
        assert!(offline.run_rename_saved(&path).is_err());
        assert!(offline.rename_status_saved(&path).is_err());
        assert!(path.join("receipt.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn held_root_rename_does_not_write_a_replacement_at_its_old_path() {
        let root = root();
        let path = root.join("op");
        let held = root.join("held");
        let mut saved = RenameJournal::create("LIB", &spec(), &path).unwrap();
        let prepared = prepared(&saved);
        let good = terminal(&prepared);
        fs::rename(&path, &held).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("foreign"), b"untouched").unwrap();
        saved.record_prepared(prepared).unwrap();
        saved.record_terminal(&good).unwrap();
        drop(saved);
        assert!(!path.join("request.json").exists());
        assert!(!path.join("receipt.json").exists());
        assert_eq!(fs::read(path.join("foreign")).unwrap(), b"untouched");
        let offline = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(20));
        assert_eq!(offline.run_rename_saved(&held).unwrap(), good);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_cached_receipt_and_replaced_header_are_never_network_fallbacks() {
        let root = root();
        let path = root.join("op");
        let mut saved = RenameJournal::create("LIB", &spec(), &path).unwrap();
        let prepared = prepared(&saved);
        let good = terminal(&prepared);
        saved.record_prepared(prepared).unwrap();
        saved.record_terminal(&good).unwrap();
        drop(saved);
        let offline = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(20));
        let receipt = fs::read(path.join("receipt.json")).unwrap();
        fs::write(path.join("receipt.json"), b"{}").unwrap();
        assert!(offline.run_rename_saved(&path).is_err());
        fs::write(path.join("receipt.json"), receipt).unwrap();
        let saved = RenameJournal::open(&path).unwrap();
        let bytes = fs::read(path.join("request.json")).unwrap();
        fs::rename(path.join("request.json"), path.join("header-held")).unwrap();
        fs::write(path.join("request.json"), bytes).unwrap();
        assert!(saved.terminal().is_err());
        drop(saved);
        fs::remove_file(path.join("request.json")).unwrap();
        std::os::unix::fs::symlink("header-held", path.join("request.json")).unwrap();
        assert!(RenameJournal::open(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preparation_rejects_recreated_same_version_parents_without_changing_original_header() {
        let root = root();
        let path = root.join("op");
        let mut saved = RenameJournal::create("LIB", &spec(), &path).unwrap();
        let bytes = fs::read(path.join("request.json")).unwrap();
        let mut wrong = prepared(&saved).value;
        let replacement = Uuid::new_v4().to_string();
        wrong["request"]["source_parent"]["inode"] = replacement.clone().into();
        wrong["request"]["destination_parent"]["inode"] = replacement.into();
        wrong["fingerprint"] = fingerprint(&wrong["request"]).into();
        let wrong = PreparedRename::parse(wrong).unwrap();
        assert!(saved.record_prepared(wrong).is_err());
        assert!(saved.prepared().is_none());
        assert_eq!(fs::read(path.join("request.json")).unwrap(), bytes);
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repeated_terminal_does_not_replace_first_owner_time_or_receipt() {
        let root = root();
        let path = root.join("op");
        let mut saved = RenameJournal::create("LIB", &spec(), &path).unwrap();
        let prepared = prepared(&saved);
        let good = terminal(&prepared);
        saved.record_prepared(prepared).unwrap();
        saved.record_terminal(&good).unwrap();
        let bytes = fs::read(path.join("receipt.json")).unwrap();
        let mut changed = good.clone();
        changed["completed_owner_epoch"] = 14.into();
        changed["completed_at_millis"] = 121.into();
        assert!(saved.record_terminal(&changed).is_err());
        assert_eq!(saved.terminal().unwrap(), Some(good));
        assert_eq!(fs::read(path.join("receipt.json")).unwrap(), bytes);
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }
}
