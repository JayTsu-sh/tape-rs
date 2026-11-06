use super::*;

#[derive(Clone, Debug)]
pub struct ListObjectsOptions {
    pub prefix: String,
    /// Empty for recursive objects, '/' for CommonPrefixes.
    pub delimiter: String,
    pub max_keys: usize,
    pub continuation_token: Option<String>,
    pub start_after: Option<String>,
}
impl Default for ListObjectsOptions {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            delimiter: String::new(),
            max_keys: 1000,
            continuation_token: None,
            start_after: None,
        }
    }
}

impl FdbClient {
    /// Pool-scoped ListObjectsV2 query. The JSON projection has contents,
    /// common_prefixes, key_count, is_truncated and next_continuation_token.
    /// This uses the FDB HTTP API; it is not an AWS authentication adapter.
    pub fn list_objects_v2(
        &self,
        library: &str,
        pool: Uuid,
        options: &ListObjectsOptions,
    ) -> Result<Value> {
        if pool.is_nil()
            || options.max_keys > 1000
            || !matches!(options.delimiter.as_str(), "" | "/")
            || [
                &options.prefix,
                options.start_after.as_deref().unwrap_or(""),
            ]
            .iter()
            .any(|s| s.len() > 2048 || s.contains('\0'))
        {
            return Err(invalid("对象列举参数无效，max-keys 须在 0..=1000"));
        }
        // Reuse the strict library validation without exposing its path route.
        reading::read_route("objects", library, "/")?;
        let mut route = format!(
            "/v1/fdb/objects/{library}?pool={pool}&prefix={}&delimiter={}&max-keys={}",
            URL_SAFE_NO_PAD.encode(&options.prefix),
            URL_SAFE_NO_PAD.encode(&options.delimiter),
            options.max_keys
        );
        if let Some(token) = &options.continuation_token {
            if token.is_empty() || token.len() > 16384 || URL_SAFE_NO_PAD.decode(token).is_err() {
                return Err(invalid("continuation-token 无效"));
            }
            route.push_str("&continuation-token=");
            route.push_str(token);
        }
        if let Some(start) = &options.start_after {
            route.push_str("&start-after=");
            route.push_str(&URL_SAFE_NO_PAD.encode(start));
        }
        let response = self.retry_request("GET", &route, None, &[])?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        reading::check_scope(&value, library)?;
        if value["pool_id"] != pool.to_string()
            || value["prefix"] != options.prefix
            || value["delimiter"] != options.delimiter
            || value["max_keys"] != options.max_keys
        {
            return Err(ClientError::Verification(
                "对象列举应答与查询作用域不符".into(),
            ));
        }
        let invalid_reply = || ClientError::Verification("对象分页格式或字典序无效".into());
        let contents = value["contents"].as_array().ok_or_else(invalid_reply)?;
        let common = value["common_prefixes"]
            .as_array()
            .ok_or_else(invalid_reply)?;
        let mut previous: Option<&str> = None;
        for item in contents {
            reading::uuid_field(item, "id", false)?;
            let key = item["key"].as_str().ok_or_else(invalid_reply)?;
            if !key.starts_with(&options.prefix)
                || previous.is_some_and(|p| p >= key)
                || item["size"].as_u64().is_none()
                || !item["version"].as_u64().is_some_and(|v| v > 0)
                || (!options.delimiter.is_empty() && key[options.prefix.len()..].contains('/'))
            {
                return Err(invalid_reply());
            }
            previous = Some(key);
        }
        previous = None;
        for item in common {
            let key = item.as_str().ok_or_else(invalid_reply)?;
            if options.delimiter.is_empty()
                || !key.starts_with(&options.prefix)
                || !key.ends_with('/')
                || key.len() <= options.prefix.len()
                || key[options.prefix.len()..key.len() - 1].contains('/')
                || previous.is_some_and(|p| p >= key)
            {
                return Err(invalid_reply());
            }
            previous = Some(key);
        }
        let count = contents.len() + common.len();
        let more = value["is_truncated"].as_bool().ok_or_else(invalid_reply)?;
        if count > options.max_keys
            || value["key_count"] != count
            || value.get("next_continuation_token").is_none()
            || more != value["next_continuation_token"].is_string()
            || (more && count == 0)
            || (!more && !value["next_continuation_token"].is_null())
        {
            return Err(invalid_reply());
        }
        Ok(value)
    }
}
