//! Stable management requests survive client and owner restarts.
use super::*;

#[derive(Clone, Debug)]
pub struct PreparedManagement {
    value: Value,
}

impl PreparedManagement {
    pub fn value(&self) -> &Value {
        &self.value
    }
    fn parse(value: Value) -> Result<Self> {
        let fields = [
            "version",
            "library",
            "namespace",
            "epoch",
            "operation",
            "change",
        ];
        if value["version"] != 1
            || value.to_string().len() > 4096
            || !value.as_object().is_some_and(|v| {
                v.len() == fields.len() && v.keys().all(|k| fields.contains(&k.as_str()))
            })
            || !value["epoch"].as_u64().is_some_and(|n| n > 0)
            || !value["change"].is_object()
            || !matches!(
                value["change"]["kind"].as_str(),
                Some(
                    "pool"
                        | "pool-delete"
                        | "drive"
                        | "tape"
                        | "media-import"
                        | "library-rescan"
                        | "media-assign"
                        | "media-unassign"
                        | "media-validate"
                        | "media-online"
                        | "media-move"
                )
            )
        {
            return Err(invalid("管理请求文件字段、版本或操作无效"));
        }
        if matches!(value["change"]["kind"].as_str(), Some("media-import")) {
            let change = &value["change"];
            let fields = ["kind", "pool", "barcode", "volume", "path", "merge_tapes"];
            if !change.as_object().is_some_and(|v| {
                v.len() == fields.len() && v.keys().all(|k| fields.contains(&k.as_str()))
            }) || !change["merge_tapes"].is_boolean()
            {
                return Err(invalid("导入请求必须固定 path 与 merge_tapes 等完整字段"));
            }
            let path = change["path"]
                .as_str()
                .ok_or_else(|| invalid("导入路径无效"))?;
            if path != "/" {
                validate_path(path)?;
            }
        }
        validate_device_change(&value["change"])?;
        let library = value["library"]
            .as_str()
            .ok_or_else(|| invalid("管理请求缺少带库"))?;
        Client::new(Vec::<String>::new()).with_library(library)?;
        for field in ["namespace", "operation"] {
            let id = reading::uuid_field(&value, field, false)?;
            if value[field] != id.to_string() {
                return Err(invalid("管理请求 UUID 必须规范化"));
            }
        }
        Ok(Self { value })
    }

    pub fn save_new(&self, path: &Path) -> Result<()> {
        save_value_new(&self.value, path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > 4097 {
            return Err(invalid("管理请求过大或不是普通文件"));
        }
        let mut bytes = Vec::new();
        (&mut file).take(4098).read_to_end(&mut bytes)?;
        if bytes.len() > 4097 {
            return Err(invalid("管理请求过大"));
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("管理请求 JSON 损坏"))?;
        if bytes.strip_suffix(b"\n").unwrap_or(&bytes) != value.to_string().as_bytes() {
            return Err(invalid("管理请求必须为规范 JSON，禁止重复字段"));
        }
        Self::parse(value)
    }

    fn library(&self) -> &str {
        self.value["library"].as_str().unwrap()
    }
    fn namespace(&self) -> &str {
        self.value["namespace"].as_str().unwrap()
    }
    fn base(&self) -> String {
        format!("/v1/fdb/control/{}", self.library())
    }
    fn route(&self) -> String {
        format!(
            "{}/requests/{}/{}",
            self.base(),
            self.value["epoch"],
            self.value["operation"].as_str().unwrap()
        )
    }
    fn verify_task(&self, value: Value) -> Result<Value> {
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(self.value.to_string().as_bytes()));
        if value["request"] != self.value
            || value["version"] != 1
            || value["fingerprint"] != fingerprint
            || value["library_serial"] != self.library()
            || value["namespace"] != self.namespace()
            || value["operation"] != self.value["operation"]
            || value["epoch"] != self.value["epoch"]
            || !value["created_at_millis"].as_u64().is_some_and(|n| n > 0)
        {
            return Err(ClientError::Verification("管理回执与保存的请求不符".into()));
        }
        let valid = match value["status"].as_str() {
            Some("queued") => {
                value["owner_epoch"] == 0
                    && value["result"].is_null()
                    && value["completed_at_millis"].is_null()
            }
            Some("running") => {
                matches!(
                    self.value["change"]["kind"].as_str(),
                    Some(
                        "media-import"
                            | "library-rescan"
                            | "media-assign"
                            | "media-unassign"
                            | "media-validate"
                            | "media-online"
                            | "media-move"
                    )
                ) && value["owner_epoch"].as_u64().is_some_and(|n| n > 0)
                    && value["result"].is_null()
                    && value["completed_at_millis"].is_null()
            }
            Some("succeeded" | "failed" | "cancelled") => {
                value["owner_epoch"].as_u64().is_some_and(|n| n > 0)
                    && value["result"].is_object()
                    && value["completed_at_millis"]
                        .as_u64()
                        .is_some_and(|n| Some(n) >= value["created_at_millis"].as_u64())
            }
            _ => false,
        };
        if !valid {
            return Err(ClientError::Verification("管理回执状态无效".into()));
        }
        Ok(value)
    }
}

fn validate_device_change(change: &Value) -> Result<()> {
    let Some(kind) = change["kind"].as_str() else {
        return Err(invalid("缺少操作类型"));
    };
    let fields: &[&str] = match kind {
        "library-rescan" => &["kind"],
        "media-assign" => &[
            "kind",
            "pool",
            "barcode",
            "volume",
            "confirm_format",
            "force_format",
        ],
        "media-unassign" | "media-validate" | "media-online" => {
            &["kind", "pool", "barcode", "volume", "revision"]
        }
        "media-move" => &[
            "kind",
            "pool",
            "barcode",
            "volume",
            "revision",
            "destination",
        ],
        _ => return Ok(()),
    };
    if !change
        .as_object()
        .is_some_and(|v| v.len() == fields.len() && v.keys().all(|k| fields.contains(&k.as_str())))
    {
        return Err(invalid("设备管理请求字段无效"));
    }
    if kind == "library-rescan" {
        return Ok(());
    }
    if !change["barcode"]
        .as_str()
        .is_some_and(valid_management_barcode)
    {
        return Err(invalid("设备管理条码无效"));
    }
    if kind == "media-move" && change["pool"].is_null() {
        if !change["volume"].is_null() || !change["revision"].is_null() {
            return Err(invalid("未分池介质的 pool、volume、revision 必须均为 null"));
        }
    } else {
        for field in ["pool", "volume"] {
            let id = reading::uuid_field(change, field, false)?;
            if change[field] != id.to_string() {
                return Err(invalid("设备管理 UUID 必须规范化"));
            }
        }
        if kind != "media-assign" && !change["revision"].as_u64().is_some_and(|v| v > 0) {
            return Err(invalid("设备管理请求须固定原归属修订号"));
        }
    }
    if kind == "media-assign"
        && (change["confirm_format"] != true || !change["force_format"].is_boolean())
    {
        return Err(invalid("初始化/格式化必须明确确认并固定 force_format"));
    }
    if kind == "media-move"
        && !matches!(change["destination"].as_str(), Some("homeslot" | "ieslot"))
    {
        return Err(invalid("移动目的仅支持 homeslot 或 ieslot"));
    }
    Ok(())
}

fn uncertain(error: ClientError) -> ClientError {
    match error {
        ClientError::NoLeader(_) | ClientError::Io(_) => ClientError::Indeterminate(
            "管理请求状态暂不可查询；保留原请求文件，查询或重试同一请求".into(),
        ),
        other => other,
    }
}

fn verify_policy(value: &Value, library: &str, kind: &str, target: &str) -> Result<()> {
    reading::check_scope(value, library)?;
    if value["kind"] != kind
        || value["target"] != target
        || value["policy"]["revision"].as_u64().is_none()
    {
        return Err(ClientError::Verification("管理资源应答无效".into()));
    }
    if kind == "drive"
        && (!value["policy"].as_object().is_some_and(|p| {
            p.len() == 3
                && p.keys()
                    .all(|key| ["revision", "enabled", "role"].contains(&key.as_str()))
        }) || !value["policy"]["enabled"].is_boolean()
            || !value["policy"]["role"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.chars().all(|c| "mrg".contains(c))))
    {
        return Err(ClientError::Verification("驱动器策略字段无效".into()));
    }
    if kind == "tape" {
        let policy = &value["policy"];
        if policy["library"] != library
            || policy["namespace"] != value["namespace"]
            || policy["barcode"] != target
            || !policy["revision"]
                .as_u64()
                .is_some_and(|revision| revision > 0)
            || !policy["online"].is_boolean()
            || !policy["blacklisted"].is_boolean()
        {
            return Err(ClientError::Verification("磁带策略身份或字段无效".into()));
        }
        for field in ["pool", "volume"] {
            if !policy[field].as_str().is_some_and(|text| {
                text.parse::<Uuid>()
                    .ok()
                    .is_some_and(|id| !id.is_nil() && id.to_string() == text)
            }) {
                return Err(ClientError::Verification("磁带策略池或卷身份无效".into()));
            }
        }
    }
    Ok(())
}

impl FdbClient {
    /// Read-only validation. Persist the returned identity before submitting it.
    pub fn prepare_management(
        &self,
        library: &str,
        namespace: Uuid,
        change: Value,
    ) -> Result<PreparedManagement> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        if change["kind"] == "tape" && change["patch"]["online"] == true {
            return Err(invalid("online=true 必须通过 tape online 的实带核验任务"));
        }
        if namespace.is_nil() {
            return Err(invalid("管理 namespace 不能为 nil"));
        }
        let base = format!("/v1/fdb/control/{library}");
        let response = self.retry_request("GET", &format!("{base}/window"), None, &[])?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let window = response.json();
        reading::check_scope(&window, library)?;
        let epoch = window["accepting"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("管理代次无效"))?;
        if window["namespace"] != namespace.to_string()
            || !window["first_retained"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= epoch)
        {
            return Err(ClientError::Verification(
                "管理窗口与 namespace 不符".into(),
            ));
        }
        let value = serde_json::json!({"version":1,"library":library,"namespace":namespace.to_string(),"epoch":epoch,"operation":Uuid::new_v4().to_string(),"change":change});
        let prepared = PreparedManagement::parse(value)?;
        let body = prepared.value.to_string();
        let response = self.retry_request(
            "POST",
            &format!("{base}/prepare"),
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let answer = response.json();
        reading::check_scope(&answer, library)?;
        if answer["namespace"] != namespace.to_string() || answer["request"] != prepared.value {
            return Err(ClientError::Verification("管理准备应答与原请求不符".into()));
        }
        Ok(prepared)
    }

    pub fn management_status(&self, request: &PreparedManagement) -> Result<Option<Value>> {
        let response = self.retry_request(
            "GET",
            &request.route(),
            None,
            &[("x-tapers-fdb-namespace", request.namespace())],
        )?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        request.verify_task(response.json()).map(Some)
    }

    fn submit_management(&self, request: &PreparedManagement) -> Result<Option<Value>> {
        let mut status = self.management_status(request).map_err(uncertain)?;
        if status.is_none() {
            let body = request.value.to_string();
            let change = &request.value()["change"];
            let route = match change["kind"].as_str() {
                Some("library-rescan") => format!("/admin/libraries/{}/rescan", request.library()),
                Some(
                    kind @ ("media-import" | "media-assign" | "media-unassign" | "media-validate"
                    | "media-online" | "media-move"),
                ) => {
                    let action = kind.strip_prefix("media-").unwrap();
                    let barcode = encode_query(
                        change["barcode"]
                            .as_str()
                            .filter(|b| valid_management_barcode(b))
                            .ok_or_else(|| invalid("缺少有效条码"))?,
                    );
                    if let Some(pool) = change["pool"].as_str() {
                        format!(
                            "/admin/libraries/{}/pools/{pool}/tapes/{barcode}/{action}",
                            request.library()
                        )
                    } else {
                        format!(
                            "/admin/libraries/{}/tapes/{barcode}/{action}",
                            request.library()
                        )
                    }
                }
                Some("tape") => {
                    let action = if change["patch"]["online"] == false {
                        "offline"
                    } else {
                        "set"
                    };
                    format!(
                        "/admin/libraries/{}/pools/{}/tapes/{}/{action}",
                        request.library(),
                        change["pool"].as_str().ok_or_else(|| invalid("缺少池"))?,
                        encode_query(
                            change["barcode"]
                                .as_str()
                                .ok_or_else(|| invalid("缺少条码"))?
                        )
                    )
                }
                _ => format!("{}/requests", request.base()),
            };
            match self.retry_request(
                "POST",
                &route,
                Some(body.as_bytes()),
                &[("Content-Type", "application/json")],
            ) {
                Ok(response) if matches!(response.status, 200 | 202) => {
                    status = Some(request.verify_task(response.json())?)
                }
                Ok(response) => return Err(rejected(response)),
                Err(_) => {}
            }
        }
        Ok(status)
    }

    /// Admit a saved request without waiting for device work to finish.
    pub fn submit_management_saved(&self, path: &Path) -> Result<Value> {
        let request = PreparedManagement::load(path)?;
        self.submit_management(&request)?.ok_or_else(|| {
            ClientError::Indeterminate("管理请求受理结果未定；保留原请求文件查询".into())
        })
    }

    /// Retrying always uses the saved identity, including after a lost response.
    pub fn run_management_saved(&self, path: &Path) -> Result<Value> {
        let request = PreparedManagement::load(path)?;
        if matches!(
            request.value()["change"]["kind"].as_str(),
            Some("media-import")
        ) {
            return Err(invalid(
                "磁带导入是异步任务；使用 submit_management_saved 提交并查询任务状态",
            ));
        }
        if matches!(
            request.value()["change"]["kind"].as_str(),
            Some(
                "library-rescan"
                    | "media-assign"
                    | "media-unassign"
                    | "media-validate"
                    | "media-online"
                    | "media-move"
            )
        ) {
            return Err(invalid(
                "设备管理操作是异步任务；使用 submit_management_saved 提交并查询任务状态",
            ));
        }
        let deadline = Instant::now() + self.transport.retry_for;
        let mut status = self.submit_management(&request)?;
        loop {
            if let Some(value) = status {
                match value["status"].as_str() {
                    Some("succeeded") => return Ok(value),
                    Some("failed" | "cancelled") => {
                        return Err(ClientError::Rejected {
                            status: 409,
                            body: value.to_string(),
                        });
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(ClientError::Indeterminate(
                    "管理请求尚未确认；保留原请求文件，以 control status/run 查询或重试".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
            status = self.management_status(&request).map_err(uncertain)?;
        }
    }

    pub fn cancel_management_saved(&self, path: &Path) -> Result<Value> {
        let request = PreparedManagement::load(path)?;
        let response = self.retry_request(
            "DELETE",
            &request.route(),
            None,
            &[("x-tapers-fdb-namespace", request.namespace())],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        request.verify_task(response.json())
    }

    pub fn management_policy(&self, library: &str, kind: &str, target: &str) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        if !matches!(kind, "drive" | "tape") {
            return Err(invalid("管理资源 kind 无效"));
        }
        let response = self.retry_request(
            "GET",
            &format!(
                "/v1/fdb/control/{library}/policy?kind={kind}&target={}",
                URL_SAFE_NO_PAD.encode(target)
            ),
            None,
            &[],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        verify_policy(&value, library, kind, target)?;
        Ok(value)
    }

    pub fn list_management(
        &self,
        library: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        if !(1..=100).contains(&limit)
            || after.is_some_and(|v| {
                v.len() > 2048
                    || !v
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
        {
            return Err(invalid("管理分页参数无效"));
        }
        let kind = "requests";
        let route = format!(
            "/v1/fdb/control/{library}/{kind}?limit={limit}{}",
            after.map(|s| format!("&after={s}")).unwrap_or_default()
        );
        let response = self.retry_request("GET", &route, None, &[])?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        reading::check_scope(&value, library)?;
        if value["limit"] != limit
            || !value["items"].as_array().is_some_and(|v| v.len() <= limit)
            || !(value["next"].is_null() || value["next"].as_str().is_some_and(|s| s.len() <= 2048))
        {
            return Err(ClientError::Verification("管理分页应答无效".into()));
        }
        for item in value["items"].as_array().unwrap() {
            let request = PreparedManagement::parse(item["request"].clone())?;
            if request.library() != library || item["namespace"] != value["namespace"] {
                return Err(ClientError::Verification("任务列表作用域不符".into()));
            }
            request.verify_task(item.clone())?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn import_request_requires_explicit_bound_path_and_merge_policy() {
        let value = serde_json::json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":Uuid::new_v4().to_string(),"barcode":"TAPE00L8","volume":null,"path":"/IMPORT","merge_tapes":false}});
        assert!(PreparedManagement::parse(value.clone()).is_ok());
        let mut root = value.clone();
        root["change"]["path"] = json!("/");
        assert!(PreparedManagement::parse(root).is_ok());
        for field in ["path", "merge_tapes"] {
            let mut missing = value.clone();
            missing["change"].as_object_mut().unwrap().remove(field);
            assert!(PreparedManagement::parse(missing).is_err());
        }
        for bad in [json!(null), json!(1), json!("true")] {
            let mut changed = value.clone();
            changed["change"]["merge_tapes"] = bad;
            assert!(PreparedManagement::parse(changed).is_err());
        }
        for bad in ["relative", "/trailing/", "/../other", "//double"] {
            let mut changed = value.clone();
            changed["change"]["path"] = json!(bad);
            assert!(PreparedManagement::parse(changed).is_err());
        }
    }

    #[test]
    fn media_import_cannot_enter_the_synchronous_management_wait_loop() {
        let value = serde_json::json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":Uuid::new_v4().to_string(),"barcode":"TAPE00L8","volume":null,"path":"/IMPORT","merge_tapes":false}});
        let root = std::env::temp_dir().join(format!("fdb-import-async-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("request.json");
        PreparedManagement::parse(value)
            .unwrap()
            .save_new(&path)
            .unwrap();
        let client = FdbClient::new(["127.0.0.1:1"]).unwrap();
        assert_eq!(
            client.run_management_saved(&path).unwrap_err().to_string(),
            "I/O: 磁带导入是异步任务；使用 submit_management_saved 提交并查询任务状态"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_requests_accept_the_backend_barcode_and_revision_boundaries() {
        for kind in ["media-unassign", "media-validate", "media-move"] {
            let mut change = json!({"kind":kind,"pool":Uuid::new_v4().to_string(),"barcode":"DEVICE0L8?#%","volume":Uuid::new_v4().to_string(),"revision":u64::MAX});
            if kind == "media-move" {
                change["destination"] = json!("homeslot");
            }
            assert!(validate_device_change(&change).is_ok());
            for barcode in ["", "bad/name", "has space", "中文"] {
                let mut bad = change.clone();
                bad["barcode"] = json!(barcode);
                assert!(validate_device_change(&bad).is_err());
            }
            change["revision"] = json!(0);
            assert!(validate_device_change(&change).is_err());
        }
        assert_eq!(encode_query("DEVICE0L8?#%"), "DEVICE0L8%3F%23%25");
    }

    #[test]
    fn tape_policy_reply_rejects_cross_scope_and_untyped_flags() {
        let ns = Uuid::new_v4().to_string();
        let value = serde_json::json!({"library_serial":"LIB_A","namespace":ns,"kind":"tape","target":"TAPE001L8","policy":{"library":"LIB_A","namespace":ns,"barcode":"TAPE001L8","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"revision":1,"online":true,"blacklisted":false}});
        assert!(verify_policy(&value, "LIB_A", "tape", "TAPE001L8").is_ok());
        for (field, bad) in [
            ("library", serde_json::json!("LIB_B")),
            ("namespace", serde_json::json!(Uuid::new_v4().to_string())),
            ("barcode", serde_json::json!("OTHERL8")),
            ("revision", serde_json::json!(0)),
            ("online", serde_json::json!("true")),
            ("blacklisted", Value::Null),
            ("volume", serde_json::json!(Uuid::nil().to_string())),
            ("pool", serde_json::json!(9)),
        ] {
            let mut changed = value.clone();
            changed["policy"][field] = bad;
            assert!(
                verify_policy(&changed, "LIB_A", "tape", "TAPE001L8").is_err(),
                "{field}"
            );
        }
        let mut changed = value.clone();
        changed["namespace"] = serde_json::json!(Uuid::new_v4().to_string());
        assert!(verify_policy(&changed, "LIB_A", "tape", "TAPE001L8").is_err());
        let drive = serde_json::json!({"library_serial":"LIB_A","namespace":ns,"kind":"drive","target":"DRV","policy":{"revision":0,"enabled":true,"role":"mrg"}});
        assert!(verify_policy(&drive, "LIB_A", "drive", "DRV").is_ok());
    }

    #[test]
    fn saved_management_cannot_be_replaced_and_receipts_bind_the_exact_request() {
        let value = serde_json::json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"drive","serial":"DRV","revision":0,"patch":{"role":"mrg"}}});
        let request = PreparedManagement::parse(value).unwrap();
        let root = std::env::temp_dir().join(format!("fdb-management-client-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("request.json");
        request.save_new(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            PreparedManagement::load(&path).unwrap().value,
            request.value
        );
        assert!(request.save_new(&path).is_err());
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(PreparedManagement::load(&link).is_err());
        assert!(request.save_new(&link).is_err());
        fs::write(&path, b"{\"version\":1,\"version\":1}").unwrap();
        assert!(PreparedManagement::load(&path).is_err());
        use sha2::{Digest, Sha256};
        let receipt = serde_json::json!({"version":1,"library_serial":"SIM","namespace":request.namespace(),"operation":request.value["operation"],"epoch":1,"request":request.value,"fingerprint":format!("{:x}",Sha256::digest(request.value.to_string().as_bytes())),"status":"queued","created_at_millis":1,"completed_at_millis":null,"owner_epoch":0,"result":null});
        request.verify_task(receipt.clone()).unwrap();
        let mut forged = receipt.clone();
        forged["request"]["change"]["patch"]["role"] = serde_json::json!("r");
        assert!(request.verify_task(forged).is_err());
        let mut forged = receipt;
        forged["status"] = serde_json::json!("succeeded");
        assert!(request.verify_task(forged).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
