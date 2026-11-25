//! Read-only physical facts for an original persisted reclaim attempt.
//! No cancellation, catalog mutation, writable volume or repair capability.
use crate::daemon::executor::OriginalAttemptProbeIo;
use crate::error::{Result, TapeError};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::{
    self, CapturedIndexSource, IndexCandidate, IndexSourceIdentity, IndexSourceScope,
    RecoveryBudget, RecoveryReport, TailKind,
};
use crate::ltfs::volume::{CommittedIndexConstruct, read_label_pair};
use crate::managed_catalog::OriginalAttemptObservation;
use crate::scsi::reservation::ReservationKey;
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;
use uuid::Uuid;

pub(in crate::daemon) struct CapturedAttemptEvidence {
    binding: AttemptBinding,
    facts: PhysicalFacts,
}
impl CapturedAttemptEvidence {
    pub(in crate::daemon) fn label(&self) -> &LtfsLabel {
        &self.facts.label
    }
    pub(in crate::daemon) fn report(&self) -> &RecoveryReport {
        &self.facts.report
    }
    pub(in crate::daemon) fn base(&self) -> &CapturedIndexSource {
        &self.facts.base
    }
    /// A captured current source is a physical fact, never an S4 verdict.
    /// The report retains IP debt and every original tail/chain restriction.
    pub(in crate::daemon) fn current(&self) -> Option<&CapturedIndexSource> {
        self.facts.current.as_ref()
    }
    pub(in crate::daemon) fn require_scope(
        &self,
        scope: &OriginalAttemptObservation,
    ) -> Result<()> {
        if self.binding.scope != ScopeBinding::from_scope(scope) {
            return Err(restricted("原attempt观察证据不属于当前完整持久范围"));
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
struct ScopeBinding {
    digest: [u8; 32],
    parent: serde_json::Value,
    namespace: Uuid,
    group: Uuid,
    attempt: Uuid,
    batch: Uuid,
    owner: u64,
    issuing_epoch: u64,
    drive: String,
    volume: Uuid,
    barcode: String,
    base: Vec<u8>,
    base_eod: u64,
}
impl ScopeBinding {
    fn from_scope(scope: &OriginalAttemptObservation) -> Self {
        Self {
            digest: scope.binding_digest(),
            parent: scope.parent().intent().clone(),
            namespace: scope.namespace(),
            group: scope.group_id(),
            attempt: scope.attempt_id(),
            batch: scope.batch_id(),
            owner: scope.owner(),
            issuing_epoch: scope.issuing_execution_epoch(),
            drive: scope.destination_drive_serial().into(),
            volume: scope.destination_volume(),
            barcode: scope.destination_barcode().into(),
            base: scope.base_source().to_bytes(),
            base_eod: scope.base_dp_eod(),
        }
    }
}
struct AttemptBinding {
    scope: ScopeBinding,
    device: String,
    key: ReservationKey,
}
struct PhysicalFacts {
    label: LtfsLabel,
    report: RecoveryReport,
    base: CapturedIndexSource,
    current: Option<CapturedIndexSource>,
}

fn restricted(reason: impl Into<String>) -> TapeError {
    TapeError::RecoveryRestricted {
        reason: reason.into(),
    }
}

/// The sole production entry takes the executor's read-only original-attempt
/// capability, including after takeover with an older immutable issuing epoch.
pub(in crate::daemon) async fn probe_original_attempt(
    io: &OriginalAttemptProbeIo<'_>,
    budget: &RecoveryBudget,
) -> Result<CapturedAttemptEvidence> {
    let scope = io.scope();
    if !io.key().is_ours()
        || io.key().round() != scope.owner()
        || io.transport().execution_guard().map(|guard| guard.0) != Some(scope.owner())
    {
        return Err(restricted("原attempt探测缺少当前owner/PR/多数派设备守卫"));
    }
    let binding = AttemptBinding {
        scope: ScopeBinding::from_scope(scope),
        device: io.transport().identity().into(),
        key: io.key(),
    };
    io.verify()?;
    let facts = capture_facts(
        io.transport(),
        scope.destination_volume(),
        scope.base_source(),
        scope.base_dp_eod(),
        budget,
    )?;
    // Positioning during a fresh probe legitimately advances the handle epoch.
    // Final authority checks only read PR; they cannot renew a changed device.
    finalize_probe_epoch(io.transport(), || io.verify())?;
    if io.transport().identity() != binding.device
        || io.key() != binding.key
        || ScopeBinding::from_scope(io.scope()) != binding.scope
    {
        return Err(restricted("原attempt探测收尾期间设备、轮次或持久范围改变"));
    }
    Ok(CapturedAttemptEvidence { binding, facts })
}

// The production final gate is private: tests can drive the device epoch and
// final verification outcome without creating an opaque scope or IO permit.
fn finalize_probe_epoch(
    device: &dyn TapeTransport,
    verify: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let completed_epoch = device
        .session_epoch()
        .ok_or_else(|| restricted("原attempt探测收尾缺少设备epoch"))?;
    verify()?;
    if device.session_epoch() != Some(completed_epoch) {
        return Err(restricted("原attempt探测最终核验期间设备epoch改变"));
    }
    Ok(())
}

// Private read algorithm, exercised on simulated media without minting a scope
// or bypassing the opaque production capability. It exposes no writable state.
fn capture_facts(
    device: &dyn TapeTransport,
    volume: Uuid,
    expected_base: &IndexSourceIdentity,
    base_eod: u64,
    budget: &RecoveryBudget,
) -> Result<PhysicalFacts> {
    let expected = expected_base.head();
    if expected.volume_uuid != volume
        || expected.location.partition != crate::ltfs::label::PART_DATA
        || expected.location.start_block >= base_eod
    {
        return Err(restricted("原attempt的base卷、数据分区或EOD范围非法"));
    }
    let label = read_label_pair(device)?;
    if label.volume_uuid != volume {
        return Err(restricted("原attempt探测装入了其他卷"));
    }
    let mut report = recovery::recover(device, &label, budget)?;
    require_report(&report)?;
    let before = ReportStamp::from_report(&report);
    let base = recovery::capture_recorded_index_source(
        &TapeDrive::new(device),
        &label,
        expected_base,
        budget,
    )?;
    if base.scope() != IndexSourceScope::Recorded
        || base.identity()?.to_bytes() != expected_base.to_bytes()
        || base.construct_end() != base_eod
        || base_eod > report.dp.eod
    {
        return Err(restricted(
            "原attempt的Recorded base完整身份或实际闭合EOD不符",
        ));
    }
    // Dirty T1/T2 cannot enter the clean-current helper. IP damage/debt stays
    // visible in report; current Some is never classified as committed here.
    let current = if report.dp.tail == TailKind::Complete
        && report.ip.tail == TailKind::Complete
        && !report.ip_ahead()
    {
        let current =
            recovery::capture_complete_index_source(&TapeDrive::new(device), &label, budget)?;
        require_current(&current, &report)?;
        Some(current)
    } else {
        None
    };
    let after = recovery::recover(device, &label, budget)?;
    require_report(&after)?;
    if before != ReportStamp::from_report(&after) {
        return Err(restricted("原attempt探测期间DP/IP卷头、尾部或EOD改变"));
    }
    if read_label_pair(device)?.to_xml()? != label.to_xml()? {
        return Err(restricted("原attempt探测期间双分区卷标改变"));
    }
    report
        .notes
        .push("原attempt探测仅返回物理事实；Current不构成完整S4、NoCommit或修尾授权".into());
    Ok(PhysicalFacts {
        label,
        report,
        base,
        current,
    })
}

fn require_report(report: &RecoveryReport) -> Result<()> {
    if let Some(reason) = report.restricted_reason() {
        return Err(restricted(reason));
    }
    if report.dp.partition != 1
        || report.ip.partition != 0
        || report.dp.budget_exhausted
        || report.ip.budget_exhausted
        || !report.chain_ok
        || !report.history_checked
        || report.chain_truncated_by_budget
        || report.dp.last_index.is_none()
        || matches!(
            report.dp.tail,
            TailKind::FakeIndex | TailKind::Unknown | TailKind::EmptyData
        )
        || matches!(report.ip.tail, TailKind::FakeIndex | TailKind::Unknown)
        || report.ip_ahead()
    {
        return Err(restricted("原attempt探测缺少完整受界DP/IP尾部与原历史证明"));
    }
    let dp = report.dp.last_index.as_ref().expect("checked DP candidate");
    if dp
        .end_block
        .checked_add(1)
        .is_none_or(|end| end > report.dp.eod)
    {
        return Err(restricted("原attempt实际DP索引闭合范围超过EOD"));
    }
    Ok(())
}

fn require_current(current: &CapturedIndexSource, report: &RecoveryReport) -> Result<()> {
    let candidate = report
        .dp
        .last_index
        .as_ref()
        .ok_or_else(|| restricted("缺少实际DP卷头"))?;
    let IndexSourceScope::Current {
        dp_eod,
        ip_generation,
        history_truncated: false,
        ..
    } = current.scope()
    else {
        return Err(restricted("原attempt的current捕获不是完整Current证明"));
    };
    if current.construct() != candidate_construct(candidate)
        || current.construct_end() != report.dp.eod
        || dp_eod != report.dp.eod
        || candidate.end_block.checked_add(1) != Some(report.dp.eod)
        || ip_generation
            != report
                .ip
                .last_index
                .as_ref()
                .map(|index| index.index.generation)
    {
        return Err(restricted(
            "原attempt的current捕获与同次report原始卷头/链/闭合边界不符",
        ));
    }
    Ok(())
}

fn candidate_construct(candidate: &IndexCandidate) -> CommittedIndexConstruct {
    CommittedIndexConstruct {
        volume_uuid: candidate.index.volume_uuid,
        generation: candidate.index.generation,
        location: candidate.index.self_location,
        sha256: candidate.construct_sha256,
        incremental: candidate.index.incremental,
    }
}
#[derive(PartialEq, Eq)]
struct ReportStamp {
    dp: ScanStamp,
    ip: ScanStamp,
    chain_ok: bool,
    history_checked: bool,
    chain_depth: u32,
    incremental_depth: u32,
    chain_truncated: bool,
    ip_debt: bool,
    append_ok: bool,
    append_position: Option<u64>,
}
#[derive(PartialEq, Eq)]
struct ScanStamp {
    partition: u8,
    eod: u64,
    tail: TailKind,
    budget_exhausted: bool,
    candidate: Option<(CommittedIndexConstruct, u64, u64)>,
}
impl ScanStamp {
    fn from_scan(scan: &recovery::PartitionScan) -> Self {
        Self {
            partition: scan.partition,
            eod: scan.eod,
            tail: scan.tail,
            budget_exhausted: scan.budget_exhausted,
            candidate: scan
                .last_index
                .as_ref()
                .map(|c| (candidate_construct(c), c.start_block, c.end_block)),
        }
    }
}
impl ReportStamp {
    fn from_report(report: &RecoveryReport) -> Self {
        Self {
            dp: ScanStamp::from_scan(&report.dp),
            ip: ScanStamp::from_scan(&report.ip),
            chain_ok: report.chain_ok,
            history_checked: report.history_checked,
            chain_depth: report.chain_depth,
            incremental_depth: report.incremental_depth,
            chain_truncated: report.chain_truncated_by_budget,
            ip_debt: report.ip_debt,
            append_ok: report.append_ok,
            append_position: report.append_position,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::ltfs::volume::LtfsVolume;
    use crate::scsi::cdb::opcode;
    use crate::scsi::sim::{
        Fault, FaultAction, LogicalObject, SimCartridge, SimLibrary, SimTransport,
    };
    use std::path::PathBuf;

    struct EpochDevice(std::cell::Cell<Option<u64>>);
    impl TapeTransport for EpochDevice {
        fn session_epoch(&self) -> Option<u64> {
            self.0.get()
        }
        fn identity(&self) -> &str {
            "probe-final-epoch-test"
        }
        fn execute_no_data(&self, _: &[u8], _: u32) -> Result<crate::scsi::device::ScsiResult> {
            panic!("private final epoch gate must not issue a CDB")
        }
        fn execute_read(
            &self,
            _: &[u8],
            _: &mut [u8],
            _: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            panic!("private final epoch gate must not issue a CDB")
        }
        fn execute_write(
            &self,
            _: &[u8],
            _: &[u8],
            _: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            panic!("private final epoch gate must not issue a CDB")
        }
    }

    #[test]
    fn final_epoch_none_to_none_never_releases_probe_evidence() {
        let device = EpochDevice(std::cell::Cell::new(None));
        let verified = std::cell::Cell::new(false);
        let result = finalize_probe_epoch(&device, || {
            verified.set(true);
            Ok(())
        });
        assert!(result.is_err());
        assert!(
            !verified.get(),
            "missing epoch must reject before final verification"
        );
    }

    #[test]
    fn final_epoch_change_during_authority_verification_is_rejected() {
        let device = EpochDevice(std::cell::Cell::new(Some(31)));
        assert!(
            finalize_probe_epoch(&device, || {
                device.0.set(Some(32));
                Ok(())
            })
            .is_err()
        );
    }

    #[test]
    fn final_epoch_disappearing_during_authority_verification_is_rejected() {
        let device = EpochDevice(std::cell::Cell::new(Some(31)));
        assert!(
            finalize_probe_epoch(&device, || {
                device.0.set(None);
                Ok(())
            })
            .is_err()
        );
    }

    #[test]
    fn final_epoch_stable_some_requires_actual_final_verification() {
        let device = EpochDevice(std::cell::Cell::new(Some(31)));
        let verified = std::cell::Cell::new(false);
        finalize_probe_epoch(&device, || {
            verified.set(true);
            Ok(())
        })
        .unwrap();
        assert!(verified.get());
    }

    #[test]
    fn final_epoch_stability_cannot_hide_final_authority_failure() {
        let device = EpochDevice(std::cell::Cell::new(Some(31)));
        let result = finalize_probe_epoch(&device, || {
            Err(TapeError::NotReady("test authority lost".into()))
        });
        assert!(
            matches!(result, Err(TapeError::NotReady(reason)) if reason == "test authority lost")
        );
    }

    const BARCODE: &str = "PROBE01L8";
    struct Case {
        lib: SimLibrary,
        dev: SimTransport,
        volume: Uuid,
        base: IndexSourceIdentity,
        eod: u64,
        budget: RecoveryBudget,
        scratch: PathBuf,
    }
    impl Drop for Case {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.scratch);
        }
    }
    impl Case {
        fn new() -> Self {
            let scratch = std::env::temp_dir().join(format!("attempt-probe-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&scratch).unwrap();
            let mut budget = RecoveryBudget::default();
            budget.index_scratch.directory = scratch.clone();
            budget.index_scratch.max_bytes = 64 << 20;
            budget.index_scratch.headroom_bytes = 0;
            let lib = SimLibrary::new(1, 2, 1);
            lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
                .unwrap();
            lib.load_into_drive(BARCODE, 0).unwrap();
            let dev = lib.drive(0);
            mkltfs(
                &dev,
                &MkltfsOptions {
                    block_size: 64 << 10,
                    ..Default::default()
                },
            )
            .unwrap();
            let label = read_label_pair(&dev).unwrap();
            let captured =
                recovery::capture_complete_index_source(&TapeDrive::new(&dev), &label, &budget)
                    .unwrap();
            let base = captured.identity().unwrap();
            let eod = captured.construct_end();
            drop(captured);
            Self {
                lib,
                dev,
                volume: label.volume_uuid,
                base,
                eod,
                budget,
                scratch,
            }
        }
        fn probe(&self) -> Result<PhysicalFacts> {
            capture_facts(&self.dev, self.volume, &self.base, self.eod, &self.budget)
        }
        fn assert_read_only(&self, before: usize) {
            let commands = self.dev.command_log();
            assert!(!commands[before..].iter().any(|op| matches!(
                *op,
                0x04 | 0x0a | 0x10 | 0x15 | 0x19 | 0x1b | 0x55 | 0x5f | 0x8d | 0xa5 | 0xa6
            )));
        }
        fn add_tail(&self, objects: Vec<LogicalObject>) {
            assert!(self.lib.with_cartridge_mut(BARCODE, |cart| {
                cart.partitions[1].objects.extend(objects);
                cart.partitions[1].flushed = cart.partitions[1].objects.len();
                cart.vcr += 1;
            }));
        }
    }

    #[test]
    fn exact_clean_base_is_read_from_media_without_any_write_or_business_state() {
        let case = Case::new();
        let media_before = case.lib.cartridge(BARCODE).unwrap();
        let before = case.dev.command_log().len();
        let facts = case.probe().unwrap();
        assert_eq!(facts.base.identity().unwrap(), case.base);
        assert_eq!(facts.base.construct_end(), case.eod);
        assert_eq!(facts.base.scope(), IndexSourceScope::Recorded);
        assert_eq!(facts.report.dp.tail, TailKind::Complete);
        assert!(facts.current.is_some());
        case.assert_read_only(before);
        let media_after = case.lib.cartridge(BARCODE).unwrap();
        assert_eq!(media_before.mam, media_after.mam);
        for (before, after) in media_before.partitions.iter().zip(&media_after.partitions) {
            assert_eq!(before.objects, after.objects);
        }
    }

    #[test]
    fn t1_and_t2_keep_exact_recorded_base_and_never_force_clean_current_or_repair() {
        for tail in [TailKind::UnindexedData, TailKind::TruncatedIndex] {
            let case = Case::new();
            let records = if tail == TailKind::UnindexedData {
                vec![LogicalObject::Record(b"uncommitted-body".to_vec())]
            } else {
                vec![
                    LogicalObject::Filemark,
                    LogicalObject::Record(b"<?xml version=\"1.0\"?><ltfsindex".to_vec()),
                ]
            };
            case.add_tail(records);
            let before = case.dev.command_log().len();
            let facts = case.probe().unwrap();
            assert_eq!(facts.report.dp.tail, tail, "{:?}", facts.report.dp.notes);
            assert_eq!(facts.base.identity().unwrap(), case.base);
            assert!(facts.report.dp.eod > case.eod);
            assert!(facts.current.is_none());
            case.assert_read_only(before);
        }
    }

    #[test]
    fn unexpected_complete_head_is_only_observed_not_guessed_from_absent_actual_or_header() {
        let case = Case::new();
        let mut volume = LtfsVolume::mount(&case.dev).unwrap();
        volume.append_bytes("new-body", b"new contents").unwrap();
        volume.commit().unwrap();
        drop(volume);
        let before = case.dev.command_log().len();
        let facts = case.probe().unwrap();
        assert_eq!(facts.base.identity().unwrap(), case.base);
        assert_eq!(
            facts.current.unwrap().construct().generation,
            case.base.head().generation + 1
        );
        case.assert_read_only(before);
    }

    #[test]
    fn incremental_head_keeps_original_chain_and_ip_debt_instead_of_an_s4_verdict() {
        let mut case = Case::new();
        let mut volume = LtfsVolume::mount(&case.dev).unwrap();
        volume
            .append_bytes("delta-body", b"incremental contents")
            .unwrap();
        volume.commit_incremental().unwrap();
        drop(volume);
        let label = read_label_pair(&case.dev).unwrap();
        let captured = recovery::capture_complete_index_source(
            &TapeDrive::new(&case.dev),
            &label,
            &case.budget,
        )
        .unwrap();
        case.base = captured.identity().unwrap();
        case.eod = captured.construct_end();
        assert!(case.base.head().incremental);
        assert!(!case.base.predecessors().is_empty());
        drop(captured);
        let before = case.dev.command_log().len();
        let facts = case.probe().unwrap();
        assert!(facts.report.ip_debt);
        assert_eq!(facts.base.identity().unwrap(), case.base);
        assert_eq!(facts.current.unwrap().identity().unwrap(), case.base);
        case.assert_read_only(before);
    }

    #[test]
    fn wrong_volume_exact_eod_and_predecessor_sha_fail_closed_without_writes() {
        let mut case = Case::new();
        let before = case.dev.command_log().len();
        assert!(
            capture_facts(
                &case.dev,
                Uuid::new_v4(),
                &case.base,
                case.eod,
                &case.budget
            )
            .is_err()
        );
        assert_eq!(case.dev.command_log().len(), before);
        assert!(
            capture_facts(
                &case.dev,
                case.volume,
                &case.base,
                case.eod + 1,
                &case.budget
            )
            .is_err()
        );
        case.assert_read_only(before);
        let mut volume = LtfsVolume::mount(&case.dev).unwrap();
        volume.append_bytes("delta", b"data").unwrap();
        volume.commit_incremental().unwrap();
        drop(volume);
        let label = read_label_pair(&case.dev).unwrap();
        let capture = recovery::capture_complete_index_source(
            &TapeDrive::new(&case.dev),
            &label,
            &case.budget,
        )
        .unwrap();
        let actual = capture.identity().unwrap();
        let mut predecessors = actual.predecessors().to_vec();
        predecessors[0].sha256[0] ^= 1;
        case.base = IndexSourceIdentity::new(actual.head(), predecessors).unwrap();
        case.eod = capture.construct_end();
        drop(capture);
        let before = case.dev.command_log().len();
        assert!(case.probe().is_err());
        case.assert_read_only(before);
    }

    #[test]
    fn fake_or_unknown_tail_and_exhausted_budget_cannot_create_evidence() {
        let case = Case::new();
        let mut budget = case.budget.clone();
        budget.max_index_bytes = 1;
        let before = case.dev.command_log().len();
        assert!(capture_facts(&case.dev, case.volume, &case.base, case.eod, &budget).is_err());
        case.assert_read_only(before);
        let index = LtfsVolume::mount(&case.dev).unwrap().index().clone();
        case.add_tail(vec![
            LogicalObject::Filemark,
            LogicalObject::Record(index.to_xml().unwrap().to_vec()),
            LogicalObject::Filemark,
        ]);
        let before = case.dev.command_log().len();
        assert!(case.probe().is_err());
        case.assert_read_only(before);
        let case = Case::new();
        assert!(case.lib.with_cartridge_mut(BARCODE, |cart| {
            cart.partitions[1].objects.truncate(3);
            cart.partitions[1].flushed = 3;
            cart.vcr += 1;
        }));
        let before = case.dev.command_log().len();
        assert!(case.probe().is_err());
        case.assert_read_only(before);
    }

    #[test]
    fn read_host_error_and_read_unit_attention_do_not_return_partial_evidence() {
        for action in [
            FaultAction::HostError(7),
            FaultAction::CheckCondition {
                sense_key: 6,
                asc: 0x29,
                ascq: 0,
            },
        ] {
            let case = Case::new();
            case.dev.inject(Fault {
                opcode: opcode::READ_6,
                action,
                after: 0,
                remaining: 1,
            });
            let before = case.dev.command_log().len();
            assert!(case.probe().is_err());
            case.assert_read_only(before);
        }
    }
}
