//! Durable external notification subscriptions. Printing never implies ACK.
use super::*;

fn route(library: &str, id: &str, suffix: &str) -> Result<String> {
    Client::new(Vec::<String>::new()).with_library(library)?;
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(invalid("通知消费者标识无效"));
    }
    Ok(format!(
        "/admin/notify/consumers/{id}{suffix}?library={}",
        encode_query(library)
    ))
}
fn result(response: Response) -> Result<Value> {
    if response.status == 200 {
        serde_json::from_slice::<Value>(&response.body)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| ClientError::Verification("通知响应不是JSON对象".into()))
    } else {
        Err(rejected(response))
    }
}

impl FdbClient {
    /// Stable operation UUID permits safe retry of a test-only notification.
    pub fn inject_notification(
        &self,
        library: &str,
        operation: Uuid,
        kind: &str,
        data: Value,
    ) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        if operation.is_nil()
            || kind.len() > 64
            || !data.is_object()
            || data.to_string().len() > 8192
        {
            return Err(invalid("测试事件UUID、类型或内容无效"));
        }
        let body =
            serde_json::json!({"operation":operation.to_string(),"event_type":kind,"data":data})
                .to_string();
        result(self.retry_request(
            "POST",
            &format!(
                "/admin/notify/events/inject?library={}",
                encode_query(library)
            ),
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?)
    }
    pub fn create_notify_consumer(
        &self,
        library: &str,
        id: &str,
        filter: Value,
        from_start: bool,
    ) -> Result<Value> {
        let body = serde_json::json!({"filter":filter,"from_start":from_start}).to_string();
        result(self.retry_request(
            "PUT",
            &route(library, id, "")?,
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?)
    }
    pub fn notify_consumer(&self, library: &str, id: &str) -> Result<Value> {
        result(self.retry_request("GET", &route(library, id, "")?, None, &[])?)
    }
    pub fn notify_consumers(&self, library: &str, after: Option<&str>) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        if let Some(id) = after {
            route(library, id, "")?;
        }
        result(self.retry_request(
            "GET",
            &format!(
                "/admin/notify/consumers?library={}{}",
                encode_query(library),
                after.map(|s| format!("&after={s}")).unwrap_or_default()
            ),
            None,
            &[],
        )?)
    }
    pub fn notify_batch(&self, library: &str, id: &str, generation: Uuid) -> Result<Value> {
        result(self.retry_request(
            "GET",
            &format!("{}&generation={generation}", route(library, id, "/events")?),
            None,
            &[],
        )?)
    }
    pub fn acknowledge_notify(
        &self,
        library: &str,
        id: &str,
        generation: Uuid,
        delivery: Uuid,
        through: u64,
    ) -> Result<Value> {
        let body = serde_json::json!({"generation":generation.to_string(),"delivery_id":delivery.to_string(),"through":through}).to_string();
        result(self.retry_request(
            "POST",
            &route(library, id, "/ack")?,
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?)
    }
    pub fn delete_notify_consumer(
        &self,
        library: &str,
        id: &str,
        generation: Uuid,
    ) -> Result<Value> {
        let body =
            serde_json::json!({"generation":generation.to_string(),"abandon_unacknowledged":true})
                .to_string();
        result(self.retry_request(
            "DELETE",
            &route(library, id, "")?,
            Some(body.as_bytes()),
            &[("Content-Type", "application/json")],
        )?)
    }
    /// Reconnects across endpoints without advancing ACK. The callback must
    /// persist/process each batch and explicitly acknowledge it when appropriate.
    /// Returning false stops; a callback error is returned without ACK or retry.
    pub fn watch_notifications<F>(
        &self,
        library: &str,
        id: &str,
        generation: Uuid,
        event_types: Option<&[String]>,
        mut receive: F,
    ) -> Result<()>
    where
        F: FnMut(&Value) -> Result<bool>,
    {
        let mut path = format!("{}&generation={generation}", route(library, id, "/stream")?);
        if let Some(types) = event_types {
            if types.len() > 16
                || types.iter().any(|v| {
                    v.is_empty()
                        || v.len() > 64
                        || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
                })
            {
                return Err(invalid("通知事件类型无效"));
            }
            path.push_str("&event_types=");
            path.push_str(&types.join(","));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut attempt = 0;
        loop {
            for endpoint in &self.transport.endpoints {
                match watch_once(&rt, endpoint, &path, &mut receive) {
                    Ok(WatchOutcome::Stop) => return Ok(()),
                    Ok(WatchOutcome::CallbackError(error)) => return Err(error),
                    Ok(WatchOutcome::Reconnect) | Err(ClientError::NoLeader(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            std::thread::sleep(retry_delay(attempt));
            attempt = attempt.saturating_add(1);
        }
    }
}
enum WatchOutcome {
    Reconnect,
    Stop,
    CallbackError(ClientError),
}

async fn watch_connect(endpoint: &str, path: &str) -> Result<(Connection, hyper::body::Incoming)> {
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
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let connection_guard = Connection(task);
    let request = hyper::Request::builder()
        .uri(path)
        .header("host", endpoint)
        .header("accept", "text/event-stream")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(connection_error)?;
    let response = tokio::time::timeout(Duration::from_secs(10), sender.send_request(request))
        .await
        .map_err(connection_error)?
        .map_err(connection_error)?;
    if response.status().as_u16() != 200 {
        let status = response.status().as_u16();
        return if matches!(status, 503 | 504 | 429) {
            Err(connection_error(status))
        } else {
            Err(ClientError::Rejected {
                status,
                body: "通知订阅被拒绝，请检查消费者实例和带库".into(),
            })
        };
    }
    if !response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        return Err(ClientError::Verification("通知响应不是 SSE".into()));
    }
    Ok((connection_guard, response.into_body()))
}

fn watch_once<F>(
    rt: &tokio::runtime::Runtime,
    endpoint: &str,
    path: &str,
    receive: &mut F,
) -> Result<WatchOutcome>
where
    F: FnMut(&Value) -> Result<bool>,
{
    let (connection_guard, mut body) = rt.block_on(watch_connect(endpoint, path))?;
    let _connection = connection_guard;
    let mut parser = Parser::default();
    loop {
        let frame = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(30), body.frame()).await })
            .map_err(connection_error)?;
        let Some(frame) = frame else {
            return Ok(WatchOutcome::Reconnect);
        };
        if let Ok(data) = frame.map_err(connection_error)?.into_data() {
            let mut callback_error = None;
            let keep = parser.push(&data, &mut |value| match receive(value) {
                Ok(keep) => Ok(keep),
                Err(error) => {
                    callback_error = Some(error);
                    Ok(false)
                }
            })?;
            if let Some(error) = callback_error {
                return Ok(WatchOutcome::CallbackError(error));
            }
            if !keep {
                return Ok(WatchOutcome::Stop);
            }
        }
    }
}

#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    data: String,
    kind: String,
}
impl Parser {
    fn push<F>(&mut self, bytes: &[u8], receive: &mut F) -> Result<bool>
    where
        F: FnMut(&Value) -> Result<bool>,
    {
        for &byte in bytes {
            if byte != b'\n' {
                self.line.push(byte);
                if self.line.len() + self.data.len() > 256 * 1024 {
                    return Err(ClientError::Verification("通知帧超过上限".into()));
                }
                continue;
            }
            let line = std::str::from_utf8(&self.line)
                .map_err(|_| ClientError::Verification("通知帧不是 UTF-8".into()))?
                .trim_end_matches('\r');
            if line.is_empty() {
                if !self.data.is_empty() {
                    let value: Value = serde_json::from_str(&self.data)
                        .map_err(|_| ClientError::Verification("通知 JSON 无效".into()))?;
                    if self.kind == "notify-error" {
                        return if value["retryable"] == true {
                            Err(connection_error(value))
                        } else {
                            Err(ClientError::Verification(format!(
                                "通知订阅发生永久错误：{value}"
                            )))
                        };
                    }
                    if self.kind == "notify" && !receive(&value)? {
                        return Ok(false);
                    }
                }
                self.data.clear();
                self.kind.clear();
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                self.data.push('\n');
            } else if let Some(kind) = line.strip_prefix("event:") {
                self.kind = kind.trim().into();
            }
            self.line.clear();
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notify_sse_handles_utf8_fragmentation_heartbeats_and_multiple_frames() {
        let raw = "retry: 1000\n: heartbeat\n\nevent: notify\nid: x\ndata: {\"text\":\"磁带\"}\n\nevent: notify\ndata: {\"text\":\"完成\"}\n\n";
        let mut parser = Parser::default();
        let mut values = Vec::new();
        for byte in raw.as_bytes() {
            assert!(
                parser
                    .push(&[*byte], &mut |v| {
                        values.push(v.clone());
                        Ok(true)
                    })
                    .unwrap()
            );
        }
        assert_eq!(
            values,
            vec![
                serde_json::json!({"text":"磁带"}),
                serde_json::json!({"text":"完成"})
            ]
        );
    }
    #[test]
    fn notify_sse_classifies_dependency_errors_without_retrying_corrupt_records() {
        let transient =
            b"event: notify-error\ndata: {\"error\":\"FDB unavailable\",\"retryable\":true}\n\n";
        let permanent =
            b"event: notify-error\ndata: {\"error\":\"log corrupt\",\"retryable\":false}\n\n";
        assert!(matches!(
            Parser::default().push(transient, &mut |_| panic!("错误帧不能投递")),
            Err(ClientError::NoLeader(_))
        ));
        assert!(matches!(
            Parser::default().push(permanent, &mut |_| panic!("错误帧不能投递")),
            Err(ClientError::Verification(_))
        ));
        assert!(
            Parser::default()
                .push(b"data: \xff\n", &mut |_| Ok(true))
                .is_err()
        );
    }
    #[test]
    fn notify_sse_rejects_malformed_and_bounded_frames_without_ack() {
        assert!(
            Parser::default()
                .push(b"event: notify\ndata: bad\n\n", &mut |_| Ok(true))
                .is_err()
        );
        assert!(
            Parser::default()
                .push(&vec![b'x'; 256 * 1024 + 1], &mut |_| Ok(true))
                .is_err()
        );
        let mut calls = 0;
        assert!(
            !Parser::default()
                .push(b"event: notify\ndata: {}\n\n", &mut |_| {
                    calls += 1;
                    Ok(false)
                })
                .unwrap()
        );
        assert_eq!(calls, 1);
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn callback_no_leader_is_returned_without_reconnecting_or_acknowledging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let generation = Uuid::new_v4();
        let batch = serde_json::json!({"delivery_id":Uuid::new_v4().to_string(),"generation":generation.to_string(),"from":0,"through":1,"created_at_millis":1,"fingerprint":"0".repeat(64),"events":[]});
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted = 0;
            while accepted < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut reader = BufReader::new(socket.try_clone().unwrap());
                        loop {
                            let mut line = String::new();
                            assert!(reader.read_line(&mut line).unwrap() > 0);
                            if line == "\r\n" {
                                break;
                            }
                        }
                        let frame = format!("event: notify\ndata: {batch}\n\n");
                        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",frame.len(),frame).unwrap();
                        accepted += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("通知测试连接失败: {e}"),
                }
            }
            accepted
        });
        let client = FdbClient::new([endpoint]).unwrap();
        let mut callbacks = 0;
        let result = client.watch_notifications("TEST", "consumer", generation, None, |_| {
            callbacks += 1;
            if callbacks == 1 {
                Err(ClientError::NoLeader("下游处理失败".into()))
            } else {
                Ok(false)
            }
        });
        let accepted = worker.join().unwrap();
        assert!(matches!(result,Err(ClientError::NoLeader(ref text)) if text=="下游处理失败"));
        assert_eq!(callbacks, 1);
        assert_eq!(accepted, 1);
    }
}
