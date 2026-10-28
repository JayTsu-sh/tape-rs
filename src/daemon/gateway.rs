//! 固定 HTTP 入口按库转发到 owner（数据）或 Raft Leader（管理写入）。
//! 每次请求只选择一次目标；发送后无论何种错误都不重发请求体。
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderName, header},
    response::Response,
};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::Semaphore;

use super::http::{self, HttpContext};
use super::multilib::LibrariesConfig;

const HOP: &str = "x-tapers-forwarded";

#[derive(Clone)]
struct Gateway {
    config: LibrariesConfig,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
    requests: Arc<Semaphore>,
}

pub fn serve(
    listen: &str,
    config: LibrariesConfig,
    libraries: BTreeMap<String, Arc<HttpContext>>,
    gateway: Arc<HttpContext>,
    peers: HashMap<u64, String>,
) -> std::io::Result<()> {
    let app = Router::new().fallback(dispatch).with_state(Gateway {
        config,
        libraries,
        gateway,
        peers,
        requests: Arc::new(Semaphore::new(32)),
    });
    http::serve_router(listen, app)
}

fn response(code: u16, value: serde_json::Value) -> Response {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

/// 返回 (库标识, 库内路径)；默认库入口兼容旧单库 URL。
fn route<'a>(path: &'a str, default: &'a str) -> Option<(&'a str, String)> {
    if let Some(rest) = path.strip_prefix("/libraries/") {
        let (library, rest) = rest.split_once('/').unwrap_or((rest, ""));
        if library.is_empty() {
            return None;
        }
        Some((library, format!("/{rest}")))
    } else if let Some(rest) = path.strip_prefix("/admin/libraries/") {
        let (library, rest) = rest.split_once('/').unwrap_or((rest, ""));
        if library.is_empty() {
            return None;
        }
        Some((
            library,
            if rest == "rescan" {
                "/admin/libraries/current/rescan".into()
            } else if rest.is_empty() {
                "/admin/libraries/current".into()
            } else {
                format!("/admin/{rest}")
            },
        ))
    } else {
        Some((default, path.to_owned()))
    }
}

/// 文件任务终态已写入每个节点的复制目录；无设备 owner 时仍可从入口本地查询。
fn local_file_task_query(method: &axum::http::Method, path: &str) -> bool {
    if method != axum::http::Method::GET {
        return false;
    }
    let task = path.strip_prefix("/tasks/").or_else(|| {
        path.strip_prefix("/pools/")
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(pool, rest)| (!pool.is_empty()).then_some(rest))
            .and_then(|rest| rest.strip_prefix("tasks/"))
    });
    task.is_some_and(|id| id.parse::<u64>().is_ok())
}

async fn dispatch(State(g): State<Gateway>, mut req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if req.method() == "GET" && path == "/readyz" {
        return http::dispatch_context(g.gateway, g.requests, req).await;
    }
    if req.method() == "GET" && (path == "/admin/libraries" || path == "/cluster") {
        let status = g
            .gateway
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let libraries: Vec<_> = g.config.libraries.iter().map(|l| {
            let s = g.libraries[&l.changer_serial].status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            json!({"id":l.changer_serial,"changer_serial":l.changer_serial,"drive_serials":l.drive_serials,"executor":s.executor.as_ref().map(|e|json!({"node":e.node,"round":e.round,"fenced":e.fenced})),"executor_ready":s.executor_ready,"applied_index":s.applied_index})
        }).collect();
        return response(
            200,
            json!({"id":status.id,"role":status.role,"leader":status.leader,"term":status.term,"applied_index":status.applied_index,"default_library":g.config.default_library,"gateway":{"executor":status.executor.as_ref().map(|e|json!({"node":e.node,"round":e.round,"fenced":e.fenced})),"executor_ready":status.executor_ready},"vips":status.vips.iter().map(|(a,r)|json!({"address":a.to_string(),"token":r.token,"holder":r.holder.as_ref().map(|h|json!({"node":h.node,"round":h.round}))})).collect::<Vec<_>>(),"libraries":libraries}),
        );
    }
    let Some((id, local_path)) = route(&path, &g.config.default_library) else {
        return response(404, json!({"error":"library_not_found"}));
    };
    let selector = http::percent_decode(id);
    let Some(library) = g.config.resolve_library(&selector) else {
        return response(404, json!({"error":"library_not_found","library_id":id}));
    };
    let id = &library.changer_serial;
    let ctx = g.libraries[&library.changer_serial].clone();
    let status = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let admin = local_path.starts_with("/admin/");
    let target = if local_file_task_query(req.method(), &local_path) {
        status.id
    } else if admin {
        // POST export 只读取本节点已冻结的目录；转发会把三节点证据变成
        // 同一个 Leader 的三份导出，不能用于切换证书。
        if req.method() == "GET"
            || req.method() == "POST" && local_path == "/admin/catalog/current/export"
        {
            status.id
        } else {
            status.leader
        }
    } else {
        status
            .executor
            .as_ref()
            .filter(|_| status.executor_ready)
            .map_or(0, |e| e.node)
    };
    if target == 0 {
        return response(
            503,
            json!({"error":"not_serving","detail":"目标带库正在恢复或没有可用 owner","library_id":id}),
        );
    }
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    if target == status.id {
        *req.uri_mut() = format!("{local_path}{query}").parse().unwrap();
        return http::dispatch_context(ctx, g.requests, req).await;
    }
    if req.headers().contains_key(HOP) {
        return response(
            503,
            json!({"error":"not_serving","detail":"目标库 owner 或 Leader 已变化，请稍后查询","library_id":id}),
        );
    }
    let Some(address) = g.peers.get(&target) else {
        return response(
            503,
            json!({"error":"not_serving","detail":"目标节点尚未配置 HTTP 地址"}),
        );
    };
    let Ok(permit) = g.requests.clone().try_acquire_owned() else {
        return response(503, json!({"error":"http_busy"}));
    };
    // 保持库作用域直到远端入口。库内路径中可能包含任意文件名，保留原始百分号编码。
    let uri = format!(
        "/libraries/{}{local_path}{query}",
        crate::client::encode_query(id)
    );
    *req.uri_mut() = uri.parse().unwrap();
    forward_request(req, address, permit, id).await
}

/// 已选择目标的单次转发。调用方负责库作用域、环路检查与并发准入。
pub(super) async fn forward_request(
    mut req: Request,
    address: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
    id: &str,
) -> Response {
    let read_only = req.method() == axum::http::Method::GET;
    strip_hop_headers(req.headers_mut());
    // 入口的 HTTP/1.1 连接已处理 100 Continue；owner 必须把它当作普通请求体。
    // 否则入口先发 100、owner 随后拒绝准入时，客户端会在发送正文中遇到断管。
    req.headers_mut().remove(header::EXPECT);
    req.headers_mut()
        .insert(HeaderName::from_static(HOP), "1".parse().unwrap());
    let connection = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(address),
    )
    .await;
    let stream = match connection {
        Ok(Ok(s)) => s,
        _ => {
            return response(
                503,
                json!({"error":"not_serving","detail":"无法连接目标带库节点，请稍后重试","library_id":id}),
            );
        }
    };
    let handshake = tokio::time::timeout(
        Duration::from_secs(5),
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await;
    let (mut sender, connection) = match handshake {
        Ok(Ok(c)) => c,
        _ => {
            return response(
                503,
                json!({"error":"not_serving","detail":"目标节点连接未就绪"}),
            );
        }
    };
    tokio::spawn(async move {
        let _ = connection.await;
    });
    // 一旦调用 send_request，写入可能已到达 owner。失败必须返回结果未定。
    match sender.send_request(req).await {
        Ok(mut upstream) => {
            strip_hop_headers(upstream.headers_mut());
            let (parts, body) = upstream.into_parts();
            Response::from_parts(
                parts,
                Body::new(ProxyBody {
                    body,
                    _permit: permit,
                }),
            )
        }
        Err(_) if read_only => response(
            503,
            json!({"error":"read_unavailable","detail":"读取转发中断，请稍后重试","library_id":id}),
        ),
        Err(_) => response(
            504,
            json!({"error":"indeterminate","status":"indeterminate","detail":"转发连接中断，操作结果未定；请查询后决定是否重发","library_id":id}),
        ),
    }
}

fn strip_hop_headers(headers: &mut axum::http::HeaderMap) {
    if let Some(connection) = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    {
        for field in connection.split(',').map(str::trim) {
            if let Ok(name) = HeaderName::from_bytes(field.as_bytes()) {
                headers.remove(name);
            }
        }
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

struct ProxyBody {
    body: hyper::body::Incoming,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl hyper::body::Body for ProxyBody {
    type Data = axum::body::Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_catalog_export_is_local_even_when_another_node_is_leader() {
        use super::super::{
            directory::Directory,
            files::{BatchPolicy, FileService},
            node::NodeStatus,
            state::{Command, ControlState},
        };
        use http_body_util::BodyExt;
        use std::sync::{Mutex, mpsc};
        let root = std::env::temp_dir().join(format!("gateway-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("directory.db");
        let mut directory = Directory::open(&path).unwrap();
        let mut control = ControlState::default();
        for (index, command) in [
            (1, Command::CatalogQuiesce),
            (2, Command::CatalogFreeze { quiesce_index: 1 }),
        ] {
            let outcome = control.apply(index, &command);
            directory.apply(index, &command, &outcome).unwrap();
        }
        let files =
            FileService::with_options(root.join("spool"), BatchPolicy::default(), Some(path))
                .unwrap();
        let (exec, _) = mpsc::channel();
        let (node, received) = mpsc::channel();
        let ctx = Arc::new(HttpContext {
            files,
            status: Arc::new(Mutex::new(NodeStatus {
                id: 2,
                leader: 1,
                applied_index: 2,
                catalog_quiesce_index: Some(1),
                catalog_frozen_index: Some(2),
                ..Default::default()
            })),
            exec: Mutex::new(exec),
            node: Mutex::new(node),
            client_addrs: HashMap::new(),
            wait_timeout: Duration::from_secs(1),
        });
        let gateway = Gateway {
            config: LibrariesConfig::parse(&json!({"default_library":"L","libraries":[{"changer_serial":"L","drive_serials":["D"]}]})).unwrap(),
            libraries: BTreeMap::from([("L".into(), ctx.clone())]), gateway: ctx.clone(), peers: HashMap::new(), requests: Arc::new(Semaphore::new(1)),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let request = Request::builder()
                .method("POST")
                .uri("/libraries/L/admin/catalog/current/export")
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap();
            let response = dispatch(State(gateway), request).await;
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["x-tapers-source-node"], "2");
            assert_eq!(response.headers()["x-tapers-freeze-index"], "2");
            assert!(
                !response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .is_empty()
            );
        });
        assert!(
            received.try_recv().is_err(),
            "只读导出不得作为管理写命令提交"
        );
        drop(ctx);
        drop(directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn routes_keep_library_identity_and_query_outside_path() {
        assert_eq!(
            route("/libraries/b/files/a%2Fb", "a"),
            Some(("b", "/files/a%2Fb".into()))
        );
        assert_eq!(
            route("/admin/libraries/b/tapes/T1", "a"),
            Some(("b", "/admin/tapes/T1".into()))
        );
        assert_eq!(
            route("/admin/libraries/b", "a"),
            Some(("b", "/admin/libraries/current".into()))
        );
        assert_eq!(route("/files/a", "a"), Some(("a", "/files/a".into())));
        assert_eq!(route("/libraries//files/x", "a"), None);
        assert!(local_file_task_query(
            &axum::http::Method::GET,
            "/pools/p/tasks/123"
        ));
        assert!(local_file_task_query(
            &axum::http::Method::GET,
            "/tasks/123"
        ));
        assert!(!local_file_task_query(
            &axum::http::Method::POST,
            "/pools/p/tasks/123"
        ));
        assert!(!local_file_task_query(
            &axum::http::Method::GET,
            "/pools/p/tasks/invalid"
        ));
    }
    #[test]
    fn a_broken_forwarded_write_is_indeterminate_and_is_never_sent_twice() {
        use super::super::{files::FileService, node::NodeStatus, state::Executor};
        use std::io::Read;
        use std::sync::{Mutex, mpsc};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (observed, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let mut body = [0u8; 4];
            stream.read_exact(&mut body).unwrap();
            observed
                .send((String::from_utf8(bytes).unwrap(), body))
                .unwrap();
            // 写入已经被接收；没有发送任何结论即断开。
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            assert!(listener.accept().is_err());
        });
        let dir = std::env::temp_dir().join(format!("gateway-lost-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir.clone()).unwrap();
        let (exec, _) = std::sync::mpsc::channel();
        let (node, _) = std::sync::mpsc::channel();
        let ctx = Arc::new(HttpContext {
            files,
            status: Arc::new(Mutex::new(NodeStatus {
                id: 1,
                leader: 1,
                executor: Some(Executor {
                    node: 2,
                    round: 1,
                    term: 1,
                    fenced: true,
                }),
                executor_ready: true,
                ..Default::default()
            })),
            exec: Mutex::new(exec),
            node: Mutex::new(node),
            client_addrs: HashMap::new(),
            wait_timeout: Duration::from_secs(1),
        });
        let config=LibrariesConfig::parse(&json!({"default_library":"a","libraries":[{"changer_serial":"a","drive_serials":["d"]}]})).unwrap();
        let gateway = Gateway {
            config,
            libraries: BTreeMap::from([("a".into(), ctx.clone())]),
            gateway: ctx,
            peers: HashMap::from([(2, addr)]),
            requests: Arc::new(Semaphore::new(32)),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let request = Request::builder()
                .method("PUT")
                .uri("/libraries/a/files/once?wait=1")
                .header("host", "vip")
                .header("content-length", "4")
                .header("expect", "100-continue")
                .body(Body::from("once"))
                .unwrap();
            let response = dispatch(State(gateway), request).await;
            assert_eq!(response.status(), 504);
            use http_body_util::BodyExt;
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["error"],
                "indeterminate"
            );
        });
        let (headers, body) = rx.recv().unwrap();
        assert!(headers.starts_with("PUT /libraries/a/files/once?wait=1 HTTP/1.1"));
        assert!(
            !headers.to_ascii_lowercase().contains("expect:"),
            "网关已接受请求体后，转发给 owner 时不能再次使用延迟准入"
        );
        assert_eq!(&body, b"once");
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
