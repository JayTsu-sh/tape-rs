//! catalog（SQLite）相关子命令。

use tape_rs::catalog::{self, Catalog};
use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::device::ScsiDevice;

use super::format_util::format_size;

fn open_catalog(override_path: Option<&str>) -> Result<Catalog> {
    let path = catalog::resolve_path(override_path)?;
    println!("catalog: {}", path.display());
    Catalog::open(&path)
}

pub fn cmd_catalog_sync(device_path: &str, catalog_path: Option<&str>, barcode: Option<&str>) -> Result<()> {
    let mut cat = open_catalog(catalog_path)?;
    let dev = ScsiDevice::open(device_path)?;
    let stats = catalog::sync_from_device(&mut cat, &dev, barcode)?;
    let cap_note = match stats.capacity {
        Some(c) => format!(
            " used={}/{}",
            format_size(c.used()),
            format_size(c.total),
        ),
        None => String::new(),
    };
    println!(
        "已同步: uuid={} barcode={} gen={} files={} extents={}{}{}",
        stats.volume_uuid,
        stats.barcode.as_deref().unwrap_or("-"),
        stats.generation,
        stats.files,
        stats.extents,
        cap_note,
        if stats.replaced_previous { "（覆盖旧快照）" } else { "" }
    );
    Ok(())
}

pub fn cmd_catalog_list(catalog_path: Option<&str>) -> Result<()> {
    let cat = open_catalog(catalog_path)?;
    let rows = catalog::query::list_volumes(&cat)?;
    if rows.is_empty() {
        println!("  (catalog 为空)");
        return Ok(());
    }
    println!(
        "{:<8}  {:<36}  {:>6}  {:>10}  {:>10}  {:>8}  {:>8}  {}",
        "BARCODE", "UUID", "GEN", "FILES", "SIZE", "USED", "TOTAL", "LAST SYNC"
    );
    for r in rows {
        let (used_s, total_s) = match (r.total_capacity, r.remaining_capacity) {
            (Some(t), Some(rem)) => (format_size(t.saturating_sub(rem)), format_size(t)),
            _ => ("-".to_string(), "-".to_string()),
        };
        println!(
            "{:<8}  {:<36}  {:>6}  {:>10}  {:>10}  {:>8}  {:>8}  {}",
            r.barcode.as_deref().unwrap_or("-"),
            r.uuid,
            r.generation,
            r.file_count,
            format_size(r.total_size),
            used_s,
            total_s,
            r.last_sync,
        );
    }
    Ok(())
}

pub fn cmd_catalog_find(pattern: &str, catalog_path: Option<&str>, limit: usize) -> Result<()> {
    let cat = open_catalog(catalog_path)?;
    let hits = catalog::query::find_by_path(&cat, pattern, limit)?;
    if hits.is_empty() {
        println!("  (无匹配)");
        return Ok(());
    }
    println!("{:<8}  {:>12}  {:<20}  {}", "BARCODE", "SIZE", "MTIME", "PATH");
    for h in hits {
        println!(
            "{:<8}  {:>12}  {:<20}  {}",
            h.barcode.as_deref().unwrap_or("-"),
            format_size(h.size),
            h.mtime.as_deref().unwrap_or("-"),
            h.path,
        );
    }
    Ok(())
}

pub fn cmd_catalog_show(key: &str, catalog_path: Option<&str>) -> Result<()> {
    let cat = open_catalog(catalog_path)?;
    let uuid = catalog::query::resolve_volume(&cat, key)?
        .ok_or_else(|| TapeError::Catalog(format!("未找到 uuid 或 barcode = {}", key)))?;
    let rows = catalog::query::show_volume(&cat, &uuid)?;
    println!("=== Volume {} ({} 个文件) ===", uuid, rows.len());
    if rows.is_empty() {
        return Ok(());
    }
    println!(
        "{:>12}  {:>4}  {:>10}  {}",
        "SIZE", "PART", "BLOCK", "PATH"
    );
    for r in rows {
        println!(
            "{:>12}  {:>4}  {:>10}  {}",
            format_size(r.size),
            r.partition,
            r.start_block,
            r.path,
        );
    }
    Ok(())
}
