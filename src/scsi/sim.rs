//! 内存模拟传输：`SimLibrary` / `SimTransport`。
//!
//! 一个 `SimLibrary` 模拟一台逻辑带库：机械手、若干驱动器、存储槽、I/E 槽和
//! 一组磁带盒。`SimLibrary::changer()` 与 `SimLibrary::drive(i)` 各返回一个
//! 实现 [`TapeTransport`] 的 `SimTransport`，它们解析与 `scsi::cdb` 相同的
//! CDB 字节布局，因此 `MediumChanger`、`TapeDrive`、`Mam`、`LtfsVolume`
//! 不需要任何改动即可跑在模拟器上。
//!
//! 磁带模型：每个分区是一串逻辑对象（记录或文件标记），逻辑块号按对象计数
//! （文件标记也占一个块号，与 SSC 及 LTFS 布局一致）。写入会截断当前位置
//! 之后的全部对象；`flushed` 之后的对象位于"驱动器缓冲"，`power_cut()` 会
//! 丢弃它们——这是模拟 D02 尾部 T1/T2 的入口。会触发刷缓冲的命令：
//! WRITE FILEMARKS、REWIND、UNLOAD、LOCATE、SPACE、ERASE、FORMAT。
//!
//! 状态分类与真实设备共用 `device::finish_command`，所以文件标记（NO SENSE，
//! ASC 0x00/0x01）同样以 `Ok(transferred = 0)` 返回，BLANK CHECK 以
//! `TapeError::ScsiCommand { sense_key: 0x08, .. }` 返回。
//!
//! 模拟器通过与否不代表设备协议已验证；真实驱动器的 EOD/SPACE/READ POSITION
//! 语义、固件差异仍须按 AGENTS.md 单独在硬件上核对。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, TapeError};
use crate::scsi::cdb::opcode;
use crate::scsi::device::{ScsiResult, finish_command};
use crate::scsi::sense::SenseInfo;
use crate::scsi::transport::TapeTransport;

/// 与真实 TS4300 观察到的地址空间一致。
pub const TRANSPORT_START: u16 = 0x0000;
pub const DT_START: u16 = 0x0001;
pub const IE_START: u16 = 0x0065;
pub const STORAGE_START: u16 = 0x03E9;

const SENSE_KEY_NOT_READY: u8 = 0x02;
const SENSE_KEY_ILLEGAL_REQUEST: u8 = 0x05;
const SENSE_KEY_DATA_PROTECT: u8 = 0x07;
const SENSE_KEY_BLANK_CHECK: u8 = 0x08;
const SENSE_KEY_VOLUME_OVERFLOW: u8 = 0x0D;

/// 磁带上的一个逻辑对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalObject {
    Record(Vec<u8>),
    Filemark,
}

/// 一个分区的内容与缓冲状态。
#[derive(Debug, Clone)]
pub struct SimPartition {
    pub objects: Vec<LogicalObject>,
    /// `objects[..flushed]` 已落"介质"；其后为驱动器缓冲，掉电即丢。
    pub flushed: usize,
    /// 记录字节容量上限。
    pub capacity: u64,
}

impl SimPartition {
    fn empty(capacity: u64) -> Self {
        Self {
            objects: Vec::new(),
            flushed: 0,
            capacity,
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.objects
            .iter()
            .map(|o| match o {
                LogicalObject::Record(d) => d.len() as u64,
                LogicalObject::Filemark => 0,
            })
            .sum()
    }

    fn truncate_at(&mut self, pos: usize) {
        self.objects.truncate(pos);
        self.flushed = self.flushed.min(pos);
    }
}

/// 一盘模拟磁带。
#[derive(Debug, Clone)]
pub struct SimCartridge {
    pub barcode: String,
    pub partitions: Vec<SimPartition>,
    /// MAM：(partition, attribute id) → (format, value)。
    pub mam: BTreeMap<(u8, u16), (u8, Vec<u8>)>,
    pub write_protected: bool,
    /// 格式化时用于划分各分区的总容量。
    pub capacity: u64,
    /// VOLUME CHANGE REFERENCE：每次修改介质内容递增，MAM 0x0009 只读暴露。从 1 起（0 非法）。
    pub vcr: u32,
}

impl SimCartridge {
    /// 未格式化的空白磁带：单分区、无 MAM 属性。
    pub fn blank(barcode: &str, capacity: u64) -> Self {
        Self {
            barcode: barcode.to_string(),
            partitions: vec![SimPartition::empty(capacity)],
            mam: BTreeMap::new(),
            write_protected: false,
            capacity,
            vcr: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct DriveState {
    serial: String,
    cartridge: Option<String>,
    loaded: bool,
    partition: u8,
    pos: usize,
    /// MODE SELECT page 0x11 设定、下一次 FORMAT 生效的分区数。
    pending_partitions: usize,
    /// 待上报的 UNIT ATTENTION (asc, ascq)：下一条非 INQUIRY 命令不执行、先返回它。
    pending_ua: Vec<(u8, u8)>,
    /// 持久预留状态，按发起方区分。
    pr: PrState,
}

/// 持久预留的最小模型：LU 作用域、Exclusive Access 语义，不跨断电保持。
#[derive(Debug, Clone, Default)]
struct PrState {
    generation: u32,
    /// 发起方 → 注册的键
    registrations: std::collections::BTreeMap<u32, u64>,
    /// 持有者发起方与预留类型
    holder: Option<(u32, u8)>,
    /// 只发给某个发起方的 UNIT ATTENTION
    ua: std::collections::BTreeMap<u32, Vec<(u8, u8)>>,
}

/// 已知设备偏差的模拟开关（用于回归：上层必须在这些偏差下仍然正确）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SimQuirks {
    /// Holo-VTL：读到文件标记时只置 FM 位，不报 residual（sg 看起来传满缓冲区），
    /// 无 VALID/INFORMATION，ASC/ASCQ 为 00/00。
    pub filemark_without_residual: bool,
    /// Holo-VTL：在 BOP 处反向 SPACE 返回 ILLEGAL REQUEST 24/00（规范为 NO SENSE + EOM + 00/04）。
    pub bop_space_illegal_request: bool,
    /// 未修复的 Holo-VTL：正向 SPACE filemarks 越过 EOD 返回 ILLEGAL REQUEST 24/00 且位置不动
    /// （规范为 BLANK CHECK 00/05 并停在 EOD）。
    pub eod_space_illegal_request: bool,
    /// 未修复的 Holo-VTL：VCR（MAM 0x0009）不随写入变化，VCI 因而永远"新鲜"。
    pub vcr_never_changes: bool,
}

impl SimQuirks {
    pub fn holo_vtl() -> Self {
        Self {
            filemark_without_residual: true,
            bop_space_illegal_request: true,
            eod_space_illegal_request: true,
            vcr_never_changes: true,
        }
    }
}

#[derive(Debug)]
struct LibState {
    quirks: SimQuirks,
    serial: String,
    cartridges: BTreeMap<String, SimCartridge>,
    storage: Vec<Option<String>>,
    ie: Vec<Option<String>>,
    drives: Vec<DriveState>,
}

#[derive(Debug, Clone, Copy)]
enum ElemRef {
    Transport,
    Drive(usize),
    Ie(usize),
    Storage(usize),
}

impl LibState {
    fn resolve(&self, addr: u16) -> Option<ElemRef> {
        if addr == TRANSPORT_START {
            return Some(ElemRef::Transport);
        }
        if addr >= DT_START && ((addr - DT_START) as usize) < self.drives.len() {
            return Some(ElemRef::Drive((addr - DT_START) as usize));
        }
        if addr >= IE_START && ((addr - IE_START) as usize) < self.ie.len() {
            return Some(ElemRef::Ie((addr - IE_START) as usize));
        }
        if addr >= STORAGE_START && ((addr - STORAGE_START) as usize) < self.storage.len() {
            return Some(ElemRef::Storage((addr - STORAGE_START) as usize));
        }
        None
    }

    fn slot_mut(&mut self, r: ElemRef) -> Option<&mut Option<String>> {
        match r {
            ElemRef::Storage(i) => Some(&mut self.storage[i]),
            ElemRef::Ie(i) => Some(&mut self.ie[i]),
            ElemRef::Drive(i) => Some(&mut self.drives[i].cartridge),
            ElemRef::Transport => None,
        }
    }

    fn drive_flush(&mut self, i: usize) {
        let d = &self.drives[i];
        if let Some(bc) = d.cartridge.clone()
            && let Some(c) = self.cartridges.get_mut(&bc)
            && let Some(p) = c.partitions.get_mut(d.partition as usize)
        {
            p.flushed = p.objects.len();
        }
    }
}

/// 故障注入动作。
#[derive(Debug, Clone)]
pub enum FaultAction {
    /// 返回 CHECK CONDITION 与指定 sense。
    CheckCondition { sense_key: u8, asc: u8, ascq: u8 },
    /// 返回 `TapeError::ScsiHost`。
    HostError(u16),
    /// 返回 `TapeError::Busy`。
    Busy,
}

/// 针对某个 opcode 的故障；先放过 `after` 次匹配命令，再生效 `remaining` 次
/// （`remaining == 0` 表示持续生效）。
#[derive(Debug, Clone)]
pub struct Fault {
    pub opcode: u8,
    pub action: FaultAction,
    pub after: u32,
    pub remaining: u32,
}

/// 模拟带库句柄（可克隆，共享同一状态）。
#[derive(Clone)]
pub struct SimLibrary {
    state: Arc<Mutex<LibState>>,
}

fn lock(m: &Mutex<LibState>) -> MutexGuard<'_, LibState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SimLibrary {
    pub fn new(drives: usize, storage_slots: usize, ie_slots: usize) -> Self {
        let drives = (0..drives)
            .map(|i| DriveState {
                serial: format!("SIMDRV{:04}", i),
                cartridge: None,
                loaded: false,
                partition: 0,
                pos: 0,
                pending_partitions: 1,
                pending_ua: Vec::new(),
                pr: PrState::default(),
            })
            .collect();
        Self {
            state: Arc::new(Mutex::new(LibState {
                quirks: SimQuirks::default(),
                serial: "SIMLIB0000000001".to_string(),
                cartridges: BTreeMap::new(),
                storage: vec![None; storage_slots],
                ie: vec![None; ie_slots],
                drives,
            })),
        }
    }

    /// 打开已知设备偏差的模拟。
    pub fn set_quirks(&self, quirks: SimQuirks) {
        lock(&self.state).quirks = quirks;
    }

    /// 把磁带放进存储槽（0 基）。
    pub fn insert_cartridge(&self, cart: SimCartridge, slot: usize) -> Result<()> {
        let mut st = lock(&self.state);
        let target = st.storage.get(slot).ok_or_else(|| TapeError::Refused {
            reason: format!("存储槽 {} 越界", slot),
        })?;
        if target.is_some() {
            return Err(TapeError::Refused {
                reason: format!("存储槽 {} 已有介质", slot),
            });
        }
        if st.cartridges.contains_key(&cart.barcode) {
            return Err(TapeError::Refused {
                reason: format!("条码 {} 重复", cart.barcode),
            });
        }
        let bc = cart.barcode.clone();
        st.cartridges.insert(bc.clone(), cart);
        st.storage[slot] = Some(bc);
        Ok(())
    }

    /// 测试便利：把指定磁带直接放入驱动器并装载（绕过机械手）。
    pub fn load_into_drive(&self, barcode: &str, drive: usize) -> Result<()> {
        let mut st = lock(&self.state);
        if !st.cartridges.contains_key(barcode) {
            return Err(TapeError::Refused {
                reason: format!("未知条码 {}", barcode),
            });
        }
        if drive >= st.drives.len() {
            return Err(TapeError::Refused {
                reason: format!("驱动器 {} 越界", drive),
            });
        }
        if st.drives[drive].cartridge.is_some() {
            return Err(TapeError::Refused {
                reason: format!("驱动器 {} 已有介质", drive),
            });
        }
        let LibState { storage, ie, .. } = &mut *st;
        for s in storage.iter_mut().chain(ie.iter_mut()) {
            if s.as_deref() == Some(barcode) {
                *s = None;
            }
        }
        let d = &mut st.drives[drive];
        d.cartridge = Some(barcode.to_string());
        d.loaded = true;
        d.partition = 0;
        d.pos = 0;
        Ok(())
    }

    /// 驱动器数量。
    pub fn drive_count(&self) -> usize {
        lock(&self.state).drives.len()
    }

    /// 某个驱动器里装着的磁带条码。
    pub fn loaded_barcode(&self, idx: usize) -> Option<String> {
        lock(&self.state).drives.get(idx).and_then(|d| d.cartridge.clone())
    }

    pub fn changer(&self) -> SimTransport {
        SimTransport {
            lib: Arc::clone(&self.state),
            target: Target::Changer,
            initiator: 0,
            faults: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            identity: "sim:changer".to_string(),
        }
    }

    pub fn drive(&self, idx: usize) -> SimTransport {
        self.drive_as(idx, 0)
    }

    /// 以指定发起方身份访问驱动器。不同发起方对应不同节点（不同的 I_T nexus），
    /// 持久预留按发起方区分。
    pub fn drive_as(&self, idx: usize, initiator: u32) -> SimTransport {
        SimTransport {
            lib: Arc::clone(&self.state),
            target: Target::Drive(idx),
            initiator,
            faults: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            identity: format!("sim:drive{}@i{}", idx, initiator),
        }
    }

    /// 驱动器复位：预留与注册全部丢失（未启用跨断电保持），未刷缓冲丢弃，
    /// 位置回到 (0, 0)，所有发起方的下一条命令先收到 29/00。
    pub fn reset_drive(&self, idx: usize) {
        let mut st = lock(&self.state);
        let barcode = st.drives[idx].cartridge.clone();
        if let Some(c) = barcode.and_then(|b| st.cartridges.get_mut(&b)) {
            for p in c.partitions.iter_mut() {
                p.objects.truncate(p.flushed);
            }
        }
        let d = &mut st.drives[idx];
        d.partition = 0;
        d.pos = 0;
        d.pr = PrState::default();
        d.pending_ua.push((0x29, 0x00));
    }

    /// 整库掉电：所有驱动器丢弃未刷缓冲，位置回到 (0, 0)，介质保持装载。
    pub fn power_cut(&self) {
        let mut st = lock(&self.state);
        for i in 0..st.drives.len() {
            let d = st.drives[i].clone();
            if let Some(bc) = d.cartridge
                && let Some(c) = st.cartridges.get_mut(&bc)
            {
                for p in c.partitions.iter_mut() {
                    let f = p.flushed;
                    p.objects.truncate(f);
                }
            }
            st.drives[i].partition = 0;
            st.drives[i].pos = 0;
        }
    }

    /// 读取磁带快照（克隆）。
    pub fn cartridge(&self, barcode: &str) -> Option<SimCartridge> {
        lock(&self.state).cartridges.get(barcode).cloned()
    }

    /// 直接修改磁带内容（用于构造异常尾部等测试场景）。
    pub fn with_cartridge_mut<F: FnOnce(&mut SimCartridge)>(&self, barcode: &str, f: F) -> bool {
        let mut st = lock(&self.state);
        match st.cartridges.get_mut(barcode) {
            Some(c) => {
                f(c);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Changer,
    Drive(usize),
}

/// 一个模拟设备节点。
pub struct SimTransport {
    lib: Arc<Mutex<LibState>>,
    target: Target,
    initiator: u32,
    faults: Mutex<Vec<Fault>>,
    log: Mutex<Vec<u8>>,
    identity: String,
}

impl SimTransport {
    pub fn inject(&self, fault: Fault) {
        self.faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(fault);
    }

    pub fn clear_faults(&self) {
        self.faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 已执行命令的 opcode 序列（用于断言命令顺序）。
    pub fn command_log(&self) -> Vec<u8> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn take_fault(&self, op: u8) -> Option<FaultAction> {
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        let idx = faults.iter().position(|f| f.opcode == op)?;
        if faults[idx].after > 0 {
            faults[idx].after -= 1;
            return None;
        }
        let action = faults[idx].action.clone();
        if faults[idx].remaining > 0 {
            faults[idx].remaining -= 1;
            if faults[idx].remaining == 0 {
                faults.remove(idx);
            }
        }
        Some(action)
    }

    fn dispatch(
        &self,
        cdb: &[u8],
        data_in: Option<&[u8]>,
        data_out: Option<&mut [u8]>,
    ) -> Result<ScsiResult> {
        let op = *cdb.first().ok_or(TapeError::InvalidResponse {
            expected: 1,
            actual: 0,
        })?;
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        if let Some(action) = self.take_fault(op) {
            return match action {
                FaultAction::CheckCondition {
                    sense_key,
                    asc,
                    ascq,
                } => Err(TapeError::ScsiCommand {
                    status: 0x02,
                    sense_key,
                    asc,
                    ascq,
                }),
                FaultAction::HostError(h) => Err(TapeError::ScsiHost(h)),
                FaultAction::Busy => Err(TapeError::Busy {
                    device: self.identity.clone(),
                }),
            };
        }
        let mut st = lock(&self.lib);
        let outcome = match self.target {
            Target::Changer => changer_command(&mut st, cdb, data_out),
            Target::Drive(i) => drive_command(&mut st, i, self.initiator, cdb, data_in, data_out),
        };
        match outcome {
            Ok(Completion {
                status,
                sense,
                transferred,
            }) => finish_command(status, sense, transferred, 0, &self.identity),
            Err(e) => Err(e),
        }
    }
}

impl TapeTransport for SimTransport {
    fn execute_no_data(&self, cdb: &[u8], _timeout_ms: u32) -> Result<ScsiResult> {
        self.dispatch(cdb, None, None)
    }

    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], _timeout_ms: u32) -> Result<ScsiResult> {
        self.dispatch(cdb, None, Some(buf))
    }

    fn execute_write(&self, cdb: &[u8], buf: &[u8], _timeout_ms: u32) -> Result<ScsiResult> {
        self.dispatch(cdb, Some(buf), None)
    }

    fn identity(&self) -> &str {
        &self.identity
    }
}

// ---------- 命令完成的内部表示 ----------

struct Completion {
    status: u8,
    sense: SenseInfo,
    transferred: usize,
}

fn good(transferred: usize) -> Result<Completion> {
    Ok(Completion {
        status: 0x00,
        sense: SenseInfo::from_bytes(&[]),
        transferred,
    })
}

fn sense_bytes(key: u8, asc: u8, ascq: u8) -> [u8; 18] {
    sense_bytes_ex(key, asc, ascq, 0, None)
}

/// `flags`：byte 2 高位（0x80 FILEMARK / 0x40 EOM / 0x20 ILI）；`info`：INFORMATION 字段（置 VALID）。
fn sense_bytes_ex(key: u8, asc: u8, ascq: u8, flags: u8, info: Option<i32>) -> [u8; 18] {
    let mut b = [0u8; 18];
    b[0] = 0x70;
    b[2] = (key & 0x0F) | (flags & 0xE0);
    if let Some(i) = info {
        b[0] |= 0x80;
        b[3..7].copy_from_slice(&i.to_be_bytes());
    }
    b[7] = 10;
    b[12] = asc;
    b[13] = ascq;
    b
}

/// NO SENSE 提示并带 SSC 标志位，经 `finish_command` 变成 Ok。
fn hint_flags(
    asc: u8,
    ascq: u8,
    flags: u8,
    info: Option<i32>,
    transferred: usize,
) -> Result<Completion> {
    Ok(Completion {
        status: 0x02,
        sense: SenseInfo::from_bytes(&sense_bytes_ex(0x00, asc, ascq, flags, info)),
        transferred,
    })
}

fn check(key: u8, asc: u8, ascq: u8) -> Result<Completion> {
    Ok(Completion {
        status: 0x02,
        sense: SenseInfo::from_bytes(&sense_bytes(key, asc, ascq)),
        transferred: 0,
    })
}

fn illegal_request() -> Result<Completion> {
    check(SENSE_KEY_ILLEGAL_REQUEST, 0x24, 0x00)
}

fn invalid_opcode() -> Result<Completion> {
    check(SENSE_KEY_ILLEGAL_REQUEST, 0x20, 0x00)
}

fn copy_out(out: Option<&mut [u8]>, data: &[u8], alloc: usize) -> usize {
    match out {
        Some(buf) => {
            let n = data.len().min(buf.len()).min(alloc);
            buf[..n].copy_from_slice(&data[..n]);
            n
        }
        None => 0,
    }
}

fn i24(b: &[u8]) -> i32 {
    let raw = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
    if raw & 0x80_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    }
}

fn u24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

fn inquiry_response(
    peripheral_type: u8,
    product: &str,
    serial: &str,
    cdb: &[u8],
    out: Option<&mut [u8]>,
) -> Result<Completion> {
    let alloc = u16::from_be_bytes([cdb[3], cdb[4]]) as usize;
    if cdb[1] & 0x01 != 0 {
        // VPD
        return match cdb[2] {
            0x80 => {
                let mut v = vec![peripheral_type, 0x80, 0x00, serial.len() as u8];
                v.extend_from_slice(serial.as_bytes());
                let n = copy_out(out, &v, alloc);
                good(n)
            }
            _ => illegal_request(),
        };
    }
    let mut v = vec![0u8; 96];
    v[0] = peripheral_type;
    v[1] = 0x80;
    v[2] = 0x06;
    v[3] = 0x02;
    v[4] = 91;
    v[8..16].copy_from_slice(b"SIM     ");
    let mut prod = [b' '; 16];
    let pb = product.as_bytes();
    prod[..pb.len().min(16)].copy_from_slice(&pb[..pb.len().min(16)]);
    v[16..32].copy_from_slice(&prod);
    v[32..36].copy_from_slice(b"0001");
    let n = copy_out(out, &v, alloc);
    good(n)
}

// ---------- 换带器 ----------

/// (地址, 条码, 驱动器序列号)
type ElemEntry = (u16, Option<String>, Option<String>);

fn changer_command(st: &mut LibState, cdb: &[u8], out: Option<&mut [u8]>) -> Result<Completion> {
    match cdb[0] {
        opcode::TEST_UNIT_READY
        | opcode::INITIALIZE_ELEMENT_STATUS
        | opcode::PREVENT_ALLOW_MEDIUM_REMOVAL
        | opcode::POSITION_TO_ELEMENT => good(0),
        opcode::INQUIRY => {
            let serial = st.serial.clone();
            inquiry_response(0x08, "TAPE-RS CHANGER", &serial, cdb, out)
        }
        opcode::MODE_SENSE_10 => {
            if cdb[2] & 0x3F != 0x1D {
                return illegal_request();
            }
            let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
            let mut v = vec![0u8; 8];
            let page_len = 20u16;
            v[0..2].copy_from_slice(&(6 + page_len).to_be_bytes());
            let mut page = vec![0x1D, 0x12];
            for (start, count) in [
                (TRANSPORT_START, 1u16),
                (STORAGE_START, st.storage.len() as u16),
                (IE_START, st.ie.len() as u16),
                (DT_START, st.drives.len() as u16),
            ] {
                page.extend_from_slice(&start.to_be_bytes());
                page.extend_from_slice(&count.to_be_bytes());
            }
            page.resize(page_len as usize, 0);
            v.extend_from_slice(&page);
            let n = copy_out(out, &v, alloc);
            good(n)
        }
        opcode::READ_ELEMENT_STATUS => read_element_status(st, cdb, out),
        opcode::MOVE_MEDIUM => {
            let src = u16::from_be_bytes([cdb[4], cdb[5]]);
            let dst = u16::from_be_bytes([cdb[6], cdb[7]]);
            let (Some(s), Some(d)) = (st.resolve(src), st.resolve(dst)) else {
                return illegal_request();
            };
            if matches!(s, ElemRef::Transport) || matches!(d, ElemRef::Transport) {
                return illegal_request();
            }
            let Some(bc) = st.slot_mut(s).and_then(|x| x.clone()) else {
                // 源为空
                return check(SENSE_KEY_ILLEGAL_REQUEST, 0x3B, 0x0E);
            };
            if st.slot_mut(d).map(|x| x.is_some()).unwrap_or(true) {
                // 目标已满
                return check(SENSE_KEY_ILLEGAL_REQUEST, 0x3B, 0x0D);
            }
            if let ElemRef::Drive(i) = s {
                st.drive_flush(i);
                st.drives[i].loaded = false;
                st.drives[i].pos = 0;
                st.drives[i].partition = 0;
            }
            *st.slot_mut(s).unwrap() = None;
            *st.slot_mut(d).unwrap() = Some(bc);
            if let ElemRef::Drive(i) = d {
                st.drives[i].loaded = true;
                st.drives[i].pos = 0;
                st.drives[i].partition = 0;
                // NOT READY TO READY CHANGE, MEDIUM MAY HAVE CHANGED
                st.drives[i].pending_ua.push((0x28, 0x00));
            }
            good(0)
        }
        opcode::EXCHANGE_MEDIUM => invalid_opcode(),
        _ => invalid_opcode(),
    }
}

fn read_element_status(st: &LibState, cdb: &[u8], out: Option<&mut [u8]>) -> Result<Completion> {
    let want_type = cdb[1] & 0x0F;
    let voltag = cdb[1] & 0x10 != 0;
    let start = u16::from_be_bytes([cdb[2], cdb[3]]);
    let count = u16::from_be_bytes([cdb[4], cdb[5]]) as u32;
    let dvcid = cdb[6] & 0x01 != 0;
    let alloc = u24(&cdb[7..10]) as usize;
    // SMC-3：从 start 起按地址顺序报告至多 count 个元素（count 是元素个数，不是地址区间）。
    let mut all: Vec<(u8, u16, Option<String>, Option<String>)> = Vec::new();
    all.push((1, TRANSPORT_START, None, None));
    for (i, d) in st.drives.iter().enumerate() {
        all.push((
            4,
            DT_START + i as u16,
            d.cartridge.clone(),
            Some(d.serial.clone()),
        ));
    }
    for (i, s) in st.ie.iter().enumerate() {
        all.push((3, IE_START + i as u16, s.clone(), None));
    }
    for (i, s) in st.storage.iter().enumerate() {
        all.push((2, STORAGE_START + i as u16, s.clone(), None));
    }
    all.sort_by_key(|e| e.1);

    // (type code, [(addr, barcode, drive serial)])
    let mut groups: Vec<(u8, Vec<ElemEntry>)> = Vec::new();
    for (t, addr, bc, id) in all
        .into_iter()
        .filter(|e| e.1 >= start && (want_type == 0 || want_type == e.0))
        .take(count as usize)
    {
        match groups.iter_mut().find(|g| g.0 == t) {
            Some(g) => g.1.push((addr, bc, id)),
            None => groups.push((t, vec![(addr, bc, id)])),
        }
    }

    let mut body = Vec::new();
    let mut total_elems = 0u16;
    let mut first_addr: Option<u16> = None;
    for (t, elems) in &groups {
        let base = if voltag { 48 } else { 12 };
        let id_extra = if dvcid && *t == 4 { 4 + 10 } else { 0 };
        let desc_len = (base + id_extra) as u16;
        let mut page = vec![*t, if voltag { 0x80 } else { 0x00 }];
        page.extend_from_slice(&desc_len.to_be_bytes());
        page.push(0);
        let bytes = desc_len as u32 * elems.len() as u32;
        page.extend_from_slice(&bytes.to_be_bytes()[1..]);
        for (addr, bc, id) in elems {
            first_addr.get_or_insert(*addr);
            total_elems += 1;
            let mut d = vec![0u8; desc_len as usize];
            d[0..2].copy_from_slice(&addr.to_be_bytes());
            d[2] = if bc.is_some() { 0x01 } else { 0x00 };
            if voltag {
                let mut tag = [b' '; 36];
                tag[32..].fill(0);
                if let Some(b) = bc {
                    let n = b.len().min(32);
                    tag[..n].copy_from_slice(&b.as_bytes()[..n]);
                } else {
                    tag[..32].fill(0);
                }
                d[12..48].copy_from_slice(&tag);
            }
            if id_extra > 0 {
                let off = base;
                d[off] = 0x02;
                d[off + 3] = 10;
                let s = id.clone().unwrap_or_default();
                let n = s.len().min(10);
                d[off + 4..off + 4 + n].copy_from_slice(&s.as_bytes()[..n]);
            }
            page.extend_from_slice(&d);
        }
        body.extend_from_slice(&page);
    }
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&first_addr.unwrap_or(start).to_be_bytes());
    v.extend_from_slice(&total_elems.to_be_bytes());
    v.push(0);
    v.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    v.extend_from_slice(&body);
    let n = copy_out(out, &v, alloc);
    good(n)
}

// ---------- 持久预留 ----------

fn pr_in(pr: &PrState, cdb: &[u8], out: Option<&mut [u8]>) -> Result<Completion> {
    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
    let mut d = pr.generation.to_be_bytes().to_vec();
    match cdb[1] & 0x1F {
        0x00 => {
            d.extend_from_slice(&((pr.registrations.len() * 8) as u32).to_be_bytes());
            for k in pr.registrations.values() {
                d.extend_from_slice(&k.to_be_bytes());
            }
        }
        0x01 => match pr.holder {
            Some((h, t)) => {
                d.extend_from_slice(&16u32.to_be_bytes());
                d.extend_from_slice(&pr.registrations.get(&h).copied().unwrap_or(0).to_be_bytes());
                d.extend_from_slice(&[0, 0, 0, 0, 0, t & 0x0F, 0, 0]);
            }
            None => d.extend_from_slice(&0u32.to_be_bytes()),
        },
        _ => return illegal_request(),
    }
    let n = copy_out(out, &d, alloc);
    good(n)
}

fn pr_out(pr: &mut PrState, me: u32, cdb: &[u8], data: Option<&[u8]>) -> Result<Completion> {
    let conflict = || {
        Ok(Completion { status: 0x18, sense: SenseInfo::from_bytes(&[]), transferred: 0 })
    };
    let Some(p) = data.filter(|p| p.len() >= 24) else {
        return illegal_request();
    };
    let rk = u64::from_be_bytes(p[0..8].try_into().expect("8 bytes"));
    let sark = u64::from_be_bytes(p[8..16].try_into().expect("8 bytes"));
    let sa = cdb[1] & 0x1F;
    let typ = cdb[2] & 0x0F;
    let mine = pr.registrations.get(&me).copied();
    // 除注册类动作外，发起方必须已注册且 rk 等于自己的键
    if !matches!(sa, 0x00 | 0x06) && mine != Some(rk) {
        return conflict();
    }
    match sa {
        0x00 | 0x06 => {
            if sa == 0x00 && mine.is_some() && mine != Some(rk) {
                return conflict();
            }
            if sark == 0 {
                pr.registrations.remove(&me);
                if pr.holder.map(|(h, _)| h) == Some(me) {
                    pr.holder = None;
                }
            } else {
                pr.registrations.insert(me, sark);
            }
            pr.generation += 1;
        }
        0x01 => match pr.holder {
            Some((h, _)) if h != me => return conflict(),
            _ => pr.holder = Some((me, typ)),
        },
        0x02 => {
            if pr.holder.map(|(h, _)| h) == Some(me) {
                pr.holder = None;
            }
        }
        0x03 => {
            let others: Vec<u32> = pr.registrations.keys().copied().filter(|&i| i != me).collect();
            for i in others {
                pr.ua.entry(i).or_default().push((0x2A, 0x03));
            }
            pr.registrations.clear();
            pr.holder = None;
            pr.generation += 1;
        }
        0x04 | 0x05 => {
            let victims: Vec<u32> = pr
                .registrations
                .iter()
                .filter(|&(&i, &k)| k == sark && i != me)
                .map(|(&i, _)| i)
                .collect();
            if victims.is_empty() {
                return conflict();
            }
            let holder_preempted = pr.holder.is_some_and(|(h, _)| victims.contains(&h));
            for v in &victims {
                pr.registrations.remove(v);
                pr.ua.entry(*v).or_default().push((0x2A, 0x03));
            }
            if holder_preempted {
                pr.holder = Some((me, typ));
            }
            pr.generation += 1;
        }
        _ => return illegal_request(),
    }
    good(p.len())
}

// ---------- 驱动器 ----------

fn drive_command(
    st: &mut LibState,
    idx: usize,
    initiator: u32,
    cdb: &[u8],
    data_in: Option<&[u8]>,
    out: Option<&mut [u8]>,
) -> Result<Completion> {
    if idx >= st.drives.len() {
        return Err(TapeError::NotReady(format!("sim drive {} 不存在", idx)));
    }
    let op = cdb[0];
    if op != opcode::INQUIRY && op != opcode::REQUEST_SENSE && !st.drives[idx].pending_ua.is_empty()
    {
        let (asc, ascq) = st.drives[idx].pending_ua.remove(0);
        return check(0x06, asc, ascq);
    }
    if op != opcode::INQUIRY && op != opcode::REQUEST_SENSE {
        if let Some(q) = st.drives[idx].pr.ua.get_mut(&initiator) {
            if !q.is_empty() {
                let (asc, ascq) = q.remove(0);
                return check(0x06, asc, ascq);
            }
        }
    }
    match op {
        opcode::PERSISTENT_RESERVE_IN => return pr_in(&st.drives[idx].pr, cdb, out),
        opcode::PERSISTENT_RESERVE_OUT => {
            return pr_out(&mut st.drives[idx].pr, initiator, cdb, data_in);
        }
        _ => {}
    }
    // Exclusive Access：非持有者的其余命令一律 RESERVATION CONFLICT（INQUIRY 等已在上面放行）
    if let Some((h, _)) = st.drives[idx].pr.holder {
        if h != initiator && op != opcode::INQUIRY && op != opcode::REQUEST_SENSE {
            return Ok(Completion {
                status: 0x18,
                sense: SenseInfo::from_bytes(&[]),
                transferred: 0,
            });
        }
    }
    match op {
        opcode::INQUIRY => {
            let serial = st.drives[idx].serial.clone();
            return inquiry_response(0x01, "TAPE-RS DRIVE", &serial, cdb, out);
        }
        opcode::PREVENT_ALLOW_MEDIUM_REMOVAL | opcode::SEND_DIAGNOSTIC => return good(0),
        opcode::RECEIVE_DIAGNOSTIC_RESULTS | opcode::REPORT_DENSITY_SUPPORT => {
            let alloc = u16::from_be_bytes([cdb[3], cdb[4]]) as usize;
            let n = copy_out(out, &[0u8; 4], alloc);
            return good(n);
        }
        opcode::LOG_SENSE => {
            let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
            let page = cdb[2] & 0x3F;
            let n = copy_out(out, &[page, 0, 0, 0], alloc);
            return good(n);
        }
        opcode::MODE_SELECT_10 => {
            let Some(p) = data_in else {
                return illegal_request();
            };
            if p.len() >= 12 && p[8] == 0x11 {
                st.drives[idx].pending_partitions = 1 + p[11] as usize;
            }
            return good(p.len());
        }
        opcode::MODE_SENSE_10 => {
            if cdb[2] & 0x3F != 0x11 {
                return illegal_request();
            }
            let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
            let mut v = vec![0u8; 8];
            v[0..2].copy_from_slice(&(6u16 + 16).to_be_bytes());
            let mut page = [0u8; 16];
            page[0] = 0x11;
            page[1] = 0x0E;
            page[2] = 3;
            page[3] = st.drives[idx].pending_partitions.saturating_sub(1) as u8;
            page[4] = 0x3C;
            v.extend_from_slice(&page);
            let n = copy_out(out, &v, alloc);
            return good(n);
        }
        opcode::LOAD_UNLOAD => {
            let load = cdb[4] & 0x01 != 0;
            if st.drives[idx].cartridge.is_none() {
                return check(SENSE_KEY_NOT_READY, 0x3A, 0x00);
            }
            if !load {
                st.drive_flush(idx);
            }
            let d = &mut st.drives[idx];
            d.loaded = load;
            d.partition = 0;
            d.pos = 0;
            return good(0);
        }
        _ => {}
    }

    // 以下命令都要求介质已装载
    let (Some(bc), true) = (st.drives[idx].cartridge.clone(), st.drives[idx].loaded) else {
        return check(SENSE_KEY_NOT_READY, 0x3A, 0x00);
    };
    let quirks = st.quirks;
    let mut drive = st.drives[idx].clone();
    let cart = st
        .cartridges
        .get_mut(&bc)
        .expect("cartridge in drive must exist");
    let result = drive_media_command(&mut drive, cart, cdb, data_in, out, quirks);
    st.drives[idx] = drive;
    result
}

fn drive_media_command(
    d: &mut DriveState,
    cart: &mut SimCartridge,
    cdb: &[u8],
    data_in: Option<&[u8]>,
    out: Option<&mut [u8]>,
    quirks: SimQuirks,
) -> Result<Completion> {
    let op = cdb[0];
    let part_idx = d.partition as usize;
    if part_idx >= cart.partitions.len() {
        return check(SENSE_KEY_ILLEGAL_REQUEST, 0x24, 0x00);
    }
    match op {
        opcode::TEST_UNIT_READY => good(0),
        opcode::REWIND => {
            cart.partitions[part_idx].flushed = cart.partitions[part_idx].objects.len();
            d.pos = 0;
            good(0)
        }
        opcode::READ_POSITION => {
            let p = &cart.partitions[part_idx];
            let mut v = [0u8; 20];
            if d.pos == 0 {
                v[0] |= 0x80;
            }
            if d.pos >= p.objects.len() {
                v[0] |= 0x04; // 自定义：位于 EOD（真实设备无此位，仅供调试）
            }
            v[1] = d.partition;
            let blk = (d.pos as u32).to_be_bytes();
            v[4..8].copy_from_slice(&blk);
            v[8..12].copy_from_slice(&blk);
            let n = copy_out(out, &v, 20);
            good(n)
        }
        opcode::READ_6 => {
            if cdb[1] & 0x01 != 0 {
                return illegal_request();
            }
            let want = u24(&cdb[2..5]) as usize;
            let p = &cart.partitions[part_idx];
            if d.pos >= p.objects.len() {
                return check(SENSE_KEY_BLANK_CHECK, 0x00, 0x05);
            }
            match &p.objects[d.pos] {
                LogicalObject::Filemark => {
                    d.pos += 1;
                    if quirks.filemark_without_residual {
                        // Holo-VTL：仅 FM 位，无 residual（缓冲区内容不变）
                        hint_flags(
                            0x00,
                            0x00,
                            0x80,
                            None,
                            want.min(out.map(|b| b.len()).unwrap_or(0)),
                        )
                    } else {
                        hint_flags(0x00, 0x01, 0x80, Some(want as i32), 0)
                    }
                }
                LogicalObject::Record(data) => {
                    d.pos += 1;
                    let n = copy_out(out, data, want);
                    if data.len() != want {
                        // 变长读且记录长度 ≠ 请求长度：ILI + INFORMATION = 请求 − 实际（可为负）
                        hint_flags(0x00, 0x00, 0x20, Some(want as i32 - data.len() as i32), n)
                    } else {
                        good(n)
                    }
                }
            }
        }
        opcode::WRITE_6 => {
            if cdb[1] & 0x01 != 0 {
                return illegal_request();
            }
            if cart.write_protected {
                return check(SENSE_KEY_DATA_PROTECT, 0x27, 0x00);
            }
            let want = u24(&cdb[2..5]) as usize;
            let Some(data) = data_in else {
                return illegal_request();
            };
            if data.len() != want {
                return illegal_request();
            }
            let p = &mut cart.partitions[part_idx];
            p.truncate_at(d.pos);
            if p.used_bytes() + data.len() as u64 > p.capacity {
                return check(SENSE_KEY_VOLUME_OVERFLOW, 0x00, 0x02);
            }
            p.objects.push(LogicalObject::Record(data.to_vec()));
            d.pos += 1;
            if !quirks.vcr_never_changes {
                cart.vcr = cart.vcr.wrapping_add(1).max(1);
            }
            good(data.len())
        }
        opcode::WRITE_FILEMARKS_6 => {
            if cart.write_protected {
                return check(SENSE_KEY_DATA_PROTECT, 0x27, 0x00);
            }
            let count = u24(&cdb[2..5]) as usize;
            let p = &mut cart.partitions[part_idx];
            p.truncate_at(d.pos);
            for _ in 0..count {
                p.objects.push(LogicalObject::Filemark);
            }
            d.pos += count;
            p.flushed = p.objects.len();
            if count > 0 && !quirks.vcr_never_changes {
                cart.vcr = cart.vcr.wrapping_add(1).max(1);
            }
            good(0)
        }
        opcode::SPACE_6 => {
            let code = cdb[1] & 0x07;
            let count = i24(&cdb[2..5]);
            let p = &mut cart.partitions[part_idx];
            p.flushed = p.objects.len();
            match code {
                0x00 => {
                    let mut remaining = count;
                    while remaining > 0 {
                        if d.pos >= p.objects.len() {
                            return check(SENSE_KEY_BLANK_CHECK, 0x00, 0x05);
                        }
                        let is_fm = p.objects[d.pos] == LogicalObject::Filemark;
                        d.pos += 1;
                        if is_fm {
                            return hint_flags(0x00, 0x01, 0x80, None, 0);
                        }
                        remaining -= 1;
                    }
                    while remaining < 0 {
                        if d.pos == 0 {
                            return if quirks.bop_space_illegal_request {
                                illegal_request()
                            } else {
                                hint_flags(0x00, 0x04, 0x40, None, 0)
                            };
                        }
                        d.pos -= 1;
                        if p.objects[d.pos] == LogicalObject::Filemark {
                            return hint_flags(0x00, 0x01, 0x80, None, 0);
                        }
                        remaining += 1;
                    }
                    good(0)
                }
                0x01 => {
                    let mut remaining = count;
                    let start_pos = d.pos;
                    while remaining > 0 {
                        if d.pos >= p.objects.len() {
                            if quirks.eod_space_illegal_request {
                                d.pos = start_pos;
                                return illegal_request();
                            }
                            return check(SENSE_KEY_BLANK_CHECK, 0x00, 0x05);
                        }
                        let is_fm = p.objects[d.pos] == LogicalObject::Filemark;
                        d.pos += 1;
                        if is_fm {
                            remaining -= 1;
                        }
                    }
                    while remaining < 0 {
                        if d.pos == 0 {
                            return if quirks.bop_space_illegal_request {
                                illegal_request()
                            } else {
                                hint_flags(0x00, 0x04, 0x40, None, 0)
                            };
                        }
                        d.pos -= 1;
                        if p.objects[d.pos] == LogicalObject::Filemark {
                            remaining += 1;
                        }
                    }
                    good(0)
                }
                0x03 => {
                    d.pos = p.objects.len();
                    good(0)
                }
                _ => illegal_request(),
            }
        }
        opcode::LOCATE_16 => {
            let cp = cdb[1] & 0x02 != 0;
            let partition = cdb[3];
            let block = u64::from_be_bytes(cdb[4..12].try_into().unwrap());
            cart.partitions[part_idx].flushed = cart.partitions[part_idx].objects.len();
            let target_part = if cp { partition as usize } else { part_idx };
            if target_part >= cart.partitions.len() {
                return illegal_request();
            }
            if block > cart.partitions[target_part].objects.len() as u64 {
                return check(SENSE_KEY_BLANK_CHECK, 0x00, 0x05);
            }
            d.partition = target_part as u8;
            d.pos = block as usize;
            good(0)
        }
        opcode::ERASE_6 => {
            if cart.write_protected {
                return check(SENSE_KEY_DATA_PROTECT, 0x27, 0x00);
            }
            let p = &mut cart.partitions[part_idx];
            p.truncate_at(d.pos);
            p.flushed = p.objects.len();
            if !quirks.vcr_never_changes {
                cart.vcr = cart.vcr.wrapping_add(1).max(1);
            }
            good(0)
        }
        opcode::FORMAT_MEDIUM => {
            if cart.write_protected {
                return check(SENSE_KEY_DATA_PROTECT, 0x27, 0x00);
            }
            let n = d.pending_partitions.max(1);
            let total = cart.capacity;
            cart.partitions = if n == 1 {
                vec![SimPartition::empty(total)]
            } else {
                let p0 = (total / 32).max(1 << 20);
                let mut v = vec![SimPartition::empty(p0)];
                let rest = total.saturating_sub(p0) / (n as u64 - 1);
                for _ in 1..n {
                    v.push(SimPartition::empty(rest));
                }
                v
            };
            d.partition = 0;
            d.pos = 0;
            if !quirks.vcr_never_changes {
                cart.vcr = cart.vcr.wrapping_add(1).max(1);
            }
            // MODE PARAMETERS CHANGED（与 Holo-VTL 实测一致）
            d.pending_ua.push((0x2A, 0x01));
            good(0)
        }
        opcode::READ_ATTRIBUTE => {
            let sa = cdb[1] & 0x1F;
            if sa != 0x00 {
                return illegal_request();
            }
            let partition = cdb[7];
            let first = u16::from_be_bytes([cdb[8], cdb[9]]);
            let alloc = u32::from_be_bytes(cdb[10..14].try_into().unwrap()) as usize;
            let mut entries: Vec<(u16, u8, Vec<u8>)> = Vec::new();
            if let Some(p) = cart.partitions.get(partition as usize) {
                const MIB: u64 = 1 << 20;
                let remaining = p.capacity.saturating_sub(p.used_bytes()) / MIB;
                let maximum = p.capacity / MIB;
                entries.push((0x0000, 0, remaining.to_be_bytes().to_vec()));
                entries.push((0x0001, 0, maximum.to_be_bytes().to_vec()));
            }
            entries.push((0x0009, 0, cart.vcr.to_be_bytes().to_vec()));
            for ((pt, id), (fmt, val)) in cart.mam.iter() {
                if *pt == partition {
                    entries.push((*id, *fmt, val.clone()));
                }
            }
            entries.sort_by_key(|e| e.0);
            let mut body = Vec::new();
            for (id, fmt, val) in entries.into_iter().filter(|e| e.0 >= first) {
                body.extend_from_slice(&id.to_be_bytes());
                body.push(fmt & 0x03);
                body.extend_from_slice(&(val.len() as u16).to_be_bytes());
                body.extend_from_slice(&val);
            }
            let mut v = (body.len() as u32).to_be_bytes().to_vec();
            v.extend_from_slice(&body);
            let n = copy_out(out, &v, alloc);
            good(n)
        }
        opcode::WRITE_ATTRIBUTE => {
            if cart.write_protected {
                return check(SENSE_KEY_DATA_PROTECT, 0x27, 0x00);
            }
            let partition = cdb[7];
            let Some(p) = data_in else {
                return illegal_request();
            };
            if p.len() < 4 {
                return illegal_request();
            }
            let mut off = 4;
            while off + 5 <= p.len() {
                let id = u16::from_be_bytes([p[off], p[off + 1]]);
                let fmt = p[off + 2] & 0x03;
                let len = u16::from_be_bytes([p[off + 3], p[off + 4]]) as usize;
                let start = off + 5;
                if start + len > p.len() {
                    return illegal_request();
                }
                if id < 0x0400 {
                    // 设备属性（含 0x0009 VCR）由驱动器维护，主机只读
                    return check(SENSE_KEY_ILLEGAL_REQUEST, 0x26, 0x00);
                }
                if len == 0 {
                    cart.mam.remove(&(partition, id));
                } else {
                    cart.mam
                        .insert((partition, id), (fmt, p[start..start + len].to_vec()));
                }
                off = start + len;
            }
            good(p.len())
        }
        _ => invalid_opcode(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::cdb;

    fn one_drive() -> (SimLibrary, SimTransport) {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("T00001L8", 8 << 20), 0)
            .unwrap();
        lib.load_into_drive("T00001L8", 0).unwrap();
        let dev = lib.drive(0);
        (lib, dev)
    }

    fn read_pos(dev: &SimTransport) -> (u8, u32) {
        let mut buf = [0u8; 20];
        dev.execute_read(&cdb::read_position(), &mut buf, 0)
            .unwrap();
        (buf[1], u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]))
    }

    #[test]
    fn write_read_filemark_and_eod() {
        let (_lib, dev) = one_drive();
        dev.execute_write(&cdb::write_6(false, 3), b"abc", 0)
            .unwrap();
        dev.execute_no_data(&cdb::write_filemarks(1), 0).unwrap();
        dev.execute_write(&cdb::write_6(false, 2), b"xy", 0)
            .unwrap();
        assert_eq!(read_pos(&dev), (0, 3));

        dev.execute_no_data(&cdb::rewind(), 0).unwrap();
        let mut buf = [0u8; 16];
        let r = dev
            .execute_read(&cdb::read_6(false, 16), &mut buf, 0)
            .unwrap();
        assert_eq!(&buf[..r.transferred], b"abc");
        // filemark：与真实设备一致，Ok 且 transferred = 0
        let r = dev
            .execute_read(&cdb::read_6(false, 16), &mut buf, 0)
            .unwrap();
        assert_eq!(r.transferred, 0);
        assert_eq!((r.sense.asc, r.sense.ascq), (0x00, 0x01));
        let r = dev
            .execute_read(&cdb::read_6(false, 16), &mut buf, 0)
            .unwrap();
        assert_eq!(&buf[..r.transferred], b"xy");
        // EOD → BLANK CHECK
        let e = dev
            .execute_read(&cdb::read_6(false, 16), &mut buf, 0)
            .unwrap_err();
        assert!(matches!(
            e,
            TapeError::ScsiCommand {
                sense_key: 0x08,
                ..
            }
        ));
    }

    #[test]
    fn space_eod_and_back_filemark() {
        let (_lib, dev) = one_drive();
        for _ in 0..3 {
            dev.execute_write(&cdb::write_6(false, 1), b"z", 0).unwrap();
        }
        dev.execute_no_data(&cdb::write_filemarks(1), 0).unwrap();
        dev.execute_write(&cdb::write_6(false, 1), b"q", 0).unwrap();
        dev.execute_no_data(&cdb::rewind(), 0).unwrap();
        dev.execute_no_data(&cdb::space(0x03, 0), 0).unwrap();
        assert_eq!(read_pos(&dev), (0, 5));
        dev.execute_no_data(&cdb::space(0x01, -1), 0).unwrap();
        // 反向跨过 filemark 后停在其 BOP 侧
        assert_eq!(read_pos(&dev), (0, 3));
        dev.execute_no_data(&cdb::locate_16(0, 1, false, false), 0)
            .unwrap();
        assert_eq!(read_pos(&dev), (0, 1));
    }

    #[test]
    fn power_cut_drops_unflushed() {
        let (lib, dev) = one_drive();
        dev.execute_write(&cdb::write_6(false, 1), b"a", 0).unwrap();
        dev.execute_no_data(&cdb::write_filemarks(0), 0).unwrap(); // 仅刷缓冲
        dev.execute_write(&cdb::write_6(false, 1), b"b", 0).unwrap();
        lib.power_cut();
        let c = lib.cartridge("T00001L8").unwrap();
        assert_eq!(
            c.partitions[0].objects,
            vec![LogicalObject::Record(b"a".to_vec())]
        );
    }

    #[test]
    fn fault_injection_once() {
        let (_lib, dev) = one_drive();
        dev.inject(Fault {
            opcode: opcode::WRITE_6,
            action: FaultAction::CheckCondition {
                sense_key: 0x03,
                asc: 0x0C,
                ascq: 0x00,
            },
            after: 0,
            remaining: 1,
        });
        let e = dev
            .execute_write(&cdb::write_6(false, 1), b"a", 0)
            .unwrap_err();
        assert!(matches!(
            e,
            TapeError::ScsiCommand {
                sense_key: 0x03,
                ..
            }
        ));
        dev.execute_write(&cdb::write_6(false, 1), b"a", 0).unwrap();
        assert_eq!(dev.command_log(), vec![opcode::WRITE_6, opcode::WRITE_6]);
    }

    #[test]
    fn mode_select_then_format_creates_partitions() {
        let (lib, dev) = one_drive();
        let mut params = [0u8; 24];
        params[8] = 0x11;
        params[9] = 0x0E;
        params[11] = 0x01;
        dev.execute_write(&cdb::mode_select_10(true, false, 24), &params, 0)
            .unwrap();
        dev.execute_no_data(&cdb::format_medium(1, false, false), 0)
            .unwrap();
        assert_eq!(lib.cartridge("T00001L8").unwrap().partitions.len(), 2);
        // FORMAT 之后第一条命令得到 UNIT ATTENTION（未执行），重发才执行
        let e = dev
            .execute_no_data(&cdb::locate_16(1, 0, true, false), 0)
            .unwrap_err();
        assert!(matches!(
            e,
            TapeError::ScsiCommand {
                sense_key: 0x06,
                asc: 0x2A,
                ascq: 0x01,
                ..
            }
        ));
        dev.execute_no_data(&cdb::locate_16(1, 0, true, false), 0)
            .unwrap();
        assert_eq!(read_pos(&dev).0, 1);
    }
}
