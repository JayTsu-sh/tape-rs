//! `catalog sync`：mount 卷 → 读 MAM barcode → 把 index 灌进 DB。
//!
//! 语义：**全量 replace**。开始时 `DELETE FROM volumes WHERE uuid=?` 触发
//! 级联删掉 files + extents，再按当前 index 重新 insert。不支持增量 —— LTFS 的
//! generation chain 意味着任一 commit 都可能挪动 extent 起点，差分开销 > 全插。
//!
//! 一个 generation 通常 10 万文件 × 2 extent ≈ 30 万行，全放一个事务里，无分批需要。

use chrono::Utc;
use log::{debug, info};
use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::ltfs::index::FileNode;
use crate::ltfs::mam::{ATTR_BARCODE, Mam, VolumeCapacity, read_volume_capacity};
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::device::ScsiDevice;

use super::db::Catalog;

#[derive(Debug, Clone)]
pub struct SyncStats {
    pub volume_uuid: String,
    pub barcode: Option<String>,
    pub generation: u64,
    pub files: usize,
    pub extents: usize,
    pub replaced_previous: bool,
    pub capacity: Option<VolumeCapacity>,
}

/// mount 指定 sg 设备上的 LTFS 卷，把最新 index 灌库。
///
/// `barcode_override` 用于手工校正（老带条 MAM 没写或写错时）；`None` 则读 MAM 0x0806。
pub fn sync_from_device(
    catalog: &mut Catalog,
    device: &ScsiDevice,
    barcode_override: Option<&str>,
) -> Result<SyncStats> {
    let vol = LtfsVolume::mount(device)?;
    let barcode = match barcode_override {
        Some(s) => Some(s.to_string()),
        None => read_barcode(device).unwrap_or(None),
    };
    // 容量读失败不挡 sync（例如驱动器/介质不支持），降级为 None 就行。
    let capacity = match read_volume_capacity(device) {
        Ok(c) if c.total > 0 => Some(c),
        Ok(_) => None,
        Err(e) => {
            debug!("读 MAM 容量失败，跳过: {}", e);
            None
        }
    };
    sync_from_volume(catalog, &vol, barcode, capacity)
}

/// 已经 mount 好的 volume → 入库。抽出来便于上层复用 LtfsVolume。
pub fn sync_from_volume(
    catalog: &mut Catalog,
    vol: &LtfsVolume<'_>,
    barcode: Option<String>,
    capacity: Option<VolumeCapacity>,
) -> Result<SyncStats> {
    let label = vol.label();
    let index = vol.index();
    let uuid_str = label.volume_uuid.to_string();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let tx = catalog.conn_mut().transaction()?;

    let replaced_previous: bool = tx
        .query_row(
            "SELECT 1 FROM volumes WHERE uuid = ?1",
            params![uuid_str],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if replaced_previous {
        tx.execute("DELETE FROM volumes WHERE uuid = ?1", params![uuid_str])?;
    }

    let (cap_total, cap_remaining, cap_sync) = match capacity {
        Some(c) => (Some(c.total as i64), Some(c.remaining as i64), Some(now.clone())),
        None => (None, None, None),
    };

    tx.execute(
        "INSERT INTO volumes (uuid, barcode, ltfs_version, generation, blocksize, index_update, last_sync,
                              total_capacity, remaining_capacity, capacity_sync)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            uuid_str,
            barcode,
            label.version,
            index.generation as i64,
            label.blocksize as i64,
            index.update_time,
            now,
            cap_total,
            cap_remaining,
            cap_sync,
        ],
    )?;

    let mut files_count = 0usize;
    let mut extents_count = 0usize;
    {
        let mut file_stmt = tx.prepare(
            "INSERT INTO files (volume_uuid, file_uid, path, size, mtime, ctime, readonly)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut extent_stmt = tx.prepare(
            "INSERT INTO extents
               (volume_uuid, file_uid, seq, partition, start_block, byte_offset, byte_count, file_offset)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;

        // 一次性 DFS：闭包里直接插，把首个错误通过 sticky holder 带出来。
        let mut sticky: Option<rusqlite::Error> = None;
        index.walk_files(|path, f| {
            if sticky.is_some() {
                return;
            }
            if let Err(e) = insert_file_and_extents(
                &mut file_stmt,
                &mut extent_stmt,
                &uuid_str,
                path,
                f,
                &mut files_count,
                &mut extents_count,
            ) {
                sticky = Some(e);
            }
        });
        if let Some(e) = sticky {
            return Err(e.into());
        }
    }

    tx.commit()?;

    info!(
        "catalog sync: uuid={} barcode={:?} gen={} files={} extents={} replaced={}",
        uuid_str, barcode, index.generation, files_count, extents_count, replaced_previous
    );
    Ok(SyncStats {
        volume_uuid: uuid_str,
        barcode,
        generation: index.generation,
        files: files_count,
        extents: extents_count,
        replaced_previous,
        capacity,
    })
}

/// 单文件入库：先插 `files` 一行，再把它的 extents 按 seq 顺序批量插入。
fn insert_file_and_extents(
    file_stmt: &mut rusqlite::Statement<'_>,
    extent_stmt: &mut rusqlite::Statement<'_>,
    uuid_str: &str,
    path: &str,
    f: &FileNode,
    files_count: &mut usize,
    extents_count: &mut usize,
) -> rusqlite::Result<()> {
    file_stmt.execute(params![
        uuid_str,
        f.meta.file_uid as i64,
        path,
        f.length as i64,
        nonempty(&f.meta.modify_time),
        nonempty(&f.meta.creation_time),
        f.meta.readonly as i64,
    ])?;
    *files_count += 1;

    for (seq, ext) in f.extents.iter().enumerate() {
        extent_stmt.execute(params![
            uuid_str,
            f.meta.file_uid as i64,
            seq as i64,
            ext.partition.to_string(),
            ext.start_block as i64,
            ext.byte_offset as i64,
            ext.byte_count as i64,
            ext.file_offset as i64,
        ])?;
        *extents_count += 1;
    }
    Ok(())
}

fn nonempty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// 从 MAM 0x0806 读 barcode，去掉 ASCII 空格填充。失败或空返回 `Ok(None)`。
fn read_barcode(device: &ScsiDevice) -> Result<Option<String>> {
    let mam = Mam::new(device);
    let attr = match mam.read_attribute(ATTR_BARCODE)? {
        Some(a) => a,
        None => return Ok(None),
    };
    let s = std::str::from_utf8(&attr.value)
        .unwrap_or("")
        .trim_matches(|c: char| c == '\0' || c == ' ')
        .to_string();
    debug!("MAM barcode raw={:?} trimmed={:?}", &attr.value[..], s);
    Ok(if s.is_empty() { None } else { Some(s) })
}
