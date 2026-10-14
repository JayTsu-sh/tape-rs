//! 单进程逻辑库：显式 assignment、资源状态和按卷访问的机械调度。
//! 调用者独占配置的设备；不与 ltfsd 或 LE 同时控制同一逻辑库。
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::changer::commands::MediumChanger;
use crate::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use crate::error::{Result, TapeError};
use crate::ltfs::index::{LtfsIndex, MetadataChange};
use crate::ltfs::volume::{LtfsVolume, VolumeSession};
use crate::scsi::inquiry::read_unit_serial;
use crate::scsi::reservation::{self, FenceOutcome, ReservationKey};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

/// IBM DVCID 可能含 vendor/product；只匹配完整 ASCII 分量，不用任意子串。
pub(crate) fn drive_id_matches(id: &str, serial: &str) -> bool {
    !serial.is_empty() && (id == serial || id.split_ascii_whitespace().any(|part| part == serial))
}

type VolumeStatusRow = (
    String,
    u16,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

fn refuse(reason: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: reason.into(),
    }
}
fn component(s: &str) -> Result<()> {
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\0']) {
        return Err(refuse("逻辑库名称必须是单一路径分量"));
    }
    Ok(())
}

/// 确切的 DTE 偏移与 transport；创建时以库存 DVCID/VPD 核对。
pub struct LibraryDrive<D> {
    pub offset: u16,
    pub device: D,
}

/// 资源库与设备绑定；所有调用由 &mut self 串行化。
pub struct Library<C, D> {
    changer: C,
    drives: Vec<LibraryDrive<D>>,
    db: Connection,
    key: ReservationKey,
    claimed_changer: bool,
    claimed_drives: Vec<bool>,
    sessions: Vec<Option<(String, VolumeSession)>>,
    stopped: bool,
    validated: bool,
    serial: String,
    _lock: nix::fcntl::Flock<std::fs::File>,
}

impl<C: TapeTransport, D: TapeTransport> Library<C, D> {
    /// 打开资源库，仅查询设备身份/库存；不抢占、装带或写介质。
    pub fn open(
        changer: C,
        drives: Vec<LibraryDrive<D>>,
        state: &Path,
        key: ReservationKey,
    ) -> Result<Self> {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.with_extension("library-lock"))?;
        let lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| TapeError::Nix(e))?;
        let db = Connection::open(state)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS library_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), serial TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS library_observations (singleton INTEGER PRIMARY KEY CHECK(singleton=1), data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS library_volumes (barcode TEXT PRIMARY KEY, pool TEXT NOT NULL,
            home INTEGER NOT NULL, blacklist TEXT, phase TEXT NOT NULL DEFAULT 'Assigned', failure TEXT, last_failure TEXT);")?;
        let columns: Vec<String> = db
            .prepare("PRAGMA table_info(library_volumes)")?
            .query_map([], |r| r.get(1))?
            .collect::<std::result::Result<_, _>>()?;
        if !columns.iter().any(|c| c == "last_failure") {
            db.execute(
                "ALTER TABLE library_volumes ADD COLUMN last_failure TEXT",
                [],
            )?;
        }
        let n = drives.len();
        let mut this = Self {
            changer,
            drives,
            db,
            key,
            claimed_changer: false,
            claimed_drives: vec![false; n],
            sessions: (0..n).map(|_| None).collect(),
            stopped: false,
            validated: false,
            serial: String::new(),
            _lock: lock,
        };
        let serial =
            read_unit_serial(&this.changer).ok_or_else(|| refuse("无法核验 changer 序列号"))?;
        let stored: Option<String> = this
            .db
            .query_row(
                "SELECT serial FROM library_identity WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if stored.as_ref().is_some_and(|s| s != &serial) {
            return Err(refuse("资源库属于其他 changer"));
        }
        let (map, inventory) = this.inventory()?;
        let mut seen = BTreeSet::new();
        let mut serials = BTreeSet::new();
        for drive in &this.drives {
            if drive.offset >= map.dt_count || !seen.insert(drive.offset) {
                return Err(refuse("驱动器偏移越界或重复"));
            }
            let actual =
                read_unit_serial(&drive.device).ok_or_else(|| refuse("无法核验驱动器序列号"))?;
            let entry = inventory
                .iter()
                .find(|e| {
                    e.address == map.dt_start + drive.offset
                        && e.element_type == ElementType::DataTransfer
                })
                .ok_or_else(|| refuse("库存缺少配置驱动器"))?;
            if !entry
                .drive_id
                .as_deref()
                .is_some_and(|id| drive_id_matches(id, &actual))
                || !serials.insert(actual)
            {
                return Err(refuse("DTE 与驱动器序列号不匹配或重复"));
            }
        }
        this.db.execute(
            "INSERT OR IGNORE INTO library_identity VALUES(1,?1)",
            [&serial],
        )?;
        this.serial = serial;
        this.validated = true;
        this.remember_inventory(&map, &inventory)?;
        Ok(this)
    }

    fn inventory(&self) -> Result<(ElementAddressMap, Vec<ElementStatus>)> {
        let mut changer = MediumChanger::new(&self.changer);
        let map = changer.load_address_map()?.clone();
        for (start, count) in [
            (map.transport_start, map.transport_count),
            (map.storage_start, map.storage_count),
            (map.ie_start, map.ie_count),
            (map.dt_start, map.dt_count),
        ] {
            if u32::from(start) + u32::from(count) > 65536 {
                return Err(TapeError::InconsistentState(
                    "element map 地址范围溢出".into(),
                ));
            }
        }
        let inventory = changer.read_all_status()?;
        if self.validated {
            self.remember_inventory(&map, &inventory)?;
        }
        Ok((map, inventory))
    }

    /// assignment 必须引用真实条码和已知 home storage；不装带。
    pub fn assign(&mut self, pool: &str, barcode: &str) -> Result<()> {
        component(pool)?;
        component(barcode)?;
        let (map, inventory) = self.inventory()?;
        let e = inventory
            .iter()
            .find(|e| e.full && e.volume_tag.as_deref() == Some(barcode))
            .ok_or_else(|| refuse("库存中不存在此条码"))?;
        let home = if e.element_type == ElementType::Storage {
            e.address
        } else {
            e.source_address
                .ok_or_else(|| refuse("无法确认介质 home slot"))?
        };
        if home < map.storage_start
            || u32::from(home) >= u32::from(map.storage_start) + u32::from(map.storage_count)
        {
            return Err(refuse("home slot 不属于 storage 地址空间"));
        }
        self.db.execute(
            "INSERT INTO library_volumes(barcode,pool,home) VALUES(?1,?2,?3)",
            params![barcode, pool, home],
        )?;
        Ok(())
    }

    pub fn unassign(&mut self, barcode: &str) -> Result<()> {
        let (_, inv) = self.inventory()?;
        if inv.iter().any(|e| {
            e.element_type == ElementType::DataTransfer
                && e.full
                && e.volume_tag.as_deref() == Some(barcode)
        }) {
            return Err(refuse("先显式归槽再取消 assignment"));
        }
        self.db
            .execute("DELETE FROM library_volumes WHERE barcode=?1", [barcode])?;
        Ok(())
    }

    pub fn blacklist(&mut self, barcode: &str, reason: Option<&str>) -> Result<()> {
        if reason.is_some_and(|s| s.trim().is_empty()) {
            return Err(refuse("blacklist 原因不能为空"));
        }
        if self.db.execute(
            "UPDATE library_volumes SET blacklist=?2 WHERE barcode=?1",
            params![barcode, reason],
        )? == 0
        {
            return Err(refuse("卷尚未 assignment"));
        }
        Ok(())
    }

    /// 包含 NEED_ASSIGN / BLACKLISTED / 设备资源和最近失败原因。
    pub fn status(&self) -> Result<Value> {
        let (map, inv) = self.inventory()?;
        let mut status = self.status_from(map, inv)?;
        status["source"] = json!("device_query");
        Ok(status)
    }
    fn remember_inventory(&self, map: &ElementAddressMap, inv: &[ElementStatus]) -> Result<()> {
        let mut status = self.status_from(map.clone(), inv.to_vec())?;
        status["storage_address_start"] = json!(map.storage_start);
        status["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        self.db.execute(
            "INSERT OR REPLACE INTO library_observations VALUES(1,?1)",
            [status.to_string()],
        )?;
        Ok(())
    }
    fn status_from(&self, map: ElementAddressMap, inv: Vec<ElementStatus>) -> Result<Value> {
        let mut volumes = Vec::new();
        for e in inv.iter().filter(|e| e.full && e.volume_tag.is_some()) {
            let barcode = e.volume_tag.as_deref().unwrap();
            let row: Option<VolumeStatusRow> = self.db.query_row(
                "SELECT pool,home,blacklist,phase,failure,last_failure FROM library_volumes WHERE barcode=?1", [barcode],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            volumes.push(match row {
                Some((pool,home,blacklist,phase,failure,last_failure)) => json!({"barcode":barcode,"pool":pool,"home_slot":home.checked_sub(map.storage_start).filter(|n|*n<map.storage_count).map(|n|u32::from(n)+1),
                    "address":e.address,"state":if blacklist.is_some(){"BLACKLISTED"}else{&phase},"blacklist":blacklist,"failure":failure,"last_failure":last_failure}),
                None => json!({"barcode":barcode,"address":e.address,"state":"NEED_ASSIGN"}),
            });
        }
        let drives: Vec<Value> = inv.iter().filter(|e| e.element_type == ElementType::DataTransfer).map(|e| json!({
            "offset":e.address-map.dt_start,"serial":e.drive_id,"barcode":e.volume_tag,"occupied":e.full,
            "configured":self.drives.iter().any(|d| map.dt_start+d.offset==e.address)})).collect();
        Ok(
            json!({"node":if self.stopped {"OwnershipLost"} else {"Available"},"library_serial":self.serial,"volumes":volumes,"drives":drives}),
        )
    }

    pub fn pools(&self) -> Result<Vec<String>> {
        let mut s = self
            .db
            .prepare("SELECT DISTINCT pool FROM library_volumes ORDER BY pool")?;
        Ok(s.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn volumes(&self, pool: &str) -> Result<Vec<String>> {
        let mut s = self
            .db
            .prepare("SELECT barcode FROM library_volumes WHERE pool=?1 ORDER BY barcode")?;
        Ok(s.query_map([pool], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn resolve<'a>(&self, path: &'a str) -> Result<(&'a str, &'a str)> {
        let path = path.trim_start_matches('/');
        let (pool, rest) = path
            .split_once('/')
            .ok_or_else(|| refuse("路径应为 /池名/条码/卷内路径"))?;
        let (barcode, relative) = rest.split_once('/').unwrap_or((rest, ""));
        component(pool)?;
        component(barcode)?;
        if relative
            .split('/')
            .any(|p| p == "." || p == ".." || p.contains('\0'))
        {
            return Err(refuse("卷内路径非法"));
        }
        let allowed: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_volumes WHERE barcode=?1 AND pool=?2)",
            params![barcode, pool],
            |r| r.get(0),
        )?;
        if !allowed {
            return Err(refuse("卷不属于指定池"));
        }
        Ok((barcode, relative))
    }
    fn phase(&self, barcode: &str, phase: &str, failure: Option<&str>) -> Result<()> {
        self.db.execute(
            "UPDATE library_volumes SET last_failure=CASE WHEN ?2='Failed' OR ?2='NON_SUPPORTED_TAPE' THEN phase || ': ' || ?3 ELSE last_failure END,failure=CASE WHEN ?2='Failed' OR ?2='NON_SUPPORTED_TAPE' THEN phase || ': ' || ?3 ELSE ?3 END,phase=?2 WHERE barcode=?1",
            params![barcode, phase, failure],
        )?;
        if self.stopped {
            self.db.execute("UPDATE library_observations SET data=json_set(data,'$.node','OwnershipLost') WHERE singleton=1",[])?;
        }
        log::info!("卷 {} 调度阶段 {}{}", barcode, phase, failure.unwrap_or(""));
        Ok(())
    }
    fn fence(&self, dev: &dyn TapeTransport) -> Result<()> {
        match reservation::fence(dev, self.key)? {
            FenceOutcome::Fenced { .. } => Ok(()),
            other => Err(refuse(format!("未取得设备独占资格: {other:?}"))),
        }
    }
    fn allowed(&self, barcode: &str) -> Result<u16> {
        let row: Option<(u16, Option<String>)> = self
            .db
            .query_row(
                "SELECT home,blacklist FROM library_volumes WHERE barcode=?1",
                [barcode],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((home, None)) => Ok(home),
            Some((_, Some(reason))) => Err(refuse(format!("BLACKLISTED: {reason}"))),
            None => Err(refuse("NEED_ASSIGN")),
        }
    }

    /// reservation → MOVE → LOAD → ready；只驱逐本资源库已分配且未 blacklist 的卷。
    fn select(&mut self, barcode: &str) -> Result<usize> {
        if self.stopped {
            return Err(refuse("设备资格已失去，请关闭并重新核验现场"));
        }
        let home = self.allowed(barcode)?;
        let (map, inv) = self.inventory()?;
        if home < map.storage_start
            || u32::from(home) >= u32::from(map.storage_start) + u32::from(map.storage_count)
        {
            return Err(refuse("已保存 home slot 与当前 element map 不符"));
        }
        let source = inv
            .iter()
            .find(|e| e.full && e.volume_tag.as_deref() == Some(barcode))
            .ok_or_else(|| refuse("目标介质不在库存"))?;
        let configured = |i: usize| {
            inv.iter().find(|e| {
                e.element_type == ElementType::DataTransfer
                    && e.address == map.dt_start + self.drives[i].offset
            })
        };
        let chosen = (0..self.drives.len())
            .find(|&i| configured(i).is_some_and(|e| e.volume_tag.as_deref() == Some(barcode)))
            .or_else(|| (0..self.drives.len()).find(|&i| configured(i).is_some_and(|e| !e.full)))
            .or_else(|| {
                (0..self.drives.len()).find(|&i| {
                    configured(i)
                        .and_then(|e| e.volume_tag.as_deref())
                        .is_some_and(|b| self.allowed(b).is_ok())
                })
            })
            .ok_or_else(|| refuse("没有可调度驱动器（不会驱逐未分配介质）"))?;
        if source.element_type == ElementType::DataTransfer
            && source.address != map.dt_start + self.drives[chosen].offset
        {
            return Err(refuse("目标介质位于未配置驱动器"));
        }
        if source.element_type != ElementType::DataTransfer && source.address != home {
            return Err(refuse("目标介质不在已确认 home slot"));
        }
        self.phase(barcode, "Reserving", None)?;
        if self.claimed_changer {
            reservation::verify_holder(&self.changer, self.key)?;
        } else {
            self.fence(&self.changer)?;
            self.claimed_changer = true;
        }
        if self.claimed_drives[chosen] {
            reservation::verify_holder(&self.drives[chosen].device, self.key)?;
        } else {
            self.fence(&self.drives[chosen].device)?;
            self.claimed_drives[chosen] = true;
        }
        let current = configured(chosen).ok_or_else(|| refuse("库存缺少配置 DTE"))?;
        if current.full && current.volume_tag.as_deref() != Some(barcode) {
            self.sessions[chosen] = None;
            self.return_drive(chosen)?;
        }
        // 重新确认库存：禁止根据旧位置盲目 MOVE。
        let (_, now) = self.inventory()?;
        let dest = map.dt_start + self.drives[chosen].offset;
        let mut moved = false;
        if !now
            .iter()
            .any(|e| e.address == dest && e.full && e.volume_tag.as_deref() == Some(barcode))
        {
            if !now
                .iter()
                .any(|e| e.address == home && e.full && e.volume_tag.as_deref() == Some(barcode))
                || now.iter().any(|e| e.address == dest && e.full)
            {
                return Err(refuse("搬带前库存已改变"));
            }
            self.phase(barcode, "Moving", None)?;
            reservation::verify_holder(&self.changer, self.key)?;
            reservation::verify_holder(&self.drives[chosen].device, self.key)?;
            let mut changer = MediumChanger::new(&self.changer);
            changer.load_address_map()?;
            changer.move_medium(home, dest)?;
            moved = true;
            let (_, after) = self.inventory()?;
            if !after
                .iter()
                .any(|e| e.address == dest && e.full && e.volume_tag.as_deref() == Some(barcode))
            {
                return Err(TapeError::InconsistentState("MOVE 后目标条码不符".into()));
            }
        }
        self.phase(barcode, "Loading", None)?;
        let drive = TapeDrive::new(&self.drives[chosen].device);
        reservation::verify_holder(&self.drives[chosen].device, self.key)?;
        if moved || !drive.test_unit_ready()? {
            drive.load()?;
        }
        self.phase(barcode, "WaitingReady", None)?;
        let deadline = Instant::now() + Duration::from_secs(7200);
        while !drive.test_unit_ready()? {
            reservation::verify_holder(&self.drives[chosen].device, self.key)?;
            if Instant::now() >= deadline {
                return Err(TapeError::NotReady("等待装带就绪超时".into()));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(chosen)
    }

    fn return_drive(&self, idx: usize) -> Result<()> {
        let (map, inv) = self.inventory()?;
        let address = map.dt_start + self.drives[idx].offset;
        let entry = inv
            .iter()
            .find(|e| e.address == address && e.element_type == ElementType::DataTransfer)
            .ok_or_else(|| refuse("DTE 不存在"))?;
        if !entry.full {
            return Ok(());
        }
        let barcode = entry
            .volume_tag
            .as_deref()
            .ok_or_else(|| refuse("驱动器内介质条码未知"))?;
        let home: u16 = self.db.query_row(
            "SELECT home FROM library_volumes WHERE barcode=?1",
            [barcode],
            |r| r.get(0),
        )?;
        if !inv
            .iter()
            .any(|e| e.address == home && e.element_type == ElementType::Storage && !e.full)
        {
            return Err(refuse("home slot 被占用"));
        }
        reservation::verify_holder(&self.changer, self.key)?;
        reservation::verify_holder(&self.drives[idx].device, self.key)?;
        self.phase(barcode, "Unloading", None)?;
        TapeDrive::new(&self.drives[idx].device).unload()?;
        reservation::verify_holder(&self.changer, self.key)?;
        reservation::verify_holder(&self.drives[idx].device, self.key)?;
        let mut changer = MediumChanger::new(&self.changer);
        changer.load_address_map()?;
        changer.move_medium(address, home)?;
        let (_, after) = self.inventory()?;
        if !after
            .iter()
            .any(|e| e.address == home && e.full && e.volume_tag.as_deref() == Some(barcode))
        {
            return Err(TapeError::InconsistentState("归槽后条码不符".into()));
        }
        self.phase(barcode, "Assigned", None)
    }

    /// 只读卷检查/读取；回调只能获得不可变卷引用。
    pub fn inspect_volume<T>(
        &mut self,
        path: &str,
        f: impl FnOnce(&LtfsVolume<'_>, &str) -> Result<T>,
    ) -> Result<T> {
        self.volume_operation(path, false, |v, p| f(v, p))
    }
    /// 显式卷更新；完整 commit/checkpoint 后才报告成功。
    pub fn with_volume<T>(
        &mut self,
        path: &str,
        f: impl FnOnce(&mut LtfsVolume<'_>, &str) -> Result<T>,
    ) -> Result<T> {
        self.volume_operation(path, true, f)
    }
    fn volume_operation<T>(
        &mut self,
        path: &str,
        write: bool,
        f: impl FnOnce(&mut LtfsVolume<'_>, &str) -> Result<T>,
    ) -> Result<T> {
        let (barcode, relative) = self.resolve(path)?;
        let mut recovering = false;
        let result: Result<T> = (|| {
            let idx = self.select(barcode)?;
            self.phase(barcode, "Recovering", None)?;
            recovering = true;
            reservation::verify_holder(&self.drives[idx].device, self.key)?;
            let mut vol = match self.sessions[idx].take().filter(|(b, _)| b == barcode) {
                Some((_, session)) => match session.resume(&self.drives[idx].device, self.key) {
                    Ok(v) => v,
                    Err(TapeError::NotReady(_)) => LtfsVolume::mount(&self.drives[idx].device)?,
                    Err(e) => return Err(e),
                },
                None => LtfsVolume::mount(&self.drives[idx].device)?,
            };
            vol.set_reservation_guard(Some(self.key));
            self.phase(
                barcode,
                if vol.writable() {
                    "Mounted"
                } else {
                    "ReadOnly"
                },
                vol.restricted_reason(),
            )?;
            recovering = false;
            let value = f(&mut vol, relative)?;
            if write {
                vol.commit()?;
            }
            self.sessions[idx] = vol
                .into_session()
                .map(|session| (barcode.to_owned(), session));
            reservation::verify_holder(&self.drives[idx].device, self.key)?;
            Ok(value)
        })();
        if let Err(e) = &result {
            if reservation::ownership_lost(e) {
                self.stopped = true;
                for s in &mut self.sessions {
                    *s = None;
                }
            }
            let phase = if recovering
                && matches!(
                    e,
                    TapeError::Ltfs(_)
                        | TapeError::Xml(_)
                        | TapeError::XmlSource(_)
                        | TapeError::ScsiCommand {
                            sense_key: 0x08,
                            ..
                        }
                ) {
                "NON_SUPPORTED_TAPE"
            } else {
                "Failed"
            };
            if let Err(record) = self.phase(barcode, phase, Some(&e.to_string())) {
                log::error!("保存失败原因失败: {record}");
            }
        }
        result
    }
    pub fn read_to<W: Write>(&mut self, path: &str, out: &mut W) -> Result<u64> {
        self.inspect_volume(path, |v, p| v.read_file_to_writer(p, out))
    }
    pub fn write_from<R: Read>(
        &mut self,
        path: &str,
        input: &mut R,
        create_only: bool,
    ) -> Result<u64> {
        self.with_volume(path, |v, p| {
            if create_only && v.index().find_file(p).is_some() {
                return Err(refuse("目标文件已存在"));
            }
            v.append_file(p, input)
        })
    }
    /// 显式正常关闭：归槽本实例访问的配置驱动器、释放本轮 PR。
    pub fn close(&mut self) -> Result<()> {
        for i in 0..self.drives.len() {
            // 不主动抢占未被本实例取得的驱动器。
            if self.claimed_drives[i] {
                reservation::verify_holder(&self.drives[i].device, self.key)?;
                self.sessions[i] = None;
                self.return_drive(i)?;
                reservation::release_and_unregister(&self.drives[i].device, self.key)?;
                self.claimed_drives[i] = false;
            }
        }
        if self.claimed_changer {
            reservation::verify_holder(&self.changer, self.key)?;
            reservation::release_and_unregister(&self.changer, self.key)?;
            self.claimed_changer = false;
        }
        Ok(())
    }
}

/// 向现有 TapeFs 提供相同的逻辑库路径；锁覆盖整个机械/文件操作。
pub struct LibraryBackend<C, D>(pub std::sync::Arc<std::sync::Mutex<Library<C, D>>>);

fn errno(e: TapeError) -> i32 {
    match e {
        TapeError::Refused { reason } => match reason.as_str() {
            "目标文件已存在" | "目标已存在" | "属性已存在" => nix::libc::EEXIST,
            "属性不存在" => nix::libc::ENODATA,
            "文件不存在" | "路径不存在" | "卷不属于指定池" => nix::libc::ENOENT,
            _ => nix::libc::EPERM,
        },
        TapeError::RecoveryRestricted { .. } => nix::libc::EROFS,
        TapeError::Busy { .. } | TapeError::NotReady(_) => nix::libc::EBUSY,
        _ => nix::libc::EIO,
    }
}
fn file_stat(
    index: &LtfsIndex,
    barcode: &str,
    length: u64,
    kind: &str,
    meta: &crate::ltfs::index::NodeMeta,
    xattrs: &[crate::ltfs::index::Xattr],
    symlink: Option<&str>,
) -> crate::client::PathStat {
    crate::client::PathStat {
        state: "committed".into(),
        length,
        current: Some(crate::client::FileStat {
            length,
            generation: index.generation,
            round: 0,
            barcode: barcode.into(),
            sha256: xattrs
                .iter()
                .find(|x| x.key == crate::ltfs::index::XATTR_SHA256 && !x.base64)
                .map(|x| x.value.clone())
                .unwrap_or_default(),
            version: (meta.file_uid, index.generation),
            metadata: json!({"kind":kind,"symlink":symlink,"modify_time":meta.modify_time,"node":{
            "readonly":meta.readonly,"creation_time":meta.creation_time,"change_time":meta.change_time,"access_time":meta.access_time},
            "xattrs":xattrs.iter().map(|x|json!({"key":x.key,"value":x.value,"base64":x.base64})).collect::<Vec<_>>()}),
        }),
    }
}
impl<C: TapeTransport + Send, D: TapeTransport + Send> crate::tapefs::Backend
    for LibraryBackend<C, D>
{
    fn stat(&self, path: &str) -> crate::tapefs::Res<Option<crate::client::PathStat>> {
        let mut lib = self.0.lock().map_err(|_| nix::libc::EIO)?;
        let p = path.trim_matches('/');
        if p.is_empty() || lib.pools().map_err(errno)?.iter().any(|v| v == p) {
            return Ok(None);
        }
        let (bc, _) = lib.resolve(path).map_err(errno)?;
        let bc = bc.to_owned();
        lib.inspect_volume(path, |v, p| {
            let i = v.index();
            Ok(if let Some(f) = i.find_file(p) {
                Some(file_stat(
                    i,
                    &bc,
                    f.length,
                    if f.symlink.is_some() {
                        "symlink"
                    } else {
                        "file"
                    },
                    &f.meta,
                    &f.xattrs,
                    f.symlink.as_deref(),
                ))
            } else {
                i.find_directory(p)
                    .map(|d| file_stat(i, &bc, 0, "directory", &d.meta, &d.xattrs, None))
            })
        })
        .map_err(errno)
    }
    fn list_dir(&self, path: &str) -> crate::tapefs::Res<Option<Vec<crate::client::DirItem>>> {
        let mut lib = self.0.lock().map_err(|_| nix::libc::EIO)?;
        let p = path.trim_matches('/');
        let item =
            |name: String, is_dir: bool, is_symlink: bool, len: u64| crate::client::DirItem {
                name,
                is_dir,
                is_symlink: Some(is_symlink),
                state: if is_dir {
                    String::new()
                } else {
                    "committed".into()
                },
                committed_length: Some(len),
                staged_length: None,
            };
        if p.is_empty() {
            return Ok(Some(
                lib.pools()
                    .map_err(errno)?
                    .into_iter()
                    .map(|n| item(n, true, false, 0))
                    .collect(),
            ));
        }
        if !p.contains('/') {
            return Ok(if lib.pools().map_err(errno)?.iter().any(|v| v == p) {
                Some(
                    lib.volumes(p)
                        .map_err(errno)?
                        .into_iter()
                        .map(|n| item(n, true, false, 0))
                        .collect(),
                )
            } else {
                None
            });
        }
        lib.inspect_volume(path, |v, p| {
            Ok(v.index().find_directory(p).map(|d| {
                d.subdirs
                    .iter()
                    .map(|s| item(s.name.clone(), true, false, 0))
                    .chain(
                        d.files
                            .iter()
                            .map(|f| item(f.name.clone(), false, f.symlink.is_some(), f.length)),
                    )
                    .collect()
            }))
        })
        .map_err(errno)
    }
    fn fetch(&self, path: &str, out: &mut std::fs::File) -> crate::tapefs::Res<u64> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .read_to(path, out)
            .map_err(errno)
    }
    fn upload(
        &self,
        path: &str,
        local: &Path,
        create_only: bool,
        _wait: bool,
    ) -> crate::tapefs::Res<bool> {
        let mut input = std::fs::File::open(local).map_err(|_| nix::libc::EIO)?;
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .write_from(path, &mut input, create_only)
            .map_err(errno)?;
        Ok(true)
    }
    fn delete(&self, path: &str) -> crate::tapefs::Res<()> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .with_volume(path, |v, p| {
                if v.remove_file(p)? {
                    Ok(())
                } else {
                    Err(refuse("文件不存在"))
                }
            })
            .map_err(errno)
    }
    fn mkdir(&self, path: &str) -> crate::tapefs::Res<()> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .with_volume(path, |v, p| v.create_directory(p))
            .map_err(errno)
    }
    fn rmdir(&self, path: &str) -> crate::tapefs::Res<()> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .with_volume(path, |v, p| v.remove_directory(p))
            .map_err(errno)
    }
    fn symlink(&self, target: &str, path: &str) -> crate::tapefs::Res<()> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .with_volume(path, |v, p| v.add_symlink(p, target))
            .map_err(errno)
    }
    fn rename(&self, from: &str, to: &str, no_replace: bool) -> crate::tapefs::Res<()> {
        let mut lib = self.0.lock().map_err(|_| nix::libc::EIO)?;
        let (b1, _) = lib.resolve(from).map_err(errno)?;
        let (b2, p2) = lib.resolve(to).map_err(errno)?;
        if b1 != b2 {
            return Err(nix::libc::EXDEV);
        }
        let p2 = p2.to_owned();
        lib.with_volume(from, |v, p| {
            if no_replace
                && (v.index().find_file(&p2).is_some() || v.index().find_directory(&p2).is_some())
            {
                return Err(refuse("目标已存在"));
            }
            v.rename_path(p, &p2)
        })
        .map_err(errno)
    }
    fn change_metadata(&self, path: &str, change: &MetadataChange) -> crate::tapefs::Res<()> {
        self.0
            .lock()
            .map_err(|_| nix::libc::EIO)?
            .with_volume(path, |v, p| v.change_node_metadata(p, change))
            .map_err(errno)
    }
    fn change_xattr(
        &self,
        path: &str,
        name: &str,
        value: Option<&[u8]>,
        flags: u32,
    ) -> crate::tapefs::Res<()> {
        use base64::Engine;
        if flags & !(nix::libc::XATTR_CREATE as u32 | nix::libc::XATTR_REPLACE as u32) != 0
            || flags == (nix::libc::XATTR_CREATE as u32 | nix::libc::XATTR_REPLACE as u32)
        {
            return Err(nix::libc::EINVAL);
        }
        let key = name.strip_prefix("user.").ok_or(nix::libc::EOPNOTSUPP)?;
        let mut lib = self.0.lock().map_err(|_| nix::libc::EIO)?;
        lib.with_volume(path, |v, p| {
            let xs = if let Some(f) = v.index().find_file(p) {
                &f.xattrs
            } else {
                &v.index()
                    .find_directory(p)
                    .ok_or_else(|| refuse("路径不存在"))?
                    .xattrs
            };
            let exists = xs.iter().any(|x| x.key == key);
            if flags & nix::libc::XATTR_CREATE as u32 != 0 && exists {
                return Err(refuse("属性已存在"));
            }
            if (flags & nix::libc::XATTR_REPLACE as u32 != 0 || value.is_none()) && !exists {
                return Err(refuse("属性不存在"));
            }
            match value {
                Some(bytes) => v.set_node_xattr(
                    p,
                    crate::ltfs::index::Xattr {
                        key: key.into(),
                        value: base64::engine::general_purpose::STANDARD.encode(bytes),
                        base64: true,
                    },
                    false,
                    false,
                ),
                None => v.remove_node_xattr(p, key),
            }
        })
        .map_err(errno)
    }
    fn sync(&self) -> crate::tapefs::Res<()> {
        Ok(())
    } // 每次变更已完成 commit/checkpoint。
}

/// 活跃控制器持锁时的只读管理面；库存为最近一次已完成查询，阶段/assignment实时取SQLite。
pub fn cached_status_if_active(state: &Path) -> Result<Option<Value>> {
    let file = match std::fs::File::open(state.with_extension("library-lock")) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            drop(lock);
            return Ok(None);
        }
        Err((_, e)) if e == nix::errno::Errno::EWOULDBLOCK => {}
        Err((_, e)) => return Err(TapeError::Nix(e)),
    }
    let db = Connection::open_with_flags(state, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.execute_batch("BEGIN")?;
    let data: String = db.query_row(
        "SELECT data FROM library_observations WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let mut status: Value =
        serde_json::from_str(&data).map_err(|e| TapeError::Catalog(e.to_string()))?;
    let storage = status["storage_address_start"]
        .as_u64()
        .ok_or_else(|| refuse("库存快照缺少地址映射"))?;
    if let Some(volumes) = status["volumes"].as_array_mut() {
        for volume in volumes {
            let barcode = volume["barcode"]
                .as_str()
                .ok_or_else(|| refuse("库存快照缺少条码"))?;
            let row:Option<VolumeStatusRow>=db.query_row("SELECT pool,home,blacklist,phase,failure,last_failure FROM library_volumes WHERE barcode=?1",[barcode],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            let object = volume
                .as_object_mut()
                .ok_or_else(|| refuse("库存快照对象非法"))?;
            for key in ["pool", "home_slot", "blacklist", "failure", "last_failure"] {
                object.remove(key);
            }
            if let Some((pool, home, blacklist, phase, failure, last_failure)) = row {
                object.insert("pool".into(), json!(pool));
                object.insert(
                    "home_slot".into(),
                    json!(u64::from(home).checked_sub(storage).map(|n| n + 1)),
                );
                object.insert(
                    "state".into(),
                    json!(if blacklist.is_some() {
                        "BLACKLISTED"
                    } else {
                        &phase
                    }),
                );
                object.insert("blacklist".into(), json!(blacklist));
                object.insert("failure".into(), json!(failure));
                object.insert("last_failure".into(), json!(last_failure));
            } else {
                object.insert("state".into(), json!("NEED_ASSIGN"));
            }
        }
    }
    status["source"] = json!("cached_inventory");
    db.execute_batch("COMMIT")?;
    Ok(Some(status))
}
