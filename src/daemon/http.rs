//! 最小 HTTP/1.1 客户端接口（骨架用，不引入新依赖）。每个连接一个线程，一次请求后关闭。
//!
//! PUT  /files/<path>[?wait=1]   上传（默认替换）。202 = 已暂存；带 wait 时等到落带，201 = 已提交。
//!                               `If-None-Match: *` 为只创建，路径已有已提交版本时 412
//! GET  /files/<path>            读出已提交的文件内容；支持单段 `Range`（206）；只有在途版本时 409
//! GET  /stat/<path>             路径状态：committed / uploading / staged；404 = 不存在
//! GET  /tasks/<id>[?wait=1]     上传任务的状态
//! GET  /list[?dir=<d>[&pending=1]]  不带 dir：已提交视图的全部文件；带 dir：该目录的直接子项
//!
//! 语义见研究笔记 file-contract.md。
//! GET  /cluster                 本节点对集群的自述
//! GET  /admin/pools             池与磁带归属（任何节点可答，来自已应用的日志）
//! POST /admin/pools/<name>[?file_limit=N]      新建池（只有 Raft Leader 受理）
//! POST /admin/tapes/<barcode>?pool=<名称或UUID>  磁带归入池
//! DELETE /admin/tapes/<barcode>                 解除归属
//! POST /admin/tapes/<barcode>/reclaim           回收：把带上还活着的文件搬到同池其他带，再重新格式化它
//!
//! 不在服务的节点一律返回 503，并在 JSON 里给出它所知的 Leader 地址。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{info, warn};
use serde_json::{Value, json};

use super::executor::ExecRequest;
use super::files::{DirEntry, FileService, PathState, ServiceError, TaskStatus};
use super::net::{AdminReply, NodeInput};
use super::node::SharedStatus;
use super::state::{Command, DEFAULT_FILE_LIMIT};

pub struct HttpContext {
    pub files: Arc<FileService>,
    pub status: SharedStatus,
    pub exec: Mutex<Sender<ExecRequest>>,
    /// 通往 Raft 循环，用来提交管理命令
    pub node: Mutex<Sender<NodeInput>>,
    /// 节点编号 → 客户端接口地址，用来给出 Leader 提示
    pub client_addrs: HashMap<u64, String>,
    pub wait_timeout: Duration,
}

pub fn serve(listen: &str, ctx: Arc<HttpContext>) -> std::io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    info!("客户端接口监听 {}", listen);
    thread::Builder::new().name("ltfsd-http".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let ctx = ctx.clone();
            let _ = thread::Builder::new().name("ltfsd-http-conn".into()).spawn(move || {
                if let Err(e) = handle(conn, &ctx) {
                    warn!("HTTP 连接出错: {}", e);
                }
            });
        }
    })?;
    Ok(())
}

fn respond(conn: &mut TcpStream, code: u16, reason: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        conn,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code,
        reason,
        ctype,
        body.len()
    )?;
    conn.write_all(body)
}

/// 206：`body` 是 `[start, start + body.len())` 这一段，`total` 是文件全长。
fn respond_range(conn: &mut TcpStream, start: u64, total: u64, body: &[u8]) -> std::io::Result<()> {
    let end = start + body.len() as u64;
    write!(
        conn,
        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        start,
        end.saturating_sub(1),
        total,
        body.len()
    )?;
    conn.write_all(body)?;
    conn.flush()
}

/// 单段 `Range: bytes=a-b` / `bytes=a-` / `bytes=-n`，按全长 `total` 解析成 `[start, end)`。
/// 多段或写法不认识时返回 `None`（按 RFC 9110 可以当没有 Range，回整个文件）；
/// 起点越过文件末尾时返回 `Some(Err(()))`（416）。
pub fn parse_range(v: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = v.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return Some(Err(()));
        }
        return Some(Ok((total.saturating_sub(n), total)));
    }
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() { total } else { b.parse::<u64>().ok()?.saturating_add(1).min(total) };
    if start >= total || end <= start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

fn respond_json(conn: &mut TcpStream, code: u16, reason: &str, v: Value) -> std::io::Result<()> {
    let mut body = v.to_string().into_bytes();
    body.push(b'\n');
    respond(conn, code, reason, "application/json", &body)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 1) && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn service_error(conn: &mut TcpStream, ctx: &HttpContext, e: ServiceError) -> std::io::Result<()> {
    match e {
        ServiceError::NotServing(why) => {
            let leader = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).leader;
            let url = ctx.client_addrs.get(&leader).map(|a| format!("http://{}", a));
            respond_json(conn, 503, "Service Unavailable", json!({"error": "not_serving", "detail": why, "leader": leader, "leader_url": url}))
        }
        ServiceError::SwitchingTape(why) => {
            // 503 且不给 Leader 提示：客户端库会稍后向同一个节点重试
            respond_json(conn, 503, "Service Unavailable", json!({"error": "switching_tape", "detail": why}))
        }
        ServiceError::NoTape(why) => respond_json(conn, 507, "Insufficient Storage", json!({"error": "no_tape", "detail": why})),
        ServiceError::TooLarge { requested, tape_capacity } => respond_json(
            conn,
            507,
            "Insufficient Storage",
            json!({"error": "too_large", "requested": requested, "tape_capacity": tape_capacity}),
        ),
        ServiceError::PathBusy(p) => respond_json(conn, 409, "Conflict", json!({"error": "path_busy", "path": p})),
        ServiceError::Exists(p) => respond_json(conn, 412, "Precondition Failed", json!({"error": "exists", "path": p})),
        ServiceError::InsufficientCapacity { requested, available } => respond_json(
            conn,
            507,
            "Insufficient Storage",
            json!({"error": "insufficient_capacity", "requested": requested, "available": available}),
        ),
        ServiceError::NotWritable(why) => respond_json(conn, 503, "Service Unavailable", json!({"error": "read_only", "detail": why})),
        ServiceError::BadPath(p) => respond_json(conn, 400, "Bad Request", json!({"error": "bad_path", "path": p})),
        ServiceError::Io(e) => respond_json(conn, 500, "Internal Server Error", json!({"error": "spool_io", "detail": e})),
    }
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// 把管理命令交给 Raft 循环并等结论。只有 Raft Leader 受理；不是 Leader 时答 503 并指出 Leader。
fn admin(conn: &mut TcpStream, ctx: &HttpContext, cmd: Command) -> std::io::Result<()> {
    let (tx, rx) = channel();
    if ctx.node.lock().unwrap_or_else(|e| e.into_inner()).send(NodeInput::Admin { cmd, reply: tx }).is_err() {
        return respond_json(conn, 503, "Service Unavailable", json!({"error": "node_gone"}));
    }
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(AdminReply::Ok(result)) => respond_json(conn, 200, "OK", json!({"result": result})),
        Ok(AdminReply::Rejected(why)) => respond_json(conn, 409, "Conflict", json!({"error": "rejected", "detail": why})),
        Ok(AdminReply::NotLeader(leader)) => {
            let url = ctx.client_addrs.get(&leader).map(|a| format!("http://{}", a));
            respond_json(conn, 503, "Service Unavailable", json!({"error": "not_leader", "detail": "管理命令只由 Raft Leader 受理", "leader": leader, "leader_url": url}))
        }
        // 超时或 Leader 身份中途丢失：命令可能已提交也可能没有，请调用方查询 /admin/pools 后决定
        Err(_) => respond_json(conn, 504, "Gateway Timeout", json!({"error": "indeterminate", "detail": "命令结论未知，请查询后决定是否重发"})),
    }
}

fn task_json(id: u64, st: &TaskStatus) -> (u16, &'static str, Value) {
    match st {
        TaskStatus::Staged => (202, "Accepted", json!({"task": id, "status": "staged"})),
        TaskStatus::Committed { generation } => (201, "Created", json!({"task": id, "status": "committed", "generation": generation})),
        TaskStatus::Failed { reason } => (500, "Internal Server Error", json!({"task": id, "status": "failed", "detail": reason})),
        TaskStatus::Indeterminate { reason } => {
            (500, "Internal Server Error", json!({"task": id, "status": "indeterminate", "detail": reason}))
        }
    }
}

fn handle(conn: TcpStream, ctx: &HttpContext) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut conn = conn;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return respond_json(&mut conn, 400, "Bad Request", json!({"error": "bad_request"}));
    };
    let method = method.to_string();
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    let wait = query.split('&').any(|kv| kv == "wait=1");
    let path = percent_decode(raw_path);
    let mut content_length: Option<u64> = None;
    let mut expects_continue = false;
    let mut create_only = false;
    let mut range: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().ok();
            }
            if k.eq_ignore_ascii_case("expect") && v.trim().eq_ignore_ascii_case("100-continue") {
                // 先不回：等准入成功再让客户端发内容；被拒绝就直接回最终结论
                expects_continue = true;
            }
            if k.eq_ignore_ascii_case("if-none-match") && v.trim() == "*" {
                create_only = true;
            }
            if k.eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/cluster") => {
            let s = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            respond_json(
                &mut conn,
                200,
                "OK",
                json!({
                    "id": s.id, "role": s.role, "term": s.term, "leader": s.leader, "local": s.local,
                    "executor": s.executor.map(|e| json!({"node": e.node, "round": e.round, "fenced": e.fenced})),
                    "serving_round": ctx.files.serving_round(),
                    "drives": s.drives,
                    "last_reclaim": s.last_reclaim.map(|r| json!({"round": r.round, "barcode": r.barcode, "outcome": r.outcome, "detail": r.detail})),
                }),
            )
        }
        ("GET", "/admin/pools") => {
            let s = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let pools: Vec<Value> = s
                .pools
                .iter()
                .map(|p| {
                    let tapes: Vec<Value> = p
                        .tapes
                        .iter()
                        .map(|b| match s.tapes.get(b) {
                            Some(t) => {
                                // 可回收空间 = 带上实际写掉的字节 − 目录里还指向这盘带的字节。
                                // 差额有三个来源：同一盘带上被重写的旧副本、当前版本已经在别的
                                // 带上的旧副本，以及历次收尾放弃的块。容量读数拿不到时（还没装载过）
                                // 退回索引里的字节数，那只算得出后一种。
                                let (live_files, live_bytes) = ctx.files.live_on(b).unwrap_or((0, 0));
                                let written = t.bytes_written.max(t.bytes_used);
                                json!({
                                    "barcode": b, "state": t.state, "generation": t.generation, "files": t.files,
                                    "bytes_used": t.bytes_used, "bytes_written": t.bytes_written,
                                    "live_files": live_files, "live_bytes": live_bytes,
                                    "reclaimable_bytes": written.saturating_sub(live_bytes),
                                })
                            }
                            None => json!({"barcode": b}),
                        })
                        .collect();
                    json!({"uuid": p.uuid, "name": p.name, "file_limit": p.file_limit, "tapes": p.tapes, "tape_details": tapes})
                })
                .collect();
            respond_json(&mut conn, 200, "OK", json!({"pools": pools, "applied_index": s.applied_index, "answered_by": s.id}))
        }
        ("POST", p) if p.starts_with("/admin/pools/") => {
            let file_limit = query_param(query, "file_limit").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_FILE_LIMIT);
            let cmd = Command::PoolCreate { uuid: uuid::Uuid::new_v4().to_string(), name: p[13..].to_string(), file_limit };
            admin(&mut conn, ctx, cmd)
        }
        ("POST", p) if p.starts_with("/admin/tapes/") && p.ends_with("/reclaim") => {
            let barcode = p[13..p.len() - "/reclaim".len()].to_string();
            admin(&mut conn, ctx, Command::TapeReclaim { barcode })
        }
        ("POST", p) if p.starts_with("/admin/tapes/") => match query_param(query, "pool") {
            Some(pool) => admin(&mut conn, ctx, Command::TapeAssign { barcode: p[13..].to_string(), pool: percent_decode(pool) }),
            None => respond_json(&mut conn, 400, "Bad Request", json!({"error": "pool_required"})),
        },
        ("DELETE", p) if p.starts_with("/admin/tapes/") => {
            admin(&mut conn, ctx, Command::TapeUnassign { barcode: p[13..].to_string() })
        }
        ("GET", "/list") => match query_param(query, "dir") {
            Some(dir) => {
                let dir = percent_decode(dir);
                let pending = query_param(query, "pending") == Some("1");
                match ctx.files.list_dir(&dir, pending) {
                    Ok(Some(entries)) => {
                        let v: Vec<Value> = entries
                            .into_iter()
                            .map(|(name, e)| match e {
                                DirEntry::Dir => json!({"name": name, "type": "dir"}),
                                DirEntry::File { committed, in_flight } => {
                                    let mut j = json!({"name": name, "type": "file", "state": in_flight.map(|f| f.0.as_str()).unwrap_or("committed")});
                                    if let Some(n) = committed {
                                        j["committed_length"] = json!(n);
                                    }
                                    if let Some((_, n)) = in_flight {
                                        j["staged_length"] = json!(n);
                                    }
                                    j
                                }
                            })
                            .collect();
                        respond_json(&mut conn, 200, "OK", json!({"dir": dir, "entries": v}))
                    }
                    Ok(None) => respond_json(&mut conn, 404, "Not Found", json!({"error": "not_found", "dir": dir})),
                    Err(e) => service_error(&mut conn, ctx, e),
                }
            }
            None => match ctx.files.list() {
                Ok(m) => respond_json(&mut conn, 200, "OK", json!({"files": m})),
                Err(e) => service_error(&mut conn, ctx, e),
            },
        },
        ("GET", p) if p.starts_with("/stat/") => match ctx.files.stat_full(&p[5..]) {
            Ok(PathState { committed, in_flight }) => {
                let committed_json = committed.as_ref().map(|s| {
                    json!({"length": s.len, "generation": s.generation, "round": s.round, "barcode": s.barcode, "sha256": s.sha256})
                });
                match (in_flight, committed_json) {
                    (None, None) => respond_json(&mut conn, 404, "Not Found", json!({"path": &p[5..], "committed": false})),
                    // 已提交且没有新版本在途：字段放在顶层（与旧格式相同）
                    (None, Some(mut j)) => {
                        j["path"] = json!(&p[5..]);
                        j["state"] = json!("committed");
                        j["committed"] = json!(true);
                        respond_json(&mut conn, 200, "OK", j)
                    }
                    // 新版本在途：顶层是在途的状态与已接收长度，已提交的当前版本（如有）放在 current
                    (Some((st, len)), cur) => {
                        let mut j = json!({"path": &p[5..], "state": st.as_str(), "length": len, "committed": false});
                        if let Some(c) = cur {
                            j["current"] = c;
                        }
                        respond_json(&mut conn, 200, "OK", j)
                    }
                }
            }
            Err(e) => service_error(&mut conn, ctx, e),
        },
        ("GET", p) if p.starts_with("/tasks/") => {
            let Ok(id) = p[7..].parse::<u64>() else {
                return respond_json(&mut conn, 400, "Bad Request", json!({"error": "bad_task_id"}));
            };
            let st = if wait { ctx.files.wait_task(id, ctx.wait_timeout) } else { ctx.files.task(id) };
            match st {
                // 查询本身成功；任务的结论在 JSON 里
                Some(st) => respond_json(&mut conn, 200, "OK", task_json(id, &st).2),
                None => respond_json(&mut conn, 404, "Not Found", json!({"error": "unknown_task", "detail": "任务记录不跨节点、不跨进程保留"})),
            }
        }
        ("GET", p) if p.starts_with("/files/") => {
            match ctx.files.stat_full(&p[6..]) {
                Err(e) => return service_error(&mut conn, ctx, e),
                Ok(PathState { committed: None, in_flight: Some((st, _)) }) => {
                    return respond_json(&mut conn, 409, "Conflict", json!({"error": "not_committed", "path": &p[6..], "state": st.as_str()}));
                }
                Ok(PathState { committed: None, in_flight: None }) => {
                    return respond_json(&mut conn, 404, "Not Found", json!({"error": "not_found", "path": &p[6..]}));
                }
                Ok(_) => {}
            }
            let (tx, rx) = channel();
            let sent = ctx.exec.lock().unwrap_or_else(|e| e.into_inner()).send(ExecRequest::Read { path: p[6..].to_string(), reply: tx });
            if sent.is_err() {
                return respond_json(&mut conn, 503, "Service Unavailable", json!({"error": "executor_gone"}));
            }
            match rx.recv_timeout(ctx.wait_timeout) {
                // TODO：按范围只读需要的块，而不是整个文件读出来再切
                Ok(Ok(bytes)) => match range.as_deref().and_then(|r| parse_range(r, bytes.len() as u64)) {
                    Some(Ok((a, b))) => respond_range(&mut conn, a, bytes.len() as u64, &bytes[a as usize..b as usize]),
                    Some(Err(())) => respond_json(&mut conn, 416, "Range Not Satisfiable", json!({"error": "bad_range", "length": bytes.len()})),
                    None => respond(&mut conn, 200, "OK", "application/octet-stream", &bytes),
                },
                Ok(Err(tape_rs_read_busy @ super::executor::ReadError::Busy(_))) => {
                    respond_json(&mut conn, 503, "Service Unavailable", json!({"error": "drive_busy", "detail": tape_rs_read_busy.to_string()}))
                }
                Ok(Err(e)) => respond_json(&mut conn, 404, "Not Found", json!({"error": "read_failed", "detail": e.to_string()})),
                Err(_) => respond_json(&mut conn, 504, "Gateway Timeout", json!({"error": "read_timeout"})),
            }
        }
        ("PUT", p) if p.starts_with("/files/") => {
            let Some(len) = content_length else {
                return respond_json(&mut conn, 411, "Length Required", json!({"error": "length_required"}));
            };
            let h = match ctx.files.begin_with(&p[6..], len, create_only) {
                Ok(h) => h,
                Err(e) => {
                    // 没有用 Expect 的客户端此刻正在发内容。不读掉就关连接，对方收到的是连接重置，
                    // 而不是这条错误。
                    // 内容太大就不读了：读完之前对方一直收不到错误，不如让它尽早失败。
                    if !expects_continue && len <= 256 << 20 {
                        let _ = std::io::copy(&mut (&mut reader).take(len), &mut std::io::sink());
                    }
                    return service_error(&mut conn, ctx, e);
                }
            };
            if expects_continue {
                conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            }
            let spooled = (|| -> Result<(), ServiceError> {
                let mut f = std::fs::File::create(&h.spool).map_err(|e| ServiceError::Io(e.to_string()))?;
                let mut left = len;
                let mut buf = vec![0u8; 256 * 1024];
                while left > 0 {
                    let want = buf.len().min(left as usize);
                    let n = reader.read(&mut buf[..want]).map_err(|e| ServiceError::Io(e.to_string()))?;
                    if n == 0 {
                        return Err(ServiceError::Io("连接在内容传完之前关闭".into()));
                    }
                    f.write_all(&buf[..n]).map_err(|e| ServiceError::Io(e.to_string()))?;
                    ctx.files.ingest(&h, n as u64)?;
                    left -= n as u64;
                }
                f.sync_all().map_err(|e| ServiceError::Io(e.to_string()))
            })();
            if let Err(e) = spooled {
                ctx.files.abort(h);
                return service_error(&mut conn, ctx, e);
            }
            let task = match ctx.files.finish(h, len) {
                Ok(t) => t,
                Err(e) => return service_error(&mut conn, ctx, e),
            };
            let st = if wait { ctx.files.wait_task(task, ctx.wait_timeout) } else { ctx.files.task(task) };
            let (code, reason, body) = task_json(task, &st.unwrap_or(TaskStatus::Staged));
            respond_json(&mut conn, code, reason, body)
        }
        _ => respond_json(&mut conn, 404, "Not Found", json!({"error": "no_such_route"})),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn range_forms() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 10))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 100))));
        assert_eq!(parse_range("bytes=95-200", 100), Some(Ok((95, 100))), "终点越过末尾就截到末尾");
        assert_eq!(parse_range("bytes=-5", 100), Some(Ok((95, 100))));
        assert_eq!(parse_range("bytes=-500", 100), Some(Ok((0, 100))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())), "起点在末尾");
        assert_eq!(parse_range("bytes=5-2", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None, "多段当没有 Range");
        assert_eq!(parse_range("items=0-1", 100), None);
    }
}
