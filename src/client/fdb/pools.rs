use super::*;

fn base(library: &str) -> Result<String> {
    Client::new(Vec::<String>::new()).with_library(library)?;
    Ok(format!("/admin/libraries/{library}/pools"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains(['\0', '/'])
}

fn verify_pool(value: &Value, library: &str) -> Result<Uuid> {
    let bad = || ClientError::Verification("FDB 池记录应答无效".into());
    let id = reading::uuid_field(value, "id", false)?;
    let time = value["created_at_millis"]
        .as_u64()
        .filter(|n| (1..=253_402_300_799_999).contains(n))
        .ok_or_else(bad)?;
    let date = chrono::DateTime::from_timestamp_millis(time as i64)
        .ok_or_else(bad)?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let p = &value["policy"];
    if value["id"] != id.to_string()
        || value["library_serial"] != library
        || !value["name"].as_str().is_some_and(valid_name)
        || !value["file_limit"].as_u64().is_some_and(|n| n > 0)
        || !value["revision"].as_u64().is_some_and(|n| n > 0)
        || value["created_at"] != date
        || !p.as_object().is_some_and(|p| {
            p.len() == 5
                && p.keys().all(|key| {
                    [
                        "media_restriction",
                        "mount_limit",
                        "low_space_warning_enabled",
                        "low_space_warning_threshold",
                        "no_space_warning_enabled",
                    ]
                    .contains(&key.as_str())
                })
        })
        || !p["media_restriction"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 32)
        || !p["mount_limit"].as_u64().is_some_and(|n| n <= 32767)
        || !p["low_space_warning_threshold"]
            .as_u64()
            .is_some_and(|n| n <= 32767)
        || p["low_space_warning_enabled"].as_bool().is_none()
        || p["no_space_warning_enabled"].as_bool().is_none()
    {
        return Err(bad());
    }
    Ok(id)
}

impl FdbClient {
    /// One stable namespace and UUID make retrying this PUT idempotent. A
    /// different existing specification is a conflict, never an update.
    pub fn create_pool(
        &self,
        library: &str,
        namespace: Uuid,
        id: Uuid,
        name: &str,
        file_limit: u64,
    ) -> Result<Value> {
        let base = base(library)?;
        if namespace.is_nil() || id.is_nil() || !valid_name(name) || file_limit == 0 {
            return Err(invalid(
                "创建池须提供有效 namespace、池 UUID、名称与文件数上限",
            ));
        }
        let body = serde_json::json!({"namespace":namespace.to_string(),"name":name,"file_limit":file_limit}).to_string();
        let response = self.retry_request(
            "PUT",
            &format!("{base}/{id}"),
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?;
        if !matches!(response.status, 200 | 201) {
            return Err(rejected(response));
        }
        let value = response.json();
        reading::check_scope(&value, library)?;
        let pool = &value["pool"];
        if value["namespace"] != namespace.to_string()
            || verify_pool(pool, library)? != id
            || pool["name"] != name
            || pool["file_limit"] != file_limit
            || pool["policy"]
                != serde_json::json!({"media_restriction":"none","mount_limit":0,"low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true})
        {
            return Err(ClientError::Verification("池创建应答与原请求不符".into()));
        }
        Ok(value)
    }

    pub fn pool(&self, library: &str, id: Uuid) -> Result<Option<Value>> {
        let base = base(library)?;
        if id.is_nil() {
            return Err(invalid("池 UUID 不能为 nil"));
        }
        let response = self.retry_request("GET", &format!("{base}/{id}"), None, &[])?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        reading::check_scope(&value, library)?;
        if verify_pool(&value["pool"], library)? != id {
            return Err(ClientError::Verification("池详情 UUID 不符".into()));
        }
        Ok(Some(value))
    }

    pub fn list_pools(&self, library: &str, limit: usize, after: Option<&str>) -> Result<Value> {
        let base = base(library)?;
        if !(1..=100).contains(&limit) {
            return Err(invalid("池分页大小须在 1..=100"));
        }
        let mut route = format!("{base}?limit={limit}");
        if let Some(after) = after {
            if after.is_empty() || after.len() > 1024 || URL_SAFE_NO_PAD.decode(after).is_err() {
                return Err(invalid("池分页游标无效"));
            }
            route.push_str("&after=");
            route.push_str(after);
        }
        let response = self.retry_request("GET", &route, None, &[])?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        reading::check_scope(&value, library)?;
        let bad = || ClientError::Verification("池分页应答无效".into());
        let rows = value["items"].as_array().ok_or_else(bad)?;
        if value["limit"] != limit || rows.len() > limit {
            return Err(bad());
        }
        let mut previous = None;
        for row in rows {
            let id = verify_pool(row, library)?;
            if previous.is_some_and(|previous| previous >= id) {
                return Err(bad());
            }
            previous = Some(id);
        }
        if !value["next"].is_null()
            && !value["next"].as_str().is_some_and(|s| {
                !s.is_empty() && s.len() <= 1024 && URL_SAFE_NO_PAD.decode(s).is_ok()
            })
        {
            return Err(bad());
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        let time = 1_793_405_000_123;
        json!({"id":Uuid::new_v4().to_string(),"library_serial":"SIM-POOLS","name":"归档","file_limit":200_000,"revision":1,
            "created_at_millis":time,"created_at":chrono::DateTime::from_timestamp_millis(time).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "policy":{"media_restriction":"none","mount_limit":0,"low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true}})
    }

    #[test]
    fn pool_reply_requires_scope_time_and_policy() {
        let value = sample();
        assert!(verify_pool(&value, "SIM-POOLS").is_ok());
        for (field, bad) in [
            ("library_serial", json!("OTHER")),
            ("id", json!(Uuid::nil().to_string())),
            ("name", json!("a/b")),
            ("file_limit", json!(0)),
            ("revision", json!(0)),
            ("created_at_millis", json!(0)),
            ("created_at", json!("1970-01-01T00:00:00Z")),
            ("policy", json!({})),
        ] {
            let mut changed = value.clone();
            changed[field] = bad;
            assert!(verify_pool(&changed, "SIM-POOLS").is_err(), "{field}");
        }
    }

    #[test]
    fn invalid_pool_requests_fail_before_transport() {
        let client = FdbClient::new(["127.0.0.1:1"]).unwrap();
        let id = Uuid::new_v4();
        for (namespace, pool, name, limit) in [
            (Uuid::nil(), id, "a", 1),
            (id, Uuid::nil(), "a", 1),
            (id, id, "a/b", 1),
            (id, id, "a", 0),
        ] {
            match client.create_pool("SIM-POOLS", namespace, pool, name, limit) {
                Err(ClientError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
                other => panic!("{other:?}"),
            }
        }
    }
}
