//! Private-FDB + Sim production-entry group-count regression. No device path.
use super::*;
use std::os::fd::AsRawFd;
use tape_rs::ltfs::index::{FileNode, LtfsIndex};
use tape_rs::managed_catalog::ManagementRequest;

const APPROVED_PRIVATE_CLUSTER: &[u8] =
    b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\n";

struct PrivateCluster {
    path: String,
    _sealed: fs::File,
}

impl PrivateCluster {
    fn seal(bytes: &[u8]) -> std::io::Result<Self> {
        if !private_cluster_bytes_valid(bytes) {
            return Err(std::io::Error::other(
                "只允许原任务完整固定private FDB cluster bytes",
            ));
        }
        let name = std::ffi::CString::new("reclaim-benchmark-private-cluster").unwrap();
        let fd = nix::sys::memfd::memfd_create(
            name.as_c_str(),
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC
                | nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
        )
        .map_err(std::io::Error::other)?;
        let mut file = fs::File::from(fd);
        file.write_all(bytes)?;
        use nix::fcntl::{FcntlArg, SealFlag, fcntl};
        fcntl(
            file.as_raw_fd(),
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_WRITE
                    | SealFlag::F_SEAL_GROW
                    | SealFlag::F_SEAL_SHRINK
                    | SealFlag::F_SEAL_SEAL,
            ),
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            path: format!("/proc/self/fd/{}", file.as_raw_fd()),
            _sealed: file,
        })
    }

    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(path)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() != APPROVED_PRIVATE_CLUSTER.len() as u64 {
            return Err(std::io::Error::other("private cluster文件类型/长度不符"));
        }
        let mut bytes = Vec::new();
        std::io::Read::take(&mut file, 4096).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
        ) {
            return Err(std::io::Error::other("读取private cluster期间文件改变"));
        }
        Self::seal(&bytes)
    }
}

fn catalog_key(kind: u8, suffix: &[u8]) -> Vec<u8> {
    let mut key = b"tape-rs/catalog/22/".to_vec();
    key.extend_from_slice(&[kind, b'/']);
    key.extend_from_slice(suffix);
    key
}

fn managed_key(namespace: Uuid, domain: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut key = catalog_key(b'@', namespace.as_bytes());
    key.extend_from_slice(domain);
    key.extend_from_slice(suffix);
    key
}

fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while end.last() == Some(&255) {
        end.pop();
    }
    *end.last_mut().expect("nonempty catalog prefix") += 1;
    end
}

async fn raw_page(
    tr: &foundationdb::Transaction,
    prefix: &[u8],
    limit: usize,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let range = foundationdb::RangeOption {
        begin: foundationdb::KeySelector::first_greater_or_equal(prefix.to_vec()),
        end: foundationdb::KeySelector::first_greater_or_equal(prefix_successor(prefix)),
        limit: Some(limit),
        ..Default::default()
    };
    let rows = tr.get_range(&range, 1, false).await.unwrap();
    assert!(
        !(rows.is_empty() && rows.more()),
        "不完整的空分页不能证明无引用"
    );
    rows.iter()
        .map(|row| (row.key().to_vec(), row.value().to_vec()))
        .collect()
}

#[derive(Clone)]
struct FormatProbe {
    cluster: String,
    namespace: Uuid,
    operation: Uuid,
    epoch: u64,
    source: Uuid,
    source_uids: Vec<u64>,
}

impl FormatProbe {
    fn capture(&self) -> serde_json::Value {
        // execute_write runs inside the production owner's Tokio runtime. The
        // read-only observer uses another thread, without nesting block_on.
        let probe = self.clone();
        std::thread::spawn(move || {
            runtime().block_on(async {
                let db = foundationdb::Database::from_path(&probe.cluster).unwrap();
                let tr = db.create_trx().unwrap();
                tr.set_option(foundationdb::options::TransactionOption::Timeout(4500)).unwrap();
                let version = tr.get_read_version().await.unwrap();
                let mut suffix = probe.epoch.to_be_bytes().to_vec();
                suffix.extend_from_slice(probe.operation.as_bytes());
                let inventory = tr.get(&managed_key(probe.namespace, b"Rm", &suffix), false)
                    .await.unwrap().expect("源初始化前必须保留原完整清单");
                let coverage = tr.get(&managed_key(probe.namespace, b"Ry", &suffix), false)
                    .await.unwrap().expect("源初始化前必须完成独立覆盖");
                let inventory_bytes=inventory.to_vec();
                let inventory: serde_json::Value = serde_json::from_slice(&inventory).unwrap();
                let progress: serde_json::Value = serde_json::from_slice(&coverage).unwrap();
                assert_eq!(inventory["complete"], true);
                assert_eq!(progress["seal"]["complete"], true);
                assert_eq!(progress["seal"]["counts"]["objects"], probe.source_uids.len());
                assert_eq!(progress["seal"]["summary"], inventory["summary"]);
                let mut witnesses=vec![json!({"kind":"Rm","key":managed_key(probe.namespace,b"Rm",&suffix),"bytes":inventory_bytes}),json!({"kind":"Ry","key":managed_key(probe.namespace,b"Ry",&suffix),"bytes":coverage.to_vec()})];
                let mut rows = Vec::new();
                for uid in &probe.source_uids {
                    let mut member = suffix.clone();
                    member.extend_from_slice(&uid.to_be_bytes());
                    let raw = tr.get(&managed_key(probe.namespace,b"Rq",&member),false)
                        .await.unwrap().expect("格式化前每个原始节点（含根）必须有覆盖证明");
                    witnesses.push(json!({"kind":"Rq","key":managed_key(probe.namespace,b"Rq",&member),"bytes":raw.to_vec(),"sha256":format!("{:x}",Sha256::digest(&raw))}));
                    let row: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                    assert_eq!(row["source"]["uid"], *uid);
                    assert_eq!(row["inventory"], inventory["summary"]["digest"]);
                    let sealed=tr.get(&managed_key(probe.namespace,b"Re",&member),false).await.unwrap().expect("覆盖必须对应完整source条目");
                    witnesses.push(json!({"kind":"Re","key":managed_key(probe.namespace,b"Re",&member),"bytes":sealed.to_vec(),"sha256":format!("{:x}",Sha256::digest(&sealed))}));
                    let sealed:serde_json::Value=serde_json::from_slice(&sealed).unwrap();
                    assert_eq!(row["source"],sealed);
                    assert!(coverage_schema_matches(&row,&inventory,*uid));
                    rows.push(row);
                }
                // Identity witness only. The product's source-coverage validator
                // and dedicated pre-consumption fault tests decide validity.
                // Unknown GroupFile schemas remain rejected above.
                assert!(raw_page(&tr,&catalog_key(b'V',probe.source.as_bytes()),1).await.is_empty(),
                    "源初始化前所有namespace的源引用必须为空");
                assert!(tr.get(&managed_key(probe.namespace,b"Rf",&suffix),false).await.unwrap().is_none(),
                    "首次初始化副作用前不能预先报告完成");
                json!({"read_version":version,"source":probe.source.to_string(),"original_operation":probe.operation.to_string(),
                    "before_first_source_content_write":true,"inventory":inventory,"coverage":progress,
                    "coverage_sha256":format!("{:x}",Sha256::new().chain_update(b"tape-rs/reclaim-complete-coverage/1").chain_update(&coverage).finalize()),
                    "source_nodes":rows,"same_rv_raw_witnesses":witnesses,"proof_validity_verified":false,"global_source_references_empty":true})
            })
        }).join().expect("源初始化前只读证明失败")
    }
}

type Trace = Arc<MediaTrace>;
struct MediaTrace {
    started: Instant,
    entries: Mutex<Vec<serde_json::Value>>,
    format_probe: Mutex<Option<FormatProbe>>,
    format_observation: Mutex<Option<serde_json::Value>>,
}
impl Default for MediaTrace {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            entries: Mutex::new(Vec::new()),
            format_probe: Mutex::new(None),
            format_observation: Mutex::new(None),
        }
    }
}
impl MediaTrace {
    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Vec<serde_json::Value>>> {
        self.entries.lock()
    }
    fn nanos(&self) -> u64 {
        self.started.elapsed().as_nanos().try_into().unwrap()
    }
    fn mark(&self, label: &str, action: impl FnOnce()) -> u64 {
        let mut entries = self.lock().unwrap();
        action();
        let at = self.nanos();
        entries.push(json!({"event":"boundary","label":label,"at_nanos":at}));
        at
    }
}
struct RecordedMedia {
    inner: Box<dyn TapeTransport + Send>,
    library: SimLibrary,
    drive: Option<usize>,
    trace: Trace,
}

fn changes_partition_layout(cdb: &[u8], bytes: &[u8]) -> bool {
    let mut at = match cdb[0] {
        0x55 if bytes.len() >= 8 => 8 + usize::from(u16::from_be_bytes([bytes[6], bytes[7]])),
        0x15 if bytes.len() >= 4 => 4 + usize::from(bytes[3]),
        _ => return false,
    };
    while at + 2 <= bytes.len() {
        if bytes[at] & 0x3f == 0x11 {
            return true;
        }
        let len = if bytes[at] & 0x40 != 0 {
            if at + 4 > bytes.len() {
                return false;
            }
            4 + usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]))
        } else {
            2 + usize::from(bytes[at + 1])
        };
        at += len;
    }
    false
}
impl RecordedMedia {
    fn begin(&self, cdb: &[u8]) -> (usize, Option<String>) {
        let barcode = self.drive.and_then(|i| self.library.loaded_barcode(i));
        let mut entries = self.trace.lock().unwrap();
        let id = entries.len();
        entries.push(json!({"event":"enter","id":id,"at_nanos":self.trace.nanos(),"drive":self.drive,"barcode":barcode,"cdb":cdb}));
        (id, barcode)
    }
    fn record(&self, id: usize, barcode: Option<String>, cdb: &[u8], bytes: &[u8], success: bool) {
        let mut entries = self.trace.lock().unwrap();
        let index_generation = if cdb[0] == 0x0a {
            LtfsIndex::parse(bytes).ok().map(|index| index.generation)
        } else {
            None
        };
        entries.push(
            json!({"event":"return","id":id,"at_nanos":self.trace.nanos(),
                "drive":self.drive,"barcode":barcode,"cdb":cdb,"success":success,
                "size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes)),
                "label":bytes.windows(10).any(|w|w==b"<ltfslabel"),
                "index":bytes.windows(10).any(|w|w==b"<ltfsindex"),
                "index_generation":index_generation
            }),
        );
    }
}
impl TapeTransport for RecordedMedia {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        let (id, barcode) = self.begin(cdb);
        assert!(
            !(barcode.as_deref() == Some(BARCODES[0]) && matches!(cdb[0], 0x04 | 0x19 | 0x0b)),
            "quick-reset夹具不允许FORMAT/ERASE/SET CAPACITY先于观察点"
        );
        if barcode.as_deref() == Some(BARCODES[0]) && cdb[0] == 0x10 && cdb[2..5] != [0, 0, 0] {
            self.observe_source_overwrite(cdb, &[])?;
        }
        let result = self.inner.execute_no_data(cdb, timeout);
        self.record(id, barcode, cdb, &[], result.is_ok());
        result
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        bytes: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        let (id, barcode) = self.begin(cdb);
        let result = self.inner.execute_read(cdb, bytes, timeout);
        let n = result.as_ref().map_or(0, |r| r.transferred);
        self.record(id, barcode, cdb, &bytes[..n], result.is_ok());
        result
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        bytes: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        let (id, barcode) = self.begin(cdb);
        assert!(
            !(barcode.as_deref() == Some(BARCODES[0]) && changes_partition_layout(cdb, bytes)),
            "quick-reset源禁止先于观察点重新分区"
        );
        if barcode.as_deref() == Some(BARCODES[0]) && matches!(cdb[0], 0x0a | 0x8d) {
            self.observe_source_overwrite(cdb, bytes)?;
        }
        let result = self.inner.execute_write(cdb, bytes, timeout);
        self.record(id, barcode, cdb, bytes, result.is_ok());
        result
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.inner.medium_change_token()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}

impl RecordedMedia {
    fn observe_source_overwrite(&self, cdb: &[u8], bytes: &[u8]) -> tape_rs::error::Result<()> {
        let probe = self.trace.format_probe.lock().unwrap().take();
        if let Some(probe) = probe {
            let position = TapeDrive::new(self.inner.as_ref()).read_position()?;
            let mut actual = probe.capture();
            actual["first_source_write"] = json!({"cdb":cdb,"barcode":BARCODES[0],
                "old_volume":probe.source.to_string(),"partition":position.partition,"block":position.block_number,
                "size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes)),
                "extra_test_read_position":true,"prefix":bytes.iter().take(4).copied().collect::<Vec<_>>()});
            *self.trace.format_observation.lock().unwrap() = Some(actual);
        }
        Ok(())
    }
}

fn record_transport(
    inner: Box<dyn TapeTransport + Send>,
    library: SimLibrary,
    drive: Option<usize>,
    trace: Trace,
) -> Box<dyn TapeTransport + Send> {
    Box::new(RecordedMedia {
        inner,
        library,
        drive,
        trace,
    })
}

#[derive(Clone, Debug, Default)]
struct ConfirmationReport {
    renewals: u64,
    failure: Option<&'static str>,
    closed_at_nanos: Option<u64>,
}
// A bounded test issuer of healthy confirmations, not a Raft coordinator.
// Uses precisely the fixture's original Serving round and 120-second grants.
struct HealthyOwner {
    stop: Arc<AtomicBool>,
    report: Arc<Mutex<ConfirmationReport>>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl HealthyOwner {
    fn start(
        f: &Fixture,
        permission: Arc<Permission>,
        round: u64,
        route: ActiveLibraryRoute,
    ) -> Self {
        Self::start_observed(f, permission, round, route, None)
    }
    fn start_observed(
        f: &Fixture,
        permission: Arc<Permission>,
        round: u64,
        route: ActiveLibraryRoute,
        trace: Option<Trace>,
    ) -> Self {
        assert!(permission.allows(round, Phase::Serving));
        let catalog = f.catalog.clone();
        let serial = f.serial.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let report = Arc::new(Mutex::new(ConfirmationReport::default()));
        let stopped = stop.clone();
        let observed = report.clone();
        let join = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let budget = Instant::now() + Duration::from_secs(600);
            loop {
                std::thread::park_timeout(Duration::from_millis(250));
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let started = Instant::now();
                let failure = if started >= budget {
                    Some("600秒软件确认预算耗尽")
                } else if !permission.allows(round, Phase::Serving) {
                    Some("原Serving许可失效，禁止复活")
                } else {
                    let result = rt.block_on(async {
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            catalog.active_library_owner(&serial),
                        )
                        .await
                    });
                    match result {
                        Ok(Ok(Some((actual, owner)))) if actual == route && owner == round => {
                            if stopped.load(Ordering::Acquire) {
                                break;
                            }
                            if !permission.allows(round, Phase::Serving)
                                || !permission.renew(
                                    round,
                                    Phase::Serving,
                                    started + Duration::from_secs(120),
                                )
                            {
                                Some("同round许可失效，禁止复活或跨phase")
                            } else {
                                observed.lock().unwrap().renewals += 1;
                                None
                            }
                        }
                        Ok(Ok(_)) => Some("FDB完整route或owner不符"),
                        _ => Some("FDB确认结果未知"),
                    }
                };
                if let Some(failure) = failure {
                    observed.lock().unwrap().failure = Some(failure);
                    let at = if let Some(trace) = &trace {
                        Some(
                            trace.mark("scope_observed_loss_and_closed_original_round", || {
                                permission.close(round)
                            }),
                        )
                    } else {
                        permission.close(round);
                        None
                    };
                    observed.lock().unwrap().closed_at_nanos = at;
                    break;
                }
            }
        });
        Self {
            stop,
            report,
            join: Some(join),
        }
    }
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            join.join().unwrap();
        }
    }
    fn finish(mut self) -> ConfirmationReport {
        self.stop_and_join();
        self.report.lock().unwrap().clone()
    }
}
impl Drop for HealthyOwner {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn evidence(f: &Fixture, name: &str, value: &serde_json::Value) {
    let root = std::env::var_os("TAPE_RS_PRODUCTION_ENTRY_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| f.root.clone());
    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{}-{name}.json", f.route.route.namespace));
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    eprintln!("PRODUCTION_ENTRY_EVIDENCE={}", path.display());
}
fn cartridge(f: &Fixture, barcode: &str) -> serde_json::Value {
    let mut result = None;
    assert!(f.library.with_cartridge_mut(barcode, |c| {
        result = Some(json!({"barcode":c.barcode,"vcr":c.vcr,"mam":format!("{:?}",c.mam),
            "partitions":c.partitions.iter().map(|p| json!({"objects":format!("{:?}",p.objects),"flushed":p.flushed,"capacity":p.capacity})).collect::<Vec<_>>() }));
    }));
    result.unwrap()
}
fn reclaim_request(rt: &tokio::runtime::Runtime, f: &Fixture) -> ManagementRequest {
    let source = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let epoch = rt
        .block_on(f.catalog.request_window(f.route.route.namespace))
        .unwrap()
        .accepting;
    ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":epoch,"operation":Uuid::new_v4().to_string(),
        "change":{"kind":"media-reclaim","pool":f.pools[0].to_string(),"barcode":BARCODES[0],"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":false}})).unwrap()
}
fn task(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    request: &ManagementRequest,
) -> serde_json::Value {
    rt.block_on(
        f.catalog
            .management_task(&f.serial, f.route, request.epoch(), request.operation()),
    )
    .unwrap()
    .unwrap()
}

fn shutdown_checked(f: &Fixture, mut owner: Owner, label: &str) {
    let (done, rx) = channel();
    let sent = owner
        .tx
        .send(ExecRequest::Shutdown { done: Some(done) })
        .is_ok();
    let acknowledged = sent && rx.recv_timeout(Duration::from_secs(15)).is_ok();
    let joined = owner.join.take().is_some_and(|join| join.join().is_ok());
    let safe = !owner.trace.lock().unwrap().iter().any(|c| {
        matches!(c[0], 0x04 | 0x19) || (!owner.writable && matches!(c[0], 0x0a | 0x10 | 0x8d))
    });
    evidence(
        f,
        label,
        &json!({"shutdown_sent":sent,"shutdown_acknowledged":acknowledged,"thread_join_ok":joined,"trace_safety_ok":safe}),
    );
    assert!(
        sent && acknowledged && joined && safe,
        "Owner必须正常Shutdown回执+join，且无越权介质命令"
    );
}

fn return_all_sim_media(library: &SimLibrary) {
    let dev = library.changer_as(71);
    let mut changer = MediumChanger::new(&dev);
    changer.load_address_map().unwrap();
    loop {
        let elements = changer.read_all_status().unwrap();
        let drives: Vec<_> = elements
            .iter()
            .filter(|e| e.element_type == ElementType::DataTransfer)
            .collect();
        let Some((index, drive)) = drives.iter().enumerate().find(|(_, e)| e.full) else {
            break;
        };
        let slot = elements
            .iter()
            .find(|e| e.element_type == ElementType::Storage && !e.full)
            .unwrap();
        TapeDrive::new(&library.drive_as(index, 71))
            .unload()
            .unwrap();
        changer.move_medium(drive.address, slot.address).unwrap();
    }
}

fn batch_body(number: usize) -> Vec<u8> {
    (0..(1 << 20))
        .map(|offset| ((offset * 31 + number * 97) % 251) as u8)
        .collect()
}

// Deliberately do not forward Sim's all-handle content token. Real ScsiDevice
// has none; the measured production path must retain the same capability limit.
struct NoContentToken {
    inner: Box<dyn TapeTransport + Send>,
}
impl TapeTransport for NoContentToken {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        out: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_read(cdb, out, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        bytes: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_write(cdb, bytes, timeout)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn medium_change_token(&self) -> Option<[u8; 32]> {
        None
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}
struct NoTokenProvider {
    base: Provider,
    library: SimLibrary,
    trace: Trace,
}
impl DeviceProvider for NoTokenProvider {
    fn open_all(&self) -> tape_rs::error::Result<Vec<ManagedDevice>> {
        let mut devices = self.base.open_all()?;
        for (index, device) in devices.iter_mut().enumerate() {
            let inner = std::mem::replace(&mut device.dev, Box::new(self.library.changer_as(71)));
            device.dev = Box::new(NoContentToken {
                inner: record_transport(
                    inner,
                    self.library.clone(),
                    (index < 2).then_some(index),
                    self.trace.clone(),
                ),
            });
            assert!(device.dev.medium_change_token().is_none());
        }
        Ok(devices)
    }
}
fn batch_owner(f: &Fixture, round: u64, full_trace: Trace) -> Owner {
    let management = json!({});
    let writable = true;
    let files = FileService::new(f.root.join(format!("owner-{round}"))).unwrap();
    let status = Arc::new(Mutex::new(NodeStatus {
        catalog_frozen_index: Some(f.route.frozen_index),
        management: Arc::new(management),
        ..Default::default()
    }));
    let permission = Arc::new(Permission::default());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let fail_vci = Arc::new(AtomicBool::new(false));
    let fail_read = Arc::new(AtomicBool::new(false));
    let fail_write = Arc::new(AtomicBool::new(false));
    let fail_load = Arc::new(AtomicBool::new(false));
    let fail_load_before = Arc::new(AtomicBool::new(false));
    let provider = Provider {
        library: f.library.clone(),
        serial: f.serial.clone(),
        trace: trace.clone(),
        permission: permission.clone(),
        fail_vci: fail_vci.clone(),
        fail_read: fail_read.clone(),
        fail_write: fail_write.clone(),
        fail_load: fail_load.clone(),
        fail_load_before: fail_load_before.clone(),
    };
    let provider: Box<dyn DeviceProvider> = Box::new(NoTokenProvider {
        base: provider,
        library: f.library.clone(),
        trace: full_trace,
    });
    let control = FdbOwnerControl {
        catalog: f.catalog.clone(),
        serial: f.serial.clone(),
        route: f.route,
        storage: Default::default(),
    };
    let (tx, rx) = channel();
    let (events, event_rx) = channel();
    let http_files = files.clone();
    let http_status = status.clone();
    let join = std::thread::spawn(move || {
        let run = if writable {
            tape_rs::daemon::executor::run_fdb_writer
        } else {
            tape_rs::daemon::executor::run_fdb_owner
        };
        run(
            provider,
            ExecOptions {
                node_id: 71,
                interval: Duration::from_millis(100),
                demo_write: false,
                salvage: false,
                block_size: 64 * 1024,
                read_idle: Duration::from_secs(300),
            },
            rx,
            events,
            files,
            status,
            control,
        )
    });
    tx.send(ExecRequest::AttachPermission {
        permission: permission.clone(),
    })
    .unwrap();
    permission.renew(
        round,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120),
    );
    tx.send(ExecRequest::Takeover { round }).unwrap();
    let event = next_owner_event(&event_rx);
    assert!(matches!(event, ExecEvent::Fenced { .. }), "{event:?}");
    permission.renew(
        round,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120),
    );
    tx.send(ExecRequest::Recover { round }).unwrap();
    let event = next_owner_event(&event_rx);
    assert!(matches!(event, ExecEvent::Serving { .. }), "{event:?}");
    permission.renew(
        round,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    Owner {
        events: event_rx,
        permission,
        pools: f.pools,
        tx,
        join: Some(join),
        trace,
        writable,
        files: http_files,
        status: http_status,
        fail_vci,
        fail_read,
        fail_write,
        fail_load,
        fail_load_before,
    }
}

async fn batch_fixture(cluster: &str, members: usize) -> Fixture {
    let objects: &[String] = &[];
    let empty_pool: Option<Uuid> = None;
    let catalog = ManagedCatalog::open(Some(cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pools = [Uuid::new_v4(), Uuid::new_v4()];
    let serial = format!("SIM-SCHEDULE-{namespace}");
    let root = std::env::temp_dir().join(format!("tape-rs-fdb-scheduling-{namespace}"));
    fs::create_dir(&root).unwrap();
    eprintln!("fdb_schedule_fixture_workspace={}", root.display());
    let library = SimLibrary::new(2, 8, 1);
    library.enable_changer_pr();
    let snapshot = root.join("snapshot.db");
    let db = rusqlite::Connection::open(&snapshot).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
         CREATE TABLE pools(uuid TEXT PRIMARY KEY,name TEXT NOT NULL,file_limit INTEGER NOT NULL);
         CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
           volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL,
           files INTEGER NOT NULL DEFAULT 0,bytes_used INTEGER NOT NULL DEFAULT 0,
           bytes_written INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
           generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
           ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
           metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
         CREATE TABLE staging(barcode TEXT NOT NULL);
         CREATE TABLE file_tasks(pool_uuid TEXT NOT NULL,task INTEGER NOT NULL,
           round INTEGER NOT NULL,path TEXT NOT NULL,status TEXT NOT NULL,
           generation INTEGER,reason TEXT NOT NULL DEFAULT '');
         INSERT INTO meta(k,v) VALUES('applied_index',92),('catalog_frozen_index',90);",
    )
    .unwrap();

    assert!(catalog.begin_managed_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    for (i, pool) in pools.iter().chain(empty_pool.iter()).enumerate() {
        let name = format!("pool-{i}");
        db.execute(
            "INSERT INTO pools(uuid,name,file_limit) VALUES(?1,?2,1000000)",
            rusqlite::params![pool.to_string(), name],
        )
        .unwrap();
        assert_eq!(
            catalog
                .create_pool(
                    namespace,
                    &PoolRecord {
                        created_at_millis: 1_793_405_000_123,
                        library_serial: serial.clone(),
                        id: *pool,
                        name,
                        file_limit: 1_000_000,
                        policy: PoolPolicy::default(),
                        revision: 1
                    }
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
    }
    let mut entries = Vec::new();
    for (i, barcode) in BARCODES.into_iter().enumerate() {
        let pool = pools[usize::from(i == 2)];
        library
            .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), i)
            .unwrap();
        library.load_into_drive(barcode, 0).unwrap();
        let drive = library.drive(0);
        mkltfs(
            &drive,
            &MkltfsOptions {
                volume_id: format!("READ{i:02}"),
                block_size: 64 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let mut volume = LtfsVolume::mount(&drive).unwrap();
        if i == 0 && !objects.is_empty() {
            for path in objects {
                let content = path
                    .strip_prefix("/file-")
                    .map(|id| format!("content-{id}").into_bytes())
                    .unwrap_or_else(|| b"object".to_vec());
                if path == "/k0000" {
                    let padding = "x".repeat(2_000_000);
                    volume.append_file_with_xattrs(path, &mut content.as_slice(), &[
                        ("tapers.s3.headers", r#"{"content-type":"text/plain","cache-control":"max-age=60","x-amz-meta-origin":"fdb-test"}"#),
                        ("unrelated-large-attribute", &padding),
                    ]).unwrap();
                } else {
                    volume.append_file(path, &mut content.as_slice()).unwrap();
                }
            }
            volume.commit().unwrap();
        }
        let baseline = ActiveIndexIdentity::from(volume.committed_index_construct());
        let xml = volume.index().to_xml().unwrap();
        if i == 0 && !objects.is_empty() {
            for record in tape_rs::daemon::files::catalog_of(volume.index()).0 {
                db.execute("INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", rusqlite::params![pool.to_string(), record.path, barcode, baseline.generation as i64, record.length as i64, record.sha256, record.version.0 as i64, record.version.1 as i64, i64::from(record.deleted), record.metadata.to_string()]).unwrap();
            }
        }
        let source = ExpectedIndex {
            volume_uuid: baseline.volume_uuid,
            generation: baseline.generation,
            location: baseline.location,
            sha256: Sha256::digest(&xml).into(),
        };
        assert_eq!(baseline.sha256, source.sha256);
        drop(volume);

        let spool_path = root.join(format!("import-{i}.db"));
        ImportSpool::build(&xml[..], &spool_path, source).unwrap();
        let spool = ImportSpool::open(&spool_path, source).unwrap();

        db.execute(
            "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                barcode,
                pool.to_string(),
                baseline.volume_uuid.to_string(),
                baseline.generation as i64
            ],
        )
        .unwrap();
        assert_eq!(
            catalog
                .assign_tape(namespace, pool, &serial, barcode)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        let placement = VolumePlacement {
            volume_id: baseline.volume_uuid,
            namespace,
            pool_id: pool,
            library_serial: serial.clone(),
            barcode: barcode.into(),
        };
        assert_eq!(
            catalog
                .register_volume_placement(&placement, 10)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        let scan = VolumeScan {
            generation: baseline.generation,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, baseline.volume_uuid, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let cache_path = root.join(format!("baseline-{i}.extent.db"));
        VolumeExtentCache::build(&xml[..], &cache_path, source).unwrap();
        assert_eq!(
            catalog
                .record_volume_source(
                    &serial,
                    10,
                    baseline.volume_uuid,
                    scan,
                    source,
                    LocalVolumeSources {
                        extent_cache: &cache_path,
                        import_spool: &spool_path,
                    },
                )
                .await
                .unwrap(),
            SourceOutcome::Recorded
        );
        let empty = ScanDigest::new(baseline.volume_uuid, scan)
            .unwrap()
            .summary();
        assert_eq!(
            catalog
                .freeze_volume_scan(&serial, 10, baseline.volume_uuid, scan, empty)
                .await
                .unwrap(),
            VerificationOutcome::Frozen
        );
        assert_eq!(
            catalog
                .verify_volume_scan_page_with_cache(
                    &serial,
                    10,
                    baseline.volume_uuid,
                    scan,
                    100,
                    &cache_path,
                )
                .await
                .unwrap(),
            VerificationOutcome::Verified(empty)
        );
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, baseline.volume_uuid, scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        let binding = ImportSourceBinding {
            placement: placement.clone(),
            scan,
            source,
        };

        entries.push((binding, spool));
        fixture_return_to_storage(&library, 0);
    }
    let volumes = entries
        .iter()
        .map(|(b, spool)| ImportVolume {
            pool_id: b.placement.pool_id,
            barcode: b.placement.barcode.clone(),
            volume_id: b.placement.volume_id,
            generation: b.source.generation,
            spool,
        })
        .collect();
    let bindings: Vec<_> = entries.iter().map(|(b, _)| b.clone()).collect();
    let staged = catalog
        .stage_audited_reconcile(namespace, &snapshot, &root, volumes, &bindings)
        .await
        .unwrap();
    if objects.is_empty() {
        assert_eq!(staged.paths, 0);
    } else {
        assert!(staged.paths >= objects.len() as u64);
    }
    catalog
        .confirm_bound_staged_import(namespace)
        .await
        .unwrap();
    while !catalog
        .materialize_bound_import_page(namespace, 100)
        .await
        .unwrap()
        .complete()
    {}
    assert_eq!(
        catalog.freeze_bound_staged_import(namespace).await.unwrap(),
        NamespaceVerificationOutcome::Frozen
    );
    loop {
        match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
            NamespaceVerificationOutcome::Progress { .. } => continue,
            NamespaceVerificationOutcome::Verified(_) => break,
            other => panic!("空目录核验失败：{other:?}"),
        }
    }
    let verified = catalog.verify_bound_import_copies(namespace).await.unwrap();
    if objects.is_empty() {
        assert_eq!(verified, 0);
    } else {
        assert!(verified >= objects.len() as u64);
    }
    catalog
        .inspect_bound_import_copy_fence(namespace)
        .await
        .unwrap();
    assert!(catalog.seal_bound_staged_import(namespace).await.unwrap());
    let route = match catalog
        .prepare_library_route(&serial, namespace, None)
        .await
        .unwrap()
    {
        PrepareRouteOutcome::Prepared(route) => route,
        other => panic!("准备路由失败：{other:?}"),
    };
    let proof = |node, path: &std::path::Path| {
        let identity = inspect_frozen_snapshot(path).unwrap();
        verify_node_export(
            path,
            node,
            identity.applied_index,
            identity.frozen_index,
            identity.snapshot_sha256,
        )
        .unwrap()
    };
    let node72 = root.join("node72.db");
    let node73 = root.join("node73.db");
    for (path, applied) in [(&node72, 93), (&node73, 94)] {
        fs::copy(&snapshot, path).unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute("UPDATE meta SET v=?1 WHERE k='applied_index'", [applied])
            .unwrap();
    }
    let evidence = [proof(71, &snapshot), proof(72, &node72), proof(73, &node73)];
    assert!(
        catalog
            .certify_prepared_cutover(&serial, route, 71, evidence)
            .await
            .unwrap()
    );
    let certificate = catalog
        .certified_prepared_cutover(&serial)
        .await
        .unwrap()
        .unwrap();
    let active = catalog
        .activate_prepared_cutover(&serial, certificate)
        .await
        .unwrap();

    let mut pool_roots = [Uuid::nil(); 2];
    for (i, pool) in pools.iter().enumerate() {
        let Some(ResolvedPath::Directory { id, .. }) = catalog
            .resolve_active_path(&serial, format!("/{pool}").as_bytes())
            .await
            .unwrap()
        else {
            panic!("missing pool root");
        };
        pool_roots[i] = id;
    }
    // Independent ordinary native S4 commits reproduce the measured source
    // generation count. Destination and unrelated tapes remain genuinely empty.
    let key = ReservationKey::new(71, 11);
    let binding = &entries[0].0;
    library.load_into_drive(BARCODES[0], 0).unwrap();
    let drive = library.drive(0);
    fence(&drive, key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    for number in 0..members {
        let bytes = batch_body(number);
        let path = root.join(format!("batch-seed-{number}"));
        fs::write(&path, &bytes).unwrap();
        let intent = ActiveFileIntent {
            barcode_constraint: Some(BARCODES[0].into()),
            namespace,
            parent: pool_roots[0],
            parent_path: b"/".to_vec(),
            name: format!("batch-file-{number}").into_bytes(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: serial.clone(),
            pool_id: Some(pools[0]),
            owner_epoch: 11,
            volume_id: binding.source.volume_uuid,
            volume_generation: volume.index().generation + 1,
            volume_scan_id: binding.scan.id,
            expected_size: bytes.len() as u64,
            expected_digest: ContentDigest::Sha256(Sha256::digest(&bytes).into()),
        };
        let committed = commit_active_media_batch(
            &catalog,
            volume,
            Uuid::new_v4(),
            &[ActiveMediaUpload {
                intent,
                spool: path,
            }],
            64 << 20,
        )
        .await
        .unwrap();
        volume = publish_active_media_batch(&catalog, committed)
            .await
            .unwrap()
            .into_volume();
    }
    assert_eq!(volume.index().generation, members as u64 + 1);
    assert_eq!(
        tape_rs::daemon::files::catalog_of(volume.index()).0.len(),
        members
    );
    drop(volume);
    fixture_return_to_storage(&library, 0);
    tape_rs::scsi::reservation::release_and_unregister(&drive, key).unwrap();
    drop(entries);
    drop(db);
    Fixture {
        root,
        library,
        catalog: Arc::new(catalog),
        serial,
        route: active,
        pools,
        pool_roots,
    }
}

fn directory_count(node: &tape_rs::ltfs::index::DirectoryNode) -> u64 {
    1 + node.subdirs.iter().map(directory_count).sum::<u64>()
}

fn inspect_batch_medium(f: &Fixture, barcode: &str) -> LtfsIndex {
    f.library.load_into_drive(barcode, 0).unwrap();
    let device = f.library.drive_as(0, 71);
    let key = ReservationKey::new(71, 11);
    fence(&device, key).unwrap();
    let volume = LtfsVolume::mount(&device).unwrap();
    let result = volume.index().clone();
    drop(volume);
    fixture_return_to_storage(&f.library, 71);
    tape_rs::scsi::reservation::release_and_unregister(&device, key).unwrap();
    result
}

fn native_files(index: &LtfsIndex) -> std::collections::BTreeMap<String, FileNode> {
    let mut files = std::collections::BTreeMap::new();
    index.walk_files(|path, node| {
        assert!(
            files
                .insert(path.trim_start_matches('/').to_owned(), node.clone())
                .is_none()
        );
    });
    files
}

fn assert_native_reference(source: &FileNode, donor: &FileNode, archive: &FileNode) {
    assert_eq!(donor.length, source.length);
    assert_eq!(archive.length, source.length);
    assert_eq!(
        donor.extents, archive.extents,
        "归档引用必须共享已落带donor的真实extent"
    );
    assert!(!donor.extents.is_empty(), "非空测试正文必须有真实extent");
    assert_ne!(
        donor.meta.file_uid, archive.meta.file_uid,
        "归档引用与donor是不同原生节点"
    );
    let mut expected = source.clone();
    expected.name = archive.name.clone();
    expected.meta.file_uid = archive.meta.file_uid;
    expected.extents = archive.extents.clone();
    assert_eq!(
        *archive, expected,
        "归档必须完整保留原始属性、时间、xattr及关闭状态"
    );
    assert_eq!(
        ContentDigest::from_ltfs_xattrs(&donor.xattrs).unwrap(),
        ContentDigest::from_ltfs_xattrs(&source.xattrs).unwrap()
    );
    assert_eq!(
        donor.extents.iter().map(|e| e.byte_count).sum::<u64>(),
        source.length
    );
    let mut offset = 0;
    for extent in &donor.extents {
        assert_eq!(extent.file_offset, offset, "extent必须覆盖完整连续文件范围");
        assert!(extent.start_block > 0);
        offset += extent.byte_count;
    }
}

async fn terminal_storage_proof(
    cluster: &str,
    f: &Fixture,
    original: &ManagementRequest,
    old_source: Uuid,
    new_source: Uuid,
    target: Uuid,
    members: usize,
    before_format: &serde_json::Value,
) -> serde_json::Value {
    let db = foundationdb::Database::from_path(cluster).unwrap();
    let tr = db.create_trx().unwrap();
    tr.set_option(foundationdb::options::TransactionOption::Timeout(4500))
        .unwrap();
    let rv = tr.get_read_version().await.unwrap();
    let ns = f.route.route.namespace;
    let mut suffix = original.epoch().to_be_bytes().to_vec();
    suffix.extend_from_slice(original.operation().as_bytes());
    let receipt = tr
        .get(&managed_key(ns, b"Rf", &suffix), false)
        .await
        .unwrap()
        .expect("原独立初始化完成回执必须保留");
    assert!(receipt.len() > 32);
    let (body, sum) = receipt.split_at(receipt.len() - 32);
    assert_eq!(
        &Sha256::new()
            .chain_update(b"tape-rs/reclaim-format-result/1")
            .chain_update(body)
            .finalize()[..],
        sum,
        "原初始化回执checksum"
    );
    let receipt: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(receipt["namespace"], ns.to_string());
    assert_eq!(receipt["library"], f.serial);
    assert_eq!(
        receipt["intent"]["operation"],
        original.operation().to_string()
    );
    assert_eq!(receipt["intent"]["epoch"], original.epoch());
    assert_eq!(receipt["goal"]["source"], old_source.to_string());
    assert_eq!(receipt["goal"]["output"], new_source.to_string());
    assert_eq!(receipt["goal"]["issued"], true);
    assert_eq!(receipt["logical_files"], members);
    assert_eq!(receipt["logical_bytes"], members << 20);
    let coverage = receipt["goal"]["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| format!("{:02x}", b.as_u64().unwrap()))
        .collect::<String>();
    assert_eq!(coverage.len(), 64);
    assert_eq!(
        coverage, before_format["coverage_sha256"],
        "终局回执必须绑定首次覆盖源之前实读的完整覆盖证明"
    );
    let mut empty_probes = Vec::new();
    let mut ranges = vec![catalog_key(b'V', old_source.as_bytes())];
    for volume in [old_source, new_source, target] {
        for kind in *b"24" {
            ranges.push(catalog_key(kind, volume.as_bytes()));
        }
        let mut reverse = b"o/".to_vec();
        reverse.extend_from_slice(volume.as_bytes());
        ranges.push(catalog_key(b'p', &reverse));
        let mut node = b"Qv".to_vec();
        node.extend_from_slice(volume.as_bytes());
        for key in [
            catalog_key(b'3', volume.as_bytes()),
            catalog_key(b'Q', volume.as_bytes()),
            catalog_key(b'%', volume.as_bytes()),
            managed_key(Uuid::nil(), &node, &[]),
        ] {
            assert!(
                tr.get(&key, false).await.unwrap().is_none(),
                "终局仍有volume占用: {key:?}"
            );
            empty_probes.push(json!({"kind":"point","key":key,"present":false}));
        }
    }
    for kind in *b"0IF" {
        ranges.push(catalog_key(kind, ns.as_bytes()));
    }
    ranges.push(managed_key(ns, b"Qh", &[]));
    for prefix in ranges {
        assert!(
            raw_page(&tr, &prefix, 1).await.is_empty(),
            "终局仍有活跃引用/写入: {prefix:?}"
        );
        empty_probes.push(json!({"kind":"range_limit1","prefix":prefix,"rows":0}));
    }
    let mut serial = f.serial.as_bytes().to_vec();
    serial.extend_from_slice(&[0, 0]);
    for key in [
        managed_key(ns, b"Rp", f.pools[0].as_bytes()),
        managed_key(ns, b"i", f.pools[0].as_bytes()),
        managed_key(ns, b"Kg", &serial),
    ] {
        assert!(
            tr.get(&key, false).await.unwrap().is_none(),
            "终局pool/library仍忙"
        );
        empty_probes.push(json!({"kind":"point","key":key,"present":false}));
    }
    json!({"read_version":rv,"receipt":receipt,"empty_occupancy_probes":empty_probes})
}

fn run_production_reclaim_batch(members: usize, fail_fast_count: bool) {
    assert!(
        std::env::var_os("RUST_MIN_STACK").is_none(),
        "必须使用默认线程栈"
    );
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let approved_cluster = PrivateCluster::open(std::path::Path::new(&cluster)).unwrap();
    let cluster = approved_cluster.path.clone();
    let _network = unsafe { foundationdb::boot() };
    let rt = runtime();
    let f = rt.block_on(Box::pin(batch_fixture(&cluster, members)));
    let source_index = inspect_batch_medium(&f, BARCODES[0]);
    let target_index = inspect_batch_medium(&f, BARCODES[1]);
    let source_generation = source_index.generation;
    let source_directories = directory_count(&source_index.root);
    let source_files = native_files(&source_index);
    let target_generation = target_index.generation;
    let target_files = native_files(&target_index);
    assert_eq!(
        source_directories, 1,
        "本基准只含原根目录，不掺入额外目录S4"
    );
    assert_eq!(
        (
            source_generation,
            source_files.len(),
            target_generation,
            target_files.len()
        ),
        (members as u64 + 1, members, 1, 0)
    );
    let source = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let target = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[1]))
        .unwrap()
        .unwrap();
    let source_logical: Vec<_> = (0..members)
        .map(|number| {
            let value = rt
                .block_on(f.catalog.resolve_active_path_for_route(
                    &f.serial,
                    format!("/{}/batch-file-{number}", f.pools[0]).as_bytes(),
                    f.route,
                ))
                .unwrap()
                .unwrap();
            let ResolvedPath::File { copy, .. } = &value else {
                panic!("源必须是普通managed文件")
            };
            assert!(
                copy.media.is_none(),
                "必须真正通过managed source group eligibility，不能raw-import替代"
            );
            assert_eq!(copy.volume_id, source.volume_id.unwrap());
            assert_eq!(
                copy.content_digest,
                ContentDigest::Sha256(Sha256::digest(batch_body(number)).into())
            );
            assert!(
                source_files
                    .values()
                    .any(|node| node.meta.file_uid == copy.ltfs_file_uid)
            );
            value
        })
        .collect();
    let before_other = [cartridge(&f, BARCODES[2]), cartridge(&f, BARCODES[3])];
    let setup = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[3]:{"blacklisted":true}}}),
        false,
    );
    shutdown_checked(&f, setup, "batch-setup-shutdown");
    let trace = Trace::default();
    let owner = batch_owner(&f, 13, trace.clone());
    let healthy = HealthyOwner::start(&f, owner.permission.clone(), 13, f.route);
    let original = reclaim_request(&rt, &f);
    let mut source_uids = vec![source_index.root.meta.file_uid];
    source_uids.extend(source_files.values().map(|node| node.meta.file_uid));
    *trace.format_probe.lock().unwrap() = Some(FormatProbe {
        cluster: cluster.clone(),
        namespace: f.route.route.namespace,
        operation: original.operation(),
        epoch: original.epoch(),
        source: source.volume_id.unwrap(),
        source_uids,
    });
    let boundary = trace.mark("batch-reclaim-submit", || {});
    rt.block_on(
        f.catalog
            .admit_management(&f.serial, f.route, 13, &original),
    )
    .unwrap();
    let started = Instant::now();
    let mut last = serde_json::Value::Null;
    let mut last_generation = target_generation;
    let completed = loop {
        let current = task(&rt, &f, &original);
        if current != last {
            evidence(&f, "batch-task", &current);
            // Keep diagnostics even if the outer software execution budget
            // expires before the final functional and performance assertions.
            let observed = rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, target.volume_id.unwrap()),
            );
            let commands: Vec<_> = trace
                .lock()
                .unwrap()
                .iter()
                .filter(|v| {
                    v["event"] == "return"
                        && v["at_nanos"].as_u64().is_some_and(|at| at >= boundary)
                })
                .cloned()
                .collect();
            let target_commands: Vec<_> = commands
                .iter()
                .filter(|v| v["barcode"] == BARCODES[1] && v["success"] == true)
                .collect();
            let count = |opcode: u8| {
                target_commands
                    .iter()
                    .filter(|v| v["cdb"][0] == opcode)
                    .count()
            };
            evidence(
                &f,
                "batch-progress-counts",
                &json!({"original":original.value(),"task":current,
                    "elapsed_seconds":started.elapsed().as_secs_f64(),
                    "target_index":format!("{observed:?}"),
                    "target_initial_generation":target_generation,
                    "target_locate":count(0x92),"target_filemarks":count(0x10),
                    "target_read_attribute":count(0x8c),
                    "target_write_attribute":count(0x8d),
                    "medium_change_token":null,"default_stack":true,
                    "functional_checks_complete":false}),
            );
            evidence(&f, "batch-progress-cdb", &json!(commands));
            if let Ok(head) = observed {
                assert!(
                    head.generation >= last_generation,
                    "同一未格式化目标卷的已观察generation倒退"
                );
                last_generation = head.generation;
                if fail_fast_count {
                    assert!(
                        head.generation - target_generation <= 2,
                        "target commit count crossed monotone group bound before completion: delta={}, bound={}; functional checks incomplete; see progress evidence",
                        head.generation - target_generation,
                        2
                    );
                }
            }
            last = current.clone();
        }
        if current["status"] == "succeeded" {
            break current;
        }
        assert_ne!(current["status"], "failed", "{current}");
        for event in owner.events.try_iter() {
            assert!(
                !matches!(event, ExecEvent::Lost { .. }),
                "{event:?}; task={current}"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "600秒纯软件预算耗尽: {current}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(completed["request"], *original.value());
    assert_eq!(completed["result"]["migrated_logical_files"], members);
    assert_eq!(completed["result"]["migrated_logical_bytes"], members << 20);
    assert_eq!(completed["result"]["initialization_attempts"], 1);
    let final_head = rt
        .block_on(
            f.catalog
                .observed_active_volume_index(&f.serial, target.volume_id.unwrap()),
        )
        .unwrap();
    let commands: Vec<_> = trace
        .lock()
        .unwrap()
        .iter()
        .filter(|v| {
            v["event"] == "return" && v["at_nanos"].as_u64().is_some_and(|at| at >= boundary)
        })
        .cloned()
        .collect();
    let target_commands: Vec<_> = commands
        .iter()
        .filter(|v| v["barcode"] == BARCODES[1] && v["success"] == true)
        .collect();
    let count = |predicate: &dyn Fn(&serde_json::Value) -> bool| {
        target_commands.iter().filter(|v| predicate(v)).count()
    };
    let counts = json!({"original":original.value(),"completed":completed,"files":members,"bytes":members << 20,"source_generation":source_generation,"source_directories":source_directories,"target_generation_before":target_generation,"target_generation_after":final_head.generation,"target_commits":final_head.generation-target_generation,
        "locate_eod":count(&|v|v["cdb"][0]==0x92 && v["cdb"][1].as_u64().unwrap()&0x38==0x18),
        "locate_block":count(&|v|v["cdb"][0]==0x92 && v["cdb"][1].as_u64().unwrap()&0x38==0),
        "fm0":count(&|v|v["cdb"][0]==0x10 && v["cdb"][2]==0 && v["cdb"][3]==0 && v["cdb"][4]==0),
        "fm1_sync":count(&|v|v["cdb"][0]==0x10 && v["cdb"][1].as_u64().unwrap()&1==0 && v["cdb"][4]==1),
        "fm1_immed":count(&|v|v["cdb"][0]==0x10 && v["cdb"][1].as_u64().unwrap()&1!=0 && v["cdb"][4]==1),
        "read_attribute":count(&|v|v["cdb"][0]==0x8c),"write_attribute":count(&|v|v["cdb"][0]==0x8d),"index_write":count(&|v|v["cdb"][0]==0x0a && v["index"]==true),
        "elapsed_seconds":started.elapsed().as_secs_f64(),"medium_change_token":null,"default_stack":true});
    evidence(&f, "batch-counts", &counts);
    evidence(&f, "batch-cdb", &json!(commands));
    eprintln!("PRODUCTION_BATCH_COUNTS={counts}");
    let mut physical = Vec::new();
    for number in 0..members {
        let Some(ResolvedPath::File {
            id,
            head,
            copy,
            attributes,
            object_metadata,
            ..
        }) = rt
            .block_on(f.catalog.resolve_active_path_for_route(
                &f.serial,
                format!("/{}/batch-file-{number}", f.pools[0]).as_bytes(),
                f.route,
            ))
            .unwrap()
        else {
            panic!("missing copied file {number}")
        };
        let ResolvedPath::File {
            id: old_id,
            head: old_head,
            attributes: old_attributes,
            object_metadata: old_object_metadata,
            copy: old_copy,
            ..
        } = &source_logical[number]
        else {
            unreachable!()
        };
        assert_eq!(id, *old_id, "迁移不得改变文件id");
        assert!(initial_file_versions_valid(old_head.version, head.version));
        assert_ne!(head.copy_id, old_head.copy_id);
        assert_eq!(copy.version, head.version);
        assert_eq!(copy.size, old_copy.size);
        assert_eq!(copy.content_digest, old_copy.content_digest);
        assert!(copy.media.is_none());
        assert_eq!(attributes, *old_attributes);
        assert_eq!(object_metadata, *old_object_metadata);
        assert_eq!(copy.volume_id, target.volume_id.unwrap());
        assert_eq!(
            copy.volume_generation, final_head.generation,
            "所有成员必须由同一最终body+archive checkpoint覆盖"
        );
        physical.push((copy.ltfs_file_uid, old_copy.ltfs_file_uid));
    }
    let confirmation = healthy.finish();
    assert!(
        confirmation.renewals > 0 && confirmation.failure.is_none(),
        "{confirmation:?}"
    );
    shutdown_checked(&f, owner, "batch-writer-shutdown");
    let before_format = trace
        .format_observation
        .lock()
        .unwrap()
        .clone()
        .expect("必须观察源首次内容覆盖之前的独立覆盖证据");
    assert!(first_source_write_valid(
        &before_format["first_source_write"]
    ));
    evidence(&f, "batch-before-source-overwrite", &before_format);
    let reset_source = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let new_source = reset_source.volume_id.unwrap();
    assert_ne!(new_source, source.volume_id.unwrap());
    assert_eq!(completed["result"]["volume"], new_source.to_string());
    assert_eq!(reset_source.pool_id, source.pool_id);
    let storage = rt.block_on(terminal_storage_proof(
        &cluster,
        &f,
        &original,
        source.volume_id.unwrap(),
        new_source,
        target.volume_id.unwrap(),
        members,
        &before_format,
    ));
    evidence(&f, "batch-terminal-storage-proof", &storage);
    for (index, before) in before_other.iter().enumerate() {
        assert_eq!(
            cartridge(&f, BARCODES[index + 2]),
            *before,
            "unrelated medium changed"
        );
    }
    f.library.power_cut();
    return_all_sim_media(&f.library);
    f.library.load_into_drive(BARCODES[1], 0).unwrap();
    let dev = f.library.drive_as(0, 71);
    let key = ReservationKey::new(71, 14);
    fence(&dev, key).unwrap();
    let volume = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(volume.label().volume_uuid, target.volume_id.unwrap());
    assert_eq!(volume.index().generation, final_head.generation);
    assert_eq!(
        volume.index().root.meta,
        target_index.root.meta,
        "原目标卷根属性不可被源根覆盖"
    );
    assert_eq!(volume.index().root.xattrs, target_index.root.xattrs);
    let files = native_files(volume.index());
    assert_eq!(
        files.len(),
        members * 2,
        "普通donor和完整原生archive各一份节点"
    );
    assert_eq!(
        directory_count(&volume.index().root),
        2,
        "原目标卷根+唯一归档根"
    );
    let archive_root = format!("_reclaim_{}", original.operation());
    let archive_dir = volume
        .index()
        .root
        .subdirs
        .iter()
        .find(|dir| dir.name == archive_root)
        .expect("原parent对应唯一归档根");
    let mut root_meta = source_index.root.meta.clone();
    root_meta.file_uid = archive_dir.meta.file_uid;
    assert_eq!(archive_dir.meta, root_meta, "归档根完整保留原source根属性");
    assert_eq!(archive_dir.xattrs, source_index.root.xattrs);
    let mut cold_checks = Vec::new();
    for (number, (uid, original_uid)) in physical.iter().enumerate() {
        let matches: Vec<_> = files
            .iter()
            .filter(|(_, node)| node.meta.file_uid == *uid)
            .collect();
        assert_eq!(matches.len(), 1, "已发布donor UID在新介质上必须唯一");
        let (donor_path, donor) = matches[0];
        let source_node = source_files
            .values()
            .find(|node| node.meta.file_uid == *original_uid)
            .unwrap();
        assert!(
            !source_node.xattrs.is_empty(),
            "本夹具至少有真实SHA/version原生属性，不允许空集合假通过"
        );
        let archive_path = format!("{archive_root}/data-{original_uid}");
        let archive = files
            .get(&archive_path)
            .expect("以原sourceUID映射的完整归档节点");
        assert_native_reference(source_node, donor, archive);
        let mut reads = Vec::new();
        for path in [donor_path.as_str(), archive_path.as_str()] {
            let mut actual = Vec::new();
            volume.read_file_to_writer(path, &mut actual).unwrap();
            assert_eq!(
                actual,
                batch_body(number),
                "power_cut后donor/archive都须实读相同完整正文"
            );
            reads.push(json!({"path":path,"bytes":actual.len(),"sha256":format!("{:x}",Sha256::digest(&actual))}));
        }
        cold_checks.push(json!({"number":number,"source_uid":original_uid,"donor_uid":uid,
            "source_native":source_node,"donor_native":donor,"archive_native":archive,"reads":reads}));
    }
    drop(volume);
    fixture_return_to_storage(&f.library, 71);
    tape_rs::scsi::reservation::release_and_unregister(&dev, key).unwrap();
    // Independently remount the initialized source after power loss, rather
    // than trusting only its FDB result or a stale mounted view.
    f.library.load_into_drive(BARCODES[0], 0).unwrap();
    let dev = f.library.drive_as(0, 71);
    fence(&dev, key).unwrap();
    let empty = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(empty.label().volume_uuid, new_source);
    assert_eq!(empty.index().generation, 1);
    assert!(native_files(empty.index()).is_empty());
    assert_eq!(directory_count(&empty.index().root), 1);
    drop(empty);
    fixture_return_to_storage(&f.library, 71);
    tape_rs::scsi::reservation::release_and_unregister(&dev, key).unwrap();
    let checkpoint_generations: std::collections::BTreeSet<u64> = target_commands
        .iter()
        .filter_map(|command| command["index_generation"].as_u64())
        .collect();
    assert_eq!(
        checkpoint_generations,
        [target_generation + 1, target_generation + 2]
            .into_iter()
            .collect(),
        "实际目标index WRITE只能属于根目录及共同body/archive两代"
    );
    assert_eq!(
        final_head.generation - target_generation,
        2,
        "PhaseA只允许根目录一次+正文与archive共用一次最终S4"
    );
    evidence(
        &f,
        "batch-functional-passed",
        &json!({"original":original.value(),"cold_body_checks":members*2,"cold_checks":cold_checks,
            "original_native_root":source_index.root.meta,"source_volume_before":source.volume_id.unwrap().to_string(),
            "new_source":new_source.to_string(),"completed":completed,"counts":counts,
            "functional_checks_complete":true,"checkpoint_generations":checkpoint_generations,
            "metadata_coverage":"all actual generated managed attributes; no custom non-hash xattrs seeded"}),
    );
}

#[test]
#[ignore = "仅隔离FDB44247+双驱动Sim；1x1MiB production NoneToken完整终局与coldread；不访问硬件"]
fn production_reclaim_one_file_shares_body_and_archive_checkpoint_without_content_token() {
    run_production_reclaim_batch(1, true);
}

#[test]
#[ignore = "仅隔离FDB44247+双驱动Sim；2x1MiB production NoneToken计数RED；不访问硬件"]
fn production_reclaim_two_files_share_bounded_target_commits_without_content_token() {
    run_production_reclaim_batch(2, true);
}

#[test]
#[ignore = "仅隔离FDB44247+双驱动Sim；8x1MiB production NoneToken与完整coldread；不访问硬件"]
fn production_reclaim_eight_files_share_bounded_target_commits_without_content_token() {
    run_production_reclaim_batch(8, false);
}

fn reference_oracle_fixture() -> (FileNode, FileNode, FileNode) {
    use tape_rs::ltfs::index::{Extent, Xattr};
    let mut source = FileNode {
        name: "original".into(),
        length: 10,
        ..Default::default()
    };
    source.meta.file_uid = 2;
    source.meta.readonly = true;
    source.meta.creation_time = "2026-11-17T01:00:00.000000000Z".into();
    source.meta.modify_time = "2026-11-17T02:00:00.000000000Z".into();
    source.meta.change_time = "2026-11-17T03:00:00.000000000Z".into();
    source.meta.access_time = "2026-11-17T04:00:00.000000000Z".into();
    source.meta.backup_time = "2026-11-17T05:00:00.000000000Z".into();
    source.set_xattr(tape_rs::ltfs::index::XATTR_SHA256, &"ab".repeat(32));
    source.xattrs.push(Xattr {
        key: "binary-preserved".into(),
        value: "AP8=".into(),
        base64: true,
    });
    source.extents = vec![Extent {
        partition: 'b',
        start_block: 10,
        byte_offset: 0,
        byte_count: 10,
        file_offset: 0,
    }];
    let mut donor = source.clone();
    donor.name = "donor".into();
    donor.meta.file_uid = 10;
    donor.extents[0].start_block = 100;
    let mut archive = source.clone();
    archive.name = "data-2".into();
    archive.meta.file_uid = 11;
    archive.extents = donor.extents.clone();
    (source, donor, archive)
}

#[test]
fn native_reference_oracle_accepts_only_uid_name_and_location_changes() {
    let (source, donor, archive) = reference_oracle_fixture();
    assert_native_reference(&source, &donor, &archive);
}

#[test]
#[should_panic(expected = "归档必须完整保留")]
fn native_reference_oracle_rejects_lost_nonempty_xattr() {
    let (source, donor, mut archive) = reference_oracle_fixture();
    archive.xattrs.retain(|attr| attr.key != "binary-preserved");
    assert_native_reference(&source, &donor, &archive);
}

#[test]
#[should_panic(expected = "归档必须完整保留")]
fn native_reference_oracle_rejects_changed_native_timestamp() {
    let (source, donor, mut archive) = reference_oracle_fixture();
    archive.meta.backup_time = archive.meta.creation_time.clone();
    assert_native_reference(&source, &donor, &archive);
}

#[test]
#[should_panic(expected = "归档引用必须共享")]
fn native_reference_oracle_rejects_detached_archive_extent() {
    let (source, donor, mut archive) = reference_oracle_fixture();
    archive.extents[0].start_block += 1;
    assert_native_reference(&source, &donor, &archive);
}

#[test]
fn overwrite_observer_distinguishes_partition_page_from_block_descriptor() {
    let mut page = vec![0; 8];
    page.extend_from_slice(&[0x11, 2, 0, 0]);
    assert!(changes_partition_layout(&[0x55; 10], &page));
    let mut descriptor = vec![0; 8];
    descriptor[7] = 8;
    descriptor.extend_from_slice(&[0x11, 0, 0, 0, 0, 0, 0, 0]);
    assert!(!changes_partition_layout(&[0x55; 10], &descriptor));
    assert!(!changes_partition_layout(&[0x0a; 6], &page));
}

// Extracted from the v1 oracle before tightening, so the new negative cases
// exercise the same predicates used by the actual benchmark.
fn private_cluster_bytes_valid(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let Some((description, coordinators)) = text
        .strip_suffix('\n')
        .and_then(|text| text.split_once('@'))
    else {
        return false;
    };
    if description.split(':').count() != 2 || coordinators.split(',').count() != 1 {
        return false;
    }
    coordinators
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|address| address == std::net::SocketAddr::from(([127, 0, 0, 1], 44247)))
        && raw == APPROVED_PRIVATE_CLUSTER
}
fn coverage_schema_matches(
    row: &serde_json::Value,
    inventory: &serde_json::Value,
    uid: u64,
) -> bool {
    let fields = ["version", "parent", "inventory", "source", "proof"];
    let proof_fields = match row["proof"]["kind"].as_str() {
        Some("node") => vec!["kind", "observation"],
        Some("raw_file") => vec!["kind", "attempt"],
        Some("ee_alias") => vec!["kind", "file", "epoch", "request"],
        _ => return false,
    };
    let bytes32 = |value: &serde_json::Value| {
        value.as_array().is_some_and(|values| {
            values.len() == 32 && values.iter().all(|b| b.as_u64().is_some_and(|b| b <= 255))
        })
    };
    row.as_object()
        .is_some_and(|v| v.len() == fields.len() && fields.iter().all(|k| v.contains_key(*k)))
        && row["version"] == 1
        && row["parent"] == inventory["parent"]
        && row["source"]["uid"] == uid
        && row["inventory"] == inventory["summary"]["digest"]
        && bytes32(&row["inventory"])
        && row["proof"].as_object().is_some_and(|v| {
            v.len() == proof_fields.len() && proof_fields.iter().all(|k| v.contains_key(*k))
        })
        && match row["proof"]["kind"].as_str() {
            Some("node") => bytes32(&row["proof"]["observation"]),
            Some("raw_file") => row["proof"]["attempt"].as_str().is_some_and(|s| {
                Uuid::parse_str(s).is_ok_and(|id| !id.is_nil() && id.to_string() == s)
            }),
            Some("ee_alias") => {
                serde_json::from_value::<tape_rs::managed_catalog::ReclaimFile>(
                    row["proof"]["file"].clone(),
                )
                .is_ok()
                    && ["file", "operation"].iter().all(|key| {
                        row["proof"]["file"][*key].as_str().is_some_and(|text| {
                            Uuid::parse_str(text)
                                .is_ok_and(|id| !id.is_nil() && id.to_string() == text)
                        })
                    })
                    && row["proof"]["file"]["version"]
                        .as_u64()
                        .is_some_and(|version| version > 0)
                    && row["proof"]["file"]["path"].as_str().is_some_and(|path| {
                        path != "/"
                            && tape_rs::managed_catalog::pool_catalog_path(
                                Uuid::from_u128(1),
                                path.as_bytes(),
                            )
                            .is_ok()
                    })
                    && bytes32(&row["proof"]["request"])
                    && row["proof"]["epoch"].as_u64().is_some_and(|v| v > 0)
            }
            _ => false,
        }
}
fn first_source_write_valid(record: &serde_json::Value) -> bool {
    record["partition"] == 0
        && record["block"] == 0
        && record["cdb"][0] == 0x0a
        && record["prefix"] == json!([86, 79, 76, 49])
}
fn initial_file_versions_valid(source: u64, target: u64) -> bool {
    source == 1 && target == 2
}
#[test]
fn isolation_guard_rejects_mixed_coordinator_and_port_prefix() {
    for bytes in [
        b"test:id@127.0.0.1:44247,10.131.7.207:45153\n".as_slice(),
        b"test:id@127.0.0.1:442470\n".as_slice(),
    ] {
        assert!(
            !private_cluster_bytes_valid(bytes),
            "must reject mixed/site coordinator or port prefix"
        );
    }
}
#[test]
fn coverage_oracle_rejects_empty_proof_and_wrong_parent() {
    let inventory = json!({"parent":"correct","summary":{"digest":[1,2,3]}});
    let row = json!({"source":{"uid":1},"inventory":[1,2,3],"parent":"wrong","proof":{}});
    assert!(!coverage_schema_matches(&row, &inventory, 1));
}
#[test]
fn first_overwrite_oracle_requires_actual_vol1_payload() {
    let record = json!({"partition":0,"block":0,"cdb":[10,0,0,0,4,0],"prefix":[66,65,68,33]});
    assert!(!first_source_write_valid(&record));
}
#[test]
fn version_oracle_rejects_shifted_initial_version() {
    assert!(!initial_file_versions_valid(2, 3));
}

#[test]
fn isolation_guard_accepts_only_exact_approved_cluster_bytes() {
    assert!(private_cluster_bytes_valid(APPROVED_PRIVATE_CLUSTER));
    for raw in [
        b"wrong:identity@127.0.0.1:44247\n".as_slice(),
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.2:44247\n".as_slice(),
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:45153\n".as_slice(),
    ] {
        assert!(!private_cluster_bytes_valid(raw));
        assert!(PrivateCluster::seal(raw).is_err());
    }
}
#[test]
fn isolation_guard_seals_bytes_before_filename_replacement() {
    let root = std::env::temp_dir().join(format!("reclaim-cluster-seal-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let input = root.join("original.cluster");
    fs::write(&input, APPROVED_PRIVATE_CLUSTER).unwrap();
    let pin = PrivateCluster::open(&input).unwrap();
    let replacement = root.join("replacement.cluster");
    fs::write(&replacement, b"site:identity@10.131.7.207:45153\n").unwrap();
    fs::rename(&replacement, &input).unwrap();
    assert_eq!(fs::read(&pin.path).unwrap(), APPROVED_PRIVATE_CLUSTER);
    assert!(
        pin._sealed
            .try_clone()
            .unwrap()
            .write_all(b"tamper")
            .is_err()
    );
    assert!(PrivateCluster::open(&input).is_err());
    fs::remove_dir_all(&root).unwrap();
}
#[test]
fn coverage_witness_requires_known_exact_schema_but_is_not_a_validity_verdict() {
    let inventory = json!({"parent":"a".repeat(64),"summary":{"digest":vec![1;32]}});
    for proof in [
        json!({"kind":"node","observation":vec![2;32]}),
        json!({"kind":"raw_file","attempt":Uuid::new_v4().to_string()}),
        json!({"kind":"ee_alias","file":{"path":"/file","file":Uuid::new_v4().to_string(),"version":1,"size":10,"operation":Uuid::new_v4().to_string()},"epoch":1,"request":vec![3;32]}),
    ] {
        let good = json!({"version":1,"parent":inventory["parent"],"inventory":inventory["summary"]["digest"],"source":{"uid":1},"proof":proof});
        assert!(coverage_schema_matches(&good, &inventory, 1));
        for (field, value) in [
            ("proof", json!({})),
            ("parent", json!("b".repeat(64))),
            ("version", json!(2)),
            ("inventory", json!(vec![9; 32])),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(!coverage_schema_matches(&bad, &inventory, 1));
        }
        if good["proof"]["kind"] == "ee_alias" {
            let mut bad = good.clone();
            bad["proof"]["file"] = json!({});
            assert!(!coverage_schema_matches(&bad, &inventory, 1));
        }
        let mut bad = good.clone();
        bad["source"]["uid"] = json!(2);
        assert!(!coverage_schema_matches(&bad, &inventory, 1));
        let mut bad = good;
        bad["proof"]["kind"] = json!("unknown_group");
        assert!(!coverage_schema_matches(&bad, &inventory, 1));
    }
}
#[test]
fn vol1_and_version_oracles_accept_exact_initial_contract() {
    assert!(first_source_write_valid(
        &json!({"partition":0,"block":0,"cdb":[10,0,0,0,80,0],"prefix":[86,79,76,49]})
    ));
    assert!(initial_file_versions_valid(1, 2));
    assert!(!initial_file_versions_valid(0, 1));
}
