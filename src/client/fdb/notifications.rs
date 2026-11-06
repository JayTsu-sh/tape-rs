//! Durable external notification subscriptions. Printing never implies ACK.
use super::*;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;

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
        "/admin/notify/consumers/{id}{suffix}?library={library}"
    ))
}
fn result(response: Response) -> Result<Value> {
    if response.status == 200 {
        Ok(response.json())
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
            &format!("/admin/notify/events/inject?library={library}"),
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
                "/admin/notify/consumers?library={library}{}",
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
        loop {
            for endpoint in &self.transport.endpoints {
                match rt.block_on(watch_once(endpoint, &path, &mut receive)) {
                    Ok(false) => return Ok(()),
                    Ok(true) | Err(ClientError::NoLeader(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
fn connection_error(error: impl std::fmt::Display) -> ClientError {
    ClientError::NoLeader(format!("通知连接暂不可用：{error}"))
}

async fn watch_once<F>(endpoint: &str, path: &str, receive: &mut F) -> Result<bool>
where
    F: FnMut(&Value) -> Result<bool>,
{
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
    // Dropping this guard cancels the connection on callback failure or stop.
    struct Connection(tokio::task::JoinHandle<()>);
    impl Drop for Connection {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _connection = Connection(task);
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
    let mut body = response.into_body();
    let mut parser = Parser::default();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), body.frame())
            .await
            .map_err(connection_error)?;
        let Some(frame) = frame else {
            return Ok(true);
        };
        if let Ok(data) = frame.map_err(connection_error)?.into_data()
            && !parser.push(&data, receive)?
        {
            return Ok(false);
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
                        return Err(connection_error(value));
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
