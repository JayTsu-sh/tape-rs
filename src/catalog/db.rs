//! Catalog 连接与 schema 迁移。
//!
//! 单文件 SQLite。WAL 模式让 `catalog sync`（写）与 `catalog find`（读）
//! 并发无锁。schema 版本通过 `PRAGMA user_version` 追踪。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Result, TapeError};

/// 当前 schema 版本。每次 breaking 变动 +1 并在 `migrate` 里加迁移步骤。
pub const SCHEMA_VERSION: u32 = 2;

/// Catalog 连接包装。除 `conn()` 外，所有读写都应走 `sync` / `query` 子模块。
pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// 打开或创建 catalog。自动建目录、开 WAL、启外键、执行迁移。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    TapeError::Catalog(format!("无法创建目录 {}: {}", parent.display(), e))
                })?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL：多读 + 1 写并发；NORMAL 够：崩溃最坏丢最后一笔事务，不会损坏 DB。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let cat = Self { conn };
        cat.migrate()?;
        Ok(cat)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    fn migrate(&self) -> Result<()> {
        let current: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if current == SCHEMA_VERSION {
            return Ok(());
        }
        if current > SCHEMA_VERSION {
            return Err(TapeError::Catalog(format!(
                "catalog schema {} 比本程序支持的 {} 新",
                current, SCHEMA_VERSION
            )));
        }
        if current < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        if current < 2 {
            self.conn.execute_batch(SCHEMA_V2)?;
        }
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS volumes (
    uuid          TEXT PRIMARY KEY,
    barcode       TEXT,
    ltfs_version  TEXT NOT NULL,
    generation    INTEGER NOT NULL,
    blocksize     INTEGER NOT NULL,
    index_update  TEXT NOT NULL,
    last_sync     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS volumes_by_barcode ON volumes(barcode);

CREATE TABLE IF NOT EXISTS files (
    volume_uuid   TEXT NOT NULL REFERENCES volumes(uuid) ON DELETE CASCADE,
    file_uid      INTEGER NOT NULL,
    path          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    mtime         TEXT,
    ctime         TEXT,
    readonly      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (volume_uuid, file_uid)
);
CREATE INDEX IF NOT EXISTS files_by_path ON files(path);
CREATE INDEX IF NOT EXISTS files_by_size ON files(size);

CREATE TABLE IF NOT EXISTS extents (
    volume_uuid   TEXT NOT NULL,
    file_uid      INTEGER NOT NULL,
    seq           INTEGER NOT NULL,
    partition     TEXT NOT NULL,
    start_block   INTEGER NOT NULL,
    byte_offset   INTEGER NOT NULL,
    byte_count    INTEGER NOT NULL,
    file_offset   INTEGER NOT NULL,
    PRIMARY KEY (volume_uuid, file_uid, seq),
    FOREIGN KEY (volume_uuid, file_uid) REFERENCES files(volume_uuid, file_uid) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS extents_by_block ON extents(volume_uuid, partition, start_block);
"#;

/// v2：volumes 表加容量缓存字段。sync 时顺带写入；inventory 从这里读。
/// bytes 单位；NULL 表示尚未采集。
const SCHEMA_V2: &str = r#"
ALTER TABLE volumes ADD COLUMN total_capacity INTEGER;
ALTER TABLE volumes ADD COLUMN remaining_capacity INTEGER;
ALTER TABLE volumes ADD COLUMN capacity_sync TEXT;
"#;

/// 按文档里的优先级解析 catalog 文件路径：`override` > `$TAPE_RS_CATALOG` > XDG 默认。
pub fn resolve_path(override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("TAPE_RS_CATALOG") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let base = match std::env::var("XDG_DATA_HOME") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            let home = std::env::var("HOME").map_err(|_| {
                TapeError::Catalog(
                    "既未指定 --catalog / $TAPE_RS_CATALOG，也无 $HOME 可推导默认路径".into(),
                )
            })?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("tape-rs").join("catalog.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_and_migrates() {
        let path = std::env::temp_dir().join(format!(
            "tape-rs-catalog-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let cat = Catalog::open(&path).unwrap();
        let v: u32 = cat
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        drop(cat);
        let _ = std::fs::remove_file(&path);
    }
}
