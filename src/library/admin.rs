//! 单进程逻辑库的管理状态；不实现 HSM、服务主备或跨节点配置。
use super::*;

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_tape_options (
           barcode TEXT PRIMARY KEY, offline_msg TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS library_drive_options (
           serial TEXT PRIMARY KEY, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)));
         CREATE TABLE IF NOT EXISTS library_drive_roles (
           serial TEXT PRIMARY KEY, role TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS library_nodegroups (name TEXT PRIMARY KEY);
         INSERT OR IGNORE INTO library_nodegroups VALUES('G0');
         CREATE TABLE IF NOT EXISTS library_local_node (
           singleton INTEGER PRIMARY KEY CHECK(singleton=1), nodegroup TEXT);
         INSERT OR IGNORE INTO library_local_node VALUES(1,'G0');",
    )?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_drive_groups (
        serial TEXT PRIMARY KEY, nodegroup TEXT);",
    )?;
    Ok(())
}

pub(super) fn decorate_status(db: &Connection, status: &mut Value) -> Result<()> {
    status["nodegroups"] = nodegroups_from_db(db)?;
    status["local_nodegroup"] = json!(local_group_from_db(db)?);
    if let Some(volumes) = status["volumes"].as_array_mut() {
        for volume in volumes {
            let message: Option<String> = db
                .query_row(
                    "SELECT offline_msg FROM library_tape_options WHERE barcode=?1",
                    [volume["barcode"].as_str().unwrap_or("")],
                    |row| row.get(0),
                )
                .optional()?;
            volume["offline_msg"] = json!(message);
            if message.is_some() {
                volume["state"] = json!("Offline");
            }
        }
    }
    if let Some(drives) = status["drives"].as_array_mut() {
        let mut statement =
            db.prepare("SELECT serial FROM library_drive_options WHERE enabled=0")?;
        let disabled = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for drive in drives {
            let id = drive["serial"].as_str().unwrap_or("");
            let enabled = !disabled.iter().any(|serial| drive_id_matches(id, serial));
            let role = role_from_db(db, id)?;
            let group = group_from_db(db, id)?;
            drive["enabled"] = json!(enabled);
            drive["role"] = json!(role);
            drive["nodegroup_name"] = json!(group);
        }
    }
    Ok(())
}

fn local_group_from_db(db: &Connection) -> Result<Option<String>> {
    Ok(db.query_row(
        "SELECT nodegroup FROM library_local_node WHERE singleton=1",
        [],
        |r| r.get(0),
    )?)
}

fn nodegroups_from_db(db: &Connection) -> Result<Value> {
    let local = local_group_from_db(db)?;
    let mut statement = db.prepare("SELECT name FROM library_nodegroups ORDER BY name")?;
    let groups = statement
        .query_map([], |r| {
            let name: String = r.get(0)?;
            Ok(json!({"local_member": local.as_deref() == Some(&name),
            "name":name, "scope":"local_direct_manager"}))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Value::Array(groups))
}

fn role_from_db(db: &Connection, id: &str) -> Result<String> {
    let mut statement = db.prepare("SELECT serial,role FROM library_drive_roles")?;
    for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (serial, role) = row?;
        if drive_id_matches(id, &serial) {
            return Ok(role);
        }
    }
    Ok("mrg".into())
}

fn group_from_db(db: &Connection, id: &str) -> Result<Option<String>> {
    let mut statement = db.prepare("SELECT serial,nodegroup FROM library_drive_groups")?;
    for row in statement.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })? {
        let (serial, group) = row?;
        if drive_id_matches(id, &serial) {
            return Ok(group);
        }
    }
    Ok(Some("G0".into()))
}

impl<C: TapeTransport, D: TapeTransport> Library<C, D> {
    pub(super) fn require_nodegroup(&self, name: &str) -> Result<()> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_nodegroups WHERE name=?1)",
            [name],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(refuse(format!("本地节点组不存在：{name}")));
        }
        Ok(())
    }

    /// 单个 direct manager 的本地成员资格；没有远端节点注册或故障接管语义。
    /// None 显式停止该管理器在所有组中的调度资格。
    pub fn local_nodegroup(&self) -> Result<Option<String>> {
        local_group_from_db(&self.db)
    }

    pub fn nodegroup_set_local(&mut self, name: Option<&str>) -> Result<()> {
        if let Some(name) = name {
            self.require_nodegroup(name)?;
        }
        if self.local_nodegroup()?.as_deref() != name {
            let (map, inv) = self.inventory()?;
            for (idx, drive) in self.drives.iter().enumerate() {
                let entry = inv
                    .iter()
                    .find(|e| {
                        e.element_type == ElementType::DataTransfer
                            && e.address == map.dt_start + drive.offset
                    })
                    .ok_or_else(|| refuse("库存缺少配置驱动器"))?;
                self.ensure_drive_policy_change(idx, entry.full)?;
            }
        }
        self.db.execute(
            "UPDATE library_local_node SET nodegroup=?1 WHERE singleton=1",
            [name],
        )?;
        Ok(())
    }

    /// 仅在本地 SQLite 中创建调度分组，不创建分布式节点。
    pub fn nodegroup_create(&mut self, name: &str) -> Result<()> {
        if !(1..=16).contains(&name.len())
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(refuse("节点组名称必须为 1..16 个 ASCII 字母、数字或下划线"));
        }
        if self.db.execute(
            "INSERT OR IGNORE INTO library_nodegroups VALUES(?1)",
            [name],
        )? == 0
        {
            return Err(refuse("本地节点组已存在"));
        }
        Ok(())
    }

    pub fn nodegroup_show(&self, name: &str) -> Result<Value> {
        self.require_nodegroup(name)?;
        Ok(json!({"name":name, "scope":"local_direct_manager",
            "local_member": self.local_nodegroup()?.as_deref() == Some(name)}))
    }

    pub fn nodegroup_list(&self) -> Result<Value> {
        nodegroups_from_db(&self.db)
    }

    pub fn nodegroup_delete(&mut self, name: &str) -> Result<()> {
        self.require_nodegroup(name)?;
        if name == "G0" || self.local_nodegroup()?.as_deref() == Some(name) {
            return Err(refuse("节点组仍被引用或为兼容默认组，不能删除"));
        }
        let used: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_drive_groups WHERE nodegroup=?1)
                 OR EXISTS(SELECT 1 FROM library_pool_groups WHERE nodegroup=?1)",
            [name],
            |r| r.get(0),
        )?;
        if used {
            return Err(refuse("节点组仍分配给驱动器或池，不能删除"));
        }
        self.db
            .execute("DELETE FROM library_nodegroups WHERE name=?1", [name])?;
        Ok(())
    }

    pub fn drive_nodegroup(&self, serial: &str) -> Result<Option<String>> {
        let (_, actual, _) = self.configured_drive(serial)?;
        group_from_db(&self.db, &actual)
    }

    /// 本地驱动分配。None 显式取消分配，后续任何角色均不可调度。
    /// 不执行远端节点分配、服务切换或机械操作。
    pub fn drive_set_nodegroup(&mut self, serial: &str, name: Option<&str>) -> Result<()> {
        if let Some(name) = name {
            self.require_nodegroup(name)?;
        }
        let (idx, actual, full) = self.configured_drive(serial)?;
        self.ensure_drive_policy_change(idx, full)?;
        self.db.execute(
            "INSERT INTO library_drive_groups(serial,nodegroup) VALUES(?1,?2)
             ON CONFLICT(serial) DO UPDATE SET nodegroup=excluded.nodegroup",
            params![actual, name],
        )?;
        Ok(())
    }

    /// 查询本地调度策略；不取得 reservation、不装带。pool 必须存在。
    pub fn drive_eligible(&self, idx: usize, pool: &str, role: char) -> Result<bool> {
        let (map, inv) = self.inventory()?;
        self.drive_eligible_in(idx, pool, role, &map, &inv)
    }

    /// select 必须在挂载额度驱逐前调用，并复用于所有候选路径。
    pub(super) fn drive_eligible_in(
        &self,
        idx: usize,
        pool: &str,
        role: char,
        map: &ElementAddressMap,
        inv: &[ElementStatus],
    ) -> Result<bool> {
        if !"mrg".contains(role) {
            return Err(refuse("操作角色必须为 m、r 或 g"));
        }
        let drive = self
            .drives
            .get(idx)
            .ok_or_else(|| refuse("驱动器索引越界"))?;
        let pool_group = self.pool_nodegroup(pool)?;
        let entry = inv
            .iter()
            .find(|e| {
                e.element_type == ElementType::DataTransfer
                    && e.address == map.dt_start + drive.offset
            })
            .ok_or_else(|| refuse("库存缺少配置驱动器"))?;
        let id = entry
            .drive_id
            .as_deref()
            .ok_or_else(|| refuse("库存缺少驱动器序列号"))?;
        let group = group_from_db(&self.db, id)?;
        Ok(self.drive_enabled(idx, map, inv)?
            && role_from_db(&self.db, id)?.contains(role)
            && group.as_deref() == Some(pool_group.as_str())
            && self.local_nodegroup()? == group)
    }

    fn configured_drive(&self, serial: &str) -> Result<(usize, String, bool)> {
        let (map, inv) = self.inventory()?;
        for (idx, drive) in self.drives.iter().enumerate() {
            let entry = inv
                .iter()
                .find(|e| {
                    e.element_type == ElementType::DataTransfer
                        && e.address == map.dt_start + drive.offset
                })
                .ok_or_else(|| refuse("库存缺少配置驱动器"))?;
            let actual =
                read_unit_serial(&drive.device).ok_or_else(|| refuse("无法核验驱动器序列号"))?;
            if !entry
                .drive_id
                .as_deref()
                .is_some_and(|id| drive_id_matches(id, &actual))
            {
                return Err(refuse("DTE 与驱动器序列号不匹配"));
            }
            if serial == actual || entry.drive_id.as_deref() == Some(serial) {
                return Ok((idx, actual, entry.full));
            }
        }
        Err(refuse("未知或未配置的驱动器序列号"))
    }

    fn ensure_drive_policy_change(&self, idx: usize, full: bool) -> Result<()> {
        if self.stopped {
            return Err(refuse("设备资格已失去，请关闭并重新核验现场"));
        }
        if full && !self.claimed_drives[idx] {
            return Err(refuse("驱动器含其他实例装载的介质；不能修改调度归属或角色"));
        }
        if self.claimed_drives[idx] {
            reservation::verify_holder(&self.drives[idx].device, self.key)?;
        } else if reservation::read_status(&self.drives[idx].device)?
            .holder
            .is_some()
        {
            return Err(refuse("驱动器由其他实例预留；不能修改调度归属或角色"));
        }
        Ok(())
    }

    pub(super) fn check_pool_group_change(&self, pool: &str) -> Result<()> {
        let (map, inv) = self.inventory()?;
        for entry in inv
            .iter()
            .filter(|e| e.element_type == ElementType::DataTransfer && e.full)
        {
            let Some(barcode) = entry.volume_tag.as_deref() else {
                continue;
            };
            let in_pool: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE barcode=?1 AND pool=?2)",
                params![barcode, pool],
                |r| r.get(0),
            )?;
            if in_pool {
                let idx = self
                    .drives
                    .iter()
                    .position(|d| map.dt_start + d.offset == entry.address)
                    .ok_or_else(|| refuse("池介质位于未配置驱动器，不能改变池分组"))?;
                self.ensure_drive_policy_change(idx, true)?;
            }
        }
        Ok(())
    }

    /// 本地调度角色：m 写入、r 读取、g 维护；未配置时沿用 mrg。
    pub fn drive_role(&self, serial: &str) -> Result<String> {
        let (_, actual, _) = self.configured_drive(serial)?;
        role_from_db(&self.db, &actual)
    }

    /// 仅修改后续调度资格，不移动介质或写入磁带。
    pub fn drive_set_role(&mut self, serial: &str, role: &str) -> Result<()> {
        let mut roles = BTreeSet::new();
        if role.is_empty() || !role.chars().all(|r| "mrg".contains(r) && roles.insert(r)) {
            return Err(refuse("驱动器角色必须为非空且无重复的 m/r/g 组合"));
        }
        let canonical: String = "mrg".chars().filter(|r| roles.contains(r)).collect();
        let (idx, actual, full) = self.configured_drive(serial)?;
        self.ensure_drive_policy_change(idx, full)?;
        self.db.execute(
            "INSERT INTO library_drive_roles(serial,role) VALUES(?1,?2)
             ON CONFLICT(serial) DO UPDATE SET role=excluded.role",
            params![actual, canonical],
        )?;
        Ok(())
    }

    pub(super) fn check_online(&self, barcode: &str) -> Result<()> {
        let offline: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_tape_options WHERE barcode=?1)",
            [barcode],
            |row| row.get(0),
        )?;
        if offline && self.online_candidate.as_deref() != Some(barcode) {
            return Err(refuse("OFFLINE: 介质已离线"));
        }
        Ok(())
    }

    /// 保留 assignment，先安全归槽本实例已访问卷，再禁止后续调度。
    /// 不抢占另一个控制器留在驱动中的介质，不改带上 GPFS/HSM 元数据。
    pub fn tape_offline(&mut self, barcode: &str, message: &str) -> Result<()> {
        if message.is_empty()
            || message.len() > 63
            || !message
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_- ".contains(&c))
        {
            return Err(refuse(
                "离线消息须为1–63个ASCII字母、数字、下划线、横线或空格",
            ));
        }
        let assigned: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE barcode=?1)",
            [barcode],
            |row| row.get(0),
        )?;
        if !assigned {
            return Err(refuse("NEED_ASSIGN"));
        }
        let (map, inv) = self.inventory()?;
        if let Some(entry) = inv.iter().find(|e| {
            e.element_type == ElementType::DataTransfer
                && e.full
                && e.volume_tag.as_deref() == Some(barcode)
        }) {
            let idx = self
                .drives
                .iter()
                .position(|d| map.dt_start + d.offset == entry.address)
                .ok_or_else(|| refuse("介质位于未配置驱动器"))?;
            if !self.claimed_drives[idx] {
                return Err(refuse("介质由其他执行实例装载；不能隐式抢占归槽"));
            }
            self.return_drive(idx)?;
            self.sessions[idx] = None;
        }
        self.db.execute("INSERT INTO library_tape_options VALUES(?1,?2) ON CONFLICT(barcode) DO UPDATE SET offline_msg=excluded.offline_msg", params![barcode,message])?;
        Ok(())
    }

    /// 真正读取 LTFS 索引验证可挂载后才恢复调度；失败保留离线标记。
    pub fn tape_online(&mut self, barcode: &str) -> Result<()> {
        let pool: Option<String> = self
            .db
            .query_row(
                "SELECT pool FROM library_volumes WHERE barcode=?1",
                [barcode],
                |row| row.get(0),
            )
            .optional()?;
        let pool = pool.ok_or_else(|| refuse("NEED_ASSIGN"))?;
        let offline: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_tape_options WHERE barcode=?1)",
            [barcode],
            |row| row.get(0),
        )?;
        if !offline {
            return Err(refuse("介质当前不是离线状态"));
        }
        self.online_candidate = Some(barcode.to_owned());
        let validated = self.inspect_volume(&format!("/{pool}/{barcode}"), |_, _| Ok(()));
        self.online_candidate = None;
        validated?;
        self.db.execute(
            "DELETE FROM library_tape_options WHERE barcode=?1",
            [barcode],
        )?;
        Ok(())
    }

    pub(super) fn drive_enabled(
        &self,
        idx: usize,
        map: &ElementAddressMap,
        inv: &[ElementStatus],
    ) -> Result<bool> {
        let entry = inv
            .iter()
            .find(|e| {
                e.element_type == ElementType::DataTransfer
                    && e.address == map.dt_start + self.drives[idx].offset
            })
            .ok_or_else(|| refuse("库存缺少配置驱动器"))?;
        let mut statement = self
            .db
            .prepare("SELECT serial FROM library_drive_options WHERE enabled=0")?;
        let disabled = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(!disabled.iter().any(|serial| {
            entry
                .drive_id
                .as_deref()
                .is_some_and(|id| drive_id_matches(id, serial))
        }))
    }

    /// 只改变调度资格，不是设备断电或重置。禁用前安全归槽本实例介质。
    pub fn drive_set_enabled(&mut self, serial: &str, enabled: bool) -> Result<()> {
        let (map, inv) = self.inventory()?;
        let entry = inv
            .iter()
            .find(|e| {
                e.element_type == ElementType::DataTransfer
                    && e.drive_id
                        .as_deref()
                        .is_some_and(|id| drive_id_matches(id, serial))
            })
            .ok_or_else(|| refuse("未知驱动器序列号"))?;
        let idx = self
            .drives
            .iter()
            .position(|d| map.dt_start + d.offset == entry.address)
            .ok_or_else(|| refuse("驱动器未配置给本实例"))?;
        self.ensure_drive_policy_change(idx, entry.full)?;
        if !enabled && entry.full {
            if !self.claimed_drives[idx] {
                return Err(refuse("驱动器含其他实例装载的介质；不能隐式抢占"));
            }
            self.return_drive(idx)?;
            self.sessions[idx] = None;
        }
        self.db.execute("INSERT INTO library_drive_options VALUES(?1,?2) ON CONFLICT(serial) DO UPDATE SET enabled=excluded.enabled", params![serial,enabled])?;
        Ok(())
    }

    pub(super) fn enforce_mount_limit(&mut self, barcode: &str) -> Result<()> {
        let pool: String = self.db.query_row(
            "SELECT pool FROM library_volumes WHERE barcode=?1",
            [barcode],
            |row| row.get(0),
        )?;
        let limit = self.pool_mount_limit(&pool)?;
        if limit == 0 {
            return Ok(());
        }
        let (map, inv) = self.inventory()?;
        // 先核验目标位置，再为挂载额度驱逐本实例其他卷。
        let target = inv
            .iter()
            .find(|entry| entry.full && entry.volume_tag.as_deref() == Some(barcode))
            .ok_or_else(|| refuse("目标介质不在库存"))?;
        let home: u16 = self.db.query_row(
            "SELECT home FROM library_volumes WHERE barcode=?1",
            [barcode],
            |row| row.get(0),
        )?;
        if target.element_type != ElementType::DataTransfer && target.address != home {
            return Err(refuse("目标介质不在已确认 home slot"));
        }
        let mut mounted = Vec::new();
        for entry in inv
            .iter()
            .filter(|e| e.element_type == ElementType::DataTransfer && e.full)
        {
            let Some(other) = entry.volume_tag.as_deref() else {
                continue;
            };
            if other == barcode {
                return Ok(());
            }
            let in_pool: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE barcode=?1 AND pool=?2)",
                params![other, pool],
                |row| row.get(0),
            )?;
            if in_pool {
                mounted.push(entry.address);
            }
        }
        while mounted.len() >= limit {
            let address = mounted.pop().unwrap();
            let idx = self
                .drives
                .iter()
                .position(|drive| map.dt_start + drive.offset == address)
                .ok_or_else(|| refuse("池挂载上限已用尽，介质位于未配置驱动器"))?;
            if !self.claimed_drives[idx] {
                return Err(refuse("池挂载上限已用尽，不能驱逐其他实例介质"));
            }
            self.return_drive(idx)?;
            self.sessions[idx] = None;
        }
        Ok(())
    }
}
