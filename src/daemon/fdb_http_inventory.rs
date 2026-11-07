//! Inventory reads join cached physical observations and current FDB ownership.
//! No query in this module opens a device or queues an executor request.
use super::*;
use uuid::Uuid;

type Cursor = (u8, BTreeMap<String, String>, String, String);

pub(super) async fn snapshot(
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
        "offline":assignment["online"].as_bool().map(|v|!v),
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

pub(super) async fn enrich_tape(
    ctx: &Context,
    serial: &str,
    status: &super::super::node::NodeStatus,
    row: &mut Value,
) -> Result<(), CatalogError> {
    let presentation = tape_with_statistics(ctx, serial, status, row.clone()).await?;
    row["location_kind"] = presentation["location"].clone();
    for (key, value) in presentation.as_object().expect("tape view") {
        if !row.as_object().expect("tape row").contains_key(key) {
            row[key] = value.clone();
        }
    }
    Ok(())
}
pub(super) async fn tape_with_statistics(
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

async fn list(ctx: Arc<Context>, serial: Option<String>, request: Request) -> Response {
    let names = match selected(&ctx, serial) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(s)) => match s.parse::<usize>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return invalid("磁带分页须在 1..=100"),
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
    let mut availability = BTreeMap::new();
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
        let remaining = limit - rows.len();
        let mut page = match ctx
            .catalog
            .active_tape_page_for_route(&serial, ctx.routes[&serial], None, after, remaining.max(1))
            .await
        {
            Ok(page) => page,
            Err(error) => return catalog_error(error),
        };
        availability.insert(serial.clone(), page["inventory_available"].clone());
        if remaining == 0 {
            if !page["items"].as_array().expect("tape page").is_empty() {
                let last = rows.last().expect("nonempty page");
                next = Some((
                    last["library_serial"].as_str().unwrap().to_owned(),
                    last["barcode"].as_str().unwrap().to_owned(),
                ));
                break;
            }
            continue;
        }
        for mut row in page["items"].as_array_mut().expect("tape page").drain(..) {
            if let Err(error) = enrich_tape(&ctx, &serial, &status, &mut row).await {
                return catalog_error(error);
            }
            rows.push(row);
        }
        if let Some(after) = page["next"].as_str() {
            next = Some((serial, after.to_owned()));
            break;
        }
    }
    let next = next
        .map(|(s, b)| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(1u8, scopes, s, b)).unwrap()));
    reply(
        StatusCode::OK,
        json!({"version":1,"items":rows,"limit":limit,"next":next,"inventory_available":availability}),
    )
}

pub(super) async fn tapes(State(ctx): State<Arc<Context>>, request: Request) -> Response {
    list(ctx, None, request).await
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
