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
    /// 只创建（`put_new`）：路径已有已提交的当前版本。
    Exists(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NoLeader(s) => write!(f, "没有可用的 Leader: {}", s),
            ClientError::Rejected { status, body } => write!(f, "服务端拒绝 ({}): {}", status, body),
            ClientError::NotFound => write!(f, "不存在"),
            ClientError::Exists(p) => write!(f, "路径已存在: {}", p),
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

/// 路径的状态（`stat_path`）。`state` 为 `committed`、`uploading` 或 `staged`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    pub state: String,
    /// committed 时是文件长度；在途时是已接收的长度
    pub length: u64,
    /// 已提交的当前版本。在途的新版本落带之前，读到的是它
    pub current: Option<FileStat>,
}

/// 目录的一个直接子项（`list_dir`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    /// 文件：`committed`、`uploading` 或 `staged`；目录为空串
    pub state: String,
    pub committed_length: Option<u64>,
    pub staged_length: Option<u64>,
}

fn file_stat(j: &Value) -> FileStat {
    FileStat {
        length: j["length"].as_u64().unwrap_or(0),
        generation: j["generation"].as_u64().unwrap_or(0),
        round: j["round"].as_u64().unwrap_or(0),
        barcode: j["barcode"].as_str().unwrap_or("").to_string(),
        sha256: j["sha256"].as_str().unwrap_or("").to_string(),
    }
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
    /// 已随卷提交落带。只有不等待落带的上传（`put_file(.., wait = false)`）会是 false
    pub committed: bool,
}

/// 请求内容：内存里的一段，或本地文件（发送时再读，重发时从头读）。
#[derive(Clone)]
enum Body<'a> {
    Mem(&'a [u8]),
    File(&'a std::path::Path, u64),
}

impl Body<'_> {
    fn len(&self) -> u64 {
        match self {
            Body::Mem(b) => b.len() as u64,
            Body::File(_, n) => *n,
        }
    }

    fn send(&self, conn: &mut TcpStream) -> std::io::Result<()> {
        match self {
            Body::Mem(b) => conn.write_all(b),
            Body::File(p, n) => {
                let f = std::fs::File::open(p)?;
                let copied = std::io::copy(&mut std::io::BufReader::with_capacity(1 << 20, f).take(*n), conn)?;
                if copied != *n {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "本地文件在上传途中变短了"));
                }
                Ok(())
            }
        }
    }
}

/// 本地文件的 sha256（十六进制小写）。
pub fn sha256_file(p: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(p)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{:02x}", b)).collect())
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

#[derive(Clone)]
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
        self.request_with(addr, method, path, body.map(Body::Mem), &[], None)
    }

    /// `sink` 给出时，200 响应的内容直接写进它，`Response::body` 为空。
    fn request_with<'w>(
        &self,
        addr: &str,
        method: &str,
        path: &str,
        body: Option<Body<'_>>,
        headers: &[(&str, &str)],
        sink: Option<&mut (dyn Write + 'w)>,
    ) -> std::result::Result<Response, (bool, std::io::Error)> {
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| (false, e))?
            .next()
            .ok_or_else(|| (false, std::io::Error::other("地址无法解析")))?;
        let conn = TcpStream::connect_timeout(&sock, Duration::from_secs(3)).map_err(|e| (false, e))?;
        self.exchange(conn, addr, method, path, body, headers, sink).map_err(|e| (true, e))
    }

    #[allow(clippy::too_many_arguments)]
    fn exchange<'w>(
        &self,
        mut conn: TcpStream,
        addr: &str,
        method: &str,
        path: &str,
        body: Option<Body<'_>>,
        headers: &[(&str, &str)],
        sink: Option<&mut (dyn Write + 'w)>,
    ) -> std::io::Result<Response> {
        conn.set_read_timeout(Some(self.io_timeout))?;
        conn.set_write_timeout(Some(self.io_timeout))?;
        let _ = conn.set_nodelay(true);
        write!(conn, "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, encode_path(path), addr)?;
        for (k, v) in headers {
            write!(conn, "{}: {}\r\n", k, v)?;
        }
        // 大的内容先问一声：服务端准入成功才回 100，被拒绝就直接给最终结论，内容不必发
        let ask_first = body.as_ref().is_some_and(|b| b.len() >= 64 * 1024);
        if let Some(b) = &body {
            write!(conn, "Content-Length: {}\r\n", b.len())?;
        }
        if ask_first {
            conn.write_all(b"Expect: 100-continue\r\n")?;
        }
        conn.write_all(b"\r\n")?;
        if let (Some(b), false) = (&body, ask_first) {
            b.send(&mut conn)?;
        }
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if ask_first && line.split_whitespace().nth(1) == Some("100") {
            // 跳过 100 响应的空行，发内容，再读最终响应
            let mut blank = String::new();
            reader.read_line(&mut blank)?;
            if let Some(b) = &body {
                b.send(&mut conn)?;
            }
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
        if let (Some(w), 200) = (sink, status) {
            match len {
                Some(n) => {
                    let copied = std::io::copy(&mut (&mut reader).take(n as u64), w)?;
                    if copied != n as u64 {
                        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "内容没有传完"));
                    }
                }
                None => {
                    std::io::copy(&mut reader, w)?;
                }
            }
            return Ok(Response { status, body: Vec::new() });
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
        self.to_leader_with(method, path, body.map(Body::Mem), &[], None)
    }

    fn to_leader_with<'w>(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Body<'_>>,
        headers: &[(&str, &str)],
        mut sink: Option<&mut (dyn Write + 'w)>,
    ) -> Result<Response> {
        let deadline = Instant::now() + self.retry_for;
        let mut last = String::from("尚未尝试");
        loop {
            let mut order: Vec<String> = self.leader.iter().cloned().collect();
            order.extend(self.endpoints.iter().filter(|e| Some(*e) != self.leader.as_ref()).cloned());
            let mut i = 0;
            while i < order.len() {
                let addr = order[i].clone();
                i += 1;
                match self.request_with(&addr, method, path, body.clone(), headers, sink.as_deref_mut()) {
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
                        // 内容可能已经写了一部分进 sink：不能在别的节点上接着写，交给调用方从头再来
                        if sent && sink.is_some() {
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

    /// 已提交的当前版本。`Ok(None)` 表示没有（可能有新版本在途，见 `stat_path`）。
    pub fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        Ok(self.stat_path(path)?.and_then(|p| p.current))
    }

    /// 路径的全貌：已提交的当前版本加在途的新版本。`Ok(None)` 表示不存在。
    pub fn stat_path(&mut self, path: &str) -> Result<Option<PathStat>> {
        let r = self.to_leader("GET", &format!("/stat/{}", path.trim_start_matches('/')), None)?;
        match r.status {
            200 => {
                let j = r.json();
                let state = j["state"].as_str().unwrap_or("committed").to_string();
                let current = if state == "committed" { Some(file_stat(&j)) } else { j.get("current").map(file_stat) };
                Ok(Some(PathStat { length: j["length"].as_u64().unwrap_or(0), state, current }))
            }
            404 => Ok(None),
            s => Err(ClientError::Rejected { status: s, body: String::from_utf8_lossy(&r.body).into_owned() }),
        }
    }

    /// 上传并等到落带（替换已有版本）。返回即表示已随卷提交；出错表示确定没有成功或无法判定。
    pub fn put(&mut self, path: &str, data: &[u8]) -> Result<PutOutcome> {
        self.put_inner(path, data, false)
    }

    /// 只创建：路径已有已提交的当前版本时返回 `Exists`，不产生新版本。
    pub fn put_new(&mut self, path: &str, data: &[u8]) -> Result<PutOutcome> {
        self.put_inner(path, data, true)
    }

    fn put_inner(&mut self, path: &str, data: &[u8], create_only: bool) -> Result<PutOutcome> {
        let digest: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(data).iter().map(|b| format!("{:02x}", b)).collect()
        };
        self.put_body(path, Body::Mem(data), &digest, create_only, true)
    }

    /// 上传本地文件，内容边读边发，不整个读进内存。`wait` 为假时服务端暂存即返回
    /// （`PutOutcome::committed == false`），这不是归档确认。
    pub fn put_file(&mut self, path: &str, local: &std::path::Path, create_only: bool, wait: bool) -> Result<PutOutcome> {
        let len = std::fs::metadata(local)?.len();
        let digest = sha256_file(local)?;
        self.put_body(path, Body::File(local, len), &digest, create_only, wait)
    }

    fn put_body(&mut self, path: &str, body: Body<'_>, digest: &str, create_only: bool, wait: bool) -> Result<PutOutcome> {
        let headers: &[(&str, &str)] = if create_only { &[("If-None-Match", "*")] } else { &[] };
        let route = format!("/files/{}{}", path.trim_start_matches('/'), if wait { "?wait=1" } else { "" });
        let len = body.len();
        let deadline = Instant::now() + self.retry_for;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let uncertain = match self.to_leader_with("PUT", &route, Some(body.clone()), headers, None) {
                Ok(r) if r.status == 201 => {
                    return Ok(PutOutcome {
                        generation: r.json()["generation"].as_u64().unwrap_or(0),
                        attempts,
                        resolved_by_query: false,
                        committed: true,
                    });
                }
                Ok(r) if r.status == 202 && !wait => {
                    return Ok(PutOutcome { generation: 0, attempts, resolved_by_query: false, committed: false });
                }
                // 服务端明确说没落带（仅暂存时执行者更换）：可以直接重传
                Ok(r) if r.status == 500 && r.json()["status"] == "failed" => false,
                Ok(r) if r.status == 500 => true,
                Ok(r) if r.status == 412 => return Err(ClientError::Exists(path.to_string())),
                Ok(r) => return Err(ClientError::Rejected { status: r.status, body: String::from_utf8_lossy(&r.body).into_owned() }),
                Err(ClientError::Io(_)) => true,
                Err(e) => return Err(e),
            };
            if uncertain {
                // 结果未定：到当前 Leader 上查这个路径。已提交且内容哈希与本次上传相同即视为成功。
                // 路径上还有在途版本（可能就是这次上传，还在暂存或落带途中）：等它有结论再判定，
                // 此时重传只会撞上路径预留。
                let mut st = self.stat_path(path)?;
                if !wait && st.as_ref().is_some_and(|s| s.state == "staged" && s.length == len) {
                    // 只要求暂存：路径上有一个完整暂存、长度相同的版本，按这次上传已暂存处理。
                    // 内容是否就是这次的，要等落带后按哈希确认（FUSE 的 fsync 做这一步）
                    return Ok(PutOutcome { generation: 0, attempts, resolved_by_query: true, committed: false });
                }
                while st.as_ref().is_some_and(|s| s.state != "committed") && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(300));
                    st = self.stat_path(path)?;
                }
                if let Some(cur) = st.and_then(|s| s.current).filter(|s| s.length == len && s.sha256 == digest) {
                    return Ok(PutOutcome { generation: cur.generation, attempts, resolved_by_query: true, committed: true });
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

    /// 读出整个文件，边收边写进 `out`，返回字节数。出错时 `out` 里可能已有一部分内容，调用方从头重来。
    pub fn get_to(&mut self, path: &str, out: &mut dyn Write) -> Result<u64> {
        let mut counter = CountingWriter { inner: out, n: 0 };
        let r = self.to_leader_with("GET", &format!("/files/{}", path.trim_start_matches('/')), None, &[], Some(&mut counter))?;
        match r.status {
            200 => Ok(counter.n),
            404 => Err(ClientError::NotFound),
            s => Err(ClientError::Rejected { status: s, body: String::from_utf8_lossy(&r.body).into_owned() }),
        }
    }

    /// 读 `[offset, offset + len)`，超出文件末尾的部分截掉。起点越过末尾时返回 `Rejected{416}`。
    pub fn get_range(&mut self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let r = self.to_leader_with("GET", &format!("/files/{}", path.trim_start_matches('/')), None, &[("Range", &range)], None)?;
        match r.status {
            206 => Ok(r.body),
            // 服务端不认这个 Range 时回整个文件
            200 => Ok(r.body.into_iter().skip(offset as usize).take(len as usize).collect()),
            404 => Err(ClientError::NotFound),
            s => Err(ClientError::Rejected { status: s, body: String::from_utf8_lossy(&r.body).into_owned() }),
        }
    }

    /// 目录的直接子项。`pending` 为真时包括只在途、尚未提交的文件。目录不存在时 `NotFound`。
    pub fn list_dir(&mut self, dir: &str, pending: bool) -> Result<Vec<DirItem>> {
        let q = format!("/list?dir={}{}", encode_query(dir), if pending { "&pending=1" } else { "" });
        let r = self.to_leader("GET", &q, None)?;
        match r.status {
            200 => Ok(r.json()["entries"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|e| DirItem {
                            name: e["name"].as_str().unwrap_or("").to_string(),
                            is_dir: e["type"] == "dir",
                            state: e["state"].as_str().unwrap_or("").to_string(),
                            committed_length: e["committed_length"].as_u64(),
                            staged_length: e["staged_length"].as_u64(),
                        })
                        .collect()
                })
                .unwrap_or_default()),
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

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    n: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.n += k as u64;
        Ok(k)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 查询参数值的百分号编码（`/` 也编码，服务端 `percent_decode` 还原）。
fn encode_query(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
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
