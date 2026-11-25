use super::*;
use serde_json::json;

impl FdbClient {
    /// 交还整座带库。保留 operation，使用它查询终态或重送同一请求。
    pub fn release_library(
        &self,
        library: &str,
        namespace: Uuid,
        operation: Uuid,
    ) -> Result<Value> {
        self.library_handoff_request(library, namespace, operation, Some(None))?
            .ok_or_else(|| ClientError::Verification("交还请求缺少回执".into()))
    }

    /// 外部应用停止使用设备后，引用当前交还回执重新加载整座带库。
    pub fn acquire_library(
        &self,
        library: &str,
        namespace: Uuid,
        operation: Uuid,
        release: Uuid,
    ) -> Result<Value> {
        if release.is_nil() || release == operation {
            return Err(invalid("加载须引用不同的有效交还回执 ID"));
        }
        self.library_handoff_request(library, namespace, operation, Some(Some(release)))?
            .ok_or_else(|| ClientError::Verification("加载请求缺少回执".into()))
    }

    pub fn library_handoff_status(
        &self,
        library: &str,
        namespace: Uuid,
        operation: Uuid,
    ) -> Result<Option<Value>> {
        self.library_handoff_request(library, namespace, operation, None)
    }

    fn library_handoff_request(
        &self,
        library: &str,
        namespace: Uuid,
        operation: Uuid,
        change: Option<Option<Uuid>>,
    ) -> Result<Option<Value>> {
        if namespace.is_nil() || operation.is_nil() {
            return Err(invalid("带库 namespace 和操作 ID 不能为零"));
        }
        let base = management::base(library)?;
        let path = format!("{base}/library/handoffs/{operation}");
        let body = change.map(|release| match release {
            None => json!({"action": "release"}).to_string(),
            Some(release) => json!({"action": "acquire", "release": release}).to_string(),
        });
        let response = self.retry_request(
            if body.is_some() { "PUT" } else { "GET" },
            &path,
            body.as_deref().map(str::as_bytes),
            &[
                ("x-tapers-catalog-namespace", &namespace.to_string()),
                ("Content-Type", "application/json"),
            ],
        )?;
        if response.status == 404 && change.is_none() {
            return Ok(None);
        }
        if !matches!(response.status, 200 | 202) {
            return Err(rejected(response));
        }
        let value = response.json();
        if value["library_serial"] != library
            || value["namespace"] != namespace.to_string()
            || value["operation"] != operation.to_string()
            || change.is_some_and(|release| value["release"] != json!(release))
            || !matches!(
                value["phase"].as_str(),
                Some("releasing" | "released" | "loading" | "blocked" | "online" | "failed")
            )
        {
            return Err(ClientError::Verification(
                "带库交还或加载回执与原请求不符".into(),
            ));
        }
        Ok(Some(value))
    }
}
