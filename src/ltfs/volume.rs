//! LTFS Volume：mount / list / read / append / commit 的高层封装。
//!
//! 约束：
//! - 必须先 `mkltfs::mkltfs(&device)` 初始化介质（或对方工具已初始化过）。
//! - `LtfsVolume::mount` 读 P0 的 label + index，把整棵目录树加载到内存。
//! - `append_file` 把 reader 的数据流追加到 P1 EOD，在内存 index 里记录 extent。
//!   数据尚未持久化；需要 `commit()` 才会把 index 写回 P1 tail + P0，并更新 MAM VCI。
//! - `commit` 失败时应将 volume 视为未知状态（下次 mount 会退回到上次 commit 的
//!   generation）。
//!
//! **磁带布局**（LTO，两 partition 模式）
//! ```text
//! P0 (index)    : [VOL1][FM][Label][FM][Index][FM][EOD]
//! P1 (data)     : [VOL1][FM][Label][FM][data....][FM][Index_gen_k][FM]
//!                                                [data....][FM][Index_gen_k+1][FM]
//!                                                ... EOD
//! ```

use std::io::{Read, Write};

use bytes::{Bytes, BytesMut};
use log::{debug, info, warn};
use md5::{Digest, Md5};
use sha2::Sha256;

use crate::error::{Result, TapeError};
use crate::tape::commands::TapeDrive;

use super::index::{
    DirectoryNode, Extent, FileNode, IndexLocation, LtfsIndex, NodeMeta, Xattr, XATTR_MD5, XATTR_SHA256,
};
use super::label::{LtfsLabel, PART_INDEX};
use super::mam::{Mam, VolumeCoherencyInfo};
use super::recovery::{self, RecoveryBudget, RecoveryReport};

/// 回收或覆盖携带的逻辑文件属性；源卷 UID 不复用，不携带 extent 或旧副本的位置。
#[derive(Debug, Clone)]
pub(crate) struct FileMetadata {
    pub meta: NodeMeta,
    pub xattrs: Vec<Xattr>,
}

impl FileMetadata {
    /// 内容覆盖保留扩展属性，时间与普通新写入一致；UID 和摘要在落带时重建。
    pub(crate) fn replacement(xattrs: Vec<Xattr>) -> Self {
        let now = crate::ltfs::label::ltfs_time_now();
        Self {
            meta: NodeMeta {
                creation_time: now.clone(),
                change_time: now.clone(),
                modify_time: now.clone(),
                access_time: now.clone(),
                backup_time: now,
                ..Default::default()
            },
            xattrs,
        }
    }
}

impl From<&DirectoryNode> for FileMetadata {
    fn from(node: &DirectoryNode) -> Self {
        Self { meta: node.meta.clone(), xattrs: node.xattrs.clone() }
    }
}

impl From<&FileNode> for FileMetadata {
    fn from(node: &FileNode) -> Self {
        Self { meta: node.meta.clone(), xattrs: node.xattrs.clone() }
    }
}

/// P0 / P1 起始块布局常量（与 `mkltfs` 生成的结构一一对应）。
pub const VOL1_BLOCK: u64 = 0;
pub const LABEL_BLOCK: u64 = 2;
/// 内容区首块：紧跟标签构造（VOL1, FM, 卷标, FM）。
pub const P1_DATA_START: u64 = 4;
/// 新分区上第一个索引记录的块号。Index Construct 是"文件标记 + 索引 + 文件标记"
/// （LTFS 2.5.1 §5.2.3），所以块 4 是开头文件标记，索引从块 5 起。
/// IBM LTFS 按这个结构反向找索引，少了开头文件标记它就读不到。
pub const FIRST_INDEX_BLOCK: u64 = 5;
/// IP 上索引的固定位置：每次提交从内容区首块重写整个 Index Construct。
pub const P0_INDEX_BLOCK: u64 = FIRST_INDEX_BLOCK;

/// 默认块大小（字节）。LTFS 常用 512 KiB，可被 mkltfs 覆写。
pub const DEFAULT_BLOCK_SIZE: u32 = 512 * 1024;

pub struct LtfsVolume<'a> {
    device: &'a dyn crate::scsi::transport::TapeTransport,
    drive: TapeDrive<'a>,
    mam: Mam<'a>,
    label: LtfsLabel,
    /// 工作索引：包含尚未提交的追加。
    working: LtfsIndex,
    /// 已发布的已提交视图（S5）：list/read/index() 只服务它。
    index: LtfsIndex,
    block_size: u32,
    /// P1 当前可写入位置。commit 后更新。
    p1_write_head: u64,
    dirty: bool,
    /// DP 上最新一份索引的位置：commit 时作为 `previousgenerationlocation`。
    last_dp_index: Option<IndexLocation>,
    last_dp_incremental: Option<IndexLocation>,
    last_ip: Option<(u64, u64)>,
    incremental_depth: u32,
    incremental_limit: u32,
    /// 挂载时的恢复报告；`writable == recovery.append_ok`。
    recovery: RecoveryReport,
    writable: bool,
    /// 只读原因：恢复受限或提交失败后的"结果未定"。
    restricted_reason: Option<String>,
    hash_policy: HashPolicy,
    /// 设了之后，每次提交在屏障前核对预留持有者是不是这个键。
    reservation_guard: Option<crate::scsi::reservation::ReservationKey>,
}

impl<'a> LtfsVolume<'a> {
    /// 挂载卷：读标签后执行 D02 恢复协议（末索引定位、尾部分类、回指链核验），
    /// 视图取 DP 末索引（可能比 IP 新）。尾部非完整时以只读挂载，不自动修复。
    pub fn mount(device: &'a dyn crate::scsi::transport::TapeTransport) -> Result<Self> {
        Self::mount_with_budget(device, &RecoveryBudget::default())
    }

    /// 同 `mount`，但使用指定搜索预算。
    pub fn mount_with_budget(
        device: &'a dyn crate::scsi::transport::TapeTransport,
        budget: &RecoveryBudget,
    ) -> Result<Self> {
        let drive = TapeDrive::new(device);
        let mam = Mam::new(device);

        // 1. 读 P0 label
        let label_bytes = read_single_block(&drive, 0, LABEL_BLOCK, /*max=*/ 64 * 1024)?;
        let label = LtfsLabel::parse(&label_bytes)?;
        let block_size = label.blocksize;
        info!(
            "LTFS label: version={} uuid={} blocksize={} compression={}",
            label.version, label.volume_uuid, block_size, label.compression
        );

        // 2. D02 恢复协议：末索引、尾部、链、追加位置
        let report = recovery::recover(device, &label, budget)?;
        if let Some(reason) = report.restricted_reason() {
            return Err(TapeError::RecoveryRestricted { reason });
        }
        let view = report
            .view()
            .expect("restricted_reason() checked view presence");
        let index = view.index.clone();
        info!(
            "index gen={} highest_uid={} (dp_tail={:?}, ip_tail={:?}, writable={})",
            index.generation,
            index.highest_file_uid,
            report.dp.tail,
            report.ip.tail,
            report.append_ok
        );

        // 3. P1 append point = 恢复报告中的 EOD。
        let p1_write_head = report.dp.eod.max(P1_DATA_START);
        let dp = report.dp.last_index.as_ref();
        let last_dp_index = dp.and_then(|c| if c.index.incremental { c.index.previous_location } else { Some(c.index.self_location) });
        let last_dp_incremental = dp.filter(|c| c.index.incremental).map(|c| c.index.self_location);
        let last_ip = report.ip.last_index.as_ref().map(|c| (c.index.generation, c.start_block));
        for note in report.notes.iter().chain(&report.dp.notes).chain(&report.ip.notes) {
            debug!("恢复备注: {}", note);
        }
        let lossy = !index.unknown_elements.is_empty();
        // <volumelockstate>：unlocked / locked / permlocked（IBM LTFS 的取值）。
        // 除 unlocked 外一律不写；不认识的取值也按锁定处理。
        let locked = index
            .volume_lock_state
            .as_deref()
            .is_some_and(|v| v != "unlocked");
        let writable = report.append_ok && !lossy && !locked;
        let restricted_reason = if writable {
            None
        } else if locked {
            Some(format!(
                "卷已锁定 (volumelockstate={})，只读挂载",
                index.volume_lock_state.as_deref().unwrap_or("")
            ))
        } else if lossy {
            // 重写索引会丢掉这些元素，所以不允许写入。
            Some(format!(
                "索引含本实现不会回写的元素，只读挂载: {}",
                index.unknown_elements.join(", ")
            ))
        } else {
            Some(format!(
                "恢复只读: dp_tail={:?} ip_tail={:?}",
                report.dp.tail, report.ip.tail
            ))
        };
        debug!("P1 write head (EOD) = {}", p1_write_head);

        Ok(Self {
            device,
            drive,
            mam,
            label,
            working: index.clone(),
            index,
            block_size,
            p1_write_head,
            dirty: false,
            last_dp_index,
            last_dp_incremental,
            last_ip,
            incremental_depth: report.incremental_depth,
            incremental_limit: 5.min(budget.max_chain_len),
            recovery: report,
            writable,
            restricted_reason,
            hash_policy: HashPolicy::default(),
            reservation_guard: None,
        })
    }

    /// 挂载时的恢复报告。
    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// 是否允许追加写入（D02 放行检查通过且此后没有失败的提交）。
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// 不可写时的原因。
    pub fn restricted_reason(&self) -> Option<&str> {
        self.restricted_reason.as_deref()
    }

    /// 尚未被成功提交覆盖的追加文件路径（工作索引中有、已发布视图中没有或长度不同）。
    pub fn pending_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.working
            .walk_files(|p, f| match self.index.find_file(p) {
                Some(pf) if pf.length == f.length && pf.meta.file_uid == f.meta.file_uid => {}
                _ => out.push(p.to_string()),
            });
        out
    }

    /// 为 P0 与 P1 写入 VCI（各自的最新索引位置）。驱动器不提供 VCR 时跳过并记录。
    fn write_vci_all(&self, p1_index_block: u64) -> Result<()> {
        let Some(vcr) = self.mam.read_vcr()? else {
            debug!("驱动器未提供 VCR，跳过 VCI 更新");
            return Ok(());
        };
        if !super::mam::vcr_is_valid(&vcr) {
            debug!("VCR 无效（全 0/全 1），跳过 VCI 更新");
            return Ok(());
        }
        let ip = if self.working.incremental { self.last_ip } else { Some((self.working.generation, P0_INDEX_BLOCK)) };
        for (partition, entry) in [(0u8, ip), (1u8, Some((self.working.generation, p1_index_block)))] {
            let Some((generation, block)) = entry else { continue; };
            let vci = VolumeCoherencyInfo {
                vcr: vcr.to_vec(),
                generation,
                block,
                volume_uuid: self.label.volume_uuid,
            };
            Mam::with_partition(self.device, partition).write_vci(&vci)?;
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(TapeError::RecoveryRestricted {
                reason: self
                    .restricted_reason
                    .clone()
                    .unwrap_or_else(|| "卷为只读".to_string()),
            })
        }
    }

    /// 提交失败：冻结本卷追加（结果未定，须重新挂载经恢复协议核实）。已发布视图不变。
    fn freeze_after_commit_failure(&mut self, stage: &str, err: &TapeError) {
        self.writable = false;
        self.restricted_reason = Some(format!("提交在 {} 阶段失败，结果未定: {}", stage, err));
        warn!(
            "commit 失败于 {}: {}；卷转为只读，重新挂载后按恢复协议核实",
            stage, err
        );
    }

    pub fn label(&self) -> &LtfsLabel {
        &self.label
    }

    /// 已发布的已提交视图。
    pub fn index(&self) -> &LtfsIndex {
        &self.index
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// 文件列表 (path, size)。
    pub fn list(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        self.index
            .walk_files(|p, f| out.push((p.to_string(), f.length)));
        out
    }

    /// 读取指定路径的文件内容到 writer，返回写出字节数。
    pub fn read_file_to_writer<W: Write>(&self, path: &str, w: &mut W) -> Result<u64> {
        let file = self
            .index
            .find_file(path)
            .ok_or_else(|| TapeError::Ltfs(format!("文件不存在: {}", path)))?;
        if let Some(target) = &file.symlink {
            // IBM EE 的布局：用户路径是符号链接，数据在 .LTFSEE_DATA/<id>。
            let resolved = resolve_symlink(path, target)
                .ok_or_else(|| TapeError::Ltfs(format!("符号链接越出卷根: {} -> {}", path, target)))?;
            let hit = self.index.find_file(&resolved);
            return match hit {
                Some(f) if f.symlink.is_none() => self.read_file_to_writer(&resolved, w),
                _ => Err(TapeError::Ltfs(format!("符号链接目标不可读: {} -> {}", path, target))),
            };
        }
        if file.extents.is_empty() {
            if file.length != 0 {
                return Err(TapeError::Ltfs(format!("文件 {} 缺少数据 extent", path)));
            }
            return Ok(0);
        }

        let mut total: u64 = 0;
        let mut buf = vec![0u8; self.block_size as usize];

        for ext in &file.extents {
            let partition = partition_char_to_num(ext.partition);
            self.drive.locate(partition, ext.start_block, true)?;

            // 从 extent 起始块读起，按 byte_offset / byte_count 精确裁剪。
            let mut remaining = ext.byte_count;
            let mut first_block = true;
            while remaining > 0 {
                let n = self.drive.read_block(&mut buf)?;
                if n == 0 {
                    return Err(TapeError::Ltfs(format!("文件 {} 的 extent 提前结束，缺少 {} 字节", path, remaining)));
                }
                let slice_start = if first_block {
                    ext.byte_offset as usize
                } else {
                    0
                };
                let available = n.saturating_sub(slice_start);
                let take = (remaining as usize).min(available);
                if take > 0 {
                    w.write_all(&buf[slice_start..slice_start + take])?;
                    total += take as u64;
                    remaining -= take as u64;
                }
                first_block = false;
            }
        }
        if total != file.length {
            return Err(TapeError::Ltfs(format!("文件 {} 长度不符: 索引 {}，读取 {}", path, file.length, total)));
        }
        w.flush()?;
        info!("读取 {}: {} 字节", path, total);
        Ok(total)
    }

    /// 追加文件：从 reader 流式写入 P1 EOD，记录 extent 到内存 index。
    /// 数据尚未持久化，需 `commit()`。
    pub fn append_file<R: Read>(&mut self, path: &str, r: &mut R) -> Result<u64> {
        self.append_file_with_xattrs(path, r, &[])
    }

    fn check_reservation_guard(&mut self) -> Result<()> {
        let Some(key) = self.reservation_guard else {
            return Ok(());
        };
        if let Err(e) = crate::scsi::reservation::verify_holder(self.device, key) {
            // 失去资格后不再允许任何写入，包括 unmount 时的自动提交
            self.writable = false;
            self.restricted_reason = Some(format!("执行资格已失去: {}", e));
            return Err(e);
        }
        Ok(())
    }

    /// 接管后的收尾：卷因尾部不完整（T1 未索引数据、T2 索引写了一半）而只读时，
    /// 在 EOD 追加一份新索引使卷重新一致、可写。不覆盖任何已有块。
    ///
    /// 只有在能证明独占时才允许：必须已设置预留自检键，且设备回读确认持有者是该键。
    /// 其他只读原因（回指链断、两分区冲突、索引含未知元素、卷已锁、视图含 IP 数据、
    /// T3 假索引、T4 未知尾部）一律拒绝，仍由人工处理。
    pub fn close_tail(&mut self, policy: TailPolicy) -> Result<CloseTailReport> {
        let key = self.reservation_guard.ok_or_else(|| TapeError::RecoveryRestricted {
            reason: "收尾要求先完成设备层隔离并设置预留自检键".to_string(),
        })?;
        crate::scsi::reservation::verify_holder(self.device, key)?;

        let refuse = |why: String| Err(TapeError::RecoveryRestricted { reason: format!("不能自动收尾: {}", why) });
        let tail = self.recovery.dp.tail;
        if !matches!(tail, recovery::TailKind::UnindexedData | recovery::TailKind::TruncatedIndex) {
            return refuse(format!("DP 尾部为 {:?}", tail));
        }
        if let Some(r) = self.recovery.restricted_reason() {
            return refuse(r);
        }
        if !self.recovery.chain_ok {
            return refuse("DP 回指链未通过".to_string());
        }
        let Some(last) = self.recovery.dp.last_index.as_ref() else {
            return refuse("DP 上没有有效索引".to_string());
        };
        if self.recovery.ip_ahead() {
            return refuse("视图来自 IP，DP 尾部另有内容".to_string());
        }
        if !self.index.unknown_elements.is_empty() {
            return refuse("索引含不会回写的元素".to_string());
        }
        if self.index.volume_lock_state.as_deref().is_some_and(|v| v != "unlocked") {
            return refuse("卷已锁定".to_string());
        }
        let ip_char = self.label.index_partition;
        let mut ip_data = false;
        self.index.walk_files(|_, f| ip_data |= f.extents.iter().any(|e| e.partition == ip_char));
        if ip_data {
            return refuse("视图含位于 IP 的文件数据".to_string());
        }

        let first = last.end_block + 1;
        let eod = self.recovery.dp.eod;
        // 追加位置复核：LOCATE 到 EOD 后 READ POSITION 必须一致
        self.drive.locate(1, eod, true)?;
        let pos = self.drive.read_position()?;
        if pos.partition != 1 || pos.block_number != eod {
            return refuse(format!("LOCATE {} 后位置为 {}@{}", eod, pos.partition, pos.block_number));
        }

        let mut report = CloseTailReport {
            tail,
            abandoned_first_block: first,
            abandoned_end_block: eod,
            salvaged: None,
            generation: 0,
        };
        if policy == TailPolicy::Salvage {
            report.salvaged = self.salvage_tail(first, eod)?;
        }
        // 被放弃的块范围记在卷根，事后可查
        let range = format!("{}:{}-{}", self.label.data_partition, first, eod.saturating_sub(1));
        let prev = self
            .working
            .root
            .xattrs
            .iter()
            .find(|x| x.key == XATTR_ABANDONED)
            .map(|x| format!("{};", x.value))
            .unwrap_or_default();
        self.working.root.xattrs.retain(|x| x.key != XATTR_ABANDONED);
        self.working.root.xattrs.push(super::index::Xattr {
            key: XATTR_ABANDONED.to_string(),
            value: format!("{prev}{range}"),
            base64: false,
        });

        info!("收尾: DP 尾部 {:?}，放弃块 {}..{}，在 EOD {} 追加索引", tail, first, eod, eod);
        self.p1_write_head = eod;
        self.writable = true;
        self.restricted_reason = None;
        self.dirty = true;
        self.commit()?;
        self.recovery.dp.tail = recovery::TailKind::Complete;
        self.recovery.append_ok = true;
        report.generation = self.index.generation;
        Ok(report)
    }

    /// 把 `[first, eod)` 里文件标记之前的数据块登记为 `_ltfs_lostandfound/` 下的一个文件。
    fn salvage_tail(&mut self, first: u64, eod: u64) -> Result<Option<(String, u64)>> {
        let bs = self.block_size as u64;
        let mut buf = vec![0u8; self.block_size as usize];
        self.drive.locate(1, first, true)?;
        let mut extents: Vec<Extent> = Vec::new();
        let mut total = 0u64;
        let mut open: Option<Extent> = None;
        for blk in first..eod {
            let n = match self.drive.read_block(&mut buf) {
                Ok(0) => break, // 文件标记：后面是写了一半的索引
                Ok(n) => n as u64,
                Err(TapeError::ScsiCommand { sense_key: 0x08, .. }) => break,
                Err(e) => return Err(e),
            };
            match open.as_mut() {
                Some(e) => e.byte_count += n,
                None => {
                    open = Some(Extent {
                        partition: self.label.data_partition,
                        start_block: blk,
                        byte_offset: 0,
                        byte_count: n,
                        file_offset: total,
                    })
                }
            }
            total += n;
            // extent 内只有最后一块可以不满；遇到短块就结束当前 extent
            if n < bs {
                extents.extend(open.take());
            }
        }
        extents.extend(open.take());
        if total == 0 {
            return Ok(None);
        }
        let name = format!("tail-gen{}-block{}", self.index.generation, first);
        let path = format!("{}/{}", LOST_AND_FOUND, name);
        let now = crate::ltfs::label::ltfs_time_now();
        self.working.highest_file_uid += 1;
        let uid = self.working.highest_file_uid;
        let dir = ensure_dir(
            &mut self.working.root,
            &mut self.working.highest_file_uid,
            &[LOST_AND_FOUND],
        );
        dir.files.retain(|f| f.name != name);
        dir.files.push(FileNode {
            name,
            length: total,
            meta: NodeMeta {
                readonly: true,
                creation_time: now.clone(),
                change_time: now.clone(),
                modify_time: now.clone(),
                access_time: now.clone(),
                backup_time: now,
                file_uid: uid,
            },
            extents,
            ..Default::default()
        });
        Ok(Some((path, total)))
    }

    /// 卷根目录上的扩展属性（已提交视图）。
    pub fn root_xattr(&self, key: &str) -> Option<&str> {
        self.index.root.xattrs.iter().find(|x| x.key == key).map(|x| x.value.as_str())
    }

    /// 设置卷根目录的文本型扩展属性。下次提交时写入索引。
    pub fn set_root_xattr(&mut self, key: &str, value: &str) -> Result<()> {
        self.ensure_writable()?;
        self.working.root.xattrs.retain(|x| x.key != key);
        self.working.root.xattrs.push(super::index::Xattr { key: key.to_string(), value: value.to_string(), base64: false });
        self.dirty = true;
        Ok(())
    }

    /// 启用提交前的预留持有者自检（设备层隔离协议）。`None` 关闭。
    pub fn set_reservation_guard(&mut self, key: Option<crate::scsi::reservation::ReservationKey>) {
        self.reservation_guard = key;
    }

    pub fn set_hash_policy(&mut self, policy: HashPolicy) {
        self.hash_policy = policy;
    }

    /// 在工作索引里登记一个符号链接（§9.2.9）。不写任何数据块，commit 后生效。
    /// IBM EE 用它把用户路径指向 `.LTFSEE_DATA/<id>`。
    pub fn add_symlink(&mut self, path: &str, target: &str) -> Result<()> {
        self.add_symlink_with_metadata(path, target, None)
    }

    /// 回收链接只复制节点，不读取目标、不携带源卷 extent 或 UID。
    pub(crate) fn add_symlink_with_metadata(
        &mut self,
        path: &str,
        target: &str,
        preserved: Option<&FileMetadata>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if target.is_empty() {
            return Err(TapeError::Ltfs(format!("符号链接目标为空: {}", path)));
        }
        self.working.highest_file_uid += 1;
        let now = crate::ltfs::label::ltfs_time_now();
        let mut meta = NodeMeta {
            readonly: false,
            creation_time: now.clone(),
            change_time: now.clone(),
            modify_time: now.clone(),
            access_time: now.clone(),
            backup_time: now,
            file_uid: self.working.highest_file_uid,
        };
        if let Some(metadata) = preserved {
            meta = metadata.meta.clone();
            meta.file_uid = self.working.highest_file_uid;
        }
        let (dir_parts, file_name) = split_path(path)?;
        let dir = ensure_dir(
            &mut self.working.root,
            &mut self.working.highest_file_uid,
            &dir_parts,
        );
        dir.files.retain(|f| f.name != file_name);
        dir.files.push(FileNode {
            name: file_name.to_string(),
            meta,
            symlink: Some(target.to_string()),
            xattrs: preserved.map(|m| m.xattrs.clone()).unwrap_or_default(),
            ..Default::default()
        });
        self.dirty = true;
        Ok(())
    }

    /// 从工作索引里去掉一个文件（LTFS 的 unlink）。数据块留在带上，不再被引用；
    /// 下次提交写出的索引里没有它。返回是否真的有这个文件。空目录保留。
    pub fn remove_file(&mut self, path: &str) -> Result<bool> {
        self.ensure_writable()?;
        let (dir_parts, file_name) = split_path(path)?;
        let mut dir = &mut self.working.root;
        for part in dir_parts {
            match dir.subdirs.iter().position(|d| d.name == part) {
                Some(i) => dir = &mut dir.subdirs[i],
                None => return Ok(false),
            }
        }
        let before = dir.files.len();
        dir.files.retain(|f| f.name != file_name);
        if dir.files.len() == before {
            return Ok(false);
        }
        self.dirty = true;
        info!("删除 {}（数据块保留在带上，待回收）", path);
        Ok(true)
    }

    /// 修改只进入工作索引；commit_incremental 将其持久化为标准 DP 增量。
    pub fn create_directory(&mut self, path: &str) -> Result<()> {
        self.ensure_writable()?; self.working.create_directory(path)?; self.dirty = true; Ok(())
    }

    /// 守护进程已按池内最新视图验证路径可替换后，丢弃本带的旧命名空间副本。
    /// 只改工作索引；不回收数据块，失败提交仍由卷冻结保护。
    pub(crate) fn discard_catalog_path(&mut self, path: &str) -> Result<()> {
        self.ensure_writable()?;
        let (parents, name) = split_path(path)?;
        let mut d = &mut self.working.root;
        for p in parents {
            let Some(child) = d.subdirs.iter_mut().find(|d| d.name == p) else {
                return Ok(());
            };
            d = child;
        }
        d.files.retain(|f| f.name != name);
        d.subdirs.retain(|d| d.name != name);
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn put_catalog_directory(
        &mut self,
        path: &str,
        version: &str,
        metadata: &FileMetadata,
    ) -> Result<()> {
        self.ensure_writable()?;
        if self.working.find_directory(path).is_none() {
            self.remove_file(path)?;
            self.working.create_directory(path)?;
        }
        let (parents, name) = split_path(path)?;
        let mut d = &mut self.working.root;
        for p in parents {
            d = d
                .subdirs
                .iter_mut()
                .find(|d| d.name == p)
                .expect("父目录已创建");
        }
        let d = d
            .subdirs
            .iter_mut()
            .find(|d| d.name == name)
            .expect("目录已创建");
        let uid = d.meta.file_uid;
        d.meta = metadata.meta.clone();
        d.meta.file_uid = uid;
        d.xattrs = metadata.xattrs.clone();
        d.xattrs.retain(|x| x.key != XATTR_VERSION);
        d.xattrs.push(Xattr {
            key: XATTR_VERSION.into(),
            value: version.into(),
            base64: false,
        });
        self.dirty = true;
        Ok(())
    }

    pub fn remove_directory(&mut self, path: &str) -> Result<()> {
        self.ensure_writable()?; self.working.remove_directory(path)?; self.dirty = true; Ok(())
    }

    pub(crate) fn set_catalog_version(&mut self, path: &str, version: &str) -> Result<()> {
        self.ensure_writable()?;
        self.working.set_catalog_version(path, version)?;
        self.dirty = true;
        Ok(())
    }

    pub fn rename_path(&mut self, from: &str, to: &str) -> Result<()> {
        self.ensure_writable()?; self.working.rename_path(from, to)?; self.dirty = true; Ok(())
    }

    pub fn set_node_xattr(&mut self, path: &str, attr: Xattr, create_only: bool, replace_only: bool) -> Result<()> {
        self.ensure_writable()?; self.working.set_node_xattr(path, attr, create_only, replace_only)?; self.dirty = true; Ok(())
    }

    pub fn remove_node_xattr(&mut self, path: &str, key: &str) -> Result<()> {
        self.ensure_writable()?; self.working.remove_node_xattr(path, key)?; self.dirty = true; Ok(())
    }

    pub fn set_node_readonly(&mut self, path: &str, readonly: bool) -> Result<()> {
        self.ensure_writable()?; self.working.set_node_readonly(path, readonly)?; self.dirty = true; Ok(())
    }

    /// 读出文件并与索引里的哈希比对。优先 sha256sum，其次 md5sum。
    pub fn verify_file(&self, path: &str) -> Result<HashVerdict> {
        let file = self
            .index
            .find_file(path)
            .ok_or_else(|| TapeError::Ltfs(format!("文件不存在: {}", path)))?;
        let (algo, expected) = match (file.xattr(XATTR_SHA256), file.xattr(XATTR_MD5)) {
            (Some(v), _) => ("sha256sum", v.to_ascii_lowercase()),
            (None, Some(v)) => ("md5sum", v.to_ascii_lowercase()),
            (None, None) => return Ok(HashVerdict::NoHash),
        };
        let mut sink = HashSink { md5: Md5::new(), sha256: Sha256::new() };
        self.read_file_to_writer(path, &mut sink)?;
        let actual = if algo == "sha256sum" { hex(&sink.sha256.finalize()) } else { hex(&sink.md5.finalize()) };
        Ok(if actual == expected {
            HashVerdict::Match { algo }
        } else {
            HashVerdict::Mismatch { algo, expected, actual }
        })
    }

    /// 同 `append_file`，并附带调用方给的文本型扩展属性。
    /// 内容哈希由本函数依照 HashPolicy 按实际写入内容计算。
    pub fn append_file_with_xattrs<R: Read>(
        &mut self,
        path: &str,
        r: &mut R,
        xattrs: &[(&str, &str)],
    ) -> Result<u64> {
        self.append_file_inner(path, r, xattrs, None)
    }

    pub(crate) fn append_relocated_file<R: Read>(&mut self, path: &str, r: &mut R, version: &str, metadata: &FileMetadata) -> Result<u64> {
        self.append_file_inner(path, r, &[(XATTR_VERSION, version)], Some(metadata))
    }

    fn append_file_inner<R: Read>(&mut self, path: &str, r: &mut R, xattrs: &[(&str, &str)], preserved: Option<&FileMetadata>) -> Result<u64> {
        self.ensure_writable()?;
        if path.is_empty() || path.ends_with('/') {
            return Err(TapeError::Ltfs(format!("非法文件路径: {}", path)));
        }
        // 写第一个字节之前先确认自己仍是预留持有者：陈旧轮次的实例在这里就停下，什么也不落带。
        self.check_reservation_guard()?;

        // 1. LOCATE P1 write head
        self.drive.locate(1, self.p1_write_head, true)?;

        // 2. 流式写入，整块为单位
        let block_size = self.block_size as usize;
        let mut buf = vec![0u8; block_size];
        let start_block = self.p1_write_head;
        let mut byte_count: u64 = 0;
        let mut cur_block = start_block;
        // 搬迁时也重算源文件已有的哈希，不复制可能过期的摘要值。
        let had_hash = |key: &str| preserved.is_some_and(|m| m.xattrs.iter().any(|x| x.key == key));
        let mut md5 = (self.hash_policy.md5 || had_hash(XATTR_MD5)).then(Md5::new);
        let mut sha256 = (self.hash_policy.sha256 || had_hash(XATTR_SHA256)).then(Sha256::new);

        loop {
            let mut filled = 0;
            while filled < block_size {
                match r.read(&mut buf[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            self.drive.write_block(&buf[..filled])?;
            if let Some(h) = md5.as_mut() {
                h.update(&buf[..filled]);
            }
            if let Some(h) = sha256.as_mut() {
                h.update(&buf[..filled]);
            }
            byte_count += filled as u64;
            cur_block += 1;
            if filled < block_size {
                break;
            }
        }

        if byte_count == 0 {
            // 空文件：不写任何 block，但在 index 里仍要登记 name + 0 length
            debug!("append 空文件: {}", path);
        }

        // 3. 更新 p1_write_head（尚未写 FM；FM 只在 commit 时写一次）
        self.p1_write_head = cur_block;

        // 4. 注册到 index
        self.working.highest_file_uid += 1;
        let uid = self.working.highest_file_uid;
        let now = crate::ltfs::label::ltfs_time_now();
        let meta = preserved.map_or_else(|| NodeMeta {
            readonly: false,
            creation_time: now.clone(),
            change_time: now.clone(),
            modify_time: now.clone(),
            access_time: now.clone(),
            backup_time: now,
            file_uid: uid,
        }, |m| NodeMeta { file_uid: uid, ..m.meta.clone() });
        let extents = if byte_count > 0 {
            vec![Extent {
                partition: self.label.data_partition,
                start_block,
                byte_offset: 0,
                byte_count,
                file_offset: 0,
            }]
        } else {
            Vec::new()
        };

        let (dir_parts, file_name) = split_path(path)?;
        let dir = ensure_dir(
            &mut self.working.root,
            &mut self.working.highest_file_uid,
            &dir_parts,
        );
        // 同名文件去重（覆盖）
        dir.files.retain(|f| f.name != file_name);
        let mut node = FileNode {
            name: file_name.to_string(),
            length: byte_count,
            meta,
            extents,
            xattrs: preserved.map(|m| m.xattrs.clone()).unwrap_or_default(),
            ..Default::default()
        };
        for (key, value) in xattrs {
            node.set_xattr(key, value);
        }
        // 内容哈希记在扩展属性里（附录 F.3）。放在调用方属性之后，
        // 保证哈希总是按实际写入内容计算；没算的那种也不接受调用方给的值。
        node.xattrs.retain(|x| x.key != XATTR_MD5 && x.key != XATTR_SHA256);
        if let Some(h) = md5 {
            node.set_xattr(XATTR_MD5, &hex(&h.finalize()));
        }
        if let Some(h) = sha256 {
            node.set_xattr(XATTR_SHA256, &hex(&h.finalize()));
        }
        dir.files.push(node);

        self.dirty = true;
        info!(
            "追加 {}: {} 字节 @ P1 block {}",
            path, byte_count, start_block
        );
        Ok(byte_count)
    }

    /// 提交：
    /// 1. P1 tail 写 FM + index + FM（形成新 generation）
    /// 2. P0 覆盖写入最新 index
    /// 3. 更新 MAM VCI
    pub fn commit(&mut self) -> Result<()> {
        // 增量之后即便没有新变化，也必须能显式生成完整检查点。
        if self.index.incremental { self.dirty = true; }
        self.commit_mode(false)
    }

    /// LTFS 2.5 提交：通常只追加 DP 增量；连续 5 份（或较小恢复预算）后写 DP/IP Full。
    /// 清洁卸载也会写 Full 检查点，计数跨重新挂载保留。
    pub fn commit_incremental(&mut self) -> Result<()> {
        if !self.dirty { return Ok(()); }
        if self.incremental_depth >= self.incremental_limit
            || self.index.self_location.partition != self.label.data_partition
        {
            return self.commit();
        }
        self.commit_mode(true)
    }

    fn commit_mode(&mut self, incremental: bool) -> Result<()> {
        if !self.dirty {
            debug!("commit: nothing to do");
            return Ok(());
        }
        self.ensure_writable()?;
        match self.commit_inner(incremental) {
            Ok(()) => Ok(()),
            Err((stage, err)) => {
                self.freeze_after_commit_failure(stage, &err);
                Err(TapeError::CommitFailed {
                    stage: stage.to_string(),
                    reason: err.to_string(),
                })
            }
        }
    }

    /// S1—S6 的设备步骤，返回失败阶段以便分类：
    /// `locate` / `opening_fm`（尚未写索引，数据仍在但本卷冻结）、
    /// `index_records` / `closing_fm`（索引残缺，结果未定）、
    /// `index_partition` / `barrier` / `vci`（DP 索引可能已完整，结果未定）。
    fn commit_inner(&mut self, incremental: bool) -> std::result::Result<(), (&'static str, TapeError)> {
        // 回指指向 DP 上的前一份 Full（LTFS 2.5.1 §5.4.3）；旧版本曾误指向 IP。
        // 动带之前先自检；失败时介质未被触碰，但为了语义统一仍按提交失败冻结。
        if let Some(key) = self.reservation_guard {
            crate::scsi::reservation::verify_holder(self.device, key)
                .map_err(|e| ("guard_pre", e))?;
        }
        let prev = self.last_dp_index;
        self.working.generation += 1;
        self.working.update_time = crate::ltfs::label::ltfs_time_now();
        self.working.previous_location = prev;
        self.working.previous_incremental_location = self.last_dp_incremental;
        self.working.incremental = incremental;
        if incremental || self.last_dp_incremental.is_some() { self.working.version = "2.5.0".into(); }

        // —— S3：DP 索引构造 [FM][records][FM] —— //
        self.drive
            .locate(1, self.p1_write_head, true)
            .map_err(|e| ("locate", e))?;
        self.drive
            .write_filemark(1)
            .map_err(|e| ("opening_fm", e))?;
        let p1_index_block = self.p1_write_head + 1;
        self.working.self_location = IndexLocation {
            partition: self.label.data_partition,
            start_block: p1_index_block,
        };
        let encoded = if incremental { self.working.incremental_since(&self.index).map_err(|e| ("encode", e))? } else { self.working.clone() };
        let xml = encoded.to_xml().map_err(|e| ("encode", e))?;
        let blocks_written = write_bytes_in_blocks(&self.drive, &xml, self.block_size as usize)
            .map_err(|e| ("index_records", e))?;
        self.drive
            .write_filemark(1)
            .map_err(|e| ("closing_fm", e))?;
        self.p1_write_head = p1_index_block + blocks_written as u64 + 1;

        // —— IP：同一份 XML，自指针指向 P0 —— //
        if !incremental {
        let mut p0_index = self.working.clone();
        p0_index.self_location = IndexLocation {
            partition: self.label.index_partition,
            start_block: P0_INDEX_BLOCK,
        };
        // 一致卷的定义：IP 末索引回指 DP 上最后一个完整索引，也就是刚写好的同代那份。
        // 指向上一代的话 IBM LTFS 会判为不一致，挂载时自行补写 IP。
        p0_index.previous_incremental_location = None;
        p0_index.previous_location = Some(IndexLocation {
            partition: self.label.data_partition,
            start_block: p1_index_block,
        });
        let p0_xml = p0_index.to_xml().map_err(|e| ("encode", e))?;
        self.drive
            .locate(0, P1_DATA_START, true)
            .map_err(|e| ("index_partition", e))?;
        self.drive
            .write_filemark(1)
            .map_err(|e| ("index_partition", e))?;
        write_bytes_in_blocks(&self.drive, &p0_xml, self.block_size as usize)
            .map_err(|e| ("index_partition", e))?;
        self.drive
            .write_filemark(1)
            .map_err(|e| ("index_partition", e))?;

        }

        // —— 屏障前自检：设备报告的预留持有者必须仍是本轮的键 —— //
        // 预留可能在不知情时丢失（驱动器复位且未启用跨断电保持）；把这个窗口限制在一次提交之内。
        if let Some(key) = self.reservation_guard {
            crate::scsi::reservation::verify_holder(self.device, key).map_err(|e| ("guard", e))?;
        }

        // —— S4：最终屏障（D01 保守候选）；§10.3：读 VCR → 写全部分区 VCI —— //
        self.drive.write_filemark(0).map_err(|e| ("barrier", e))?;
        self.write_vci_all(p1_index_block).map_err(|e| ("vci", e))?;

        // —— S5：发布已提交视图 —— //
        self.dirty = false;
        if incremental {
            self.last_dp_incremental = Some(self.working.self_location);
            self.incremental_depth += 1;
        }
        else {
            self.last_dp_index = Some(self.working.self_location);
            self.last_dp_incremental = None;
            self.incremental_depth = 0;
            self.last_ip = Some((self.working.generation, P0_INDEX_BLOCK));
        }
        self.working.materialized = incremental;
        self.index = self.working.clone();
        info!(
            "commit: gen {} @ P0 {}, P1 {}",
            self.working.generation, P0_INDEX_BLOCK, p1_index_block
        );
        Ok(())
    }

    /// 卸载：保证写回并倒带。调用后 Volume 被消费。
    pub fn unmount(mut self) -> Result<()> {
        if (self.dirty || self.index.incremental) && self.writable {
            self.commit()?;
        }
        self.drive.rewind()?;
        Ok(())
    }
}

// ---------- 辅助函数 ----------

fn partition_char_to_num(c: char) -> u8 {
    if c == PART_INDEX { 0 } else { 1 }
}

fn split_path(path: &str) -> Result<(Vec<&str>, &str)> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err(TapeError::Ltfs("文件路径为空".into()));
    }
    let file = *parts.last().unwrap();
    let dirs = parts[..parts.len() - 1].to_vec();
    Ok((dirs, file))
}

/// 写入时计算哪些内容哈希。默认只算 sha256sum：有 SHA 指令的 CPU 上它比 md5 快约 3 倍，
/// 而两个同时算的单线程吞吐会低于 LTO-8 的原生速率。md5sum 只在要给 IBM LTFS 校验时打开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashPolicy {
    pub md5: bool,
    pub sha256: bool,
}

impl Default for HashPolicy {
    fn default() -> Self {
        Self { md5: false, sha256: true }
    }
}

/// 磁带上现在是什么。用来决定能不能自动格式化：只有空白带可以，别的数据绝不自动覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatProbe {
    /// 第一块是带 LTFS 标识的 VOL1 卷标
    Ltfs,
    /// 从带首就读到空白（从未写过，或已被整盘擦除）
    Blank,
    /// 有数据但不是 LTFS。附带看到了什么
    Foreign(String),
}

/// 只读探测：定位到带首读一块。不写任何东西。
pub fn probe_format(device: &dyn crate::scsi::transport::TapeTransport) -> Result<FormatProbe> {
    let drive = TapeDrive::new(device);
    drive.locate(0, VOL1_BLOCK, true)?;
    let mut buf = vec![0u8; DEFAULT_BLOCK_SIZE as usize];
    match drive.read_block(&mut buf) {
        Err(TapeError::ScsiCommand { sense_key: 0x08, .. }) => Ok(FormatProbe::Blank),
        Err(e) => Err(e),
        Ok(0) => Ok(FormatProbe::Foreign("带首是文件标记".to_string())),
        Ok(n) if n >= 28 && &buf[0..4] == b"VOL1" && &buf[24..28] == b"LTFS" => Ok(FormatProbe::Ltfs),
        Ok(n) => Ok(FormatProbe::Foreign(format!("带首 {} 字节，开头 {:02x?}", n, &buf[..n.min(8)]))),
    }
}

/// 池成员标记的键（LTFS 2.5.1 F.4）。写在卷根目录的扩展属性里，UUID 是身份，名称只是标签。
pub const XATTR_POOL_UUID: &str = "ltfs.mediaPool.uuid";
pub const XATTR_POOL_NAME: &str = "ltfs.mediaPool.name";

/// 收尾时对未索引数据的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TailPolicy {
    /// 放弃：数据块留在带上但不被任何文件引用，块范围记在卷根的扩展属性里。
    #[default]
    Discard,
    /// 打捞：读一遍这段数据，登记为 `_ltfs_lostandfound/` 下的只读文件。耗时与数据量成正比。
    Salvage,
}

/// 卷根扩展属性：历次收尾放弃的块范围，形如 `b:20-37;b:51-60`。
pub const XATTR_ABANDONED: &str = "tapers.abandonedBlocks";
/// 文件扩展属性：这一份内容的版本，形如 `轮次.序号`。
///
/// 同一路径可以同时存在于多盘带上（重写、回收搬迁），目录必须能判断哪一份是当前版本。
/// 轮次是 `Takeover` 条目的日志索引（全局单调），一轮之内只有一个执行者在串行写入，
/// 所以 (轮次, 序号) 是整个集群范围内的全序。没有这个属性的文件（EE 写的、打捞出来的）
/// 记为 (0, 0)。
pub const XATTR_VERSION: &str = "tapers.version";
/// 墓碑文件的扩展属性：被删除的路径。
pub const XATTR_DELETED_PATH: &str = "tapers.deletedPath";
/// 墓碑所在的目录。删除一个路径时，在当前写入带上登记一个零长度文件
/// `/.tapers/deleted/<sha256(路径)>`，带 `tapers.deletedPath` 与 `tapers.version`。
///
/// 被删文件的旧副本可能留在任何一盘别的带上，删除不去装载它们。带是目录的权威，
/// 所以"删过"这件事也必须落在带上，否则装载那盘旧带做一次对账就会把文件找回来，
/// 控制存储整体丢失后从带重建目录也会复活它。墓碑与文件用同一套版本比较。
pub const TOMBSTONE_DIR: &str = ".tapers/deleted";

/// 路径 `path`（以 `/` 开头）的墓碑文件在卷内的路径（不以 `/` 开头）。
/// 用哈希做文件名：被删路径里的目录层次不能在墓碑目录里重建（`/a` 和 `/a/b` 会冲突）。
pub fn tombstone_path(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    format!("{}/{}", TOMBSTONE_DIR, hex(&digest))
}

/// 卷内路径（不以 `/` 开头）是不是墓碑目录下的文件。
pub fn is_tombstone_path(path: &str) -> bool {
    path.trim_start_matches('/').strip_prefix(TOMBSTONE_DIR).is_some_and(|rest| rest.starts_with('/'))
}

/// 与 IBM LTFS 的 lost+found 目录同名。
pub const LOST_AND_FOUND: &str = "_ltfs_lostandfound";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseTailReport {
    pub tail: recovery::TailKind,
    /// 被放弃（或打捞）的块范围 `[first, end)`
    pub abandoned_first_block: u64,
    pub abandoned_end_block: u64,
    /// 打捞出的文件路径与字节数
    pub salvaged: Option<(String, u64)>,
    /// 收尾后的索引代数
    pub generation: u64,
}

/// `verify_file` 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashVerdict {
    Match { algo: &'static str },
    Mismatch { algo: &'static str, expected: String, actual: String },
    /// 索引里没有哈希属性（例如符号链接，或不写哈希的实现写的文件）。
    NoHash,
}

struct HashSink {
    md5: Md5,
    sha256: Sha256,
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.md5.update(buf);
        self.sha256.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 把符号链接目标解析成卷内路径。只跟随一层；绝对路径和越出卷根的返回 None。
fn resolve_symlink(link_path: &str, target: &str) -> Option<String> {
    if target.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = link_path.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// 逐级找到或新建目录。新目录从 `highest_uid` 取号：fileuid 0 是保留值，
/// IBM LTFS 遇到会拒读整份索引（LTFS17106E）。
fn ensure_dir<'d>(
    root: &'d mut DirectoryNode,
    highest_uid: &mut u64,
    parts: &[&str],
) -> &'d mut DirectoryNode {
    let mut cur = root;
    for part in parts {
        let pos = cur.subdirs.iter().position(|d| d.name == *part);
        let idx = match pos {
            Some(i) => i,
            None => {
                let now = crate::ltfs::label::ltfs_time_now();
                *highest_uid += 1;
                cur.subdirs.push(DirectoryNode {
                    name: (*part).to_string(),
                    meta: NodeMeta {
                        creation_time: now.clone(),
                        change_time: now.clone(),
                        modify_time: now.clone(),
                        access_time: now.clone(),
                        backup_time: now,
                        file_uid: *highest_uid,
                        ..Default::default()
                    },
                    ..Default::default()
                });
                cur.subdirs.len() - 1
            }
        };
        cur = &mut cur.subdirs[idx];
    }
    cur
}

/// 读 P0 label 那类"已知单块、较小"的数据。先 LOCATE，然后 read_block 一次。
fn read_single_block(
    drive: &TapeDrive<'_>,
    partition: u8,
    block: u64,
    cap: usize,
) -> Result<Bytes> {
    drive.locate(partition, block, true)?;
    let mut buf = BytesMut::zeroed(cap);
    let n = drive.read_block(&mut buf)?;
    buf.truncate(n);
    Ok(buf.freeze())
}

/// 当前位置写入 data 的全部字节，按 block_size 分块。返回写入块数。
fn write_bytes_in_blocks(drive: &TapeDrive<'_>, data: &[u8], block_size: usize) -> Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut blocks = 0;
    for chunk in data.chunks(block_size) {
        drive.write_block(chunk)?;
        blocks += 1;
    }
    Ok(blocks)
}

#[cfg(test)]
mod relocation_tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::scsi::sim::{SimCartridge, SimLibrary};

    #[test]
    fn relocation_keeps_logical_metadata_but_rebuilds_physical_identity_and_hashes() {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("META01L8", 64 << 20), 0).unwrap();
        lib.load_into_drive("META01L8", 0).unwrap();
        let dev = lib.drive(0);
        mkltfs(&dev, &MkltfsOptions::default()).unwrap();
        let when = "2020-01-02T03:04:05.123456789Z".to_string();
        let metadata = FileMetadata {
            meta: NodeMeta {
                readonly: true, file_uid: 9000, creation_time: when.clone(), change_time: when.clone(),
                modify_time: when.clone(), access_time: when.clone(), backup_time: when.clone(),
            },
            xattrs: vec![
                Xattr { key: "note".into(), value: "原始文本".into(), base64: false },
                Xattr { key: "binary".into(), value: "AAEC/w==".into(), base64: true },
                Xattr { key: XATTR_VERSION.into(), value: "1.1".into(), base64: false },
                Xattr { key: XATTR_MD5.into(), value: "过期摘要".into(), base64: false },
                Xattr { key: XATTR_SHA256.into(), value: "过期摘要".into(), base64: false },
            ],
        };
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        for (path, data) in [("/data", &b"content"[..]), ("/empty", &b""[..])] {
            vol.append_relocated_file(path, &mut std::io::Cursor::new(data), "9.7", &metadata).unwrap();
        }
        vol.commit().unwrap();
        let vol = LtfsVolume::mount(&dev).unwrap();
        let mut uids = std::collections::HashSet::new();
        for (path, data) in [("data", &b"content"[..]), ("empty", &b""[..])] {
            let node = vol.index().find_file(path).unwrap();
            assert!(node.meta.readonly);
            assert_ne!(node.meta.file_uid, 9000);
            assert!(uids.insert(node.meta.file_uid));
            assert_eq!(node.meta.modify_time, when);
            assert_eq!(node.meta.creation_time, when);
            assert_eq!(node.meta.change_time, when);
            assert_eq!(node.meta.access_time, when);
            assert_eq!(node.meta.backup_time, when);
            assert_eq!(node.xattr(XATTR_VERSION), Some("9.7"));
            assert_eq!(node.xattr("note"), Some("原始文本"));
            assert_eq!(node.xattrs.iter().find(|x| x.key == "binary"), Some(&metadata.xattrs[1]));
            assert_eq!(node.xattr(XATTR_MD5), Some(hex(&Md5::digest(data)).as_str()));
            assert_eq!(node.xattr(XATTR_SHA256), Some(hex(&Sha256::digest(data)).as_str()));
            let mut read = Vec::new();
            vol.read_file_to_writer(path, &mut read).unwrap();
            assert_eq!(read, data);
        }
    }
}
