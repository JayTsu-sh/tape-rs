//! Read-only operational queries. Namespace selection never changes on retry.
use super::*;
const NS: &str = "x-tapers-catalog-namespace";
fn bad(reason: &str) -> ClientError {
    ClientError::Verification(reason.into())
}
fn canonical(value: &Value) -> Result<Uuid> {
    value
        .as_str()
        .and_then(|s| {
            Uuid::parse_str(s)
                .ok()
                .filter(|v| !v.is_nil() && v.to_string() == s)
        })
        .ok_or_else(|| bad("查询响应UUID无效"))
}
fn scope(value: &Value, library: &str, namespace: &str) -> Result<()> {
    pools::scope(value, library, namespace)?;
    if value["version"] != 1 {
        return Err(bad("管理查询响应版本无效"));
    }
    Ok(())
}
fn check_library(value: &Value, library: &str, namespace: &str) -> Result<()> {
    scope(value, library, namespace)?;
    if !value["owner_epoch"]
        .as_u64()
        .is_some_and(|v| v > 0 && v <= 0xFFFF_FFFF_FFFF)
    {
        return Err(bad("带库owner epoch无效"));
    }
    let nodes = value["nodes"]
        .as_array()
        .ok_or_else(|| bad("带库nodes缺失"))?;
    let mut ids = std::collections::BTreeSet::new();
    for n in nodes {
        let id = n["id"]
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or_else(|| bad("节点ID无效"))?;
        if !ids.insert(id) || !n["endpoint"].as_str().is_some_and(|v| !v.is_empty()) {
            return Err(bad("节点重复或地址无效"));
        }
    }
    if !(value["active_node"].is_null()
        || value["active_node"]
            .as_u64()
            .is_some_and(|v| ids.contains(&v)))
    {
        return Err(bad("active_node不属于该库nodes"));
    }
    for field in ["num_of_volumes", "num_of_drives"] {
        if !(value[field].is_null() || value[field].as_u64().is_some()) {
            return Err(bad("库存计数无效"));
        }
    }
    if !(value["inventory"].is_null() || value["inventory"].is_object())
        || !(value["monitor"].is_null() || value["monitor"].is_object())
    {
        return Err(bad("库存/巡检响应无效"));
    }
    Ok(())
}
fn barcode(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| (b'!'..=b'~').contains(&b) && b != b'/')
}
fn check_tape(value: &Value, library: &str, namespace: &str, pool: Option<&str>) -> Result<()> {
    pools::scope(value, library, namespace)?;
    if !value["barcode"].as_str().is_some_and(barcode)
        || !matches!(
            value["presence"].as_str(),
            Some("unknown" | "observed" | "not_observed")
        )
    {
        return Err(bad("磁带条码或观测状态无效"));
    }
    let assignment = &value["assignment"];
    if assignment.is_null() {
        if pool.is_some() || !value["pool"].is_null() {
            return Err(bad("池过滤返回了未分配磁带"));
        }
    } else {
        let id = canonical(&assignment["pool"])?;
        if assignment["library"] != library
            || assignment["namespace"] != namespace
            || assignment["barcode"] != value["barcode"]
            || !assignment["online"].is_boolean()
            || !assignment["blacklisted"].is_boolean()
            || !assignment["revision"].as_u64().is_some_and(|r| r > 0)
            || pool.is_some_and(|p| id.to_string() != p)
        {
            return Err(bad("磁带归属响应作用域无效"));
        }
        if !assignment["volume"].is_null() {
            canonical(&assignment["volume"])?;
        }
        pools::check_pool(&value["pool"], library)?;
        if value["pool"]["id"] != assignment["pool"] {
            return Err(bad("磁带与池响应标识不符"));
        }
    }
    Ok(())
}
impl FdbClient {
    pub fn libraries(&self, filter: Option<&str>) -> Result<Value> {
        if let Some(serial) = filter {
            pools::base(serial)?;
        }
        let path = format!(
            "/admin/libraries{}",
            filter
                .map(|s| format!("?library={}", encode_query(s)))
                .unwrap_or_default()
        );
        let value = pools::parsed(self.retry_request("GET", &path, None, &[])?, &[200])?;
        if value["version"] != 1 {
            return Err(bad("带库清单版本无效"));
        }
        let items = value["items"]
            .as_array()
            .ok_or_else(|| bad("带库清单缺失"))?;
        let mut previous = None;
        for item in items {
            let library = item["library_serial"]
                .as_str()
                .ok_or_else(|| bad("带库序列号缺失"))?;
            let ns = canonical(&item["namespace"])?;
            if previous.is_some_and(|p: &str| library <= p) || filter.is_some_and(|f| f != library)
            {
                return Err(bad("带库清单重复、无序或超出指定库"));
            }
            check_library(item, library, &ns.to_string())?;
            previous = Some(library);
        }
        if filter.is_some() && items.len() != 1 {
            return Err(bad("指定带库清单不完整"));
        }
        Ok(value)
    }
    pub fn library(&self, library: &str, namespace: &str) -> Result<Value> {
        pools::uuid(namespace)?;
        let value = pools::parsed(
            self.retry_request(
                "GET",
                &format!("{}/library", pools::base(library)?),
                None,
                &[(NS, namespace)],
            )?,
            &[200],
        )?;
        scope(&value, library, namespace)?;
        check_library(&value["library"], library, namespace)?;
        Ok(value)
    }
    /// pool is an already resolved, canonical UUID; names are resolved once by pool().
    pub fn tape_page(
        &self,
        library: &str,
        namespace: &str,
        pool: Option<&str>,
        after: Option<&str>,
    ) -> Result<Value> {
        pools::uuid(namespace)?;
        if let Some(pool) = pool {
            pools::uuid(pool)?;
        }
        let path = format!(
            "{}/tapes?limit=100{}{}",
            pools::base(library)?,
            pool.map(|v| format!("&pool={}", encode_query(v)))
                .unwrap_or_default(),
            after
                .map(|v| format!("&after={}", encode_query(v)))
                .unwrap_or_default()
        );
        let value = pools::parsed(
            self.retry_request("GET", &path, None, &[(NS, namespace)])?,
            &[200],
        )?;
        scope(&value, library, namespace)?;
        if value["pool"] != pool.map(Value::from).unwrap_or(Value::Null)
            || !value["inventory_available"].is_boolean()
        {
            return Err(bad("磁带分页池作用域或库存可用性无效"));
        }
        let items = value["items"]
            .as_array()
            .filter(|v| v.len() <= 100)
            .ok_or_else(|| bad("磁带分页大小无效"))?;
        if value.get("next").is_none()
            || !(value["next"].is_null()
                || value["next"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty() && v.len() <= 512))
        {
            return Err(bad("磁带分页游标无效"));
        }
        let mut previous = None;
        for item in items {
            check_tape(item, library, namespace, pool)?;
            let current = item["barcode"].as_str().unwrap();
            if previous.is_some_and(|p: &str| current <= p) {
                return Err(bad("磁带分页条码重复或无序"));
            }
            previous = Some(current);
        }
        if items.is_empty() && !value["next"].is_null() {
            return Err(bad("空磁带页不能继续分页"));
        }
        Ok(value)
    }
    pub fn tape(
        &self,
        library: &str,
        namespace: &str,
        pool: Option<&str>,
        barcode_value: &str,
    ) -> Result<Value> {
        pools::uuid(namespace)?;
        if let Some(pool) = pool {
            pools::uuid(pool)?;
        }
        if !barcode(barcode_value) {
            return Err(invalid("磁带条码无效"));
        }
        let path = format!(
            "{}/tapes/{}{}",
            pools::base(library)?,
            encode_query(barcode_value),
            pool.map(|v| format!("?pool={}", encode_query(v)))
                .unwrap_or_default()
        );
        let value = pools::parsed(
            self.retry_request("GET", &path, None, &[(NS, namespace)])?,
            &[200],
        )?;
        scope(&value, library, namespace)?;
        check_tape(&value["tape"], library, namespace, pool)?;
        if value["tape"]["barcode"] != barcode_value {
            return Err(bad("磁带查询返回其他条码"));
        }
        Ok(value)
    }
}
