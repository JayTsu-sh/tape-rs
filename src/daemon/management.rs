//! 集群管理状态：只应用已提交决策，不在 Raft 线程访问设备。
use super::state::ControlState;
use serde_json::{Value, json};

pub const MAX_TASKS: usize = 1024;
pub const MAX_STATE: usize = 2 << 20;
pub const MAX_RESULT: usize = 256 << 10;

pub fn empty() -> Value {
    json!({"pools":{},"tapes":{},"drives":{},"nodegroups":["G0"],"nodes":{},"tasks":{},"inventory":null})
}

pub fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
        .ok_or_else(|| format!("缺少或无效的字段：{key}"))
}
fn name(s: &str) -> Result<(), String> {
    if (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(())
    } else {
        Err("名称须为 1–16 个 ASCII 字母、数字或下划线".into())
    }
}
fn group_exists(m: &Value, group: &str) -> Result<(), String> {
    if m["nodegroups"]
        .as_array()
        .is_some_and(|a| a.iter().any(|g| g == group))
    {
        Ok(())
    } else {
        Err(format!("节点组不存在：{group}"))
    }
}
pub fn request(resource: &str, action: &str, target: &str, params: Value) -> Value {
    json!({"resource":resource,"action":action,"target":target,"params":params})
}

/// 拒绝必须保持整个状态不变；验证和更新在影子状态上完成。
pub fn apply(ctl: &mut ControlState, index: u64, req: &Value) -> Result<String, String> {
    let mut next = ctl.clone();
    if next.management.is_null() {
        next.management = empty();
    }
    let reply = apply_inner(&mut next, index, req)?;
    next.management["revision"] = json!(index);
    if next.management.to_string().len() > MAX_STATE {
        return Err("管理状态容量已满，请清理终态任务历史".into());
    }
    *ctl = next;
    Ok(reply.to_string())
}

fn apply_inner(c: &mut ControlState, index: u64, r: &Value) -> Result<Value, String> {
    let resource = text(r, "resource")?;
    let action = text(r, "action")?;
    let target = r["target"].as_str().ok_or("缺少 target")?;
    if target.len() > 255 || target.contains(['/', '\0']) {
        return Err("资源标识非法".into());
    }
    let p = &r["params"];
    if !p.is_object() {
        return Err("params 必须是对象".into());
    }
    match (resource, action) {
        ("pools", "delete") => {
            let uuid = c.resolve_pool(target).ok_or("池不存在")?;
            if c.tapes.values().any(|u| *u == uuid) {
                return Err("池仍有磁带，不能删除".into());
            }
            if active_tasks(&c.management) {
                return Err("存在未结束管理任务".into());
            }
            c.pools.remove(&uuid);
            c.management["pools"].as_object_mut().unwrap().remove(&uuid);
            return Ok(json!({"uuid":uuid,"deleted":true}));
        }
        ("pools", "set") => {
            let mut shadow = c.clone();
            pool_set(&mut shadow, target, p)?;
        }
        ("nodegroups", "create") => {
            name(target)?;
            if group_exists(&c.management, target).is_ok() {
                return Err("节点组已存在".into());
            }
            c.management["nodegroups"]
                .as_array_mut()
                .unwrap()
                .push(json!(target));
            return Ok(json!({"name":target}));
        }
        ("nodegroups", "delete") => {
            group_exists(&c.management, target)?;
            if target == "G0"
                || active_tasks(&c.management)
                || ["pools", "drives"].iter().any(|key| {
                    c.management[*key]
                        .as_object()
                        .unwrap()
                        .values()
                        .any(|v| v["nodegroup"] == target)
                })
                || c.management["nodes"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|v| v == target)
            {
                return Err("节点组仍被使用或存在管理任务".into());
            }
            c.management["nodegroups"]
                .as_array_mut()
                .unwrap()
                .retain(|g| g != target);
            return Ok(json!({"deleted":true}));
        }
        ("tasks", "cancel") => {
            let task = c.management["tasks"].get_mut(target).ok_or("任务不存在")?;
            if task["state"] == "queued" {
                task["state"] = json!("cancelled");
            }
            if task["state"] == "running" || task["state"] == "cancelled" {
                task["cancel_requested"] = json!(true);
            }
            return Ok(task.clone());
        }
        ("tasks", "clearhistory") => {
            if p["confirm"] != true {
                return Err("清理任务历史须显式 confirm=true".into());
            }
            let tasks = c.management["tasks"].as_object_mut().unwrap();
            let before = tasks.len();
            tasks.retain(|_, t| ["queued", "running"].iter().any(|s| t["state"] == *s));
            return Ok(json!({"cleared":before-tasks.len()}));
        }
        _ => {}
    }
    validate_job(c, resource, action, target, p)?;
    if c.executor.as_ref().is_none_or(|e| !e.fenced) || !c.executor_ready {
        return Err("设备 owner 尚未就绪".into());
    }
    if c.management.to_string().len() + 2 * MAX_RESULT + r.to_string().len() + 16384 > MAX_STATE {
        return Err("管理结果预留空间不足，请清理终态历史".into());
    }
    if c.management["tasks"].as_object().unwrap().len() >= MAX_TASKS {
        return Err("管理任务历史已满，请显式清理终态历史".into());
    }
    // 一个集群设备组一次只接收一个管理任务，避免在途配置与任务前提相互覆盖。
    if active_tasks(&c.management) {
        return Err("已有管理任务执行中".into());
    }
    if c.tape_state
        .values()
        .any(|s| s == "reclaiming" || s == "reformatting")
    {
        return Err("回收执行期间不能启动其他管理任务".into());
    }
    c.management["tasks"][index.to_string()] = json!({"id":index,"request":r,"state":"queued","progress":0,"cancel_requested":false,"round":null,"owner":null,"result":null,"created_index":index});
    Ok(json!({"task":index,"status":"queued"}))
}

fn pool_set(c: &mut ControlState, target: &str, p: &Value) -> Result<Value, String> {
    let uuid = c.resolve_pool(target).ok_or("池不存在")?;
    let attribute = text(p, "attribute")?;
    let value = text(p, "value")?;
    match attribute {
        "poolname" => {
            name(value)?;
            if c.pools
                .iter()
                .any(|(u, pool)| u != &uuid && pool.name == value)
            {
                return Err("池名称已存在".into());
            }
            c.pools.get_mut(&uuid).unwrap().name = value.into();
        }
        "file_limit" => {
            let limit = value
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 && *n <= i64::MAX as u64)
                .ok_or("file_limit 必须为正整数")?;
            c.pools.get_mut(&uuid).unwrap().file_limit = limit;
        }
        "mediarestriction" => {
            if ![
                "none", "L5", "L6", "L7", "L8", "L9", "M8", "JB", "JC", "JD", "JE", "JY", "JZ",
                "JV", "JK", "JL", "JM", "JF",
            ]
            .contains(&value)
            {
                return Err("不支持的介质限制".into());
            }
        }
        "mountlimit" | "lowspacewarningthreshold" => {
            value
                .parse::<u16>()
                .ok()
                .filter(|n| *n <= 32767)
                .ok_or("属性必须为 0–32767 的整数")?;
        }
        "lowspacewarningenable" | "nospacewarningenable" => {
            if !["yes", "no"].contains(&value) {
                return Err("属性必须为 yes 或 no".into());
            }
        }
        "nodegroup" => {
            group_exists(&c.management, value)?;
        }
        _ => return Err("不支持的池属性".into()),
    }
    if c.management["pools"][&uuid].is_null() {
        c.management["pools"][&uuid] = json!({});
    }
    c.management["pools"][&uuid][attribute] = json!(value);
    Ok(json!({"uuid":uuid,"name":c.pools[&uuid].name,"file_limit":c.pools[&uuid].file_limit}))
}

fn validate_job(
    c: &ControlState,
    resource: &str,
    action: &str,
    target: &str,
    p: &Value,
) -> Result<(), String> {
    if resource == "tapes"
        && [
            "assign",
            "online",
            "validate",
            "reconcile",
            "objects",
            "restore",
        ]
        .contains(&action)
    {
        require_data_tape(&c.management, target)?;
    }
    match (resource, action) {
        ("libraries", "rescan") | ("pools", "set") => {}
        ("tapes", "assign") => {
            if c.tapes.contains_key(target) {
                return Err("磁带已经分配".into());
            }
            let pool = c.resolve_pool(text(p, "pool")?).ok_or("池不存在")?;
            check_media(
                c.management["pools"][&pool]["mediarestriction"]
                    .as_str()
                    .unwrap_or("none"),
                target,
            )?;
        }
        (
            "tapes",
            "unassign" | "offline" | "online" | "blacklist" | "clear-blacklist" | "move"
            | "validate" | "reconcile" | "objects" | "restore",
        ) => {
            if !c.tapes.contains_key(target) {
                return Err("磁带尚未分配".into());
            }
            match action {
                "online" => {
                    if c.management["tapes"][target]["offline"].is_null() {
                        return Err("磁带当前不是离线状态".into());
                    }
                }
                "offline" => {
                    text(p, "message")?;
                }
                "blacklist" => {
                    text(p, "reason")?;
                }
                "move" => {
                    let location = text(p, "location")?;
                    if location != "homeslot" && location != "ieslot" {
                        return Err("location 必须为 homeslot 或 ieslot".into());
                    }
                    if location == "ieslot" && c.management["tapes"][target]["offline"].is_null() {
                        return Err("移动至 I/E 前必须先 offline".into());
                    }
                }
                "objects" => {
                    if p.get("offset")
                        .is_some_and(|v| v.as_u64().is_none_or(|n| n > usize::MAX as u64))
                        || p.get("limit")
                            .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 500))
                    {
                        return Err("offset 必须为非负整数，limit 须为 1–500".into());
                    }
                }
                "restore" => {
                    let dest = std::path::Path::new(text(p, "destination")?);
                    if !dest.is_absolute() {
                        return Err("恢复目的地必须是 owner 上的绝对路径".into());
                    }
                    if p["confirm"] != true {
                        return Err("恢复到 owner 文件系统须显式 confirm=true".into());
                    }
                }
                "validate" if p.get("full_read").is_some_and(|v| !v.is_boolean()) => {
                    return Err("full_read 必须为布尔值".into());
                }
                _ => {}
            }
        }
        ("drives", "up" | "down" | "role" | "assign") => {
            if target.is_empty() {
                return Err("驱动器序列号为空".into());
            }
            let owner = c
                .executor
                .as_ref()
                .filter(|e| e.fenced)
                .ok_or("设备 owner 尚未就绪")?;
            let inventory = &c.management["inventory"];
            if inventory["owner"].as_u64() != Some(owner.node)
                || inventory["round"].as_u64() != Some(owner.round)
            {
                return Err("驱动库存已过期，请先显式 rescan".into());
            }
            if !inventory["drives"].as_array().is_some_and(|drives| {
                drives
                    .iter()
                    .any(|drive| drive["serial"].as_str() == Some(target))
            }) {
                return Err("驱动器不在当前库存中".into());
            }
            if action == "role" {
                let role = text(p, "role")?;
                let mut seen = std::collections::BTreeSet::new();
                if !role.chars().all(|r| "mrg".contains(r) && seen.insert(r)) {
                    return Err("角色必须为无重复的 m/r/g 组合".into());
                }
            }
            if action == "assign" {
                let group = text(p, "group")?;
                if group != "-" {
                    group_exists(&c.management, group)?;
                }
            }
        }
        ("nodes", "assign") => {
            let node = target.parse::<u64>().map_err(|_| "节点编号非法")?;
            if !c.candidates.contains_key(&node)
                && c.executor.as_ref().is_none_or(|e| e.node != node)
            {
                return Err("节点不存在".into());
            }
            let group = text(p, "group")?;
            if group != "-" {
                group_exists(&c.management, group)?;
            }
        }
        _ => return Err("未知管理操作".into()),
    }
    Ok(())
}

pub fn active_tasks(m: &Value) -> bool {
    m["tasks"].as_object().is_some_and(|tasks| {
        tasks
            .values()
            .any(|t| t["state"] == "queued" || t["state"] == "running")
    })
}
pub fn check_media(restriction: &str, barcode: &str) -> Result<(), String> {
    if restriction == "none"
        || barcode
            .strip_suffix(restriction)
            .is_some_and(|s| s.chars().count() == 6)
    {
        Ok(())
    } else {
        Err("条码不符合池介质限制".into())
    }
}

pub fn interrupt(c: &mut ControlState) {
    if let Some(tasks) = c.management.get_mut("tasks").and_then(Value::as_object_mut) {
        let mut changed = false;
        for task in tasks.values_mut().filter(|t| t["state"] == "running") {
            changed = true;
            task["state"] = json!("indeterminate");
            task["result"] =
                json!({"detail":"执行轮次已结束，须核实设备及目的地状态；不会自动重放"});
        }
        if changed {
            c.management["revision"] = json!(c.applied_index);
        }
    }
}

pub fn report(c: &mut ControlState, id: u64, round: u64, phase: &str, result: &Value) -> bool {
    let Some(owner) = c
        .executor
        .as_ref()
        .filter(|e| e.round == round && e.fenced)
        .map(|e| e.node)
    else {
        return false;
    };
    let key = id.to_string();
    let Some(task) = c.management["tasks"].get(&key) else {
        return false;
    };
    if phase == "running" && task["state"] == "queued" {
        let task = &mut c.management["tasks"][&key];
        task["state"] = json!("running");
        task["round"] = json!(round);
        task["owner"] = json!(owner);
        c.management["revision"] = json!(c.applied_index);
        return false;
    }
    if phase == "progress" && task["state"] == "running" && task["round"] == round {
        if let Some(progress) = result["progress"].as_u64().filter(|n| *n <= 100) {
            c.management["tasks"][&key]["progress"] = json!(progress);
            c.management["revision"] = json!(c.applied_index);
        }
        return false;
    }
    if task["state"] != "running"
        || task["round"] != round
        || !["success", "failed", "cancelled"].contains(&phase)
    {
        return false;
    }
    let r = task["request"].clone();
    if phase == "success"
        && let Err(detail) = finish_config(c, &r, result)
    {
        let task = &mut c.management["tasks"][&key];
        task["state"] = json!("failed");
        task["result"] = json!({"detail":detail});
        c.management["revision"] = json!(c.applied_index);
        return false;
    }
    let task = &mut c.management["tasks"][&key];
    task["state"] = json!(phase);
    if phase == "success" {
        task["progress"] = json!(100);
    }
    task["finished_index"] = json!(c.applied_index);
    task["result"] = result.clone();
    c.management["revision"] = json!(c.applied_index);
    phase == "success"
}

/// 目录变更由状态机的已应用结果产生，不把设备报告中的任意 JSON 当作写库指令。
pub fn directory_changes(c: &ControlState, id: u64) -> Value {
    let r = &c.management["tasks"][id.to_string()]["request"];
    let target = r["target"].as_str().unwrap_or("");
    match (r["resource"].as_str(), r["action"].as_str()) {
        (Some("pools"), Some("set")) => {
            let name = if r["params"]["attribute"] == "poolname" {
                r["params"]["value"].as_str().unwrap_or(target)
            } else {
                target
            };
            c.resolve_pool(name).map(|uuid|json!({"pool_update":{"uuid":uuid,"name":c.pools[&uuid].name,"file_limit":c.pools[&uuid].file_limit}})).unwrap_or(json!({}))
        }
        (Some("tapes"), Some("assign")) => c
            .tapes
            .get(target)
            .map(|uuid| json!({"assignment":{"barcode":target,"pool_uuid":uuid}}))
            .unwrap_or(json!({})),
        (Some("tapes"), Some("unassign")) => json!({"unassigned":target}),
        _ => json!({}),
    }
}

fn finish_config(c: &mut ControlState, r: &Value, result: &Value) -> Result<(), String> {
    let resource = r["resource"].as_str().unwrap();
    let action = r["action"].as_str().unwrap();
    let target = r["target"].as_str().unwrap();
    let p = &r["params"];
    if let Some(observation) = result.get("inventory") {
        c.management["inventory"] = observation.clone();
    }
    if resource == "tapes" && result.get("home_address").is_some() {
        if c.management["tapes"][target].is_null() {
            c.management["tapes"][target] = json!({});
        }
        c.management["tapes"][target]["home_address"] = result["home_address"].clone();
    }
    match (resource, action) {
        ("pools", "set") => {
            pool_set(c, target, p)?;
        }
        ("tapes", "assign") => {
            if c.management["tapes"][target].is_null() {
                c.management["tapes"][target] = json!({});
            }
            c.management["tapes"][target]["home_address"] = result["home_address"].clone();
            if let Some(pool) = c.resolve_pool(p["pool"].as_str().unwrap()) {
                c.tapes.insert(target.into(), pool);
            }
        }
        ("tapes", "unassign") => {
            c.tapes.remove(target);
            c.tape_summary.remove(target);
            c.tape_state.remove(target);
            let saved = c.management["tapes"][target].clone();
            c.management["tapes"]
                .as_object_mut()
                .unwrap()
                .remove(target);
            if !saved["capacity"].is_null() || !saved["index_capacity"].is_null() {
                c.management["tapes"][target] = json!({"capacity":saved["capacity"],"capacity_refresh":saved["capacity_refresh"],"index_capacity":saved["index_capacity"],"index_capacity_refresh":saved["index_capacity_refresh"]});
            }
        }
        ("tapes", "offline" | "online" | "blacklist" | "clear-blacklist") => {
            if c.management["tapes"][target].is_null() {
                c.management["tapes"][target] = json!({});
            }
            let (key, value) = match action {
                "offline" => ("offline", p["message"].clone()),
                "online" => ("offline", Value::Null),
                "blacklist" => ("blacklist", p["reason"].clone()),
                _ => ("blacklist", Value::Null),
            };
            c.management["tapes"][target][key] = value;
        }
        ("drives", _) => {
            if c.management["drives"][target].is_null() {
                c.management["drives"][target] = json!({});
            }
            let (key, value) = match action {
                "up" => ("enabled", json!(true)),
                "down" => ("enabled", json!(false)),
                "role" => ("role", p["role"].clone()),
                _ => (
                    "nodegroup",
                    if p["group"] == "-" {
                        Value::Null
                    } else {
                        p["group"].clone()
                    },
                ),
            };
            c.management["drives"][target][key] = value;
        }
        ("nodes", "assign") => {
            c.management["nodes"][target] = if p["group"] == "-" {
                Value::Null
            } else {
                p["group"].clone()
            };
        }
        _ => {}
    }
    Ok(())
}

pub fn cleaning_source(m: &Value, barcode: &str) -> Option<&'static str> {
    let medium_type = m["inventory"]["volumes"]
        .as_array()
        .and_then(|rows| rows.iter().find(|v| v["barcode"] == barcode))
        .and_then(|v| v["medium_type"].as_u64())
        .and_then(|v| u8::try_from(v).ok());
    crate::changer::element::cleaning_source(medium_type, Some(barcode))
}

pub fn require_data_tape(m: &Value, barcode: &str) -> Result<(), String> {
    if cleaning_source(m, barcode).is_some() {
        Err("清洗带不能分配到数据池或执行 LTFS 数据操作".into())
    } else {
        Ok(())
    }
}

pub fn tape_allowed(m: &Value, barcode: &str) -> bool {
    cleaning_source(m, barcode).is_none()
        && m["tapes"][barcode]["offline"].is_null()
        && m["tapes"][barcode]["blacklist"].is_null()
}
pub fn drive_allowed(m: &Value, serial: &str, node: u64, pool: &str, role: char) -> bool {
    let d = &m["drives"][serial];
    let node_key = node.to_string();
    let node_group = m["nodes"].get(&node_key).unwrap_or(&Value::Null);
    let group = m["pools"][pool]["nodegroup"].as_str().unwrap_or("G0");
    d["enabled"] != false
        && d["role"].as_str().unwrap_or("mrg").contains(role)
        && d.get("nodegroup").map_or(Some("G0"), Value::as_str) == Some(group)
        && (m["nodes"].get(&node_key).is_none() || node_group.as_str() == Some(group))
}

pub fn drive_policy(m: &Value, serial: &str) -> Value {
    let d = &m["drives"][serial];
    json!({"enabled":d["enabled"]!=false,"role":d["role"].as_str().unwrap_or("mrg"),"nodegroup":d.get("nodegroup").cloned().unwrap_or(json!("G0"))})
}
pub fn pool_options(m: &Value, uuid: &str) -> Value {
    let mut options = json!({"mediarestriction":"none","mountlimit":"0","nodegroup":"G0","lowspacewarningenable":"yes","lowspacewarningthreshold":"0","nospacewarningenable":"yes"});
    if let Some(saved) = m["pools"][uuid].as_object() {
        for (k, v) in saved {
            options[k] = v.clone();
        }
    }
    options
}

/// 调度只复制资源策略，避免每次读取文件时复制整份管理任务历史。
pub fn policy_view(st: &crate::daemon::node::NodeStatus) -> crate::daemon::node::NodeStatus {
    crate::daemon::node::NodeStatus {
        pools: st.pools.clone(),
        tapes: st.tapes.clone(),
        management: std::sync::Arc::new(
            json!({"pools":st.management["pools"],"tapes":st.management["tapes"],"drives":st.management["drives"],"nodes":st.management["nodes"]}),
        ),
        ..Default::default()
    }
}

/// 容量属于设备观测，不属于访问策略；历史值不得从 policy 绕过有效性检查。
pub fn tape_policy(m: &Value, barcode: &str) -> Value {
    let mut policy = m["tapes"][barcode].clone();
    if let Some(p) = policy.as_object_mut() {
        p.remove("capacity");
        p.remove("capacity_refresh");
        p.remove("index_capacity");
        p.remove("index_capacity_refresh");
    }
    policy
}

/// 只改变观测缓存，不在控制状态机里访问设备。刷新失败保留上次成功值。
pub fn observe_tape(c: &mut ControlState, barcode: &str, observation: &Value, index: u64) {
    if !c.tapes.contains_key(barcode) || observation.to_string().len() > 8192 {
        return;
    }
    if observation["invalidate"] == true {
        invalidate_capacity(
            c,
            barcode,
            observation["reason"].as_str().unwrap_or("介质身份失效"),
            index,
        );
        return;
    }
    let Some(uuid) = observation["volume_uuid"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
    else {
        return;
    };
    if observation["generation"].as_u64().is_none_or(|g| g == 0)
        || observation["observed_at"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 64)
    {
        return;
    }
    if c.management.is_null() {
        c.management = empty();
    }
    let previous = c.management["tapes"].get(barcode).cloned();
    let mut record = previous.clone().unwrap_or(json!({}));
    for (key, refresh_key, error_key) in [
        ("capacity", "capacity_refresh", "error"),
        ("index_capacity", "index_capacity_refresh", "index_error"),
    ] {
        if record[key]["volume_uuid"]
            .as_str()
            .is_some_and(|old| old != uuid)
        {
            record[key] = Value::Null;
            record[refresh_key] = json!({"status":"invalidated","error":"卷身份已变化"});
        }
        // 旧版 owner 报告只有 DP；不把未上报的 IP 当成失败或零。
        let Some(cap) = observation.get(key) else {
            continue;
        };
        let measured = cap["maximum"]
            .as_u64()
            .zip(cap["remaining"].as_u64())
            .filter(|(m, r)| *m > 0 && r <= m && *m <= i64::MAX as u64);
        if let Some((maximum, remaining)) = measured {
            record[key] = json!({"volume_uuid":uuid,"generation":observation["generation"],"maximum":maximum,"remaining":remaining,"observed_at":observation["observed_at"],"partition":cap["partition"]});
        }
        record[refresh_key] = json!({"status":if measured.is_some(){"ok"}else{"failed"},"observed_at":observation["observed_at"],"error":if measured.is_some(){&Value::Null}else{&observation[error_key]},"owner":c.executor.as_ref().map(|e|e.node),"round":c.executor.as_ref().map(|e|e.round)});
    }
    c.management["tapes"][barcode] = record;
    if c.management.to_string().len() > MAX_STATE {
        if let Some(previous) = previous {
            c.management["tapes"][barcode] = previous;
        } else {
            c.management["tapes"]
                .as_object_mut()
                .unwrap()
                .remove(barcode);
        }
        return;
    }
    c.management["revision"] = json!(index);
}

pub fn invalidate_capacity(c: &mut ControlState, barcode: &str, reason: &str, index: u64) {
    if let Some(record) = c
        .management
        .get_mut("tapes")
        .and_then(|t| t.get_mut(barcode))
    {
        for (key, refresh) in [
            ("capacity", "capacity_refresh"),
            ("index_capacity", "index_capacity_refresh"),
        ] {
            record[key] = Value::Null;
            record[refresh] = json!({"status":"invalidated","error":reason});
        }
        c.management["revision"] = json!(index);
    }
}

/// 目录提交可能晚于分区采样；只清除其他卷的旧值，保留新 UUID 已成功读取的分区。
pub fn invalidate_other_volume(c: &mut ControlState, barcode: &str, uuid: &str, index: u64) {
    if let Some(record) = c
        .management
        .get_mut("tapes")
        .and_then(|t| t.get_mut(barcode))
    {
        let mut changed = false;
        for (key, refresh) in [
            ("capacity", "capacity_refresh"),
            ("index_capacity", "index_capacity_refresh"),
        ] {
            if !record[key].is_null() && record[key]["volume_uuid"].as_str() != Some(uuid) {
                record[key] = Value::Null;
                record[refresh] = json!({"status":"invalidated","error":"卷身份已变化"});
                changed = true;
            }
        }
        if changed {
            c.management["revision"] = json!(index);
        }
    }
}

fn current_capacity<'a>(
    m: &'a Value,
    barcode: &str,
    summary: Option<&crate::daemon::node::TapeRow>,
    key: &str,
) -> Option<&'a Value> {
    if cleaning_source(m, barcode).is_some() {
        return None;
    }
    let cap = &m["tapes"][barcode][key];
    let maximum = cap["maximum"].as_u64()?;
    let remaining = cap["remaining"].as_u64()?;
    let identity_matches = match (cap["volume_uuid"].as_str(), summary) {
        (Some(uuid), Some(s)) => s.volume_uuid.is_empty() || uuid == s.volume_uuid,
        (Some(_), None) => true,
        // 旧版观测没有 UUID，只兼容同代结果，不能跨代推断介质身份。
        (None, Some(s)) => cap["generation"].as_u64() == Some(s.generation),
        _ => false,
    };
    (identity_matches
        && maximum > 0
        && maximum <= i64::MAX as u64
        && remaining <= maximum
        && cap["generation"].as_u64().is_some_and(|g| g > 0)
        && cap["observed_at"].as_str().is_some_and(|s| !s.is_empty()))
    .then_some(cap)
}

fn capacity_fields(
    capacity: Option<u64>,
    free_space: Option<u64>,
    observed_at: Option<&str>,
    scope: &str,
) -> Value {
    let source = match scope {
        "IP" => "observed_ip_mam",
        "IP+DP" => "observed_ip_dp_mam",
        _ => "observed_dp_mam",
    };
    json!({
        "capacity":capacity,"free_space":free_space,
        "used_space":capacity.zip(free_space).and_then(|(c,f)|c.checked_sub(f)),
        "capacity_unit":"bytes","capacity_partition":scope,
        "capacity_source":if capacity.is_some(){source}else{"unknown"},
        "capacity_observed_at":observed_at,
        "capacity_cached":capacity.map(|_|true),
        // EE 的对象/不可追加/可回收空间有独立语义，不能用索引文件字节数冒充。
        "active_space":null,"non_appendable_space":null,"reclaimable_space":null,
        "reclaimable_percent":null
    })
}

/// Raft 应用进度与物理库存观测时间是两个独立维度。
pub fn inventory_observation(s: &crate::daemon::node::NodeStatus) -> Value {
    let inv = &s.management["inventory"];
    let matches = inv["owner"]
        .as_u64()
        .zip(inv["round"].as_u64())
        .map(|(node, round)| {
            s.executor
                .as_ref()
                .is_some_and(|e| e.node == node && e.round == round)
        });
    json!({"source":if inv.is_null(){"unknown"}else{"cached_inventory"},
        "observed_at":inv["observed_at"],"owner":inv["owner"],"round":inv["round"],
        "owner_matches_current":matches})
}

fn partition_view(
    s: &crate::daemon::node::NodeStatus,
    barcode: &str,
    key: &str,
    scope: &str,
) -> Value {
    let cap = current_capacity(&s.management, barcode, s.tapes.get(barcode), key);
    let mut row = capacity_fields(
        cap.and_then(|c| c["maximum"].as_u64()),
        cap.and_then(|c| c["remaining"].as_u64()),
        cap.and_then(|c| c["observed_at"].as_str()),
        scope,
    );
    row["partition_number"] = json!(
        cap.and_then(|c| c["partition"].as_u64())
            .or_else(|| (scope == "DP" && cap.is_some()).then_some(1))
    );
    row["capacity_generation"] = json!(cap.and_then(|c| c["generation"].as_u64()));
    row["capacity_generation_matches"] = json!(
        cap.and_then(|c| c["generation"].as_u64())
            .zip(s.tapes.get(barcode).map(|t| t.generation))
            .map(|(observed, current)| observed == current)
    );
    row["capacity_refresh"] = s.management["tapes"][barcode][if scope == "DP" {
        "capacity_refresh"
    } else {
        "index_capacity_refresh"
    }]
    .clone();
    row
}

/// 整卷只有在两个分区均已知且相加无溢出时才有完整容量。
fn volume_capacity(data: &Value, index: &Value) -> Value {
    let sum = |field: &str| {
        data[field]
            .as_u64()
            .zip(index[field].as_u64())
            .and_then(|(a, b)| a.checked_add(b))
    };
    let maximum = sum("capacity");
    let remaining = sum("free_space");
    let complete = maximum.is_some()
        && remaining.is_some()
        && !(data["partition_number"].as_u64().is_some()
            && data["partition_number"] == index["partition_number"]);
    let observed_at = if complete {
        data["capacity_observed_at"]
            .as_str()
            .zip(index["capacity_observed_at"].as_str())
            .map(|(a, b)| a.min(b))
    } else {
        None
    };
    let mut total = capacity_fields(
        maximum.filter(|_| complete),
        remaining.filter(|_| complete),
        observed_at,
        "IP+DP",
    );
    if data["capacity_source"] == "empty_pool" && index["capacity_source"] == "empty_pool" {
        total["capacity_source"] = json!("empty_pool");
        total["capacity_cached"] = json!(false);
    }
    total
}

/// EE 的可追加资格属于磁带及池策略，不以此刻是否装载或 owner 是否在线判断。
fn capacity_appendability(
    m: &Value,
    barcode: &str,
    summary: Option<&crate::daemon::node::TapeRow>,
    pool: Option<&crate::daemon::directory::PoolRow>,
) -> (Option<bool>, Option<&'static str>) {
    if cleaning_source(m, barcode).is_some() {
        return (Some(false), Some("清洗带不承载用户数据"));
    }
    let Some(pool) = pool else {
        return (Some(false), Some("磁带尚未分配"));
    };
    let policy = &m["tapes"][barcode];
    if !policy["blacklist"].is_null() {
        return (Some(false), Some("磁带在黑名单中"));
    }
    if !policy["offline"].is_null() {
        return (Some(false), Some("磁带已离线"));
    }
    if check_media(
        pool_options(m, &pool.uuid)["mediarestriction"]
            .as_str()
            .unwrap_or("none"),
        barcode,
    )
    .is_err()
    {
        return (Some(false), Some("介质不符合池限制"));
    }
    let Some(summary) = summary.filter(|s| s.generation > 0) else {
        return (None, Some("尚未确认卷状态"));
    };
    match summary.state.as_str() {
        "appendable" if summary.files >= pool.file_limit => (Some(false), Some("文件数已达池上限")),
        "appendable" => (Some(true), None),
        "full" => (Some(false), Some("磁带已满")),
        "data_full" => (Some(false), Some("磁带已达写入上限")),
        "check" | "label_mismatch" => (Some(false), Some("磁带须检查")),
        "reclaiming" | "reformatting" => (Some(false), Some("磁带正在维护")),
        _ => (None, Some("尚未确认卷状态")),
    }
}

fn ee_tape_capacity(
    m: &Value,
    barcode: &str,
    summary: Option<&crate::daemon::node::TapeRow>,
    pool: Option<&crate::daemon::directory::PoolRow>,
) -> Value {
    // EE 主列表的未分配容量为业务上的零；物理缓存由 partitions 单独返回。
    let cleaning = cleaning_source(m, barcode).is_some();
    if cleaning || pool.is_none() {
        let mut row = capacity_fields(Some(0), Some(0), None, "DP");
        row["capacity_source"] = json!(if cleaning { "cleaning" } else { "unassigned" });
        row["capacity_cached"] = json!(false);
        row["non_appendable_space"] = json!(0);
        row["non_appendable"] = json!(0);
        row["appendable"] = if cleaning { json!("no") } else { Value::Null };
        row["appendable_reason"] = json!(if cleaning {
            "清洗带不承载用户数据"
        } else {
            "磁带尚未分配"
        });
        return row;
    }
    let cap = current_capacity(m, barcode, summary, "capacity");
    let maximum = cap.and_then(|c| c["maximum"].as_u64());
    let physical_free = cap.and_then(|c| c["remaining"].as_u64());
    let mut row = capacity_fields(
        maximum,
        physical_free,
        cap.and_then(|c| c["observed_at"].as_str()),
        "DP",
    );
    let (appendable, reason) = capacity_appendability(m, barcode, summary, pool);
    row["free_space"] = json!(
        physical_free
            .zip(appendable)
            .map(|(free, enabled)| if enabled { free } else { 0 })
    );
    row["non_appendable_space"] = json!(
        physical_free
            .zip(appendable)
            .map(|(free, enabled)| if enabled { 0 } else { free })
    );
    // 文档使用 non_appendable，已保存的 EE JSON 使用 non_appendable_space；两种命名同值。
    row["non_appendable"] = row["non_appendable_space"].clone();
    row["appendable"] =
        json!(
            pool.and(appendable)
                .map(|enabled| if enabled { "yes" } else { "no" })
        );
    row["appendable_reason"] = json!(reason);
    row
}

/// EE 1.3.5 严重程度；本项目 check/维护/黑名单状态单独映射。
/// 保留介质健康告警，不能因 offline 或 unassigned 的展示状态而将其降级。
fn tape_status(state: &str, volume_state: Option<&str>) -> &'static str {
    [Some(state), volume_state]
        .into_iter()
        .flatten()
        .map(|state| match state {
            "appendable" | "append_fenced" | "data_full" | "exported" | "offline"
            | "recall_only" | "unassigned" | "write_protected" => (1, "ok"),
            "full" | "need_replace" | "reclaiming" | "reformatting" | "cleaning" => (2, "info"),
            "require_replace" => (3, "degraded"),
            "check_tape_library" | "blacklisted" => (4, "warning"),
            "check" | "require_validate" | "check_key_server" | "check_hba" | "inaccessible"
            | "non_supported" | "duplicated" | "missing" | "disconnected" | "unformatted"
            | "label_mismatch" | "need_unlock" => (5, "error"),
            _ => (0, "unknown"),
        })
        .max_by_key(|(severity, _)| *severity)
        .map_or("unknown", |(_, status)| status)
}

pub fn tape_views(s: &crate::daemon::node::NodeStatus) -> Vec<Value> {
    let m = &s.management;
    let volumes = m["inventory"]["volumes"].as_array();
    let mut barcodes: std::collections::BTreeSet<&str> =
        s.tapes.keys().map(String::as_str).collect();
    if let Some(volumes) = volumes {
        barcodes.extend(volumes.iter().filter_map(|v| v["barcode"].as_str()));
    }
    let observation = inventory_observation(s);
    barcodes.into_iter().map(|b| {
        let summary = s.tapes.get(b);
        let pool = s.pools.iter().find(|p| p.tapes.iter().any(|t| t == b));
        let policy = tape_policy(m,b);
        let access = if !policy["blacklist"].is_null(){"blacklisted"}else if !policy["offline"].is_null(){"offline"}else{"online"};
        let location = volumes.and_then(|rows| rows.iter().find(|v|v["barcode"]==b));
        let drive = m["inventory"]["drives"].as_array().and_then(|drives|drives.iter().find(|d|d["barcode"]==b));
        let cap = current_capacity(m,b,summary,"capacity");
        let volume_state = summary.map(|t|t.state.as_str());
        let cleaning = cleaning_source(m,b);
        let state = if cleaning.is_some() {"non_supported"} else if access != "online" { access } else if pool.is_none(){"unassigned"}else{volume_state.unwrap_or("unknown")};
        let status = if cleaning.is_some() {
            let health = tape_status("cleaning", volume_state);
            if access == "blacklisted" && health != "error" { "warning" } else { health }
        } else { tape_status(state, volume_state) };
        let mut row = ee_tape_capacity(m,b,summary,pool);
        let data=partition_view(s,b,"capacity","DP");
        let index=partition_view(s,b,"index_capacity","IP");
        row["volume_capacity"]=volume_capacity(&data,&index);
        row["partitions"]=json!({"data":data,"index":index});
        row.as_object_mut().unwrap().extend(json!({
            "barcode":b,"state":state,"volume_state":volume_state,
            "status":status,
            "cleaning":cleaning.is_some(),"cleaning_source":cleaning,
            "medium_type":location.and_then(|l|l["medium_type"].as_u64()),
            "generation":summary.map(|t|t.generation),"files":summary.map(|t|t.files),
            "volume_uuid":summary.map(|t|t.volume_uuid.as_str()).filter(|u|!u.is_empty()).or_else(||cap.and_then(|c|c["volume_uuid"].as_str())),
            "bytes_used":summary.map(|t|t.bytes_used),"bytes_written":summary.map(|t|t.bytes_written),
            "capacity_generation":pool.and(cap).and_then(|c|c["generation"].as_u64()),
            "capacity_generation_matches":pool.and(cap).and_then(|c|c["generation"].as_u64()).zip(summary.map(|t|t.generation)).map(|(observed,current)|observed==current),
            "capacity_refresh":if pool.is_some(){m["tapes"][b]["capacity_refresh"].clone()}else{Value::Null},
            "policy":policy,"access":access,"offline_msg":m["tapes"][b]["offline"],
            "pool":pool.map(|p|&p.uuid),"pool_id":pool.map(|p|&p.uuid),"pool_name":pool.map(|p|&p.name),
            "location":location,"address":location.and_then(|l|l["address"].as_u64()),
            "location_type":location.and_then(|l|match l["element_type"].as_u64(){Some(2)=>Some("homeslot"),Some(3)=>Some("ieslot"),Some(4)=>Some("drive"),_=>None}),
            "drive_id":drive.and_then(|d|d["serial"].as_str()),
            "inventory_observation":observation
        }).as_object().unwrap().clone());
        row
    }).collect()
}

pub fn drive_views(s: &crate::daemon::node::NodeStatus) -> Vec<Value> {
    let m = &s.management;
    m["inventory"]["drives"]
        .as_array()
        .map(|drives| {
            drives
                .iter()
                .map(|d| {
                    let mut row = d.clone();
                    let policy = drive_policy(m, d["serial"].as_str().unwrap_or(""));
                    row["enabled"] = policy["enabled"].clone();
                    row["role"] = policy["role"].clone();
                    row["nodegroup_name"] = policy["nodegroup"].clone();
                    row["policy"] = policy;
                    row["inventory_observation"] = inventory_observation(s);
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn pool_view(
    s: &crate::daemon::node::NodeStatus,
    pool: &crate::daemon::directory::PoolRow,
) -> Value {
    let options = pool_options(&s.management, &pool.uuid);
    let observation = pool_observation(&s.management, pool, &s.tapes);
    let mut row = observation.clone();
    // 保留旧容量观测对象和池字段，在顶层提供与 EE REST 相同的容量/属性命名。
    row.as_object_mut().unwrap().remove("source");
    row.as_object_mut().unwrap().remove("observed_at");
    row.as_object_mut().unwrap().extend(json!({
        "id":pool.uuid,"uuid":pool.uuid,"name":pool.name,"file_limit":pool.file_limit,
        "tapes":pool.tapes,"num_of_tapes":pool.tapes.len(),
        "media_restriction":options["mediarestriction"],
        "mount_limit":options["mountlimit"].as_str().and_then(|v|v.parse::<u64>().ok()),
        "nodegroup_name":options["nodegroup"],
        "low_space_warning_enable":options["lowspacewarningenable"]!="no",
        "low_space_warning_threshold":options["lowspacewarningthreshold"].as_str().and_then(|v|v.parse::<u64>().ok()),
        "no_space_warning_enable":options["nospacewarningenable"]!="no",
        "options":options,"capacity_observation":observation
    }).as_object().unwrap().clone());
    row
}

/// 容量和告警基于最近一次有效 DP MAM 缓存；未装载介质不因查询而搬带。
pub fn pool_observation(
    m: &Value,
    pool: &crate::daemon::directory::PoolRow,
    tapes: &std::collections::BTreeMap<String, crate::daemon::node::TapeRow>,
) -> Value {
    let data = pool_partition_observation(m, pool, tapes, "capacity", "DP");
    let index = pool_partition_observation(m, pool, tapes, "index_capacity", "IP");
    let mut observation = data.clone();
    let mut available = Some(0u64);
    let mut non_appendable = Some(0u64);
    for b in &pool.tapes {
        let tape = ee_tape_capacity(m, b, tapes.get(b), Some(pool));
        available = available
            .zip(tape["free_space"].as_u64())
            .and_then(|(a, b)| a.checked_add(b));
        non_appendable = non_appendable
            .zip(tape["non_appendable_space"].as_u64())
            .and_then(|(a, b)| a.checked_add(b));
    }
    if observation["capacity"].is_null() {
        available = None;
        non_appendable = None;
    }
    observation["free_space"] = json!(available);
    observation["non_appendable_space"] = json!(non_appendable);
    observation["non_appendable"] = json!(non_appendable);
    observation["capacity_complete"] =
        json!(observation["capacity"].is_u64() && available.is_some() && non_appendable.is_some());
    observation["volume_capacity"] = volume_capacity(&data, &index);
    observation["partitions"] = json!({"data":data,"index":index});
    let remaining = observation["free_space"].as_u64();
    let options = &m["pools"][&pool.uuid];
    let threshold = options["lowspacewarningthreshold"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    observation["observed_at"] = observation["capacity_observed_at"].clone();
    observation["source"] = observation["capacity_source"].clone();
    observation["low_space_warning_threshold_unit"] = json!("TiB");
    observation["warning_scope"] = json!("available_space");
    observation["low_space_warning"] =
        json!(remaining.map(|r| options["lowspacewarningenable"] != "no"
            && threshold > 0
            && u128::from(r) < u128::from(threshold) * (1u128 << 40)));
    observation["no_space_warning"] =
        json!(remaining.map(|r| options["nospacewarningenable"] != "no" && r == 0));
    observation
}

fn pool_partition_observation(
    m: &Value,
    pool: &crate::daemon::directory::PoolRow,
    tapes: &std::collections::BTreeMap<String, crate::daemon::node::TapeRow>,
    key: &str,
    scope: &str,
) -> Value {
    let mut maximum = Some(0u64);
    let mut remaining = Some(0u64);
    let mut oldest: Option<&str> = None;
    let mut known = 0usize;
    if pool.tapes.is_empty() {
        let mut empty = capacity_fields(Some(0), Some(0), None, scope);
        empty["capacity_source"] = json!("empty_pool");
        empty["capacity_cached"] = json!(false);
        empty["capacity_complete"] = json!(true);
        empty["capacity_known_tapes"] = json!(0);
        empty["capacity_unknown_tapes"] = json!(0);
        return empty;
    }
    for b in &pool.tapes {
        let Some(cap) = current_capacity(m, b, tapes.get(b), key) else {
            maximum = None;
            remaining = None;
            continue;
        };
        known += 1;
        maximum = maximum
            .zip(cap["maximum"].as_u64())
            .and_then(|(a, b)| a.checked_add(b));
        remaining = remaining
            .zip(cap["remaining"].as_u64())
            .and_then(|(a, b)| a.checked_add(b));
        if let Some(at) = cap["observed_at"].as_str()
            && oldest.is_none_or(|old| at < old)
        {
            oldest = Some(at);
        }
    }
    if maximum.is_none() || remaining.is_none() {
        maximum = None;
        remaining = None;
        oldest = None;
    }
    let mut row = capacity_fields(maximum, remaining, oldest, scope);
    row["capacity_complete"] = json!(maximum.is_some() && remaining.is_some());
    row["capacity_known_tapes"] = json!(known);
    row["capacity_unknown_tapes"] = json!(pool.tapes.len() - known);
    row
}

/// 快照可以缺少旧版本没有的管理状态；已有状态必须符合本版本结构。
pub fn valid_snapshot(m: &Value) -> bool {
    if m.is_null() {
        return true;
    }
    if m.to_string().len() > MAX_STATE
        || !["pools", "tapes", "drives", "nodes", "tasks"]
            .iter()
            .all(|k| m[*k].is_object())
        || !m["nodegroups"]
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string))
    {
        return false;
    }
    if !["pools", "tapes", "drives"]
        .iter()
        .all(|k| m[*k].as_object().unwrap().values().all(Value::is_object))
        || !m["nodes"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v.is_null() || v.is_string())
        || !(m["inventory"].is_null() || m["inventory"].is_object())
    {
        return false;
    }
    m["tasks"].as_object().unwrap().iter().all(|(id, t)| {
        id.parse::<u64>()
            .ok()
            .filter(|i| *i > 0)
            .is_some_and(|id| Some(id) == t["id"].as_u64())
            && [
                "queued",
                "running",
                "success",
                "failed",
                "cancelled",
                "indeterminate",
            ]
            .iter()
            .any(|s| t["state"] == *s)
            && t["cancel_requested"].is_boolean()
            && t["request"]["params"].is_object()
            && ["resource", "action", "target"]
                .iter()
                .all(|k| t["request"][*k].is_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::{Applied, Command, ControlSnapshot, Executor};
    fn state() -> ControlState {
        let mut c = ControlState::default();
        c.apply(
            1,
            &Command::PoolCreate {
                uuid: "uuid".into(),
                name: "pool".into(),
                file_limit: 200000,
            },
        );
        c.apply(
            2,
            &Command::TapeAssign {
                barcode: "TEST01L8".into(),
                pool: "pool".into(),
            },
        );
        c.executor = Some(Executor {
            node: 1,
            term: 1,
            round: 3,
            fenced: true,
        });
        c.executor_ready = true;
        c.applied_index = 3;
        c
    }

    #[test]
    fn cleaning_inventory_is_informational_and_cannot_enter_a_data_pool() {
        use crate::daemon::node::NodeStatus;
        let mut c = state();
        c.management["inventory"] = json!({"volumes":[
            {"barcode":"CUSTOM", "medium_type":2, "element_type":2},
            {"barcode":"CLNU03L1", "element_type":3},
            {"barcode":"DATA01L1", "medium_type":1, "element_type":2}
        ]});
        // 升级前错误归类为数据带的观测不能再作为清洗带物理容量返回。
        c.management["tapes"]["CUSTOM"] = json!({
            "capacity":{"volume_uuid":"old-data-volume","generation":9,"maximum":1024,"remaining":512,"observed_at":"2026-10-20T00:00:00Z"},
            "index_capacity":{"volume_uuid":"old-data-volume","generation":9,"maximum":128,"remaining":64,"observed_at":"2026-10-20T00:00:00Z"}
        });
        for barcode in ["CUSTOM", "CLNU03L1"] {
            assert!(!tape_allowed(&c.management, barcode));
            assert!(validate_job(&c, "tapes", "assign", barcode, &json!({"pool":"pool"})).is_err());
            assert!(matches!(
                c.apply(
                    4,
                    &Command::TapeAssign {
                        barcode: barcode.into(),
                        pool: "pool".into()
                    }
                ),
                Applied::Admin(Err(_))
            ));
        }
        let mut s = NodeStatus {
            management: c.management.clone().into(),
            ..Default::default()
        };
        for assigned in [false, true] {
            if assigned {
                s.pools = vec![crate::daemon::directory::PoolRow {
                    uuid: "uuid".into(),
                    name: "pool".into(),
                    file_limit: 10,
                    tapes: vec!["CUSTOM".into(), "CLNU03L1".into()],
                }];
            }
            let rows = tape_views(&s);
            for barcode in ["CUSTOM", "CLNU03L1"] {
                let row = rows.iter().find(|r| r["barcode"] == barcode).unwrap();
                assert_eq!(row["cleaning"], true);
                assert_eq!(row["state"], "non_supported");
                assert_eq!(row["status"], "info");
                assert_eq!(row["capacity"], 0);
                assert_eq!(row["free_space"], 0);
                assert_eq!(row["appendable"], "no");
                assert_eq!(row["capacity_source"], "cleaning");
                assert!(row["partitions"]["data"]["capacity"].is_null());
                assert!(row["partitions"]["index"]["capacity"].is_null());
                assert!(row["volume_capacity"]["capacity"].is_null());
                assert!(row["capacity_generation"].is_null());
            }
            let data = rows.iter().find(|r| r["barcode"] == "DATA01L1").unwrap();
            assert_eq!(data["cleaning"], false);
            assert_eq!(data["state"], "unassigned");
        }
    }
    #[test]
    fn ee_status_preserves_health_when_assignment_or_access_changes() {
        use crate::daemon::{
            directory::PoolRow,
            node::{NodeStatus, TapeRow},
        };
        let mut s = NodeStatus {
            pools: vec![PoolRow {
                uuid: "p".into(),
                name: "pool".into(),
                file_limit: 10,
                tapes: vec!["B".into()],
            }],
            ..Default::default()
        };
        let mut m = empty();
        for (state, expected) in [
            ("appendable", "ok"),
            ("data_full", "ok"),
            ("full", "info"),
            ("need_replace", "info"),
            ("require_replace", "degraded"),
            ("check_tape_library", "warning"),
            ("check", "error"),
            ("label_mismatch", "error"),
            ("non_supported", "error"),
            ("reclaiming", "info"),
            ("reformatting", "info"),
            ("future_state", "unknown"),
        ] {
            s.tapes.insert(
                "B".into(),
                TapeRow {
                    state: state.into(),
                    generation: 1,
                    ..Default::default()
                },
            );
            s.management = std::sync::Arc::new(m.clone());
            assert_eq!(tape_views(&s)[0]["status"], expected, "{state}");
        }
        m["tapes"]["B"] = json!({"offline":"maintenance"});
        s.management = std::sync::Arc::new(m.clone());
        s.tapes.get_mut("B").unwrap().state = "check".into();
        assert_eq!(tape_views(&s)[0]["state"], "offline");
        assert_eq!(tape_views(&s)[0]["status"], "error");
        s.pools.clear();
        assert_eq!(tape_views(&s)[0]["status"], "error");
        s.tapes.get_mut("B").unwrap().state = "appendable".into();
        assert_eq!(tape_views(&s)[0]["status"], "ok");
        m["tapes"]["B"]["blacklist"] = json!("operator policy");
        s.management = std::sync::Arc::new(m);
        assert_eq!(tape_views(&s)[0]["status"], "warning");
    }

    #[test]
    fn ee_unassigned_capacity_is_zero_without_discarding_partition_history() {
        use crate::daemon::{
            directory::PoolRow,
            node::{NodeStatus, TapeRow},
        };
        let mut s = NodeStatus::default();
        s.tapes.insert(
            "B".into(),
            TapeRow {
                volume_uuid: "u".into(),
                state: "appendable".into(),
                generation: 7,
                ..Default::default()
            },
        );
        let mut m = empty();
        m["tapes"]["B"] = json!({
            "capacity":{"volume_uuid":"u","generation":7,"maximum":1000,"remaining":400,"partition":1,"observed_at":"2026-10-20T00:00:00Z"},
            "index_capacity":{"volume_uuid":"u","generation":7,"maximum":100,"remaining":0,"partition":0,"observed_at":"2026-10-20T00:00:00Z"},
            "capacity_refresh":{"status":"failed","error":"unavailable"}
        });
        s.management = std::sync::Arc::new(m);
        let original = s.management.clone();
        let unassigned = tape_views(&s).remove(0);
        assert_eq!(unassigned["status"], "ok");
        assert_eq!(unassigned["state"], "unassigned");
        for field in [
            "capacity",
            "used_space",
            "free_space",
            "non_appendable_space",
            "non_appendable",
        ] {
            assert_eq!(unassigned[field], 0, "{field}");
        }
        assert_eq!(unassigned["capacity_source"], "unassigned");
        assert_eq!(unassigned["capacity_cached"], false);
        for field in [
            "capacity_observed_at",
            "capacity_generation",
            "capacity_generation_matches",
            "capacity_refresh",
            "appendable",
            "reclaimable_space",
            "reclaimable_percent",
        ] {
            assert!(unassigned[field].is_null(), "{field}");
        }
        assert_eq!(unassigned["partitions"]["data"]["capacity"], 1000);
        assert_eq!(
            unassigned["partitions"]["data"]["capacity_refresh"]["status"],
            "failed"
        );
        assert_eq!(unassigned["partitions"]["index"]["free_space"], 0);
        s.pools.push(PoolRow {
            uuid: "p".into(),
            name: "pool".into(),
            file_limit: 10,
            tapes: vec!["B".into()],
        });
        let assigned = tape_views(&s).remove(0);
        assert_eq!(assigned["capacity"], 1000);
        assert_eq!(
            assigned["free_space"], 400,
            "IP remaining zero must not imply data full"
        );
        assert_eq!(assigned["capacity_cached"], true);
        assert_eq!(assigned["partitions"], unassigned["partitions"]);
        assert_eq!(
            s.management, original,
            "queries must not mutate cached observations"
        );
        std::sync::Arc::make_mut(&mut s.management)["tapes"]["B"]["capacity"] = Value::Null;
        assert!(tape_views(&s)[0]["capacity"].is_null());
        s.pools.clear();
        let no_observation = tape_views(&s).remove(0);
        assert_eq!(no_observation["capacity"], 0);
        assert!(no_observation["partitions"]["data"]["capacity"].is_null());
    }

    #[test]
    fn ee_capacity_separates_available_and_non_appendable_space() {
        use crate::daemon::{directory::PoolRow, node::TapeRow};
        let pool = PoolRow {
            uuid: "p".into(),
            name: "pool".into(),
            file_limit: 10,
            tapes: vec!["TEST01L8".into()],
        };
        let mut s = crate::daemon::node::NodeStatus {
            pools: vec![pool.clone()],
            ..Default::default()
        };
        s.tapes.insert(
            "TEST01L8".into(),
            TapeRow {
                volume_uuid: "u".into(),
                state: "appendable".into(),
                generation: 8,
                ..Default::default()
            },
        );
        let mut m = empty();
        m["tapes"]["TEST01L8"] = json!({"capacity":{"volume_uuid":"u","generation":7,"maximum":1000,"remaining":400,"observed_at":"2026-10-20T00:00:00Z"}});
        for (state, policy, expected_free, expected_appendable) in [
            ("appendable", json!({}), 400, "yes"),
            ("full", json!({}), 0, "no"),
            ("data_full", json!({}), 0, "no"),
            ("check", json!({}), 0, "no"),
            ("label_mismatch", json!({}), 0, "no"),
            ("appendable", json!({"offline":"maintenance"}), 0, "no"),
            ("appendable", json!({"blacklist":"bad medium"}), 0, "no"),
        ] {
            s.tapes.get_mut("TEST01L8").unwrap().state = state.into();
            let cap = m["tapes"]["TEST01L8"]["capacity"].clone();
            m["tapes"]["TEST01L8"] = policy;
            m["tapes"]["TEST01L8"]["capacity"] = cap;
            s.management = std::sync::Arc::new(m.clone());
            let tape = &tape_views(&s)[0];
            assert_eq!(tape["appendable"], expected_appendable, "{tape}");
            assert_eq!(tape["capacity"], 1000);
            assert_eq!(tape["used_space"], 600);
            assert_eq!(tape["free_space"], expected_free);
            assert_eq!(tape["non_appendable_space"], 400 - expected_free);
            assert_eq!(tape["partitions"]["data"]["free_space"], 400);
            let pool = pool_observation(&m, &pool, &s.tapes);
            assert_eq!(pool["free_space"], expected_free);
            assert_eq!(pool["non_appendable_space"], 400 - expected_free);
            assert_eq!(pool["no_space_warning"], expected_free == 0);
        }
    }

    #[test]
    fn ee_pool_aggregates_each_tapes_eligibility_and_distinguishes_unknown_from_empty() {
        use crate::daemon::{directory::PoolRow, node::TapeRow};
        let mut pool = PoolRow {
            uuid: "p".into(),
            name: "pool".into(),
            file_limit: 10,
            tapes: vec!["T1L8".into(), "T2L8".into()],
        };
        let mut tapes = std::collections::BTreeMap::new();
        let mut m = empty();
        for b in &pool.tapes {
            tapes.insert(
                b.clone(),
                TapeRow {
                    volume_uuid: b.clone(),
                    state: "appendable".into(),
                    generation: 8,
                    ..Default::default()
                },
            );
            m["tapes"][b] = json!({"capacity":{"volume_uuid":b,"generation":7,"maximum":1000,"remaining":400,"observed_at":"2026-10-20T00:00:00Z"}});
        }
        m["tapes"]["T2L8"]["offline"] = json!("maintenance");
        let mixed = pool_observation(&m, &pool, &tapes);
        assert_eq!(mixed["capacity"], 2000);
        assert_eq!(mixed["used_space"], 1200);
        assert_eq!(mixed["free_space"], 400);
        assert_eq!(mixed["non_appendable_space"], 400);
        assert_eq!(mixed["partitions"]["data"]["free_space"], 800);
        assert_eq!(mixed["capacity_complete"], true);
        assert_eq!(mixed["capacity_known_tapes"], 2);
        assert_eq!(mixed["capacity_unknown_tapes"], 0);
        assert!(mixed["reclaimable_percent"].is_null());
        tapes.get_mut("T1L8").unwrap().files = 10;
        assert_eq!(pool_observation(&m, &pool, &tapes)["free_space"], 0);
        tapes.get_mut("T1L8").unwrap().files = 0;
        m["pools"]["p"] = json!({"mediarestriction":"L9"});
        assert_eq!(
            pool_observation(&m, &pool, &tapes)["non_appendable_space"],
            800
        );
        m["tapes"]["T2L8"]["capacity"] = Value::Null;
        let unknown = pool_observation(&m, &pool, &tapes);
        assert_eq!(unknown["capacity_complete"], false);
        assert_eq!(unknown["capacity_known_tapes"], 1);
        assert_eq!(unknown["capacity_unknown_tapes"], 1);
        assert_eq!(unknown["partitions"]["index"]["capacity_unknown_tapes"], 2);
        for field in [
            "capacity",
            "used_space",
            "free_space",
            "non_appendable_space",
            "no_space_warning",
        ] {
            assert!(unknown[field].is_null(), "{field}: {unknown}");
        }
        // 缺失成员排在前面时，仍统计后续成员，不能在第一个未知处提前结束。
        pool.tapes.reverse();
        assert_eq!(
            pool_observation(&m, &pool, &tapes)["capacity_known_tapes"],
            1
        );
        m["tapes"]["T2L8"]["capacity"] = m["tapes"]["T1L8"]["capacity"].clone();
        m["tapes"]["T2L8"]["capacity"]["volume_uuid"] = json!("T2L8");
        tapes.get_mut("T2L8").unwrap().state = "unknown".into();
        m["tapes"]["T2L8"]["offline"] = Value::Null;
        m["pools"]["p"] = json!({});
        let uncertain = pool_observation(&m, &pool, &tapes);
        assert_eq!(uncertain["capacity"], 2000);
        assert_eq!(uncertain["capacity_known_tapes"], 2);
        assert_eq!(uncertain["capacity_unknown_tapes"], 0);
        assert_eq!(uncertain["capacity_complete"], false);
        assert!(uncertain["free_space"].is_null());
        assert!(uncertain["no_space_warning"].is_null());
        pool.tapes.clear();
        let empty = pool_observation(&m, &pool, &tapes);
        for field in [
            "capacity",
            "used_space",
            "free_space",
            "non_appendable_space",
        ] {
            assert_eq!(empty[field], 0);
        }
        assert_eq!(empty["capacity_source"], "empty_pool");
        assert_eq!(empty["capacity_complete"], true);
        assert_eq!(empty["capacity_known_tapes"], 0);
        assert_eq!(empty["capacity_unknown_tapes"], 0);
        assert_eq!(empty["volume_capacity"]["capacity_source"], "empty_pool");
    }

    #[test]
    fn ee_pool_overflow_is_incomplete_even_when_every_tape_is_observed() {
        use crate::daemon::{directory::PoolRow, node::TapeRow};
        let pool = PoolRow {
            uuid: "p".into(),
            name: "pool".into(),
            file_limit: 10,
            tapes: vec!["A".into(), "B".into(), "C".into()],
        };
        let mut m = empty();
        let mut tapes = std::collections::BTreeMap::new();
        for b in &pool.tapes {
            tapes.insert(
                b.clone(),
                TapeRow {
                    volume_uuid: b.clone(),
                    generation: 1,
                    state: "appendable".into(),
                    ..Default::default()
                },
            );
            m["tapes"][b] = json!({"capacity":{"volume_uuid":b,"generation":1,"maximum":i64::MAX,"remaining":0,"observed_at":"2026-10-20T00:00:00Z"}});
        }
        let row = pool_observation(&m, &pool, &tapes);
        assert!(row["capacity"].is_null());
        assert!(row["free_space"].is_null());
        assert!(row["no_space_warning"].is_null());
        assert_eq!(row["capacity_complete"], false);
        assert_eq!(row["capacity_known_tapes"], 3);
        assert_eq!(row["capacity_unknown_tapes"], 0);
    }

    #[test]
    fn partitions_refresh_independently_and_survive_catalog_identity_change() {
        let mut c = state();
        let observation = |uuid, dp, ip| json!({"volume_uuid":uuid,"generation":7,"observed_at":"2026-10-20T00:00:00Z","capacity":dp,"index_capacity":ip,"error":"DP unavailable","index_error":"IP unavailable"});
        let dp = json!({"maximum":1024,"remaining":512,"partition":1});
        let ip = json!({"maximum":128,"remaining":64,"partition":0});
        observe_tape(
            &mut c,
            "TEST01L8",
            &observation("a", dp.clone(), ip.clone()),
            4,
        );
        observe_tape(
            &mut c,
            "TEST01L8",
            &observation(
                "a",
                json!({"maximum":1024,"remaining":256,"partition":1}),
                Value::Null,
            ),
            5,
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity"]["remaining"],
            256
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["index_capacity"]["remaining"],
            64
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["index_capacity_refresh"]["status"],
            "failed"
        );
        // 新介质 DP 失败、IP 成功，不允许留下旧介质 DP，也不允许目录更新抹掉新 IP。
        observe_tape(&mut c, "TEST01L8", &observation("b", Value::Null, ip), 6);
        invalidate_other_volume(&mut c, "TEST01L8", "b", 7);
        assert!(c.management["tapes"]["TEST01L8"]["capacity"].is_null());
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["index_capacity"]["remaining"],
            64
        );
        invalidate_capacity(&mut c, "TEST01L8", "format", 8);
        assert!(c.management["tapes"]["TEST01L8"]["index_capacity"].is_null());
        let data = capacity_fields(Some(u64::MAX), Some(0), Some("a"), "DP");
        let index = capacity_fields(Some(1), Some(0), Some("b"), "IP");
        assert!(volume_capacity(&data, &index)["capacity"].is_null());
    }

    #[test]
    fn capacity_cache_survives_refresh_failure_and_rejects_old_owner_reports() {
        let mut c = state();
        c.executor_instance = 1;
        c.candidates.insert(
            1,
            crate::daemon::state::Candidate {
                instance: 1,
                sequence: 1,
                group: "g".into(),
                drives: 1,
                eligible: true,
            },
        );
        let make = |generation, capacity| Command::ObserveTape {
            barcode: "TEST01L8".into(),
            observation: json!({"volume_uuid":"volume-a","generation":generation,"observed_at":"2026-10-20T00:00:00Z","capacity":capacity,"error":"unavailable"}),
        };
        let first = make(7, json!({"maximum":1024,"remaining":512}));
        let report = |sequence, round, command| Command::ExecutorReport {
            node: 1,
            instance: 1,
            round,
            sequence,
            command: Box::new(command),
        };
        let first_report = report(1, 3, first);
        assert_eq!(
            Command::decode(&first_report.encode()),
            Some(first_report.clone())
        );
        c.apply(4, &first_report);
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity"]["remaining"],
            512
        );
        c.apply(5, &report(2, 3, make(8, Value::Null)));
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity"]["generation"],
            7
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity_refresh"]["status"],
            "failed"
        );
        let snapshot = ControlSnapshot {
            control: c.clone(),
            source: 1,
            directory_index: 0,
        };
        assert_eq!(ControlSnapshot::decode(&snapshot.encode()), Some(snapshot));
        c.apply(
            6,
            &report(3, 3, make(8, json!({"maximum":1024,"remaining":256}))),
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity"]["remaining"],
            256
        );
        c.apply(7, &Command::Takeover { node: 2, term: 2 });
        c.apply(
            8,
            &report(4, 3, make(9, json!({"maximum":1024,"remaining":0}))),
        );
        assert_eq!(
            c.management["tapes"]["TEST01L8"]["capacity"]["remaining"],
            256
        );
        // 重新格式化或检测到新 UUID 时，旧容量不能跨介质沿用。
        invalidate_capacity(&mut c, "TEST01L8", "reformat", 9);
        assert!(c.management["tapes"]["TEST01L8"]["capacity"].is_null());
        observe_tape(
            &mut c,
            "TEST01L8",
            &json!({"volume_uuid":"volume-b","generation":1,"observed_at":"2026-10-20T00:00:01Z","capacity":null}),
            10,
        );
        assert!(c.management["tapes"]["TEST01L8"]["capacity"].is_null());
    }

    #[test]
    fn legacy_takeover_and_unassignment_keep_absent_management_state_absent() {
        let mut c = state();
        assert!(c.management.is_null());
        c.apply(4, &Command::Takeover { node: 2, term: 2 });
        assert!(c.management.is_null());
        c.apply(
            5,
            &Command::TapeUnassign {
                barcode: "TEST01L8".into(),
            },
        );
        assert!(c.management.is_null());
    }
    #[test]
    fn capacity_requires_complete_current_observations_and_uses_tib_thresholds() {
        use crate::daemon::{directory::PoolRow, node::TapeRow};
        let pool = PoolRow {
            uuid: "u".into(),
            name: "p".into(),
            file_limit: 100,
            tapes: vec!["b".into()],
        };
        let mut tapes = std::collections::BTreeMap::new();
        tapes.insert(
            "b".into(),
            TapeRow {
                state: "appendable".into(),
                generation: 7,
                ..Default::default()
            },
        );
        let mut m = empty();
        assert!(pool_observation(&m, &pool, &tapes)["free_space"].is_null());
        m["tapes"]["b"] = json!({"capacity":{"generation":7,"maximum":2u64<<40,"remaining":1u64<<39,"observed_at":"2026-10-20T00:00:00Z"}});
        m["pools"]["u"] = json!({"lowspacewarningthreshold":"1"});
        let measured = pool_observation(&m, &pool, &tapes);
        assert_eq!(measured["low_space_warning"], true);
        assert_eq!(measured["free_space"], 1u64 << 39);
        assert_eq!(measured["used_space"], (2u64 << 40) - (1u64 << 39));
        tapes.get_mut("b").unwrap().generation = 8;
        let stale = pool_observation(&m, &pool, &tapes);
        for field in [
            "capacity",
            "free_space",
            "used_space",
            "observed_at",
            "capacity_observed_at",
            "low_space_warning",
            "no_space_warning",
        ] {
            assert!(stale[field].is_null(), "{field}: {stale}");
        }
        tapes.get_mut("b").unwrap().generation = 7;
        m["tapes"]["b"]["capacity"]["remaining"] = json!(u64::MAX);
        assert!(pool_observation(&m, &pool, &tapes)["free_space"].is_null());
    }
    #[test]
    fn malformed_management_snapshots_are_refused_before_mutable_indexing() {
        let mut m = empty();
        assert!(valid_snapshot(&m));
        m["tasks"] = Value::Null;
        assert!(!valid_snapshot(&m));
        let mut m = empty();
        m["drives"]["d"] = json!("bad");
        assert!(!valid_snapshot(&m));
        let mut m = empty();
        m["tasks"]["bad"] = json!({"id":null,"state":"queued","cancel_requested":false,"request":{"resource":"tapes","action":"validate","target":"b","params":{}}});
        assert!(!valid_snapshot(&m));
    }

    #[test]
    fn configuration_is_atomic_snapshotted_and_old_snapshots_remain_readable() {
        let mut c = state();
        apply(
            &mut c,
            4,
            &request(
                "pools",
                "set",
                "pool",
                json!({"attribute":"mediarestriction","value":"L8"}),
            ),
        )
        .unwrap();
        report(&mut c, 4, 3, "running", &Value::Null);
        assert!(report(&mut c, 4, 3, "success", &json!({})));
        let before = c.clone();
        assert!(
            apply(
                &mut c,
                5,
                &request(
                    "pools",
                    "set",
                    "pool",
                    json!({"attribute":"mediarestriction","value":"L99"})
                )
            )
            .is_err()
        );
        assert_eq!(c, before);
        c.applied_index = 5;
        let snapshot = ControlSnapshot {
            control: c,
            source: 1,
            directory_index: 0,
        };
        assert_eq!(
            ControlSnapshot::decode(&snapshot.encode()),
            Some(snapshot.clone())
        );
        let mut old: Value = serde_json::from_slice(&snapshot.encode()).unwrap();
        old.as_object_mut().unwrap().remove("management");
        assert!(
            ControlSnapshot::decode(old.to_string().as_bytes())
                .unwrap()
                .control
                .management
                .is_null()
        );
    }
    #[test]
    fn task_claim_results_and_cancellation_are_bound_to_committed_round() {
        let mut c = state();
        apply(
            &mut c,
            4,
            &request(
                "tapes",
                "offline",
                "TEST01L8",
                json!({"message":"maintenance"}),
            ),
        )
        .unwrap();
        assert!(tape_allowed(&c.management, "TEST01L8"));
        assert!(!report(&mut c, 4, 99, "running", &Value::Null));
        assert_eq!(c.management["tasks"]["4"]["state"], "queued");
        report(&mut c, 4, 3, "running", &Value::Null);
        apply(&mut c, 5, &request("tasks", "cancel", "4", json!({}))).unwrap();
        assert!(
            report(&mut c, 4, 3, "success", &json!({})),
            "已完成的操作不能被取消请求抹去"
        );
        assert!(!tape_allowed(&c.management, "TEST01L8"));
        let before = c.clone();
        assert!(!report(&mut c, 4, 3, "failed", &json!({})));
        assert_eq!(c, before);
    }
    #[test]
    fn takeover_preserves_queued_jobs_and_never_replays_running_jobs() {
        let mut c = state();
        apply(
            &mut c,
            4,
            &request("libraries", "rescan", "current", json!({})),
        )
        .unwrap();
        c.apply(5, &Command::Takeover { node: 2, term: 2 });
        assert_eq!(c.management["tasks"]["4"]["state"], "queued");
        c.executor.as_mut().unwrap().fenced = true;
        c.executor_ready = true;
        report(&mut c, 4, 5, "running", &Value::Null);
        c.apply(6, &Command::Takeover { node: 1, term: 3 });
        assert_eq!(c.management["tasks"]["4"]["state"], "indeterminate");
        assert!(!report(&mut c, 4, 5, "success", &json!({})));
        assert!(c.management["inventory"].is_null());
    }
    #[test]
    fn rejected_and_cancelled_jobs_cannot_change_resources() {
        let mut c = state();
        assert!(
            apply(
                &mut c,
                4,
                &request("tapes", "move", "TEST01L8", json!({"location":"ieslot"}))
            )
            .is_err()
        );
        apply(
            &mut c,
            5,
            &request("libraries", "rescan", "current", json!({})),
        )
        .unwrap();
        assert!(
            apply(
                &mut c,
                6,
                &request("drives", "role", "serial", json!({"role":"rr"}))
            )
            .is_err()
        );
        apply(&mut c, 7, &request("tasks", "cancel", "5", json!({}))).unwrap();
        assert_eq!(c.management["tasks"]["5"]["state"], "cancelled");
        assert!(!report(&mut c, 5, 3, "running", &Value::Null));
        assert!(
            apply(
                &mut c,
                8,
                &request("tasks", "clearhistory", "clearhistory", json!({}))
            )
            .is_err()
        );
        apply(
            &mut c,
            9,
            &request(
                "tasks",
                "clearhistory",
                "clearhistory",
                json!({"confirm":true}),
            ),
        )
        .unwrap();
        assert!(c.management["tasks"].as_object().unwrap().is_empty());
    }
    #[test]
    fn group_role_and_tape_policies_have_real_scheduling_effect() {
        let mut m = empty();
        assert!(drive_allowed(&m, "s", 1, "p", 'm'));
        m["drives"]["s"] = json!({"role":"rg"});
        assert!(!drive_allowed(&m, "s", 1, "p", 'm'));
        assert!(drive_allowed(&m, "s", 1, "p", 'r'));
        m["drives"]["s"]["enabled"] = json!(false);
        assert!(!drive_allowed(&m, "s", 1, "p", 'r'));
        m["drives"]["s"] = json!({"nodegroup":"G1"});
        assert!(!drive_allowed(&m, "s", 1, "p", 'r'));
        m["pools"]["p"] = json!({"nodegroup":"G1"});
        assert!(drive_allowed(&m, "s", 1, "p", 'r'));
        m["nodes"]["1"] = Value::Null;
        assert!(!drive_allowed(&m, "s", 1, "p", 'r'));
        m["tapes"]["b"] = json!({"blacklist":"fault"});
        assert!(!tape_allowed(&m, "b"));
    }
    #[test]
    fn drive_policy_changes_require_current_owner_inventory_and_known_serial() {
        let mut c = state();
        let params = json!({"role":"rg"});
        assert_eq!(
            validate_job(&c, "drives", "role", "new", &params).unwrap_err(),
            "驱动库存已过期，请先显式 rescan"
        );
        c.management["inventory"] = json!({"owner":1,"round":2,"drives":[{"serial":"old"}]});
        assert!(validate_job(&c, "drives", "role", "old", &params).is_err());
        c.management["inventory"]["round"] = json!(3);
        assert_eq!(
            validate_job(&c, "drives", "role", "new", &params).unwrap_err(),
            "驱动器不在当前库存中"
        );
        validate_job(&c, "drives", "role", "old", &params).unwrap();
        c.management["inventory"] = Value::Null;
        assert!(apply(&mut c, 4, &request("drives", "down", "old", json!({}))).is_err());
        assert!(
            c.management["drives"]
                .as_object()
                .is_none_or(|drives| drives.is_empty())
        );
    }
    #[test]
    fn command_wire_roundtrip_and_result_without_claim_are_rejected() {
        let mut c = state();
        let cmd = Command::Manage {
            request: request("libraries", "rescan", "current", json!({})),
        };
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd.clone()));
        assert!(matches!(c.apply(4, &cmd), Applied::Admin(Ok(_))));
        let report = Command::ManagementReport {
            task: 4,
            round: 3,
            phase: "success".into(),
            result: json!({}),
        };
        assert_eq!(Command::decode(&report.encode()), Some(report.clone()));
        assert_eq!(c.apply(5, &report), Applied::Nothing);
        let wrapped = Command::ExecutorReport {
            node: 1,
            instance: 1,
            round: 3,
            sequence: 1,
            command: Box::new(report),
        };
        assert_eq!(Command::decode(&wrapped.encode()), Some(wrapped));
    }
}
