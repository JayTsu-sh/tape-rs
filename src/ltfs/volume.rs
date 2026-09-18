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
    DirectoryNode, Extent, FileNode, IndexLocation, LtfsIndex, NodeMeta, XATTR_MD5, XATTR_SHA256,
};
use super::label::{LtfsLabel, PART_INDEX};
use super::mam::{Mam, VolumeCoherencyInfo};
use super::recovery::{self, RecoveryBudget, RecoveryReport};

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
        let last_dp_index = recovery::dp_index_location(&report, label.data_partition);
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
        for (partition, block) in [(0u8, P0_INDEX_BLOCK), (1u8, p1_index_block)] {
            let vci = VolumeCoherencyInfo {
                vcr: vcr.to_vec(),
                generation: self.working.generation,
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
                    break;
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
        self.ensure_writable()?;
        if target.is_empty() {
            return Err(TapeError::Ltfs(format!("符号链接目标为空: {}", path)));
        }
        self.working.highest_file_uid += 1;
        let now = crate::ltfs::label::ltfs_time_now();
        let meta = NodeMeta {
            readonly: false,
            creation_time: now.clone(),
            change_time: now.clone(),
            modify_time: now.clone(),
            access_time: now.clone(),
            backup_time: now,
            file_uid: self.working.highest_file_uid,
        };
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
            ..Default::default()
        });
        self.dirty = true;
        Ok(())
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
    /// `ltfs.hash.md5sum` 总是由本函数按实际写入内容计算。
    pub fn append_file_with_xattrs<R: Read>(
        &mut self,
        path: &str,
        r: &mut R,
        xattrs: &[(&str, &str)],
    ) -> Result<u64> {
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
        let mut md5 = self.hash_policy.md5.then(Md5::new);
        let mut sha256 = self.hash_policy.sha256.then(Sha256::new);

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
        let meta = NodeMeta {
            readonly: false,
            creation_time: now.clone(),
            change_time: now.clone(),
            modify_time: now.clone(),
            access_time: now.clone(),
            backup_time: now,
            file_uid: uid,
        };
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
        if !self.dirty {
            debug!("commit: nothing to do");
            return Ok(());
        }
        self.ensure_writable()?;
        match self.commit_inner() {
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
    fn commit_inner(&mut self) -> std::result::Result<(), (&'static str, TapeError)> {
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
        let xml = self.working.to_xml().map_err(|e| ("encode", e))?;
        let blocks_written = write_bytes_in_blocks(&self.drive, &xml, self.block_size as usize)
            .map_err(|e| ("index_records", e))?;
        self.drive
            .write_filemark(1)
            .map_err(|e| ("closing_fm", e))?;
        self.p1_write_head = p1_index_block + blocks_written as u64 + 1;

        // —— IP：同一份 XML，自指针指向 P0 —— //
        let mut p0_index = self.working.clone();
        p0_index.self_location = IndexLocation {
            partition: self.label.index_partition,
            start_block: P0_INDEX_BLOCK,
        };
        // 一致卷的定义：IP 末索引回指 DP 上最后一个完整索引，也就是刚写好的同代那份。
        // 指向上一代的话 IBM LTFS 会判为不一致，挂载时自行补写 IP。
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
        self.last_dp_index = Some(self.working.self_location);
        self.index = self.working.clone();
        info!(
            "commit: gen {} @ P0 {}, P1 {}",
            self.working.generation, P0_INDEX_BLOCK, p1_index_block
        );
        Ok(())
    }

    /// 卸载：保证写回并倒带。调用后 Volume 被消费。
    pub fn unmount(mut self) -> Result<()> {
        if self.dirty && self.writable {
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
