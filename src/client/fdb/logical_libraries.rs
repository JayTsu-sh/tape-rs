use super::*;
use crate::logical_library::{
    ChangeLogicalLibrary, CreateLogicalLibrary, MAX_REQUEST_BYTES, valid_name,
};

const NAMESPACE: &str = "x-tapers-catalog-namespace";

fn base(library: &str, namespace: Uuid) -> Result<String> {
    if namespace.is_nil() {
        return Err(invalid("逻辑带库操作需要非零 namespace UUID"));
    }
    Ok(format!("{}/logical-libraries", management::base(library)?))
}

fn verify_scope(value: &Value, library: &str, namespace: Uuid) -> Result<()> {
    reading::check_scope(value, library)?;
    if value["version"] != 1 || value["namespace"] != namespace.to_string() {
        return Err(ClientError::Verification(
            "逻辑带库应答版本或 namespace 无效".into(),
        ));
    }
    Ok(())
}

fn verify_definition(value: &Value) -> Result<CreateLogicalLibrary> {
    let bad = || ClientError::Verification("逻辑带库定义应答无效".into());
    let mut record = value.clone();
    if record
        .as_object_mut()
        .and_then(|v| v.remove("revision"))
        .and_then(|v| v.as_u64())
        .is_none_or(|revision| revision == 0)
    {
        return Err(bad());
    }
    let result: CreateLogicalLibrary = serde_json::from_value(record).map_err(|_| bad())?;
    let mut normalized = result.clone();
    normalized.normalize().map_err(|_| bad())?;
    if normalized != result {
        return Err(bad());
    }
    Ok(result)
}

fn verify_change(value: &Value, id: Uuid, operation: Uuid) -> Result<CreateLogicalLibrary> {
    let record = verify_definition(&value["logical_library"])?;
    if record.id != id
        || value["operation"] != operation.to_string()
        || value["deleted"].as_bool().is_none()
        || !value["fingerprint"].as_str().is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return Err(ClientError::Verification("逻辑带库变更回执身份无效".into()));
    }
    Ok(record)
}

impl FdbClient {
    pub fn change_logical_library(
        &self,
        library: &str,
        namespace: Uuid,
        id: Uuid,
        mut change: ChangeLogicalLibrary,
    ) -> Result<Value> {
        let next_revision = change
            .revision
            .checked_add(1)
            .filter(|_| change.revision != 0)
            .ok_or_else(|| invalid("逻辑带库修订号无效或已耗尽"))?;
        if id.is_nil() || change.version != 1 || change.operation.is_nil() {
            return Err(invalid("逻辑带库变更版本或身份无效"));
        }
        if let Some(replacement) = &mut change.replacement {
            let mut record = CreateLogicalLibrary {
                id,
                version: 1,
                name: replacement.name.clone(),
                resources: replacement.resources.clone(),
            };
            record.normalize().map_err(invalid)?;
            replacement.resources = record.resources;
        }
        let path = format!("{}/{}", base(library, namespace)?, id);
        let body = serde_json::to_vec(&change).map_err(|_| invalid("逻辑带库变更无法编码"))?;
        let deleting = change.replacement.is_none();
        let method = if deleting { "DELETE" } else { "PATCH" };
        let response = self
            .retry_request(
                method,
                &path,
                Some(&body),
                &[
                    ("Content-Type", "application/json"),
                    (NAMESPACE, &namespace.to_string()),
                ],
            )
            .map_err(|error| match error {
                ClientError::NoLeader(_) => ClientError::Indeterminate(format!(
                    "逻辑带库变更结果未定：{}；请查询原操作",
                    change.operation
                )),
                _ => error,
            })?;
        let value = management::parsed(response, &[200])?;
        verify_scope(&value, library, namespace)?;
        let record = verify_change(&value, id, change.operation)?;
        if value["logical_library"]["revision"] != next_revision
            || value["deleted"] != deleting
            || change.replacement.is_some_and(|replacement| {
                record.name != replacement.name || record.resources != replacement.resources
            })
        {
            return Err(ClientError::Verification(
                "逻辑带库变更回执与原请求不符".into(),
            ));
        }
        Ok(value)
    }

    pub fn logical_library_change_status(
        &self,
        library: &str,
        namespace: Uuid,
        id: Uuid,
        operation: Uuid,
    ) -> Result<Value> {
        if id.is_nil() || operation.is_nil() {
            return Err(invalid("查询逻辑带库变更时须提供非零 UUID"));
        }
        let path = format!("{}/{id}/changes/{operation}", base(library, namespace)?);
        let value = management::parsed(
            self.retry_request("GET", &path, None, &[(NAMESPACE, &namespace.to_string())])?,
            &[200],
        )?;
        verify_scope(&value, library, namespace)?;
        verify_change(&value, id, operation)?;
        Ok(value)
    }

    /// Keep the original request UUID and resource selection for retries.
    pub fn create_logical_library(
        &self,
        library: &str,
        namespace: Uuid,
        request: &CreateLogicalLibrary,
    ) -> Result<Value> {
        let path = base(library, namespace)?;
        let mut request = request.clone();
        request.normalize().map_err(invalid)?;
        let body = serde_json::to_vec(&request).map_err(|_| invalid("逻辑带库请求无法编码"))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(invalid("逻辑带库请求超过大小上限"));
        }
        let response = self
            .retry_request(
                "POST",
                &path,
                Some(&body),
                &[
                    ("Content-Type", "application/json"),
                    (NAMESPACE, &namespace.to_string()),
                ],
            )
            .map_err(|error| match error {
                ClientError::NoLeader(_) => ClientError::Indeterminate(format!(
                    "创建逻辑带库结果未定：{}；保留原参数并按该 UUID 查询或重试",
                    request.id
                )),
                _ => error,
            })?;
        let value = management::parsed(response, &[200, 201])?;
        verify_scope(&value, library, namespace)?;
        if verify_definition(&value["logical_library"])? != request
            || value["logical_library"]["revision"] != 1
        {
            return Err(ClientError::Verification(
                "逻辑带库创建回执与原请求不符".into(),
            ));
        }
        Ok(value)
    }

    pub fn list_logical_libraries(&self, library: &str, namespace: Uuid) -> Result<Value> {
        let path = base(library, namespace)?;
        let value = management::parsed(
            self.retry_request("GET", &path, None, &[(NAMESPACE, &namespace.to_string())])?,
            &[200],
        )?;
        verify_scope(&value, library, namespace)?;
        let items = value["items"]
            .as_array()
            .ok_or_else(|| ClientError::Verification("逻辑带库清单应答无效".into()))?;
        for item in items {
            verify_definition(item)?;
        }
        Ok(value)
    }

    pub fn logical_library(&self, library: &str, namespace: Uuid, selector: &str) -> Result<Value> {
        let id = Uuid::parse_str(selector).ok();
        if !valid_name(selector) && !id.is_some_and(|id| !id.is_nil() && id.to_string() == selector)
        {
            return Err(invalid("逻辑带库选择器须为名称或规范 UUID"));
        }
        let path = format!("{}/{}", base(library, namespace)?, encode_query(selector));
        let value = management::parsed(
            self.retry_request("GET", &path, None, &[(NAMESPACE, &namespace.to_string())])?,
            &[200],
        )?;
        verify_scope(&value, library, namespace)?;
        let record = verify_definition(&value["logical_library"])?;
        if id.map_or(record.name != selector, |id| record.id != id) {
            return Err(ClientError::Verification("逻辑带库应答与选择器不符".into()));
        }
        Ok(value)
    }
}
