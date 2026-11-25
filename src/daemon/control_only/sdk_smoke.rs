//! 只读 SDK 输入冒烟；须由主会话移交私库窗口后，在独立进程显式执行。
use super::*;
use crate::managed_catalog::ManagedCatalog;
use serde_json::{Value, json};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Mutex;

const TEST: &str = "daemon::control_only::sdk_smoke::control_only_pinned_sdk_readonly_smoke";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateInstance {
    version: u32,
    window_id: String,
    endpoint: String,
    cluster_file: PathBuf,
    cluster_contents: String,
    server_pid: u32,
    server_start_ticks: u64,
    boot_id: String,
    server_executable: PathBuf,
    server_sha256: String,
    server_argv: Vec<String>,
    listener_inode: u64,
    client_library: PathBuf,
    client_sha256: String,
}
impl PrivateInstance {
    fn verify(&self) -> Result<Value> {
        if self.version != 1
            || self.window_id.is_empty()
            || self.endpoint != "127.0.0.1:44247"
            || self.server_pid < 2
            || self.server_pid == std::process::id()
            || !self.cluster_file.is_absolute()
            || !self.server_executable.is_absolute()
            || !self.client_library.is_absolute()
            || self.listener_inode == 0
            || !sha(&self.server_sha256)
            || !sha(&self.client_sha256)
        {
            return Err(invalid("SDK smoke 私有实例 guard 无效"));
        }
        let lines: Vec<_> = self
            .cluster_contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        if lines.len() != 1
            || lines[0].split_once('@').map(|(_, endpoints)| endpoints)
                != Some(self.endpoint.as_str())
        {
            return Err(invalid("SDK smoke 仅允许单一 127.0.0.1:44247 coordinator"));
        }
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        if boot.trim() != self.boot_id {
            return Err(invalid("SDK smoke boot identity 漂移"));
        }
        let process = PathBuf::from(format!("/proc/{}", self.server_pid));
        let stat = std::fs::read_to_string(process.join("stat"))?;
        let tail = stat
            .rsplit_once(") ")
            .ok_or_else(|| invalid("SDK smoke proc stat 无效"))?
            .1;
        if tail
            .split_whitespace()
            .nth(19)
            .and_then(|n| n.parse::<u64>().ok())
            != Some(self.server_start_ticks)
        {
            return Err(invalid("SDK smoke PID/start identity 漂移"));
        }
        let network_namespace = std::fs::read_link(process.join("ns/net"))?;
        if network_namespace != std::fs::read_link("/proc/self/ns/net")? {
            return Err(invalid("SDK smoke server/client 不在同一网络命名空间"));
        }
        let argv = std::fs::read(process.join("cmdline"))?;
        let expected_argv: Vec<u8> = self
            .server_argv
            .iter()
            .flat_map(|arg| arg.as_bytes().iter().copied().chain([0]))
            .collect();
        if argv != expected_argv
            || std::fs::read_link(process.join("exe"))? != self.server_executable
            || digest(&std::fs::read(process.join("exe"))?) != self.server_sha256
        {
            return Err(invalid("SDK smoke server argv/ELF 漂移"));
        }
        let socket = format!("socket:[{}]", self.listener_inode);
        let owned = std::fs::read_dir(process.join("fd"))?
            .filter_map(|entry| entry.ok())
            .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|p| p == Path::new(&socket)));
        let tcp = std::fs::read_to_string(process.join("net/tcp"))?;
        let listener = tcp.lines().find(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() > 9
                && fields[3] == "0A"
                && fields[9].parse::<u64>().ok() == Some(self.listener_inode)
                && matches!(fields[1], "0100007F:ACD7" | "00000000:ACD7")
        });
        if !owned || listener.is_none() {
            return Err(invalid("SDK smoke 私有 listener identity 漂移"));
        }
        let cluster = FileSeal::read(&self.cluster_file)?;
        if std::fs::read(&self.cluster_file)? != self.cluster_contents.as_bytes()
            || cluster.sha256 != digest(self.cluster_contents.as_bytes())
        {
            return Err(invalid("SDK smoke 原 cluster exact bytes 漂移"));
        }
        let maps = std::fs::read_to_string("/proc/self/maps")?;
        let client: Vec<_> = maps
            .lines()
            .filter(|line| line.contains("libfdb_c.so"))
            .collect();
        if client.is_empty()
            || client
                .iter()
                .any(|line| line.split_whitespace().last() != self.client_library.to_str())
            || FileSeal::read(&self.client_library)?.sha256 != self.client_sha256
        {
            return Err(invalid("SDK smoke client maps/ELF 不符"));
        }
        Ok(
            json!({"window_id":self.window_id,"endpoint":self.endpoint,"server_pid":self.server_pid,
            "server_start_ticks":self.server_start_ticks,"boot_id":self.boot_id,"server_executable":self.server_executable,
            "server_sha256":self.server_sha256,"server_argv":self.server_argv,"listener":listener,
            "cluster_source":self.cluster_file,"cluster_source_seal":cluster,"cluster_bytes":self.cluster_contents.len(),
            "network_namespace":network_namespace,
            "client_maps":client,"client_sha256":self.client_sha256}),
        )
    }
}
struct Evidence(Mutex<std::fs::File>);
impl Evidence {
    fn record(&self, phase: &str, value: Value) -> Result<()> {
        let line = json!({"phase":phase,"value":value}).to_string();
        eprintln!("CONTROL_PINNED_SDK_SMOKE {line}");
        writeln!(
            self.0
                .lock()
                .map_err(|_| invalid("SDK smoke evidence lock poisoned"))?,
            "{line}"
        )?;
        Ok(())
    }
}
struct OwnedSwap {
    path: PathBuf,
    saved: PathBuf,
    restored: bool,
}
impl OwnedSwap {
    fn replace(path: &Path) -> Result<Self> {
        let saved = path.with_extension(format!("original-{}", uuid::Uuid::new_v4()));
        std::fs::rename(path, &saved)?;
        let mut swap = Self {
            path: path.to_owned(),
            saved,
            restored: false,
        };
        let create = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(b"not-a-foundationdb-cluster-file\n")?;
            Ok(())
        })();
        if let Err(error) = create {
            swap.restore()?;
            return Err(error);
        }
        Ok(swap)
    }
    fn restore(&mut self) -> Result<()> {
        if !self.restored {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            std::fs::rename(&self.saved, &self.path)?;
            self.restored = true;
        }
        Ok(())
    }
}
impl Drop for OwnedSwap {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

#[test]
#[ignore = "主会话移交 127.0.0.1:44247 只读窗口后，按显式 guard 在独立进程执行"]
fn control_only_pinned_sdk_readonly_smoke() {
    smoke().expect("SDK pinned connection smoke 失败；停止，不重试或改库");
}
fn smoke() -> Result<()> {
    if std::env::var("TAPE_RS_CONTROL_PINNED_SDK_SMOKE").as_deref()
        != Ok("read-only-127.0.0.1:44247")
        || std::env::var("RUST_TEST_THREADS").as_deref() != Ok("1")
        || std::env::var_os("RUST_MIN_STACK").is_some()
    {
        return Err(invalid(
            "SDK smoke 缺少明确只读窗口/单线程环境，或改变了默认栈",
        ));
    }
    let args: Vec<_> = std::env::args().collect();
    if !args.iter().any(|arg| arg == TEST)
        || !args.iter().any(|arg| arg == "--exact")
        || !args.iter().any(|arg| arg == "--ignored")
    {
        return Err(invalid(
            "SDK smoke 须在独立进程用完整名称 --exact --ignored 执行",
        ));
    }
    let guard_path = PathBuf::from(
        std::env::var_os("TAPE_RS_CONTROL_PINNED_SDK_GUARD")
            .ok_or_else(|| invalid("缺少私有实例 guard"))?,
    );
    let guard: PrivateInstance =
        serde_json::from_slice(&std::fs::read(guard_path)?).map_err(|e| invalid(e.to_string()))?;
    let before = guard.verify()?;
    let evidence_root = PathBuf::from(
        std::env::var_os("TAPE_RS_CONTROL_PINNED_SDK_EVIDENCE")
            .ok_or_else(|| invalid("缺少独立 evidence 根目录"))?,
    );
    if !evidence_root.is_absolute() || std::fs::canonicalize(&evidence_root)? != evidence_root {
        return Err(invalid("evidence 根目录须为现有绝对规范路径"));
    }
    let owned = evidence_root.join(format!("owned-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&owned)?;
    let evidence = Evidence(Mutex::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(owned.join("events.jsonl"))?,
    ));
    evidence.record("guard_before", before)?;
    let source = FileSeal::read(&guard.cluster_file)?;
    let stamp = |m: std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    let source_stamp = stamp(std::fs::metadata(&guard.cluster_file)?);
    let bytes = std::fs::read(&guard.cluster_file)?;
    if bytes != guard.cluster_contents.as_bytes() {
        return Err(invalid("复制 cluster 前来源漂移"));
    }
    let path = owned.join("cluster-copy.cluster");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    drop(file);
    let pin = Arc::new(PinnedCluster::new(&path, &digest(&bytes))?);
    let pinned_fd = pin.sdk_path();
    let pin_lifetime = Arc::downgrade(&pin);
    let serial = format!("PINSDK-{}", uuid::Uuid::new_v4().simple());
    let result = (|| -> Result<()> {
        guard.verify()?;
        // SAFETY: argv/env guard 要求本测试独占新进程；仅此处启动 SDK，所有 catalog 在 network 之前析构。
        let _network = unsafe { foundationdb::boot() };
        let connection = open_catalog_from_pin(pin.clone(), |input| {
            let mut swapped = OwnedSwap::replace(&path)?;
            pin.verify_sealed()?;
            // SAFETY: pin.file 在 closure 期间持有有效 fd；F_GET_SEALS 不接收用户指针。
            let flags = unsafe { nix::libc::fcntl(pin.file.as_raw_fd(), nix::libc::F_GET_SEALS) };
            if input != pinned_fd.as_path()
                || std::fs::read(input)? != bytes
                || flags < 0
                || flags & cluster_seals() != cluster_seals()
            {
                return Err(invalid("SDK 实际输入不是已封存的原 cluster bytes"));
            }
            evidence.record("real_sdk_open",json!({"actual_sdk_arg":input,"fd":pin.file.as_raw_fd(),"seals":flags,
                "sha256":digest(&std::fs::read(input)?),"bytes":bytes.len(),"owned_source_path":path,
                "owned_source_replaced_with_invalid_bytes":true}))?;
            let opened = ManagedCatalog::open(Some(
                input
                    .to_str()
                    .ok_or_else(|| invalid("SDK fd path 非 UTF-8"))?,
            ));
            swapped.restore()?;
            opened.map_err(|e| invalid(e.to_string()))
        })?;
        let reader = Arc::new(FdbAuthorityReader { connection });
        drop(pin);
        if pin_lifetime.upgrade().is_none() {
            return Err(invalid("catalog 生命周期没有保持 pinned fd"));
        }
        let catalog_identity = Arc::as_ptr(&reader.connection.catalog) as usize;
        evidence.record("readonly_point_get_begin",json!({"api":"ManagedCatalog::active_library_route","serial":serial,"catalog_identity":catalog_identity}))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let route = runtime
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    reader.connection.catalog.active_library_route(&serial),
                )
                .await
            })
            .map_err(|_| invalid("只读 route point get 超时；停止"))?
            .map_err(|e| invalid(e.to_string()))?;
        evidence.record("readonly_point_get_end",json!({"serial":serial,"result":if route.is_none(){"None"}else{"Some"},"catalog_identity":catalog_identity}))?;
        if route.is_some() {
            return Err(invalid(
                "随机 serial 已有 route；停止，不创建 namespace 或换新 serial",
            ));
        }
        let mut swapped = OwnedSwap::replace(&path)?;
        if reader.connection.cluster.verify_source().is_ok() || reader.active_route(&serial).is_ok()
        {
            return Err(invalid("authority reader 未拒绝 source 漂移"));
        }
        evidence.record(
            "authority_source_drift_rejected",
            json!({"serial":serial,"catalog_identity":catalog_identity}),
        )?;
        swapped.restore()?;
        let (reply, received) = std::sync::mpsc::channel();
        let same = reader.clone();
        let same_serial = serial.clone();
        let worker = std::thread::spawn(move || {
            let _ = reply.send(same.active_route(&same_serial));
        });
        let observed = received
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| invalid("authority reader 超时；停止"))??;
        worker
            .join()
            .map_err(|_| invalid("authority reader 线程失败"))?;
        let expected = ExpectedRoute {
            namespace: "00000000-0000-0000-0000-000000000001".into(),
            revision: 1,
            frozen_index: 1,
            certificate_sha256: "01".repeat(32),
            initial_owner_epoch: 1,
        };
        if observed.is_some() {
            return Err(invalid("缺失 route 被误认为 ExistingRoute 授权"));
        }
        let existing_route_authorized = observed.as_ref() == Some(&expected);
        let catalog_after = Arc::as_ptr(&reader.connection.catalog) as usize;
        if existing_route_authorized || catalog_after != catalog_identity {
            return Err(invalid("缺失 route 授权或 catalog 生命周期不符"));
        }
        evidence.record("authority_restored_same_catalog_none",json!({"serial":serial,"catalog_identity":catalog_identity,
            "route":"None","existing_route_authorized":existing_route_authorized,"fd_alive":pin_lifetime.upgrade().is_some(),"actual_sdk_arg":pinned_fd}))?;
        guard.verify()?;
        Ok(())
    })();
    let after = guard.verify();
    evidence.record(
        "guard_after",
        match &after {
            Ok(value) => value.clone(),
            Err(error) => json!({"error":error.to_string()}),
        },
    )?;
    if FileSeal::read(&guard.cluster_file)? != source
        || stamp(std::fs::metadata(&guard.cluster_file)?) != source_stamp
    {
        return Err(invalid("真实原 cluster 文件发生变化"));
    }
    evidence.record("result",json!({"success":result.is_ok()&&after.is_ok(),"error":result.as_ref().err().map(ToString::to_string),
        "proof":"SDK pinned connection/readonly missing-route only; not ExistingRoute authority success","owned_evidence":owned}))?;
    after?;
    result
}
