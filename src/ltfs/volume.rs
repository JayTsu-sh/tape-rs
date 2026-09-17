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
        let writable = report.append_ok && !lossy;
        let restricted_reason = if writable {
            None
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
