//! axum HTTP/1.1 接口。协议解析异步执行，同步业务由有界阻塞任务处理；一次请求后关闭。
//!
//! PUT  /files/<path>[?wait=1]   上传（默认替换）。202 = 已暂存；带 wait 时等到落带，201 = 已提交。
//!                               `If-None-Match: *` 为只创建，路径已有已提交版本时 412
//! GET  /files/<path>            读出已提交的文件内容；支持单段 `Range`（206）；只有在途版本时 409
//! DELETE /files/<path>[?wait=1] 删除：202 = 墓碑已暂存，200 = 墓碑已落带
//! POST/DELETE /directories/<path> 创建/删除空目录；201 = 索引已提交，202 = 仍在暂存
//! GET  /stat/<path>             路径状态：committed / uploading / staged；404 = 不存在
//! GET  /tasks/<id>[?wait=1]     上传任务的状态
//! GET  /list[?dir=<d>[&pending=1]]  不带 dir：已提交视图的全部文件；带 dir：该目录的直接子项
//!
//! 语义见研究笔记 file-contract.md。
//! GET  /cluster                 本节点对集群的自述
//! GET  /readyz                  HTTP VIP 就绪：本机是已恢复且许可有效的 owner 时 200，否则 503
//! GET  /admin/pools             池与磁带归属（任何节点可答，来自已应用的日志）
//! POST /admin/pools/<name>[?file_limit=N]      新建池（只有 Raft Leader 受理）
//! POST /admin/tapes/<barcode>?pool=<名称或UUID>  磁带归入池
//! DELETE /admin/tapes/<barcode>                 解除归属
//! POST /admin/tapes/<barcode>/reclaim           回收：把带上还活着的文件搬到同池其他带，再重新格式化它
//!
//! 数据请求不在服务时返回 503，提示设备 owner 地址；管理请求仍提示控制 Leader。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header, request::Parts};
use axum::response::Response;
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use log::{info, warn};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;

use super::executor::ExecRequest;
use super::files::{DirEntry, FileService, PathState, ServiceError, TaskStatus};
use super::net::{AdminReply, NodeInput};
use super::node::{NodeStatus, SharedStatus};
use super::state::{Command, DEFAULT_FILE_LIMIT};

pub struct HttpContext {
    pub files: Arc<FileService>,
    pub status: SharedStatus,
    pub exec: Mutex<Sender<ExecRequest>>,
    /// 通往 Raft 循环，用来提交管理命令
    pub node: Mutex<Sender<NodeInput>>,
    /// 节点编号 → 客户端接口地址，用来给出控制 Leader / 数据 owner 提示
    pub client_addrs: HashMap<u64, String>,
    pub wait_timeout: Duration,
}

/// VIP 与 HTTP 就绪检查共用资格判定，不以控制 Leader 或可写磁带作为条件。
pub(super) fn ready_owner(
    s: &NodeStatus,
    files: &FileService,
) -> Option<(u64, std::time::Instant)> {
    let owner = s.executor.as_ref()?;
    if s.shutting_down || !s.executor_ready || owner.node != s.id || !owner.fenced {
        return None;
    }
    files
        .serving_deadline(owner.round)
        .map(|deadline| (owner.round, deadline))
}

// 固定内部资源上限，不把 HTTP 并发映射为设备并发。
const MAX_CONNECTIONS: usize = 256;
const MAX_REQUESTS: usize = 32;
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct HttpState {
    ctx: Arc<HttpContext>,
    requests: Arc<Semaphore>,
}

fn router(ctx: Arc<HttpContext>) -> Router {
    Router::new().fallback(dispatch).with_state(HttpState {
        ctx,
        requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
    })
}

pub fn serve(listen: &str, ctx: Arc<HttpContext>) -> std::io::Result<()> {
    // 绑定与运行时创建在调用线程完成，启动失败仍同步返回给调用方。
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(MAX_REQUESTS + 8)
        .enable_all()
        .build()?;
    info!("客户端接口监听 {}", listen);
    thread::Builder::new()
        .name("ltfsd-http".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(e) => {
                        warn!("HTTP 监听器启动失败: {}", e);
                        return;
                    }
                };
                let app = router(ctx);
                let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
                loop {
                    let (conn, _) = match listener.accept().await {
                        Ok(conn) => conn,
                        Err(e) => {
                            warn!("HTTP 接受连接失败: {}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        // 不为超限连接创建等待任务。
                        drop(conn);
                        continue;
                    };
                    let app = app.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        serve_connection(conn, app).await;
                    });
                }
            });
        })?;
    Ok(())
}

async fn serve_connection(conn: tokio::net::TcpStream, app: Router) {
    let mut http = hyper::server::conn::http1::Builder::new();
    http.keep_alive(false)
        .title_case_headers(true)
        .max_buf_size(32 * 1024)
        .max_header_size(32 * 1024)
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    if let Err(e) = http
        .serve_connection(TokioIo::new(conn), TowerToHyperService::new(app))
        .await
    {
        warn!("HTTP 连接出错: {}", e);
    }
}

/// 许可随响应体一起存活，慢下载也计入并发上限。
struct LimitedBody {
    body: Body,
    _permit: OwnedSemaphorePermit,
}

impl HttpBody for LimitedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

async fn dispatch(State(state): State<HttpState>, req: Request) -> Response {
    // 健康检查不排在上传/设备等待之后，资格判定与 VIP 完全共用。
    if req.method() == "GET" && req.uri().path() == "/readyz" {
        return ready_response(&state.ctx);
    }
    let Ok(permit) = state.requests.try_acquire_owned() else {
        return json_response(
            503,
            json!({"error":"http_busy", "detail":"HTTP 并发请求已达上限"}),
        );
    };
    let (parts, body) = req.into_parts();
    // 不对在途写操作套整体超时：连接消失不意味着设备操作已取消。
    match tokio::task::spawn_blocking(move || {
        let mut response = Response::new(Body::empty());
        let mut reader = BodyReader {
            body,
            pending: Bytes::new(),
        };
        if let Err(e) = handle(parts, &mut reader, &mut response, &state.ctx) {
            warn!("HTTP 请求出错: {}", e);
            response = json_response(400, json!({"error":"bad_request", "detail":e.to_string()}));
        }
        let (parts, body) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(LimitedBody {
                body,
                _permit: permit,
            }),
        )
    })
    .await
    {
        Ok(response) => response,
        Err(e) => {
            warn!("HTTP 请求任务失败: {}", e);
            json_response(
                500,
                json!({"error":"indeterminate", "status":"indeterminate", "detail":"HTTP 请求任务异常，操作结果未定"}),
            )
        }
    }
}

/// 只在 spawn_blocking 内跨接异步请求体；逐帧读取，避免整文件缓冲。
struct BodyReader {
    body: Body,
    pending: Bytes,
}

impl Read for BodyReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            let frame = tokio::runtime::Handle::current()
                .block_on(async {
                    tokio::time::timeout(BODY_IDLE_TIMEOUT, self.body.frame()).await
                })
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "HTTP 内容接收超时")
                })?;
            match frame {
                None => return Ok(0),
                Some(Err(e)) => return Err(std::io::Error::other(e)),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.pending = data;
                    }
                }
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending.split_to(n));
        Ok(n)
    }
}

fn json_response(code: u16, v: Value) -> Response {
    let mut response = Response::new(Body::empty());
    respond_json(&mut response, code, "", v).expect("固定 HTTP 响应头合法");
    response
}

fn respond(
    conn: &mut Response,
    code: u16,
    _reason: &str,
    ctype: &str,
    body: Vec<u8>,
) -> std::io::Result<()> {
    *conn = Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CONNECTION, "close")
        .body(Body::from(body))
        .map_err(std::io::Error::other)?;
    Ok(())
}

/// 暂存文件若在发送中意外截断，必须报告错误，不能把短内容当作成功结束。
struct DownloadFile(tokio::io::Take<tokio::fs::File>);

impl tokio::io::AsyncRead for DownloadFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let has_room = buf.remaining() > 0;
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Ok(()))
                if has_room && buf.filled().len() == before && self.0.limit() != 0 =>
            {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "下载暂存内容不完整",
                )))
            }
            result => result,
        }
    }
}

/// 磁带读完后再发送；Range 从磁盘切片，内存不随文件大小增长。
fn respond_file(
    conn: &mut Response,
    mut file: std::fs::File,
    range: Option<&str>,
) -> std::io::Result<()> {
    let total = file.metadata()?.len();
    let (start, end, partial) = match range.and_then(|r| parse_range(r, total)) {
        Some(Err(())) => {
            return respond_json(
                conn,
                416,
                "Range Not Satisfiable",
                json!({"error":"bad_range", "length":total}),
            );
        }
        Some(Ok((start, end))) => (start, end, true),
        None => (0, total, false),
    };
    file.seek(SeekFrom::Start(start))?;
    let file = tokio::fs::File::from_std(file);
    let stream = ReaderStream::with_capacity(
        DownloadFile(tokio::io::AsyncReadExt::take(file, end - start)),
        256 * 1024,
    );
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, end - start)
        .header(header::CONNECTION, "close");
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end - 1, total),
        );
    }
    *conn = response
        .body(Body::from_stream(stream))
        .map_err(std::io::Error::other)?;
    Ok(())
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
        if n == 0 || total == 0 {
            return Some(Err(()));
        }
        return Some(Ok((total.saturating_sub(n), total)));
    }
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        total
    } else {
        b.parse::<u64>().ok()?.saturating_add(1).min(total)
    };
    if start >= total || end <= start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

fn respond_json(conn: &mut Response, code: u16, reason: &str, v: Value) -> std::io::Result<()> {
    let mut body = v.to_string().into_bytes();
    body.push(b'\n');
    respond(conn, code, reason, "application/json", body)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && s.is_char_boundary(i + 1)
            && s.is_char_boundary(i + 3)
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn service_error(conn: &mut Response, ctx: &HttpContext, e: ServiceError) -> std::io::Result<()> {
    match e {
        ServiceError::NotServing(why) => {
            let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let leader = status.leader;
            let owner = status.executor.as_ref().map(|e| e.node);
            let url = ctx
                .client_addrs
                .get(&owner.unwrap_or(0))
                .map(|a| format!("http://{}", a));
            respond_json(
                conn,
                503,
                "Service Unavailable",
                // leader_url 保持旧客户端的重定向字段；数据请求指向 owner，不是控制 Leader。
                json!({"error": "not_serving", "detail": why, "leader": leader, "owner": owner, "owner_url":url, "leader_url": url}),
            )
        }
        ServiceError::SwitchingTape(why) => {
            // 503 且不给 Leader 提示：客户端库会稍后向同一个节点重试
            respond_json(
                conn,
                503,
                "Service Unavailable",
                json!({"error": "switching_tape", "detail": why}),
            )
        }
        ServiceError::NoAttribute(name) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error":"no_attribute", "name":name}),
        ),
        ServiceError::ProtectedAttribute(name) => respond_json(
            conn,
            403,
            "Forbidden",
            json!({"error":"protected_attribute", "name":name}),
        ),
        ServiceError::AttributeTooLarge => respond_json(
            conn,
            413,
            "Payload Too Large",
            json!({"error":"attribute_too_large"}),
        ),
        ServiceError::CrossDevice(path) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error":"cross_device", "path":path}),
        ),
        ServiceError::NoTape(why) => respond_json(
            conn,
            507,
            "Insufficient Storage",
            json!({"error": "no_tape", "detail": why}),
        ),
        ServiceError::TooLarge {
            requested,
            tape_capacity,
        } => respond_json(
            conn,
            507,
            "Insufficient Storage",
            json!({"error": "too_large", "requested": requested, "tape_capacity": tape_capacity}),
        ),
        ServiceError::NotEmpty(p) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error": "not_empty", "path": p}),
        ),
        ServiceError::IsDirectory(p) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error": "is_directory", "path": p}),
        ),
        ServiceError::NotDirectory(p) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error": "not_directory", "path": p}),
        ),
        ServiceError::PathBusy(p) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error": "path_busy", "path": p}),
        ),
        ServiceError::Exists(p) => respond_json(
            conn,
            412,
            "Precondition Failed",
            json!({"error": "exists", "path": p}),
        ),
        ServiceError::NotFound(p) => respond_json(
            conn,
            404,
            "Not Found",
            json!({"error": "not_found", "path": p}),
        ),
        ServiceError::InsufficientCapacity {
            requested,
            available,
        } => respond_json(
            conn,
            507,
            "Insufficient Storage",
            json!({"error": "insufficient_capacity", "requested": requested, "available": available}),
        ),
        ServiceError::NotWritable(why) => respond_json(
            conn,
            503,
            "Service Unavailable",
            json!({"error": "read_only", "detail": why}),
        ),
        ServiceError::BadPath(p) => respond_json(
            conn,
            400,
            "Bad Request",
            json!({"error": "bad_path", "path": p}),
        ),
        ServiceError::Io(e) => respond_json(
            conn,
            500,
            "Internal Server Error",
            json!({"error": "spool_io", "detail": e}),
        ),
    }
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// 把管理命令交给 Raft 循环并等结论。只有 Raft Leader 受理；不是 Leader 时答 503 并指出 Leader。
fn admin(conn: &mut Response, ctx: &HttpContext, cmd: Command) -> std::io::Result<()> {
    let (tx, rx) = channel();
    if ctx
        .node
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(NodeInput::Admin { cmd, reply: tx })
        .is_err()
    {
        return respond_json(
            conn,
            503,
            "Service Unavailable",
            json!({"error": "node_gone"}),
        );
    }
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(AdminReply::Ok(result)) => respond_json(conn, 200, "OK", json!({"result": result})),
        Ok(AdminReply::Rejected(why)) => respond_json(
            conn,
            409,
            "Conflict",
            json!({"error": "rejected", "detail": why}),
        ),
        Ok(AdminReply::NotLeader(leader)) => {
            let url = ctx
                .client_addrs
                .get(&leader)
                .map(|a| format!("http://{}", a));
            respond_json(
                conn,
                503,
                "Service Unavailable",
                json!({"error": "not_leader", "detail": "管理命令只由 Raft Leader 受理", "leader": leader, "leader_url": url}),
            )
        }
        // 超时或 Leader 身份中途丢失：命令可能已提交也可能没有，请调用方查询 /admin/pools 后决定
        Err(_) => respond_json(
            conn,
            504,
            "Gateway Timeout",
            json!({"error": "indeterminate", "detail": "命令结论未知，请查询后决定是否重发"}),
        ),
    }
}

fn task_json(id: u64, st: &TaskStatus) -> (u16, &'static str, Value) {
    match st {
        TaskStatus::Staged => (202, "Accepted", json!({"task": id, "status": "staged"})),
        TaskStatus::Committed { generation } => (
            201,
            "Created",
            json!({"task": id, "status": "committed", "generation": generation}),
        ),
        TaskStatus::Failed { reason } => (
            500,
            "Internal Server Error",
            json!({"task": id, "status": "failed", "detail": reason}),
        ),
        TaskStatus::Indeterminate { reason } => (
            500,
            "Internal Server Error",
            json!({"task": id, "status": "indeterminate", "detail": reason}),
        ),
    }
}

fn ready_response(ctx: &HttpContext) -> Response {
    let s = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let owner = s.executor.as_ref();
    let ready = ready_owner(&s, &ctx.files).is_some();
    json_response(
        if ready { 200 } else { 503 },
        json!({
            "ready": ready, "id": s.id,
            "owner": owner.map(|e| e.node), "round": owner.map(|e| e.round),
            "shutting_down": s.shutting_down,
            "vips": s.vips.iter().map(|(a,r)| json!({"address":a.to_string(),"token":r.token,"holder":r.holder.as_ref().map(|h|json!({"node":h.node,"round":h.round}))})).collect::<Vec<_>>(),
        }),
    )
}

fn handle(
    parts: Parts,
    reader: &mut dyn Read,
    conn: &mut Response,
    ctx: &HttpContext,
) -> std::io::Result<()> {
    let method = parts.method.to_string();
    let raw_path = parts.uri.path();
    let query = parts.uri.query().unwrap_or("");
    let wait = query.split('&').any(|kv| kv == "wait=1");
    let path = percent_decode(raw_path);
    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let expects_continue = parts
        .headers
        .get(header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));
    let create_only = parts
        .headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v == "*");
    let range = parts
        .headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    match (method.as_str(), path.as_str()) {
        ("GET", "/readyz") => {
            *conn = ready_response(ctx);
            Ok(())
        }
        ("POST", "/sync") => {
            let Some(round) = ctx.files.serving_round() else {
                return service_error(
                    conn,
                    ctx,
                    ServiceError::NotServing("同步目标尚未接管".into()),
                );
            };
            let (tx, rx) = channel();
            if ctx
                .exec
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send(ExecRequest::Sync { round, reply: tx })
                .is_err()
            {
                return respond_json(
                    conn,
                    503,
                    "Service Unavailable",
                    json!({"error":"executor_gone"}),
                );
            }
            match rx.recv_timeout(ctx.wait_timeout) {
                Ok(Ok(())) => {
                    respond_json(conn, 200, "OK", json!({"status":"synced","round":round}))
                }
                Ok(Err(reason)) => respond_json(
                    conn,
                    500,
                    "Internal Server Error",
                    json!({"error":"indeterminate","detail":reason}),
                ),
                Err(_) => respond_json(
                    conn,
                    504,
                    "Gateway Timeout",
                    json!({"error":"indeterminate","detail":"同步结果未定"}),
                ),
            }
        }
        ("GET", "/cluster") => {
            let s = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            respond_json(
                conn,
                200,
                "OK",
                json!({
                    "id": s.id, "role": s.role, "term": s.term, "leader": s.leader, "local": s.local,
                    "shutting_down": s.shutting_down,
                    "executor": s.executor.map(|e| json!({"node": e.node, "round": e.round, "fenced": e.fenced})),
                    "executor_ready":s.executor_ready,
                    "candidates":s.candidates.iter().map(|(id,c)|json!({"node":id,"instance":c.instance,"group":c.group,"drives":c.drives,"eligible":c.eligible})).collect::<Vec<_>>(),
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
            respond_json(
                conn,
                200,
                "OK",
                json!({"pools": pools, "applied_index": s.applied_index, "answered_by": s.id}),
            )
        }
        ("POST", p) if p.starts_with("/admin/pools/") => {
            let file_limit = query_param(query, "file_limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_FILE_LIMIT);
            let cmd = Command::PoolCreate {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: p[13..].to_string(),
                file_limit,
            };
            admin(conn, ctx, cmd)
        }
        ("POST", p) if p.starts_with("/admin/tapes/") && p.ends_with("/reclaim") => {
            let barcode = p[13..p.len() - "/reclaim".len()].to_string();
            admin(conn, ctx, Command::TapeReclaim { barcode })
        }
        ("POST", p) if p.starts_with("/admin/tapes/") => match query_param(query, "pool") {
            Some(pool) => admin(
                conn,
                ctx,
                Command::TapeAssign {
                    barcode: p[13..].to_string(),
                    pool: percent_decode(pool),
                },
            ),
            None => respond_json(conn, 400, "Bad Request", json!({"error": "pool_required"})),
        },
        ("DELETE", p) if p.starts_with("/admin/tapes/") => admin(
            conn,
            ctx,
            Command::TapeUnassign {
                barcode: p[13..].to_string(),
            },
        ),
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
                                DirEntry::File { committed, in_flight, is_symlink } => {
                                    let mut j = json!({"name": name, "type": "file", "is_symlink": is_symlink, "state": in_flight.map(|f| f.0.as_str()).unwrap_or("committed")});
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
                        respond_json(conn, 200, "OK", json!({"dir": dir, "entries": v}))
                    }
                    Ok(None) => respond_json(
                        conn,
                        404,
                        "Not Found",
                        json!({"error": "not_found", "dir": dir}),
                    ),
                    Err(e) => service_error(conn, ctx, e),
                }
            }
            None => match ctx.files.list() {
                Ok(m) => respond_json(conn, 200, "OK", json!({"files": m})),
                Err(e) => service_error(conn, ctx, e),
            },
        },
        ("GET", p) if p.starts_with("/stat/") => match ctx.files.stat_full(&p[5..]) {
            Ok(PathState {
                committed,
                in_flight,
            }) => {
                let committed_json = committed.as_ref().map(|s| {
                    json!({"length": s.len, "generation": s.generation, "round": s.round, "barcode": s.barcode, "sha256": s.sha256, "version": s.version, "metadata": s.metadata})
                });
                match (in_flight, committed_json) {
                    (None, None) => respond_json(
                        conn,
                        404,
                        "Not Found",
                        json!({"path": &p[5..], "committed": false}),
                    ),
                    // 已提交且没有新版本在途：字段放在顶层（与旧格式相同）
                    (None, Some(mut j)) => {
                        j["path"] = json!(&p[5..]);
                        j["state"] = json!("committed");
                        j["committed"] = json!(true);
                        respond_json(conn, 200, "OK", j)
                    }
                    // 新版本在途：顶层是在途的状态与已接收长度，已提交的当前版本（如有）放在 current
                    (Some((st, len)), cur) => {
                        let mut j = json!({"path": &p[5..], "state": st.as_str(), "length": len, "committed": false});
                        if let Some(c) = cur {
                            j["current"] = c;
                        }
                        respond_json(conn, 200, "OK", j)
                    }
                }
            }
            Err(e) => service_error(conn, ctx, e),
        },
        ("GET", p) if p.starts_with("/tasks/") => {
            let Ok(id) = p[7..].parse::<u64>() else {
                return respond_json(conn, 400, "Bad Request", json!({"error": "bad_task_id"}));
            };
            let st = if wait {
                ctx.files.wait_task(id, ctx.wait_timeout)
            } else {
                ctx.files.task(id)
            };
            match st {
                // 查询本身成功；任务的结论在 JSON 里
                Some(st) => respond_json(conn, 200, "OK", task_json(id, &st).2),
                None => respond_json(
                    conn,
                    404,
                    "Not Found",
                    json!({"error": "unknown_task", "detail": "任务记录不跨节点、不跨进程保留"}),
                ),
            }
        }
        ("GET", p) if p.starts_with("/files/") => {
            match ctx.files.stat_full(&p[6..]) {
                Err(e) => return service_error(conn, ctx, e),
                Ok(PathState {
                    committed: None,
                    in_flight: Some((st, _)),
                }) => {
                    return respond_json(
                        conn,
                        409,
                        "Conflict",
                        json!({"error": "not_committed", "path": &p[6..], "state": st.as_str()}),
                    );
                }
                Ok(PathState {
                    committed: None,
                    in_flight: None,
                }) => {
                    return respond_json(
                        conn,
                        404,
                        "Not Found",
                        json!({"error": "not_found", "path": &p[6..]}),
                    );
                }
                Ok(PathState {
                    committed: Some(st),
                    ..
                }) if super::files::is_directory(&st.metadata) => {
                    return service_error(conn, ctx, ServiceError::IsDirectory(p[6..].into()));
                }
                Ok(_) => {}
            }
            let (tx, rx) = channel();
            let file = match ctx.files.read_spool() {
                Ok(file) => file,
                Err(e) => {
                    return respond_json(
                        conn,
                        500,
                        "Internal Server Error",
                        json!({"error": "spool_io", "detail": e.to_string()}),
                    );
                }
            };
            let sent =
                ctx.exec
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .send(ExecRequest::ReadTo {
                        path: p[6..].to_string(),
                        file,
                        reply: tx,
                    });
            if sent.is_err() {
                return respond_json(
                    conn,
                    503,
                    "Service Unavailable",
                    json!({"error": "executor_gone"}),
                );
            }
            match rx.recv_timeout(ctx.wait_timeout) {
                Ok(Ok(file)) => respond_file(conn, file, range),
                Ok(Err(tape_rs_read_busy @ super::executor::ReadError::Busy(_))) => respond_json(
                    conn,
                    503,
                    "Service Unavailable",
                    json!({"error": "drive_busy", "detail": tape_rs_read_busy.to_string()}),
                ),
                Ok(Err(e)) => respond_json(
                    conn,
                    500,
                    "Internal Server Error",
                    json!({"error": "read_failed", "detail": e.to_string()}),
                ),
                Err(_) => respond_json(
                    conn,
                    504,
                    "Gateway Timeout",
                    json!({"error": "read_timeout"}),
                ),
            }
        }
        (method @ ("POST" | "DELETE"), "/xattrs") => {
            let Some(path) = query_param(query, "path") else {
                return respond_json(conn, 400, "Bad Request", json!({"error":"missing_path"}));
            };
            let Some(name) = query_param(query, "name") else {
                return respond_json(
                    conn,
                    400,
                    "Bad Request",
                    json!({"error":"missing_attribute_name"}),
                );
            };
            let flags = match query_param(query, "flags").unwrap_or("0").parse::<u32>() {
                Ok(v) => v,
                Err(_) => {
                    return respond_json(
                        conn,
                        400,
                        "Bad Request",
                        json!({"error":"invalid_flags"}),
                    );
                }
            };
            let value = if method == "POST" {
                let Some(len) = content_length else {
                    return respond_json(
                        conn,
                        411,
                        "Length Required",
                        json!({"error":"length_required"}),
                    );
                };
                if len > 65536 {
                    return service_error(conn, ctx, ServiceError::AttributeTooLarge);
                }
                let mut value = vec![0; len as usize];
                reader.read_exact(&mut value)?;
                Some(value)
            } else {
                None
            };
            let task = match ctx.files.change_xattr(
                &percent_decode(path),
                &percent_decode(name),
                value.as_deref(),
                flags,
            ) {
                Ok(t) => t,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = ctx
                .files
                .wait_task(task, ctx.wait_timeout)
                .unwrap_or(TaskStatus::Staged);
            let (code, reason, body) = task_json(task, &st);
            respond_json(conn, code, reason, body)
        }
        ("POST", "/metadata") => {
            let Some(path) = query_param(query, "path") else {
                return service_error(conn, ctx, ServiceError::BadPath("缺少路径".into()));
            };
            let Some(len) = content_length.filter(|n| *n <= 1024) else {
                return service_error(conn, ctx, ServiceError::BadPath("属性请求长度非法".into()));
            };
            let mut body = vec![0; len as usize];
            reader.read_exact(&mut body)?;
            let parsed = (|| -> Option<crate::ltfs::index::MetadataChange> {
                let value: Value = serde_json::from_slice(&body).ok()?;
                let fields = value.as_object()?;
                if fields
                    .keys()
                    .any(|k| !matches!(k.as_str(), "readonly" | "access_time" | "modify_time"))
                {
                    return None;
                }
                let boolean = match value.get("readonly") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_bool()?),
                };
                let time = |name| -> Option<Option<String>> {
                    match value.get(name) {
                        None | Some(Value::Null) => Some(None),
                        Some(v) => Some(Some(v.as_str()?.to_string())),
                    }
                };
                Some(crate::ltfs::index::MetadataChange {
                    readonly: boolean,
                    access_time: time("access_time")?,
                    modify_time: time("modify_time")?,
                })
            })();
            let Some(change) = parsed else {
                return service_error(conn, ctx, ServiceError::BadPath("属性请求字段非法".into()));
            };
            let task = match ctx.files.change_metadata(&percent_decode(path), &change) {
                Ok(t) => t,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = ctx
                .files
                .wait_task(task, ctx.wait_timeout)
                .unwrap_or(TaskStatus::Staged);
            let (code, reason, body) = task_json(task, &st);
            respond_json(conn, code, reason, body)
        }
        ("POST", "/symlinks") => {
            let (Some(path), Some(target)) =
                (query_param(query, "path"), query_param(query, "target"))
            else {
                return respond_json(
                    conn,
                    400,
                    "Bad Request",
                    json!({"error":"missing_path_or_target"}),
                );
            };
            let task = match ctx
                .files
                .symlink(&percent_decode(target), &percent_decode(path))
            {
                Ok(task) => task,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = ctx
                .files
                .wait_task(task, ctx.wait_timeout)
                .unwrap_or(TaskStatus::Staged);
            let (code, reason, body) = task_json(task, &st);
            respond_json(conn, code, reason, body)
        }
        ("POST", "/rename") => {
            let Some(from) = query_param(query, "from") else {
                return respond_json(conn, 400, "Bad Request", json!({"error":"missing_source"}));
            };
            let Some(to) = query_param(query, "to") else {
                return respond_json(conn, 400, "Bad Request", json!({"error":"missing_target"}));
            };
            let task = match ctx.files.rename(
                &percent_decode(from),
                &percent_decode(to),
                query_param(query, "noreplace") == Some("1"),
            ) {
                Ok(task) => task,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = ctx
                .files
                .wait_task(task, ctx.wait_timeout)
                .unwrap_or(TaskStatus::Staged);
            let (code, reason, body) = task_json(task, &st);
            respond_json(conn, code, reason, body)
        }
        (method @ ("POST" | "DELETE"), p) if p.starts_with("/directories/") => {
            let task = match if method == "POST" {
                ctx.files.mkdir(&p[12..])
            } else {
                ctx.files.rmdir(&p[12..])
            } {
                Ok(task) => task,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = ctx
                .files
                .wait_task(task, ctx.wait_timeout)
                .unwrap_or(TaskStatus::Staged);
            let (code, reason, body) = task_json(task, &st);
            respond_json(conn, code, reason, body)
        }
        ("DELETE", p) if p.starts_with("/files/") => {
            let task = match ctx.files.delete(&p[6..]) {
                Ok(task) => task,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = if wait {
                ctx.files.wait_task(task, ctx.wait_timeout)
            } else {
                ctx.files.task(task)
            };
            let (code, reason, body) = task_json(task, &st.unwrap_or(TaskStatus::Staged));
            let (code, reason) = if code == 201 {
                (200, "OK")
            } else {
                (code, reason)
            };
            respond_json(conn, code, reason, body)
        }
        ("PUT", p) if p.starts_with("/files/") => {
            let Some(len) = content_length else {
                return respond_json(
                    conn,
                    411,
                    "Length Required",
                    json!({"error": "length_required"}),
                );
            };
            let h = match ctx.files.begin_with(&p[6..], len, create_only) {
                Ok(h) => h,
                Err(e) => {
                    // 没有用 Expect 的客户端此刻正在发内容。不读掉就关连接，对方收到的是连接重置，
                    // 而不是这条错误。
                    // 内容太大就不读了：读完之前对方一直收不到错误，不如让它尽早失败。
                    if !expects_continue && len <= 256 << 20 {
                        let _ = std::io::copy(&mut reader.take(len), &mut std::io::sink());
                    }
                    return service_error(conn, ctx, e);
                }
            };
            let spooled = (|| -> Result<(), ServiceError> {
                let mut f =
                    std::fs::File::create(&h.spool).map_err(|e| ServiceError::Io(e.to_string()))?;
                let mut left = len;
                let mut buf = vec![0u8; 256 * 1024];
                while left > 0 {
                    let want = buf.len().min(left as usize);
                    let n = reader
                        .read(&mut buf[..want])
                        .map_err(|e| ServiceError::Io(e.to_string()))?;
                    if n == 0 {
                        return Err(ServiceError::Io("连接在内容传完之前关闭".into()));
                    }
                    f.write_all(&buf[..n])
                        .map_err(|e| ServiceError::Io(e.to_string()))?;
                    ctx.files.ingest(&h, n as u64)?;
                    left -= n as u64;
                }
                f.sync_all().map_err(|e| ServiceError::Io(e.to_string()))
            })();
            if let Err(e) = spooled {
                ctx.files.abort(h);
                return service_error(conn, ctx, e);
            }
            let task = match ctx.files.finish(h, len) {
                Ok(t) => t,
                Err(e) => return service_error(conn, ctx, e),
            };
            let st = if wait {
                ctx.files.wait_task(task, ctx.wait_timeout)
            } else {
                ctx.files.task(task)
            };
            let (code, reason, body) = task_json(task, &st.unwrap_or(TaskStatus::Staged));
            respond_json(conn, code, reason, body)
        }
        _ => respond_json(conn, 404, "Not Found", json!({"error": "no_such_route"})),
    }
}

#[cfg(test)]
mod tests {
    use super::super::node::NodeStatus;
    use super::super::permission::{Permission, Phase};
    use super::super::state::Executor;
    use super::*;
    use std::time::Instant;

    fn test_context() -> Arc<HttpContext> {
        let dir = std::env::temp_dir().join(format!("http-axum-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir).unwrap();
        let (exec, _) = channel();
        let (node, _) = channel();
        Arc::new(HttpContext {
            files,
            status: Arc::new(Mutex::new(NodeStatus::default())),
            exec: Mutex::new(exec),
            node: Mutex::new(node),
            client_addrs: HashMap::new(),
            wait_timeout: Duration::from_secs(1),
        })
    }

    // 使用生产连接配置，且每次只接一条连接，测试结束后回收运行时。
    fn exchange(app: Router, request: &[u8]) -> String {
        use std::net::TcpStream;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        thread::scope(|scope| {
            scope.spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (conn, _) = listener.accept().await.unwrap();
                    tokio::time::timeout(Duration::from_secs(3), serve_connection(conn, app))
                        .await
                        .unwrap();
                });
            });
            let mut conn = TcpStream::connect(address).unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            conn.write_all(request).unwrap();
            let mut response = String::new();
            conn.read_to_string(&mut response).unwrap();
            response
        })
    }

    #[test]
    fn protocol_limits_reject_conflicting_lengths_and_oversized_headers() {
        let ctx = test_context();
        let response = exchange(
            router(ctx.clone()),
            b"PUT /files/a HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        let request = format!(
            "GET /cluster HTTP/1.1\r\nHost: x\r\nX-Large: {}\r\n\r\n",
            "a".repeat(40 * 1024)
        );
        let response = exchange(router(ctx), request.as_bytes());
        assert!(response.starts_with("HTTP/1.1 431"), "{response}");
    }

    #[test]
    fn rejected_expect_upload_does_not_request_body() {
        let response = exchange(router(test_context()), b"PUT /files/a HTTP/1.1\r\nHost: x\r\nContent-Length: 8388608\r\nExpect: 100-continue\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(!response.contains("100 Continue"), "准入之前不得请求内容");
    }

    #[test]
    fn request_limit_keeps_readiness_available_and_holds_until_body_dropped() {
        let requests = Arc::new(Semaphore::new(1));
        let state = HttpState {
            ctx: test_context(),
            requests: requests.clone(),
        };
        let app = Router::new().fallback(dispatch).with_state(state.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let held_response = runtime.block_on(dispatch(
            State(state),
            Request::builder()
                .uri("/cluster")
                .body(Body::empty())
                .unwrap(),
        ));
        assert_eq!(held_response.status(), StatusCode::OK);
        let response = exchange(app.clone(), b"GET /cluster HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            response.starts_with("HTTP/1.1 503") && response.contains("http_busy"),
            "{response}"
        );
        let response = exchange(app.clone(), b"GET /readyz HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            response.contains("\"ready\":false") && !response.contains("http_busy"),
            "{response}"
        );
        assert_eq!(requests.available_permits(), 0);
        drop(held_response);
        assert_eq!(requests.available_permits(), 1);
        let response = exchange(app, b"GET /cluster HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    #[test]
    fn cancelled_request_keeps_accepted_command_and_capacity_until_result() {
        let ctx = test_context();
        let (node, input) = channel();
        *ctx.node.lock().unwrap() = node;
        let requests = Arc::new(Semaphore::new(1));
        let state = HttpState {
            ctx,
            requests: requests.clone(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let request = Request::builder()
                .method("POST")
                .uri("/admin/pools/test")
                .body(Body::empty())
                .unwrap();
            let task = tokio::spawn(dispatch(State(state), request));
            let command = tokio::task::spawn_blocking(move || {
                input.recv_timeout(Duration::from_secs(2)).unwrap()
            })
            .await
            .unwrap();
            let NodeInput::Admin { reply, .. } = command else {
                panic!("应收到管理命令");
            };
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(
                requests.available_permits(),
                0,
                "HTTP 等待取消不能释放在途操作额度"
            );
            reply
                .send(AdminReply::Ok("committed".into()))
                .expect("已接收的命令仍有接收者");
            tokio::time::timeout(Duration::from_secs(2), async {
                while requests.available_permits() == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn truncated_download_is_an_error_instead_of_clean_eof() {
        let path = std::env::temp_dir().join(format!("http-spool-{}", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.write_all(b"0123456789").unwrap();
        let truncator = file.try_clone().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut response = Response::new(Body::empty());
            respond_file(&mut response, file, None).unwrap();
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
            truncator.set_len(5).unwrap();
            let error = axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("下载暂存内容不完整"), "{error}");
        });
    }

    fn readiness(ctx: &HttpContext) -> (u16, Value) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let response = ready_response(ctx);
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap())
        })
    }

    #[test]
    fn readiness_requires_a_recovered_local_owner_and_live_serving_permission() {
        let dir = std::env::temp_dir().join(format!("http-ready-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir.clone()).unwrap();
        let (exec, _exec_rx) = channel();
        let (node, _node_rx) = channel();
        let ctx = HttpContext {
            files: files.clone(),
            status: Arc::new(Mutex::new(NodeStatus::default())),
            exec: Mutex::new(exec),
            node: Mutex::new(node),
            client_addrs: HashMap::new(),
            wait_timeout: Duration::from_secs(1),
        };
        assert_eq!(readiness(&ctx).0, 503, "启动阶段不可就绪");
        {
            let mut s = ctx.status.lock().unwrap();
            s.id = 1;
            s.leader = 3;
            s.role = "Follower".into();
            s.executor = Some(Executor {
                node: 1,
                term: 1,
                round: 4,
                fenced: true,
            });
            s.executor_ready = true;
        }
        assert_eq!(readiness(&ctx).0, 503, "未绑定许可的旧协议不可持有 VIP");
        let permission = Arc::new(Permission::default());
        files.set_permission(permission.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        for phase in [Phase::Fencing, Phase::Execution] {
            assert!(permission.renew(4, phase, deadline));
            assert_eq!(readiness(&ctx).0, 503, "隔离/恢复阶段不可就绪");
        }
        assert!(permission.renew(4, Phase::Serving, deadline));
        assert!(files.serving_round().is_none(), "没有可写磁带的初始库");
        let (code, body) = readiness(&ctx);
        assert_eq!(code, 200, "follower owner 即使没有可写磁带也应就绪");
        assert_eq!(body["ready"], true);
        assert_eq!(body["owner"], 1);
        assert_eq!(body["round"], 4);
        {
            ctx.status.lock().unwrap().executor_ready = false;
        }
        assert_eq!(readiness(&ctx).0, 503);
        ctx.status.lock().unwrap().executor_ready = true;
        ctx.status.lock().unwrap().executor.as_mut().unwrap().fenced = false;
        assert_eq!(readiness(&ctx).0, 503);
        ctx.status.lock().unwrap().executor.as_mut().unwrap().fenced = true;
        ctx.status.lock().unwrap().shutting_down = true;
        assert_eq!(readiness(&ctx).0, 503, "停机收尾仍有许可，但不应持有 VIP");
        ctx.status.lock().unwrap().shutting_down = false;
        ctx.status.lock().unwrap().id = 3;
        ctx.status.lock().unwrap().role = "Leader".into();
        assert_eq!(readiness(&ctx).0, 503, "控制 Leader 不等于设备 owner");
        ctx.status.lock().unwrap().id = 1;
        ctx.status.lock().unwrap().executor.as_mut().unwrap().round = 5;
        assert_eq!(readiness(&ctx).0, 503, "旧轮次许可不能授权新 owner");
        ctx.status.lock().unwrap().executor.as_mut().unwrap().round = 4;
        permission.close(4);
        assert_eq!(readiness(&ctx).0, 503, "关闭许可立即反映到 HTTP");
        assert!(permission.renew(
            5,
            Phase::Serving,
            Instant::now() + Duration::from_millis(30)
        ));
        ctx.status.lock().unwrap().executor.as_mut().unwrap().round = 5;
        assert_eq!(readiness(&ctx).0, 200);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(readiness(&ctx).0, 503, "状态快照未刷新也不能延长许可");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn range_forms() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 10))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 100))));
        assert_eq!(
            parse_range("bytes=95-200", 100),
            Some(Ok((95, 100))),
            "终点越过末尾就截到末尾"
        );
        assert_eq!(parse_range("bytes=-5", 100), Some(Ok((95, 100))));
        assert_eq!(parse_range("bytes=-500", 100), Some(Ok((0, 100))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())), "起点在末尾");
        assert_eq!(parse_range("bytes=5-2", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=-5", 0), Some(Err(())));
        assert_eq!(parse_range("bytes=0-", 0), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None, "多段当没有 Range");
        assert_eq!(parse_range("items=0-1", 100), None);
    }
}
