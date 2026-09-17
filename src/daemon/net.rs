//! 节点间传输。Raft 自己处理丢包与重发，所以这里尽力而为：连不上就丢，队列满了就丢。
//! 帧格式：4 字节大端长度 + protobuf 编码的 `raft::eraftpb::Message`。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use protobuf::Message as _;
use raft::eraftpb::Message;

use super::executor::ExecEvent;

/// Raft 循环的全部输入。
#[derive(Debug)]
pub enum NodeInput {
    Raft(Box<Message>),
    Exec(ExecEvent),
    /// 管理命令：由 Leader 提交到日志，应用后经 `reply` 回结论。
    Admin { cmd: super::state::Command, reply: std::sync::mpsc::Sender<AdminReply> },
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

const MAX_FRAME: usize = 8 << 20;

pub struct TcpNet {
    outboxes: HashMap<u64, SyncSender<Vec<u8>>>,
}

impl TcpNet {
    /// 监听 `listen`，并为每个对端起一个发送线程。收到的消息送进 `inbox`。
    pub fn start(listen: &str, peers: &HashMap<u64, String>, inbox: Sender<NodeInput>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(listen)?;
        info!("节点间通信监听 {}", listen);
        thread::Builder::new().name("ltfsd-accept".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let inbox = inbox.clone();
                let _ = thread::Builder::new().name("ltfsd-recv".into()).spawn(move || receive(conn, inbox));
            }
        })?;
        let mut outboxes = HashMap::new();
        for (&id, addr) in peers {
            let (tx, rx) = sync_channel::<Vec<u8>>(1024);
            let addr = addr.clone();
            thread::Builder::new().name(format!("ltfsd-send-{}", id)).spawn(move || sender(id, addr, rx))?;
            outboxes.insert(id, tx);
        }
        Ok(Self { outboxes })
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

fn receive(mut conn: TcpStream, inbox: Sender<NodeInput>) {
    let _ = conn.set_nodelay(true);
    loop {
        let mut len = [0u8; 4];
        if conn.read_exact(&mut len).is_err() {
            return;
        }
        let n = u32::from_be_bytes(len) as usize;
        if n > MAX_FRAME {
            warn!("收到过大的帧 ({} 字节)，断开", n);
            return;
        }
        let mut buf = vec![0u8; n];
        if conn.read_exact(&mut buf).is_err() {
            return;
        }
        match Message::parse_from_bytes(&buf) {
            Ok(m) => {
                if inbox.send(NodeInput::Raft(Box::new(m))).is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!("无法解析的 Raft 消息: {}", e);
                return;
            }
        }
    }
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
        let ok = conn.as_mut().is_some_and(|c| {
            c.write_all(&(bytes.len() as u32).to_be_bytes()).and_then(|_| c.write_all(&bytes)).is_ok()
        });
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
