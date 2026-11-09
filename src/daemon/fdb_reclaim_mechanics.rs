//! One durable reclaim mechanical command. Recovery only observes an issued
//! command; it never infers non-execution from a lost reply or a timeout.
use super::executor::fdb_operation_guard::Guard;
use super::permission::{Permission, Phase};
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
fn readiness(device: &dyn TapeTransport) -> Result<Option<bool>> {
    match retry_unit_attention("回收机械就绪观察", || {
        device.execute_no_data(&cdb::test_unit_ready(), 10_000)
    }) {
        Ok(_) => Ok(Some(true)),
        Err(TapeError::ScsiCommand {
            status: 2,
            sense_key: 2,
            asc,
            ascq,
            sense,
        }) if sense.as_deref().and_then(|s| s.current_condition()) == Some((2, asc, ascq)) => {
            Ok(if matches!((asc, ascq), (0x04, 0x02) | (0x3a, 0)) {
                Some(false)
            } else {
                None
            })
        }
        Err(other) => Err(other),
    }
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
    ) -> Result<(ElementAddressMap, Vec<ElementStatus>, Option<bool>)> {
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
    pub async fn execute(
        &self,
        reservation: &ReclaimReservation,
        plan: &ReclaimMediaPlan,
    ) -> Result<()> {
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
        self.scope(reservation, None).await?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let (map, rows, ready) = self.snapshot(reservation, plan, None)?;
        if plan.condition(&map, &rows, ready, true).map_err(error)? {
            return Ok(());
        }
        let proof = ReclaimMediaPreparation::from_read(plan, self.key.round(), &map, &rows, ready)
            .map_err(error)?;
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
            ReclaimMediaPreparation::from_read(plan, self.key.round(), &map, &rows, ready)
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
