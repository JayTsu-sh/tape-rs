//! 非 HA 逻辑库的池注册表；旧 assignment 中的隐式池仍按需可见。
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::{Library, refuse};
use crate::error::Result;
use crate::scsi::transport::TapeTransport;

const MEDIA_RESTRICTIONS: &[&str] = &[
    "none", "L5", "L6", "L7", "L8", "L9", "M8", "JB", "JC", "JD", "JE", "JY", "JZ", "JV", "JK",
    "JL", "JM", "JF",
];

const POOL_QUERY: &str = "SELECT names.name, p.uuid,
    COALESCE(p.media_restriction, 'none'), COALESCE(p.mount_limit, 0),
    (SELECT COUNT(*) FROM library_volumes v WHERE v.pool=names.name),
    COALESCE(g.nodegroup, 'G0'), COALESCE(w.low_enabled, 1),
    COALESCE(w.low_threshold, 0), COALESCE(w.no_enabled, 1)
    FROM (SELECT DISTINCT pool AS name FROM library_volumes
          UNION SELECT name FROM library_pools) names
    LEFT JOIN library_pools p ON p.name=names.name
    LEFT JOIN library_pool_groups g ON g.pool=names.name
    LEFT JOIN library_pool_warnings w ON w.pool=names.name";

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_pools (
            name TEXT PRIMARY KEY NOT NULL,
            uuid TEXT NOT NULL UNIQUE,
            media_restriction TEXT NOT NULL DEFAULT 'none',
            mount_limit INTEGER NOT NULL DEFAULT 0 CHECK(mount_limit BETWEEN 0 AND 32767)
        );
        CREATE TABLE IF NOT EXISTS library_pool_groups (
            pool TEXT PRIMARY KEY, nodegroup TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS library_pool_warnings (
            pool TEXT PRIMARY KEY, low_enabled INTEGER NOT NULL DEFAULT 1 CHECK(low_enabled IN (0,1)),
            low_threshold INTEGER NOT NULL DEFAULT 0 CHECK(low_threshold BETWEEN 0 AND 32767),
            no_enabled INTEGER NOT NULL DEFAULT 1 CHECK(no_enabled IN (0,1))
        );
        CREATE TABLE IF NOT EXISTS library_tape_capacity (
            barcode TEXT PRIMARY KEY, maximum INTEGER NOT NULL, remaining INTEGER NOT NULL,
            observed_at TEXT NOT NULL
        );
        CREATE TRIGGER IF NOT EXISTS library_capacity_unassign AFTER DELETE ON library_volumes
        BEGIN DELETE FROM library_tape_capacity WHERE barcode=OLD.barcode; END;
        CREATE TRIGGER IF NOT EXISTS library_capacity_access AFTER UPDATE OF phase ON library_volumes
        WHEN NEW.phase IN ('Recovering','Failed','NON_SUPPORTED_TAPE')
        BEGIN DELETE FROM library_tape_capacity WHERE barcode=NEW.barcode; END;
        CREATE TRIGGER IF NOT EXISTS library_capacity_home AFTER UPDATE OF home ON library_volumes
        WHEN NEW.home != OLD.home
        BEGIN DELETE FROM library_tape_capacity WHERE barcode=NEW.barcode; END;",
    )?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if !(1..=16).contains(&name.len())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(refuse("池名称必须为 1..16 个 ASCII 字母、数字或下划线"));
    }
    Ok(())
}

fn exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_pools WHERE name=?1)
             OR EXISTS(SELECT 1 FROM library_volumes WHERE pool=?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn require_pool(db: &Connection, name: &str) -> Result<()> {
    if !exists(db, name)? {
        return Err(refuse(format!("池不存在：{name}")));
    }
    Ok(())
}

fn describe(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let uuid: Option<String> = row.get(1)?;
    Ok(json!({
        "name": row.get::<_, String>(0)?,
        "explicit": uuid.is_some(),
        "uuid": uuid,
        "media_restriction": row.get::<_, String>(2)?,
        "mount_limit": row.get::<_, u16>(3)?,
        "num_of_tapes": row.get::<_, u64>(4)?,
        "nodegroup_name": row.get::<_, String>(5)?,
        "low_space_warning_enable": row.get::<_, bool>(6)?,
        "low_space_warning_threshold": row.get::<_, u16>(7)?,
        "no_space_warning_enable": row.get::<_, bool>(8)?,
    }))
}

/// 只查询资源库；缓存状态与实时状态共用同一份池信息。
pub(super) fn list_from_db(db: &Connection) -> Result<Value> {
    let mut statement = db.prepare(&format!("{POOL_QUERY} ORDER BY names.name"))?;
    let mut pools = statement
        .query_map([], describe)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for pool in &mut pools {
        decorate_capacity(db, pool)?;
    }
    Ok(Value::Array(pools))
}

fn decorate_capacity(db: &Connection, pool: &mut Value) -> Result<()> {
    let mut statement = db.prepare(
        "SELECT c.maximum,c.remaining,c.observed_at FROM library_volumes v
         LEFT JOIN library_tape_capacity c ON c.barcode=v.barcode WHERE v.pool=?1",
    )?;
    let rows = statement
        .query_map([pool["name"].as_str().unwrap_or("")], |r| {
            Ok((
                r.get::<_, Option<u64>>(0)?,
                r.get::<_, Option<u64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut complete = !rows.is_empty();
    let (mut maximum, mut remaining) = (0u64, 0u64);
    let mut oldest: Option<String> = None;
    for (max, free, observed) in rows {
        if let (Some(max), Some(free), Some(observed)) = (max, free, observed) {
            maximum = maximum
                .checked_add(max)
                .ok_or_else(|| refuse("池容量合计溢出"))?;
            remaining = remaining
                .checked_add(free)
                .ok_or_else(|| refuse("池剩余容量合计溢出"))?;
            if oldest.as_ref().is_none_or(|s| observed < *s) {
                oldest = Some(observed);
            }
        } else {
            complete = false;
        }
    }
    // 这是已观测 DP 原始剩余字节的本地阈值判断，不是 EE 后台告警事件或可回收空间估算。
    let low = complete.then(|| {
        pool["low_space_warning_enable"] == true
            && pool["low_space_warning_threshold"].as_u64().unwrap_or(0) != 0
            && remaining < pool["low_space_warning_threshold"].as_u64().unwrap_or(0) * (1u64 << 40)
    });
    let no_space = complete.then(|| pool["no_space_warning_enable"] == true && remaining == 0);
    pool["capacity"] = json!(complete.then_some(maximum));
    pool["free_space"] = json!(complete.then_some(remaining));
    pool["used_space"] = json!(complete.then_some(maximum - remaining));
    pool["capacity_source"] = json!(if complete {
        "observed_dp_mam"
    } else {
        "unknown"
    });
    pool["capacity_observed_at"] = json!(if complete { oldest } else { None });
    pool["low_space_warning_threshold_unit"] = json!("TiB");
    pool["warning_scope"] = json!("observed_dp_free_bytes");
    pool["low_space_warning"] = json!(low);
    pool["no_space_warning"] = json!(no_space);
    Ok(())
}

fn check_media(restriction: &str, barcode: &str) -> Result<()> {
    // none 兼容历史条码；其他规则只检查六字符前缀与介质后缀，不推断物理密度。
    if restriction == "none" {
        return Ok(());
    }
    if !MEDIA_RESTRICTIONS.contains(&restriction) {
        return Err(refuse(format!("不支持的池介质限制：{restriction}")));
    }
    if !barcode
        .strip_suffix(restriction)
        .is_some_and(|prefix| prefix.chars().count() == 6)
    {
        return Err(refuse(format!(
            "条码 {barcode} 不符合池介质限制 {restriction}（六字符前缀加介质后缀）"
        )));
    }
    Ok(())
}

impl<C: TapeTransport, D: TapeTransport> Library<C, D> {
    /// 创建显式空池，仅修改本地资源库。
    pub fn pool_create(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let transaction = self.db.unchecked_transaction()?;
        if exists(&transaction, name)? {
            return Err(refuse(format!("池已存在：{name}")));
        }
        transaction.execute(
            "INSERT INTO library_pools(name,uuid) VALUES(?1,?2)",
            params![name, uuid::Uuid::new_v4().to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pool_delete(&mut self, name: &str) -> Result<()> {
        let transaction = self.db.unchecked_transaction()?;
        require_pool(&transaction, name)?;
        let nonempty: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE pool=?1)",
            [name],
            |r| r.get(0),
        )?;
        if nonempty {
            return Err(refuse(format!("池非空，不能删除：{name}")));
        }
        transaction.execute("DELETE FROM library_pools WHERE name=?1", [name])?;
        transaction.execute("DELETE FROM library_pool_groups WHERE pool=?1", [name])?;
        transaction.execute("DELETE FROM library_pool_warnings WHERE pool=?1", [name])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pool_show(&self, name: &str) -> Result<Value> {
        let mut pool = self
            .db
            .query_row(
                &format!("{POOL_QUERY} WHERE names.name=?1"),
                [name],
                describe,
            )
            .optional()?
            .ok_or_else(|| refuse(format!("池不存在：{name}")))?;
        decorate_capacity(&self.db, &mut pool)?;
        Ok(pool)
    }

    pub fn pool_list(&self) -> Result<Value> {
        list_from_db(&self.db)
    }

    /// 本地调度分组。旧隐式池沿用 G0，不推断远端节点归属。
    pub fn pool_nodegroup(&self, name: &str) -> Result<String> {
        require_pool(&self.db, name)?;
        Ok(self
            .db
            .query_row(
                "SELECT nodegroup FROM library_pool_groups WHERE pool=?1",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "G0".into()))
    }

    /// 保存调用方从两个实际 DP MAM 容量属性得到的完整字节观测。
    /// None 清除未知值；缺属性不得传 0。观测在下次卷访问前保守失效。
    /// 不读取设备、不估算容量，也不代表 EE 的可回收/不可追加空间。
    pub fn record_tape_capacity(&self, barcode: &str, measured: Option<(u64, u64)>) -> Result<()> {
        let assigned: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE barcode=?1)",
            [barcode],
            |r| r.get(0),
        )?;
        if !assigned {
            return Err(refuse("NEED_ASSIGN"));
        }
        match measured {
            Some((maximum, remaining)) => {
                if maximum == 0 || remaining > maximum || maximum > i64::MAX as u64 {
                    return Err(refuse(
                        "容量观测无效：maximum 必须为正整数，remaining 不得大于 maximum",
                    ));
                }
                self.db.execute(
                    "INSERT INTO library_tape_capacity VALUES(?1,?2,?3,?4)
                     ON CONFLICT(barcode) DO UPDATE SET maximum=excluded.maximum,
                     remaining=excluded.remaining,observed_at=excluded.observed_at",
                    params![
                        barcode,
                        maximum,
                        remaining,
                        crate::ltfs::label::ltfs_time_now()
                    ],
                )?;
            }
            None => {
                self.db.execute(
                    "DELETE FROM library_tape_capacity WHERE barcode=?1",
                    [barcode],
                )?;
            }
        }
        Ok(())
    }

    /// 校验后原子更新配置；旧隐式池仅在成功 set 后取得持久 UUID。
    pub fn pool_set(&mut self, name: &str, attribute: &str, value: &str) -> Result<()> {
        let limit = match attribute {
            "poolname" => {
                validate_name(value)?;
                None
            }
            "mediarestriction" => {
                if !MEDIA_RESTRICTIONS.contains(&value) {
                    return Err(refuse(format!("不支持的池介质限制：{value}")));
                }
                None
            }
            "mountlimit" | "lowspacewarningthreshold" => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(refuse(format!("{attribute} 必须为 0..32767 的整数")));
                }
                Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|n| *n <= 32767)
                        .ok_or_else(|| refuse(format!("{attribute} 必须为 0..32767 的整数")))?,
                )
            }
            "nodegroup" => {
                self.require_nodegroup(value)?;
                None
            }
            "lowspacewarningenable" | "nospacewarningenable" => {
                if !["yes", "no"].contains(&value) {
                    return Err(refuse(format!("{attribute} 必须为 yes 或 no")));
                }
                None
            }
            _ => return Err(refuse(format!("不支持的池属性：{attribute}"))),
        };

        if attribute == "nodegroup" && self.pool_nodegroup(name)? != value {
            self.check_pool_group_change(name)?;
        }

        let transaction = self.db.unchecked_transaction()?;
        require_pool(&transaction, name)?;
        if attribute == "poolname" && value != name && exists(&transaction, value)? {
            return Err(refuse(format!("目标池已存在：{value}")));
        }
        if attribute == "mediarestriction" {
            let mut statement =
                transaction.prepare("SELECT barcode FROM library_volumes WHERE pool=?1")?;
            for barcode in statement.query_map([name], |r| r.get::<_, String>(0))? {
                check_media(value, &barcode?)?;
            }
        }
        transaction.execute(
            "INSERT INTO library_pools(name,uuid)
             SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM library_pools WHERE name=?1)",
            params![name, uuid::Uuid::new_v4().to_string()],
        )?;
        match attribute {
            "poolname" => {
                transaction.execute(
                    "UPDATE library_pools SET name=?2 WHERE name=?1",
                    params![name, value],
                )?;
                transaction.execute(
                    "UPDATE library_volumes SET pool=?2 WHERE pool=?1",
                    params![name, value],
                )?;
                transaction.execute(
                    "UPDATE library_pool_groups SET pool=?2 WHERE pool=?1",
                    params![name, value],
                )?;
                transaction.execute(
                    "UPDATE library_pool_warnings SET pool=?2 WHERE pool=?1",
                    params![name, value],
                )?;
            }
            "mediarestriction" => {
                transaction.execute(
                    "UPDATE library_pools SET media_restriction=?2 WHERE name=?1",
                    params![name, value],
                )?;
            }
            "mountlimit" => {
                transaction.execute(
                    "UPDATE library_pools SET mount_limit=?2 WHERE name=?1",
                    params![name, limit],
                )?;
            }
            "nodegroup" => {
                transaction.execute(
                    "INSERT INTO library_pool_groups(pool,nodegroup) VALUES(?1,?2)
                     ON CONFLICT(pool) DO UPDATE SET nodegroup=excluded.nodegroup",
                    params![name, value],
                )?;
            }
            "lowspacewarningenable" | "lowspacewarningthreshold" | "nospacewarningenable" => {
                transaction.execute(
                    "INSERT OR IGNORE INTO library_pool_warnings(pool) VALUES(?1)",
                    [name],
                )?;
                let column = match attribute {
                    "lowspacewarningenable" => "low_enabled",
                    "nospacewarningenable" => "no_enabled",
                    _ => "low_threshold",
                };
                let setting = limit.map(i64::from).unwrap_or(i64::from(value == "yes"));
                transaction.execute(
                    &format!("UPDATE library_pool_warnings SET {column}=?2 WHERE pool=?1"),
                    params![name, setting],
                )?;
            }
            _ => unreachable!(),
        }
        transaction.commit()?;
        Ok(())
    }

    pub(super) fn pool_names(&self) -> Result<Vec<String>> {
        let mut statement = self.db.prepare(
            "SELECT DISTINCT pool AS name FROM library_volumes
             UNION SELECT name FROM library_pools ORDER BY name",
        )?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// 0 表示池不额外限制挂载数；实际调度仍须核对设备与 reservation。
    pub(super) fn pool_mount_limit(&self, pool: &str) -> Result<usize> {
        require_pool(&self.db, pool)?;
        Ok(self
            .db
            .query_row(
                "SELECT mount_limit FROM library_pools WHERE name=?1",
                [pool],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub(super) fn check_pool_media(&self, pool: &str, barcode: &str) -> Result<()> {
        // assign 仍可创建旧式隐式池，因此未注册的池默认不限制条码。
        let restriction: Option<String> = self
            .db
            .query_row(
                "SELECT media_restriction FROM library_pools WHERE name=?1",
                [pool],
                |r| r.get(0),
            )
            .optional()?;
        check_media(restriction.as_deref().unwrap_or("none"), barcode)
    }
}
