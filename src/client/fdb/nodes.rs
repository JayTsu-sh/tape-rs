//! Saved logical filesystem requests work without the FoundationDB SDK.
use super::*;
use sha2::{Digest, Sha256};
mod conditions;
mod journal;
pub use conditions::NodeCondition;
pub use journal::NodeJournal;

const MAX_REQUEST: usize = 8 << 20;
const MAX_SAVED: usize = MAX_REQUEST + 4096;
const MAX_ATTRIBUTE_RESPONSE: usize = 64 << 20;
const MAX_TIME: u64 = 253_402_300_799_999;

pub(super) fn fields(value: &Value, names: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|v| v.len() == names.len() && v.keys().all(|k| names.contains(&k.as_str())))
}

pub(super) fn uuid(value: &Value, field: &str) -> Result<Uuid> {
    let id = reading::uuid_field(value, field, false)?;
    if value[field] != id.to_string() {
        return Err(invalid("节点请求 UUID 必须使用规范形式"));
    }
    Ok(id)
}

fn mutation(value: &Value) -> Result<(bool, bool)> {
    let kind = value["kind"]
        .as_str()
        .ok_or_else(|| invalid("节点变更缺少 kind"))?;
    let valid = match kind {
        "create-directory" | "unlink" | "remove-directory" => fields(value, &["kind"]),
        "create-symlink" => {
            fields(value, &["kind", "target"])
                && value["target"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && crate::ltfs::xml_text_valid(s))
        }
        "set-xattr" => {
            fields(value, &["kind", "attribute", "mode"])
                && fields(&value["attribute"], &["key", "value", "base64"])
                && value["attribute"]["key"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && value["attribute"]["value"].is_string()
                && value["attribute"]["base64"].is_boolean()
                && matches!(
                    value["mode"].as_str(),
                    Some("upsert" | "create" | "replace")
                )
        }
        "remove-xattr" => {
            fields(value, &["kind", "key"]) && value["key"].as_str().is_some_and(|s| !s.is_empty())
        }
        "change-metadata" => {
            if !fields(value, &["kind", "readonly", "access_time", "modify_time"])
                || !(value["readonly"].is_null() || value["readonly"].is_boolean())
                || ["access_time", "modify_time"]
                    .iter()
                    .any(|key| !(value[key].is_null() || value[key].is_string()))
            {
                return Err(invalid("节点属性变更字段无效"));
            }
            let change = crate::ltfs::index::MetadataChange {
                readonly: value["readonly"].as_bool(),
                access_time: value["access_time"].as_str().map(str::to_owned),
                modify_time: value["modify_time"].as_str().map(str::to_owned),
            };
            !change.is_empty() && change.validate().is_ok()
        }
        _ => false,
    };
    if !valid {
        return Err(invalid("节点变更字段、类型或操作无效"));
    }
    Ok((
        matches!(kind, "create-directory" | "create-symlink"),
        matches!(
            kind,
            "create-directory" | "create-symlink" | "unlink" | "remove-directory"
        ),
    ))
}

fn fingerprint(request: &Value) -> String {
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"tape-rs/node-request/1")
            .chain_update(request.to_string().as_bytes())
            .finalize()
    )
}

/// Persist before submission. Retrying never changes the logical precondition.
#[derive(Clone, Debug)]
pub struct PreparedNodeMutation {
    value: Value,
}

impl PreparedNodeMutation {
    fn parse(value: Value) -> Result<Self> {
        if !fields(
            &value,
            &[
                "version",
                "library_serial",
                "namespace",
                "request",
                "fingerprint",
            ],
        ) || value["version"] != 1
            || value.to_string().len() > MAX_SAVED
        {
            return Err(invalid("节点请求文件字段、版本或长度无效"));
        }
        let library = value["library_serial"]
            .as_str()
            .ok_or_else(|| invalid("节点请求缺少带库"))?;
        Client::new(Vec::<String>::new()).with_library(library)?;
        uuid(&value, "namespace")?;
        let request = &value["request"];
        if !fields(
            request,
            &[
                "version",
                "library",
                "namespace",
                "pool",
                "operation",
                "request_epoch",
                "path",
                "parent",
                "parent_version",
                "node",
                "expected_version",
                "baseline_sha256",
                "time",
                "mutation",
            ],
        ) || request["version"] != 1
            || request["library"] != library
            || request["namespace"] != value["namespace"]
            || request.to_string().len() > MAX_REQUEST
            || value["fingerprint"] != fingerprint(request)
            || !request["request_epoch"].as_u64().is_some_and(|n| n > 0)
            || !request["parent_version"].as_u64().is_some_and(|n| n > 0)
            || !request["baseline_sha256"].as_array().is_some_and(|v| {
                v.len() == 32 && v.iter().all(|n| n.as_u64().is_some_and(|n| n <= 255))
            })
        {
            return Err(invalid("节点请求身份、作用域或摘要无效"));
        }
        for field in ["namespace", "pool", "operation", "parent", "node"] {
            uuid(request, field)?;
        }
        if request["parent"] == request["node"] {
            return Err(invalid("节点与父目录身份相同"));
        }
        validate_path(
            request["path"]
                .as_str()
                .ok_or_else(|| invalid("节点路径无效"))?,
        )?;
        let (creating, changes_parent) = mutation(&request["mutation"])?;
        if creating {
            let mut inode: [u8; 16] = Sha256::new()
                .chain_update(b"tape-rs/node-mutation-inode/1")
                .chain_update(uuid(request, "namespace")?.as_bytes())
                .chain_update(request["request_epoch"].as_u64().unwrap().to_be_bytes())
                .chain_update(uuid(request, "operation")?.as_bytes())
                .finalize()[..16]
                .try_into()
                .unwrap();
            inode[6] = (inode[6] & 15) | 0x80;
            inode[8] = (inode[8] & 63) | 0x80;
            if request["node"] != Uuid::from_bytes(inode).to_string() {
                return Err(invalid("创建节点 inode 与原 operation 不符"));
            }
        }
        if creating != request["expected_version"].is_null()
            || (!creating
                && !request["expected_version"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n < u64::MAX))
            || (changes_parent && request["parent_version"] == u64::MAX)
        {
            return Err(invalid("节点创建、原版本或父目录版本条件无效"));
        }
        let time = request["time"]
            .as_str()
            .ok_or_else(|| invalid("节点请求缺少时间"))?;
        crate::ltfs::index::MetadataChange {
            modify_time: Some(time.into()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid("节点请求时间无效"))?;
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
        format!("/v1/fdb/nodes/{}", self.library())
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
        let bad = || ClientError::Verification("节点回执与原请求或保留状态不符".into());
        let request = self.request();
        if !fields(
            &value,
            &[
                "version",
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
            let kind = request["mutation"]["kind"].as_str().unwrap();
            let (_, changes_parent) = mutation(&request["mutation"])?;
            let parent_version = request["parent_version"]
                .as_u64()
                .unwrap()
                .checked_add(u64::from(changes_parent))
                .ok_or_else(bad)?;
            let version = request["expected_version"]
                .as_u64()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(bad)?;
            if !fields(result, &["node", "version", "parent_version", "deleted"])
                || result["node"] != request["node"]
                || result["version"] != version
                || result["parent_version"] != parent_version
                || result["deleted"] != matches!(kind, "unlink" | "remove-directory")
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
            "节点请求结果暂不可查询；保留原请求目录，使用 fdb fs status/run 查询或重试".into(),
        ),
        other => other,
    }
}

impl FdbClient {
    /// Read the committed logical metadata; no tape or live owner is required.
    pub fn node_attributes(&self, library: &str, pool: Uuid, path: &str) -> Result<Option<Value>> {
        if pool.is_nil() {
            return Err(invalid("节点属性查询 pool 不能为 nil"));
        }
        let route = reading::read_route("attributes", library, path)?;
        let response = self.retry_request("GET", &format!("{route}&pool={pool}"), None, &[])?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        if response.body.len() > MAX_ATTRIBUTE_RESPONSE {
            return Err(ClientError::Verification("节点属性响应超过预算".into()));
        }
        let value = response.json();
        verify_attributes(&value, library, pool, path)?;
        Ok(Some(value))
    }
    /// Persist the original intent before read-only preparation. Retry with the saved directory.
    pub fn prepare_node_mutation(
        &self,
        library: &str,
        pool: Uuid,
        path: &str,
        condition: NodeCondition,
        change: Value,
        directory: &Path,
    ) -> Result<PreparedNodeMutation> {
        let mut saved = NodeJournal::create(library, pool, path, condition, change, directory)?;
        self.prepare_node_journal(&mut saved)
    }

    pub fn prepare_node_journal(&self, saved: &mut NodeJournal) -> Result<PreparedNodeMutation> {
        saved.durable()?;
        saved.terminal()?;
        if let Some(prepared) = &saved.prepared {
            return Ok(prepared.clone());
        }
        let input = saved.original.input();
        let namespace = saved.namespace().to_string();
        let answer = self.retry_request(
            "POST",
            &format!("/v1/fdb/nodes/{}/prepare", saved.library()),
            Some(input.to_string().as_bytes()),
            &[
                ("Content-Type", "application/json"),
                ("x-tapers-catalog-namespace", &namespace),
            ],
        )?;
        if answer.status != 200 {
            return Err(rejected(answer));
        }
        let prepared = PreparedNodeMutation::parse(answer.json())
            .map_err(|error| ClientError::Verification(format!("节点准备应答无效：{error}")))?;
        saved.original.verify(&prepared)?;
        saved.record_prepared(prepared.clone())?;
        Ok(prepared)
    }

    /// Read-only original status. A Draft returns None without an RPC.
    pub fn node_mutation_status_saved(&self, directory: &Path) -> Result<Option<Value>> {
        let saved = NodeJournal::open_shared(directory)?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(Some(terminal));
        }
        match saved.prepared() {
            Some(prepared) => self.node_mutation_status(prepared),
            None => Ok(None),
        }
    }

    pub fn node_mutation_status(&self, request: &PreparedNodeMutation) -> Result<Option<Value>> {
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
    pub fn run_node_journal(&self, saved: &mut NodeJournal) -> Result<Value> {
        saved.durable()?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(terminal);
        }
        let prepared = self.prepare_node_journal(saved)?;
        let terminal = self.run_node_request(&prepared)?;
        saved.record_terminal(&terminal)?;
        Ok(terminal)
    }

    pub fn run_node_mutation_saved(&self, path: &Path) -> Result<Value> {
        let mut saved = NodeJournal::open(path)?;
        let terminal = self.run_node_journal(&mut saved)?;
        if terminal["status"] != "completed" {
            return Err(ClientError::Rejected {
                status: 409,
                body: terminal.to_string(),
            });
        }
        Ok(terminal)
    }

    fn run_node_request(&self, request: &PreparedNodeMutation) -> Result<Value> {
        let deadline = Instant::now() + self.transport.retry_for;
        let mut status = self.node_mutation_status(request).map_err(uncertain)?;
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
                    "节点变更尚未确认；保留原请求目录，使用 fdb fs status/run 查询或重试".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
            status = self.node_mutation_status(request).map_err(uncertain)?;
        }
    }

    pub fn cancel_node_mutation_saved(&self, path: &Path) -> Result<Value> {
        let mut saved = NodeJournal::open(path)?;
        self.cancel_node_journal(&mut saved)
    }

    pub fn cancel_node_journal(&self, saved: &mut NodeJournal) -> Result<Value> {
        saved.durable()?;
        if let Some(terminal) = saved.terminal()? {
            return Ok(terminal);
        }
        let request = saved
            .prepared()
            .ok_or_else(|| invalid("原节点请求尚未准备或登记，不发送取消"))?;
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

pub(super) fn verify_attributes(
    value: &Value,
    library: &str,
    pool: Uuid,
    path: &str,
) -> Result<()> {
    let bad = || ClientError::Verification("节点属性应答作用域、身份或字段无效".into());
    if !fields(
        value,
        &[
            "schema_version",
            "library_serial",
            "namespace",
            "pool_id",
            "path",
            "id",
            "version",
            "kind",
            "attributes_digest",
            "metadata",
            "xattrs",
            "symlink_target",
        ],
    ) || value["schema_version"] != 1
        || value["library_serial"] != library
        || value["pool_id"] != pool.to_string()
        || value["path"] != path
        || !value["version"].as_u64().is_some_and(|n| n > 0)
        || !value["attributes_digest"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(bad());
    }
    uuid(value, "namespace")?;
    uuid(value, "id")?;
    let meta = &value["metadata"];
    if !fields(
        meta,
        &[
            "readonly",
            "creation_time",
            "change_time",
            "modify_time",
            "access_time",
            "backup_time",
            "file_uid",
        ],
    ) || !meta["readonly"].is_boolean()
        || !meta["file_uid"].as_u64().is_some_and(|n| n > 0)
        || [
            "creation_time",
            "change_time",
            "modify_time",
            "access_time",
            "backup_time",
        ]
        .iter()
        .any(|key| !meta[key].is_string())
    {
        return Err(bad());
    }
    // Preserve original opaque timestamps and xattr text, including unknown
    // imported values; strict new-mutation syntax must not rewrite old metadata.
    if !value["xattrs"].as_array().is_some_and(|items| {
        items.iter().all(|item| {
            fields(item, &["key", "value", "base64"])
                && item["key"].is_string()
                && item["value"].is_string()
                && item["base64"].is_boolean()
        })
    }) {
        return Err(bad());
    }
    match value["kind"].as_str() {
        Some("file" | "directory") if value["symlink_target"].is_null() => Ok(()),
        Some("symlink")
            if value["symlink_target"]
                .as_str()
                .is_some_and(|s| !s.is_empty()) =>
        {
            Ok(())
        }
        _ => Err(bad()),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attribute_reads_bind_scope_and_preserve_unknown_original_metadata() {
        let p = prepared();
        let pool = uuid(p.request(), "pool").unwrap();
        let original = json!({"schema_version":1,"library_serial":p.library(),"namespace":p.namespace(),
            "pool_id":pool.to_string(),"path":"/child","id":p.request()["node"],"version":9,"kind":"file",
            "attributes_digest":"7".repeat(64),"metadata":{"readonly":false,"file_uid":7,
                "creation_time":"","change_time":"unknown original time","modify_time":"","access_time":"","backup_time":""},
            "xattrs":[{"key":"user.opaque","value":"原样保留","base64":false}],"symlink_target":null});
        assert!(verify_attributes(&original, p.library(), pool, "/child").is_ok());
        assert!(verify_attributes(&original, p.library(), Uuid::new_v4(), "/child").is_err());
        assert!(verify_attributes(&original, p.library(), pool, "/other").is_err());
        for field in [
            "id",
            "version",
            "kind",
            "attributes_digest",
            "metadata",
            "xattrs",
            "symlink_target",
        ] {
            let mut bad = original.clone();
            match field {
                "id" => bad[field] = json!(Uuid::nil().to_string()),
                "version" => bad[field] = json!(0),
                "kind" => bad[field] = json!("physical-ee-alias"),
                "attributes_digest" => bad[field] = json!("G".repeat(64)),
                "metadata" => bad[field]["readonly"] = json!("false"),
                "xattrs" => bad[field][0]["base64"] = Value::Null,
                _ => bad[field] = json!(".LTFSEE_DATA/physical-data"),
            }
            assert!(
                verify_attributes(&bad, p.library(), pool, "/child").is_err(),
                "{field}"
            );
        }
        let mut native = original;
        native["kind"] = json!("symlink");
        native["symlink_target"] = json!(".LTFSEE_DATA/native-target");
        assert!(verify_attributes(&native, p.library(), pool, "/child").is_ok());
    }

    // Inert logical wire fixtures; none is an admission or media proof.
    pub(super) fn prepared() -> PreparedNodeMutation {
        let ns = Uuid::new_v4();
        let op = Uuid::new_v4();
        let mut node: [u8; 16] = Sha256::new()
            .chain_update(b"tape-rs/node-mutation-inode/1")
            .chain_update(ns.as_bytes())
            .chain_update(1u64.to_be_bytes())
            .chain_update(op.as_bytes())
            .finalize()[..16]
            .try_into()
            .unwrap();
        node[6] = (node[6] & 15) | 0x80;
        node[8] = (node[8] & 63) | 0x80;
        let request = json!({"version":1,"library":"NODE-CLIENT","namespace":ns.to_string(),"pool":Uuid::new_v4().to_string(),
            "operation":op.to_string(),"request_epoch":1,"path":"/child","parent":Uuid::new_v4().to_string(),
            "parent_version":7,"node":Uuid::from_bytes(node).to_string(),"expected_version":null,
            "baseline_sha256":vec![7u8;32],"time":"2026-11-01T12:00:00.000000000Z","mutation":{"kind":"create-directory"}});
        PreparedNodeMutation::parse(
            json!({"version":1,"library_serial":"NODE-CLIENT","namespace":ns.to_string(),
            "fingerprint":fingerprint(&request),"request":request}),
        )
        .unwrap()
    }

    pub(super) fn receipt(request: &PreparedNodeMutation) -> Value {
        json!({"version":1,"library_serial":request.library(),"namespace":request.namespace(),"pool":request.request()["pool"],
            "epoch":1,"operation":request.request()["operation"],"fingerprint":request.value["fingerprint"],"status":"completed",
            "accepted_owner_epoch":100,"created_at_millis":1000,"prepared_owner_epoch":100,"prepared_at_millis":1001,
            "completed_owner_epoch":101,"completed_at_millis":1002,"rejection_reason":null,
            "result":{"node":request.request()["node"],"version":1,"parent_version":8,"deleted":false}})
    }

    #[test]
    fn creation_rejects_a_forged_inode_even_with_a_matching_request_fingerprint() {
        let prepared = prepared();
        let mut value = prepared.value.clone();
        value["request"]["node"] = json!(Uuid::new_v4().to_string());
        value["fingerprint"] = json!(fingerprint(&value["request"]));
        assert!(PreparedNodeMutation::parse(value).is_err());
    }

    #[test]
    fn logical_requests_reject_unknown_or_missing_fields_and_bad_identity() {
        let prepared = prepared();
        PreparedNodeMutation::parse(prepared.value.clone()).unwrap();
        for field in [
            "namespace",
            "time",
            "expected_version",
            "node",
            "baseline_sha256",
        ] {
            let mut changed = prepared.value.clone();
            match field {
                "namespace" => changed[field] = json!(Uuid::new_v4().to_string()),
                "time" => changed["request"][field] = json!("not a time"),
                "expected_version" => {
                    changed["request"].as_object_mut().unwrap().remove(field);
                }
                "node" => changed["request"][field] = changed["request"]["parent"].clone(),
                _ => changed["request"][field] = json!(vec![0u8; 31]),
            }
            changed["fingerprint"] = json!(fingerprint(&changed["request"]));
            assert!(PreparedNodeMutation::parse(changed).is_err(), "{field}");
        }
        let mut extra = prepared.value;
        extra["extra"] = json!(true);
        assert!(PreparedNodeMutation::parse(extra).is_err());
    }

    #[test]
    fn receipts_preserve_original_identity_and_reject_invalid_state_or_outcome() {
        let p = prepared();
        let completed = receipt(&p);
        assert_eq!(p.verify(completed.clone()).unwrap(), completed);
        for field in [
            "namespace",
            "fingerprint",
            "result",
            "owner",
            "time",
            "prepared",
            "reason",
            "extra",
        ] {
            let mut bad = completed.clone();
            match field {
                "namespace" => bad[field] = json!(Uuid::new_v4().to_string()),
                "fingerprint" => bad[field] = json!("0".repeat(64)),
                "result" => bad[field]["node"] = json!(Uuid::new_v4().to_string()),
                "owner" => bad["completed_owner_epoch"] = json!(99),
                "time" => bad["completed_at_millis"] = json!(1000),
                "prepared" => {
                    bad["prepared_owner_epoch"] = Value::Null;
                    bad["prepared_at_millis"] = Value::Null;
                }
                "reason" => bad["rejection_reason"] = json!("cannot attach a rejection to success"),
                _ => bad[field] = json!(true),
            }
            assert!(p.verify(bad).is_err(), "{field}");
        }
        let mut queued = completed.clone();
        queued["status"] = json!("queued");
        for f in [
            "prepared_owner_epoch",
            "prepared_at_millis",
            "completed_owner_epoch",
            "completed_at_millis",
            "result",
        ] {
            queued[f] = Value::Null;
        }
        assert!(p.verify(queued.clone()).is_ok());
        let mut rejected = queued.clone();
        rejected["status"] = json!("rejected");
        rejected["completed_owner_epoch"] = json!(101);
        rejected["completed_at_millis"] = json!(1002);
        rejected["rejection_reason"] = json!("中".repeat(166) + "ab");
        assert!(p.verify(rejected.clone()).is_ok());
        for reason in [
            String::new(),
            " ".into(),
            "reason\n".into(),
            "中".repeat(167),
        ] {
            rejected["rejection_reason"] = json!(reason);
            assert!(p.verify(rejected.clone()).is_err());
        }
        queued["result"] = completed["result"].clone();
        assert!(p.verify(queued).is_err());
    }

    #[test]
    fn retained_deletion_and_metadata_versions_are_verified_without_current_stat() {
        let mut value = prepared().value;
        value["request"]["expected_version"] = json!(9);
        for (change, parent, deleted) in [
            (json!({"kind":"remove-directory"}), 8, true),
            (
                json!({"kind":"change-metadata","readonly":false,"access_time":null,"modify_time":null}),
                7,
                false,
            ),
        ] {
            value["request"]["mutation"] = change;
            value["fingerprint"] = json!(fingerprint(&value["request"]));
            let p = PreparedNodeMutation::parse(value.clone()).unwrap();
            let mut first = receipt(&p);
            first["result"]["version"] = json!(10);
            first["result"]["parent_version"] = json!(parent);
            first["result"]["deleted"] = json!(deleted);
            assert!(p.verify(first.clone()).is_ok());
            first["result"]["version"] = json!(9);
            assert!(p.verify(first).is_err());
        }
    }
}
