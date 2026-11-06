use super::*;
use crate::managed_catalog::ObjectListRequest;

static OBJECT_REQUESTS: Semaphore = Semaphore::const_new(8);

fn text(request: &Request, key: &str) -> Result<String, &'static str> {
    let value = query_value(request, key)?.unwrap_or("");
    if value.len() > 2800 {
        return Err("对象查询参数过长");
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "对象参数不是 Base64URL")?;
    let text = String::from_utf8(bytes).map_err(|_| "对象参数不是 UTF-8")?;
    if text.len() > 2048 || text.contains('\0') {
        return Err("对象参数过长或含 NUL");
    }
    Ok(text)
}

pub(super) async fn list(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let Ok(_permit) = OBJECT_REQUESTS.try_acquire() else {
        return unavailable("object_list_busy");
    };
    let Some(&route) = ctx.routes.get(&serial) else {
        return reply(StatusCode::NOT_FOUND, json!({"error":"带库未配置"}));
    };
    let parsed = (|| -> Result<_, &'static str> {
        let pool = query_value(&request, "pool")?
            .and_then(|v| uuid::Uuid::parse_str(v).ok())
            .filter(|id| !id.is_nil())
            .ok_or("缺少有效 pool UUID")?;
        let prefix = text(&request, "prefix")?;
        let delimiter = text(&request, "delimiter")?;
        if !matches!(delimiter.as_str(), "" | "/") {
            return Err("delimiter 只支持 / 或空值");
        }
        let max_keys = query_value(&request, "max-keys")?
            .unwrap_or("1000")
            .parse::<usize>()
            .ok()
            .filter(|n| *n <= 1000)
            .ok_or("max-keys 须在 0..=1000")?;
        let mut start = text(&request, "start-after")?;
        if let Some(token) = query_value(&request, "continuation-token")? {
            if token.len() > 16384 {
                return Err("continuation-token 过长");
            }
            let raw = URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| "continuation-token 编码无效")?;
            let value: Value =
                serde_json::from_slice(&raw).map_err(|_| "continuation-token 无效")?;
            if value["version"] != 1
                || value["library"] != serial
                || value["namespace"] != route.route.namespace.to_string()
                || value["pool"] != pool.to_string()
                || value["prefix"] != prefix
                || value["delimiter"] != delimiter
            {
                return Err("continuation-token 不属于本次查询或 Active namespace");
            }
            start = value["last"]
                .as_str()
                .filter(|v| v.len() <= 2048 && !v.contains('\0'))
                .ok_or("continuation-token 游标无效")?
                .to_owned();
        }
        Ok((pool, prefix, delimiter, max_keys, start))
    })();
    let (pool, prefix, delimiter, max_keys, start) = match parsed {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    match ctx
        .catalog
        .list_active_objects_for_route(
            &serial,
            route,
            ObjectListRequest {
                pool,
                prefix: &prefix,
                delimiter: &delimiter,
                max_keys,
                start_after: &start,
            },
        )
        .await
    {
        Ok(page) => {
            let next = page.next.map(|last| URL_SAFE_NO_PAD.encode(json!({"version":1,"library":serial,"namespace":route.route.namespace.to_string(),"pool":pool.to_string(),"prefix":prefix,"delimiter":delimiter,"last":String::from_utf8(last).expect("UTF-8 object key")}).to_string()));
            let mut contents = Vec::new();
            let mut common = Vec::new();
            let count = page.items.len();
            for item in page.items {
                if let Some((id, head)) = item.file {
                    contents.push(json!({"key":item.key,"id":id.to_string(),"size":head.size,"version":head.version}));
                } else {
                    common.push(item.key);
                }
            }
            reply(
                StatusCode::OK,
                json!({"library_serial":serial,"namespace":route.route.namespace.to_string(),"pool_id":pool.to_string(),"prefix":prefix,"delimiter":delimiter,"max_keys":max_keys,"key_count":count,"contents":contents,"common_prefixes":common,"is_truncated":next.is_some(),"next_continuation_token":next}),
            )
        }
        Err(e) => catalog_error(e),
    }
}
