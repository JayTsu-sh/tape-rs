//! `catalog list` / `find` / `show` 的只读查询。

use std::collections::HashMap;

use rusqlite::params;

use crate::error::Result;

use super::db::Catalog;

#[derive(Debug, Clone)]
pub struct VolumeRow {
    pub uuid: String,
    pub barcode: Option<String>,
    pub generation: u64,
    pub blocksize: u64,
    pub last_sync: String,
    pub file_count: u64,
    pub total_size: u64,
    /// MAM 容量快照（字节）。sync 时读不到就是 None。
    pub total_capacity: Option<u64>,
    pub remaining_capacity: Option<u64>,
}

/// inventory 展示用的轻量容量视图。
#[derive(Debug, Clone, Copy)]
pub struct CapacitySnapshot {
    pub total: u64,
    pub remaining: u64,
}

impl CapacitySnapshot {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.remaining)
    }
}

#[derive(Debug, Clone)]
pub struct FileHit {
    pub volume_uuid: String,
    pub barcode: Option<String>,
    pub path: String,
    pub size: u64,
    pub mtime: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileExtentRow {
    pub path: String,
    pub size: u64,
    pub file_uid: u64,
    pub partition: char,
    pub start_block: u64,
    pub byte_count: u64,
    pub file_offset: u64,
}

/// 列出所有已收录的卷，附带行数和总字节。
pub fn list_volumes(catalog: &Catalog) -> Result<Vec<VolumeRow>> {
    let mut stmt = catalog.conn().prepare(
        "SELECT v.uuid, v.barcode, v.generation, v.blocksize, v.last_sync,
                COALESCE((SELECT COUNT(*) FROM files f WHERE f.volume_uuid = v.uuid), 0),
                COALESCE((SELECT SUM(size) FROM files f WHERE f.volume_uuid = v.uuid), 0),
                v.total_capacity, v.remaining_capacity
         FROM volumes v
         ORDER BY COALESCE(v.barcode, v.uuid)",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(VolumeRow {
                uuid: r.get(0)?,
                barcode: r.get(1)?,
                generation: r.get::<_, i64>(2)? as u64,
                blocksize: r.get::<_, i64>(3)? as u64,
                last_sync: r.get(4)?,
                file_count: r.get::<_, i64>(5)? as u64,
                total_size: r.get::<_, i64>(6)? as u64,
                total_capacity: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                remaining_capacity: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 按 barcode 批量取容量快照。inventory 读一次就够，避免 N+1。
pub fn capacity_by_barcode(catalog: &Catalog) -> Result<HashMap<String, CapacitySnapshot>> {
    let mut stmt = catalog.conn().prepare(
        "SELECT barcode, total_capacity, remaining_capacity
         FROM volumes
         WHERE barcode IS NOT NULL
           AND total_capacity IS NOT NULL
           AND remaining_capacity IS NOT NULL",
    )?;
    let mut out = HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let barcode: String = r.get(0)?;
        let total: i64 = r.get(1)?;
        let remaining: i64 = r.get(2)?;
        out.insert(
            barcode,
            CapacitySnapshot { total: total as u64, remaining: remaining as u64 },
        );
    }
    Ok(out)
}

/// 按路径模糊查找。`pattern` 直接作为 SQL LIKE 使用：
/// - 含 `%` / `_` 的视为 LIKE 表达式（用户直接控制）；
/// - 否则按"包含"匹配，自动加 `%pattern%`。
pub fn find_by_path(catalog: &Catalog, pattern: &str, limit: usize) -> Result<Vec<FileHit>> {
    let like = if pattern.contains('%') || pattern.contains('_') {
        pattern.to_string()
    } else {
        format!("%{}%", pattern)
    };
    let mut stmt = catalog.conn().prepare(
        "SELECT f.volume_uuid, v.barcode, f.path, f.size, f.mtime
         FROM files f
         JOIN volumes v ON v.uuid = f.volume_uuid
         WHERE f.path LIKE ?1
         ORDER BY v.barcode, f.path
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![like, limit as i64], |r| {
            Ok(FileHit {
                volume_uuid: r.get(0)?,
                barcode: r.get(1)?,
                path: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                mtime: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 把 UUID 或 barcode 解析成 volumes.uuid。匹配不到返回 None。
pub fn resolve_volume(catalog: &Catalog, key: &str) -> Result<Option<String>> {
    let found: Option<String> = catalog
        .conn()
        .query_row(
            "SELECT uuid FROM volumes WHERE uuid = ?1 OR barcode = ?1 LIMIT 1",
            params![key],
            |r| r.get(0),
        )
        .ok();
    Ok(found)
}

/// 列出指定卷的所有文件（含首个 extent 的定位信息，用于脱机读规划）。
pub fn show_volume(catalog: &Catalog, volume_uuid: &str) -> Result<Vec<FileExtentRow>> {
    let mut stmt = catalog.conn().prepare(
        "SELECT f.path, f.size, f.file_uid,
                e.partition, e.start_block, e.byte_count, e.file_offset
         FROM files f
         LEFT JOIN extents e
           ON e.volume_uuid = f.volume_uuid AND e.file_uid = f.file_uid AND e.seq = 0
         WHERE f.volume_uuid = ?1
         ORDER BY f.path",
    )?;
    let rows = stmt
        .query_map(params![volume_uuid], |r| {
            let part: Option<String> = r.get(3)?;
            Ok(FileExtentRow {
                path: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
                file_uid: r.get::<_, i64>(2)? as u64,
                partition: part
                    .as_deref()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('?'),
                start_block: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                byte_count: r.get::<_, Option<i64>>(5)?.unwrap_or(0) as u64,
                file_offset: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
