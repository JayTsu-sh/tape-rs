//! 显式介质搬运及本地 LTFS 索引维护；不实现 GPFS/HSM 对账。
use super::*;

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_volume_indexes (
            barcode TEXT PRIMARY KEY, uuid TEXT NOT NULL, generation TEXT NOT NULL,
            index_xml TEXT NOT NULL, observed_at TEXT NOT NULL);",
    )?;
    Ok(())
}

impl<C: TapeTransport, D: TapeTransport> Library<C, D> {
    fn maintenance_path(&self, barcode: &str) -> Result<String> {
        component(barcode)?;
        let pool: Option<String> = self
            .db
            .query_row(
                "SELECT pool FROM library_volumes WHERE barcode=?1",
                [barcode],
                |r| r.get(0),
            )
            .optional()?;
        Ok(format!(
            "/{}/{barcode}",
            pool.ok_or_else(|| refuse("NEED_ASSIGN"))?
        ))
    }

    /// 检查真实卷索引；可选读取每个普通文件，并验证索引内已有的内容哈希。
    /// 无哈希只证明可读，不宣称 checksum 已验证；链接只检查索引记录。
    pub fn tape_validate(&mut self, barcode: &str, full_read: bool) -> Result<Value> {
        let path = self.maintenance_path(barcode)?;
        // 独立任务连接避免在可变借用 Library 的挂载回调中再次借用 self。
        let task = self
            .task
            .as_ref()
            .map(|(_, id)| -> Result<_> {
                let state = self.db.path().ok_or_else(|| refuse("任务需要文件资源库"))?;
                Ok((tasks::TaskStore::open(Path::new(state))?, *id))
            })
            .transpose()?;
        let cancelled = std::cell::Cell::new(false);
        let result = self.inspect_maintenance_volume(&path, |volume, _| {
            let mut entries = Vec::new();
            volume.index().walk_files(|path, file| {
                entries.push((path.to_owned(), file.length, file.symlink.is_some()));
            });
            let mut files = Vec::new();
            let count = entries.len().max(1);
            for (position, (path, length, symlink)) in entries.into_iter().enumerate() {
                if let Some((store, id)) = &task
                    && !store.checkpoint(*id, 50 + (position.saturating_mul(40) / count) as u8,
                        &format!("校验文件前安全检查: {path}"), None)? {
                    cancelled.set(true);
                    return Err(refuse("任务已在文件读取前取消；已发生的副作用未回滚"));
                }
                let checksum = if symlink {
                    "not_applicable_symlink"
                } else if !full_read {
                    "not_verified_index_only"
                } else {
                    use crate::ltfs::volume::HashVerdict;
                    match volume.verify_file(&path)? {
                        HashVerdict::Match { algo } => {
                            if algo == "sha256sum" { "verified_sha256sum" } else { "verified_md5sum" }
                        }
                        HashVerdict::NoHash => {
                            let read = volume.read_file_to_writer(&path, &mut std::io::sink())?;
                            if read != length {
                                return Err(TapeError::Ltfs(format!("文件长度不符: {path}, expected={length}, actual={read}")));
                            }
                            "not_verified_no_hash"
                        }
                        HashVerdict::Mismatch { algo, expected, actual } => {
                            return Err(TapeError::Ltfs(format!("checksum 不符: {path}, {algo}, expected={expected}, actual={actual}")));
                        }
                    }
                };
                files.push(json!({"path":path,"length":length,"read":full_read && !symlink,"checksum":checksum}));
            }
            Ok(json!({"barcode":barcode,"uuid":volume.index().volume_uuid.to_string(),
                "generation":volume.index().generation.to_string(),"full_read":full_read,
                "index_valid":true,"writable":volume.writable(),"restricted_reason":volume.restricted_reason(),
                "files":files,"scope":"local_ltfs_validation"}))
        });
        if cancelled.get() {
            self.task_cancelled.set(true);
        }
        result
    }

    /// 从实际挂载卷更新本地 SQLite 完整索引视图，成功后单事务替换。
    /// 不创建 GPFS snapshot、不恢复 HSM stub，也不重建 EE 对象身份。
    /// 无法保真的未知 XML 会拒绝替换已有缓存。
    pub fn tape_reconcile(&mut self, barcode: &str) -> Result<Value> {
        let path = self.maintenance_path(barcode)?;
        let (uuid, generation, xml) = self.inspect_maintenance_volume(&path, |volume, _| {
            if !volume.index().unknown_elements.is_empty() {
                return Err(refuse(format!(
                    "索引含无法保真缓存的未知 XML 元素: {}",
                    volume.index().unknown_elements.join(", ")
                )));
            }
            let mut snapshot = volume.index().clone();
            // 缓存可查询的完整视图，不把已重放的目录冒充原始增量 XML。
            snapshot.incremental = false;
            snapshot.previous_incremental_location = None;
            let xml = String::from_utf8(snapshot.to_xml()?.to_vec())
                .map_err(|e| TapeError::Ltfs(format!("索引 XML 不是 UTF-8: {e}")))?;
            Ok((
                snapshot.volume_uuid.to_string(),
                snapshot.generation.to_string(),
                xml,
            ))
        })?;
        self.check_task(75, "发布本地索引缓存前检查")?;
        let observed_at = crate::ltfs::label::ltfs_time_now();
        let tx = self.db.unchecked_transaction()?;
        tx.execute("INSERT INTO library_volume_indexes(barcode,uuid,generation,index_xml,observed_at)
            VALUES(?1,?2,?3,?4,?5) ON CONFLICT(barcode) DO UPDATE SET
            uuid=excluded.uuid,generation=excluded.generation,index_xml=excluded.index_xml,observed_at=excluded.observed_at",
            params![barcode, uuid, generation, xml, observed_at])?;
        tx.commit()?;
        Ok(
            json!({"barcode":barcode,"uuid":uuid,"generation":generation,
            "index_xml":xml,"observed_at":observed_at,"source":"local_index_cache"}),
        )
    }

    /// 只读本地缓存；None 表示尚无成功对账，不触发装带。
    pub fn tape_index(&self, barcode: &str) -> Result<Option<Value>> {
        self.maintenance_path(barcode)?;
        Ok(self.db.query_row(
            "SELECT uuid,generation,index_xml,observed_at FROM library_volume_indexes WHERE barcode=?1",
            [barcode], |r| Ok(json!({"barcode":barcode,"uuid":r.get::<_, String>(0)?,
                "generation":r.get::<_, String>(1)?,"index_xml":r.get::<_, String>(2)?,
                "observed_at":r.get::<_, String>(3)?,"source":"local_index_cache"})),
        ).optional()?)
    }

    /// 只搬运已分配介质。I/E 导出要求先 offline；不抢占外部装载卷。
    /// 往返保留 assignment、offline 标记及带上内容。
    /// 不是 EE export（解除池归属及 purge GPFS 引用）或 import（创建 HSM stub）。
    pub fn tape_move(&mut self, barcode: &str, location: &str) -> Result<Value> {
        let result = self.move_managed_tape(barcode, location);
        if let Err(error) = &result {
            if reservation::ownership_lost(error) {
                self.stopped = true;
                self.sessions.iter_mut().for_each(|session| *session = None);
            }
            if let Err(record) = self.phase(barcode, "Failed", Some(&error.to_string())) {
                log::error!("保存搬带失败原因失败: {record}");
            }
        }
        result
    }

    fn move_managed_tape(&mut self, barcode: &str, location: &str) -> Result<Value> {
        self.task_outcome(
            0,
            "尚未开始搬带",
            json!({"effects":"not_started","barcode":barcode}),
        )?;
        self.check_task(5, "搬带预检")?;
        component(barcode)?;
        if !matches!(location, "ieslot" | "homeslot") {
            return Err(refuse("目标位置须为 ieslot 或 homeslot"));
        }
        if self.stopped {
            return Err(refuse("设备资格已失去，请关闭并重新核验现场"));
        }
        let row: Option<(u16, Option<String>)> = self
            .db
            .query_row(
                "SELECT home,blacklist FROM library_volumes WHERE barcode=?1",
                [barcode],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (home, blacklist) = row.ok_or_else(|| refuse("NEED_ASSIGN"))?;
        if let Some(reason) = blacklist {
            return Err(refuse(format!("BLACKLISTED: {reason}")));
        }
        let offline: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_tape_options WHERE barcode=?1)",
            [barcode],
            |r| r.get(0),
        )?;
        if location == "ieslot" && !offline {
            return Err(refuse("搬至 I/E 前须先 offline"));
        }
        if !offline {
            self.allowed(barcode)?;
        }
        let (map, inv) = self.inventory()?;
        let in_range = |address: u16, start: u16, count: u16| {
            address >= start && u32::from(address) < u32::from(start) + u32::from(count)
        };
        if !in_range(home, map.storage_start, map.storage_count) {
            return Err(refuse("已保存 home slot 与当前 element map 不符"));
        }
        let sources: Vec<_> = inv
            .iter()
            .filter(|e| e.full && e.volume_tag.as_deref() == Some(barcode))
            .collect();
        if sources.len() != 1 {
            return Err(refuse("目标条码在库存中缺失或重复"));
        }
        let source = sources[0];
        let drive = match source.element_type {
            ElementType::Storage if source.address == home => None,
            ElementType::ImportExport if in_range(source.address, map.ie_start, map.ie_count) => {
                None
            }
            ElementType::DataTransfer => {
                let idx = self
                    .drives
                    .iter()
                    .position(|d| map.dt_start.checked_add(d.offset) == Some(source.address))
                    .ok_or_else(|| refuse("介质位于未配置驱动器"))?;
                if !self.claimed_drives[idx] || !self.claimed_changer {
                    return Err(refuse("介质由其他执行实例装载；不能隐式抢占归槽"));
                }
                Some(idx)
            }
            _ => return Err(refuse("目标介质不在已确认 home slot、I/E 或本实例驱动器")),
        };
        let destination = if location == "homeslot" {
            home
        } else if source.element_type == ElementType::ImportExport {
            source.address
        } else {
            inv.iter()
                .filter(|e| {
                    e.element_type == ElementType::ImportExport
                        && !e.full
                        && in_range(e.address, map.ie_start, map.ie_count)
                })
                .map(|e| e.address)
                .min()
                .ok_or_else(|| refuse("没有空闲 I/E slot"))?
        };
        if source.address != destination
            && !inv.iter().any(|e| {
                e.address == destination
                    && !e.full
                    && e.element_type
                        == if location == "homeslot" {
                            ElementType::Storage
                        } else {
                            ElementType::ImportExport
                        }
            })
        {
            return Err(refuse("目标 slot 不存在或已被占用"));
        }
        // 驱动器必须通过既有正常卸载/归槽路径，任何归槽前提失败都不能先抢 changer。
        if drive.is_some()
            && !inv
                .iter()
                .any(|e| e.address == home && e.element_type == ElementType::Storage && !e.full)
        {
            return Err(refuse("home slot 不存在或被占用"));
        }
        if source.address == destination {
            self.task_outcome(90, "目标已在指定位置，无需搬带", json!({"effects":"not_started","barcode":barcode,"location":location,"address":destination,"moved":false}))?;
            return Ok(
                json!({"barcode":barcode,"location":location,"address":destination,"moved":false}),
            );
        }
        self.check_task(10, "搬带取得设备资格前检查")?;
        if self.claimed_changer {
            reservation::verify_holder(&self.changer, self.key)?;
        } else {
            self.fence(&self.changer)?;
            self.claimed_changer = true;
        }
        let source_address = if let Some(idx) = drive {
            self.check_task(20, "正常归槽前检查")?;
            self.task_outcome(20, "正常卸载归槽可能开始", json!({"effects":"may_started","barcode":barcode,"source":source.address,"destination":home}))?;
            self.return_drive(idx)?;
            self.sessions[idx] = None;
            self.task_outcome(50, "已核验正常归槽", json!({"effects":"completed","barcode":barcode,"location":"homeslot","address":home,"moved":true}))?;
            home
        } else {
            source.address
        };
        if source_address != destination {
            let (current_map, now) = self.inventory()?;
            if current_map.storage_start != map.storage_start
                || current_map.storage_count != map.storage_count
                || current_map.ie_start != map.ie_start
                || current_map.ie_count != map.ie_count
                || current_map.dt_start != map.dt_start
                || current_map.dt_count != map.dt_count
                || current_map.transport_start != map.transport_start
                || current_map.transport_count != map.transport_count
                || !now.iter().any(|e| {
                    e.address == source_address
                        && e.full
                        && e.volume_tag.as_deref() == Some(barcode)
                })
                || !now.iter().any(|e| {
                    e.address == destination
                        && !e.full
                        && e.element_type
                            == if location == "homeslot" {
                                ElementType::Storage
                            } else {
                                ElementType::ImportExport
                            }
                })
            {
                return Err(refuse("搬带前 element map 或库存已改变"));
            }
            self.check_task(60, "MOVE 前安全检查")?;
            self.phase(barcode, "Moving", None)?;
            reservation::verify_holder(&self.changer, self.key)?;
            let mut changer = MediumChanger::new(&self.changer);
            changer.load_address_map()?;
            self.task_outcome(65, "MOVE 可能开始", json!({"effects":"may_started","barcode":barcode,"source":source_address,"destination":destination}))?;
            changer.move_medium(source_address, destination)?;
            let (_, after) = self.inventory()?;
            if !after.iter().any(|e| {
                e.address == destination && e.full && e.volume_tag.as_deref() == Some(barcode)
            }) {
                return Err(TapeError::InconsistentState(format!(
                    "MOVE 后目标条码不符: barcode={barcode}, source={source_address}, destination={destination}"
                )));
            }
        }
        self.phase(barcode, "Assigned", None)?;
        let outcome = json!({"effects":"completed","barcode":barcode,"location":location,"address":destination,"moved":true});
        self.task_outcome(90, "已核验搬带后条码", outcome.clone())?;
        Ok(outcome)
    }
}
