//! Retries are restricted to reads, stable notification identities and saved
//! pool request identities. Never use this transport for arbitrary control writes.
use super::*;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use uuid::Uuid;
mod notifications;
pub mod pools;
mod queries;

fn invalid(reason: &str) -> ClientError {
    ClientError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        reason,
    ))
}

pub struct FdbClient {
    transport: Client,
}

// Each attempt owns and closes its connection, including timeout/error paths.
struct Connection(tokio::task::JoinHandle<()>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn connection_error(error: impl std::fmt::Display) -> ClientError {
    ClientError::NoLeader(format!("FDB 管理连接暂不可用：{error}"))
}
fn retry_delay(attempt: u32) -> Duration {
    Duration::from_millis((100_u64 << attempt.min(5)) + u64::from(Uuid::new_v4().as_bytes()[0]))
}

impl FdbClient {
    pub fn new<S: Into<String>>(endpoints: impl IntoIterator<Item = S>) -> Result<Self> {
        let transport = Client::new(endpoints);
        if transport.endpoints.is_empty()
            || transport.endpoints.iter().any(|address| {
                address
                    .parse::<hyper::http::uri::Authority>()
                    .ok()
                    .is_none_or(|a| a.port_u16().is_none())
                    || !address.contains(':')
                    || address.contains('@')
                    || address.chars().any(char::is_whitespace)
            })
        {
            return Err(invalid("FDB 节点地址须为不带路径的 host:port"));
        }
        Ok(Self { transport })
    }
    pub fn with_timeouts(mut self, retry: Duration, io: Duration) -> Self {
        self.transport.retry_for = retry;
        self.transport.io_timeout = io;
        self
    }
    fn retry_request(
        &self,
        method: &str,
        route: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Response> {
        if body.is_some_and(|v| v.len() > 16 * 1024) {
            return Err(invalid("FDB 管理请求体超过16KiB上限"));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let deadline = Instant::now() + self.transport.retry_for;
        let mut attempt = 0;
        loop {
            for endpoint in &self.transport.endpoints {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(connection_error("重试预算已耗尽"));
                }
                let result = rt.block_on(async {
                    tokio::time::timeout(
                        remaining.min(self.transport.io_timeout),
                        rpc_once(endpoint, method, route, body, headers),
                    )
                    .await
                });
                match result {
                    Ok(Ok(response)) if !matches!(response.status, 429 | 503 | 504) => {
                        return Ok(response);
                    }
                    Ok(Err(error)) if !matches!(error, ClientError::NoLeader(_)) => {
                        return Err(error);
                    }
                    _ => {}
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(connection_error("重试预算已耗尽"));
            }
            std::thread::sleep(retry_delay(attempt).min(remaining));
            attempt = attempt.saturating_add(1);
        }
    }
}
async fn rpc_once(
    endpoint: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
) -> Result<Response> {
    let socket = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(endpoint),
    )
    .await
    .map_err(connection_error)?
    .map_err(connection_error)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .map_err(connection_error)?;
    let _connection = Connection(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let mut request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", endpoint);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(http_body_util::Full::new(bytes::Bytes::copy_from_slice(
            body.unwrap_or_default(),
        )))
        .map_err(|_| invalid("FDB 管理请求无效"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(connection_error)?;
    let status = response.status().as_u16();
    if matches!(status, 429 | 503 | 504) {
        return Ok(Response {
            status,
            body: Vec::new(),
        });
    }
    const LIMIT: usize = 2 * 1024 * 1024;
    if response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|v| v > LIMIT as u64)
    {
        return Err(ClientError::Verification("FDB 管理响应超过2MiB上限".into()));
    }
    let mut incoming = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = incoming.frame().await {
        if let Ok(data) = frame.map_err(connection_error)?.into_data() {
            if data.len() > LIMIT - bytes.len() {
                return Err(ClientError::Verification("FDB 管理响应超过2MiB上限".into()));
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(Response {
        status,
        body: bytes,
    })
}
fn rejected(response: Response) -> ClientError {
    ClientError::Rejected {
        status: response.status,
        body: String::from_utf8_lossy(&response.body).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    pub(super) fn server(
        response: Vec<u8>,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut raw = Vec::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                raw.extend_from_slice(line.as_bytes());
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let at = raw.len();
            raw.resize(at + length, 0);
            reader.read_exact(&mut raw[at..]).unwrap();
            std::thread::sleep(delay);
            let _ = stream.write_all(&response);
            raw
        });
        (endpoint, task)
    }
    pub(super) fn response(status: u16, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
    }
    #[test]
    fn retry_preserves_injection_identity_and_encoded_library_across_endpoints() {
        let (a, ta) = server(response(503, "{}"), Duration::ZERO);
        let (b, tb) = server(response(200, "{\"ok\":true}"), Duration::ZERO);
        let operation = Uuid::new_v4();
        let result = FdbClient::new([a, b])
            .unwrap()
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(1))
            .inject_notification(
                "LIB&test=1",
                operation,
                "tape.state.changed",
                serde_json::json!({"state":"test"}),
            )
            .unwrap();
        assert_eq!(result["ok"], true);
        let first = ta.join().unwrap();
        let second = tb.join().unwrap();
        let first = String::from_utf8(first).unwrap();
        let second = String::from_utf8(second).unwrap();
        assert!(
            first.starts_with("POST /admin/notify/events/inject?library=LIB%26test%3D1 HTTP/1.1")
        );
        assert_eq!(
            first.split("\r\n\r\n").nth(1),
            second.split("\r\n\r\n").nth(1)
        );
        assert!(first.contains(&operation.to_string()));
    }
    #[test]
    fn watch_callback_can_call_ack_without_nested_runtime_panic() {
        let generation = Uuid::new_v4();
        let delivery = Uuid::new_v4();
        let frame = format!(
            "event: notify\ndata: {{\"generation\":\"{generation}\",\"delivery_id\":\"{delivery}\",\"through\":1,\"events\":[]}}\n\n"
        );
        let raw=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{frame}",frame.len()).into_bytes();
        let (endpoint, server) = server(raw, Duration::ZERO);
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable = dead.local_addr().unwrap().to_string();
        drop(dead);
        let ack_client = FdbClient::new([unavailable])
            .unwrap()
            .with_timeouts(Duration::from_millis(50), Duration::from_secs(1));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            FdbClient::new([endpoint]).unwrap().watch_notifications(
                "TEST",
                "consumer",
                generation,
                None,
                |_| {
                    assert!(matches!(
                        ack_client.acknowledge_notify("TEST", "consumer", generation, delivery, 1),
                        Err(ClientError::NoLeader(_))
                    ));
                    Ok(false)
                },
            )
        }));
        server.join().unwrap();
        assert!(
            matches!(result, Ok(Ok(()))),
            "回调内ACK不得启动嵌套runtime或panic"
        );
    }
    #[test]
    fn rpc_deadline_bounds_slow_dependency_and_does_not_wait_full_io_timeout() {
        let (a, task) = server(response(200, "{}"), Duration::from_millis(400));
        let start = Instant::now();
        let result = FdbClient::new([a])
            .unwrap()
            .with_timeouts(Duration::from_millis(100), Duration::from_secs(30))
            .notify_consumer("TEST", "consumer");
        assert!(matches!(result, Err(ClientError::NoLeader(_))));
        assert!(start.elapsed() < Duration::from_secs(2));
        task.join().unwrap();
    }
    #[test]
    fn rpc_rejects_invalid_json_and_oversized_content_length_without_retry() {
        for raw in [
            response(200, "bad"),
            response(200, "null"),
            b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n".to_vec(),
        ] {
            let (a, task) = server(raw, Duration::ZERO);
            let result = FdbClient::new([a])
                .unwrap()
                .notify_consumer("TEST", "consumer");
            assert!(matches!(result, Err(ClientError::Verification(_))));
            task.join().unwrap();
        }
    }
    #[test]
    fn rpc_rejects_oversized_chunked_body_and_permanent_status() {
        let chunk = "x".repeat(2 * 1024 * 1024 + 1);
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
            chunk.len()
        )
        .into_bytes();
        let (a, task) = server(chunked, Duration::ZERO);
        assert!(matches!(
            FdbClient::new([a])
                .unwrap()
                .notify_consumer("TEST", "consumer"),
            Err(ClientError::Verification(_))
        ));
        task.join().unwrap();
        let (a, task) = server(response(500, "{\"error\":\"日志损坏\"}"), Duration::ZERO);
        assert!(matches!(
            FdbClient::new([a])
                .unwrap()
                .notify_consumer("TEST", "consumer"),
            Err(ClientError::Rejected { status: 500, .. })
        ));
        task.join().unwrap();
    }
}
