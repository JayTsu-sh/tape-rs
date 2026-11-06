//! Inventory reads join cached physical observations and current FDB ownership.
//! No query in this module opens a device or queues an executor request.
use super::*;
use uuid::Uuid;

type Cursor = (u8, BTreeMap<String, String>, String, String);

async fn snapshot(
    ctx: &Context,
    serial: &str,
) -> Result<super::super::node::NodeStatus, CatalogError> {
    let mut status = ctx
        .owner
        .as_ref()
        .and_then(|o| o.libraries.get(serial))
        .map(|l| l.status.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default();
    if let Some(observation) = ctx
        .catalog
        .active_library_inventory(serial, ctx.routes[serial])
        .await?
    {
        let mut management = (*status.management).clone();
        if management.is_null() {
            management = json!({});
        }
        management["inventory"] = observation;
        status.management = Arc::new(management);
    }
    if let Some(monitor) = ctx
        .catalog
        .active_library_monitor(serial, ctx.routes[serial])
        .await?
    {
        let mut management = (*status.management).clone();
        if management.is_null() {
            management = json!({});
        }
        management["monitor"] = monitor;
        status.management = Arc::new(management);
    }
    Ok(status)
}

fn observed(s: &super::super::node::NodeStatus) -> Vec<String> {
    s.management["inventory"]["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["barcode"].as_str().map(str::to_owned))
        .collect()
}

fn tape_view(serial: &str, s: &super::super::node::NodeStatus, data: Value) -> Value {
    let barcode = data["barcode"].as_str().expect("catalog inventory barcode");
    let inventory = &s.management["inventory"];
    let locations: Vec<_> = inventory["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["barcode"] == barcode)
        .collect();
    let location = (locations.len() == 1).then(|| locations[0]);
    let cleaning = crate::changer::element::cleaning_source(
        location
            .and_then(|l| l["medium_type"].as_u64())
            .and_then(|v| v.try_into().ok()),
        Some(barcode),
    );
    let assignment = &data["assignment"];
    let monitor = &data["monitor"];
    let sample = &monitor["sample"];
    let observation = super::super::management::inventory_observation(s);
    let state = crate::managed_catalog::tape_inventory_state(
        &data,
        inventory,
        observation["owner_matches_current"] == true,
    );
    let alerts = sample["alerts"].as_array();
    let status = match state {
        "duplicated" | "missing" | "require_validate" => "error",
        "require_replace" => "degraded",
        "blacklisted" => "warning",
        "unknown" => "unknown",
        "appendable" if sample["errors"].as_array().is_some_and(|v| v.is_empty()) => {
            if alerts.is_some_and(|a| !a.is_empty()) {
                "warning"
            } else {
                "ok"
            }
        }
        _ => "info",
    };
    let maximum = sample["data"]["maximum"].as_u64();
    let remaining = sample["data"]["remaining"].as_u64();
    json!({
        "tapeid":barcode,"barcode":barcode,"library":serial,"library_serial":serial,
        "pool_id":data["pool"]["id"],"poolname":data["pool"]["name"],
        "pool_name":data["pool"]["name"],"volume_uuid":assignment["volume"],
        "status":status,
        "state":state,"cleaning":cleaning.is_some(),"cleaning_source":cleaning,
        "online":assignment["online"],"blacklisted":assignment["blacklisted"],
        "offline":assignment["online"].as_bool().map(|v|!v),"recall_only":state=="recall_only",
        "offline_message":assignment["offline_message"],
        "usable":maximum,"used":maximum.zip(remaining).and_then(|(m,r)|m.checked_sub(r)),"available":remaining,
        "reclaimable":data["statistics"]["reclaimable"],"reclaimable_unit":"percent",
        "reclaimable_bytes":data["statistics"]["reclaimable_bytes"],"live_bytes":data["statistics"]["live_bytes"],
        "live_data_objects":data["statistics"]["live_data_objects"],"statistics":data["statistics"],
        "capacity_unit":"bytes","capacity_source":if maximum.is_some(){"mam"}else{"unknown"},
        "monitor":monitor,"library_monitor":s.management["monitor"],
        "location":location.and_then(|l|match l["element_type"].as_u64(){Some(1)=>Some("robot"),Some(2)=>Some("homeslot"),Some(3)=>Some("ieslot"),Some(4)=>Some("drive"),_=>None}),
        "address":location.and_then(|l|l["address"].as_u64()),
        "inventory_observation":observation,"task_id":data["task_id"],
        "task_progress":data["task_progress"],"assignment":assignment
    })
}

async fn tape_with_statistics(
    ctx: &Context,
    serial: &str,
    status: &super::super::node::NodeStatus,
    mut row: Value,
) -> Result<Value, CatalogError> {
    let barcode = row["barcode"]
        .as_str()
        .expect("validated inventory barcode")
        .to_owned();
    row["statistics"] = ctx
        .catalog
        .active_tape_statistics(serial, ctx.routes[serial], &barcode)
        .await?;
    Ok(tape_view(serial, status, row))
}

fn selected(ctx: &Context, serial: Option<String>) -> Result<Vec<String>, Box<Response>> {
    match serial {
        Some(serial) if ctx.routes.contains_key(&serial) => Ok(vec![serial]),
        Some(_) => Err(Box::new(reply(
            StatusCode::NOT_FOUND,
            json!({"error":"带库未配置"}),
        ))),
        None => Ok(ctx.routes.keys().cloned().collect()),
    }
}

async fn library_row(ctx: &Context, serial: &str) -> Result<Value, CatalogError> {
    let s = snapshot(ctx, serial).await?;
    let drives: Vec<_> = s.management["inventory"]["drives"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["serial"].as_str().map(str::to_owned))
        .collect();
    let mut value = super::super::management::library_inventory_view(serial, &drives, &s);
    if s.management["inventory"].is_null() {
        value["num_of_drives"] = Value::Null;
    }
    if let Some(library) = ctx
        .owner
        .as_ref()
        .and_then(|owner| owner.libraries.get(serial))
    {
        let ids: std::collections::BTreeSet<_> = library
            .client_addrs
            .keys()
            .copied()
            .chain(s.candidates.keys().copied())
            .chain((s.id != 0).then_some(s.id))
            .collect();
        value["nodes"] = json!(ids.into_iter().map(|id| {
            let candidate=s.candidates.get(&id);
            json!({"id":id,"http_address":library.client_addrs.get(&id).map(|address|address.split_once("/libraries/").map_or(address.as_str(),|(physical,_)|physical)),"active":value["active_node"]==id,"candidate_observed":candidate.is_some(),"advertised_eligible":candidate.map(|c|c.eligible),"drives":candidate.map(|c|c.drives)})
        }).collect::<Vec<_>>());
    }
    value["namespace"] = json!(ctx.routes[serial].route.namespace.to_string());
    value["monitor"] = s.management["monitor"].clone();
    Ok(value)
}

pub(super) async fn libraries(State(ctx): State<Arc<Context>>) -> Response {
    let mut rows = Vec::new();
    for serial in ctx.routes.keys() {
        match library_row(&ctx, serial).await {
            Ok(row) => rows.push(row),
            Err(e) => return catalog_error(e),
        }
    }
    reply(StatusCode::OK, json!({"source":"fdb","libraries":rows}))
}

async fn show_library(ctx: Arc<Context>, serial: Option<String>, request: Request) -> Response {
    let names = match selected(&ctx, serial) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if names.len() != 1 {
        return invalid("多个带库时必须指定 --library 或库路径");
    }
    let mut payload = match library_row(&ctx, &names[0]).await {
        Ok(v) => v,
        Err(e) => return catalog_error(e),
    };
    if let Some(attribute) = match query_value(&request, "attribute") {
        Ok(v) => v,
        Err(e) => return invalid(e),
    } {
        let Some(value) = payload.get(attribute) else {
            return invalid("未知带库属性");
        };
        payload = json!({attribute:value});
    }
    reply(StatusCode::OK, json!({"source":"fdb","payload":payload}))
}

pub(super) async fn library(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    show_library(ctx, Some(serial), request).await
}
pub(super) async fn default_library(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    show_library(ctx, None, request).await
}

async fn list(ctx: Arc<Context>, serial: Option<String>, request: Request) -> Response {
    let names = match selected(&ctx, serial) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(s)) => match s.parse::<usize>() {
            Ok(n) if (1..=1000).contains(&n) => n,
            _ => return invalid("磁带分页须在 1..=1000"),
        },
        Err(e) => return invalid(e),
    };
    let scopes: BTreeMap<_, _> = names
        .iter()
        .map(|s| (s.clone(), ctx.routes[s].route.namespace.to_string()))
        .collect();
    let cursor = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(s)) if s.len() <= 16384 => match URL_SAFE_NO_PAD
            .decode(s)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cursor>(&b).ok())
        {
            Some((1, bound, library, barcode))
                if bound == scopes && scopes.contains_key(&library) =>
            {
                Some((library, barcode))
            }
            _ => return invalid("磁带游标不属于当前带库或 namespace"),
        },
        _ => return invalid("磁带分页游标无效"),
    };
    let mut rows: Vec<Value> = Vec::new();
    let mut next = None;
    for serial in names {
        if cursor.as_ref().is_some_and(|(last, _)| serial < *last) {
            continue;
        }
        let after = cursor
            .as_ref()
            .filter(|(last, _)| *last == serial)
            .map(|(_, b)| b.as_str());
        let status = match snapshot(&ctx, &serial).await {
            Ok(v) => v,
            Err(e) => return catalog_error(e),
        };
        let observed = observed(&status);
        let remaining = limit - rows.len();
        let page = match ctx
            .catalog
            .active_tape_inventory(
                &serial,
                ctx.routes[&serial],
                &observed,
                after,
                remaining.max(1),
            )
            .await
        {
            Ok(p) => p,
            Err(e) => return catalog_error(e),
        };
        if remaining == 0 {
            if !page.items.is_empty() {
                let last = rows.last().expect("nonempty page");
                next = Some((
                    last["library_serial"].as_str().unwrap().to_owned(),
                    last["barcode"].as_str().unwrap().to_owned(),
                ));
                break;
            }
            continue;
        }
        for row in page.items {
            match tape_with_statistics(&ctx, &serial, &status, row).await {
                Ok(row) => rows.push(row),
                Err(error) => return catalog_error(error),
            }
        }
        if let Some(after) = page.next {
            next = Some((serial, String::from_utf8(after).expect("validated barcode")));
            break;
        }
    }
    let next = next
        .map(|(s, b)| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1u8, scopes, s, b)).unwrap()));
    reply(
        StatusCode::OK,
        json!({"source":"fdb","payload":{"tapes":rows,"limit":limit,"next":next}}),
    )
}

pub(super) async fn tapes(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    list(ctx, None, request).await
}
pub(super) async fn scoped_tapes(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    list(ctx, Some(serial), request).await
}

async fn show_tape(
    ctx: Arc<Context>,
    serial: Option<String>,
    barcode: String,
    pool: Option<Uuid>,
    request: Request,
) -> Response {
    let names = match selected(&ctx, serial) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if names.len() != 1 {
        return invalid("多个带库时必须指定 --library 或库路径");
    }
    let serial = &names[0];
    let status = match snapshot(&ctx, serial).await {
        Ok(v) => v,
        Err(e) => return catalog_error(e),
    };
    let row = match ctx
        .catalog
        .active_tape_inventory_entry(serial, ctx.routes[serial], &barcode)
        .await
    {
        Ok(Some(v)) => v,
        Ok(None) if observed(&status).contains(&barcode) => {
            json!({"barcode":barcode,"assignment":null,"pool":null})
        }
        Ok(None) => {
            return reply(
                StatusCode::NOT_FOUND,
                json!({"error":"磁带不在库存或当前归属中"}),
            );
        }
        Err(e) => return catalog_error(e),
    };
    if pool.is_some_and(|pool| row["pool"]["id"] != pool.to_string()) {
        return reply(StatusCode::NOT_FOUND, json!({"error":"磁带不属于指定池"}));
    }
    let mut payload = match tape_with_statistics(&ctx, serial, &status, row).await {
        Ok(row) => row,
        Err(error) => return catalog_error(error),
    };
    if let Some(attribute) = match query_value(&request, "attribute") {
        Ok(v) => v,
        Err(e) => return invalid(e),
    } {
        let Some(value) = payload.get(attribute) else {
            return invalid("未知磁带属性");
        };
        payload = json!({attribute:value});
    }
    reply(StatusCode::OK, json!({"source":"fdb","payload":payload}))
}
pub(super) async fn tape(
    State(ctx): State<Arc<Context>>,
    Path(barcode): Path<String>,
    request: Request,
) -> Response {
    show_tape(ctx, None, barcode, None, request).await
}
pub(super) async fn scoped_tape(
    State(ctx): State<Arc<Context>>,
    Path((serial, barcode)): Path<(String, String)>,
    request: Request,
) -> Response {
    show_tape(ctx, Some(serial), barcode, None, request).await
}

fn one(ctx: &Context, serial: Option<String>) -> Result<String, Box<Response>> {
    let names = selected(ctx, serial)?;
    if names.len() != 1 {
        return Err(Box::new(invalid("多个带库时必须指定 --library 或库路径")));
    }
    Ok(names[0].clone())
}

async fn import(
    ctx: Arc<Context>,
    serial: Option<String>,
    barcode: String,
    pool: Uuid,
    request: Request,
) -> Response {
    let serial = match one(&ctx, serial) {
        Ok(s) => s,
        Err(e) => return *e,
    };
    let (parts, body) = request.into_parts();
    let prepared = match management::body(Request::new(body)).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if prepared.value()["library"] != serial
        || prepared.value()["change"]["kind"] != "media-import"
        || prepared.value()["change"]["barcode"] != barcode
        || prepared.value()["change"]["pool"] != pool.to_string()
    {
        return invalid("导入请求与带库、条码或操作类型不符");
    }
    management::admit(
        State(ctx),
        Path(serial),
        Request::from_parts(parts, Body::from(prepared.value().to_string())),
    )
    .await
}

async fn tasks_inner(ctx: Arc<Context>, serial: Option<String>, request: Request) -> Response {
    let serial = match one(&ctx, serial) {
        Ok(s) => s,
        Err(e) => return *e,
    };
    management::tasks(State(ctx), Path(serial), request).await
}
pub(super) async fn tasks(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    tasks_inner(ctx, None, request).await
}
pub(super) async fn scoped_tasks(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    tasks_inner(ctx, Some(serial), request).await
}

async fn task_inner(
    ctx: Arc<Context>,
    serial: Option<String>,
    id: String,
    mut request: Request,
    cancel: bool,
) -> Response {
    let serial = match one(&ctx, serial) {
        Ok(s) => s,
        Err(e) => return *e,
    };
    let mut parts = id.splitn(3, ':');
    let (Some(bound), Some(epoch), Some(operation)) = (parts.next(), parts.next(), parts.next())
    else {
        return invalid("任务号格式为 namespace:epoch:UUID");
    };
    let epoch = match epoch.parse::<u64>() {
        Ok(n) if n > 0 => n,
        _ => return invalid("任务代次无效"),
    };
    let namespace = ctx.routes[&serial].route.namespace.to_string();
    if bound != namespace {
        return reply(
            StatusCode::CONFLICT,
            json!({"error":"任务不属于当前带库 namespace"}),
        );
    }
    // The public admin selector is bound to the server's fixed Active route.
    // The internal primitive still validates that route on every transaction.
    request
        .headers_mut()
        .insert("x-tapers-fdb-namespace", namespace.parse().unwrap());
    if cancel {
        management::cancel(
            State(ctx),
            Path((serial, epoch, operation.to_owned())),
            request,
        )
        .await
    } else {
        management::get_task(
            State(ctx),
            Path((serial, epoch, operation.to_owned())),
            request,
        )
        .await
    }
}
pub(super) async fn task(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    task_inner(ctx, None, id, request, false).await
}
pub(super) async fn scoped_task(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    task_inner(ctx, Some(serial), id, request, false).await
}
pub(super) async fn cancel(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    task_inner(ctx, None, id, request, true).await
}
pub(super) async fn scoped_cancel(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    task_inner(ctx, Some(serial), id, request, true).await
}

pub(super) async fn pool_scope(
    ctx: &Context,
    serial: &str,
    pool: &str,
) -> Result<Uuid, Box<Response>> {
    let Some(route) = ctx.routes.get(serial) else {
        return Err(Box::new(reply(
            StatusCode::NOT_FOUND,
            json!({"error":"带库未配置"}),
        )));
    };
    let id = Uuid::parse_str(pool)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == pool)
        .ok_or_else(|| Box::new(invalid("池 UUID 无效或不规范")))?;
    match ctx.catalog.active_pool_for_route(serial, *route, id).await {
        Ok(Some(_)) => Ok(id),
        Ok(None) => Err(Box::new(reply(
            StatusCode::NOT_FOUND,
            json!({"error":"池不属于指定带库或不存在"}),
        ))),
        Err(e) => Err(Box::new(catalog_error(e))),
    }
}

pub(super) async fn pool_tapes(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool)): Path<(String, String)>,
    request: Request,
) -> Response {
    let pool = match pool_scope(&ctx, &serial, &pool).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let route = ctx.routes[&serial];
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(v)) => match v.parse::<usize>() {
            Ok(n @ 1..=1000) => n,
            _ => return invalid("磁带分页大小须在 1..=1000"),
        },
        Err(e) => return invalid(e),
    };
    type PoolCursor = (u8, String, String, String, String);
    let after = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(s)) if s.len() <= 4096 => match URL_SAFE_NO_PAD
            .decode(s)
            .ok()
            .and_then(|b| serde_json::from_slice::<PoolCursor>(&b).ok())
        {
            Some((1, l, n, p, b))
                if l == serial
                    && n == route.route.namespace.to_string()
                    && p == pool.to_string() =>
            {
                Some(b)
            }
            _ => return invalid("磁带游标不属于指定带库、池或 namespace"),
        },
        _ => return invalid("磁带游标无效"),
    };
    let status = match snapshot(&ctx, &serial).await {
        Ok(v) => v,
        Err(e) => return catalog_error(e),
    };
    match ctx
        .catalog
        .active_pool_tape_inventory(&serial, route, pool, after.as_deref(), limit)
        .await
    {
        Ok(page) => {
            let next = page.next.map(|b| {
                URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&(
                        1u8,
                        &serial,
                        route.route.namespace.to_string(),
                        pool.to_string(),
                        String::from_utf8(b).expect("validated barcode"),
                    ))
                    .unwrap(),
                )
            });
            let mut tapes = Vec::with_capacity(page.items.len());
            for row in page.items {
                match tape_with_statistics(&ctx, &serial, &status, row).await {
                    Ok(row) => tapes.push(row),
                    Err(error) => return catalog_error(error),
                }
            }
            reply(
                StatusCode::OK,
                json!({"source":"fdb","library_serial":serial,"namespace":route.route.namespace.to_string(),"pool":pool.to_string(),"payload":{"tapes":tapes,"limit":limit,"next":next}}),
            )
        }
        Err(e) => catalog_error(e),
    }
}
pub(super) async fn pool_tape(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let pool = match pool_scope(&ctx, &serial, &pool).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    show_tape(ctx, Some(serial), barcode, Some(pool), request).await
}
pub(super) async fn pool_import(
    State(ctx): State<Arc<Context>>,
    Path((serial, pool, barcode)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let pool = match pool_scope(&ctx, &serial, &pool).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    import(ctx, Some(serial), barcode, pool, request).await
}
