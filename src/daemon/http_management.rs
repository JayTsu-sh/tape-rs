//! 管理资源路由；查询只读已应用状态，操作送入 Raft。
use super::*;
use crate::daemon::management as model;

pub(super) fn handle(
    method: &str,
    path: &str,
    length: Option<u64>,
    reader: &mut dyn Read,
    conn: &mut Response,
    ctx: &HttpContext,
) -> std::io::Result<bool> {
    if !path.starts_with("/admin/") {
        return Ok(false);
    }
    let pieces: Vec<&str> = path.trim_start_matches("/admin/").split('/').collect();
    let resource = pieces[0];
    let target = pieces.get(1).copied().unwrap_or("");
    let action = pieces.get(2).copied().unwrap_or("");
    if pieces.len() > 3
        || pieces
            .iter()
            .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains('\0'))
    {
        respond_json(conn, 404, "", json!({"error":"not_found"}))?;
        return Ok(true);
    }
    if method == "GET" {
        // 原有池列表保留字段和容量计算。
        if path == "/admin/pools" {
            return Ok(false);
        }
        let s = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if resource == "catalog" && target == "current" && action == "audit" {
            match ctx.files.catalog_cutover_audit() {
                Ok(audit) => respond_json(
                    conn,
                    200,
                    "",
                    json!({"source":"local_sqlite","answered_by":s.id,
                        "control_applied_index":s.applied_index,
                        "directory_applied_index":audit.applied_index,
                        "quiesce_index":s.catalog_quiesce_index,
                        "frozen_index":s.catalog_frozen_index,
                        "projected_frozen_index":audit.frozen_index,
                        "executor_present":s.executor.is_some(),
                        "catalog_pending":s.catalog_pending,
                        "has_staging":audit.has_staging,
                        "has_unresolved_file_tasks":audit.has_unresolved_file_tasks,
                        "frozen_projection_caught_up":s.catalog_frozen_index.is_some_and(|index| audit.frozen_index == Some(index) && audit.applied_index >= index)}),
                )?,
                Err(e) => respond_json(
                    conn,
                    503,
                    "",
                    json!({"error":"catalog_unavailable","detail":e.to_string()}),
                )?,
            }
            return Ok(true);
        }
        let m = if s.management.is_null() {
            model::empty()
        } else {
            (*s.management).clone()
        };
        let value = match resource {
            "catalog" if target == "current" && action.is_empty() => Some(
                json!({"authority":"raft_sqlite","quiesce_index":s.catalog_quiesce_index,"frozen_index":s.catalog_frozen_index}),
            ),
            "status" if target.is_empty() => {
                let policies: serde_json::Map<String, Value> = m["tapes"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|b| (b.clone(), model::tape_policy(&m, b)))
                    .collect();
                Some(
                    json!({"inventory":m["inventory"],"inventory_observation":model::inventory_observation(&s),"pools":s.pools.iter().map(|p|model::pool_view(&s,p)).collect::<Vec<_>>(),"tapes":model::tape_views(&s),"drives":model::drive_views(&s),"tape_policies":policies,"drive_policies":m["drives"],"nodegroups":m["nodegroups"],"nodes":m["nodes"]}),
                )
            }
            "tasks" if action.is_empty() => {
                if target.is_empty() {
                    Some(Value::Array(
                        m["tasks"].as_object().unwrap().values().cloned().collect(),
                    ))
                } else {
                    m["tasks"].get(target).cloned()
                }
            }
            "libraries" if action.is_empty() && (target.is_empty() || target == "current") => Some(
                json!({"scope":"cluster_device_group","inventory":m["inventory"],"inventory_observation":model::inventory_observation(&s),"executor":s.executor.as_ref().map(|e|json!({"node":e.node,"round":e.round})),"executor_ready":s.executor_ready,"catalog_quiesce_index":s.catalog_quiesce_index,"catalog_frozen_index":s.catalog_frozen_index}),
            ),
            "nodegroups" if action.is_empty() => {
                if target.is_empty() {
                    Some(m["nodegroups"].clone())
                } else {
                    m["nodegroups"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|g| *g == target)
                        .map(|_| json!({"name":target,"nodes":m["nodes"],"drives":m["drives"]}))
                }
            }
            "nodes" if action.is_empty() => {
                let rows: Vec<Value> = s.candidates.iter().map(|(id,c)|json!({"id":id,"instance":c.instance,"device_group":c.group,"drives":c.drives,"eligible":c.eligible,"nodegroup":m["nodes"].get(id.to_string()).cloned().unwrap_or(json!("G0"))})).collect();
                if target.is_empty() {
                    Some(json!(rows))
                } else {
                    rows.into_iter().find(|r| {
                        r["id"].as_u64().map(|id| id.to_string()).as_deref() == Some(target)
                    })
                }
            }
            "drives" if action.is_empty() => {
                let rows = model::drive_views(&s);
                if target.is_empty() {
                    Some(
                        json!({"drives":rows,"policies":m["drives"],"observed_at":m["inventory"]["observed_at"],"owner":m["inventory"]["owner"],"round":m["inventory"]["round"],"inventory_observation":model::inventory_observation(&s)}),
                    )
                } else {
                    rows.into_iter().find(|d| d["serial"] == target)
                }
            }
            "tapes" if action.is_empty() => {
                let rows = model::tape_views(&s);
                if target.is_empty() {
                    Some(
                        json!({"tapes":rows,"inventory_observed_at":m["inventory"]["observed_at"],"inventory_observation":model::inventory_observation(&s)}),
                    )
                } else {
                    rows.into_iter().find(|t| t["barcode"] == target)
                }
            }
            "pools" if !target.is_empty() && action.is_empty() => s
                .pools
                .iter()
                .find(|p| p.name == target || p.uuid == target)
                .map(|p| model::pool_view(&s, p)),
            _ => None,
        };
        match value {
            Some(value) => respond_json(
                conn,
                200,
                "",
                json!({"source":"raft_applied","applied_index":s.applied_index,"answered_by":s.id,"payload":value}),
            )?,
            None => respond_json(
                conn,
                404,
                "",
                json!({"error":"not_found","detail":"管理资源不存在，设备清单须先显式 rescan"}),
            )?,
        }
        return Ok(true);
    }
    if method == "POST" && resource == "catalog" && target == "current" && action == "export" {
        if length.unwrap_or(0) != 0 {
            respond_json(
                conn,
                400,
                "",
                json!({"error":"bad_request","detail":"目录快照导出不接受请求体"}),
            )?;
            return Ok(true);
        }
        let before = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(frozen_index) = before.catalog_frozen_index else {
            respond_json(conn, 409, "", json!({"error":"catalog_not_frozen"}))?;
            return Ok(true);
        };
        if before.catalog_pending || before.catalog_quiesce_index.is_none() {
            respond_json(conn, 409, "", json!({"error":"catalog_not_ready"}))?;
            return Ok(true);
        }
        let export = match ctx.files.catalog_export_frozen_snapshot(frozen_index) {
            Ok(export) => export,
            Err(error) => {
                respond_json(
                    conn,
                    503,
                    "",
                    json!({"error":"catalog_export_unavailable","detail":error.to_string()}),
                )?;
                return Ok(true);
            }
        };
        let after = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if after.catalog_frozen_index != Some(frozen_index)
            || after.catalog_quiesce_index != before.catalog_quiesce_index
            || after.catalog_pending
            || after.id != before.id
        {
            respond_json(conn, 409, "", json!({"error":"catalog_freeze_changed"}))?;
            return Ok(true);
        }
        respond_catalog_snapshot(conn, export, after.id)?;
        return Ok(true);
    }
    let op = match (method, resource, action) {
        ("POST", "catalog", "quiesce" | "resume" | "freeze") if target == "current" => Some(action),
        ("PATCH", "pools", "") if !target.is_empty() => Some("set"),
        ("DELETE", "pools", "") if !target.is_empty() => Some("delete"),
        ("POST", "nodegroups", "") if !target.is_empty() => Some("create"),
        ("DELETE", "nodegroups", "") if !target.is_empty() => Some("delete"),
        ("POST", "tasks", "") if target == "clearhistory" => Some("clearhistory"),
        ("POST", "tasks", "cancel") => Some("cancel"),
        ("POST", "libraries", "rescan") if target == "current" => Some("rescan"),
        (
            "POST",
            "tapes",
            "assign" | "unassign" | "offline" | "online" | "blacklist" | "clear-blacklist" | "move"
            | "validate" | "reconcile" | "objects" | "restore",
        ) => Some(action),
        ("POST", "drives", "up" | "down" | "role" | "assign") => Some(action),
        ("POST", "nodes", "assign") => Some("assign"),
        _ => None,
    };
    if let Some(op) = op {
        let len = length.unwrap_or(0);
        if len > 16384 {
            respond_json(
                conn,
                413,
                "",
                json!({"error":"too_large","detail":"管理请求体最多 16 KiB"}),
            )?;
            return Ok(true);
        }
        let mut bytes = vec![0; len as usize];
        reader.read_exact(&mut bytes)?;
        let params = if bytes.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(p) if p.is_object() => p,
                _ => {
                    respond_json(
                        conn,
                        400,
                        "",
                        json!({"error":"bad_json","detail":"请求体必须是 JSON 对象"}),
                    )?;
                    return Ok(true);
                }
            }
        };
        let command = if resource == "catalog" {
            if params["confirm"] != true {
                respond_json(
                    conn,
                    400,
                    "",
                    json!({"error":"confirmation_required","detail":"目录切换操作须显式 confirm=true"}),
                )?;
                return Ok(true);
            }
            if op == "quiesce" {
                Command::CatalogQuiesce
            } else {
                let Some(quiesce_index) = params["quiesce_index"].as_u64().filter(|i| *i > 0)
                else {
                    respond_json(
                        conn,
                        400,
                        "",
                        json!({"error":"bad_request","detail":"恢复或冻结旧目录须提供有效的 quiesce_index"}),
                    )?;
                    return Ok(true);
                };
                if op == "freeze" {
                    Command::CatalogFreeze { quiesce_index }
                } else {
                    Command::CatalogResume { quiesce_index }
                }
            }
        } else {
            Command::Manage {
                request: model::request(resource, op, target, params),
            }
        };
        admin(conn, ctx, command)?;
        return Ok(true);
    }
    // 保留已有创建/归属/回收接口，且不让未知动作落入旧的前缀匹配。
    if pieces.len() == 2
        && ((resource == "pools" && method == "POST")
            || (resource == "tapes" && ["POST", "DELETE"].contains(&method)))
        || pieces.len() == 3 && resource == "tapes" && action == "reclaim" && method == "POST"
    {
        return Ok(false);
    }
    respond_json(
        conn,
        404,
        "",
        json!({"error":"not_found","detail":"未知管理端点或方法"}),
    )?;
    Ok(true)
}
