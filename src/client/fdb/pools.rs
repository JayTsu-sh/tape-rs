//! Library-scoped pool metadata. Only fixed, durably checkpointed identities are
//! submitted. The checkpoint is not a task database; FDB owns all task states.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const NS: &str = "x-tapers-catalog-namespace";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum PoolIntent {
    Create {
        name: String,
        file_limit: u64,
    },
    Set {
        selector: String,
        attribute: String,
        value: String,
    },
    Delete {
        selector: String,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolRequest {
    version: u8,
    library: String,
    namespace: String,
    input: PoolIntent,
    request: Value,
    fingerprint: String,
}
fn uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or_else(|| invalid("标识须为规范非零UUID"))
}
fn name(text: &str) -> bool {
    !text.is_empty() && text.len() <= 255 && !text.contains(['/', '\0'])
}
fn base(library: &str) -> Result<String> {
    Client::new(Vec::<String>::new()).with_library(library)?;
    Ok(format!("/libraries/{}/admin", encode_query(library)))
}
fn patch(attribute: &str, value: &str) -> Result<Value> {
    let value = match attribute {
        "name" if name(value) => json!(value),
        "media_restriction"
            if matches!(
                value,
                "none"
                    | "L5"
                    | "L6"
                    | "L7"
                    | "L8"
                    | "L9"
                    | "M8"
                    | "JB"
                    | "JC"
                    | "JD"
                    | "JE"
                    | "JY"
                    | "JZ"
                    | "JV"
                    | "JK"
                    | "JL"
                    | "JM"
                    | "JF"
            ) =>
        {
            json!(value)
        }
        "file_limit" => json!(
            value
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("file_limit须为正整数"))?
        ),
        "mount_limit" | "low_space_warning_threshold" => json!(
            value
                .parse::<u16>()
                .ok()
                .filter(|n| *n <= 32767)
                .ok_or_else(|| invalid("策略阈值须在0..=32767"))?
        ),
        "low_space_warning_enabled" | "no_space_warning_enabled" => json!(
            value
                .parse::<bool>()
                .map_err(|_| invalid("策略开关须为true或false"))?
        ),
        _ => return Err(invalid("池属性或属性值无效")),
    };
    Ok(json!({attribute:value}))
}
fn defaults() -> Value {
    json!({"media_restriction":"none","mount_limit":0,"low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true})
}
fn parsed(response: Response, statuses: &[u16]) -> Result<Value> {
    if !statuses.contains(&response.status) {
        return Err(rejected(response));
    }
    serde_json::from_slice(&response.body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| ClientError::Verification("池管理响应不是JSON对象".into()))
}
fn scope(value: &Value, library: &str, namespace: &str) -> Result<()> {
    if value["library_serial"] != library || value["namespace"] != namespace {
        return Err(ClientError::Verification(
            "管理响应的带库或命名空间不符".into(),
        ));
    }
    Ok(())
}
fn unknown(checkpoint: &Path, error: ClientError) -> ClientError {
    ClientError::Indeterminate(format!(
        "{}；保留请求文件{}，按原身份恢复",
        error,
        checkpoint.display()
    ))
}
impl PoolRequest {
    fn digest(&self) -> String {
        format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&json!({"version":self.version,"library":self.library,"namespace":self.namespace,"input":self.input,"request":self.request})).expect("JSON serializes")))
    }
    fn validate(&self) -> Result<()> {
        base(&self.library)?;
        uuid(&self.namespace)?;
        if self.version != 1 || self.fingerprint != self.digest() {
            return Err(invalid("池请求文件版本或校验不符"));
        }
        let expected = match &self.input {
            PoolIntent::Create {
                name: n,
                file_limit,
            } => {
                if !name(n) || *file_limit == 0 {
                    return Err(invalid("池名称或file_limit无效"));
                }
                uuid(
                    self.request["id"]
                        .as_str()
                        .ok_or_else(|| invalid("池UUID缺失"))?,
                )?;
                if !self.request["created_at_millis"]
                    .as_u64()
                    .is_some_and(|n| (1..=253402300799999).contains(&n))
                {
                    return Err(invalid("池创建时间无效"));
                }
                json!({"version":1,"id":self.request["id"],"name":n,"file_limit":file_limit,"created_at_millis":self.request["created_at_millis"],"policy":defaults()})
            }
            PoolIntent::Set {
                selector,
                attribute,
                value,
            } => {
                if !name(selector) {
                    return Err(invalid("池选择器无效"));
                }
                self.management_body(Some(patch(attribute, value)?))?
            }
            PoolIntent::Delete { selector } => {
                if !name(selector) {
                    return Err(invalid("池选择器无效"));
                }
                self.management_body(None)?
            }
        };
        if expected != self.request {
            return Err(invalid("请求正文与原始参数或身份不符"));
        }
        Ok(())
    }
    fn management_body(&self, patch: Option<Value>) -> Result<Value> {
        uuid(
            self.request["operation"]
                .as_str()
                .ok_or_else(|| invalid("任务UUID缺失"))?,
        )?;
        uuid(
            self.request["change"]["id"]
                .as_str()
                .ok_or_else(|| invalid("池UUID缺失"))?,
        )?;
        if !self.request["epoch"].as_u64().is_some_and(|v| v > 0)
            || !self.request["change"]["revision"]
                .as_u64()
                .is_some_and(|v| v > 0 && v < u64::MAX)
        {
            return Err(invalid("任务代次或修订号无效"));
        }
        let mut change = json!({"kind":if patch.is_some(){"pool"}else{"pool-delete"},"id":self.request["change"]["id"],"revision":self.request["change"]["revision"]});
        if let Some(patch) = patch {
            change["patch"] = patch;
        }
        Ok(
            json!({"version":1,"library":self.library,"namespace":self.namespace,"epoch":self.request["epoch"],"operation":self.request["operation"],"change":change}),
        )
    }
    pub fn is_library(&self, library: &str) -> bool {
        self.library == library
    }
    pub fn matches(&self, library: &str, input: &PoolIntent) -> bool {
        self.library == library && &self.input == input
    }
    /// Never overwrite a checkpoint, including a partial checkpoint after an I/O failure.
    pub fn save_new(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).expect("JSON serializes");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("请求检查点必须是普通文件"));
        }
        let mut bytes = Vec::new();
        (&mut file).take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(invalid("请求检查点超过16KiB"));
        }
        let request: Self =
            serde_json::from_slice(&bytes).map_err(|_| invalid("请求检查点损坏"))?;
        if serde_json::to_vec(&request).expect("JSON serializes") != bytes {
            return Err(invalid("请求检查点编码不是原始规范格式"));
        }
        request.validate()?;
        Ok(request)
    }
}
impl FdbClient {
    pub fn pool_context(&self, library: &str) -> Result<Value> {
        let value = parsed(
            self.retry_request("GET", &format!("{}/context", base(library)?), None, &[])?,
            &[200],
        )?;
        let ns = value["namespace"]
            .as_str()
            .ok_or_else(|| invalid("目录命名空间缺失"))?;
        uuid(ns)?;
        scope(&value, library, ns)?;
        if value["version"] != 1 || !value["window"]["accepting"].as_u64().is_some_and(|v| v > 0) {
            return Err(ClientError::Verification("管理代次无效".into()));
        }
        Ok(value)
    }
    pub fn pool_page(&self, library: &str, namespace: &str, after: Option<&str>) -> Result<Value> {
        self.metadata_page(library, namespace, "pools", after)
    }
    pub fn management_page(
        &self,
        library: &str,
        namespace: &str,
        after: Option<&str>,
    ) -> Result<Value> {
        self.metadata_page(library, namespace, "tasks", after)
    }
    fn metadata_page(
        &self,
        library: &str,
        namespace: &str,
        kind: &str,
        after: Option<&str>,
    ) -> Result<Value> {
        uuid(namespace)?;
        let route = format!(
            "{}/{kind}?limit=100{}",
            base(library)?,
            after
                .map(|s| format!("&after={}", encode_query(s)))
                .unwrap_or_default()
        );
        let value = parsed(
            self.retry_request("GET", &route, None, &[(NS, namespace)])?,
            &[200],
        )?;
        scope(&value, library, namespace)?;
        if !value["items"].is_array()
            || !(value["next"].is_null() || value["next"].as_str().is_some_and(|s| !s.is_empty()))
            || value.get("next").is_none()
        {
            return Err(ClientError::Verification("管理分页响应无效".into()));
        }
        Ok(value)
    }
    pub fn pool(&self, library: &str, namespace: &str, selector: &str) -> Result<Value> {
        uuid(namespace)?;
        if !name(selector) {
            return Err(invalid("池选择器无效"));
        }
        let value = parsed(
            self.retry_request(
                "GET",
                &format!("{}/pools/{}", base(library)?, encode_query(selector)),
                None,
                &[(NS, namespace)],
            )?,
            &[200],
        )?;
        scope(&value, library, namespace)?;
        check_pool(&value["pool"], library)?;
        Ok(value)
    }
    pub fn prepare_pool(&self, library: &str, input: PoolIntent) -> Result<PoolRequest> {
        // Validate command parameters before contacting the server.
        match &input {
            PoolIntent::Create {
                name: n,
                file_limit,
            } if name(n) && *file_limit > 0 => {}
            PoolIntent::Set {
                selector,
                attribute,
                value,
            } if name(selector) => {
                patch(attribute, value)?;
            }
            PoolIntent::Delete { selector } if name(selector) => {}
            _ => return Err(invalid("池管理参数无效")),
        }
        let context = self.pool_context(library)?;
        let namespace = context["namespace"]
            .as_str()
            .expect("validated namespace")
            .to_owned();
        let request = match &input {
            PoolIntent::Create { name, file_limit } => {
                let time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| invalid("系统时间无效"))?
                    .as_millis();
                json!({"version":1,"id":Uuid::new_v4().to_string(),"name":name,"file_limit":file_limit,"created_at_millis":time as u64,"policy":defaults()})
            }
            PoolIntent::Set { selector, .. } | PoolIntent::Delete { selector } => {
                let pool = self.pool(library, &namespace, selector)?;
                let mut change = json!({"kind":"pool-delete","id":pool["pool"]["id"],"revision":pool["pool"]["revision"]});
                if let PoolIntent::Set {
                    attribute, value, ..
                } = &input
                {
                    change["kind"] = json!("pool");
                    change["patch"] = patch(attribute, value)?;
                }
                json!({"version":1,"library":library,"namespace":namespace,"epoch":context["window"]["accepting"],"operation":Uuid::new_v4().to_string(),"change":change})
            }
        };
        let mut prepared = PoolRequest {
            version: 1,
            library: library.into(),
            namespace,
            input,
            request,
            fingerprint: String::new(),
        };
        prepared.fingerprint = prepared.digest();
        prepared.validate()?;
        Ok(prepared)
    }
    /// Requires the exact checkpoint saved before the first mutation request.
    pub fn submit_pool(&self, checkpoint: &Path) -> Result<Value> {
        let p = PoolRequest::load(checkpoint)?;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(checkpoint)?
            .sync_all()?;
        std::fs::File::open(
            checkpoint
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        let (method, route, statuses) = match p.input {
            PoolIntent::Create { .. } => (
                "POST",
                format!("{}/pools", base(&p.library)?),
                vec![200, 201],
            ),
            PoolIntent::Set { .. } => (
                "PATCH",
                format!(
                    "{}/pools/{}",
                    base(&p.library)?,
                    p.request["change"]["id"].as_str().expect("validated id")
                ),
                vec![202],
            ),
            PoolIntent::Delete { .. } => (
                "DELETE",
                format!(
                    "{}/pools/{}",
                    base(&p.library)?,
                    p.request["change"]["id"].as_str().expect("validated id")
                ),
                vec![202],
            ),
        };
        let bytes = serde_json::to_vec(&p.request).expect("JSON serializes");
        let response = self.retry_request(
            method,
            &route,
            Some(&bytes),
            &[(NS, &p.namespace), ("content-type", "application/json")],
        );
        let response = response.map_err(|e| unknown(checkpoint, e))?;
        if !statuses.contains(&response.status) {
            return Err(rejected(response));
        }
        let result = (|| {
            let value = parsed(response, &statuses)?;
            if matches!(p.input, PoolIntent::Create { .. }) {
                scope(&value, &p.library, &p.namespace)?;
                check_pool(&value["pool"], &p.library)?;
                if value["pool"]["id"] != p.request["id"] {
                    return Err(ClientError::Verification("创建回执UUID不符".into()));
                }
            } else {
                check_task(&value, &p)?;
            }
            Ok(value)
        })();
        result.map_err(|e| unknown(checkpoint, e))
    }

    pub fn pool_request_status(&self, checkpoint: &Path, cancel: bool) -> Result<Value> {
        let p = PoolRequest::load(checkpoint)?;
        if matches!(p.input, PoolIntent::Create { .. }) {
            if cancel {
                return Err(invalid("池创建是原子事务，没有可取消的异步任务"));
            }
            return self.pool(
                &p.library,
                &p.namespace,
                p.request["id"].as_str().expect("validated id"),
            );
        }
        let route = format!(
            "{}/tasks/{}/{}{}",
            base(&p.library)?,
            p.request["epoch"],
            p.request["operation"]
                .as_str()
                .expect("validated operation"),
            if cancel { "/cancel" } else { "" }
        );
        let result = (|| {
            let value = parsed(
                self.retry_request(
                    if cancel { "POST" } else { "GET" },
                    &route,
                    None,
                    &[(NS, &p.namespace)],
                )?,
                &[200],
            )?;
            check_task(&value, &p)?;
            Ok(value)
        })();
        if cancel {
            result.map_err(|error| match error {
                ClientError::Rejected { .. } => error,
                _ => unknown(checkpoint, error),
            })
        } else {
            result
        }
    }
}
fn check_pool(p: &Value, library: &str) -> Result<()> {
    uuid(p["id"].as_str().ok_or_else(|| invalid("池回执缺少UUID"))?)?;
    if p["library_serial"] != library
        || !p["name"].as_str().is_some_and(name)
        || !p["revision"].as_u64().is_some_and(|n| n > 0)
        || !p["file_limit"].as_u64().is_some_and(|n| n > 0)
    {
        return Err(ClientError::Verification("池回执身份或修订号无效".into()));
    }
    Ok(())
}
fn check_task(task: &Value, p: &PoolRequest) -> Result<()> {
    scope(task, &p.library, &p.namespace)?;
    if task["epoch"] != p.request["epoch"]
        || task["operation"] != p.request["operation"]
        || task["request"] != p.request
        || !matches!(
            task["status"].as_str(),
            Some("queued" | "running" | "succeeded" | "failed" | "cancelled")
        )
    {
        return Err(ClientError::Indeterminate(
            "任务回执与原请求不符，请保留请求文件".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepared() -> PoolRequest {
        let mut p = PoolRequest {
            version: 1,
            library: "LIB 中文".into(),
            namespace: Uuid::new_v4().to_string(),
            input: PoolIntent::Create {
                name: "归档".into(),
                file_limit: 100,
            },
            request: json!({"version":1,"id":Uuid::new_v4().to_string(),"name":"归档","file_limit":100,"created_at_millis":1_793_405_000_123u64,"policy":defaults()}),
            fingerprint: String::new(),
        };
        p.fingerprint = p.digest();
        p
    }
    fn dir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tapers-pool-client-{}", Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn checkpoint_survives_restart_without_overwrite_or_partial_file_repair() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir();
        let path = dir.join("request.json");
        let p = prepared();
        p.save_new(&path).unwrap();
        let loaded = PoolRequest::load(&path).unwrap();
        assert_eq!(loaded.request, p.request);
        assert!(loaded.matches(&p.library, &p.input));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(prepared().save_new(&path).is_err());
        std::fs::write(&path, b"{\"version\":1").unwrap();
        assert!(PoolRequest::load(&path).is_err());
        assert!(p.save_new(&path).is_err());
        std::fs::write(&path, vec![b' '; 16385]).unwrap();
        assert!(PoolRequest::load(&path).is_err());
        std::os::unix::fs::symlink(&path, dir.join("symlink")).unwrap();
        assert!(PoolRequest::load(&dir.join("symlink")).is_err());
        assert!(PoolRequest::load(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn modified_checkpoint_parameters_and_malformed_receipts_cannot_manufacture_success() {
        let mut p = prepared();
        p.request["name"] = json!("other");
        assert!(p.validate().is_err());
        p.fingerprint = p.digest();
        assert!(p.validate().is_err());
        assert!(
            parsed(
                Response {
                    status: 200,
                    body: b"null".to_vec()
                },
                &[200]
            )
            .is_err()
        );
        assert!(
            parsed(
                Response {
                    status: 404,
                    body: b"{}".to_vec()
                },
                &[200]
            )
            .is_err()
        );
        assert!(patch("media_restriction", "L8").is_ok());
        assert!(patch("media_restriction", "lto8").is_err());
        assert!(patch("nodegroup", "all").is_err());
    }
    #[test]
    fn mutation_retries_keep_saved_identity_and_namespace_across_endpoints() {
        use super::super::tests::{response, server};
        let p = prepared();
        let dir = dir();
        let path = dir.join("request");
        p.save_new(&path).unwrap();
        let pool = json!({"id":p.request["id"],"name":"归档","library_serial":p.library,"revision":1,"file_limit":100});
        let body =
            json!({"library_serial":p.library,"namespace":p.namespace,"created":true,"pool":pool})
                .to_string();
        let (a, ta) = server(response(503, "{}"), Duration::ZERO);
        let (b, tb) = server(response(201, &body), Duration::ZERO);
        let result = FdbClient::new([a, b])
            .unwrap()
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(1))
            .submit_pool(&path)
            .unwrap();
        assert_eq!(result["pool"]["id"], p.request["id"]);
        let a = String::from_utf8(ta.join().unwrap()).unwrap();
        let b = String::from_utf8(tb.join().unwrap()).unwrap();
        assert_eq!(
            a.split_once("\r\n\r\n").unwrap().1,
            b.split_once("\r\n\r\n").unwrap().1
        );
        assert!(a.starts_with("POST /libraries/LIB%20%E4%B8%AD%E6%96%87/admin/pools HTTP/1.1"));
        assert!(a.contains(&format!("x-tapers-catalog-namespace: {}", p.namespace)));
        assert_eq!(PoolRequest::load(&path).unwrap().request, p.request);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn malformed_success_receipts_are_unknown_without_changing_checkpoint() {
        use super::super::tests::{response, server};
        let p = prepared();
        let dir = dir();
        let path = dir.join("request");
        p.save_new(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        for body in [
            "null".to_owned(),
            "bad JSON".to_owned(),
            json!({"namespace":Uuid::new_v4().to_string(),"library_serial":p.library,"pool":{}})
                .to_string(),
        ] {
            let (address, worker) = server(response(201, &body), Duration::ZERO);
            let error = FdbClient::new([address])
                .unwrap()
                .submit_pool(&path)
                .unwrap_err();
            assert!(matches!(error, ClientError::Indeterminate(_)), "{error}");
            let raw = String::from_utf8(worker.join().unwrap()).unwrap();
            assert!(raw.ends_with(&p.request.to_string()));
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cancellation_with_a_lost_or_invalid_receipt_preserves_original_task_identity() {
        use super::super::tests::{response, server};
        let mut p = prepared();
        p.input = PoolIntent::Set {
            selector: "归档".into(),
            attribute: "file_limit".into(),
            value: "101".into(),
        };
        p.request = json!({"version":1,"library":p.library,"namespace":p.namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"file_limit":101}}});
        p.fingerprint = p.digest();
        let dir = dir();
        let path = dir.join("request");
        p.save_new(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let (address, worker) = server(response(200, "null"), Duration::ZERO);
        let result = FdbClient::new([address])
            .unwrap()
            .pool_request_status(&path, true);
        assert!(matches!(result, Err(ClientError::Indeterminate(_))));
        let raw = String::from_utf8(worker.join().unwrap()).unwrap();
        assert!(raw.starts_with(&format!(
            "POST /libraries/LIB%20%E4%B8%AD%E6%96%87/admin/tasks/1/{}/cancel",
            p.request["operation"].as_str().unwrap()
        )));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        let result = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::from_millis(30), Duration::from_millis(20))
            .pool_request_status(&path, true);
        assert!(matches!(result, Err(ClientError::Indeterminate(_))));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unavailable_dependency_preserves_original_checkpoint_and_reports_unknown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        let p = prepared();
        let dir = dir();
        let path = dir.join("request");
        p.save_new(&path).unwrap();
        let result = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::from_millis(30), Duration::from_millis(20))
            .submit_pool(&path);
        assert!(matches!(result, Err(ClientError::Indeterminate(_))));
        assert_eq!(PoolRequest::load(&path).unwrap().request, p.request);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
