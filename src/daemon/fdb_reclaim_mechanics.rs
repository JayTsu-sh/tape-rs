//! One durable reclaim mechanical command. Recovery only observes an issued
//! command; it never infers non-execution from a lost reply or a timeout.
use super::executor::fdb_operation_guard::Guard;
use super::permission::{DeviceActivityGuard, Permission, Phase};
use crate::changer::{
    commands::MediumChanger,
    element::{ElementAddressMap, ElementStatus},
};
use crate::error::{Result, TapeError};
use crate::managed_catalog::{
    ActiveLibraryRoute, ManagedCatalog, ReclaimMediaAction, ReclaimMediaCommand,
    ReclaimMediaObservation, ReclaimMediaPlan, ReclaimMediaPreparation, ReclaimReservation,
};
use crate::scsi::{
    cdb,
    reservation::{ReservationKey, verify_holder},
    transport::{TapeTransport, retry_unit_attention},
};
use crate::tape::commands::TapeDrive;
use std::time::Duration;

pub struct ReclaimMechanics<'a> {
    pub catalog: &'a ManagedCatalog,
    pub changer: &'a dyn TapeTransport,
    pub drive: &'a dyn TapeTransport,
    pub route: ActiveLibraryRoute,
    pub key: ReservationKey,
    pub permission: &'a Permission,
}
fn error(value: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!("原生回收机械任务: {value}"))
}
fn waiting() -> TapeError {
    TapeError::NotReady("原回收机械命令结果尚未核实，保留原 issue，禁止重放".into())
}
enum Readiness {
    Known(bool),
    Unknown,
    BecomingReady(TapeError),
}
impl Readiness {
    fn known(&self) -> Option<bool> {
        match self {
            Self::Known(ready) => Some(*ready),
            Self::Unknown | Self::BecomingReady(_) => None,
        }
    }
}

fn readiness(device: &dyn TapeTransport) -> Result<Readiness> {
    match retry_unit_attention("回收机械就绪观察", || {
        device.execute_no_data(&cdb::test_unit_ready(), 10_000)
    }) {
        Ok(_) => Ok(Readiness::Known(true)),
        Err(reason) => match &reason {
            TapeError::ScsiCommand {
                status: 2,
                sense_key: 2,
                asc,
                ascq,
                sense,
            } if sense.as_deref().and_then(|s| s.current_condition()) == Some((2, *asc, *ascq)) => {
                Ok(match (*asc, *ascq) {
                    (0x04, 0x01) => Readiness::BecomingReady(reason),
                    (0x04, 0x02) | (0x3a, 0) => Readiness::Known(false),
                    _ => Readiness::Unknown,
                })
            }
            _ => Err(reason),
        },
    }
}

// A fixed wait budget within one execute call, separate from SG_IO timeouts.
// An issued command is never subject to this retry schedule.
const LOAD_READINESS_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

enum PreIssue {
    Complete,
    Prepare(ReclaimMediaPreparation),
    WaitForLoad(TapeError),
}

pub(crate) enum LoadPreIssue {
    Ready,
    NeedsLoad,
    Pending,
    BecomingReady(TapeError),
}

fn prepare_before_issue(
    plan: &ReclaimMediaPlan,
    owner: u64,
    map: &ElementAddressMap,
    rows: &[ElementStatus],
    readiness: Readiness,
) -> Result<PreIssue> {
    let ready = match readiness {
        Readiness::BecomingReady(reason) if plan.action == ReclaimMediaAction::Load => {
            // Project the completed position only to check the current inventory.
            // A matching row does not prove readiness or authorize an issue.
            return match plan.condition(map, rows, Some(true), true) {
                Ok(true) => Ok(PreIssue::WaitForLoad(reason)),
                Ok(false) => Err(error(format!("原 Load 位置不再匹配: {reason}"))),
                Err(detail) => Err(error(format!("原 Load 库存核对失败: {detail}; {reason}"))),
            };
        }
        observation => observation.known(),
    };
    if plan.condition(map, rows, ready, true).map_err(error)? {
        return Ok(PreIssue::Complete);
    }
    ReclaimMediaPreparation::from_read(plan, owner, map, rows, ready)
        .map(PreIssue::Prepare)
        .map_err(error)
}

impl ReclaimMechanics<'_> {
    pub async fn prepare_layout(
        &self,
        reservation: &ReclaimReservation,
        source: crate::managed_catalog::ReclaimDriveBinding,
        destination: crate::managed_catalog::ReclaimDriveBinding,
    ) -> Result<crate::managed_catalog::ReclaimLayout> {
        self.scope(reservation, None).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.changer, || {
            futures_executor::block_on(self.scope(reservation, None))?;
            verify_holder(self.changer, self.key)
        });
        let mut robot = MediumChanger::new(&checked);
        let map = robot.load_address_map()?.clone();
        let rows = robot.read_all_status()?;
        let proof = crate::managed_catalog::ReclaimLayoutEvidence::from_read(
            reservation,
            self.key.round(),
            source,
            destination,
            &map,
            &rows,
        )
        .map_err(error)?;
        checked.verify_proof()?;
        self.catalog
            .prepare_reclaim_layout(reservation, self.key.round(), &proof)
            .await
            .map_err(error)
    }
    pub async fn formatted_source_home(
        &self,
        reservation: &ReclaimReservation,
        plan: &crate::managed_catalog::ReclaimFormatPlan,
        proof: &crate::managed_catalog::ReclaimFormatCommitEvidence,
    ) -> Result<crate::managed_catalog::ReclaimFormatHomeEvidence> {
        if plan.owner() != self.key.round() {
            return Err(error("归位证明不能跨 owner 复用旧初始化证明"));
        }
        self.scope(reservation, None).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let command = self
            .catalog
            .reclaim_confirmed_media(reservation, self.key.round())
            .await
            .map_err(error)?
            .ok_or_else(|| error("格式化后归位缺少原 MOVE 回执"))?;
        let (map, rows, _) = self.snapshot(reservation, command.plan(), None)?;
        let evidence = crate::managed_catalog::ReclaimFormatHomeEvidence::from_read(
            reservation,
            plan,
            proof,
            &command,
            &map,
            &rows,
        )
        .map_err(error)?;
        self.scope(reservation, None).await?;
        Ok(evidence)
    }
    async fn scope(
        &self,
        reservation: &ReclaimReservation,
        command: Option<&ReclaimMediaCommand>,
    ) -> Result<()> {
        let owner = self.key.round();
        let execution = Some((owner, self.permission as *const Permission as usize));
        if self.changer.execution_guard() != execution
            || self.drive.execution_guard() != execution
            || reservation.namespace() != self.route.route.namespace
            || !(self.permission.allows(owner, Phase::Execution)
                || self.permission.allows(owner, Phase::Serving))
            || self
                .catalog
                .active_library_owner(reservation.serial())
                .await
                .map_err(error)?
                != Some((self.route, owner))
        {
            return Err(error("原 owner、route 或多数派许可实例改变"));
        }
        if let Some(command) = command {
            self.catalog
                .verify_reclaim_media(reservation, owner, command)
                .await
                .map_err(error)?;
        } else {
            let parent = self
                .catalog
                .reserved_reclaim_status(reservation, owner)
                .await
                .map_err(error)?;
            if parent["owner_epoch"] != owner
                || !matches!(
                    parent["phase"].as_str(),
                    Some("reserved" | "copying" | "reformatting")
                )
            {
                return Err(error("原回收父任务不再持有执行权"));
            }
            if self
                .catalog
                .reclaim_media_pending(reservation, owner)
                .await
                .map_err(error)?
                .is_some()
            {
                return Err(error("须先核实原已发机械命令"));
            }
        }
        Ok(())
    }
    fn snapshot(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
        command: Option<&ReclaimMediaCommand>,
    ) -> Result<(ElementAddressMap, Vec<ElementStatus>, Readiness)> {
        let check = || futures_executor::block_on(self.scope(reservation, command));
        let changer = Guard::new(self.changer, || {
            check()?;
            verify_holder(self.changer, self.key)
        });
        let drive = Guard::new(self.drive, || {
            check()?;
            verify_holder(self.drive, self.key)
        });
        let mut robot = MediumChanger::new(&changer);
        let map = robot.load_address_map()?.clone();
        let rows = robot.read_all_status()?;
        if crate::scsi::inquiry::read_unit_serial(&drive).as_deref()
            != Some(plan.drive_serial.as_str())
        {
            drive.verify_proof()?;
            return Err(error("实际驱动器序列号与原计划不符"));
        }
        let ready = readiness(&drive)?;
        changer.verify_proof()?;
        drive.verify_proof()?;
        Ok((map, rows, ready))
    }
    pub async fn observe(
        &self,
        reservation: &ReclaimReservation,
        original: &ReclaimMediaCommand,
    ) -> Result<bool> {
        self.scope(reservation, Some(original)).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let (map, rows, ready) = self.snapshot(reservation, original.plan(), Some(original))?;
        let ready = ready.known();
        if !original
            .plan()
            .condition(&map, &rows, ready, true)
            .map_err(error)?
        {
            return Ok(false);
        }
        let proof =
            ReclaimMediaObservation::from_read(original, self.key.round(), &map, &rows, ready)
                .map_err(error)?;
        self.scope(reservation, Some(original)).await?;
        self.catalog
            .confirm_reclaim_media(reservation, self.key.round(), original, &proof)
            .await
            .map_err(error)?;
        Ok(true)
    }

    async fn preissue(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
    ) -> Result<(PreIssue, DeviceActivityGuard<'_>)> {
        self.scope(reservation, None).await?;
        let activity = self.permission.begin_device_activity(self.key.round())?;
        let (map, rows, ready) = self.snapshot(reservation, plan, None)?;
        let decision = prepare_before_issue(plan, self.key.round(), &map, &rows, ready)?;
        Ok((decision, activity))
    }

    // Observe only. The returned state carries no issue authority.
    pub(crate) async fn observe_load_preissue(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
    ) -> Result<LoadPreIssue> {
        if plan.action != ReclaimMediaAction::Load {
            return Err(error("仅原 Load 可并行观察就绪状态"));
        }
        if self
            .catalog
            .reclaim_media_pending(reservation, self.key.round())
            .await
            .map_err(error)?
            .is_some()
        {
            return Ok(LoadPreIssue::Pending);
        }
        Ok(match self.preissue(reservation, plan).await?.0 {
            PreIssue::Complete => LoadPreIssue::Ready,
            PreIssue::Prepare(_) => LoadPreIssue::NeedsLoad,
            PreIssue::WaitForLoad(reason) => LoadPreIssue::BecomingReady(reason),
        })
    }

    pub(crate) async fn wait_load_preissue(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
        mut reason: TapeError,
    ) -> Result<bool> {
        let mut observation = 0;
        loop {
            self.load_backoff(reservation, plan, observation, &reason)
                .await?;
            match self.observe_load_preissue(reservation, plan).await? {
                LoadPreIssue::Ready => return Ok(true),
                LoadPreIssue::NeedsLoad => return Ok(false),
                LoadPreIssue::Pending => return Err(waiting()),
                LoadPreIssue::BecomingReady(next) => reason = next,
            }
            observation += 1;
        }
    }

    async fn load_backoff(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
        observation: usize,
        reason: &TapeError,
    ) -> Result<()> {
        let operation = &reservation.intent()["operation"];
        let owner = self.key.round();
        let attempt = observation + 1;
        let Some(delay) = LOAD_READINESS_BACKOFFS.get(observation).copied() else {
            return Err(TapeError::NotReady(format!(
                "回收 {operation} owner={owner} 驱动 {} 磁带 {} 原 Load 发令前 {attempt} 次就绪观察后等待耗尽: {reason}",
                plan.drive_serial, plan.barcode
            )));
        };
        log::debug!(
            "回收 {operation} owner={owner} 驱动 {} 磁带 {} 原 Load 发令前就绪观察 {attempt}，等待 {delay:?}: {reason}",
            plan.drive_serial,
            plan.barcode
        );
        tokio::time::sleep(delay).await;
        Ok(())
    }

    pub async fn execute(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
    ) -> Result<()> {
        let mut observation = 0;
        let (proof, _activity) = loop {
            if let Some(original) = self
                .catalog
                .reclaim_media_pending(reservation, self.key.round())
                .await
                .map_err(error)?
            {
                if original.plan() != plan {
                    return Err(error("不能用新计划替换原未知机械命令"));
                }
                return if self.observe(reservation, &original).await? {
                    Ok(())
                } else {
                    Err(waiting())
                };
            }
            let (decision, activity) = self.preissue(reservation, plan).await?;
            match decision {
                PreIssue::Complete => return Ok(()),
                PreIssue::Prepare(proof) => break (proof, activity),
                PreIssue::WaitForLoad(reason) => {
                    drop(activity);
                    self.load_backoff(reservation, plan, observation, &reason)
                        .await?;
                    observation += 1;
                }
            }
        };
        let (original, granted) = self
            .catalog
            .issue_reclaim_media(reservation, self.key.round(), &proof)
            .await
            .map_err(error)?;
        if granted {
            let check = || futures_executor::block_on(self.scope(reservation, Some(&original)));
            let changer = Guard::new(self.changer, || {
                check()?;
                verify_holder(self.changer, self.key)
            });
            let drive = Guard::new(self.drive, || {
                check()?;
                verify_holder(self.drive, self.key)
            });
            let (map, rows, ready) = self.snapshot(reservation, plan, Some(&original))?;
            ReclaimMediaPreparation::from_read(plan, self.key.round(), &map, &rows, ready.known())
                .map_err(error)?;
            match plan.action {
                ReclaimMediaAction::Load => TapeDrive::new(&drive).load()?,
                ReclaimMediaAction::Unload => TapeDrive::new(&drive).unload()?,
                ReclaimMediaAction::Move => {
                    let mut robot = MediumChanger::new(&changer);
                    if robot.load_address_map()? != &plan.map {
                        return Err(error("发 MOVE 前原地址映射改变"));
                    }
                    robot.move_medium(plan.source, plan.destination)?;
                }
            }
            changer.verify_proof()?;
            drive.verify_proof()?;
        }
        if self.observe(reservation, &original).await? {
            Ok(())
        } else {
            Err(waiting())
        }
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use crate::changer::element::ElementType;
    use crate::scsi::{device::ScsiResult, sense::SenseInfo};
    use std::cell::Cell;

    /// A TUR-only fixture. An accidental LOAD/MOVE or any other CDB fails here.
    struct Tur {
        response: u8,
        asc: u8,
        ascq: u8,
        length: usize,
        not_ready_count: u8,
        calls: Cell<u8>,
    }

    impl Tur {
        fn new(response: u8, asc: u8, ascq: u8, length: usize, not_ready_count: u8) -> Self {
            Self {
                response,
                asc,
                ascq,
                length,
                not_ready_count,
                calls: Cell::new(0),
            }
        }
    }

    impl TapeTransport for Tur {
        fn identity(&self) -> &str {
            "sim-reclaim-tur-only"
        }

        fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
            assert_eq!(cdb, crate::scsi::cdb::test_unit_ready());
            assert_eq!(timeout_ms, 10_000);
            let calls = self.calls.get() + 1;
            self.calls.set(calls);
            if calls > self.not_ready_count {
                return Ok(ScsiResult {
                    status: 0,
                    sense: SenseInfo::from_bytes(&[]),
                    transferred: 0,
                    duration_ms: 0,
                });
            }
            let mut raw = [0u8; 18];
            raw[0] = self.response;
            raw[2] = 2;
            raw[7] = 10;
            raw[12] = self.asc;
            raw[13] = self.ascq;
            Err(TapeError::ScsiCommand {
                status: 2,
                sense_key: 2,
                asc: self.asc,
                ascq: self.ascq,
                sense: Some(Box::new(SenseInfo::from_bytes(&raw[..self.length]))),
            })
        }

        fn execute_read(&self, _: &[u8], _: &mut [u8], _: u32) -> Result<ScsiResult> {
            panic!("not a TUR")
        }

        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
            panic!("not a TUR")
        }
    }

    fn loaded_fixture() -> (ReclaimMediaPlan, Vec<ElementStatus>) {
        let plan = ReclaimMediaPlan {
            action: ReclaimMediaAction::Load,
            barcode: "TEST00L8".into(),
            volume: uuid::Uuid::from_u128(1),
            drive_serial: "DRIVE1".into(),
            source: 10,
            destination: 10,
            home: 100,
            map: ElementAddressMap {
                transport_start: 0,
                transport_count: 1,
                storage_start: 100,
                storage_count: 2,
                ie_start: 200,
                ie_count: 1,
                dt_start: 10,
                dt_count: 1,
            },
        };
        let row = ElementStatus {
            address: 10,
            element_type: ElementType::DataTransfer,
            full: true,
            accessible: Some(true),
            exception: false,
            asc: 0,
            ascq: 0,
            disabled: false,
            medium_type: Some(1),
            volume_tag: Some(plan.barcode.clone()),
            source_address: Some(plan.home),
            drive_id: Some(plan.drive_serial.clone()),
        };
        (plan, vec![row])
    }

    fn classify(device: &Tur, plan: &ReclaimMediaPlan, rows: &[ElementStatus]) -> Result<PreIssue> {
        let checked = Guard::new(device, || Ok(()));
        let observed = readiness(&checked)?;
        checked.verify_proof()?;
        prepare_before_issue(plan, 7, &plan.map, rows, observed)
    }

    #[test]
    fn pre_issue_complete_current_becoming_ready_can_wait_for_original_load() {
        let (plan, rows) = loaded_fixture();
        let drive = Tur::new(0x70, 4, 1, 18, 2);
        for _ in 0..2 {
            let state =
                classify(&drive, &plan, &rows).expect("原 Load 未发令时，完整 02/04/01 应等待");
            let PreIssue::WaitForLoad(TapeError::ScsiCommand { sense, .. }) = state else {
                panic!("not-ready observation was mistaken for ready or issue evidence");
            };
            assert_eq!(sense.unwrap().current_condition(), Some((2, 4, 1)));
        }
        assert!(matches!(
            classify(&drive, &plan, &rows),
            Ok(PreIssue::Complete)
        ));
        assert_eq!(drive.calls.get(), 3);
    }
    #[test]
    fn pre_issue_keeps_other_sense_and_invalid_inventory_outside_wait() {
        let (mut plan, mut rows) = loaded_fixture();
        for (response, asc, ascq, length) in [(0x71, 4, 1, 18), (0x70, 4, 1, 13), (0x70, 4, 7, 18)]
        {
            let drive = Tur::new(response, asc, ascq, length, 1);
            assert!(classify(&drive, &plan, &rows).is_err());
            assert_eq!(drive.calls.get(), 1);
        }
        for (asc, ascq) in [(4, 2), (0x3a, 0)] {
            let drive = Tur::new(0x70, asc, ascq, 18, 1);
            assert!(matches!(
                classify(&drive, &plan, &rows),
                Ok(PreIssue::Prepare(_))
            ));
        }
        rows[0].source_address = Some(101);
        let drive = Tur::new(0x70, 4, 1, 18, 1);
        assert!(classify(&drive, &plan, &rows).is_err());
        rows[0].source_address = Some(plan.home);
        plan.action = ReclaimMediaAction::Unload;
        let drive = Tur::new(0x70, 4, 1, 18, 1);
        assert!(classify(&drive, &plan, &rows).is_err());
    }

    #[test]
    fn pre_issue_wait_budget_is_four_observations_and_thirty_seconds() {
        let mut delays = LOAD_READINESS_BACKOFFS.into_iter();
        assert_eq!(
            (0..4)
                .map(|_| delays.next().unwrap().as_secs())
                .sum::<u64>(),
            30
        );
        assert_eq!(delays.next(), None);
    }
}
