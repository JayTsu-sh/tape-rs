//! D02 恢复协议：末构造定位、尾部分类、回指链核验与安全追加位置判定。
//!
//! 对应 `.scratch/ltfs-ha/recovery-tail-protocol.md`。只读核验，不修改介质：
//!
//! 1. 每个分区 `SPACE` 到 EOD 取 `E`，反向按文件标记步进找到最后一个构造
//!    `[FM][records][FM]`，解析并核验自指针（卷 UUID、分区、起始块）。
//!    自指针不符的按 LTFS 2.5.1 §5.4.2 视为数据 extent，继续向前找。
//! 2. 末索引之后到 `E` 的区域分类为 T0 完整 / T1 未索引数据 / T2 残缺索引 /
//!    T3 假索引 / T4 未知。
//! 3. 沿 `previousgenerationlocation` 核验 DP 回指链（本实现只写 Full 索引）。
//! 4. 只有 DP 与 IP 尾部都完整、链核验通过、追加位置与末索引位置算术一致且
//!    LOCATE 后 READ POSITION 复核一致，才判定可以追加。
//!
//! 预算耗尽或证据不足时给出受限结论，不猜测、不试写。
//!
//! 快速路径（§10.3）：读介质 VCR 与各分区 VCI；某分区 VCI 中的 VCR 与当前 VCR
//! 相等且有效时，其 block number 作为该分区末索引的定位提示——仍要读取并核验
//! 实际构造，提示无效即退回标准反向搜索。EOD 与尾部分类不因快速路径省略。

use bytes::BytesMut;
use log::{debug, info, warn};

use crate::error::{Result, TapeError};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

use super::index::{IndexLocation, LtfsIndex};
use super::label::LtfsLabel;
use super::mam::{Mam, vcr_is_valid, vcr_matches};
use super::volume::P1_DATA_START;

/// 末索引之后区域的分类（协议中的 T0—T4，另加"尚无索引的空数据区"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailKind {
    /// T0：末索引的结束文件标记紧接 EOD。
    Complete,
    /// 分区只有标签构造、尚无任何索引且无数据（本项目 mkltfs 历史布局）。
    EmptyData,
    /// T1：末索引之后有数据记录但没有索引构造。
    UnindexedData,
    /// T2：末索引之后有文件标记 + 索引前缀，缺结束文件标记。
    TruncatedIndex,
    /// T3：末索引之后存在结构完整但自指针不符的构造。
    FakeIndex,
    /// T4：EOD 不可靠、读错或位置核对失败。
    Unknown,
}

impl TailKind {
    pub fn allows_append(self) -> bool {
        matches!(self, TailKind::Complete | TailKind::EmptyData)
    }
}

/// 经核验的索引候选。
#[derive(Debug, Clone)]
pub struct IndexCandidate {
    /// 索引第一条记录的块号（自指针 startblock）。
    pub start_block: u64,
    /// 结束文件标记的块号。
    pub end_block: u64,
    pub index: LtfsIndex,
}

/// 搜索预算。数值默认值仅为开发默认，正式值由 08/06 测量后确定。
#[derive(Debug, Clone)]
pub struct RecoveryBudget {
    /// 每分区反向步进（构造）次数上限。
    pub max_back_steps: u32,
    /// 单个索引构造允许读取的最大字节数。
    pub max_index_bytes: usize,
    /// 回指链核验深度上限；超出时记为"部分核验"。
    pub max_chain_len: u32,
}

impl Default for RecoveryBudget {
    fn default() -> Self {
        Self {
            max_back_steps: 8,
            max_index_bytes: 64 << 20,
            max_chain_len: 64,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PartitionScan {
    pub partition: u8,
    pub eod: u64,
    pub last_index: Option<IndexCandidate>,
    pub tail: TailKind,
    /// 末索引由 VCI 快速路径直接定位（未做反向步进）。
    pub hint_used: bool,
    pub back_steps: u32,
    pub budget_exhausted: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub dp: PartitionScan,
    pub ip: PartitionScan,
    pub chain_ok: bool,
    /// 已核验的回指深度（不含末索引本身）。
    pub chain_depth: u32,
    /// 最新 DP Full 之后已重放的连续增量数。
    pub incremental_depth: u32,
    pub chain_truncated_by_budget: bool,
    /// IP 落后于 DP（允许，记为维护债务）。
    pub ip_debt: bool,
    pub append_ok: bool,
    pub append_position: Option<u64>,
    pub notes: Vec<String>,
}

impl RecoveryReport {
    /// 当前视图：DP 末索引优先，其次 IP 末索引。
    pub fn view(&self) -> Option<&IndexCandidate> {
        if self.ip_ahead() {
            return self.ip.last_index.as_ref();
        }
        self.dp.last_index.as_ref().or(self.ip.last_index.as_ref())
    }

    /// IP 末索引比 DP 新，且它的回指针正好指向 DP 末索引。这是规范定义的一致卷
    /// （§4 "consistent"：IP 末索引回指 DP 上最后一个完整索引）。IBM LTFS 在只有
    /// 元数据变化、或挂载时补写 IP 时会留下这种状态，此时视图取 IP。
    pub fn ip_ahead(&self) -> bool {
        match (&self.dp.last_index, &self.ip.last_index) {
            (Some(d), Some(i)) => {
                i.index.generation > d.index.generation
                    && if d.index.incremental {
                        i.index.previous_incremental_location == Some(d.index.self_location)
                            && i.index.previous_location == d.index.previous_location
                    } else {
                        i.index.previous_location == Some(d.index.self_location)
                    }
            }
            _ => false,
        }
    }

    /// 受限原因；`None` 表示可以发布当前视图（可读）。
    pub fn restricted_reason(&self) -> Option<String> {
        if self.dp.last_index.is_none() && self.dp.budget_exhausted {
            // DP 上可能还有未检查到的更新索引，IP 视图不能证明当前性
            return Some("DP 搜索预算耗尽，未能证明 IP 视图为最新".to_string());
        }
        if self.view().is_none() {
            return Some(if self.dp.budget_exhausted || self.ip.budget_exhausted {
                "搜索预算耗尽，未找到有效索引".to_string()
            } else {
                "两个分区都没有有效索引".to_string()
            });
        }
        if !self.chain_ok {
            return Some("DP 回指链核验失败".to_string());
        }
        if let (Some(d), Some(i)) = (&self.dp.last_index, &self.ip.last_index)
            && i.index.generation > d.index.generation
            && !self.ip_ahead()
        {
            return Some(format!(
                "IP 索引代数 {} 高于 DP 代数 {}，且 IP 回指针不指向 DP 末索引，两分区冲突",
                i.index.generation, d.index.generation
            ));
        }
        None
    }
}

// ---------- 设备原语 ----------

/// 反向跨过一个文件标记，返回该文件标记的块号；到达 BOP 返回 `None`。
fn back_fm(drive: &TapeDrive<'_>) -> Result<Option<u64>> {
    match drive.space_filemarks(-1) {
        Ok(()) => {}
        // 物理驱动器在 BOP 处返回 NO SENSE + EOM（经 finish_command 为 Ok）；
        // Holo-VTL 实测返回 ILLEGAL REQUEST 24/00。SPACE(-1 filemark) 的 CDB 是固定且
        // 合法的，这个错误只可能来自"前面已无文件标记"，按到达 BOP 处理。
        Err(TapeError::ScsiCommand {
            sense_key: 0x05,
            asc: 0x24,
            ..
        }) => {
            debug!("反向 SPACE 被拒（ILLEGAL REQUEST 24/00），按 BOP 处理");
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let pos = drive.read_position()?;
    if pos.block_number == 0 {
        Ok(None)
    } else {
        Ok(Some(pos.block_number))
    }
}

/// 从 `start` 读记录直到文件标记或 EOD。返回 (字节, 是否以文件标记结束, 结束后位置)。
fn read_until_fm(
    drive: &TapeDrive<'_>,
    partition: u8,
    start: u64,
    block_size: usize,
    max_bytes: usize,
) -> Result<(BytesMut, bool, u64)> {
    drive.locate(partition, start, true)?;
    let mut out = BytesMut::new();
    let mut buf = vec![0u8; block_size];
    let mut next = start;
    loop {
        match drive.read_block(&mut buf) {
            Ok(0) => {
                next += 1;
                return Ok((out, true, next));
            }
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                next += 1;
                if out.len() > max_bytes {
                    return Err(TapeError::RecoveryRestricted {
                        reason: format!("构造超过预算 {} 字节", max_bytes),
                    });
                }
            }
            Err(TapeError::ScsiCommand {
                sense_key: 0x08, ..
            }) => return Ok((out, false, next)),
            Err(e) => return Err(e),
        }
    }
}

fn looks_like_index(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    [b"<ltfsindex".as_slice(), b"<ltfsincrementalindex".as_slice()].iter().any(|tag| head.windows(tag.len()).any(|w| w == *tag))
}

fn validate_candidate(
    idx: &LtfsIndex,
    label: &LtfsLabel,
    part_char: char,
    start_block: u64,
) -> std::result::Result<(), String> {
    if idx.incremental && part_char != label.data_partition {
        return Err("增量索引只允许位于 DP".into());
    }
    if idx.previous_incremental_location.is_some_and(|p| p.partition != label.data_partition || (part_char == label.data_partition && p.start_block >= start_block)) {
        return Err("增量回指分区或方向非法".into());
    }
    if idx.volume_uuid != label.volume_uuid {
        return Err(format!("卷 UUID 不符: {}", idx.volume_uuid));
    }
    if idx.self_location.partition != part_char || idx.self_location.start_block != start_block {
        return Err(format!(
            "自指针不符: 声明 {}@{}, 实际 {}@{}",
            idx.self_location.partition, idx.self_location.start_block, part_char, start_block
        ));
    }
    if let Some(prev) = idx.previous_location
        && prev.partition == part_char
        && prev.start_block >= start_block
    {
        return Err("回指指向自身或更后位置".to_string());
    }
    Ok(())
}

// ---------- 分区扫描 ----------

/// 标准路径：定位 EOD，反向找末索引构造，分类尾部。
pub fn scan_partition(
    drive: &TapeDrive<'_>,
    partition: u8,
    part_char: char,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: Option<u64>,
) -> Result<PartitionScan> {
    let block_size = label.blocksize as usize;
    let mut notes = Vec::new();

    drive.locate(partition, 0, true)?;
    drive.space_to_eod()?;
    let pos = drive.read_position()?;
    if pos.partition != partition as u32 {
        return Ok(PartitionScan {
            partition,
            eod: pos.block_number,
            last_index: None,
            tail: TailKind::Unknown,
            hint_used: false,
            back_steps: 0,
            budget_exhausted: false,
            notes: vec![format!(
                "READ POSITION 分区 {} 与预期 {} 不符",
                pos.partition, partition
            )],
        });
    }
    let eod = pos.block_number;
    debug!("分区 {} EOD = {}", partition, eod);

    let mut last_index: Option<IndexCandidate> = None;
    let mut back_steps = 0u32;
    let mut budget_exhausted = false;
    let mut saw_fake = false;
    let mut hint_used = false;

    // 快速路径：VCI 提示的块必须是有效索引构造 [FM][records][FM] 且自指针等于该块。
    if let Some(b) = hint {
        if b == 0 || b >= eod {
            notes.push(format!("VCI 提示块 {} 超出分区范围 (EOD {})", b, eod));
        } else {
            match read_until_fm(drive, partition, b, block_size, budget.max_index_bytes) {
                Ok((bytes, true, next)) if looks_like_index(&bytes) => {
                    match LtfsIndex::parse(&bytes) {
                        Ok(idx) => match validate_candidate(&idx, label, part_char, b) {
                            Ok(()) => {
                                info!(
                                    "分区 {} VCI 快速路径命中 gen={} @ {}..{}",
                                    partition,
                                    idx.generation,
                                    b,
                                    next - 1
                                );
                                last_index = Some(IndexCandidate {
                                    start_block: b,
                                    end_block: next - 1,
                                    index: idx,
                                });
                                hint_used = true;
                            }
                            Err(why) => notes.push(format!("VCI 提示 @{} 无效: {}", b, why)),
                        },
                        Err(e) => notes.push(format!("VCI 提示 @{} 解析失败: {}", b, e)),
                    }
                }
                Ok(_) => notes.push(format!("VCI 提示 @{} 不是完整索引构造", b)),
                Err(e) => notes.push(format!("VCI 提示 @{} 读取失败: {}", b, e)),
            }
            // 提示命中还不够：只有当提示索引的结束文件标记紧接 EOD 时，它才必然是末索引。
            // 否则（VCR 不随写入变化的设备，或 DP 索引已写完但 VCI 未及更新）提示之后可能还有
            // 更新的有效索引，必须退回标准反向搜索，不能漏掉已可靠提交的状态。
            // 实测依据：Holo-VTL 的 VCR（MAM 0x0009）在写入后不变化。
            if hint_used
                && let Some(c) = &last_index
                && c.end_block + 1 != eod
            {
                notes.push(format!(
                    "VCI 提示 @{} 之后仍有内容（结束于 {}，EOD {}），不采信，转标准路径",
                    c.start_block, c.end_block, eod
                ));
                last_index = None;
                hint_used = false;
            }
            if !hint_used {
                if !notes.iter().any(|n| n.contains("转标准路径")) {
                    notes.push("VCI 提示无效，转标准路径".to_string());
                }
                drive.locate(partition, eod, true)?;
            }
        }
    }

    // 相邻文件标记之间的区域交替为索引记录与数据 extent；从 EOD 反向逐区域检查。
    // `upper` 是当前区域的上界文件标记；第一次 back_fm 得到 EOD 之前的最后一个文件标记，
    // 其后的区域（upper+1 .. eod）是尾部，由 classify_tail 单独分类。
    let mut upper = if last_index.is_some() {
        None
    } else {
        back_fm(drive)?
    };
    while let Some(up) = upper {
        if back_steps >= budget.max_back_steps {
            budget_exhausted = true;
            notes.push(format!("反向步进达到预算 {}", budget.max_back_steps));
            break;
        }
        let p0 = back_fm(drive)?;
        back_steps += 1;
        let rec_start = p0.map(|p| p + 1).unwrap_or(0);
        let (bytes, ended_by_fm, next) = read_until_fm(
            drive,
            partition,
            rec_start,
            block_size,
            budget.max_index_bytes,
        )?;
        if !ended_by_fm || next != up + 1 {
            notes.push(format!("构造 [{}..{}] 边界核对失败", rec_start, up));
            saw_fake = true;
        } else if looks_like_index(&bytes) {
            match LtfsIndex::parse(&bytes) {
                Ok(idx) => match validate_candidate(&idx, label, part_char, rec_start) {
                    Ok(()) => {
                        info!(
                            "分区 {} 末索引 gen={} @ {}..{}",
                            partition, idx.generation, rec_start, up
                        );
                        last_index = Some(IndexCandidate {
                            start_block: rec_start,
                            end_block: up,
                            index: idx,
                        });
                        break;
                    }
                    Err(why) => {
                        notes.push(format!("候选 @{} 无效: {}", rec_start, why));
                        saw_fake = true;
                    }
                },
                Err(e) => {
                    notes.push(format!("候选 @{} 解析失败: {}", rec_start, e));
                    saw_fake = true;
                }
            }
        }
        match p0 {
            Some(p) => {
                drive.locate(partition, p, true)?;
                upper = Some(p);
            }
            None => break,
        }
    }

    let tail = classify_tail(
        drive,
        partition,
        &last_index,
        eod,
        block_size,
        budget,
        saw_fake,
        &mut notes,
    )?;
    if last_index.is_none() && budget_exhausted {
        notes.push("未找到末索引且预算耗尽：结论受限".to_string());
    }
    Ok(PartitionScan {
        partition,
        eod,
        last_index,
        tail,
        hint_used,
        back_steps,
        budget_exhausted,
        notes,
    })
}

#[allow(clippy::too_many_arguments)]
fn classify_tail(
    drive: &TapeDrive<'_>,
    partition: u8,
    last_index: &Option<IndexCandidate>,
    eod: u64,
    block_size: usize,
    budget: &RecoveryBudget,
    saw_fake: bool,
    notes: &mut Vec<String>,
) -> Result<TailKind> {
    let region_start = match last_index {
        Some(c) => c.end_block + 1,
        None => {
            if eod < P1_DATA_START {
                notes.push("分区短于标签构造".to_string());
                return Ok(TailKind::Unknown);
            }
            if eod == P1_DATA_START {
                return Ok(TailKind::EmptyData);
            }
            P1_DATA_START
        }
    };
    if eod == region_start {
        return Ok(TailKind::Complete);
    }
    if eod < region_start {
        notes.push(format!("EOD {} 早于末索引结束 {}", eod, region_start));
        return Ok(TailKind::Unknown);
    }
    // 区域内是否还有文件标记：有则是构造尝试，否则是纯数据尾。
    drive.locate(partition, region_start, true)?;
    match drive.space_filemarks(1) {
        // 规范：正向 SPACE 用尽文件标记时停在 EOD 并返回 BLANK CHECK 00/05。
        // 未修复的 Holo-VTL 返回 ILLEGAL REQUEST 24/00；两者都表示区域内没有文件标记。
        // 误判只会导致只读，方向安全。
        Err(TapeError::ScsiCommand {
            sense_key: 0x08, ..
        })
        | Err(TapeError::ScsiCommand {
            sense_key: 0x05,
            asc: 0x24,
            ..
        }) => {
            notes.push(format!("末索引后有 {} 个未索引对象", eod - region_start));
            Ok(TailKind::UnindexedData)
        }
        Err(e) => Err(e),
        Ok(()) => {
            let after_fm = drive.read_position()?.block_number;
            let (bytes, ended_by_fm, _) = read_until_fm(
                drive,
                partition,
                after_fm,
                block_size,
                budget.max_index_bytes,
            )?;
            if ended_by_fm || saw_fake {
                notes.push("末索引后存在自指针不符或边界异常的构造".to_string());
                Ok(TailKind::FakeIndex)
            } else if looks_like_index(&bytes) {
                notes.push(format!(
                    "末索引后有 {} 字节索引前缀且无结束文件标记",
                    bytes.len()
                ));
                Ok(TailKind::TruncatedIndex)
            } else {
                Ok(TailKind::UnindexedData)
            }
        }
    }
}

// ---------- 回指链 ----------

/// 沿 `previousgenerationlocation` 核验 DP 链。返回 (是否通过, 深度, 是否因预算截断)。
pub fn verify_chain(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    cand: &IndexCandidate,
    budget: &RecoveryBudget,
    notes: &mut Vec<String>,
) -> Result<(bool, u32, bool)> {
    let block_size = label.blocksize as usize;
    let dp_char = label.data_partition;
    let mut prev = cand.index.previous_location;
    let mut cur_gen = cand.index.generation;
    let mut depth = 0u32;
    while let Some(loc) = prev {
        if loc.partition != dp_char {
            // 历史版本的 commit 把回指写成 IP 位置；按规范这不是 DP 链的一环，链在此结束。
            notes.push(format!(
                "回指指向分区 {}（非 DP），链在此结束",
                loc.partition
            ));
            return Ok((true, depth, false));
        }
        if depth >= budget.max_chain_len {
            notes.push(format!("链核验深度达到预算 {}", budget.max_chain_len));
            return Ok((true, depth, true));
        }
        let (bytes, ended_by_fm, _) = match read_until_fm(
            drive,
            1,
            loc.start_block,
            block_size,
            budget.max_index_bytes,
        ) {
            Ok(v) => v,
            Err(e) => {
                notes.push(format!("链 @{} 读取失败: {}", loc.start_block, e));
                return Ok((false, depth, false));
            }
        };
        if !ended_by_fm {
            notes.push(format!("链 @{} 构造无结束文件标记", loc.start_block));
            return Ok((false, depth, false));
        }
        let idx = match LtfsIndex::parse(&bytes) {
            Ok(i) => i,
            Err(e) => {
                notes.push(format!("链 @{} 解析失败: {}", loc.start_block, e));
                return Ok((false, depth, false));
            }
        };
        if let Err(why) = validate_candidate(&idx, label, dp_char, loc.start_block) {
            notes.push(format!("链 @{} 无效: {}", loc.start_block, why));
            return Ok((false, depth, false));
        }
        if idx.generation >= cur_gen {
            notes.push(format!(
                "链 @{} 代数 {} 不低于后继 {}",
                loc.start_block, idx.generation, cur_gen
            ));
            return Ok((false, depth, false));
        }
        depth += 1;
        cur_gen = idx.generation;
        prev = idx.previous_location;
    }
    Ok((true, depth, false))
}

/// 增量目标必须完整回溯到其 Full 基线，再按正序原子应用；预算不足不能降级成成功。
fn replay_incremental_chain(
    drive: &TapeDrive<'_>, label: &LtfsLabel, candidate: &mut IndexCandidate, budget: &RecoveryBudget,
) -> Result<u32> {
    let mut chain = Vec::new();
    let mut current = candidate.index.clone();
    while current.incremental {
        if chain.len() >= budget.max_chain_len as usize {
            return Err(TapeError::RecoveryRestricted { reason: "增量链重放预算耗尽".into() });
        }
        let loc = current.previous_incremental_location.or(current.previous_location)
            .ok_or_else(|| TapeError::Ltfs("增量缺少基线位置".into()))?;
        if loc.partition != label.data_partition || loc.start_block >= current.self_location.start_block {
            return Err(TapeError::Ltfs("增量前驱分区或方向非法".into()));
        }
        let (bytes, ended, _) = read_until_fm(drive, 1, loc.start_block, label.blocksize as usize, budget.max_index_bytes)?;
        if !ended { return Err(TapeError::Ltfs("增量前驱构造残缺".into())); }
        let prior = LtfsIndex::parse(&bytes)?;
        validate_candidate(&prior, label, label.data_partition, loc.start_block).map_err(TapeError::Ltfs)?;
        if prior.generation >= current.generation || prior.incremental != current.previous_incremental_location.is_some() {
            return Err(TapeError::Ltfs("增量前驱类型或代数不符".into()));
        }
        chain.push(current);
        current = prior;
    }
    let count = chain.len() as u32;
    for delta in chain.iter().rev() { current = current.apply_incremental(delta)?; }
    candidate.index = current;
    Ok(count)
}

// ---------- VCI 快速路径 ----------

/// 按 §10.3 读取介质 VCR 与两分区 VCI，返回各分区的末索引定位提示。
/// 任何读取失败或不新鲜都只产生提示缺失，不产生错误。
fn vci_hints(
    device: &dyn TapeTransport,
    label: &LtfsLabel,
    notes: &mut Vec<String>,
) -> [Option<u64>; 2] {
    let vcr = match Mam::with_partition(device, 0).read_vcr() {
        Ok(Some(v)) if vcr_is_valid(&v) => v,
        Ok(Some(_)) => {
            notes.push("介质 VCR 全 0/全 1，无效".to_string());
            return [None, None];
        }
        Ok(None) => {
            notes.push("驱动器未提供 VCR 属性".to_string());
            return [None, None];
        }
        Err(e) => {
            notes.push(format!("读取 VCR 失败: {}", e));
            return [None, None];
        }
    };
    let mut hints = [None, None];
    for (p, slot) in [(0u8, 0usize), (1u8, 1usize)] {
        match Mam::with_partition(device, p).read_vci() {
            Ok(Some(vci)) => {
                if vci.volume_uuid != label.volume_uuid {
                    notes.push(format!("分区 {} VCI 卷 UUID 不符", p));
                } else if !vcr_matches(&vci.vcr, &vcr) {
                    notes.push(format!("分区 {} VCI 过期（VCR 不匹配）", p));
                } else {
                    debug!(
                        "分区 {} VCI 新鲜: gen={} block={}",
                        p, vci.generation, vci.block
                    );
                    hints[slot] = Some(vci.block);
                }
            }
            Ok(None) => notes.push(format!("分区 {} 无 VCI", p)),
            Err(e) => notes.push(format!("分区 {} VCI 解析失败: {}", p, e)),
        }
    }
    hints
}

// ---------- 整卷恢复 ----------

/// 执行 R2—R5 的只读核验，返回报告。调用方按 `restricted_reason()` 决定是否发布视图。
pub fn recover(
    device: &dyn TapeTransport,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<RecoveryReport> {
    let drive = TapeDrive::new(device);
    let drive = &drive;
    let mut notes = Vec::new();
    let hints = vci_hints(device, label, &mut notes);
    let mut dp = scan_partition(drive, 1, label.data_partition, label, budget, hints[1])?;
    let ip = scan_partition(drive, 0, label.index_partition, label, budget, hints[0])?;

    let replay = match &mut dp.last_index {
        Some(c) if c.index.incremental => replay_incremental_chain(drive, label, c, budget),
        _ => Ok(0),
    };
    let (mut chain_ok, mut chain_depth, chain_truncated) = match &dp.last_index {
        Some(c) => verify_chain(drive, label, c, budget, &mut notes)?,
        None => (true, 0, false),
    };

    let mut incremental_depth = 0;
    match replay {
        Ok(depth) => { incremental_depth = depth; chain_depth += depth; },
        Err(e) => { chain_ok = false; notes.push(format!("增量链恢复失败: {e}")); }
    }
    let ip_debt = match (&dp.last_index, &ip.last_index) {
        (Some(d), Some(i)) => i.index.generation < d.index.generation,
        (Some(_), None) => true,
        _ => false,
    };
    if ip_debt {
        notes.push("IP 落后于 DP，记为维护债务".to_string());
    }

    let mut report = RecoveryReport {
        dp,
        ip,
        chain_ok,
        chain_depth,
        incremental_depth,
        chain_truncated_by_budget: chain_truncated,
        ip_debt,
        append_ok: false,
        append_position: None,
        notes,
    };

    // 提交会从内容区首块整段重写 IP（FM + 索引 + FM），所以：
    // - 视图里只要有文件数据放在 IP 上（IBM 的放置策略会这样做），重写就会毁掉它，必须只读；
    // - 没有 IP 数据时，IP 自身的尾部状态不影响追加安全，只要 DP 末索引完整可作视图。
    //   典型情形：上次提交在 IP 阶段失败，IP 只剩开头文件标记。
    let index_partition = label.index_partition;
    let mut ip_has_data = false;
    if let Some(view) = report.view() {
        view.index.walk_files(|_, f| {
            if f.extents.iter().any(|e| e.partition == index_partition) {
                ip_has_data = true;
            }
        });
    }
    if ip_has_data {
        report.notes.push("视图中有文件数据位于 IP，提交会重写 IP，不放行追加".to_string());
    }
    let ip_ok = report.ip.tail == TailKind::Complete
        || (report.dp.last_index.is_some() && report.dp.tail == TailKind::Complete);
    if report.restricted_reason().is_none()
        && report.dp.tail.allows_append()
        && ip_ok
        && !ip_has_data
        && report.chain_ok
    {
        check_append_position(drive, &mut report)?;
    } else {
        report.notes.push(format!(
            "不放行追加: dp_tail={:?} ip_tail={:?} chain_ok={}",
            report.dp.tail, report.ip.tail, report.chain_ok
        ));
    }
    if !report.append_ok {
        warn!("卷 {} 恢复为只读: {:?}", label.volume_uuid, report.notes);
    }
    Ok(report)
}

/// R5：追加位置算术核对 + LOCATE/READ POSITION 复核。不试写。
fn check_append_position(drive: &TapeDrive<'_>, report: &mut RecoveryReport) -> Result<()> {
    let expected = match &report.dp.last_index {
        Some(c) => c.end_block + 1,
        None => P1_DATA_START,
    };
    let eod = report.dp.eod;
    if eod != expected {
        report
            .notes
            .push(format!("追加位置核对失败: EOD {} ≠ 预期 {}", eod, expected));
        report.dp.tail = TailKind::Unknown;
        return Ok(());
    }
    drive.locate(1, eod, true)?;
    let pos = drive.read_position()?;
    if pos.partition != 1 || pos.block_number != eod {
        report.notes.push(format!(
            "LOCATE {} 后 READ POSITION 得到 {}@{}",
            eod, pos.partition, pos.block_number
        ));
        report.dp.tail = TailKind::Unknown;
        return Ok(());
    }
    report.append_ok = true;
    report.append_position = Some(eod);
    Ok(())
}

/// 便于调用方记录的 DP 末索引位置。
pub fn dp_index_location(report: &RecoveryReport, dp_char: char) -> Option<IndexLocation> {
    report.dp.last_index.as_ref().map(|c| IndexLocation {
        partition: dp_char,
        start_block: c.start_block,
    })
}
