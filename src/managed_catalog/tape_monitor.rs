//! Owner-fenced observations, separate from assignment and file authority.
//! Polling never clears an I/O failure: LOG SENSE may consume device alerts.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TapeCapacity {
    pub partition: u8,
    pub maximum: u64,
    pub remaining: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TapeMonitorSample {
    pub drive_serial: String,
    pub observed_at: String,
    pub write_protected: Option<bool>,
    pub data: Option<TapeCapacity>,
    pub index: Option<TapeCapacity>,
    pub alerts: Option<Vec<u16>>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TapeMonitorError {
    pub operation: String,
    pub cdb: Vec<u8>,
    pub observed_at: String,
    pub status: Option<u8>,
    pub sense_key: Option<u8>,
    pub asc: Option<u8>,
    pub ascq: Option<u8>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TapeMediaFault {
    NeedReplace,
    RequireReplace,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TapeMonitorRecord {
    pub version: u8,
    #[serde(with = "uuid_text")]
    pub namespace: Uuid,
    pub library: String,
    pub barcode: String,
    #[serde(with = "uuid_text")]
    pub volume: Uuid,
    pub owner_epoch: u64,
    pub observed_at: String,
    pub last_success: Option<String>,
    pub sample: Option<TapeMonitorSample>,
    pub alert_history: Vec<u16>,
    pub media_fault: Option<TapeMediaFault>,
    pub media_fault_evidence: Option<TapeMonitorError>,
    pub last_error: Option<TapeMonitorError>,
    pub import_required: bool,
    pub requires_validation: bool,
    pub verified_generation: Option<u64>,
}

mod uuid_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        id: &Uuid,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Uuid, D::Error> {
        let s = String::deserialize(d)?;
        let id = Uuid::parse_str(&s).map_err(serde::de::Error::custom)?;
        if id.to_string() != s {
            return Err(serde::de::Error::custom("UUID 编码不规范"));
        }
        Ok(id)
    }
}

fn valid_time(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}
fn validate(record: &TapeMonitorRecord) -> Result<()> {
    if record.version != 1
        || record.namespace.is_nil()
        || record.volume.is_nil()
        || record.owner_epoch == 0
        || !valid_time(&record.observed_at)
        || record.last_success.as_ref().is_some_and(|s| !valid_time(s))
        || record.alert_history.len() > 64
        || record.alert_history.iter().any(|v| !(1..=64).contains(v))
        || record.alert_history.windows(2).any(|w| w[0] >= w[1])
        || record.verified_generation == Some(0)
    {
        return Err(CatalogError::Invalid("磁带监控身份、时间或历史无效"));
    }
    control::validate_physical_serial(&record.library)?;
    validate_barcode(&record.barcode)?;
    if let Some(sample) = &record.sample {
        if sample.drive_serial.is_empty()
            || sample.drive_serial.len() > 255
            || !valid_time(&sample.observed_at)
            || sample.errors.len() > 8
            || sample.errors.iter().any(|s| s.len() > 1024)
            || sample.alerts.as_ref().is_some_and(|v| {
                v.len() > 64
                    || v.iter().any(|n| !(1..=64).contains(n))
                    || v.windows(2).any(|w| w[0] >= w[1])
            })
        {
            return Err(CatalogError::Invalid("磁带监控采样字段无效"));
        }
        for capacity in [&sample.data, &sample.index].into_iter().flatten() {
            if capacity.partition > 1
                || capacity.maximum == 0
                || capacity.remaining > capacity.maximum
                || capacity.maximum > i64::MAX as u64
            {
                return Err(CatalogError::Invalid("磁带监控容量无效"));
            }
        }
        if let (Some(data), Some(index)) = (&sample.data, &sample.index)
            && data.partition == index.partition
        {
            return Err(CatalogError::Invalid("数据与索引分区不能相同"));
        }
    }
    for error in [&record.last_error, &record.media_fault_evidence]
        .into_iter()
        .flatten()
    {
        if !valid_time(&error.observed_at)
            || error.operation.is_empty()
            || error.cdb.len() > 16
            || error.operation.len() > 64
            || error.message.len() > 2048
        {
            return Err(CatalogError::Invalid("磁带监控错误字段无效"));
        }
    }
    if record.media_fault != record.media_fault_evidence.as_ref().and_then(media_fault) {
        return Err(CatalogError::Invalid(
            "永久介质故障缺少匹配的 CDB/sense 证据",
        ));
    }
    Ok(())
}

fn record_key(ns: Uuid, serial: &str, barcode: &str) -> Vec<u8> {
    let mut k = owner_key(serial);
    k.extend_from_slice(b"/tape-monitor/");
    k.extend_from_slice(ns.as_bytes());
    append_component(&mut k, barcode.as_bytes());
    k
}
fn library_key(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut k = owner_key(serial);
    k.extend_from_slice(b"/monitor/");
    k.extend_from_slice(ns.as_bytes());
    k
}

async fn read_library_at(tr: &Transaction, ns: Uuid, serial: &str) -> Result<Option<Value>> {
    let Some(raw) = tr.get(&library_key(ns, serial), false).await? else {
        return Ok(None);
    };
    let v: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("带库监控编码"))?;
    if v["version"] != 1
        || v["namespace"] != ns.to_string()
        || v["interval_seconds"] != 30
        || !v["owner_epoch"].as_u64().is_some_and(|n| n > 0)
        || !v.as_object().is_some_and(|o| o.len() == 7)
        || !v["last_attempt"].as_str().is_some_and(valid_time)
        || !(v["last_success"].is_null() || v["last_success"].as_str().is_some_and(valid_time))
        || !(v["error"].is_null() || v["error"].as_str().is_some_and(|s| s.len() <= 2048))
        || v.to_string().as_bytes() != raw.as_ref()
    {
        return Err(CatalogError::Corrupt("带库监控作用域、字段或编码"));
    }
    Ok(Some(v))
}

pub(super) async fn read_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
) -> Result<Option<TapeMonitorRecord>> {
    let Some(raw) = tr.get(&record_key(ns, serial, barcode), false).await? else {
        return Ok(None);
    };
    if raw.len() > 16_384 {
        return Err(CatalogError::Corrupt("磁带监控记录超过上限"));
    }
    let record: TapeMonitorRecord =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("磁带监控记录编码"))?;
    validate(&record).map_err(|_| CatalogError::Corrupt("磁带监控记录字段"))?;
    if record.namespace != ns
        || record.library != serial
        || record.barcode != barcode
        || serde_json::to_value(&record)
            .map_err(|_| CatalogError::Corrupt("监控编码"))?
            .to_string()
            .as_bytes()
            != raw.as_ref()
    {
        return Err(CatalogError::Corrupt("磁带监控记录作用域或规范编码"));
    }
    Ok(Some(record))
}

impl TapeMonitorRecord {
    /// Existing owned S4/tail recovery must still run after a transient error;
    /// it proves the result before reopening ordinary writes. Physical write
    /// prohibitions and permanent media faults apply to recovery as well.
    pub fn recovery_write_allowed(&self) -> bool {
        self.media_fault.is_none()
            && self.sample.as_ref().is_none_or(|s| {
                s.write_protected != Some(true)
                    && s.index.as_ref().is_none_or(|c| c.remaining > 0)
                    && !self.alert_history.contains(&17)
            })
    }
    pub fn write_allowed(&self) -> bool {
        self.recovery_write_allowed() && !self.import_required && !self.requires_validation
    }
    pub fn data_write_allowed(&self) -> bool {
        self.write_allowed()
            && self
                .sample
                .as_ref()
                .and_then(|s| s.data.as_ref())
                .is_none_or(|c| c.remaining > 0)
    }
    pub fn state(&self) -> &'static str {
        if let Some(fault) = &self.media_fault {
            return match fault {
                TapeMediaFault::NeedReplace => "need_replace",
                TapeMediaFault::RequireReplace => "require_replace",
            };
        }
        if self.import_required {
            return "import_required";
        }
        if self.requires_validation {
            return "require_validate";
        }
        let Some(sample) = &self.sample else {
            return "unknown";
        };
        if sample.write_protected == Some(true) {
            return "write_protected";
        }
        if self.alert_history.contains(&17) {
            return "recall_only";
        }
        if sample.index.as_ref().is_some_and(|c| c.remaining == 0) {
            return "full";
        }
        if sample.data.as_ref().is_some_and(|c| c.remaining == 0) {
            return "data_full";
        }
        if sample.errors.is_empty()
            && sample.write_protected.is_some()
            && sample.alerts.is_some()
            && sample.data.is_some()
            && sample.index.is_some()
            && self.verified_generation.is_some()
        {
            "appendable"
        } else {
            "unknown"
        }
    }
    pub fn to_json(&self) -> Value {
        let mut value =
            serde_json::to_value(self).expect("monitor record has no fallible JSON values");
        value["state"] = json!(self.state());
        value["write_allowed"] = json!(self.write_allowed());
        value["data_write_allowed"] = json!(self.data_write_allowed());
        value["alert_history_details"] = json!(
            self.alert_history
                .iter()
                .map(|&flag| alert_detail(flag))
                .collect::<Vec<_>>()
        );
        if let Some(alerts) = self.sample.as_ref().and_then(|s| s.alerts.as_ref()) {
            value["sample"]["alert_details"] = json!(
                alerts
                    .iter()
                    .map(|&flag| alert_detail(flag))
                    .collect::<Vec<_>>()
            );
        }
        value
    }
}

fn alert_detail(flag: u16) -> Value {
    let (scope, description) = match flag {
        5 => (
            "drive_or_medium",
            "读取失败，单凭告警不能区分驱动器或介质故障",
        ),
        6 => (
            "drive_or_medium",
            "写入失败，单凭告警不能区分驱动器或介质故障",
        ),
        17 => ("medium_format", "只读格式"),
        20 => ("drive", "驱动器需要立即清洁"),
        21 => ("drive", "驱动器要求周期清洁"),
        35 => ("environment", "驱动器湿度超出规定范围"),
        36 => ("environment", "驱动器温度超出规定范围"),
        37 => ("environment", "驱动器电压超出规定范围"),
        52 => (
            "previous_medium",
            "上一盘已卸载介质的系统区写入失败，不归因于当前磁带",
        ),
        _ => (
            "unspecified",
            "保留原始 TapeAlert 编号，需结合驱动器及介质诊断",
        ),
    };
    json!({"flag":flag,"scope":scope,"description":description})
}

/// Persisted I/O fault; transient/ownership errors are deliberately not media
/// replacement diagnoses. Only a proven command context may set these states.
fn media_fault(error: &TapeMonitorError) -> Option<TapeMediaFault> {
    match (
        error.cdb.first(),
        error.status,
        error.sense_key,
        error.asc,
        error.ascq,
    ) {
        (Some(0x08), Some(2), Some(3), Some(0x11), Some(0)) => Some(TapeMediaFault::NeedReplace),
        (Some(0x0a | 0x10), Some(2), Some(3), Some(0x0c), Some(0)) => {
            Some(TapeMediaFault::RequireReplace)
        }
        _ => None,
    }
}

impl ManagedCatalog {
    /// One guarded transaction merges observations; fields omitted by a poll
    /// are preserved. A different owner cannot overwrite a newer round.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_tape_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        barcode: &str,
        volume: Uuid,
        sample: Option<&TapeMonitorSample>,
        error: Option<&TapeMonitorError>,
        index: Option<(ActiveIndexIdentity, bool)>,
    ) -> Result<TapeMonitorRecord> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let assignment = Self::verified_assignment(&tr, serial, barcode)
                            .await?
                            .filter(|a| a.namespace == ns && a.volume_id == Some(volume))
                            .ok_or(CatalogError::Invalid("监控磁带与当前分配身份不符"))?;
                        let _ = assignment;
                        let now = crate::ltfs::label::ltfs_time_now();
                        let mut record =
                            read_at(&tr, ns, serial, barcode)
                                .await?
                                .unwrap_or(TapeMonitorRecord {
                                    version: 1,
                                    namespace: ns,
                                    library: serial.into(),
                                    barcode: barcode.into(),
                                    volume,
                                    owner_epoch: owner,
                                    observed_at: now.clone(),
                                    last_success: None,
                                    sample: None,
                                    alert_history: Vec::new(),
                                    media_fault: None,
                                    media_fault_evidence: None,
                                    last_error: None,
                                    import_required: false,
                                    requires_validation: false,
                                    verified_generation: None,
                                });
                        if record.volume != volume {
                            // Keep any diagnosis across barcode reuse. Formatting alone is
                            // not evidence that a previously bad cartridge was replaced.
                            record.sample = None;
                            record.last_success = None;
                            record.verified_generation = None;
                            record.volume = volume;
                        }
                        record.owner_epoch = owner;
                        record.observed_at = now;
                        if let Some(sample) = sample {
                            let mut flags: std::collections::BTreeSet<_> =
                                record.alert_history.iter().copied().collect();
                            flags.extend(sample.alerts.iter().flatten().copied());
                            record.alert_history = flags.into_iter().collect();
                            record.sample = Some(sample.clone());
                            if sample.errors.is_empty()
                                && sample.alerts.is_some()
                                && sample.write_protected.is_some()
                                && sample.data.is_some()
                                && sample.index.is_some()
                            {
                                record.last_success = Some(sample.observed_at.clone());
                            }
                        }
                        if let Some(error) = error {
                            let fault = media_fault(error);
                            if record.media_fault != Some(TapeMediaFault::RequireReplace)
                                && fault.is_some()
                            {
                                record.media_fault = fault;
                                record.media_fault_evidence = Some(error.clone());
                            }
                            record.last_error = Some(error.clone());
                            record.requires_validation = true;
                        }
                        if let Some((index, import_required)) = index {
                            if index.volume_uuid != volume
                                || active_volume::observed_at(&tr, serial, volume).await?.1 != index
                            {
                                return Err(CatalogError::Invalid("监控索引与FDB已发布身份不符"));
                            }
                            record.import_required = import_required;
                            if !import_required {
                                record.verified_generation = Some(index.generation);
                                record.requires_validation = false;
                            }
                        }
                        validate(&record)?;
                        let bytes = serde_json::to_value(&record)
                            .map_err(|_| CatalogError::Invalid("监控编码失败"))?
                            .to_string();
                        if bytes.len() > 16_384 {
                            return Err(CatalogError::Invalid("磁带监控记录超过上限"));
                        }
                        let capacity =
                            tape_capacity::sample_mutation(&tr, &record, sample.is_some()).await?;
                        tr.set(&record_key(ns, serial, barcode), bytes.as_bytes());
                        if let Some((key, value)) = capacity {
                            match value {
                                Some(raw) => tr.set(&key, &raw),
                                None => tr.clear(&key),
                            }
                        }
                        notifications::tape_changed_at(
                            &tr,
                            ns,
                            serial,
                            barcode,
                            "monitor_observed",
                        )
                        .await?;
                        Ok(record)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn active_tape_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        barcode: &str,
    ) -> Result<Option<TapeMonitorRecord>> {
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        read_at(&tr, ns, serial, barcode).await
    }

    pub async fn record_library_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        error: Option<&str>,
    ) -> Result<()> {
        if error.is_some_and(|e| e.len() > 2048) {
            return Err(CatalogError::Invalid("带库监控错误超长"));
        }
        self.db.run(|tr,_|async move { requests::retry_result(async {
            let ns=routing::require_runtime_active_owner(&tr,serial,route,owner).await?;
            let key=library_key(ns,serial);
            let old=read_library_at(&tr,ns,serial).await?;
            let now=crate::ltfs::label::ltfs_time_now();
            let last_success=if error.is_none() { json!(now) } else { old.as_ref().map(|v|v["last_success"].clone()).unwrap_or(Value::Null) };
            tr.set(&key,json!({"version":1,"namespace":ns.to_string(),"owner_epoch":owner,"interval_seconds":30,"last_attempt":now,"last_success":last_success,"error":error}).to_string().as_bytes());
            notifications::changed_at(&tr,ns,serial,"library-monitor","library.monitor.changed",json!({"error":error,"status":if error.is_some(){"error"}else{"ok"}})).await?;
            Ok(())
        }.await) }).await?
    }
    pub async fn active_library_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        read_library_at(&tr, ns, serial).await
    }
}

pub(in crate::managed_catalog) async fn require_data_writable(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
) -> Result<()> {
    if read_at(tr, ns, serial, barcode)
        .await?
        .is_some_and(|r| !r.data_write_allowed())
    {
        return Err(CatalogError::MediaUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn error(opcode: u8, key: u8, asc: u8, ascq: u8) -> TapeMonitorError {
        TapeMonitorError {
            operation: "write".into(),
            cdb: vec![opcode, 0, 0, 0, 0, 0],
            observed_at: crate::ltfs::label::ltfs_time_now(),
            status: Some(2),
            sense_key: Some(key),
            asc: Some(asc),
            ascq: Some(ascq),
            message: "模拟错误".into(),
        }
    }
    fn record() -> TapeMonitorRecord {
        let now = crate::ltfs::label::ltfs_time_now();
        TapeMonitorRecord {
            version: 1,
            namespace: Uuid::new_v4(),
            library: "SIM-MONITOR".into(),
            barcode: "TST001L8".into(),
            volume: Uuid::new_v4(),
            owner_epoch: 1,
            observed_at: now.clone(),
            last_success: None,
            sample: Some(TapeMonitorSample {
                drive_serial: "SIMDRV".into(),
                observed_at: now,
                write_protected: Some(false),
                data: Some(TapeCapacity {
                    partition: 1,
                    maximum: 1024,
                    remaining: 512,
                }),
                index: Some(TapeCapacity {
                    partition: 0,
                    maximum: 128,
                    remaining: 64,
                }),
                alerts: Some(vec![]),
                errors: vec![],
            }),
            alert_history: vec![],
            media_fault: None,
            media_fault_evidence: None,
            last_error: None,
            import_required: false,
            requires_validation: false,
            verified_generation: Some(1),
        }
    }
    #[test]
    fn media_diagnosis_requires_actual_read_or_write_cdb_and_exact_sense() {
        assert_eq!(
            media_fault(&error(8, 3, 0x11, 0)),
            Some(TapeMediaFault::NeedReplace)
        );
        for op in [0x0a, 0x10] {
            assert_eq!(
                media_fault(&error(op, 3, 0x0c, 0)),
                Some(TapeMediaFault::RequireReplace)
            );
        }
        for op in [0x8c, 0x8d, 0x4d, 0x92, 0x5e, 0] {
            assert_eq!(media_fault(&error(op, 3, 0x11, 0)), None);
            assert_eq!(media_fault(&error(op, 3, 0x0c, 0)), None);
        }
        for (op, key, asc, q) in [
            (8, 4, 0x11, 0),
            (8, 6, 0x11, 0),
            (8, 3, 0x11, 1),
            (8, 3, 0x0c, 0),
            (0xa, 3, 0x11, 0),
            (0xa, 3, 0x0c, 1),
            (0xa, 4, 0x0c, 0),
        ] {
            assert_eq!(media_fault(&error(op, key, asc, q)), None);
        }
        let mut host = error(8, 3, 0x11, 0);
        host.status = None;
        assert_eq!(media_fault(&host), None);
    }
    #[test]
    fn data_full_still_allows_metadata_but_wp_and_index_full_do_not() {
        let mut r = record();
        assert_eq!(r.state(), "appendable");
        assert!(r.data_write_allowed());
        r.sample.as_mut().unwrap().data.as_mut().unwrap().remaining = 0;
        assert_eq!(r.state(), "data_full");
        assert!(r.write_allowed());
        assert!(!r.data_write_allowed());
        r.sample.as_mut().unwrap().index.as_mut().unwrap().remaining = 0;
        assert_eq!(r.state(), "full");
        assert!(!r.write_allowed());
        r.sample.as_mut().unwrap().write_protected = Some(true);
        assert_eq!(r.state(), "write_protected");
        assert!(!r.write_allowed());
    }
    #[test]
    fn drive_and_environmental_alerts_do_not_become_bad_media() {
        let mut r = record();
        r.sample.as_mut().unwrap().alerts = Some(vec![5, 6, 35, 36, 37, 52]);
        r.alert_history = vec![5, 6, 35, 36, 37, 52];
        assert_eq!(r.media_fault, None);
        assert!(r.data_write_allowed());
        assert_eq!(
            r.to_json()["sample"]["alert_details"][2]["scope"],
            "environment"
        );
        assert_eq!(
            r.to_json()["sample"]["alert_details"][5]["scope"],
            "previous_medium"
        );
        r.alert_history.push(17);
        r.alert_history.sort_unstable();
        r.sample.as_mut().unwrap().alerts = Some(vec![]);
        assert_eq!(r.state(), "recall_only");
        assert!(!r.write_allowed());
    }
    #[test]
    fn missing_samples_are_unknown_and_malformed_records_are_rejected() {
        let mut r = record();
        r.sample = None;
        assert_eq!(r.state(), "unknown");
        r.import_required = true;
        assert_eq!(r.state(), "import_required");
        assert!(!r.write_allowed());
        assert!(
            r.recovery_write_allowed(),
            "原恢复协议仍须判定外部 Full，不能跳过后按旧头读"
        );
        r.import_required = false;
        r.requires_validation = true;
        assert!(!r.write_allowed());
        assert!(
            r.recovery_write_allowed(),
            "暂态错误仍可接续原已拥有 S4 恢复"
        );
        r.media_fault = Some(TapeMediaFault::NeedReplace);
        assert!(!r.recovery_write_allowed());
        assert_eq!(r.state(), "need_replace");
        r.alert_history = vec![35, 35];
        assert!(validate(&r).is_err());
        r.alert_history = vec![];
        r.observed_at = "bad".into();
        assert!(validate(&r).is_err());
        let mut r = record();
        r.sample.as_mut().unwrap().data.as_mut().unwrap().remaining = 1025;
        assert!(validate(&r).is_err());
    }
}
