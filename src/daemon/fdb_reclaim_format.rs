//! One original quick-initialization attempt, or read-only result observation.
//! Existing partition layout and manufacturer identity are mandatory. Every
//! CDB checks the original durable issue, owner/Permission instance and PR.
use super::executor::fdb_operation_guard::{Guard, confirmed_device_failure};
use super::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::mam::{Mam, MediumIdentity};
use crate::ltfs::mkltfs::{
    MkltfsOptions, require_two_partition_layout, reset_ltfs_at, reset_partition_records,
};
use crate::ltfs::recovery::{
    RecoveryBudget, capture_complete_index_source, capture_recorded_index_source,
};
use crate::managed_catalog::{
    ActiveLibraryRoute, ManagedCatalog, ReclaimFormatAdmission, ReclaimFormatCommitEvidence,
    ReclaimFormatPlan, ReclaimFormatRetryEvidence, ReclaimReservation,
};
use crate::scsi::reservation::{ReservationKey, verify_holder};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;
use sha2::{Digest, Sha256};

pub struct ReclaimFormatter<'a> {
    pub catalog: &'a ManagedCatalog,
    pub device: &'a dyn TapeTransport,
    pub route: ActiveLibraryRoute,
    pub key: ReservationKey,
    pub permission: &'a Permission,
    pub budget: &'a RecoveryBudget,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ReclaimFormatOutcome {
    Committed(ReclaimFormatCommitEvidence),
    Retry(ReclaimFormatRetryEvidence),
}
fn error(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!("FDB 回收空卷初始化: {detail}"))
}
fn refused(detail: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: detail.into(),
    }
}
fn blank(error: &TapeError) -> bool {
    confirmed_device_failure(error) && matches!(error, TapeError::ScsiCommand { sense_key: 8, .. })
}
fn label_at(
    device: &dyn TapeTransport,
    partition: u8,
    hash: &mut Sha256,
) -> Result<Option<LtfsLabel>> {
    let drive = TapeDrive::new(device);
    drive.locate(partition, crate::ltfs::volume::LABEL_BLOCK, true)?;
    let mut buf = vec![0u8; 64 << 10];
    match drive.read_block(&mut buf) {
        Ok(n) => {
            let raw = buf.get(..n).ok_or(TapeError::InvalidResponse {
                expected: buf.len(),
                actual: n,
            })?;
            hash.update([partition]);
            hash.update((n as u64).to_be_bytes());
            hash.update(raw);
            match LtfsLabel::parse(raw) {
                Ok(label) => Ok(Some(label)),
                Err(TapeError::Ltfs(_) | TapeError::Xml(_) | TapeError::XmlSource(_)) => Ok(None),
                Err(other) => Err(other),
            }
        }
        Err(e) if blank(&e) => {
            hash.update([partition, 0xff]);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
fn same_label(actual: &LtfsLabel, expected: &LtfsLabel, partition: u8) -> Result<bool> {
    let mut normalized = actual.clone();
    let expected_location = if partition == 0 { 'a' } else { 'b' };
    if normalized.location != expected_location {
        return Ok(false);
    }
    normalized.location = expected.location;
    Ok(normalized.to_xml()? == expected.to_xml()?)
}
/// Verify an exact prefix of the empty recipe, including EOD. Unexpected
/// records, extra filemarks or a complete different index are never a retry.
fn new_prefix(
    device: &dyn TapeTransport,
    options: &MkltfsOptions,
    time: &str,
    partition: u8,
    hash: &mut Sha256,
) -> Result<bool> {
    let records = reset_partition_records(options, time, partition)?;
    let drive = TapeDrive::new(device);
    drive.locate(partition, 0, true)?;
    let mut buffer = vec![0u8; crate::ltfs::volume::DEFAULT_BLOCK_SIZE as usize];
    for record in &records {
        let n = match drive.read_block(&mut buffer) {
            Ok(n) => n,
            Err(e) if blank(&e) => {
                hash.update([partition, 0xfe]);
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let actual = buffer.get(..n).ok_or(TapeError::InvalidResponse {
            expected: buffer.len(),
            actual: n,
        })?;
        hash.update([partition]);
        hash.update((n as u64).to_be_bytes());
        hash.update(actual);
        match record {
            None if n == 0 => {}
            Some(expected) if n > 0 && expected.starts_with(actual) => {
                if actual.len() != expected.len() {
                    return Ok(false);
                }
            }
            _ => return Err(refused("原初始化分区含不属于固定空卷配方的记录，禁止重置")),
        }
    }
    match drive.read_block(&mut buffer) {
        Err(e) if blank(&e) => Ok(true),
        Err(e) => Err(e),
        Ok(_) => Err(refused("固定初始空索引后还有其他记录，禁止重新初始化")),
    }
}
fn mam_complete(
    device: &dyn TapeTransport,
    plan: &ReclaimFormatPlan,
    barcode: &str,
) -> Result<bool> {
    use crate::ltfs::mam::*;
    let mam = Mam::new(device);
    let short = barcode.chars().take(6).collect::<String>();
    for (id, text, len) in [
        (ATTR_APP_VENDOR, crate::ltfs::mkltfs::APP_VENDOR, 8),
        (ATTR_APP_NAME, crate::ltfs::mkltfs::APP_NAME, 32),
        (ATTR_APP_VERSION, crate::ltfs::mkltfs::APP_VERSION, 8),
        (ATTR_BARCODE, short.as_str(), 32),
        (
            ATTR_APP_FORMAT_VERSION,
            crate::ltfs::label::LTFS_VERSION,
            16,
        ),
    ] {
        let Some(attr) = mam.read_attribute(id)? else {
            return Ok(false);
        };
        let mut expected = vec![b' '; len];
        expected[..text.len()].copy_from_slice(text.as_bytes());
        if attr.format != AttributeFormat::Ascii as u8 || attr.value.as_ref() != expected {
            return Ok(false);
        }
    }
    if let Some(vcr) = mam.read_vcr()?
        && vcr_is_valid(&vcr)
    {
        for partition in [0, 1] {
            let Some(vci) = Mam::with_partition(device, partition).read_vci()? else {
                return Ok(false);
            };
            if vci.volume_uuid != plan.output()
                || vci.generation != 1
                || vci.block != crate::ltfs::volume::FIRST_INDEX_BLOCK
                || !vcr_matches(&vcr, &vci.vcr)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
impl ReclaimFormatter<'_> {
    async fn scope(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
    ) -> Result<()> {
        let owner = self.key.round();
        if self.device.execution_guard()
            != Some((owner, self.permission as *const Permission as usize))
            || reservation.intent()["namespace"] != self.route.route.namespace.to_string()
            || plan.owner() != owner
            || !(self.permission.allows(owner, Phase::Serving)
                || self.permission.allows(owner, Phase::Execution))
        {
            return Err(error("初始化原 owner、执行许可实例或命名空间不符"));
        }
        if self
            .catalog
            .active_library_owner(
                reservation.intent()["library"]
                    .as_str()
                    .ok_or_else(|| error("原带库缺失"))?,
            )
            .await
            .map_err(error)?
            != Some((self.route, owner))
        {
            return Err(error("初始化原带库 route 或 owner 改变"));
        }
        self.catalog
            .verify_reclaim_format(reservation, owner, plan)
            .await
            .map_err(error)
    }
    fn options(&self, reservation: &ReclaimReservation, plan: &ReclaimFormatPlan) -> MkltfsOptions {
        MkltfsOptions {
            volume_id: reservation.spec().barcode().chars().take(6).collect(),
            volume_uuid: Some(plan.output()),
            quick: true,
            ..Default::default()
        }
    }
    fn old_dp(
        &self,
        device: &dyn TapeTransport,
        plan: &ReclaimFormatPlan,
        label: &LtfsLabel,
        hash: &mut Sha256,
    ) -> Result<()> {
        let expected = plan.source_identity().map_err(error)?;
        let source =
            capture_recorded_index_source(&TapeDrive::new(device), label, &expected, self.budget)?;
        if source.identity()? != expected || source.construct_end() != plan.source_dp_eod() {
            return Err(refused("原数据分区不再是已完整覆盖的索引构造"));
        }
        let drive = TapeDrive::new(device);
        drive.locate(1, plan.source_dp_eod(), true)?;
        let mut buffer = vec![0u8; 64 << 10];
        match drive.read_block(&mut buffer) {
            Err(e) if blank(&e) => {}
            Err(e) => return Err(e),
            Ok(_) => return Err(refused("封存源 DP 尾部之后出现了新记录，禁止重置")),
        }
        hash.update(source.identity()?.to_bytes());
        Ok(())
    }
    /// Read-only even on an incomplete volume; no saved snapshot authorizes a
    /// reset. A new owner must bind a new actual observation to its own issue.
    pub async fn observe(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
    ) -> Result<ReclaimFormatOutcome> {
        self.scope(reservation, plan).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.device, || {
            futures_executor::block_on(self.scope(reservation, plan))?;
            verify_holder(self.device, self.key)
        });
        let medium = Mam::new(&checked).read_medium_identity()?;
        if &medium != plan.cartridge() {
            return Err(refused("当前介质制造商/序列号不同于原格式化目标"));
        }
        require_two_partition_layout(&checked)?;
        let mut hash = Sha256::new();
        let ip = label_at(&checked, 0, &mut hash)?;
        let dp = label_at(&checked, 1, &mut hash)?;
        let old = plan.source_label().map_err(error)?;
        let options = self.options(reservation, plan);
        let records = reset_partition_records(&options, plan.formatted_at(), 0)?;
        let new = LtfsLabel::parse(records[2].as_ref().expect("fixed label"))?;
        let classify = |label: &Option<LtfsLabel>, partition: u8| -> Result<u8> {
            match label {
                None => Ok(0),
                Some(label) if same_label(label, &old, partition)? => Ok(1),
                Some(label) if same_label(label, &new, partition)? => Ok(2),
                _ => Err(refused("实际卷标签不属于原源或固定初始化目标")),
            }
        };
        let a = classify(&ip, 0)?;
        let b = classify(&dp, 1)?;
        let mut original = false;
        let mut complete = false;
        match (a, b) {
            (1, 1) => {
                let actual = capture_complete_index_source(
                    &TapeDrive::new(&checked),
                    ip.as_ref().unwrap(),
                    self.budget,
                )?;
                plan.require_source_capture(&actual).map_err(error)?;
                hash.update(actual.identity()?.to_bytes());
                original = true;
            }
            (0 | 2, 1) => {
                new_prefix(&checked, &options, plan.formatted_at(), 0, &mut hash)?;
                self.old_dp(&checked, plan, dp.as_ref().unwrap(), &mut hash)?;
            }
            (2, 0 | 2) => {
                let first = new_prefix(&checked, &options, plan.formatted_at(), 0, &mut hash)?;
                let second = new_prefix(&checked, &options, plan.formatted_at(), 1, &mut hash)?;
                if !first {
                    return Err(refused("原初始化顺序不允许 IP 残缺时已重建 DP"));
                }
                complete = second && mam_complete(&checked, plan, reservation.spec().barcode())?;
            }
            _ => return Err(refused("原初始化两分区状态不属于可恢复的写入顺序")),
        }
        let outcome = if complete {
            let current = capture_complete_index_source(
                &TapeDrive::new(&checked),
                ip.as_ref().unwrap(),
                self.budget,
            )?;
            ReclaimFormatOutcome::Committed(
                ReclaimFormatCommitEvidence::from_current(plan, self.key.round(), &current)
                    .map_err(error)?,
            )
        } else {
            ReclaimFormatOutcome::Retry(
                ReclaimFormatRetryEvidence::from_observation(
                    plan,
                    self.key.round(),
                    &medium,
                    original,
                    hash.finalize().into(),
                )
                .map_err(error)?,
            )
        };
        if Mam::new(&checked).read_medium_identity()? != medium {
            return Err(refused("初始化结果读取期间介质身份改变"));
        }
        checked.verify_proof()?;
        Ok(outcome)
    }
    /// Consume exactly one admission. Replays only observe. A failed write
    /// returns with its original issue and claims intact; never retries inline.
    pub async fn execute(
        &mut self,
        reservation: &ReclaimReservation,
        admission: ReclaimFormatAdmission,
    ) -> Result<ReclaimFormatOutcome> {
        let plan = admission.plan();
        let observed = self.observe(reservation, plan).await?;
        if admission.needs_observation() || matches!(observed, ReclaimFormatOutcome::Committed(_)) {
            return Ok(observed);
        }
        self.scope(reservation, plan).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.device, || {
            futures_executor::block_on(self.scope(reservation, plan))?;
            verify_holder(self.device, self.key)
        });
        let medium: MediumIdentity = Mam::new(&checked).read_medium_identity()?;
        if &medium != plan.cartridge() {
            return Err(refused("初始化前介质制造商/序列号改变"));
        }
        require_two_partition_layout(&checked)?;
        log::info!(
            "原生 reclaim 重建空卷 条码={} 输出={} 原尝试={}",
            reservation.spec().barcode(),
            plan.output(),
            plan.attempt()
        );
        reset_ltfs_at(
            &checked,
            &self.options(reservation, plan),
            plan.formatted_at(),
        )?;
        checked.verify_proof()?;
        match self.observe(reservation, plan).await? {
            result @ ReclaimFormatOutcome::Committed(_) => Ok(result),
            ReclaimFormatOutcome::Retry(_) => {
                Err(refused("初始化后未读回完整固定空卷，保留原尝试等待核实"))
            }
        }
    }
}
