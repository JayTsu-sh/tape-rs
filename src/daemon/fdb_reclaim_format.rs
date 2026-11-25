//! One original quick-initialization attempt, or read-only result observation.
//! Existing partition layout and manufacturer identity are mandatory. Every
//! CDB checks the original durable issue, owner/Permission instance and PR.
use super::executor::fdb_operation_guard::{Guard, confirmed_device_failure};
use super::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::index::{IndexLocation, LtfsIndex};
use crate::ltfs::label::{LtfsLabel, PART_DATA, PART_INDEX};
use crate::ltfs::mam::{Mam, MediumIdentity};
use crate::ltfs::mkltfs::{
    MkltfsOptions, require_two_partition_layout, reset_ltfs_at, reset_partition_records,
};
use crate::ltfs::recovery::{
    RecoveryBudget, capture_complete_index_source, capture_recorded_index_source,
};
use crate::ltfs::volume::{CommittedIndexConstruct, FIRST_INDEX_BLOCK};
use crate::managed_catalog::{
    ActiveLibraryRoute, ManagedCatalog, ReclaimFormatAdmission, ReclaimFormatCommitEvidence,
    ReclaimFormatPlan, ReclaimFormatRetryEvidence, ReclaimReservation,
};
use crate::scsi::reservation::{ReservationKey, verify_holder, verify_holder_without_ua_retry};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::time::{Duration, Instant};

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

struct FormatAuthorityTiming<'a> {
    operation: &'a serde_json::Value,
    attempt: u32,
    phase: &'static str,
    checks: Cell<u64>,
    scope_time: Cell<Duration>,
    pr_time: Cell<Duration>,
}

impl<'a> FormatAuthorityTiming<'a> {
    fn new(
        reservation: &'a ReclaimReservation,
        plan: &ReclaimFormatPlan,
        phase: &'static str,
    ) -> Self {
        Self {
            operation: &reservation.intent()["operation"],
            attempt: plan.attempt(),
            phase,
            checks: Cell::default(),
            scope_time: Cell::default(),
            pr_time: Cell::default(),
        }
    }
}

impl Drop for FormatAuthorityTiming<'_> {
    fn drop(&mut self) {
        log::debug!(
            "回收源初始化命令权限核对耗时: 操作={} 尝试={} 阶段={} 检查次数={} 许可及FDB耗时={:?} PR耗时={:?}",
            self.operation,
            self.attempt,
            self.phase,
            self.checks.get(),
            self.scope_time.get(),
            self.pr_time.get(),
        );
    }
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
/// Verify an exact prefix of the empty recipe, including EOD. A complete
/// prefix returns the actual index record. Unexpected records, extra filemarks
/// or a complete different index are never a retry.
fn new_prefix(
    device: &dyn TapeTransport,
    partition: u8,
    records: &[Option<Vec<u8>>],
    hash: &mut Sha256,
) -> Result<Option<Vec<u8>>> {
    let drive = TapeDrive::new(device);
    drive.locate(partition, 0, true)?;
    let mut buffer = vec![0u8; crate::ltfs::volume::DEFAULT_BLOCK_SIZE as usize];
    let mut index_xml = None;
    for (block, record) in records.iter().enumerate() {
        let n = match drive.read_block(&mut buffer) {
            Ok(n) => n,
            Err(e) if blank(&e) => {
                hash.update([partition, 0xfe]);
                return Ok(None);
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
                    return Ok(None);
                }
                if block as u64 == FIRST_INDEX_BLOCK {
                    index_xml = Some(actual.to_vec());
                }
            }
            _ => return Err(refused("原初始化分区含不属于固定空卷配方的记录，禁止重置")),
        }
    }
    match drive.read_block(&mut buffer) {
        Err(e) if blank(&e) => index_xml
            .map(Some)
            .ok_or_else(|| refused("固定空卷配方缺少实际读回的索引记录")),
        Err(e) => Err(e),
        Ok(_) => Err(refused("固定初始空索引后还有其他记录，禁止重新初始化")),
    }
}

/// Publication evidence is minted only after the original guarded task reads
/// both exact partitions, their physical tails, and the new MAM attributes.
pub(crate) struct VerifiedEmptyReset(CommittedIndexConstruct);

impl VerifiedEmptyReset {
    pub(crate) fn head(self) -> CommittedIndexConstruct {
        self.0
    }
}

fn capture_empty_reset_head(
    device: &dyn TapeTransport,
    options: &MkltfsOptions,
    formatted_at: &str,
    max_index_bytes: usize,
) -> Result<CommittedIndexConstruct> {
    let drive = TapeDrive::new(device);
    let mut hash = Sha256::new();
    let dp_location = IndexLocation {
        partition: PART_DATA,
        start_block: FIRST_INDEX_BLOCK,
    };
    let mut head = None;
    for (partition, name, previous) in [(0, PART_INDEX, Some(dp_location)), (1, PART_DATA, None)] {
        let records = reset_partition_records(options, formatted_at, partition)?;
        let actual = new_prefix(device, partition, &records, &mut hash)?
            .ok_or_else(|| refused("初始化后未读回完整固定空卷，保留原尝试等待核实"))?;
        if actual.len() > max_index_bytes {
            return Err(refused("实际空卷索引超出原恢复预算"));
        }
        // Independently locate and confirm the physical EOD while the drive
        // is still in this partition. The previous BLANK CHECK may itself
        // leave no reliable short position on some drives.
        let end = FIRST_INDEX_BLOCK + 2;
        drive.locate_eod(partition)?;
        if !drive.position_matches(partition, end)? {
            return Err(refused("初始化后实际分区带尾不符"));
        }
        let index = LtfsIndex::parse(&actual)?;
        if Some(index.volume_uuid) != options.volume_uuid
            || index.generation != 1
            || index.incremental
            || index.self_location.partition != name
            || index.self_location.start_block != FIRST_INDEX_BLOCK
            || index.previous_location != previous
        {
            return Err(refused("初始化后实际空索引代次、位置或反链不符"));
        }
        if partition == 1 {
            head = Some(CommittedIndexConstruct {
                volume_uuid: index.volume_uuid,
                generation: index.generation,
                location: index.self_location,
                sha256: Sha256::digest(&actual).into(),
                incremental: index.incremental,
            });
        }
    }
    head.ok_or_else(|| refused("初始化后缺少实际 DP 空索引"))
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
        self.catalog
            .verify_reclaim_format_for_route(reservation, self.route, owner, plan)
            .await
            .map_err(error)
    }
    fn authorize_cdb(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
        bracketed: bool,
        timing: &FormatAuthorityTiming<'_>,
    ) -> Result<()> {
        timing.checks.set(timing.checks.get() + 1);
        let started = Instant::now();
        let scoped = futures_executor::block_on(self.scope(reservation, plan));
        timing
            .scope_time
            .set(timing.scope_time.get() + started.elapsed());
        scoped?;
        let started = Instant::now();
        let pr = if bracketed {
            verify_holder_without_ua_retry(self.device, self.key)
        } else {
            verify_holder(self.device, self.key)
        };
        timing.pr_time.set(timing.pr_time.get() + started.elapsed());
        pr
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
        self.observe_guarded(reservation, plan, false, |checked, medium| {
            self.classify_result(checked, reservation, plan, medium)
        })
        .await
    }
    async fn observe_guarded(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
        fresh_reset: bool,
        read: impl FnOnce(&dyn TapeTransport, &MediumIdentity) -> Result<ReclaimFormatOutcome>,
    ) -> Result<ReclaimFormatOutcome> {
        self.scope(reservation, plan).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let phase = if fresh_reset {
            "空卷读回"
        } else {
            "原状态观察"
        };
        let timing = FormatAuthorityTiming::new(reservation, plan, phase);
        let authority = || self.authorize_cdb(reservation, plan, fresh_reset, &timing);
        let checked = if fresh_reset {
            Guard::new_bracketed_read(self.device, authority)
        } else {
            Guard::new(self.device, authority)
        };
        let medium = Mam::new(&checked).read_medium_identity()?;
        if &medium != plan.cartridge() {
            return Err(refused("当前介质制造商/序列号不同于原格式化目标"));
        }
        require_two_partition_layout(&checked)?;
        let outcome = read(&checked, &medium)?;
        if Mam::new(&checked).read_medium_identity()? != medium {
            return Err(refused("初始化结果读取期间介质身份改变"));
        }
        checked.verify_proof()?;
        Ok(outcome)
    }
    fn classify_result(
        &self,
        checked: &dyn TapeTransport,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
        medium: &MediumIdentity,
    ) -> Result<ReclaimFormatOutcome> {
        let mut hash = Sha256::new();
        let ip = label_at(checked, 0, &mut hash)?;
        let dp = label_at(checked, 1, &mut hash)?;
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
                    &TapeDrive::new(checked),
                    ip.as_ref().unwrap(),
                    self.budget,
                )?;
                plan.require_source_capture(&actual).map_err(error)?;
                hash.update(actual.identity()?.to_bytes());
                original = true;
            }
            (0 | 2, 1) => {
                new_prefix(checked, 0, &records, &mut hash)?;
                self.old_dp(checked, plan, dp.as_ref().unwrap(), &mut hash)?;
            }
            (2, 0 | 2) => {
                let first = new_prefix(checked, 0, &records, &mut hash)?;
                let dp_records = reset_partition_records(&options, plan.formatted_at(), 1)?;
                let second = new_prefix(checked, 1, &dp_records, &mut hash)?;
                if first.is_none() {
                    return Err(refused("原初始化顺序不允许 IP 残缺时已重建 DP"));
                }
                complete =
                    second.is_some() && mam_complete(checked, plan, reservation.spec().barcode())?;
            }
            _ => return Err(refused("原初始化两分区状态不属于可恢复的写入顺序")),
        }
        if complete {
            self.complete_evidence(checked, plan, ip.as_ref().unwrap())
        } else {
            Ok(ReclaimFormatOutcome::Retry(
                ReclaimFormatRetryEvidence::from_observation(
                    plan,
                    self.key.round(),
                    medium,
                    original,
                    hash.finalize().into(),
                )
                .map_err(error)?,
            ))
        }
    }
    fn complete_evidence(
        &self,
        checked: &dyn TapeTransport,
        plan: &ReclaimFormatPlan,
        label: &LtfsLabel,
    ) -> Result<ReclaimFormatOutcome> {
        let current = capture_complete_index_source(&TapeDrive::new(checked), label, self.budget)?;
        Ok(ReclaimFormatOutcome::Committed(
            ReclaimFormatCommitEvidence::from_current(plan, self.key.round(), &current)
                .map_err(error)?,
        ))
    }
    fn verify_completed_reset(
        &self,
        checked: &dyn TapeTransport,
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
    ) -> Result<ReclaimFormatOutcome> {
        let options = self.options(reservation, plan);
        let head = capture_empty_reset_head(
            checked,
            &options,
            plan.formatted_at(),
            self.budget.max_index_bytes,
        )?;
        if !mam_complete(checked, plan, reservation.spec().barcode())? {
            return Err(refused("初始化后未读回完整固定空卷，保留原尝试等待核实"));
        }
        Ok(ReclaimFormatOutcome::Committed(
            ReclaimFormatCommitEvidence::from_verified_reset(
                plan,
                self.key.round(),
                VerifiedEmptyReset(head),
            )
            .map_err(error)?,
        ))
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
        let timing = FormatAuthorityTiming::new(reservation, plan, "空卷写入");
        let checked = Guard::new(self.device, || {
            self.authorize_cdb(reservation, plan, false, &timing)
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
        drop(checked);
        drop(timing);
        self.observe_guarded(reservation, plan, true, |checked, _| {
            self.verify_completed_reset(checked, reservation, plan)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::volume::read_label_pair;
    use crate::scsi::cdb::opcode;
    use crate::scsi::device::ScsiResult;
    use crate::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};
    use std::cell::RefCell;

    const TIME: &str = "2026-11-09T01:02:03.123456789Z";

    fn empty_reset() -> (SimTransport, MkltfsOptions) {
        let library = SimLibrary::new(1, 2, 1);
        library
            .insert_cartridge(SimCartridge::blank("RSET02L8", 64 << 20), 0)
            .unwrap();
        library.load_into_drive("RSET02L8", 0).unwrap();
        let device = library.drive(0);
        crate::ltfs::mkltfs::mkltfs(&device, &MkltfsOptions::default()).unwrap();
        let options = MkltfsOptions {
            volume_id: "RSET02".into(),
            volume_uuid: Some(uuid::Uuid::new_v4()),
            quick: true,
            ..Default::default()
        };
        reset_ltfs_at(&device, &options, TIME).unwrap();
        (device, options)
    }

    struct Trace<'a> {
        device: &'a SimTransport,
        locates: RefCell<Vec<(u8, bool, u64)>>,
        bad_position: bool,
    }

    impl TapeTransport for Trace<'_> {
        fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
            if cdb[0] == opcode::LOCATE_16 {
                let block = u64::from_be_bytes(cdb[4..12].try_into().unwrap());
                self.locates
                    .borrow_mut()
                    .push((cdb[3], cdb[1] & 0x18 == 0x18, block));
            }
            self.device.execute_no_data(cdb, timeout)
        }

        fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
            let result = self.device.execute_read(cdb, data, timeout)?;
            if self.bad_position && cdb[0] == opcode::READ_POSITION {
                data[0] |= 0x02;
            }
            Ok(result)
        }

        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
            panic!("空卷验证不能写带")
        }

        fn identity(&self) -> &str {
            self.device.identity()
        }
    }

    #[test]
    fn empty_reset_observation_matches_generic_capture_in_one_partition_pass() {
        let (device, options) = empty_reset();
        let trace = Trace {
            device: &device,
            locates: RefCell::default(),
            bad_position: false,
        };
        let budget = RecoveryBudget::default();
        let start = device.command_log().len();
        let exact =
            capture_empty_reset_head(&trace, &options, TIME, budget.max_index_bytes).unwrap();
        let log = device.command_log();
        let reads = &log[start..];
        assert_eq!(
            *trace.locates.borrow(),
            [(0, false, 0), (0, true, 0), (1, false, 0), (1, true, 0)]
        );
        assert_eq!(reads.iter().filter(|&&op| op == opcode::READ_6).count(), 16);
        assert!(!reads.contains(&opcode::SPACE_6));
        let label = read_label_pair(&device).unwrap();
        let generic =
            capture_complete_index_source(&TapeDrive::new(&device), &label, &budget).unwrap();
        assert_eq!(exact, generic.construct());
        assert_eq!(generic.construct_end(), FIRST_INDEX_BLOCK + 2);
        assert!(generic.predecessors().is_empty());
    }

    #[test]
    fn empty_reset_observation_rejects_incomplete_or_wrong_media() {
        for bad_record in ["extra", "short", "changed"] {
            let (device, options) = empty_reset();
            let drive = TapeDrive::new(&device);
            if bad_record == "extra" {
                drive.locate_eod(1).unwrap();
            } else {
                let records = reset_partition_records(&options, TIME, 1).unwrap();
                let mut corrupted = records[FIRST_INDEX_BLOCK as usize].clone().unwrap();
                if bad_record == "short" {
                    corrupted.pop();
                } else {
                    corrupted[0] ^= 1;
                }
                drive.locate(1, FIRST_INDEX_BLOCK, true).unwrap();
                drive.write_block(&corrupted).unwrap();
            }
            drive.write_filemark(1).unwrap();
            assert!(
                capture_empty_reset_head(&device, &options, TIME, usize::MAX).is_err(),
                "unexpected media change: {bad_record}"
            );
        }
        let (device, options) = empty_reset();
        let trace = Trace {
            device: &device,
            locates: RefCell::default(),
            bad_position: true,
        };
        assert!(capture_empty_reset_head(&trace, &options, TIME, usize::MAX).is_err());
        assert_eq!(*trace.locates.borrow(), [(0, false, 0), (0, true, 0)]);
        assert!(capture_empty_reset_head(&device, &options, TIME, 1).is_err());
    }

    #[test]
    fn empty_reset_observation_latches_unit_attention() {
        let (device, options) = empty_reset();
        device.inject(Fault {
            opcode: opcode::LOCATE_16,
            action: FaultAction::CheckCondition {
                sense_key: 6,
                asc: 0x28,
                ascq: 0,
            },
            after: 1,
            remaining: 1,
        });
        let guard = Guard::new_bracketed_read(&device, || Ok(()));
        let start = device.command_log().len();
        assert!(capture_empty_reset_head(&guard, &options, TIME, usize::MAX).is_err());
        assert!(guard.verify_proof().is_err());
        assert_eq!(
            device.command_log()[start..]
                .iter()
                .filter(|&&op| op == opcode::LOCATE_16)
                .count(),
            2,
            "首次 UA 以后不再向驱动器发送定位命令"
        );
    }
}
