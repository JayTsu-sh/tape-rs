//! 节点间传输。Raft 自己处理丢包与重发，所以 Raft 消息这条路尽力而为：连不上就丢，队列满了就丢。
//!
//! 帧格式：1 字节种类 + 4 字节大端长度 + 载荷。
//!
//! - 种类 0：protobuf 编码的 `raft::eraftpb::Message`。
//! - 种类 1：目录库快照请求（载荷是 8 字节大端的最小索引）。应答在同一条连接上写回，
//!   格式见 `serve_directory`。目录库动辄几百 MB，塞不进 Raft 消息，所以走这条单独的路。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use protobuf::Message as _;
use raft::eraftpb::Message;
use sha2::{Digest, Sha256};

use super::executor::ExecEvent;
use crate::error::{Result, TapeError};

/// Raft 循环的全部输入。
#[derive(Debug)]
pub enum NodeInput {
    Raft(Box<Message>),
    Exec(ExecEvent),
    /// 管理命令：由 Leader 提交到日志，应用后经 `reply` 回结论。
    Admin { cmd: super::state::Command, reply: std::sync::mpsc::Sender<AdminReply> },
    /// 目录库拉取的结果：`Ok((覆盖到的索引, 落地的文件))`。在别的线程上拉，不挡住 Raft 循环。
    Directory(std::result::Result<(u64, PathBuf), String>),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminReply {
    Ok(String),
    /// 被状态机拒绝（例如重名、已归属）。状态未变。
    Rejected(String),
    /// 本节点不是 Leader。带上它所知的 Leader 编号。
    NotLeader(u64),
}

pub trait Network: Send {
    fn send(&self, msg: Message);
}

/// 从别的节点拉目录库快照。与 `Network` 分开：它要在拉取线程上用，必须 `Sync`。
pub trait DirectoryFetch: Send + Sync {
    /// 从节点 `from` 取一份覆盖到至少 `min_index` 的目录库快照写进 `dest`，
    /// 返回它实际覆盖到的日志索引（可能比要求的新）。
    fn fetch(&self, from: u64, min_index: u64, dest: &Path) -> Result<u64>;
}

const MAX_FRAME: usize = 8 << 20;
const FRAME_RAFT: u8 = 0;
const FRAME_DIRECTORY: u8 = 1;
/// 目录库快照的上限。超过它说明规模已经不是这套实现的设计点了。
const MAX_DIRECTORY: u64 = 8 << 30;

pub struct TcpNet {
    outboxes: HashMap<u64, SyncSender<Vec<u8>>>,
    peers: HashMap<u64, String>,
    bound: std::net::SocketAddr,
}

impl TcpNet {
    /// 监听 `listen`，并为每个对端起一个发送线程。收到的消息送进 `inbox`。
    /// `directory_snapshot` 是本节点目录库快照文件的位置，用来应答别人的拉取请求。
    pub fn start(
        listen: &str,
        peers: &HashMap<u64, String>,
        inbox: Sender<NodeInput>,
        directory_snapshot: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(listen)?;
        let bound = listener.local_addr()?;
        info!("节点间通信监听 {}", bound);
        thread::Builder::new().name("ltfsd-accept".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let inbox = inbox.clone();
                let snap = directory_snapshot.clone();
                let _ = thread::Builder::new().name("ltfsd-recv".into()).spawn(move || receive(conn, inbox, snap));
            }
        })?;
        let mut outboxes = HashMap::new();
        for (&id, addr) in peers {
            let (tx, rx) = sync_channel::<Vec<u8>>(1024);
            let addr = addr.clone();
            thread::Builder::new().name(format!("ltfsd-send-{}", id)).spawn(move || sender(id, addr, rx))?;
            outboxes.insert(id, tx);
        }
        Ok(Self { outboxes, peers: peers.clone(), bound })
    }

    /// 实际监听的地址（`--listen` 给 0 端口时用得上）。
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.bound
    }

    /// 拉目录库用的句柄。它只需要对端地址，所以可以随便跨线程用。
    pub fn fetcher(&self) -> Arc<dyn DirectoryFetch> {
        Arc::new(TcpFetch { peers: self.peers.clone() })
    }
}

impl Network for TcpNet {
    fn send(&self, msg: Message) {
        let Some(tx) = self.outboxes.get(&msg.to) else {
            return;
        };
        let Ok(bytes) = msg.write_to_bytes() else {
            return;
        };
        if let Err(TrySendError::Full(_)) = tx.try_send(bytes) {
            debug!("发往节点 {} 的队列已满，丢弃", msg.to);
        }
    }
}

struct TcpFetch {
    peers: HashMap<u64, String>,
}

fn io_err(what: &str, e: std::io::Error) -> TapeError {
    TapeError::Ltfs(format!("拉取目录库：{}: {}", what, e))
}

impl DirectoryFetch for TcpFetch {
    fn fetch(&self, from: u64, min_index: u64, dest: &Path) -> Result<u64> {
        let addr = self.peers.get(&from).ok_or_else(|| TapeError::Ltfs(format!("不认识节点 {}", from)))?;
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| io_err("解析地址", e))?
            .next()
            .ok_or_else(|| TapeError::Ltfs(format!("无法解析 {}", addr)))?;
        let mut c = TcpStream::connect_timeout(&sock, Duration::from_secs(3)).map_err(|e| io_err("连接", e))?;
        c.set_read_timeout(Some(Duration::from_secs(60))).map_err(|e| io_err("设置超时", e))?;
        c.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| io_err("设置超时", e))?;
        let mut req = vec![FRAME_DIRECTORY];
        req.extend_from_slice(&8u32.to_be_bytes());
        req.extend_from_slice(&min_index.to_be_bytes());
        c.write_all(&req).map_err(|e| io_err("发送请求", e))?;

        let mut head = [0u8; 17]; // 1 状态 + 8 索引 + 8 长度
        c.read_exact(&mut head).map_err(|e| io_err("读应答头", e))?;
        if head[0] != 0 {
            return Err(TapeError::Ltfs(format!("节点 {} 暂时给不出覆盖到索引 {} 的目录库快照", from, min_index)));
        }
        let index = u64::from_be_bytes(head[1..9].try_into().expect("8 字节"));
        let len = u64::from_be_bytes(head[9..17].try_into().expect("8 字节"));
        if index < min_index {
            return Err(TapeError::Ltfs(format!("节点 {} 的目录库快照只到索引 {}，不够 {}", from, index, min_index)));
        }
        if len > MAX_DIRECTORY {
            return Err(TapeError::Ltfs(format!("目录库快照过大：{} 字节", len)));
        }
        let tmp = dest.with_extension("part");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| io_err("建临时文件", e))?;
            let mut hasher = Sha256::new();
            let mut left = len;
            let mut buf = vec![0u8; 1 << 20];
            while left > 0 {
                let n = buf.len().min(left as usize);
                c.read_exact(&mut buf[..n]).map_err(|e| io_err("读内容", e))?;
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n]).map_err(|e| io_err("写文件", e))?;
                left -= n as u64;
            }
            f.sync_all().map_err(|e| io_err("落盘", e))?;
            let mut want = [0u8; 32];
            c.read_exact(&mut want).map_err(|e| io_err("读校验和", e))?;
            if hasher.finalize().as_slice() != want {
                let _ = std::fs::remove_file(&tmp);
                return Err(TapeError::Ltfs("目录库快照校验和不符".into()));
            }
        }
        std::fs::rename(&tmp, dest).map_err(|e| io_err("改名", e))?;
        Ok(index)
    }
}

fn receive(mut conn: TcpStream, inbox: Sender<NodeInput>, snapshot: Option<PathBuf>) {
    let _ = conn.set_nodelay(true);
    loop {
        let mut head = [0u8; 5];
        if conn.read_exact(&mut head).is_err() {
            return;
        }
        let n = u32::from_be_bytes(head[1..5].try_into().expect("4 字节")) as usize;
        if n > MAX_FRAME {
            warn!("收到过大的帧 ({} 字节)，断开", n);
            return;
        }
        let mut buf = vec![0u8; n];
        if conn.read_exact(&mut buf).is_err() {
            return;
        }
        match head[0] {
            FRAME_RAFT => match Message::parse_from_bytes(&buf) {
                Ok(m) => {
                    if inbox.send(NodeInput::Raft(Box::new(m))).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    warn!("无法解析的 Raft 消息: {}", e);
                    return;
                }
            },
            FRAME_DIRECTORY => {
                let min = buf.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes).unwrap_or(0);
                if let Err(e) = serve_directory(&mut conn, snapshot.as_deref(), min) {
                    debug!("应答目录库拉取失败: {}", e);
                }
                return; // 这条连接是一次性的
            }
            other => {
                warn!("未知的帧种类 {}，断开", other);
                return;
            }
        }
    }
}

/// 应答目录库拉取。格式：1 字节状态（0 成功，1 给不出）+ 8 字节覆盖到的索引 +
/// 8 字节长度 + 文件内容 + 32 字节 sha256。
fn serve_directory(conn: &mut TcpStream, snapshot: Option<&Path>, min_index: u64) -> std::io::Result<()> {
    let _ = conn.set_write_timeout(Some(Duration::from_secs(60)));
    let ready = snapshot.filter(|p| p.exists()).and_then(|p| {
        let index = super::directory::Directory::applied_index_of(p).ok()?;
        (index >= min_index).then_some((p, index))
    });
    let Some((path, index)) = ready else {
        conn.write_all(&[1u8])?;
        conn.write_all(&[0u8; 16])?;
        return Ok(());
    };
    let bytes = std::fs::read(path)?;
    let mut out = vec![0u8];
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    conn.write_all(&out)?;
    conn.write_all(&bytes)?;
    conn.write_all(&Sha256::digest(&bytes))?;
    info!("已送出覆盖到索引 {} 的目录库快照（{} 字节）", index, bytes.len());
    Ok(())
}

fn sender(id: u64, addr: String, rx: Receiver<Vec<u8>>) {
    let mut conn: Option<TcpStream> = None;
    while let Ok(bytes) = rx.recv() {
        if conn.is_none() {
            conn = connect(&addr);
            if conn.is_none() {
                continue; // 连不上：丢弃这条，Raft 会重发
            }
            debug!("已连接节点 {} ({})", id, addr);
        }
        let mut frame = vec![FRAME_RAFT];
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&bytes);
        let ok = conn.as_mut().is_some_and(|c| c.write_all(&frame).is_ok());
        if !ok {
            conn = None;
        }
    }
}

fn connect(addr: &str) -> Option<TcpStream> {
    let sock = addr.to_socket_addrs().ok()?.next()?;
    let c = TcpStream::connect_timeout(&sock, Duration::from_millis(300)).ok()?;
    let _ = c.set_nodelay(true);
    let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::directory::Directory;
    use crate::daemon::state::{Applied, Command, ControlState};

    /// 两种帧走同一个监听口：Raft 消息照常送进 inbox，目录库快照在同一条连接上取回来。
    #[test]
    fn both_frame_kinds_work_over_one_listener() {
        let dir = std::env::temp_dir().join(format!("ltfsd-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("directory.db");

        // 准备一份有内容的目录库快照
        let mut ctl = ControlState::default();
        let mut d = Directory::open(&live).unwrap();
        for (i, cmd) in [
            Command::PoolCreate { uuid: "u-1".into(), name: "archive".into(), file_limit: 10 },
            Command::TapeAssign { barcode: "T1".into(), pool: "archive".into() },
        ]
        .iter()
        .enumerate()
        {
            let out = ctl.apply(i as u64 + 1, cmd);
            assert!(matches!(out, Applied::Admin(Ok(_))));
            d.apply(i as u64 + 1, cmd, &out).unwrap();
        }
        let snap = Directory::snapshot_path(&live);
        d.write_snapshot(&snap).unwrap();
        assert_eq!(Directory::applied_index_of(&snap).unwrap(), 2);

        // 服务端先起，拿到实际端口后再起客户端，客户端把它当作节点 7
        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel();
        let server = TcpNet::start("127.0.0.1:0", &HashMap::new(), inbox_tx, Some(snap.clone())).unwrap();
        let peers = HashMap::from([(7u64, server.local_addr().to_string())]);
        let (client_inbox, _client_rx) = std::sync::mpsc::channel::<NodeInput>();
        let client = TcpNet::start("127.0.0.1:0", &peers, client_inbox, None).unwrap();

        // Raft 消息
        let mut m = Message::default();
        m.to = 7;
        m.from = 1;
        m.term = 42;
        client.send(m);
        match inbox_rx.recv_timeout(Duration::from_secs(5)).expect("收到消息") {
            NodeInput::Raft(got) => assert_eq!((got.to, got.from, got.term), (7, 1, 42)),
            other => panic!("{:?}", other),
        }

        // 目录库快照
        let fetcher = client.fetcher();
        let dest = dir.join("pulled.db");
        assert_eq!(fetcher.fetch(7, 2, &dest).unwrap(), 2);
        assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&snap).unwrap());
        assert_eq!(Directory::open_reader(&dest).unwrap().pools().unwrap().len(), 1);
        // 覆盖不到要求的索引时明确拒绝，不给出半份
        let dest2 = dir.join("pulled2.db");
        assert!(fetcher.fetch(7, 99, &dest2).is_err());
        assert!(!dest2.exists());
        assert!(fetcher.fetch(9, 1, &dest2).is_err(), "不认识的节点");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
