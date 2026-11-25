//! 本地控制观察 allowlist；无业务分派、代理或转发入口。
use super::*;

pub fn router(
    config: LibrariesConfig,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
) -> Router {
    Router::new().fallback(dispatch).with_state(Gateway {
        config,
        libraries,
        gateway,
        peers,
        requests: Arc::new(Semaphore::new(32)),
    })
}
async fn dispatch(State(g): State<Gateway>, req: Request) -> Response {
    let deny = || {
        response(
            403,
            json!({"error":"control_only","detail":"显式无业务控制模式拒绝此接口","business_ready":false}),
        )
    };
    if req.method() != "GET"
        || req.uri().path().contains('%')
        || req.uri().path().contains("//")
        || req.headers().contains_key(HOP)
    {
        return deny();
    }
    let path_owned = req.uri().path().to_owned();
    let path = path_owned.as_str();
    if path == "/readyz" {
        return response(503, json!({"mode":"control_only","business_ready":false}));
    }
    if matches!(path, "/cluster" | "/admin/libraries") && req.uri().query().is_none() {
        let status = g
            .gateway
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let libraries: Vec<_> = g.config.libraries.iter().map(|library| {
            let s = g.libraries[&library.changer_serial].status.lock().unwrap_or_else(|e|e.into_inner()).clone();
            json!({"library_serial":library.changer_serial,"id":s.id,"leader":s.leader,"term":s.term,"role":s.role,
                "applied_index":s.applied_index,"catalog_quiesce_index":s.catalog_quiesce_index,"catalog_frozen_index":s.catalog_frozen_index,
                "control_executor":s.executor.as_ref().map(|e|json!({"node":e.node,"round":e.round,"fenced":e.fenced})),
                "historical_executor_ready":s.executor_ready,"business_ready":false})
        }).collect();
        return response(
            200,
            json!({"mode":"control_only","id":status.id,"leader":status.leader,"term":status.term,
            "role":status.role,"applied_index":status.applied_index,"business_ready":false,"libraries":libraries}),
        );
    }
    let Some(rest) = path.strip_prefix("/libraries/") else {
        return deny();
    };
    let Some((serial, suffix)) = rest.split_once('/') else {
        return deny();
    };
    let Some(ctx) = g.libraries.get(serial).cloned() else {
        return deny();
    };
    if !ctx.files.is_control_only() {
        return response(503, json!({"error":"control_binding_unavailable"}));
    }
    match suffix {
        "admin/catalog/native-barrier" => {
            native_barrier::respond(serial, &g.peers, ctx, g.requests, req).await
        }
        "admin/catalog/current" | "admin/catalog/audit" if req.uri().query().is_none() => {
            let current = suffix.ends_with("current");
            let Ok(permit) = g.requests.try_acquire_owned() else {
                return response(503, json!({"error":"http_busy"}));
            };
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let before = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                ctx.files.verify_control_only_status(&before)?;
                let audit = ctx.files.catalog_cutover_audit()?;
                let after = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                ctx.files.verify_control_only_status(&after)?;
                Ok::<_,crate::error::TapeError>(if current { ctx.files.control_only_description().unwrap_or(serde_json::Value::Null) }
                    else { json!({"mode":"control_only","applied_index":audit.applied_index,"frozen_index":audit.frozen_index,
                        "has_staging":audit.has_staging,"has_unresolved_file_tasks":audit.has_unresolved_file_tasks,"has_catalog_rows":audit.has_catalog_rows}) })
            }).await {
                Ok(Ok(value)) => response(200,value),
                Ok(Err(error)) => response(503,json!({"error":"control_binding_unavailable","detail":error.to_string()})),
                Err(error) => response(503,json!({"error":"control_binding_unavailable","detail":error.to_string()})),
            }
        }
        _ => deny(),
    }
}
