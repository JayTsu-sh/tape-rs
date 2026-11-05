//! Read operational facts from FDB; never dispatch a device request.
use super::*;
use uuid::Uuid;

fn known_query(request: &Request, allowed: &[&str]) -> Result<(), Box<Response>> {
    for part in request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|s| !s.is_empty())
    {
        let (key, _) = part
            .split_once('=')
            .ok_or_else(|| Box::new(invalid("查询参数格式无效")))?;
        if !allowed.contains(&key) {
            return Err(Box::new(invalid("查询包含不支持的参数")));
        }
        query_value(request, key).map_err(|e| Box::new(invalid(e)))?;
    }
    Ok(())
}
fn scoped(
    ctx: &Context,
    serial: &str,
    request: &Request,
    allowed: &[&str],
) -> Result<(OwnedSemaphorePermit, ActiveLibraryRoute), Box<Response>> {
    let permit = pools::budget(ctx)?;
    let route = pools::route(ctx, serial)?;
    pools::namespace(request.headers(), route)?;
    known_query(request, allowed)?;
    Ok((permit, route))
}
fn age(time: &Value) -> Value {
    time.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| json!((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()))
        .unwrap_or(Value::Null)
}
pub(super) async fn library_view(
    ctx: &Context,
    serial: &str,
    route: ActiveLibraryRoute,
    details: bool,
) -> crate::managed_catalog::Result<Value> {
    let mut view = ctx
        .catalog
        .active_library_management_view(serial, route)
        .await?;
    view["inventory_age_seconds"] = age(&view["inventory"]["observed_at"]);
    view["inventory_owner_current"] = if view["inventory"].is_null() {
        Value::Null
    } else {
        json!(view["inventory"]["round"] == view["owner_epoch"])
    };
    view["num_of_volumes"] = view["inventory"]["volumes"]
        .as_array()
        .map(|v| json!(v.len()))
        .unwrap_or(Value::Null);
    view["num_of_drives"] = view["inventory"]["drives"]
        .as_array()
        .map(|v| json!(v.len()))
        .unwrap_or(Value::Null);
    let mut nodes = Vec::new();
    let mut active = Value::Null;
    if let Some(owner) = &ctx.owner
        && let Some(library) = owner.libraries.get(serial)
    {
        let status = library.status.lock().unwrap_or_else(|e| e.into_inner());
        let peers = owner.peers.iter().collect::<BTreeMap<_, _>>();
        for (id, address) in peers {
            let reported = status.candidates.get(id);
            nodes.push(json!({"id":id,"endpoint":address,"reported_eligible":reported.map(|v|v.eligible),"reported_drives":reported.map(|v|v.drives)}));
        }
        if !status.shutting_down
            && !status.catalog_pending
            && status.executor_ready
            && status.catalog_frozen_index == Some(route.frozen_index)
            && let Some(e) = status.executor.as_ref().filter(|e| {
                e.fenced
                    && e.node > 0
                    && view["owner_epoch"] == e.round
                    && owner.peers.contains_key(&e.node)
            })
        {
            active = json!(e.node);
        }
    }
    view["nodes"] = json!(nodes);
    view["active_node"] = active;
    view["execution_readiness"] = json!("unknown");
    if !details && !view["inventory"].is_null() {
        view["inventory"] = json!({"owner":view["inventory"]["owner"],"round":view["inventory"]["round"],"observed_at":view["inventory"]["observed_at"]});
    }
    Ok(view)
}
pub(super) async fn libraries(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    let _permit = match pools::budget(&ctx) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if let Err(e) = known_query(&request, &["library"]) {
        return *e;
    }
    let filter = match query_value(&request, "library") {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    if filter.is_some_and(|v| !ctx.routes.contains_key(v)) {
        return reply(StatusCode::NOT_FOUND, json!({"error":"library_not_found"}));
    }
    let mut items = Vec::new();
    for (serial, route) in ctx
        .routes
        .iter()
        .filter(|(s, _)| filter.is_none_or(|f| f == s.as_str()))
    {
        match library_view(&ctx, serial, *route, false).await {
            Ok(v) => items.push(v),
            Err(e) => return pools::failure(e),
        }
    }
    reply(StatusCode::OK, json!({"version":1,"items":items}))
}
pub(super) async fn library(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let (_permit, route) = match scoped(&ctx, &serial, &request, &[]) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match library_view(&ctx, &serial, route, true).await {
        Ok(v) => reply(
            StatusCode::OK,
            json!({"version":1,"library_serial":serial,"namespace":route.route.namespace.to_string(),"library":v}),
        ),
        Err(e) => pools::failure(e),
    }
}
fn pool_selector(request: &Request) -> Result<Option<String>, Box<Response>> {
    query_value(request, "pool")
        .map(|value| value.map(http::percent_decode))
        .map_err(|e| Box::new(invalid(e)))
}
async fn selected_pool(
    ctx: &Context,
    serial: &str,
    route: ActiveLibraryRoute,
    selector: Option<&str>,
) -> Result<Option<Uuid>, Box<Response>> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    match pools::resolve(ctx, serial, route, selector).await {
        Ok(Some(pool)) => Ok(Some(pool.id)),
        Ok(None) => Err(Box::new(reply(
            StatusCode::NOT_FOUND,
            json!({"error":"pool_not_found"}),
        ))),
        Err(error) => Err(error),
    }
}
fn cursor_prefix(serial: &str, route: ActiveLibraryRoute, pool: Option<Uuid>) -> Vec<u8> {
    let mut p = b"T1/".to_vec();
    p.extend_from_slice(&(serial.len() as u64).to_be_bytes());
    p.extend_from_slice(serial.as_bytes());
    p.extend_from_slice(route.route.namespace.as_bytes());
    p.extend_from_slice(pool.unwrap_or(Uuid::nil()).as_bytes());
    p
}
fn encode_cursor(
    serial: &str,
    route: ActiveLibraryRoute,
    pool: Option<Uuid>,
    barcode: &str,
) -> String {
    let mut p = cursor_prefix(serial, route, pool);
    p.extend_from_slice(barcode.as_bytes());
    URL_SAFE_NO_PAD.encode(p)
}
fn decode_cursor(
    serial: &str,
    route: ActiveLibraryRoute,
    pool: Option<Uuid>,
    encoded: &str,
) -> Result<String, Box<Response>> {
    if encoded.len() > 512 {
        return Err(Box::new(invalid("磁带分页游标过长")));
    }
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| Box::new(invalid("磁带分页游标无效")))?;
    let p = cursor_prefix(serial, route, pool);
    raw.strip_prefix(p.as_slice())
        .filter(|v| !v.is_empty())
        .and_then(|v| String::from_utf8(v.to_vec()).ok())
        .ok_or_else(|| Box::new(invalid("磁带游标的库、命名空间或池不符")))
}
pub(super) async fn tapes(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let (_permit, route) = match scoped(&ctx, &serial, &request, &["pool", "limit", "after"]) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let selector = match pool_selector(&request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let pool = match selected_pool(&ctx, &serial, route, selector.as_deref()).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(v)) => match v.parse::<usize>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return invalid("磁带分页limit须在1..=100"),
        },
        Err(e) => return invalid(e),
    };
    let after = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(v)) => match decode_cursor(&serial, route, pool, v) {
            Ok(v) => Some(v),
            Err(e) => return *e,
        },
        Err(e) => return invalid(e),
    };
    match ctx
        .catalog
        .active_tape_page_for_route(&serial, route, pool, after.as_deref(), limit)
        .await
    {
        Ok(mut v) => {
            if let Some(b) = v["next"].as_str() {
                v["next"] = json!(encode_cursor(&serial, route, pool, b));
            }
            reply(StatusCode::OK, v)
        }
        Err(e) => pools::failure(e),
    }
}
pub(super) async fn tape(
    State(ctx): State<Arc<Context>>,
    Path((serial, barcode)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (_permit, route) = match scoped(&ctx, &serial, &request, &["pool"]) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let selector = match pool_selector(&request) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let pool = match selected_pool(&ctx, &serial, route, selector.as_deref()).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match ctx
        .catalog
        .active_tape_for_route(&serial, route, &barcode)
        .await
    {
        Ok(Some(value)) if pool.is_none_or(|p| value["assignment"]["pool"] == p.to_string()) => {
            reply(
                StatusCode::OK,
                json!({"version":1,"library_serial":serial,"namespace":route.route.namespace.to_string(),"tape":value}),
            )
        }
        Ok(_) => reply(StatusCode::NOT_FOUND, json!({"error":"tape_not_found"})),
        Err(e) => pools::failure(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tape_cursors_bind_library_namespace_and_pool() {
        let route = ActiveLibraryRoute {
            route: crate::managed_catalog::PreparedLibraryRoute {
                namespace: Uuid::new_v4(),
                revision: 1,
            },
            frozen_index: 90,
            certificate_sha256: [1; 32],
            initial_owner_epoch: 91,
        };
        let pool = Uuid::new_v4();
        let cursor = encode_cursor("LIB_A", route, Some(pool), "T003L8");
        assert_eq!(
            decode_cursor("LIB_A", route, Some(pool), &cursor).unwrap(),
            "T003L8"
        );
        assert!(decode_cursor("LIB_B", route, Some(pool), &cursor).is_err());
        assert!(decode_cursor("LIB_A", route, None, &cursor).is_err());
        assert!(decode_cursor("LIB_A", route, Some(Uuid::new_v4()), &cursor).is_err());
        let mut changed = route;
        changed.route.namespace = Uuid::new_v4();
        assert!(decode_cursor("LIB_A", changed, Some(pool), &cursor).is_err());
        assert!(decode_cursor("LIB_A", route, Some(pool), &"x".repeat(513)).is_err());
    }
}
