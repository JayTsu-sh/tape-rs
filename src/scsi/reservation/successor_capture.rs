//! 只读采集固定 B 库路径上的原始 PR 证据。

use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix::sys::stat::{SFlag, fstat, major, minor};
use serde::{Deserialize, Serialize};

use super::{PR_TIMEOUT_MS, PR_TYPE_EXCLUSIVE_ACCESS, ReservationKey};
use crate::error::{Result, TapeError};
use crate::scsi::cdb;
use crate::scsi::device::{RawCompletion, ReadOnlyCapture, ScsiDevice, ScsiResult};
use crate::scsi::inquiry::normalize_fc_target_port;

const PROFILE: &str = "b-fc-v1";
const SCHEMA: &str = "fresh.stopped.sg-pr.capture.v1";
const VPD_ALLOC: u16 = 4096;
const PR_ALLOC: u16 = 1024;
const VPD_TIMEOUT_MS: u32 = 10_000;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum Host {
    #[serde(rename = "207")]
    Host207,
    #[serde(rename = "208")]
    Host208,
    #[serde(rename = "209")]
    Host209,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Purpose {
    Paths,
    Pr,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Actor {
    host: Host,
    node: u8,
    round: u64,
}

/// 采集请求中的主机与 actor 由外部证据链核实。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    version: u32,
    profile: String,
    transition_id: String,
    challenge: String,
    host: Host,
    purpose: Purpose,
    actor: Actor,
}

impl CaptureRequest {
    fn key(&self) -> CaptureResult<ReservationKey> {
        if self.version != 1 || self.profile != PROFILE {
            return Err(Failure::unsupported("采集请求版本或 profile 不受支持"));
        }
        for (name, value) in [
            ("transition_id", &self.transition_id),
            ("challenge", &self.challenge),
        ] {
            if !uuid::Uuid::parse_str(value)
                .is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == *value)
            {
                return Err(Failure::invalid(format!("{name} 需要规范非空 UUID")));
            }
        }
        if self.purpose == Purpose::Pr && self.host != self.actor.host {
            return Err(Failure::invalid("PR 采集须在原 actor 所在主机"));
        }
        if self.actor.node == 0 || !(1..=0xffff_ffff_ffff).contains(&self.actor.round) {
            return Err(Failure::invalid("原 actor 的节点号或预留轮次越界"));
        }
        Ok(ReservationKey::new(self.actor.node, self.actor.round))
    }
}

#[derive(Clone, Copy, Debug)]
struct DeviceSpec {
    role: &'static str,
    node: &'static str,
    serial: &'static str,
    pdt: u8,
    target: u64,
}

fn devices(host: Host) -> [DeviceSpec; 3] {
    let [changer, drive0, drive1] = match host {
        Host::Host207 | Host::Host208 => ["sg5", "sg4", "sg6"],
        Host::Host209 => ["sg7", "sg6", "sg4"],
    };
    [
        DeviceSpec {
            role: "changer",
            node: changer,
            serial: "55L3A78011W9LL01",
            pdt: 8,
            target: 0x5000e1117018605d,
        },
        DeviceSpec {
            role: "drive0",
            node: drive0,
            serial: "117018605B",
            pdt: 1,
            target: 0x5000e1117018605d,
        },
        DeviceSpec {
            role: "drive1",
            node: drive1,
            serial: "117018606F",
            pdt: 1,
            target: 0x5000e11170186070,
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Classification {
    PathsComplete,
    CompleteEmpty,
    ObservedSingleton,
    Unsupported,
    Invalid,
    IoIncomplete,
}

impl Classification {
    fn exit_code(self) -> u8 {
        match self {
            Self::PathsComplete | Self::CompleteEmpty | Self::ObservedSingleton => 0,
            Self::Invalid => 2,
            Self::Unsupported => 3,
            Self::IoIncomplete => 4,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Failure {
    classification: Classification,
    stage: &'static str,
    role: Option<&'static str>,
    detail: String,
    errno: Option<i32>,
}

type CaptureResult<T> = std::result::Result<T, Failure>;

impl Failure {
    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            classification: Classification::Invalid,
            stage: "request",
            role: None,
            detail: detail.into(),
            errno: None,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            classification: Classification::Unsupported,
            ..Self::invalid(detail)
        }
    }

    fn source(error: TapeError) -> Self {
        let errno = errno(&error);
        let classification = match error {
            TapeError::Refused { .. } | TapeError::InvalidResponse { .. } => {
                Classification::Invalid
            }
            _ => Classification::IoIncomplete,
        };
        Self {
            classification,
            errno,
            ..Self::invalid(error.to_string())
        }
    }

    fn filesystem(path: &Path, error: std::io::Error) -> Self {
        Self {
            detail: format!("{}: {error}", path.display()),
            ..Self::source(error.into())
        }
    }

    fn at(mut self, stage: &'static str, role: &'static str) -> Self {
        self.stage = stage;
        self.role = Some(role);
        self
    }
}

fn errno(error: &TapeError) -> Option<i32> {
    match error {
        TapeError::Nix(errno) => Some(*errno as i32),
        TapeError::Io(error) => error.raw_os_error(),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 15) as usize] as char);
    }
    encoded
}

#[derive(Debug, Serialize)]
pub struct CaptureOutcome {
    schema: &'static str,
    version: u32,
    request: Option<CaptureRequest>,
    classification: Classification,
    stage: &'static str,
    failure: Option<Failure>,
    devices: Vec<DeviceEvidence>,
}

impl CaptureOutcome {
    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA,
            version: 1,
            request: None,
            classification: Classification::Invalid,
            stage: "request",
            failure: Some(Failure::invalid(detail)),
            devices: Vec::new(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        self.classification.exit_code()
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.failure.as_ref().map(|failure| failure.detail.as_str())
    }
}

#[derive(Debug, Serialize)]
struct DeviceEvidence {
    role: &'static str,
    node: &'static str,
    serial: &'static str,
    preflight_path: Option<PathIdentity>,
    before_path: Option<PathIdentity>,
    last_path: Option<PathIdentity>,
    after_path: Option<PathIdentity>,
    before_vpd: Option<VpdIdentity>,
    after_vpd: Option<VpdIdentity>,
    reads: Vec<ReadEvidence>,
    observation: Option<PrObservation>,
}

impl From<DeviceSpec> for DeviceEvidence {
    fn from(spec: DeviceSpec) -> Self {
        Self {
            role: spec.role,
            node: spec.node,
            serial: spec.serial,
            preflight_path: None,
            before_path: None,
            last_path: None,
            after_path: None,
            before_vpd: None,
            after_vpd: None,
            reads: Vec::new(),
            observation: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    mode: u32,
    rdev: u64,
}

impl FileIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            rdev: metadata.rdev(),
        }
    }

    fn scsi_generic(&self) -> bool {
        SFlag::from_bits_truncate(self.mode) == SFlag::S_IFCHR && major(self.rdev) == 21
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SysfsObject {
    path: String,
    dev: u64,
    ino: u64,
}

impl SysfsObject {
    fn of(path: &Path) -> CaptureResult<Self> {
        let name = path
            .to_str()
            .ok_or_else(|| Failure::unsupported("sysfs 路径无法编码"))?;
        let metadata = path.metadata().map_err(|e| Failure::filesystem(path, e))?;
        Ok(Self {
            path: name.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PathIdentity {
    fd_stat: FileIdentity,
    sg: SysfsObject,
    scsi_device: SysfsObject,
    fc_host: SysfsObject,
    fc_rport: SysfsObject,
    hctl: String,
    pdt: u8,
    initiator_port: String,
    target_port: String,
    cached_vpd80_hex: String,
}

fn opened_stat(device: &ReadOnlyCapture) -> Result<FileIdentity> {
    let stat = fstat(device.fd().as_raw_fd())?;
    Ok(FileIdentity {
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        rdev: stat.st_rdev,
    })
}

fn dev_stat(node: &str) -> CaptureResult<FileIdentity> {
    let path = Path::new("/dev").join(node);
    let stat =
        FileIdentity::of(&fs::symlink_metadata(&path).map_err(|e| Failure::filesystem(&path, e))?);
    if !stat.scsi_generic() {
        return Err(Failure::invalid("固定节点不是 SCSI generic 字符设备"));
    }
    Ok(stat)
}

fn sysfs_bytes(path: &Path, limit: usize) -> CaptureResult<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| Failure::filesystem(path, e))?;
    if bytes.len() > limit {
        return Err(Failure::unsupported("sysfs 属性长度越界"));
    }
    Ok(bytes)
}

fn sysfs_name(path: &Path) -> CaptureResult<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Failure::unsupported("sysfs 对象缺少规范名称"))
}

fn decimal(text: &str) -> bool {
    text.parse::<u64>().is_ok_and(|n| n.to_string() == text)
}

fn port_name(path: &Path) -> CaptureResult<String> {
    let bytes = sysfs_bytes(&path.join("port_name"), 32)?;
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .filter(|text| text.starts_with("0x"))
        .and_then(|text| normalize_fc_target_port(text).ok())
        .filter(|port| port != "0xffffffffffffffff")
        .ok_or_else(|| Failure::unsupported("FC port_name 格式或数值异常"))
}

fn resolve_sysfs(root: &Path, spec: DeviceSpec, stat: FileIdentity) -> CaptureResult<PathIdentity> {
    let canonical = |path: &Path| {
        path.canonicalize()
            .map_err(|e| Failure::filesystem(path, e))
    };
    let class = root.join("class");
    let dev_char = root
        .join("dev/char")
        .join(format!("{}:{}", major(stat.rdev), minor(stat.rdev)));
    let sg = canonical(&dev_char)?;
    let generic_class = class.join("scsi_generic");
    if !stat.scsi_generic()
        || !sg.starts_with(root.join("devices"))
        || sysfs_name(&sg)? != spec.node
        || canonical(&generic_class.join(spec.node))? != sg
        || canonical(&sg.join("subsystem"))? != generic_class
    {
        return Err(Failure::invalid("fd 设备号与 SCSI generic 类对象不符"));
    }
    let lu = canonical(&sg.join("device"))?;
    if sg != lu.join("scsi_generic").join(spec.node) {
        return Err(Failure::invalid("SCSI generic 类对象与 LU 父对象不符"));
    }
    let hctl = sysfs_name(&lu)?;
    let fields: Vec<_> = hctl.split(':').collect();
    let [h, c, t, l] = fields.as_slice() else {
        return Err(Failure::unsupported("缺少规范 H:C:T:L"));
    };
    if ![h, c, t, l].into_iter().all(|field| decimal(field)) {
        return Err(Failure::unsupported("H:C:T:L 字段格式异常"));
    }
    fn parent(path: &Path) -> CaptureResult<&Path> {
        path.parent()
            .ok_or_else(|| Failure::unsupported("SCSI LU 缺少 FC 路径父对象"))
    }
    let target = parent(&lu)?;
    let rport = parent(target)?;
    let host = parent(rport)?;
    let rport_name = sysfs_name(rport)?;
    let host_name = format!("host{h}");
    if sysfs_name(target)? != format!("target{h}:{c}:{t}")
        || !rport_name
            .strip_prefix(&format!("rport-{h}:{c}-"))
            .is_some_and(decimal)
        || sysfs_name(host)? != host_name
    {
        return Err(Failure::unsupported("H:C:T:L 与 FC host/rport 父链不符"));
    }
    let fc_host = canonical(&class.join("fc_host").join(&host_name))?;
    let fc_rport = canonical(&class.join("fc_remote_ports").join(rport_name))?;
    if fc_host != host.join("fc_host").join(host_name)
        || fc_rport != rport.join("fc_remote_ports").join(rport_name)
    {
        return Err(Failure::invalid("FC 类对象不属于该 LU 的 host/rport 父链"));
    }
    let device_type = sysfs_bytes(&lu.join("type"), 8)?;
    let pdt = std::str::from_utf8(&device_type)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .filter(|text| decimal(text))
        .and_then(|text| text.parse::<u8>().ok())
        .ok_or_else(|| Failure::unsupported("sysfs SCSI 设备类型无法确认"))?;
    let cached_vpd80 = sysfs_bytes(&lu.join("vpd_pg80"), usize::from(VPD_ALLOC))?;
    Ok(PathIdentity {
        fd_stat: stat,
        sg: SysfsObject::of(&sg)?,
        scsi_device: SysfsObject::of(&lu)?,
        fc_host: SysfsObject::of(&fc_host)?,
        fc_rport: SysfsObject::of(&fc_rport)?,
        hctl: hctl.to_owned(),
        pdt,
        initiator_port: port_name(&fc_host)?,
        target_port: port_name(&fc_rport)?,
        cached_vpd80_hex: hex(&cached_vpd80),
    })
}

fn vpd80_identity(path: &PathIdentity, spec: DeviceSpec) -> CaptureResult<()> {
    let mut expected_vpd80 = vec![spec.pdt, 0x80, 0, spec.serial.len() as u8];
    expected_vpd80.extend_from_slice(spec.serial.as_bytes());
    if path.pdt != spec.pdt
        || path.cached_vpd80_hex != hex(&expected_vpd80)
        || path.target_port != format!("0x{:016x}", spec.target)
    {
        return Err(Failure::invalid(
            "缓存设备类型、VPD 80 序列号或目标 WWPN 与固定角色不符",
        ));
    }
    Ok(())
}

struct SgPath {
    spec: DeviceSpec,
    device: ReadOnlyCapture,
}

impl SgPath {
    fn open(
        spec: DeviceSpec,
        evidence: &mut DeviceEvidence,
    ) -> CaptureResult<(Self, PathIdentity)> {
        use nix::sys::statfs::{SYSFS_MAGIC, statfs};
        let sys = Path::new("/sys");
        if statfs(sys)
            .map_err(|e| Failure::source(e.into()))?
            .filesystem_type()
            != SYSFS_MAGIC
        {
            return Err(Failure::invalid("采集路径需要 Linux sysfs"));
        }
        let expected = resolve_sysfs(sys, spec, dev_stat(spec.node)?)?;
        evidence.preflight_path = Some(expected.clone());
        // 在打开节点前排除错角色。打开后再次以实际 fd 核实相同映射。
        vpd80_identity(&expected, spec)?;
        let device = ScsiDevice::open_capture_readonly(&format!("/dev/{}", spec.node))
            .map_err(Failure::source)?;
        let path = Self { spec, device };
        let actual = path.binding()?;
        evidence.before_path = Some(actual.clone());
        require_stable(&expected, &actual)?;
        Ok((path, actual))
    }
}

trait CapturePath {
    fn binding(&self) -> CaptureResult<PathIdentity>;
    fn read(
        &self,
        command: &ReadCommand,
        buffer: &mut [u8],
    ) -> (Option<RawCompletion>, Result<ScsiResult>);
}

impl CapturePath for SgPath {
    fn binding(&self) -> CaptureResult<PathIdentity> {
        let stat = opened_stat(&self.device).map_err(Failure::source)?;
        if stat != dev_stat(self.spec.node)? {
            return Err(Failure::invalid("实际 fd 与固定节点身份已分离"));
        }
        resolve_sysfs(Path::new("/sys"), self.spec, stat)
    }

    fn read(
        &self,
        command: &ReadCommand,
        buffer: &mut [u8],
    ) -> (Option<RawCompletion>, Result<ScsiResult>) {
        self.device
            .read_capture(&command.cdb, buffer, command.timeout_ms)
    }
}

fn require_stable(before: &PathIdentity, current: &PathIdentity) -> CaptureResult<()> {
    if before != current {
        return Err(Failure::invalid("采集期间 fd 或 sysfs/FC 路径身份发生变化"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceOp {
    Vpd80,
    Vpd83,
    ReadKeys,
    ReadReservation,
    ReadFullStatus,
}

struct ReadCommand {
    op: EvidenceOp,
    cdb: Vec<u8>,
    allocation: usize,
    timeout_ms: u32,
}

impl EvidenceOp {
    fn command(self) -> ReadCommand {
        let (cdb, allocation, timeout_ms) = match self {
            Self::Vpd80 => (
                cdb::inquiry_vpd(0x80, VPD_ALLOC).to_vec(),
                VPD_ALLOC,
                VPD_TIMEOUT_MS,
            ),
            Self::Vpd83 => (
                cdb::inquiry_vpd(0x83, VPD_ALLOC).to_vec(),
                VPD_ALLOC,
                VPD_TIMEOUT_MS,
            ),
            Self::ReadKeys => (
                cdb::persistent_reserve_in(0x00, PR_ALLOC).to_vec(),
                PR_ALLOC,
                PR_TIMEOUT_MS,
            ),
            Self::ReadReservation => (
                cdb::persistent_reserve_in(0x01, PR_ALLOC).to_vec(),
                PR_ALLOC,
                PR_TIMEOUT_MS,
            ),
            Self::ReadFullStatus => (
                cdb::persistent_reserve_in(0x03, PR_ALLOC).to_vec(),
                PR_ALLOC,
                PR_TIMEOUT_MS,
            ),
        };
        ReadCommand {
            op: self,
            cdb,
            allocation: usize::from(allocation),
            timeout_ms,
        }
    }
}

#[derive(Debug, Serialize)]
struct ReadEvidence {
    stage: &'static str,
    operation: EvidenceOp,
    fd_stat: FileIdentity,
    cdb_hex: String,
    allocation: usize,
    timeout_ms: u32,
    raw_completion: Option<RawCompletion>,
    transferred: Option<usize>,
    response_hex: Option<String>,
    errno: Option<i32>,
    error: Option<String>,
    binding_failure: Option<Failure>,
}

fn valid_transfer(raw: &RawCompletion, allocation: usize) -> Option<usize> {
    if raw.requested_len as usize != allocation {
        return None;
    }
    allocation.checked_sub(usize::try_from(raw.resid).ok()?)
}

fn strict_completion(record: &ReadEvidence, result: Result<ScsiResult>) -> CaptureResult<usize> {
    if let Err(error) = result {
        return Err(if super::is_unsupported(&error) {
            Failure::unsupported(error.to_string())
        } else {
            Failure::source(error)
        });
    }
    let Some(raw) = &record.raw_completion else {
        return Err(Failure::invalid("SG_IO 缺少原始完成记录"));
    };
    if raw.status != 0
        || raw.masked_status != 0
        || raw.msg_status != 0
        || raw.host_status != 0
        || raw.driver_status != 0
        || raw.info != 0
        || raw.sb_len_wr != 0
        || !raw.sense_bytes.is_empty()
    {
        return Err(Failure::unsupported(
            "只读采集收到非 GOOD 或带 sense 的 SG_IO 完成",
        ));
    }
    record
        .transferred
        .ok_or_else(|| Failure::invalid("SG_IO 有符号残差或请求长度异常"))
}

fn verify_binding(path: &impl CapturePath, evidence: &mut DeviceEvidence) -> CaptureResult<()> {
    let current = path.binding()?;
    let result = require_stable(
        evidence.before_path.as_ref().expect("已核对打开的路径"),
        &current,
    );
    evidence.last_path = Some(current);
    result
}

fn read(
    path: &impl CapturePath,
    evidence: &mut DeviceEvidence,
    stage: &'static str,
    op: EvidenceOp,
) -> CaptureResult<Vec<u8>> {
    let role = evidence.role;
    (|| {
        verify_binding(path, evidence)?;
        let command = op.command();
        let mut buffer = vec![0; command.allocation];
        let (raw, result) = path.read(&command, &mut buffer);
        let transferred = raw.as_ref().and_then(|raw| {
            // 传输层失败时保留原 resid，此时无法确认响应缓冲区中的实际传输。
            if raw.host_status != 0 || !matches!(raw.driver_status, 0 | 8) {
                return None;
            }
            valid_transfer(raw, buffer.len())
        });
        let mut record = ReadEvidence {
            stage,
            operation: command.op,
            fd_stat: evidence
                .before_path
                .as_ref()
                .expect("已核对打开的路径")
                .fd_stat
                .clone(),
            cdb_hex: hex(&command.cdb),
            allocation: command.allocation,
            timeout_ms: command.timeout_ms,
            raw_completion: raw,
            transferred,
            response_hex: transferred.map(|len| hex(&buffer[..len])),
            errno: result.as_ref().err().and_then(errno),
            error: result.as_ref().err().map(ToString::to_string),
            binding_failure: None,
        };
        let validated = strict_completion(&record, result);
        // 即使命令失败也检查路径，原命令错误和原始完成记录仍留在对应读记录。
        let binding = verify_binding(path, evidence).map_err(|e| e.at("post_command_path", role));
        record.binding_failure = binding.as_ref().err().cloned();
        evidence.reads.push(record);
        buffer.truncate(validated?);
        binding?;
        Ok(buffer)
    })()
    .map_err(|e: Failure| e.at(stage, role))
}

fn vpd_payload(bytes: &[u8], spec: DeviceSpec, page: u8) -> CaptureResult<&[u8]> {
    let Some((header, payload)) = bytes.split_first_chunk::<4>() else {
        return Err(Failure::unsupported("VPD 响应头截断"));
    };
    if header[0] != spec.pdt
        || header[1] != page
        || usize::from(u16::from_be_bytes([header[2], header[3]])) != payload.len()
    {
        return Err(Failure::unsupported(
            "VPD 设备类型、页号或实际长度与响应头不符",
        ));
    }
    Ok(payload)
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct LuNaa {
    kind: &'static str,
    value_hex: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct VpdIdentity {
    lu_id: LuNaa,
    target_port: String,
    relative_target_port: u16,
}

fn set_once<T>(value: &mut Option<T>, found: T) -> CaptureResult<()> {
    if value.replace(found).is_some() {
        return Err(Failure::unsupported("VPD 83 相关描述符重复或冲突"));
    }
    Ok(())
}

fn vpd83_identity(bytes: &[u8], spec: DeviceSpec) -> CaptureResult<VpdIdentity> {
    let mut remaining = vpd_payload(bytes, spec, 0x83)?;
    let mut lu = None;
    let mut port = None;
    let mut rtpi = None;
    while !remaining.is_empty() {
        let (header, body) = remaining
            .split_first_chunk::<4>()
            .ok_or_else(|| Failure::unsupported("VPD 83 描述符头截断"))?;
        let (value, tail) = body
            .split_at_checked(usize::from(header[3]))
            .ok_or_else(|| Failure::unsupported("VPD 83 描述符长度越界"))?;
        remaining = tail;
        // 只采纳明确的 LU NAA、FC target-port NAA 及同一目标的 RTPI。
        // 非采纳的辅助标识也只接受其作用域与基本格式明确的形状。
        match (header[0], header[1], header[2], value) {
            (0x01, 0x03, 0, value @ [first, ..]) if value.len() == 8 && first >> 4 == 5 => {
                set_once(&mut lu, hex(value))?;
            }
            (0x01, 0x93, 0, value @ [first, ..]) if value.len() == 8 && first >> 4 == 5 => {
                set_once(&mut port, format!("0x{}", hex(value)))?;
            }
            (0x01, 0x94, 0, [0, 0, hi, lo]) if *hi != 0 || *lo != 0 => {
                set_once(&mut rtpi, u16::from_be_bytes([*hi, *lo]))?;
            }
            (0x02, 0x01 | 0x21, 0, value)
                if value.len() >= 8 && value.iter().all(|byte| (b' '..=b'~').contains(byte)) => {}
            (0x01, 0x23 | 0xa3, 0, value @ [first, ..])
                if matches!((first >> 4, value.len()), (2 | 5, 8) | (6, 16)) => {}
            _ => return Err(Failure::unsupported("VPD 83 描述符超出可核实的 FC 子集")),
        }
    }
    let (Some(lu), Some(port), Some(relative_target_port)) = (lu, port, rtpi) else {
        return Err(Failure::unsupported(
            "VPD 83 缺少唯一 LU NAA、FC 目标 WWPN 或 RTPI",
        ));
    };
    if port != format!("0x{:016x}", spec.target) {
        return Err(Failure::unsupported(
            "VPD 83 目标 WWPN 与同 fd 的 FC 路径不符",
        ));
    }
    Ok(VpdIdentity {
        lu_id: LuNaa {
            kind: "vpd83-fc-naa8",
            value_hex: lu,
        },
        target_port: port,
        relative_target_port,
    })
}

fn read_vpd(
    path: &impl CapturePath,
    evidence: &mut DeviceEvidence,
    spec: DeviceSpec,
    stage: &'static str,
) -> CaptureResult<VpdIdentity> {
    let serial = read(path, evidence, stage, EvidenceOp::Vpd80)?;
    let payload = vpd_payload(&serial, spec, 0x80).map_err(|e| e.at(stage, spec.role))?;
    if payload != spec.serial.as_bytes() {
        return Err(Failure::invalid("同 fd 的 VPD 80 序列号与固定角色不符").at(stage, spec.role));
    }
    let id = read(path, evidence, stage, EvidenceOp::Vpd83)?;
    vpd83_identity(&id, spec).map_err(|e| e.at(stage, spec.role))
}

fn pr_payload(bytes: &[u8]) -> CaptureResult<(u32, &[u8])> {
    let (header, payload) = bytes
        .split_first_chunk::<8>()
        .ok_or_else(|| Failure::unsupported("PR IN 响应头截断"))?;
    let [a, b, c, d, e, f, g, h] = *header;
    if u32::from_be_bytes([e, f, g, h]) as usize != payload.len() {
        return Err(Failure::unsupported("PR IN 响应的声明长度与实际长度不符"));
    }
    Ok((u32::from_be_bytes([a, b, c, d]), payload))
}

fn full_status_descriptors(mut payload: &[u8]) -> CaptureResult<Vec<(&[u8; 24], &[u8])>> {
    let mut descriptors = Vec::new();
    while !payload.is_empty() {
        let (header, body) = payload
            .split_first_chunk::<24>()
            .ok_or_else(|| Failure::unsupported("READ FULL STATUS 描述符头截断"))?;
        let length = u32::from_be_bytes(header[20..24].try_into().expect("四字节长度字段"));
        let (transport, tail) = body
            .split_at_checked(length as usize)
            .ok_or_else(|| Failure::unsupported("READ FULL STATUS 描述符的 TransportID 截断"))?;
        descriptors.push((header, transport));
        payload = tail;
    }
    Ok(descriptors)
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct PrObservation {
    classification: Classification,
    generation: u32,
    key_hex: Option<String>,
    relative_target_port: Option<u16>,
    initiator_port: Option<String>,
}

fn observe_round(
    round: &[Vec<u8>; 3],
    key: ReservationKey,
    relative_target_port: u16,
    initiator_port: &str,
) -> CaptureResult<PrObservation> {
    let [keys_response, reservation_response, full_status] = round;
    let (generation, keys) = pr_payload(keys_response)?;
    let (res_generation, reservation) = pr_payload(reservation_response)?;
    let (full_generation, full) = pr_payload(full_status)?;
    if generation != res_generation || generation != full_generation {
        return Err(Failure::unsupported("同轮 PR IN 的 generation 不一致"));
    }
    let descriptors = full_status_descriptors(full)?;
    if keys.is_empty() && reservation.is_empty() && descriptors.is_empty() {
        return Ok(PrObservation {
            classification: Classification::CompleteEmpty,
            generation,
            key_hex: None,
            relative_target_port: None,
            initiator_port: None,
        });
    }
    let [(holder, transport)] = descriptors.as_slice() else {
        return Err(Failure::unsupported(
            "READ FULL STATUS 无法确认唯一注册 nexus",
        ));
    };
    let key_bytes = key.0.to_be_bytes();
    let mut expected_reservation = [0; 16];
    expected_reservation[..8].copy_from_slice(&key_bytes);
    expected_reservation[13] = PR_TYPE_EXCLUSIVE_ACCESS;
    let Ok(transport) = <&[u8; 24]>::try_from(*transport) else {
        return Err(Failure::unsupported(
            "TransportID 无法确认为 24 字节的 FCP 形状",
        ));
    };
    let port = format!("0x{}", hex(&transport[8..16]));
    if keys != key_bytes || reservation != expected_reservation
        || holder[..8] != key_bytes || holder[8..12] != [0; 4]
        || holder[12] != 1 // R_HOLDER=1, ALL_TG_PT=0
        || holder[13] != PR_TYPE_EXCLUSIVE_ACCESS || holder[14..18] != [0; 4]
        || holder[18..20] != relative_target_port.to_be_bytes()
        || transport[..8] != [0; 8]
        || transport[8..16] == [0; 8]
        || transport[16..] != [0; 8]
        || port != initiator_port
    {
        return Err(Failure::unsupported(
            "PR 记录未共同证明原 actor 的 FC 单一 Exclusive Access 预留",
        ));
    }
    Ok(PrObservation {
        classification: Classification::ObservedSingleton,
        generation,
        key_hex: Some(hex(&key_bytes)),
        relative_target_port: Some(relative_target_port),
        initiator_port: Some(port),
    })
}

fn read_round(
    path: &impl CapturePath,
    evidence: &mut DeviceEvidence,
    stage: &'static str,
) -> CaptureResult<[Vec<u8>; 3]> {
    Ok([
        read(path, evidence, stage, EvidenceOp::ReadKeys)?,
        read(path, evidence, stage, EvidenceOp::ReadReservation)?,
        read(path, evidence, stage, EvidenceOp::ReadFullStatus)?,
    ])
}

fn read_pr(
    path: &impl CapturePath,
    evidence: &mut DeviceEvidence,
    key: ReservationKey,
) -> CaptureResult<PrObservation> {
    let first = read_round(path, evidence, "pr_first")?;
    let second = read_round(path, evidence, "pr_second")?;
    let role = evidence.role;
    let rtpi = evidence
        .before_vpd
        .as_ref()
        .expect("VPD 预检完成")
        .relative_target_port;
    let initiator = &evidence
        .before_path
        .as_ref()
        .expect("路径预检完成")
        .initiator_port;
    let first_observation =
        observe_round(&first, key, rtpi, initiator).map_err(|e| e.at("pr_first", role))?;
    let second_observation =
        observe_round(&second, key, rtpi, initiator).map_err(|e| e.at("pr_second", role))?;
    if first_observation != second_observation {
        return Err(
            Failure::unsupported("两轮完整 PR IN 的 generation 或预留状态不同")
                .at("pr_stability", role),
        );
    }
    Ok(first_observation)
}

fn distinct_path(first: &PathIdentity, second: &PathIdentity) -> bool {
    first.fd_stat.rdev != second.fd_stat.rdev
        && first.sg != second.sg
        && first.scsi_device != second.scsi_device
        && first.hctl != second.hctl
}

fn acquire<P: CapturePath>(
    request: &CaptureRequest,
    evidence: &mut [DeviceEvidence],
    specs: &[DeviceSpec],
    mut open: impl FnMut(DeviceSpec, &mut DeviceEvidence) -> CaptureResult<(P, PathIdentity)>,
) -> CaptureResult<Classification> {
    let key = request.key()?;
    let mut paths = Vec::with_capacity(specs.len());
    for (i, &spec) in specs.iter().enumerate() {
        let (path, identity) =
            open(spec, &mut evidence[i]).map_err(|e| e.at("open_path", spec.role))?;
        let unique = evidence[..i].iter().all(|device| {
            distinct_path(
                device.before_path.as_ref().expect("已打开的路径"),
                &identity,
            )
        });
        evidence[i].before_path = Some(identity);
        vpd80_identity(
            evidence[i].before_path.as_ref().expect("已打开的路径"),
            spec,
        )
        .map_err(|e| e.at("path_preflight", spec.role))?;
        if !unique {
            return Err(Failure::invalid("两个固定角色别名至同一 SCSI 路径")
                .at("path_preflight", spec.role));
        }
        paths.push(path);
    }
    for (i, (path, &spec)) in paths.iter().zip(specs).enumerate() {
        let identity = read_vpd(path, &mut evidence[i], spec, "vpd_before")?;
        let unique = evidence[..i].iter().all(|device| {
            device.before_vpd.as_ref().expect("已读取 VPD 的路径").lu_id != identity.lu_id
        });
        evidence[i].before_vpd = Some(identity);
        if !unique {
            return Err(Failure::invalid("两个固定角色报告相同 LU NAA").at("vpd_before", spec.role));
        }
    }
    let mut observations = Vec::new();
    if request.purpose == Purpose::Pr {
        for (path, device) in paths.iter().zip(evidence.iter_mut()) {
            observations.push(read_pr(path, device, key)?);
        }
    }
    for ((path, device), &spec) in paths.iter().zip(evidence.iter_mut()).zip(specs) {
        let identity = read_vpd(path, device, spec, "vpd_after")?;
        device.after_vpd = Some(identity);
        if device.before_vpd != device.after_vpd {
            return Err(Failure::invalid("采集前后 VPD 83 身份不同").at("vpd_after", spec.role));
        }
    }
    for (path, device) in paths.iter().zip(evidence.iter_mut()) {
        verify_binding(path, device).map_err(|e| e.at("path_after", device.role))?;
        device.after_path = device.last_path.take();
    }
    let classification = if request.purpose == Purpose::Paths {
        Classification::PathsComplete
    } else if observations
        .iter()
        .any(|observation| observation.classification == Classification::ObservedSingleton)
    {
        Classification::ObservedSingleton
    } else {
        Classification::CompleteEmpty
    };
    for (device, observation) in evidence.iter_mut().zip(observations) {
        device.observation = Some(observation);
    }
    Ok(classification)
}

fn capture_with<P: CapturePath>(
    request: CaptureRequest,
    open: impl FnMut(DeviceSpec, &mut DeviceEvidence) -> CaptureResult<(P, PathIdentity)>,
) -> CaptureOutcome {
    let specs = devices(request.host);
    let mut devices = specs
        .into_iter()
        .map(DeviceEvidence::from)
        .collect::<Vec<_>>();
    let (classification, stage, failure) = match acquire(&request, &mut devices, &specs, open) {
        Ok(classification) => (classification, "complete", None),
        Err(failure) => (failure.classification, failure.stage, Some(failure)),
    };
    CaptureOutcome {
        schema: SCHEMA,
        version: 1,
        request: Some(request),
        classification,
        stage,
        devices,
        failure,
    }
}

/// 通过只读 SG fd 采集固定 B 库的诊断原件。
pub fn capture_b_pr_evidence(request: CaptureRequest) -> CaptureOutcome {
    capture_with(request, SgPath::open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::device::finish_command;
    use crate::scsi::sense::SenseInfo;
    use crate::scsi::sim::SimLibrary;
    use crate::scsi::transport::TapeTransport;
    use nix::sys::stat::makedev;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    const INITIATOR: u64 = 0x10000090fa123456;
    const RTPI: u16 = 257;
    const GEN: u32 = 19;
    const KEY: ReservationKey = ReservationKey(0x4c03000000008cab);

    fn request(purpose: Purpose) -> CaptureRequest {
        CaptureRequest {
            version: 1,
            profile: PROFILE.to_string(),
            transition_id: "c869501b-9e9d-498c-a60b-5756251303eb".to_owned(),
            challenge: "12f9a603-cfc6-443a-9958-9cc78c8830da".to_owned(),
            host: Host::Host209,
            purpose,
            actor: Actor {
                host: Host::Host209,
                node: 3,
                round: 36011,
            },
        }
    }

    fn vpd(spec: DeviceSpec, page: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![spec.pdt, page];
        bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn vpd83(spec: DeviceSpec, lu_suffix: u8) -> Vec<u8> {
        let mut payload = vec![1, 3, 0, 8, 0x50, 1, 2, 3, 4, 5, 6, lu_suffix];
        payload.extend_from_slice(&[1, 0x93, 0, 8]);
        payload.extend_from_slice(&spec.target.to_be_bytes());
        payload.extend_from_slice(&[1, 0x94, 0, 4, 0, 0, 1, 1]);
        vpd(spec, 0x83, &payload)
    }

    fn pr(generation: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = generation.to_be_bytes().to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn pr_round(singleton: bool, generation: u32) -> [Vec<u8>; 3] {
        if !singleton {
            return std::array::from_fn(|_| pr(generation, &[]));
        }
        let mut reservation = [0; 16];
        reservation[..8].copy_from_slice(&KEY.0.to_be_bytes());
        reservation[13] = 3;
        let mut full = [0; 48];
        full[..8].copy_from_slice(&KEY.0.to_be_bytes());
        full[12] = 1;
        full[13] = 3;
        full[18..20].copy_from_slice(&RTPI.to_be_bytes());
        full[23] = 24;
        full[32..40].copy_from_slice(&INITIATOR.to_be_bytes());
        [
            pr(generation, &KEY.0.to_be_bytes()),
            pr(generation, &reservation),
            pr(generation, &full),
        ]
    }

    fn completed(command: &ReadCommand, transferred: usize) -> RawCompletion {
        RawCompletion {
            status: 0,
            masked_status: 0,
            msg_status: 0,
            host_status: 0,
            driver_status: 0,
            info: 0,
            duration_ms: 3,
            sb_len_wr: 0,
            sense_bytes: Vec::new(),
            resid: (command.allocation - transferred) as i32,
            requested_len: command.allocation as u32,
        }
    }

    #[derive(Clone)]
    enum Fault {
        Resid(i32),
        UnitAttention,
        IllegalRequest,
        Host,
        Driver,
        CheckWithoutSense,
        Message,
        Ioctl,
    }

    #[derive(Clone)]
    struct Frame {
        op: EvidenceOp,
        response: Vec<u8>,
        fault: Option<Fault>,
    }

    fn frames(spec: DeviceSpec, i: usize, purpose: Purpose, singleton: bool) -> Vec<Frame> {
        let frame = |op, response| Frame {
            op,
            response,
            fault: None,
        };
        let vpd = [
            frame(EvidenceOp::Vpd80, vpd(spec, 0x80, spec.serial.as_bytes())),
            frame(EvidenceOp::Vpd83, vpd83(spec, i as u8)),
        ];
        let mut frames = vpd.to_vec();
        if purpose == Purpose::Pr {
            for _ in 0..2 {
                frames.extend(
                    [
                        EvidenceOp::ReadKeys,
                        EvidenceOp::ReadReservation,
                        EvidenceOp::ReadFullStatus,
                    ]
                    .into_iter()
                    .zip(pr_round(singleton, GEN + i as u32))
                    .map(|(op, response)| frame(op, response)),
                );
            }
        }
        frames.extend(vpd);
        frames
    }

    fn fixture_path(spec: DeviceSpec, i: usize) -> PathIdentity {
        let node = u64::from(spec.node.as_bytes()[2] - b'0');
        let object = |name: &str| SysfsObject {
            path: format!("/fixture/{name}/{i}"),
            ino: node,
            dev: 42,
        };
        PathIdentity {
            fd_stat: FileIdentity {
                dev: 42,
                ino: node,
                mode: nix::libc::S_IFCHR | 0o600,
                rdev: makedev(21, node),
            },
            sg: object("sg"),
            scsi_device: object("lu"),
            fc_host: object("fc_host"),
            fc_rport: object("fc_rport"),
            hctl: format!("18:0:0:{i}"),
            pdt: spec.pdt,
            initiator_port: format!("0x{INITIATOR:016x}"),
            target_port: format!("0x{:016x}", spec.target),
            cached_vpd80_hex: hex(&vpd(spec, 0x80, spec.serial.as_bytes())),
        }
    }

    struct FixturePath<'a> {
        identity: PathIdentity,
        frames: RefCell<VecDeque<Frame>>,
        group_reads: &'a Cell<usize>,
        drift_after: Option<usize>,
    }

    impl CapturePath for FixturePath<'_> {
        fn binding(&self) -> CaptureResult<PathIdentity> {
            let mut identity = self.identity.clone();
            if self
                .drift_after
                .is_some_and(|i| self.group_reads.get() >= i)
            {
                identity.fc_rport.ino += 1;
            }
            Ok(identity)
        }

        fn read(
            &self,
            command: &ReadCommand,
            buf: &mut [u8],
        ) -> (Option<RawCompletion>, Result<ScsiResult>) {
            self.group_reads.set(self.group_reads.get() + 1);
            let frame = self
                .frames
                .borrow_mut()
                .pop_front()
                .expect("每条 CDB 恰好发一次");
            assert_eq!(frame.op, command.op);
            let len = frame.response.len();
            buf[..len].copy_from_slice(&frame.response);
            let mut raw = completed(command, len);
            match frame.fault {
                Some(Fault::Resid(value)) => raw.resid = value,
                Some(Fault::UnitAttention | Fault::IllegalRequest) => {
                    raw.status = 2;
                    raw.driver_status = 8;
                    raw.info = 1;
                    raw.sense_bytes = if matches!(frame.fault, Some(Fault::UnitAttention)) {
                        vec![0x72, 6, 0x29, 0, 0, 0, 0, 0]
                    } else {
                        vec![0x72, 5, 0x24, 0, 0, 0, 0, 0]
                    };
                    raw.sb_len_wr = raw.sense_bytes.len() as u8;
                    raw.resid = command.allocation as i32;
                }
                Some(Fault::Host) => {
                    raw.host_status = 3;
                    return (Some(raw), Err(TapeError::ScsiHost(3)));
                }
                Some(Fault::Driver) => {
                    raw.driver_status = 4;
                    return (Some(raw), Err(TapeError::ScsiDriver(4)));
                }
                Some(Fault::CheckWithoutSense) => raw.status = 2,
                Some(Fault::Message) => raw.msg_status = 7,
                Some(Fault::Ioctl) => return (None, Err(nix::errno::Errno::EINTR.into())),
                None => {}
            }
            let result = finish_command(
                raw.status,
                SenseInfo::from_bytes(&raw.sense_bytes),
                len,
                3,
                "fixture",
            );
            (Some(raw), result)
        }
    }

    fn capture_fixture(
        request: CaptureRequest,
        configure: impl Fn(usize, &mut PathIdentity, &mut Vec<Frame>) -> Option<usize>,
        singleton_index: Option<usize>,
    ) -> (CaptureOutcome, usize) {
        let total = Cell::new(0);
        let purpose = request.purpose;
        let mut i = 0;
        let outcome = capture_with(request, |spec, _| {
            assert_eq!(total.get(), 0, "三条固定路径先全部核对");
            let mut identity = fixture_path(spec, i);
            let mut script = frames(spec, i, purpose, singleton_index == Some(i));
            let drift_after = configure(i, &mut identity, &mut script);
            i += 1;
            Ok((
                FixturePath {
                    identity: identity.clone(),
                    frames: RefCell::new(script.into()),
                    group_reads: &total,
                    drift_after,
                },
                identity,
            ))
        });
        (outcome, total.get())
    }

    #[test]
    fn cdb_whitelist_timeouts_and_fixed_paths() {
        for (op, wire, alloc, timeout) in [
            (EvidenceOp::Vpd80, "120180100000", 4096, 10000),
            (EvidenceOp::Vpd83, "120183100000", 4096, 10000),
            (EvidenceOp::ReadKeys, "5e000000000000040000", 1024, 30000),
            (
                EvidenceOp::ReadReservation,
                "5e010000000000040000",
                1024,
                30000,
            ),
            (
                EvidenceOp::ReadFullStatus,
                "5e030000000000040000",
                1024,
                30000,
            ),
        ] {
            let command = op.command();
            assert_eq!(
                (
                    hex(&command.cdb).as_str(),
                    command.allocation,
                    command.timeout_ms
                ),
                (wire, alloc, timeout)
            );
        }
        for (host, expected) in [
            (Host::Host207, ["sg5", "sg4", "sg6"]),
            (Host::Host208, ["sg5", "sg4", "sg6"]),
            (Host::Host209, ["sg7", "sg6", "sg4"]),
        ] {
            assert_eq!(devices(host).map(|spec| spec.node), expected);
        }
    }

    #[test]
    fn role_wrong_target_alias_or_actor_mismatch_sends_zero_commands() {
        for fault in [
            "wrong_target",
            "rdev_alias",
            "hctl_alias",
            "wrong_actor",
            "truncated_round",
        ] {
            let mut input = request(Purpose::Pr);
            match fault {
                "wrong_actor" => input.actor.host = Host::Host208,
                "truncated_round" => input.actor.round = 1 << 48,
                _ => {}
            }
            let (outcome, count) = capture_fixture(
                input,
                |i, bind, _| {
                    match (fault, i) {
                        ("wrong_target", 0) => bind.target_port = "0x5000e11170186070".into(),
                        ("rdev_alias", 1) => bind.fd_stat.rdev = makedev(21, 7),
                        ("hctl_alias", 1) => bind.hctl = "18:0:0:0".into(),
                        _ => {}
                    }
                    None
                },
                None,
            );
            assert_eq!(outcome.classification, Classification::Invalid, "{fault}");
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn stable_group_paths_empty_and_mixed_singleton() {
        for (purpose, singleton, class, cdb_count) in [
            (Purpose::Paths, None, Classification::PathsComplete, 12),
            (Purpose::Pr, None, Classification::CompleteEmpty, 30),
            (Purpose::Pr, Some(1), Classification::ObservedSingleton, 30),
        ] {
            let (outcome, reads) = capture_fixture(request(purpose), |_, _, _| None, singleton);
            assert_eq!(outcome.classification, class);
            assert_eq!(reads, cdb_count);
            for (i, device) in outcome.devices.iter().enumerate() {
                assert_eq!(device.before_path, device.after_path);
                assert_eq!(device.before_vpd, device.after_vpd);
                assert_eq!(
                    device.before_vpd.as_ref().unwrap().lu_id.kind,
                    "vpd83-fc-naa8"
                );
                let expected = if purpose == Purpose::Paths {
                    None
                } else if singleton == Some(i) {
                    Some(Classification::ObservedSingleton)
                } else {
                    Some(Classification::CompleteEmpty)
                };
                assert_eq!(
                    device.observation.as_ref().map(|o| o.classification),
                    expected
                );
                assert!(
                    device
                        .reads
                        .iter()
                        .all(|read| read.response_hex.as_ref().unwrap().len()
                            == 2 * read.transferred.unwrap())
                );
            }
            assert!(!serde_json::to_string(&outcome).unwrap().contains("lun_hex"));
        }
    }

    #[test]
    fn actual_length_and_error_evidence_survive_first_failure() {
        for (fault, classification) in [
            (Fault::Resid(-1), Classification::Invalid),
            (
                Fault::Resid(i32::from(PR_ALLOC) + 1),
                Classification::Invalid,
            ),
            (Fault::UnitAttention, Classification::IoIncomplete),
            (Fault::IllegalRequest, Classification::Unsupported),
            (Fault::Host, Classification::IoIncomplete),
            (Fault::Driver, Classification::IoIncomplete),
            (Fault::CheckWithoutSense, Classification::Unsupported),
            (Fault::Message, Classification::Unsupported),
            (Fault::Ioctl, Classification::IoIncomplete),
        ] {
            let (outcome, reads) = capture_fixture(
                request(Purpose::Pr),
                |i, _, frames| {
                    if i == 0 {
                        frames[4].fault = Some(fault.clone());
                    }
                    None
                },
                None,
            );
            assert_eq!(outcome.classification, classification);
            assert_eq!(reads, 9, "6 VPD 前置 + 3 PR，无重发");
            assert!(
                outcome
                    .devices
                    .iter()
                    .all(|device| device.observation.is_none())
            );
            let last = outcome.devices[0].reads.last().unwrap();
            assert_eq!(last.operation, EvidenceOp::ReadFullStatus);
            let raw = last.raw_completion.as_ref();
            match fault {
                Fault::Resid(value) => {
                    assert_eq!(raw.unwrap().resid, value);
                    assert!(last.response_hex.is_none());
                }
                Fault::UnitAttention => {
                    assert_eq!(raw.unwrap().sense_bytes[1..4], [6, 0x29, 0]);
                    assert!(last.error.is_some());
                }
                Fault::IllegalRequest => assert_eq!(raw.unwrap().sense_bytes[1..4], [5, 0x24, 0]),
                Fault::Host => {
                    assert_eq!(raw.unwrap().host_status, 3);
                    assert!(last.response_hex.is_none());
                }
                Fault::Driver => {
                    assert_eq!(raw.unwrap().driver_status, 4);
                    assert!(last.response_hex.is_none());
                }
                Fault::CheckWithoutSense => {
                    assert_eq!(raw.unwrap().status, 2);
                    assert!(last.error.is_none());
                }
                Fault::Message => assert_eq!(raw.unwrap().msg_status, 7),
                Fault::Ioctl => {
                    assert!(raw.is_none());
                    assert_eq!(last.errno, Some(nix::libc::EINTR));
                }
            }
        }
    }

    #[test]
    fn good_completion_with_truncated_payload_retains_only_transferred_bytes() {
        let (outcome, reads) = capture_fixture(
            request(Purpose::Pr),
            |i, _, frames| {
                if i == 0 {
                    frames[4].response.truncate(5);
                }
                None
            },
            None,
        );
        assert_eq!(reads, 12, "主机只采集两轮三条命令");
        assert_eq!(outcome.classification, Classification::Unsupported);
        let read = &outcome.devices[0].reads[4];
        assert_eq!(read.transferred, Some(5));
        assert_eq!(read.response_hex.as_deref(), Some("0000001300"));
        assert_eq!(read.raw_completion.as_ref().unwrap().status, 0);
    }

    #[test]
    fn full_status_rejects_extra_nexus_and_unsupported_fields_or_lengths() {
        let [keys, reservation, full] = pr_round(true, GEN);
        assert_eq!(
            observe_round(
                &[keys.clone(), reservation.clone(), full.clone()],
                KEY,
                RTPI,
                &format!("0x{INITIATOR:016x}")
            )
            .unwrap()
            .classification,
            Classification::ObservedSingleton
        );
        let mut rejected = Vec::new();
        for (i, value) in [
            (12, 3),
            (13, 0x13),
            (13, 1),
            (18, 0),
            (24, 0x40),
            (24, 6),
            (33, 1),
            (40, 1),
            (23, 25),
        ] {
            let mut body = full[8..].to_vec();
            body[i] = value;
            rejected.push(pr(GEN, &body));
        }
        rejected.push(pr(GEN, &[&full[8..], &full[8..]].concat()));
        rejected.push(pr(GEN + 1, &full[8..]));
        rejected.extend([
            full[..5].to_vec(),
            full[..full.len() - 1].to_vec(),
            [full.clone(), vec![0]].concat(),
        ]);
        for raw in rejected {
            assert_eq!(
                observe_round(
                    &[keys.clone(), reservation.clone(), raw],
                    KEY,
                    RTPI,
                    &format!("0x{INITIATOR:016x}")
                )
                .unwrap_err()
                .classification,
                Classification::Unsupported
            );
        }
        for (key, port) in [
            (ReservationKey(1), format!("0x{INITIATOR:016x}")),
            (KEY, "0x10000090fa000000".into()),
        ] {
            assert!(
                observe_round(
                    &[keys.clone(), reservation.clone(), full.clone()],
                    key,
                    RTPI,
                    &port
                )
                .is_err()
            );
        }
    }

    #[test]
    fn reread_rejects_new_generation_or_state_and_after_identity_changes() {
        for fault in [
            "generation",
            "same_generation_new_state",
            "new_naa",
            "duplicate_naa",
            "sysfs_drift",
        ] {
            let (outcome, reads) = capture_fixture(
                request(Purpose::Pr),
                |i, _, frames| {
                    if i != 0 {
                        return None;
                    }
                    match fault {
                        "generation" | "same_generation_new_state" => {
                            let new_round = pr_round(
                                fault == "same_generation_new_state",
                                GEN + u32::from(fault == "generation"),
                            );
                            for (frame, response) in frames[5..8].iter_mut().zip(new_round) {
                                frame.response = response;
                            }
                        }
                        "new_naa" => frames[9].response[15] = 9,
                        "duplicate_naa" => frames[1].response[15] = 1,
                        "sysfs_drift" => return Some(1),
                        _ => unreachable!(),
                    }
                    None
                },
                None,
            );
            assert_ne!(outcome.exit_code(), 0, "{fault}");
            assert!(
                outcome
                    .devices
                    .iter()
                    .all(|device| device.observation.is_none())
            );
            if fault == "sysfs_drift" {
                assert_eq!(reads, 1);
                assert!(outcome.devices[0].reads[0].binding_failure.is_some());
            }
            if fault == "duplicate_naa" {
                assert!(
                    outcome
                        .devices
                        .iter()
                        .flat_map(|device| &device.reads)
                        .all(|read| matches!(
                            read.operation,
                            EvidenceOp::Vpd80 | EvidenceOp::Vpd83
                        ))
                );
            }
        }
    }

    #[test]
    fn vpd_framing_association_rtpi_and_target_are_checked() {
        let spec = devices(Host::Host209)[0];
        let full = vpd83(spec, 2);
        assert_eq!(
            vpd83_identity(&full, spec).unwrap().relative_target_port,
            RTPI
        );
        for (i, value) in [
            (0, 0x28),
            (2, 1),
            (4, 2),
            (5, 0x13),
            (17, 0x13),
            (19, 20),
            (27, 0x70),
            (32, 1),
        ] {
            let mut bad = full.clone();
            bad[i] = value;
            assert!(vpd83_identity(&bad, spec).is_err(), "offset {i}");
        }
        let duplicate = vpd(spec, 0x83, &[&full[4..], &full[4..16]].concat());
        assert!(vpd83_identity(&duplicate, spec).is_err());
        assert!(vpd83_identity(&full[..full.len() - 1], spec).is_err());
        assert!(vpd_payload(&[spec.pdt, 0x80, 0], spec, 0x80).is_err());
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("facts3-capture-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fixture_sysfs_selects_npiv_fc_host_and_rejects_cross_links() {
        let dir = TempDir::new();
        let sys = &dir.0;
        let class = sys.join("class");
        let physical = sys.join("devices/pci0000:00/host17");
        let host = physical.join("vport-17:0-0/host18");
        let rport = host.join("rport-18:0-42");
        let lu = rport.join("target18:0:4/18:0:4:1");
        let sg = lu.join("scsi_generic/sg7");
        let fc_host = host.join("fc_host/host18");
        let fc_physical = physical.join("fc_host/host17");
        let fc_rport = rport.join("fc_remote_ports/rport-18:0-42");
        for path in [
            &sg,
            &fc_host,
            &fc_physical,
            &fc_rport,
            &class.join("scsi_generic"),
            &class.join("fc_host"),
            &class.join("fc_remote_ports"),
            &sys.join("dev/char"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let spec = devices(Host::Host209)[0];
        for (target, link) in [
            (&lu, sg.join("device")),
            (&sg, class.join("scsi_generic/sg7")),
            (&sg, sys.join("dev/char/21:7")),
            (&class.join("scsi_generic"), sg.join("subsystem")),
            (&fc_host, class.join("fc_host/host18")),
            (&fc_physical, class.join("fc_host/host17")),
            (&fc_rport, class.join("fc_remote_ports/rport-18:0-42")),
        ] {
            symlink(target, link).unwrap();
        }
        fs::write(fc_host.join("port_name"), format!("0x{INITIATOR:016x}\n")).unwrap();
        fs::write(fc_physical.join("port_name"), "0x10000090fa111111\n").unwrap();
        fs::write(
            fc_rport.join("port_name"),
            format!("0x{:016x}\n", spec.target),
        )
        .unwrap();
        fs::write(lu.join("type"), b"8\n").unwrap();
        fs::write(lu.join("vpd_pg80"), vpd(spec, 0x80, spec.serial.as_bytes())).unwrap();
        // 真 fstat 取临时文件 inode，夹具仅替换字符设备类型和 rdev，无设备 fd。
        let file = File::create(sys.join("fixture-fd")).unwrap();
        let stat = fstat(file.as_raw_fd()).unwrap();
        let mut modeled = FileIdentity {
            dev: stat.st_dev,
            ino: stat.st_ino,
            mode: nix::libc::S_IFCHR | 0o600,
            rdev: makedev(21, 7),
        };
        let binding = resolve_sysfs(sys, spec, modeled.clone()).unwrap();
        assert_eq!(binding.hctl, "18:0:4:1");
        assert_eq!(binding.initiator_port, format!("0x{INITIATOR:016x}"));
        assert_eq!(binding.fd_stat.ino, stat.st_ino);
        vpd80_identity(&binding, spec).unwrap();
        modeled.rdev = makedev(21, 6);
        assert!(resolve_sysfs(sys, spec, modeled.clone()).is_err());
        modeled.rdev = makedev(21, 7);
        fs::remove_file(class.join("fc_host/host18")).unwrap();
        symlink(fc_physical, class.join("fc_host/host18")).unwrap();
        assert!(resolve_sysfs(sys, spec, modeled).is_err());
    }

    #[test]
    fn capture_open_rejects_regular_file_and_symlink_before_ioctl() {
        let path = TempDir::new();
        let regular = path.0.join("regular");
        let symlink_path = path.0.join("link");
        fs::write(&regular, b"plain fixture").unwrap();
        symlink(&regular, &symlink_path).unwrap();
        assert!(matches!(
            ScsiDevice::open_capture_readonly(regular.to_str().unwrap()),
            Err(TapeError::Refused { .. })
        ));
        assert!(
            matches!(ScsiDevice::open_capture_readonly(symlink_path.to_str().unwrap()),
            Err(TapeError::Io(error)) if error.raw_os_error() == Some(nix::libc::ELOOP))
        );
    }

    #[test]
    fn simulator_provides_actual_keys_and_reservation_but_lacks_full_status() {
        let lib = SimLibrary::new(1, 1, 1);
        let drive = lib.drive(0);
        for op in [EvidenceOp::ReadKeys, EvidenceOp::ReadReservation] {
            let command = op.command();
            let mut data = vec![0; command.allocation];
            let completion = drive
                .execute_read(&command.cdb, &mut data, command.timeout_ms)
                .unwrap();
            assert_eq!(
                pr_payload(&data[..completion.transferred]).unwrap(),
                (0, &[][..])
            );
        }
        let full = EvidenceOp::ReadFullStatus.command();
        let error = drive
            .execute_read(&full.cdb, &mut vec![0; full.allocation], full.timeout_ms)
            .unwrap_err();
        assert!(matches!(error, TapeError::ScsiCommand { sense_key: 5, .. }));
        assert_eq!(drive.command_log(), vec![0x5e; 3]);
    }
}
