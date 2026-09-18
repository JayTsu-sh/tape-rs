//! ltfsd 的 Rust 客户端库。应用只面对这一层；线上协议（目前是 HTTP/1.1）是可替换的内部细节。
//!
//! 库替调用方处理三件事：
//! - **找 Leader**：给它全部节点的地址即可；不在服务的节点会答 503 并指出 Leader，库自动跟随。
//! - **故障切换期间的等待**：没有节点在服务时按 `retry_for` 重试。
//! - **结果未定的判定与安全重试**：上传途中连接断开，或服务端明确答"结果未定"时，
//!   库到（新的）Leader 上查询该路径：已提交则视为成功，否则重传。归档成功始终以卷提交为准。
//!
//! 判定"已提交的就是我这次上传的"靠内容哈希：服务端写带时算出 sha256 并记在索引和目录里，
//! 库把它与本地算出的值比对。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug)]
pub enum ClientError {
    /// 在 `retry_for` 之内没有任何节点在服务。
    NoLeader(String),
    /// 服务端明确拒绝：路径冲突、空间不足、路径非法等。重试无益。
    Rejected { status: u16, body: String },
    NotFound,
    Io(std::io::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NoLeader(s) => write!(f, "没有可用的 Leader: {}", s),
            ClientError::Rejected { status, body } => write!(f, "服务端拒绝 ({}): {}", status, body),
            ClientError::NotFound => write!(f, "不存在"),
            ClientError::Io(e) => write!(f, "I/O: {}", e),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub length: u64,
    /// 包含该文件的索引代数
    pub generation: u64,
    /// 回答这次查询的执行轮次
    pub round: u64,
    /// 所在磁带的条码
    pub barcode: String,
    /// 服务端写带时算出的内容 sha256（十六进制小写）
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInfo {
    pub uuid: String,
    pub name: String,
    pub file_limit: u64,
    pub tapes: Vec<String>,
    pub tape_details: Vec<TapeInfo>,
}

/// 池里一盘带的概况。`reclaimable` 是带上已用字节里目录已经不再引用的部分：
/// 被重写或被搬走的旧副本，以及历次收尾放弃的块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeInfo {
    pub barcode: String,
    pub state: String,
    pub generation: u64,
    pub files: u64,
    pub bytes_used: u64,
    pub live_files: u64,
    pub live_bytes: u64,
    pub reclaimable: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    pub generation: u64,
    /// 总共发了几次上传请求（1 = 一次成功）
    pub attempts: u32,
    /// 成功是通过事后查询路径判定的，而不是直接收到的提交确认
    pub resolved_by_query: bool,
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Client {
    endpoints: Vec<String>,
    /// 上次成功服务的节点，优先尝试
    leader: Option<String>,
    /// 没有 Leader 或结果未定时，最多坚持多久
    pub retry_for: Duration,
    pub io_timeout: Duration,
}

impl Client {
    /// `endpoints` 是全部节点的客户端接口地址，形如 `10.131.9.71:7401`。
    pub fn new<S: Into<String>>(endpoints: impl IntoIterator<Item = S>) -> Self {
        Self {
            endpoints: endpoints.into_iter().map(Into::into).collect(),
            leader: None,
            retry_for: Duration::from_secs(60),
            io_timeout: Duration::from_secs(600),
        }
    }

    /// 出错时第一项说明请求是否可能已经到达服务端：连接都没建立起来则为 false。
    fn request(&self, addr: &str, method: &str, path: &str, body: Option<&[u8]>) -> std::result::Result<Response, (bool, std::io::Error)> {
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| (false, e))?
            .next()
            .ok_or_else(|| (false, std::io::Error::other("地址无法解析")))?;
        let conn = TcpStream::connect_timeout(&sock, Duration::from_secs(3)).map_err(|e| (false, e))?;
        self.exchange(conn, addr, method, path, body).map_err(|e| (true, e))
    }

    fn exchange(&self, mut conn: TcpStream, addr: &str, method: &str, path: &str, body: Option<&[u8]>) -> std::io::Result<Response> {
        conn.set_read_timeout(Some(self.io_timeout))?;
        conn.set_write_timeout(Some(self.io_timeout))?;
        let _ = conn.set_nodelay(true);
        write!(conn, "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, encode_path(path), addr)?;
        // 大的内容先问一声：服务端准入成功才回 100，被拒绝就直接给最终结论，内容不必发
        let ask_first = body.is_some_and(|b| b.len() >= 64 * 1024);
        if let Some(b) = body {
            write!(conn, "Content-Length: {}\r\n", b.len())?;
        }
        if ask_first {
            conn.write_all(b"Expect: 100-continue\r\n")?;
        }
        conn.write_all(b"\r\n")?;
        if let (Some(b), false) = (body, ask_first) {
            conn.write_all(b)?;
        }
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if ask_first && line.split_whitespace().nth(1) == Some("100") {
            // 跳过 100 响应的空行，发内容，再读最终响应
            let mut blank = String::new();
            reader.read_line(&mut blank)?;
            conn.write_all(body.unwrap_or_default())?;
            line.clear();
            reader.read_line(&mut line)?;
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "连接在响应之前关闭"))?;
        let mut len: Option<usize> = None;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().ok();
                }
            }
        }
        let mut body = Vec::new();
        match len {
            Some(n) => {
                body.resize(n, 0);
                reader.read_exact(&mut body)?;
            }
            None => {
                reader.read_to_end(&mut body)?;
            }
        }
        Ok(Response { status, body })
    }

    /// 向 Leader 发一次请求。503 时跟随提示或换节点，直到 `retry_for` 用完。
    /// 传输层错误原样返回，由调用方决定含义（对上传来说它意味着结果未定）。
    fn to_leader(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Response> {
        let deadline = Instant::now() + self.retry_for;
        let mut last = String::from("尚未尝试");
        loop {
            let mut order: Vec<String> = self.leader.iter().cloned().collect();
            order.extend(self.endpoints.iter().filter(|e| Some(*e) != self.leader.as_ref()).cloned());
            let mut i = 0;
            while i < order.len() {
                let addr = order[i].clone();
                i += 1;
                match self.request(&addr, method, path, body) {
                    Ok(r) if r.status == 503 => {
                        let j = r.json();
                        last = format!("{} 不在服务: {}", addr, j.get("detail").and_then(Value::as_str).unwrap_or(""));
                        if let Some(url) = j.get("leader_url").and_then(Value::as_str) {
                            let hint = url.trim_start_matches("http://").to_string();
                            if !order[..i].contains(&hint) && !order[i..].contains(&hint) {
                                order.insert(i, hint);
                            }
                        }
                        if self.leader.as_deref() == Some(addr.as_str()) {
                            self.leader = None;
                        }
                    }
                    Ok(r) => {
                        self.leader = Some(addr);
                        return Ok(r);
                    }
                    Err((sent, e)) => {
                        if self.leader.as_deref() == Some(addr.as_str()) {
                            self.leader = None;
                        }
                        // 带内容的请求一旦可能已到达服务端，就可能已被执行：不能换个节点悄悄重发，
                        // 交给调用方按"结果未定"处理
                        if sent && body.is_some() {
                            return Err(ClientError::Io(e));
                        }
                        last = format!("{}: {}", addr, e);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(ClientError::NoLeader(last));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// 已提交视图里的文件。`Ok(None)` 表示未提交。
    pub fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        let r = self.to_leader("GET", &format!("/stat/{}", path.trim_start_matches('/')), None)?;
        match r.status {
            200 => {
                let j = r.json();
                Ok(Some(FileStat {
                    length: j["length"].as_u64().unwrap_or(0),
                    generation: j["generation"].as_u64().unwrap_or(0),
                    round: j["round"].as_u64().unwrap_or(0),
                    barcode: j["barcode"].as_str().unwrap_or("").to_string(),
                    sha256: j["sha256"].as_str().unwrap_or("").to_string(),
                }))
            }
            404 => Ok(None),
            s => Err(ClientError::Rejected { status: s, body: String::from_utf8_lossy(&r.body).into_owned() }),
        }
    }

    /// 上传并等到落带。返回即表示已随卷提交；出错表示确定没有成功或无法判定。
    pub fn put(&mut self, path: &str, data: &[u8]) -> Result<PutOutcome> {
        let route = format!("/files/{}?wait=1", path.trim_start_matches('/'));
        let digest: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(data).iter().map(|b| format!("{:02x}", b)).collect()
        };
        let deadline = Instant::now() + self.retry_for;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let uncertain = match self.to_leader("PUT", &route, Some(data)) {
                Ok(r) if r.status == 201 => {
                    return Ok(PutOutcome {
                        generation: r.json()["generation"].as_u64().unwrap_or(0),
                        attempts,
                        resolved_by_query: false,
                    });
                }
                // 服务端明确说没落带（仅暂存时执行者更换）：可以直接重传
                Ok(r) if r.status == 500 && r.json()["status"] == "failed" => false,
                Ok(r) if r.status == 500 => true,
                Ok(r) => return Err(ClientError::Rejected { status: r.status, body: String::from_utf8_lossy(&r.body).into_owned() }),
                Err(ClientError::Io(_)) => true,
                Err(e) => return Err(e),
            };
            if uncertain {
                // 结果未定：到当前 Leader 上查这个路径。已提交且内容哈希与本次上传相同即视为成功。
                if let Some(st) = self.stat(path)?.filter(|s| s.length == data.len() as u64 && s.sha256 == digest) {
                    return Ok(PutOutcome { generation: st.generation, attempts, resolved_by_query: true });
                }
            }
            if Instant::now() >= deadline {
                return Err(ClientError::NoLeader("上传在期限内没有得到确认".into()));
            }
        }
    }

    pub fn get(&mut self, path: &str) -> Result<Vec<u8>> {
        let r = self.to_leader("GET", &format!("/files/{}", path.trim_start_matches('/')), None)?;
        match r.status {
            200 => Ok(r.body),
            404 => Err(ClientError::NotFound),
            s => Err(ClientError::Rejected { status: s, body: String::from_utf8_lossy(&r.body).into_owned() }),
        }
    }

    pub fn list(&mut self) -> Result<Vec<(String, u64)>> {
        let r = self.to_leader("GET", "/list", None)?;
        let j = r.json();
        let mut out: Vec<(String, u64)> = j["files"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0))).collect())
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    // ---------- 管理 ----------

    fn admin(&mut self, method: &str, route: &str) -> Result<String> {
        // 带一个空内容：管理命令和上传一样，一旦可能已送达就不能悄悄换节点重发
        let r = self.to_leader(method, route, Some(b""))?;
        let j = r.json();
        match r.status {
            200 => Ok(j["result"].as_str().unwrap_or("").to_string()),
            s => Err(ClientError::Rejected { status: s, body: j["detail"].as_str().map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(&r.body).into_owned()) }),
        }
    }

    /// 新建池，返回池 UUID。`file_limit` 为每盘带的文件数软上限，`None` 用默认值。
    pub fn pool_create(&mut self, name: &str, file_limit: Option<u64>) -> Result<String> {
        let q = file_limit.map(|n| format!("?file_limit={}", n)).unwrap_or_default();
        self.admin("POST", &format!("/admin/pools/{}{}", name, q))
    }

    /// 把一盘带归入池。`pool` 可以是池名或 UUID。
    pub fn tape_assign(&mut self, barcode: &str, pool: &str) -> Result<String> {
        self.admin("POST", &format!("/admin/tapes/{}?pool={}", barcode, pool))
    }

    pub fn tape_unassign(&mut self, barcode: &str) -> Result<String> {
        self.admin("DELETE", &format!("/admin/tapes/{}", barcode))
    }

    /// 回收一盘带：把它上面还活着的文件搬到同池的其他带上，然后重新格式化它。
    /// 这个调用只是把带标成"回收中"就返回；搬迁由执行者在后台做，进度看 `pools`。
    pub fn tape_reclaim(&mut self, barcode: &str) -> Result<String> {
        self.admin("POST", &format!("/admin/tapes/{}/reclaim", barcode))
    }

    /// 池与磁带归属。由 `addr` 指定的节点用它已应用的日志回答；`None` 时问 Leader。
    pub fn pools(&mut self, addr: Option<&str>) -> Result<Vec<PoolInfo>> {
        let j = match addr {
            Some(a) => self.request(a, "GET", "/admin/pools", None).map_err(|(_, e)| e)?.json(),
            None => self.to_leader("GET", "/admin/pools", None)?.json(),
        };
        Ok(j["pools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| PoolInfo {
                        uuid: p["uuid"].as_str().unwrap_or("").to_string(),
                        name: p["name"].as_str().unwrap_or("").to_string(),
                        file_limit: p["file_limit"].as_u64().unwrap_or(0),
                        tapes: p["tapes"].as_array().map(|t| t.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                        tape_details: p["tape_details"]
                            .as_array()
                            .map(|t| {
                                t.iter()
                                    .map(|d| TapeInfo {
                                        barcode: d["barcode"].as_str().unwrap_or("").to_string(),
                                        state: d["state"].as_str().unwrap_or("-").to_string(),
                                        generation: d["generation"].as_u64().unwrap_or(0),
                                        files: d["files"].as_u64().unwrap_or(0),
                                        bytes_used: d["bytes_used"].as_u64().unwrap_or(0),
                                        live_files: d["live_files"].as_u64().unwrap_or(0),
                                        live_bytes: d["live_bytes"].as_u64().unwrap_or(0),
                                        reclaimable: d["reclaimable_bytes"].as_u64().unwrap_or(0),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// 某个节点的自述（不跟随 Leader）。
    pub fn node_status(&self, addr: &str) -> Result<Value> {
        Ok(self.request(addr, "GET", "/cluster", None).map_err(|(_, e)| e)?.json())
    }
}

fn encode_path(path: &str) -> String {
    let (p, q) = path.split_once('?').map(|(p, q)| (p, Some(q))).unwrap_or((path, None));
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    if let Some(q) = q {
        out.push('?');
        out.push_str(q);
    }
    out
}
