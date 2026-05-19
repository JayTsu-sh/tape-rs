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
use chrono::Utc;
use log::{debug, info};

use crate::error::{Result, TapeError};
use crate::tape::commands::TapeDrive;

use super::index::{DirectoryNode, Extent, FileNode, IndexLocation, LtfsIndex, NodeMeta};
use super::label::{LtfsLabel, PART_INDEX};
use super::mam::{Mam, VolumeCoherencyInfo};

/// P0 / P1 起始块布局常量（与 `mkltfs` 生成的结构一一对应）。
pub const VOL1_BLOCK: u64 = 0;
pub const LABEL_BLOCK: u64 = 2;
pub const P0_INDEX_BLOCK: u64 = 4;
/// P1 数据区的首个可用块（label 之后）。
pub const P1_DATA_START: u64 = 4;

/// 默认块大小（字节）。LTFS 常用 512 KiB，可被 mkltfs 覆写。
pub const DEFAULT_BLOCK_SIZE: u32 = 512 * 1024;

pub struct LtfsVolume<'a> {
    drive: TapeDrive<'a>,
    mam: Mam<'a>,
    label: LtfsLabel,
    index: LtfsIndex,
    block_size: u32,
    /// P1 当前可写入位置。commit 后更新。
    p1_write_head: u64,
    dirty: bool,
}

impl<'a> LtfsVolume<'a> {
    /// 挂载卷：读 P0 label + latest index。
    pub fn mount(device: &'a crate::scsi::device::ScsiDevice) -> Result<Self> {
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

        // 2. 读 P0 index（block 4 起，直到 FM）
        let index_bytes = read_blocks_until_fm(&drive, 0, P0_INDEX_BLOCK, block_size as usize)?;
        let index = LtfsIndex::parse(&index_bytes)?;
        info!("index gen={} highest_uid={}", index.generation, index.highest_file_uid);

        // 3. P1 append point = P1 EOD。用 SPACE EOD + READ POSITION 获取真实位置。
        drive.locate(1, 0, true)?;
        drive.space_to_eod()?;
        let pos = drive.read_position()?;
        let p1_write_head = pos.block_number.max(P1_DATA_START);
        debug!("P1 write head (EOD) = {}", p1_write_head);

        Ok(Self {
            drive,
            mam,
            label,
            index,
            block_size,
            p1_write_head,
            dirty: false,
        })
    }

    pub fn label(&self) -> &LtfsLabel {
        &self.label
    }

    pub fn index(&self) -> &LtfsIndex {
        &self.index
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// 文件列表 (path, size)。
    pub fn list(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        self.index.walk_files(|p, f| out.push((p.to_string(), f.length)));
        out
    }

    /// 读取指定路径的文件内容到 writer，返回写出字节数。
    pub fn read_file_to_writer<W: Write>(&self, path: &str, w: &mut W) -> Result<u64> {
        let file = self
            .index
            .find_file(path)
            .ok_or_else(|| TapeError::Ltfs(format!("文件不存在: {}", path)))?;
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
                let slice_start = if first_block { ext.byte_offset as usize } else { 0 };
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
        self.index.highest_file_uid += 1;
        let uid = self.index.highest_file_uid;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
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
        let dir = ensure_dir(&mut self.index.root, &dir_parts);
        // 同名文件去重（覆盖）
        dir.files.retain(|f| f.name != file_name);
        dir.files.push(FileNode {
            name: file_name.to_string(),
            length: byte_count,
            meta,
            extents,
        });

        self.dirty = true;
        info!("追加 {}: {} 字节 @ P1 block {}", path, byte_count, start_block);
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

        let prev = Some(self.index.self_location);
        self.index.generation += 1;
        self.index.update_time = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        self.index.previous_location = prev;

        // —— 先写 P1 tail —— //
        // LOCATE P1 到 append point，先补一个 FM 分隔数据与 index
        self.drive.locate(1, self.p1_write_head, true)?;
        self.drive.write_filemark(1)?;
        // FM 消耗一个 block 号
        let p1_index_block = self.p1_write_head + 1;
        self.index.self_location = IndexLocation {
            partition: self.label.data_partition,
            start_block: p1_index_block,
        };

        let xml = self.index.to_xml()?;
        let blocks_written = write_bytes_in_blocks(&self.drive, &xml, self.block_size as usize)?;
        self.drive.write_filemark(1)?;
        self.p1_write_head = p1_index_block + blocks_written as u64 + 1;

        // —— 再写 P0 —— //
        // 同一份 XML 但 self_location 指向 P0
        let mut p0_index = self.index.clone();
        p0_index.self_location = IndexLocation {
            partition: self.label.index_partition,
            start_block: P0_INDEX_BLOCK,
        };
        let p0_xml = p0_index.to_xml()?;
        self.drive.locate(0, P0_INDEX_BLOCK, true)?;
        write_bytes_in_blocks(&self.drive, &p0_xml, self.block_size as usize)?;
        self.drive.write_filemark(1)?;

        // —— 更新 MAM VCI（LTFS 2.4 Annex B.3.2 Binary 66 字节）—— //
        let vci = VolumeCoherencyInfo {
            vcr: self.index.generation,
            count: 0,
            generation: self.index.generation,
            volume_uuid: self.label.volume_uuid,
        };
        self.mam.write_vci(&vci)?;

        self.dirty = false;
        info!("commit: gen {} @ P0 {}, P1 {}", self.index.generation, P0_INDEX_BLOCK, p1_index_block);
        Ok(())
    }

    /// 卸载：保证写回并倒带。调用后 Volume 被消费。
    pub fn unmount(mut self) -> Result<()> {
        if self.dirty {
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

fn ensure_dir<'d>(root: &'d mut DirectoryNode, parts: &[&str]) -> &'d mut DirectoryNode {
    let mut cur = root;
    for part in parts {
        let pos = cur.subdirs.iter().position(|d| d.name == *part);
        let idx = match pos {
            Some(i) => i,
            None => {
                let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                cur.subdirs.push(DirectoryNode {
                    name: (*part).to_string(),
                    meta: NodeMeta {
                        creation_time: now.clone(),
                        change_time: now.clone(),
                        modify_time: now.clone(),
                        access_time: now.clone(),
                        backup_time: now,
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
fn read_single_block(drive: &TapeDrive<'_>, partition: u8, block: u64, cap: usize) -> Result<Bytes> {
    drive.locate(partition, block, true)?;
    let mut buf = BytesMut::zeroed(cap);
    let n = drive.read_block(&mut buf)?;
    buf.truncate(n);
    Ok(buf.freeze())
}

/// 读到 filemark 为止，把内容拼起来返回（index XML 可能跨多块）。
fn read_blocks_until_fm(
    drive: &TapeDrive<'_>,
    partition: u8,
    start_block: u64,
    block_size: usize,
) -> Result<Bytes> {
    drive.locate(partition, start_block, true)?;
    let mut out = BytesMut::new();
    let mut buf = vec![0u8; block_size];
    loop {
        match drive.read_block(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // FILEMARK (sense_key=0x00, asc=0x00, ascq=0x01) / BLANK CHECK (0x08)
            Err(TapeError::ScsiCommand { sense_key: 0x00, asc: 0x00, ascq: 0x01, .. }) => break,
            Err(TapeError::ScsiCommand { sense_key: 0x08, .. }) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out.freeze())
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

