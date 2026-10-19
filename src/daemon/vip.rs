//! 同子网 IPv4 HTTP VIP。仅管理显式地址，不修改接口主地址、网关或子网路由。
//! 使用 rtnetlink 和 AF_PACKET；有限地址寿命使进程崩溃/暂停后不留下永久 VIP。

use std::ffi::{CStr, CString};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};
use nix::libc;

use super::executor::{DeviceProvider, ManagedDevice};
use super::files::FileService;
use super::http::ready_owner;
use super::net::NodeInput;
use super::node::{NodeStatus, SharedStatus};
use super::state::Command;

const LIFETIME: u32 = 2;
const POLL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct VipConfig {
    pub address: Ipv4Addr,
    pub interface: String,
}

impl VipConfig {
    /// HTTP 必须监听所有本机 IPv4 地址，避免备用节点启动时 VIP 尚不存在而 bind 失败。
    pub fn validate(&self, listen: &str, permit: Duration) -> io::Result<()> {
        let listen: SocketAddr = listen
            .parse()
            .map_err(|_| invalid("VIP 需要 IPv4 数字监听地址"))?;
        if !matches!(listen, SocketAddr::V4(a) if a.ip().is_unspecified() && a.port() != 0) {
            return Err(invalid("VIP 需要 --client-listen 0.0.0.0:端口"));
        }
        if permit < Duration::from_secs(4) {
            return Err(invalid("HTTP VIP 需要 --execution-permit-ms 至少 4000"));
        }
        self.validate_address()
    }

    fn validate_address(&self) -> io::Result<()> {
        if self.interface.is_empty()
            || self.interface.len() + 2 >= libc::IFNAMSIZ
            || self
                .interface
                .bytes()
                .any(|b| b == 0 || b == b'/' || b == b':')
        {
            return Err(invalid("VIP 接口名需要 1..=13 字节，且不能含 NUL、/ 或 :"));
        }
        let first = self.address.octets()[0];
        if first == 0 || first == 127 || first >= 224 || self.address.is_link_local() {
            return Err(invalid("VIP 必须是非回环、非链路本地的 IPv4 单播地址"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

trait AddressManager {
    fn refresh(&mut self, lifetime: u32) -> io::Result<()>;
    fn remove(&mut self) -> io::Result<()>;
    fn announce(&mut self) -> io::Result<()>;
}

/// 内核地址寿命和公告；调用前必须已核对 Raft VIP 授权。
#[derive(Default)]
struct Lease {
    held: bool,
    announced: Option<Instant>,
}

impl Lease {
    fn step(
        &mut self,
        now: Instant,
        owner: Option<(u64, Instant)>,
        net: &mut impl AddressManager,
    ) -> io::Result<()> {
        let owner = owner.filter(|(_, deadline)| {
            deadline.saturating_duration_since(now) >= Duration::from_secs(2)
        });
        let Some((_, deadline)) = owner else {
            // 即使本进程尚未持有，也清理同标签的旧进程残留；不碰其他地址。
            return self.withdraw(net);
        };
        // 留一秒余量，且不把内核地址寿命续到多数派许可截止之后。
        let lifetime = deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .saturating_sub(1)
            .min(u64::from(LIFETIME)) as u32;
        if lifetime == 0 {
            return self.withdraw(net);
        }
        net.refresh(lifetime)?;
        // 地址操作可能经历调度暂停；过期回复不再发送旧 owner 的 ARP 公告。
        if deadline <= Instant::now() {
            return self.withdraw(net);
        }
        if !self.held {
            info!("HTTP VIP 已接管");
            self.announced = None;
        }
        self.held = true;
        // 周期公告也覆盖内核到期后重新添加、链路短暂中断和邻居缓存丢失。
        if self
            .announced
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(2))
        {
            net.announce()?;
            self.announced = Some(now);
        }
        Ok(())
    }

    fn withdraw(&mut self, net: &mut impl AddressManager) -> io::Result<()> {
        net.remove()?;
        self.held = false;
        self.announced = None;
        Ok(())
    }
}

/// 单线程完成地址操作和撤址确认。发送释放后封存 token，即使共享快照尚未更新也不重用。
#[derive(Default)]
struct VipSession {
    lease: Lease,
    released_token: u64,
    token: Option<u64>,
}

impl VipSession {
    fn step(
        &mut self,
        now: Instant,
        status: &NodeStatus,
        owner: Option<(u64, Instant)>,
        address: Ipv4Addr,
        net: &mut impl AddressManager,
    ) -> io::Result<Option<Command>> {
        let record = status.vips.get(&address).cloned().unwrap_or_default();
        let assigned = status
            .executor
            .as_ref()
            .filter(|e| e.node == status.id && !status.shutting_down)
            .map(|e| e.round);
        if let Some(holder) = &record.holder
            && holder.node == status.id
            && Some(holder.round) == assigned
            && record.token > self.released_token
        {
            if self.token != Some(record.token) {
                self.lease.withdraw(net)?;
                self.token = Some(record.token);
            }
            if let Err(e) = self
                .lease
                .step(now, owner.filter(|o| o.0 == holder.round), net)
            {
                self.released_token = self.released_token.max(record.token);
                return Err(e);
            }
            return Ok(None);
        }
        // remove 的成功约定包括删除 ACK 后回读为空；失败不能提交释放。
        self.lease.withdraw(net)?;
        self.token = None;
        if let Some(holder) = record.holder {
            if holder.node == status.id {
                self.released_token = self.released_token.max(record.token);
                return Ok(Some(Command::VipRelease {
                    address,
                    node: holder.node,
                    round: holder.round,
                    token: record.token,
                }));
            }
        } else if let Some(round) = assigned {
            return Ok(Some(Command::VipAcquire {
                address,
                node: status.id,
                round,
                expected_token: record.token,
            }));
        }
        Ok(None)
    }
}

/// 持有期间运行独立网络线程；Drop 先撤下地址再等待线程退出。
pub struct HttpVip {
    config: VipConfig,
    net: Option<LinuxAddress>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    _lock: nix::fcntl::Flock<std::fs::File>,
}

impl HttpVip {
    /// 在设备/节点线程启动前核对配置及权限，取得本机 VIP 锁。不添加地址。
    pub fn prepare(config: VipConfig, permit: Duration) -> io::Result<Self> {
        if permit < Duration::from_secs(4) {
            return Err(invalid("HTTP VIP 需要至少四秒的执行许可"));
        }
        config.validate_address()?;
        std::fs::create_dir_all("/run/lock/tape-rs")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(format!(
                "/run/lock/tape-rs/http-vip-{}.lock",
                config.address
            ))?;
        let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| io::Error::other(format!("本机已有实例管理 HTTP VIP：{e}")))?;
        let net = LinuxAddress::new(&config)?;
        Ok(Self {
            config,
            net: Some(net),
            stop: None,
            worker: None,
            _lock: lock,
        })
    }

    /// VIP 网卡失去链路时撤销本节点执行准入，由已有 HA 协议选择其他候选。
    pub fn guard_provider(&self, inner: Box<dyn DeviceProvider>) -> Box<dyn DeviceProvider> {
        Box::new(VipProvider {
            config: self.config.clone(),
            inner,
        })
    }

    pub fn start(
        &mut self,
        status: SharedStatus,
        files: Arc<FileService>,
        node: mpsc::Sender<NodeInput>,
    ) -> io::Result<()> {
        let Some(mut net) = self.net.take() else {
            return Err(invalid("VIP 线程已启动"));
        };
        let (stop, rx) = mpsc::channel();
        let config = self.config.clone();
        let worker = thread::Builder::new()
            .name("ltfsd-http-vip".into())
            .spawn(move || {
                let mut session = VipSession::default();
                let mut last_error = None;
                let mut last_proposal: Option<(Command, Instant)> = None;
                let mut waiting = None;
                loop {
                    let snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let owner = ready_owner(&snapshot, &files);
                    let now = Instant::now();
                    match session.step(now, &snapshot, owner, config.address, &mut net) {
                        Ok(command) => {
                            last_error = None;
                            if let Some(cmd) = command
                                && last_proposal.as_ref().is_none_or(|(old, at)| {
                                    *old != cmd || now.duration_since(*at) >= Duration::from_secs(1)
                                })
                            {
                                if node.send(NodeInput::Vip(cmd.clone())).is_err() {
                                    break;
                                }
                                last_proposal = Some((cmd, now));
                            }
                        }
                        Err(e) => {
                            let message = e.to_string();
                            if last_error.as_ref() != Some(&message) {
                                warn!(
                                    "HTTP VIP {} ({}) 未就绪：{}",
                                    config.address, config.interface, e
                                );
                            }
                            last_error = Some(message);
                            // 出错不保留地址；有限寿命仍是异常退出的最后保障。
                            let _ = net.remove();
                            session.lease.held = false;
                            session.lease.announced = None;
                        }
                    }
                    let blocked = snapshot
                        .vips
                        .get(&config.address)
                        .and_then(|r| r.holder.as_ref())
                        .filter(|h| h.node != snapshot.id)
                        .map(|h| (h.node, h.round));
                    if blocked != waiting {
                        if let Some((old, round)) = blocked {
                            info!(
                                "HTTP VIP {} 等待节点 {} 轮次 {} 的撤址确认，超时不会强制接管",
                                config.address, old, round
                            );
                        }
                        waiting = blocked;
                    }
                    if !matches!(rx.recv_timeout(POLL), Err(mpsc::RecvTimeoutError::Timeout)) {
                        break;
                    }
                }
                let mut snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                snapshot.shutting_down = true;
                match session.step(Instant::now(), &snapshot, None, config.address, &mut net) {
                    Ok(Some(cmd)) => {
                        let _ = node.send(NodeInput::Vip(cmd));
                    }
                    Ok(None) => {}
                    Err(e) => warn!("HTTP VIP 收尾未能确认撤址，保留占用记录：{e}"),
                }
            })?;
        self.stop = Some(stop);
        self.worker = Some(worker);
        Ok(())
    }
}

struct VipProvider {
    config: VipConfig,
    inner: Box<dyn DeviceProvider>,
}

impl DeviceProvider for VipProvider {
    fn open_all(&self) -> crate::error::Result<Vec<ManagedDevice>> {
        interface_info(&self.config.interface, self.config.address)?;
        self.inner.open_all()
    }

    fn probe(&self) -> crate::error::Result<usize> {
        interface_info(&self.config.interface, self.config.address)?;
        self.inner.probe()
    }
}

impl Drop for HttpVip {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct LinuxAddress {
    config: VipConfig,
    index: u32,
    label: Vec<u8>,
    mac: [u8; 6],
    arp: OwnedFd,
}

impl LinuxAddress {
    fn new(config: &VipConfig) -> io::Result<Self> {
        let name =
            CString::new(config.interface.as_str()).map_err(|_| invalid("VIP 接口名含 NUL"))?;
        // SAFETY: name 是有效的 NUL 结尾字符串，无输出指针。
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        let mac = interface_info(&config.interface, config.address)?;
        // CAP_NET_ADMIN 在添加地址时需要；尽早拒绝缺少权限的配置。
        let status = std::fs::read_to_string("/proc/self/status")?;
        let caps = status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:\t"))
            .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
            .unwrap_or(0);
        if caps & (1 << 12) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HTTP VIP 需要 CAP_NET_ADMIN",
            ));
        }
        let arp = socket(libc::AF_PACKET, (libc::ETH_P_ARP as u16).to_be() as i32)?;
        let mut result = Self {
            config: config.clone(),
            index,
            label: format!("{}:t\0", config.interface).into_bytes(),
            mac,
            arp,
        };
        result.address_state()?;
        Ok(result)
    }

    /// 内核标签、/32 和有限寿命都匹配才属于本功能。冲突时不接管、不删除。
    fn address_state(&mut self) -> io::Result<bool> {
        let mut found = false;
        for (kind, body) in netlink(22, 0x301, &[libc::AF_INET as u8, 0, 0, 0, 0, 0, 0, 0])? {
            if kind != 20 {
                continue;
            }
            if body.len() < 8 {
                return Err(invalid("截断的 IPv4 地址响应"));
            }
            let attrs = attributes(&body[8..])?;
            if attrs.iter().find(|a| a.0 == 2).map(|a| a.1)
                != Some(self.config.address.octets().as_slice())
            {
                continue;
            }
            let index = u32::from_ne_bytes(body[4..8].try_into().unwrap());
            let label = attrs.iter().find(|a| a.0 == 3).map(|a| a.1);
            let finite = attrs.iter().find(|a| a.0 == 6).is_some_and(|a| {
                a.1.len() >= 8 && u32::from_ne_bytes(a.1[4..8].try_into().unwrap()) <= LIFETIME
            });
            if index != self.index
                || body[1] != 32
                || label != Some(self.label.as_slice())
                || !finite
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "VIP 已是本机其他地址，拒绝修改",
                ));
            }
            found = true;
        }
        Ok(found)
    }

    fn change(&self, kind: u16, flags: u16, lifetime: Option<u32>) -> io::Result<()> {
        let mut body = vec![libc::AF_INET as u8, 32, 0, 0];
        body.extend_from_slice(&self.index.to_ne_bytes());
        attribute(&mut body, 1, &self.config.address.octets());
        attribute(&mut body, 2, &self.config.address.octets());
        attribute(&mut body, 3, &self.label);
        if let Some(lifetime) = lifetime {
            attribute(&mut body, 8, &0x200u32.to_ne_bytes()); // IFA_F_NOPREFIXROUTE
            let mut cache = vec![];
            cache.extend_from_slice(&lifetime.to_ne_bytes());
            cache.extend_from_slice(&lifetime.to_ne_bytes());
            cache.extend_from_slice(&[0; 8]);
            attribute(&mut body, 6, &cache);
        }
        netlink(kind, flags | 5, &body).map(|_| ()) // REQUEST | ACK
    }
}

impl AddressManager for LinuxAddress {
    fn refresh(&mut self, lifetime: u32) -> io::Result<()> {
        let mac = interface_info(&self.config.interface, self.config.address)?;
        if mac != self.mac {
            return Err(invalid("VIP 接口 MAC 已改变，需要重启本节点以重新核对接口"));
        }
        let exists = self.address_state()?;
        self.change(20, if exists { 0x100 } else { 0x600 }, Some(lifetime)) // REPLACE 或 CREATE|EXCL
    }
    fn remove(&mut self) -> io::Result<()> {
        if self.address_state()? {
            match self.change(21, 0, None) {
                Err(e) if e.raw_os_error() == Some(libc::EADDRNOTAVAIL) => {}
                result => result?,
            }
            info!("HTTP VIP {} 已撤下", self.config.address);
        }
        if self.address_state()? {
            return Err(io::Error::other("VIP 删除后回读仍存在，拒绝确认释放"));
        }
        Ok(())
    }
    fn announce(&mut self) -> io::Result<()> {
        let frame = announcement(self.mac, self.config.address);
        // SAFETY: sockaddr_ll 允许零初始化，随后设置 family/index/硬件目标。
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = (libc::ETH_P_ARP as u16).to_be();
        addr.sll_ifindex = self.index as i32;
        addr.sll_halen = 6;
        addr.sll_addr[..6].fill(0xff);
        // SAFETY: frame 与 addr 的有效期覆盖调用；长度为实际缓冲区/结构长度。
        let sent = unsafe {
            libc::sendto(
                self.arp.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                0,
                (&addr as *const libc::sockaddr_ll).cast(),
                std::mem::size_of_val(&addr) as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != frame.len() {
            return Err(io::Error::other("ARP 公告发送不完整"));
        }
        Ok(())
    }
}

fn socket(family: i32, protocol: i32) -> io::Result<OwnedFd> {
    // SAFETY: 无指针参数；成功返回的新 fd 只交给 OwnedFd 持有一次。
    let fd = unsafe { libc::socket(family, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd 已确认有效且尚未由任何 Rust 对象持有。
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 500_000,
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: timeout 指针有效且长度正确，setsockopt 不保留指针。
        if unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&timeout as *const libc::timeval).cast(),
                std::mem::size_of_val(&timeout) as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fd)
}

fn netlink(kind: u16, flags: u16, body: &[u8]) -> io::Result<Vec<(u16, Vec<u8>)>> {
    let fd = socket(libc::AF_NETLINK, libc::NETLINK_ROUTE)?;
    // SAFETY: sockaddr_nl 可零初始化，pid/groups=0 表示内核单播。
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as u16;
    // SAFETY: 地址有效，长度与 sockaddr_nl 匹配。
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&kernel as *const libc::sockaddr_nl).cast(),
            std::mem::size_of_val(&kernel) as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut request = vec![];
    request.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
    request.extend_from_slice(&kind.to_ne_bytes());
    request.extend_from_slice(&flags.to_ne_bytes());
    request.extend_from_slice(&1u32.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(body);
    // SAFETY: request 是有效缓冲区，send 不保留指针。
    if unsafe { libc::send(fd.as_raw_fd(), request.as_ptr().cast(), request.len(), 0) }
        != request.len() as isize
    {
        return Err(io::Error::last_os_error());
    }
    let mut result = vec![];
    loop {
        let mut buffer = [0u8; 32768];
        // SAFETY: buffer 可写且长度准确。MSG_TRUNC 让过长报文可检测而不是静默截断。
        let n = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_TRUNC,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 || n as usize > buffer.len() {
            return Err(invalid("Netlink 响应为空或截断"));
        }
        let mut bytes = &buffer[..n as usize];
        while !bytes.is_empty() {
            if bytes.len() < 16 {
                return Err(invalid("截断的 Netlink 消息头"));
            }
            let len = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
            if len < 16 || len > bytes.len() {
                return Err(invalid("Netlink 消息长度越界"));
            }
            let kind = u16::from_ne_bytes(bytes[4..6].try_into().unwrap());
            let flags = u16::from_ne_bytes(bytes[6..8].try_into().unwrap());
            if bytes[8..12] != 1u32.to_ne_bytes() || flags & 0x10 != 0 {
                return Err(invalid("Netlink 序号不符或地址转储被中断"));
            }
            let body = &bytes[16..len];
            if kind == 2 || kind == 3 {
                if body.len() < 4 {
                    return Err(invalid("截断的 Netlink 结果"));
                }
                let error = i32::from_ne_bytes(body[..4].try_into().unwrap());
                if error < 0 {
                    return Err(io::Error::from_raw_os_error(-error));
                }
                if error != 0 {
                    return Err(invalid("无效的 Netlink 结果"));
                }
                return Ok(result);
            }
            if kind == 4 {
                return Err(invalid("Netlink 响应溢出"));
            }
            result.push((kind, body.to_vec()));
            let next = (len + 3) & !3;
            if next > bytes.len() {
                return Err(invalid("Netlink 对齐长度越界"));
            }
            bytes = &bytes[next..];
        }
    }
}

fn attribute(bytes: &mut Vec<u8>, kind: u16, value: &[u8]) {
    bytes.extend_from_slice(&((4 + value.len()) as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(value);
    bytes.resize((bytes.len() + 3) & !3, 0);
}

fn attributes(mut bytes: &[u8]) -> io::Result<Vec<(u16, &[u8])>> {
    let mut result = vec![];
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(invalid("截断的 Netlink 属性"));
        }
        let len = u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize;
        let next = (len + 3) & !3;
        if len < 4 || next > bytes.len() {
            return Err(invalid("Netlink 属性长度越界"));
        }
        result.push((
            u16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
            &bytes[4..len],
        ));
        bytes = &bytes[next..];
    }
    Ok(result)
}

fn interface_info(name: &str, vip: Ipv4Addr) -> io::Result<[u8; 6]> {
    let mut head = std::ptr::null_mut();
    // SAFETY: head 是有效输出指针；成功后由 Guard 恰好释放一次链表。
    if unsafe { libc::getifaddrs(&mut head) } < 0 {
        return Err(io::Error::last_os_error());
    }
    struct Guard(*mut libc::ifaddrs);
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: 指针来自成功的 getifaddrs，未提前释放。
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let _guard = Guard(head);
    let mut mac = None;
    let mut subnet = false;
    let mut item = head;
    while !item.is_null() {
        // SAFETY: item 是 getifaddrs 链表节点，Guard 在整个遍历中存活。
        let entry = unsafe { &*item };
        item = entry.ifa_next;
        if entry.ifa_name.is_null() || entry.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: 非空 ifa_name 由 getifaddrs 提供，保证 NUL 结尾。
        if unsafe { CStr::from_ptr(entry.ifa_name) }.to_bytes() != name.as_bytes() {
            continue;
        }
        let required = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
        if entry.ifa_flags & required != required {
            return Err(invalid("VIP 接口未启用或链路不可用"));
        }
        // SAFETY: ifa_addr 非空，其 family 字段决定后续结构类型。
        match unsafe { (*entry.ifa_addr).sa_family as i32 } {
            libc::AF_PACKET => {
                // SAFETY: AF_PACKET 地址由内核以 sockaddr_ll 提供。
                let addr = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_ll>() };
                if addr.sll_hatype == 1 && addr.sll_halen == 6 {
                    mac = Some(addr.sll_addr[..6].try_into().unwrap());
                }
            }
            libc::AF_INET if !entry.ifa_netmask.is_null() => {
                // SAFETY: AF_INET 地址及其非空掩码都是 sockaddr_in。
                let addr = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
                let mask = unsafe { &*entry.ifa_netmask.cast::<libc::sockaddr_in>() };
                let ip = u32::from_be(addr.sin_addr.s_addr);
                let mask = u32::from_be(mask.sin_addr.s_addr);
                let vip = u32::from(vip);
                if vip == ip {
                    return Err(invalid("VIP 不能使用接口已有主地址"));
                }
                subnet |= mask != 0
                    && (ip & mask) == (vip & mask)
                    && vip & !mask != 0
                    && vip & !mask != !mask;
            }
            _ => {}
        }
    }
    if !subnet {
        return Err(invalid(
            "VIP 必须位于指定接口已有 IPv4 子网中，且不能是网络/广播地址",
        ));
    }
    mac.ok_or_else(|| invalid("VIP 需要以太网接口"))
}

fn announcement(mac: [u8; 6], address: Ipv4Addr) -> [u8; 60] {
    let mut frame = [0u8; 60];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&mac);
    frame[12..22].copy_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
    frame[22..28].copy_from_slice(&mac);
    frame[28..32].copy_from_slice(&address.octets());
    frame[38..42].copy_from_slice(&address.octets());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryAddress {
        held: bool,
        refreshes: Vec<u32>,
        announcements: usize,
        fail: bool,
        fail_remove: bool,
    }

    impl AddressManager for MemoryAddress {
        fn refresh(&mut self, lifetime: u32) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::other("模拟添加失败"));
            }
            self.held = true;
            self.refreshes.push(lifetime);
            Ok(())
        }
        fn remove(&mut self) -> io::Result<()> {
            if self.fail_remove {
                return Err(io::Error::other("模拟撤址回读失败"));
            }
            self.held = false;
            Ok(())
        }
        fn announce(&mut self) -> io::Result<()> {
            self.announcements += 1;
            Ok(())
        }
    }

    fn test_address() -> Ipv4Addr {
        Ipv4Addr::new(192, 0, 2, 100)
    }

    fn status(
        node: u64,
        current: u64,
        round: u64,
        record: super::super::state::VipRecord,
    ) -> NodeStatus {
        NodeStatus {
            id: node,
            executor: Some(super::super::state::Executor {
                node: current,
                round,
                term: 1,
                fenced: true,
            }),
            executor_ready: true,
            vips: std::collections::BTreeMap::from([(test_address(), record)]),
            ..Default::default()
        }
    }

    fn grant(node: u64, round: u64, token: u64) -> super::super::state::VipRecord {
        super::super::state::VipRecord {
            token,
            holder: Some(super::super::state::VipOwner { node, round }),
        }
    }

    #[test]
    fn reservation_and_recovery_run_in_parallel_and_publish_only_after_both_are_ready() {
        let now = Instant::now();
        let ready = Some((4, now + Duration::from_secs(30)));
        let mut net = MemoryAddress::default();
        let mut session = VipSession::default();
        let mut snapshot = status(1, 1, 4, Default::default());
        assert!(matches!(
            session
                .step(now, &snapshot, None, test_address(), &mut net)
                .unwrap(),
            Some(Command::VipAcquire {
                expected_token: 0,
                ..
            })
        ));
        assert!(!net.held, "申请未提交不能发布");
        snapshot.vips.insert(test_address(), grant(1, 4, 10));
        session
            .step(now, &snapshot, None, test_address(), &mut net)
            .unwrap();
        assert!(!net.held, "已预约但尚未恢复也不能发布");
        session
            .step(now, &snapshot, ready, test_address(), &mut net)
            .unwrap();
        assert!(net.held, "恢复和授权都完成后不附加八秒等待");
        assert_eq!(net.announcements, 1);
        session
            .step(now, &snapshot, None, test_address(), &mut net)
            .unwrap();
        assert!(!net.held, "失去服务资格立即撤址");
        session
            .step(now, &snapshot, ready, test_address(), &mut net)
            .unwrap();
        assert!(net.held);
        assert_eq!(net.announcements, 2);
    }

    #[test]
    fn old_withdrawal_ack_is_required_even_after_unbounded_wait_and_new_owner_is_ready() {
        let now = Instant::now();
        let ready = Some((5, now + Duration::from_secs(60)));
        let snapshot = status(2, 2, 5, grant(1, 4, 10));
        let mut net = MemoryAddress::default();
        let mut session = VipSession::default();
        for elapsed in [Duration::ZERO, Duration::from_secs(30)] {
            assert!(
                session
                    .step(now + elapsed, &snapshot, ready, test_address(), &mut net)
                    .unwrap()
                    .is_none()
            );
            assert!(!net.held, "ready/时间经过都不能代替旧节点撤址确认");
        }
    }

    #[test]
    fn failed_removal_never_acknowledges_and_a_released_token_is_never_reused() {
        let now = Instant::now();
        let mut net = MemoryAddress {
            held: true,
            fail_remove: true,
            ..Default::default()
        };
        let mut session = VipSession::default();
        let mut snapshot = status(1, 2, 5, grant(1, 4, 10));
        assert!(
            session
                .step(now, &snapshot, None, test_address(), &mut net)
                .is_err()
        );
        assert!(net.held);
        assert_eq!(session.released_token, 0);
        net.fail_remove = false;
        assert!(matches!(
            session
                .step(now, &snapshot, None, test_address(), &mut net)
                .unwrap(),
            Some(Command::VipRelease { token: 10, .. })
        ));
        assert!(!net.held);
        snapshot.executor.as_mut().unwrap().node = 1;
        snapshot.executor.as_mut().unwrap().round = 4;
        assert!(matches!(
            session
                .step(
                    now,
                    &snapshot,
                    Some((4, now + Duration::from_secs(30))),
                    test_address(),
                    &mut net
                )
                .unwrap(),
            Some(Command::VipRelease { token: 10, .. })
        ));
        assert!(
            !net.held,
            "确认释放后的旧快照/迟到许可不能重新添加旧授权地址"
        );
    }

    #[test]
    fn a_failed_add_seals_the_token_and_requires_a_new_committed_reservation() {
        let now = Instant::now();
        let mut net = MemoryAddress {
            fail: true,
            ..Default::default()
        };
        let mut session = VipSession::default();
        let snapshot = status(1, 1, 4, grant(1, 4, 10));
        let owner = Some((4, now + Duration::from_secs(30)));
        assert!(
            session
                .step(now, &snapshot, owner, test_address(), &mut net)
                .is_err()
        );
        assert_eq!(net.announcements, 0);
        net.fail = false;
        assert!(matches!(
            session
                .step(now, &snapshot, owner, test_address(), &mut net)
                .unwrap(),
            Some(Command::VipRelease { token: 10, .. })
        ));
        assert!(!net.held);
    }

    #[test]
    fn two_nodes_never_publish_together_during_a_handover_with_failed_old_cleanup() {
        use super::super::state::ControlState;
        let now = Instant::now();
        let mut state = ControlState::default();
        state.apply(1, &Command::Takeover { node: 1, term: 1 });
        state.apply(
            2,
            &Command::VipAcquire {
                address: test_address(),
                node: 1,
                round: 1,
                expected_token: 0,
            },
        );
        let mut old = VipSession::default();
        let mut new = VipSession::default();
        let mut old_net = MemoryAddress::default();
        let mut new_net = MemoryAddress::default();
        old.step(
            now,
            &status(1, 1, 1, state.vips[&test_address()].clone()),
            Some((1, now + Duration::from_secs(30))),
            test_address(),
            &mut old_net,
        )
        .unwrap();
        state.apply(3, &Command::Takeover { node: 2, term: 2 });
        old_net.fail_remove = true;
        assert!(
            old.step(
                now,
                &status(1, 2, 3, state.vips[&test_address()].clone()),
                None,
                test_address(),
                &mut old_net
            )
            .is_err()
        );
        new.step(
            now,
            &status(2, 2, 3, state.vips[&test_address()].clone()),
            Some((3, now + Duration::from_secs(30))),
            test_address(),
            &mut new_net,
        )
        .unwrap();
        assert!(old_net.held && !new_net.held);
        old_net.fail_remove = false;
        let ack = old
            .step(
                now,
                &status(1, 2, 3, state.vips[&test_address()].clone()),
                None,
                test_address(),
                &mut old_net,
            )
            .unwrap()
            .unwrap();
        assert!(!old_net.held, "先完成实际删除，再发送确认");
        state.apply(4, &ack);
        let request = new
            .step(
                now,
                &status(2, 2, 3, state.vips[&test_address()].clone()),
                Some((3, now + Duration::from_secs(30))),
                test_address(),
                &mut new_net,
            )
            .unwrap()
            .unwrap();
        assert!(!new_net.held, "释放已确认仍须等新授权提交");
        state.apply(5, &request);
        new.step(
            now,
            &status(2, 2, 3, state.vips[&test_address()].clone()),
            Some((3, now + Duration::from_secs(30))),
            test_address(),
            &mut new_net,
        )
        .unwrap();
        assert!(!old_net.held && new_net.held);
    }

    #[test]
    fn kernel_lifetime_never_consumes_the_permission_deadline_margin() {
        let now = Instant::now();
        let mut net = MemoryAddress::default();
        let mut lease = Lease::default();
        lease
            .step(now, Some((4, now + Duration::from_millis(2500))), &mut net)
            .unwrap();
        assert_eq!(net.refreshes, vec![1]);
        lease
            .step(now, Some((4, now + Duration::from_millis(1900))), &mut net)
            .unwrap();
        assert!(!net.held);
    }

    #[test]
    fn vip_configuration_requires_an_explicit_usable_ipv4_listener_and_interface() {
        let mut config = VipConfig {
            address: "192.0.2.100".parse().unwrap(),
            interface: "eth0".into(),
        };
        let permit = Duration::from_secs(6);
        assert!(config.validate("0.0.0.0:7401", permit).is_ok());
        for addr in ["192.0.2.1:7401", "[::]:7401", "0.0.0.0:0", "localhost:7401"] {
            assert!(config.validate(addr, permit).is_err(), "{addr}");
        }
        assert!(
            config
                .validate("0.0.0.0:7401", Duration::from_millis(3999))
                .is_err()
        );
        for addr in [
            "0.0.0.0",
            "127.0.0.2",
            "224.1.2.3",
            "255.255.255.255",
            "169.254.1.1",
        ] {
            config.address = addr.parse().unwrap();
            assert!(config.validate("0.0.0.0:7401", permit).is_err(), "{addr}");
        }
        config.address = "192.0.2.100".parse().unwrap();
        for iface in ["", "abcdefghijklmn", "eth0:t", "../eth0", "eth\0"] {
            config.interface = iface.into();
            assert!(config.validate("0.0.0.0:7401", permit).is_err(), "{iface}");
        }
    }

    #[test]
    fn netlink_attributes_preserve_alignment_and_reject_truncation() {
        let mut bytes = vec![];
        attribute(&mut bytes, 3, b"eth0:t\0");
        attribute(&mut bytes, 2, &[192, 0, 2, 100]);
        assert_eq!(
            attributes(&bytes).unwrap(),
            vec![(3, b"eth0:t\0".as_slice()), (2, &[192, 0, 2, 100])]
        );
        for bad in [
            &bytes[..1],
            &bytes[..3],
            &bytes[..10],
            &bytes[..bytes.len() - 1],
            &[3, 0, 1, 0],
            &[255, 255, 0, 0],
        ] {
            assert!(attributes(bad).is_err());
        }
    }

    #[test]
    fn gratuitous_arp_is_a_broadcast_request_with_identical_sender_and_target_ip() {
        let mac = [2, 0, 0, 0, 0, 1];
        let frame = announcement(mac, Ipv4Addr::new(192, 0, 2, 100));
        assert_eq!(&frame[..6], &[255; 6]);
        assert_eq!(&frame[6..12], &mac);
        assert_eq!(&frame[12..22], &[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        assert_eq!(&frame[22..28], &mac);
        assert_eq!(&frame[28..32], &[192, 0, 2, 100]);
        assert_eq!(&frame[32..38], &[0; 6]);
        assert_eq!(&frame[38..42], &frame[28..32]);
        assert_eq!(&frame[42..], &[0; 18]);
    }

    fn isolated_config() -> VipConfig {
        assert_eq!(
            std::env::var("TAPE_RS_VIP_NETNS_TEST").as_deref(),
            Ok("1"),
            "仅允许显式隔离网络测试"
        );
        use std::os::unix::fs::MetadataExt;
        let host = std::fs::metadata("/proc/1/ns/net")
            .ok()
            .map(|m| m.ino())
            .or_else(|| std::env::var("TAPE_RS_VIP_HOST_NETNS").ok()?.parse().ok())
            .expect("无法读取 PID 1 命名空间，需在 unshare 前记录 TAPE_RS_VIP_HOST_NETNS 的 inode");
        assert_ne!(
            std::fs::metadata("/proc/self/ns/net").unwrap().ino(),
            host,
            "禁止在宿主网络运行"
        );
        VipConfig {
            address: Ipv4Addr::new(192, 0, 2, 100),
            interface: "viptest0".into(),
        }
    }

    #[test]
    #[ignore = "需独立网络命名空间、viptest0=192.0.2.1/24、CAP_NET_ADMIN/RAW 和 TAPE_RS_VIP_NETNS_TEST=1"]
    fn kernel_address_lifecycle() {
        let config = isolated_config();
        config
            .validate("0.0.0.0:7401", Duration::from_secs(6))
            .unwrap();
        let mut net = LinuxAddress::new(&config).unwrap();
        assert!(!net.address_state().unwrap());
        net.refresh(2).unwrap();
        assert!(net.address_state().unwrap());
        net.announce().unwrap();
        net.refresh(1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while net.address_state().unwrap() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert!(!net.address_state().unwrap(), "不再刷新时内核必须回收地址");
        net.refresh(2).unwrap();
        net.remove().unwrap();
        assert!(!net.address_state().unwrap());
        // 同一地址但不同标签属于外部配置，不能覆盖或删除。
        let own_label = net.label.clone();
        net.label = b"viptest0:other\0".to_vec();
        net.change(20, 0x600, Some(2)).unwrap();
        net.label = own_label.clone();
        assert!(net.refresh(2).is_err());
        assert!(net.remove().is_err());
        net.label = b"viptest0:other\0".to_vec();
        assert!(net.change(21, 0, None).is_ok());
        net.label = own_label;
        assert!(!net.address_state().unwrap());
        struct Provider;
        impl DeviceProvider for Provider {
            fn open_all(&self) -> crate::error::Result<Vec<ManagedDevice>> {
                Ok(vec![])
            }
            fn probe(&self) -> crate::error::Result<usize> {
                Ok(1)
            }
        }
        let provider = VipProvider {
            config: config.clone(),
            inner: Box::new(Provider),
        };
        assert_eq!(provider.probe().unwrap(), 1);
        let mut link = vec![0, 0, 0, 0];
        link.extend_from_slice(&net.index.to_ne_bytes());
        link.extend_from_slice(&0u32.to_ne_bytes());
        link.extend_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
        netlink(16, 5, &link).unwrap(); // RTM_NEWLINK：只清除 IFF_UP
        assert!(provider.probe().is_err(), "VIP 链路故障也必须撤销执行准入");
        assert!(provider.open_all().is_err());
        assert!(net.refresh(2).is_err());
        assert!(!net.address_state().unwrap());
        link[8..12].copy_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
        netlink(16, 5, &link).unwrap();
        assert_eq!(provider.probe().unwrap(), 1);
    }

    #[test]
    #[ignore = "仅在显式隔离网络内运行；验证真实 VIP、HTTP、ARP、SIGSTOP/SIGCONT/SIGKILL"]
    fn kernel_process_expiry() {
        use super::super::node::NodeStatus;
        use super::super::permission::{Permission, Phase};
        use super::super::state::Executor;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::process::{Command, Stdio};
        use std::sync::Mutex;
        let config = isolated_config();
        let child_dir = std::env::var("TAPE_RS_VIP_CHILD_DIR");
        if let Ok(dir) = child_dir {
            let files = FileService::new(dir.into()).unwrap();
            let permission = Arc::new(Permission::default());
            files.set_permission(permission.clone());
            let status = Arc::new(Mutex::new(NodeStatus {
                id: 1,
                leader: 3,
                role: "Follower".into(),
                executor_ready: false,
                executor: Some(Executor {
                    node: 1,
                    round: 4,
                    term: 1,
                    fenced: true,
                }),
                ..Default::default()
            }));
            let (exec, _exec_rx) = mpsc::channel();
            let (node, node_rx) = mpsc::channel();
            let status_for_control = status.clone();
            // 此处只模拟控制日志提交；真实 Raft 转交与持久化由 ha_cluster/vip_ownership 测试覆盖。
            thread::spawn(move || {
                let mut control = super::super::state::ControlState::default();
                let mut index = 100;
                for input in node_rx {
                    if let NodeInput::Vip(cmd) = input {
                        let mut snapshot = status_for_control.lock().unwrap();
                        control.executor = snapshot.executor.clone();
                        control.apply(index, &cmd);
                        index += 1;
                        snapshot.vips = control.vips.clone();
                    }
                }
            });
            super::super::http::serve(
                "0.0.0.0:27401",
                Arc::new(super::super::http::HttpContext {
                    files: files.clone(),
                    status: status.clone(),
                    exec: Mutex::new(exec),
                    node: Mutex::new(node.clone()),
                    client_addrs: Default::default(),
                    wait_timeout: Duration::from_secs(1),
                }),
            )
            .unwrap();
            let for_shutdown = status.clone();
            thread::spawn(move || {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).unwrap();
                if line.trim() == "shutdown" {
                    for_shutdown.lock().unwrap().shutting_down = true;
                }
            });
            let mut vip = HttpVip::prepare(config.clone(), Duration::from_secs(6)).unwrap();
            let assigned_at = Instant::now();
            vip.start(status.clone(), files, node).unwrap();
            let mut observer = LinuxAddress::new(&config).unwrap();
            let mut announced = false;
            let recovery = Duration::from_millis(
                std::env::var("TAPE_RS_VIP_RECOVERY_MS")
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
            let mut ready = false;
            loop {
                permission.renew(4, Phase::Serving, Instant::now() + Duration::from_secs(6));
                if !ready && assigned_at.elapsed() >= recovery {
                    status.lock().unwrap().executor_ready = true;
                    println!("VIP_OWNER_READY {}", assigned_at.elapsed().as_millis());
                    std::io::stdout().flush().unwrap();
                    ready = true;
                }
                if !announced && observer.address_state().unwrap() {
                    println!("VIP_CHILD_READY {}", assigned_at.elapsed().as_millis());
                    std::io::stdout().flush().unwrap();
                    announced = true;
                }
                thread::sleep(POLL);
            }
        }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let dir = std::env::temp_dir().join(format!("vip-process-{}", uuid::Uuid::new_v4()));
        let mut observer = LinuxAddress::new(&config).unwrap();
        // 绑定独立的 veth 对端，必须实际收到网络帧，而不只是验证编码器。
        let receiver = socket(libc::AF_PACKET, (libc::ETH_P_ARP as u16).to_be() as i32).unwrap();
        // SAFETY: sockaddr_ll 可零初始化，接口名为固定 NUL 结尾字面量。
        let mut peer: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        peer.sll_family = libc::AF_PACKET as u16;
        peer.sll_protocol = (libc::ETH_P_ARP as u16).to_be();
        peer.sll_ifindex = unsafe { libc::if_nametoindex(c"vippeer0".as_ptr()) } as i32;
        assert!(peer.sll_ifindex > 0);
        // SAFETY: peer 与长度匹配，指针有效。
        assert_eq!(
            unsafe {
                libc::bind(
                    receiver.as_raw_fd(),
                    (&peer as *const libc::sockaddr_ll).cast(),
                    std::mem::size_of_val(&peer) as libc::socklen_t,
                )
            },
            0
        );
        for scenario in ["pause", "shutdown", "kill"] {
            let mut child = Child(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--ignored",
                        "--exact",
                        "daemon::vip::tests::kernel_process_expiry",
                        "--nocapture",
                    ])
                    .env("TAPE_RS_VIP_CHILD_DIR", &dir)
                    .env(
                        "TAPE_RS_VIP_RECOVERY_MS",
                        if scenario == "pause" { "9000" } else { "1000" },
                    )
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let (ready_tx, ready_rx) = mpsc::channel();
            let output = thread::spawn(move || {
                let mut owner_ready_at = None;
                for line in BufReader::new(stdout).lines() {
                    let line = line.unwrap();
                    if let Some((_, at)) = line.split_once("VIP_OWNER_READY ") {
                        owner_ready_at = Some(at.parse::<u128>().unwrap());
                    }
                    if let Some((_, at)) = line.split_once("VIP_CHILD_READY ") {
                        let _ =
                            ready_tx.send((owner_ready_at.unwrap(), at.parse::<u128>().unwrap()));
                    }
                }
            });
            let (ready_at, published_at) = ready_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            assert!(published_at >= ready_at);
            assert!(
                published_at - ready_at < 2500,
                "已提交的独占授权与恢复都就绪后不附加等待"
            );
            println!("{scenario}: owner ready {ready_at}ms, VIP published {published_at}ms");
            assert!(observer.address_state().unwrap());
            let mut request = std::net::TcpStream::connect("192.0.2.100:27401").unwrap();
            request
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            request
                .write_all(b"GET /readyz HTTP/1.1\r\nHost: vip\r\n\r\n")
                .unwrap();
            let mut reply = String::new();
            request.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            if scenario == "pause" {
                thread::sleep(Duration::from_secs(3));
                assert!(
                    observer.address_state().unwrap(),
                    "健康 owner 必须持续刷新两秒寿命的 VIP"
                );
                let mut frame = [0u8; 2048];
                // SAFETY: frame 可写且长度准确；接收超时已由 socket 设置。
                let n = unsafe {
                    libc::recv(
                        receiver.as_raw_fd(),
                        frame.as_mut_ptr().cast(),
                        frame.len(),
                        0,
                    )
                };
                assert!(n >= 42, "对端必须实际收到 ARP 公告");
                assert_eq!(&frame[28..32], &config.address.octets());
                assert_eq!(&frame[38..42], &config.address.octets());
                // SAFETY: PID 来自仍被 Child 持有的直接子进程。
                assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGSTOP) }, 0);
            } else if scenario == "shutdown" {
                child
                    .0
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"shutdown\n")
                    .unwrap();
            } else {
                child.0.kill().unwrap();
                child.0.wait().unwrap();
            }
            let withdrawal_started = Instant::now();
            let deadline = Instant::now() + Duration::from_secs(4);
            while observer.address_state().unwrap() && Instant::now() < deadline {
                thread::sleep(POLL);
            }
            assert!(
                !observer.address_state().unwrap(),
                "{scenario}: VIP 必须撤下或到期"
            );
            println!(
                "{scenario}: previous VIP absent after {}ms",
                withdrawal_started.elapsed().as_millis()
            );
            if scenario == "pause" {
                // 超过旧许可的六秒后恢复，旧进程不能以迟到续期重新抢回 VIP。
                thread::sleep(Duration::from_secs(7));
                // SAFETY: 同上；Child 在此之前不会释放 PID。
                assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGCONT) }, 0);
                thread::sleep(Duration::from_secs(1));
                assert!(!observer.address_state().unwrap());
            }
            drop(child);
            output.join().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
