//! Destructive assignment on the sole guarded device thread. Unknown format
//! completion is resolved by reading the original UUID, never a second FORMAT.
use super::super::management as policy;
use super::fdb_move::Snapshot;
use super::fdb_observations::{
    Completion, Context, Publication, Publisher, bounded_error, publish,
};
use super::*;
use crate::managed_catalog::{
    ActiveIndexIdentity, AssignExecution, AssignPhase, AssignPlan, CatalogError, ManagementRequest,
};
use crate::scsi::device::ScsiResult;
use serde_json::Value;
use std::cell::Cell;

fn refused(detail: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: detail.into(),
    }
}
fn all_partitions_blank(device: &dyn TapeTransport) -> Result<bool> {
    let drive = TapeDrive::new(device);
    let mut buffer = vec![0; crate::ltfs::volume::DEFAULT_BLOCK_SIZE as usize];
    for partition in 0..drive.defined_partitions()? {
        drive.locate(partition, 0, true)?;
        match drive.read_block(&mut buffer) {
            Err(TapeError::ScsiCommand {
                sense_key: 0x08, ..
            }) => {}
            Err(error) => return Err(error),
            Ok(_) => return Ok(false),
        }
    }
    Ok(true)
}
fn plan(
    snapshot: &Snapshot,
    request: &ManagementRequest,
    revision: u64,
    status: &SharedStatus,
) -> Result<AssignPlan> {
    snapshot.robot()?;
    let barcode = request.value()["change"]["barcode"]
        .as_str()
        .expect("barcode");
    let source = snapshot.barcode(barcode)?;
    if source.is_cleaning() || source.element_type == ElementType::ImportExport {
        return Err(refused("分配拒绝清洗带及I/E槽磁带；数据带须先移回home"));
    }
    Snapshot::enabled(source, source.element_type != ElementType::DataTransfer)?;
    let home = match source.element_type {
        ElementType::Storage => source.address,
        ElementType::DataTransfer => source
            .source_address
            .ok_or_else(|| refused("已装带缺少可信home来源"))?,
        _ => return Err(refused("分配源不是存储槽或已知驱动器")),
    };
    let view = policy::policy_view(&status.lock().unwrap_or_else(|e| e.into_inner())).management;
    if !policy::tape_allowed(&view, barcode) {
        return Err(refused("原磁带被运维策略禁止"));
    }
    let drive = if source.element_type == ElementType::DataTransfer {
        source
    } else {
        snapshot
            .rows
            .iter()
            .filter(|e| e.element_type == ElementType::DataTransfer && !e.full)
            .filter(|e| Snapshot::enabled(e, true).is_ok())
            .filter(|e| {
                e.drive_id
                    .as_ref()
                    .is_some_and(|s| policy::drive_allowed(&view, s, 'm'))
            })
            .min_by_key(|e| e.address)
            .ok_or_else(|| refused("没有允许装载的空驱动器"))?
    };
    Snapshot::enabled(drive, source.address != drive.address)?;
    if !drive
        .drive_id
        .as_ref()
        .is_some_and(|s| policy::drive_allowed(&view, s, 'm'))
    {
        return Err(refused("原驱动器被运维策略禁止"));
    }
    let plan = AssignPlan {
        version: 1,
        barcode: barcode.into(),
        captured_at: snapshot.observation["observed_at"]
            .as_str()
            .expect("time")
            .into(),
        map: snapshot.map.clone(),
        source: Snapshot::location(source)?,
        drive: Snapshot::location(drive)?,
        home_address: home,
        pool_revision: revision,
    };
    plan.validate().map_err(|e| refused(e.to_string()))?;
    Ok(plan)
}
fn reconcile(snapshot: &Snapshot, plan: &AssignPlan) -> Result<bool> {
    if snapshot.map != plan.map {
        return Err(refused("地址map改变，保留原分配计划"));
    }
    snapshot.robot()?;
    let actual = snapshot.barcode(&plan.barcode)?;
    if actual.is_cleaning() {
        return Err(refused("原条码被识别为清洗带"));
    }
    Snapshot::enabled(actual, actual.element_type != ElementType::DataTransfer)?;
    let drive = snapshot.at(plan.drive.address)?;
    if Snapshot::location(drive)? != plan.drive {
        return Err(refused("原目标驱动器身份改变"));
    }
    if actual.address == plan.drive.address {
        if plan.source.address != plan.drive.address && snapshot.at(plan.source.address)?.full {
            return Err(refused("原源位置又被占用，须核对原装载结果"));
        }
        return Ok(true);
    }
    if actual.address != plan.source.address || drive.full {
        return Err(refused("原条码或原驱动器位置与分配计划冲突"));
    }
    Snapshot::enabled(drive, true)?;
    Ok(false)
}

/// Acknowledged task ownership and policy, FDB availability, local permission
/// and physical PR are checked before each mkltfs/probe command, including all
/// writes following a long FORMAT. This wraps the domain transport, not SG_IO.
struct DriveGuard<'a> {
    context: Context<'a>,
    runtime: &'a tokio::runtime::Runtime,
    request: &'a ManagementRequest,
    phase: AssignPhase,
    inner: &'a dyn TapeTransport,
    permanent: &'a Cell<bool>,
    drive_serial: &'a str,
}
impl DriveGuard<'_> {
    fn check(&self) -> Result<()> {
        match publish(
            &self.context,
            self.runtime,
            self.context.control.catalog.stage_media_assign(
                &self.context.control.serial,
                self.context.control.route,
                self.context.active.round,
                self.request,
                self.phase,
            ),
        ) {
            Publication::Confirmed(_) => {}
            Publication::Lost(e) => return Err(self.context.lost(e)),
            Publication::Paused => return Err(self.context.lost("原执行许可已失效".into())),
            Publication::Failed { detail, permanent } => {
                self.permanent.set(permanent);
                return Err(refused(format!("原分配命令未获FDB确认：{detail}")));
            }
        }
        let view = policy::policy_view(
            &self
                .context
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
        .management;
        if !policy::drive_allowed(&view, self.drive_serial, 'm')
            || !policy::tape_allowed(
                &view,
                self.request.value()["change"]["barcode"]
                    .as_str()
                    .expect("barcode"),
            )
        {
            return Err(refused("原磁带被运维策略禁止"));
        }
        reservation::verify_holder(self.inner, self.context.active.key)?;
        if !self.context.ready() {
            return Err(self.context.lost("设备命令前Serving许可失效".into()));
        }
        Ok(())
    }
}
impl TapeTransport for DriveGuard<'_> {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.check()?;
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.check()?;
        self.inner.execute_read(cdb, buf, timeout)
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.check()?;
        self.inner.execute_write(cdb, buf, timeout)
    }
}

pub(super) struct Job {
    request: ManagementRequest,
    plan: Option<AssignPlan>,
    execution: Option<AssignExecution>,
    load_attempted: bool,
    format_attempted: bool,
    proof: Option<ActiveIndexIdentity>,
    completion: Option<Completion>,
    publishing: bool,
    diagnostic: Option<(String, bool)>,
    reject: Option<String>,
    permanent: Cell<bool>,
    pub(super) evicted_drive: Option<usize>,
}
impl Job {
    pub(super) fn new(request: ManagementRequest, task: &Value) -> Result<Self> {
        let execution = if task["execution"].is_null() {
            None
        } else {
            Some(
                AssignExecution::parse(task["execution"].clone(), &request)
                    .map_err(|e| refused(e.to_string()))?,
            )
        };
        let load_attempted = execution.as_ref().is_some_and(|e| {
            matches!(
                e.phase,
                AssignPhase::Threading | AssignPhase::Inspecting | AssignPhase::Formatting
            )
        });
        let format_attempted = execution
            .as_ref()
            .is_some_and(|e| e.phase == AssignPhase::Formatting);
        Ok(Self {
            plan: execution.as_ref().map(|e| e.plan.clone()),
            request,
            execution,
            load_attempted,
            format_attempted,
            proof: None,
            completion: None,
            publishing: false,
            diagnostic: None,
            reject: None,
            permanent: Cell::new(false),
            evicted_drive: None,
        })
    }
    pub(super) fn diagnostic_identity(&self) -> String {
        format!(
            "{} 条码={} 新卷={} 原阶段={:?}",
            self.request.operation(),
            self.request.value()["change"]["barcode"],
            self.request.value()["change"]["volume"],
            self.execution.as_ref().map(|e| e.phase)
        )
    }
    fn accept(&mut self, task: Value) -> Result<bool> {
        if task["status"] != "running" {
            return Ok(true);
        }
        self.execution = Some(
            AssignExecution::parse(task["execution"].clone(), &self.request)
                .map_err(|e| refused(e.to_string()))?,
        );
        Ok(false)
    }
    pub(super) fn poll(
        &mut self,
        context: Context<'_>,
        publisher: &mut Publisher,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Publication<bool>> {
        let result = self.poll_inner(context, publisher, runtime);
        match result {
            Err(error) if ownership_lost(&error) => Err(error),
            Err(error) => {
                let detail = bounded_error(&error.to_string());
                if self.execution.is_none() {
                    self.reject = Some(detail);
                    return Ok(Publication::Confirmed(false));
                }
                let permanent = self.permanent.replace(false);
                self.diagnostic = Some((detail.clone(), permanent));
                Ok(if permanent {
                    Publication::Confirmed(false)
                } else {
                    Publication::Failed {
                        detail,
                        permanent: false,
                    }
                })
            }
            Ok(Publication::Failed { detail, permanent }) if self.execution.is_some() => {
                self.diagnostic = Some((bounded_error(&detail), permanent));
                Ok(if permanent {
                    Publication::Confirmed(false)
                } else {
                    Publication::Failed { detail, permanent }
                })
            }
            other => other,
        }
    }
    fn poll_inner(
        &mut self,
        context: Context<'_>,
        publisher: &mut Publisher,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Publication<bool>> {
        let c = &context.control.catalog;
        let serial = &context.control.serial;
        let route = context.control.route;
        let owner = context.active.round;
        if let Some(detail) = self.reject.clone() {
            return Ok(publish(
                &context,
                runtime,
                c.reject_media_assign(serial, route, owner, &self.request, &detail),
            )
            .map(|_| true));
        }
        if let Some((detail, permanent)) = self.diagnostic.clone() {
            return Ok(
                match publish(
                    &context,
                    runtime,
                    c.defer_media_assign(serial, route, owner, &self.request, &detail),
                ) {
                    Publication::Confirmed(_) => {
                        self.diagnostic = None;
                        Publication::Failed { detail, permanent }
                    }
                    Publication::Failed {
                        detail: current,
                        permanent: p,
                    } => Publication::Failed {
                        detail: format!("原诊断{detail}；保存失败{current}"),
                        permanent: p || permanent,
                    },
                    other => other.map(|_| false),
                },
            );
        }
        if self.plan.is_none() {
            let pool = uuid::Uuid::parse_str(
                self.request.value()["change"]["pool"]
                    .as_str()
                    .expect("pool"),
            )
            .expect("pool");
            let policy = match publish(
                &context,
                runtime,
                c.active_pool_for_route(serial, route, pool),
            ) {
                Publication::Confirmed(Some(pool)) => pool,
                Publication::Confirmed(None) => {
                    self.reject = Some("原分配池不存在".into());
                    return Ok(Publication::Confirmed(false));
                }
                other => return Ok(other.map(|_| false)),
            };
            self.plan = Some(plan(
                &Snapshot::capture(&context)?,
                &self.request,
                policy.revision,
                context.status,
            )?);
        }
        match publish(&context, runtime, async {
            match c
                .prepare_media_assign(
                    serial,
                    route,
                    owner,
                    &self.request,
                    self.plan.as_ref().expect("plan"),
                )
                .await
            {
                Err(error) if matches!(error.domain(), CatalogError::Invalid(_)) => {
                    Ok(Err(error.to_string()))
                }
                other => other.map(Ok),
            }
        }) {
            Publication::Confirmed(Ok(task)) => {
                if self.accept(task)? {
                    return Ok(Publication::Confirmed(true));
                }
            }
            Publication::Confirmed(Err(detail)) if self.execution.is_none() => {
                self.reject = Some(bounded_error(&detail));
                return Ok(Publication::Confirmed(false));
            }
            Publication::Confirmed(Err(detail)) => return Err(refused(detail)),
            other => return Ok(other.map(|_| false)),
        }
        if self.publishing {
            if self.completion.is_none() {
                publisher.poll(context, runtime)?;
                self.completion = publisher.take_completion();
            }
            let Some(completion) = &self.completion else {
                return Ok(Publication::Confirmed(false));
            };
            return Ok(publish(
                &context,
                runtime,
                c.complete_media_assign(
                    serial,
                    route,
                    owner,
                    &self.request,
                    self.proof.expect("proof"),
                    &completion.captured_at,
                ),
            )
            .map(|_| true));
        }
        let p = self.plan.clone().expect("plan");
        let snapshot = Snapshot::capture(&context)?;
        let loaded = reconcile(&snapshot, &p)?;
        let phase = self.execution.as_ref().expect("execution").phase;
        let view = policy::policy_view(&context.status.lock().unwrap_or_else(|e| e.into_inner()))
            .management;
        let drive = context
            .active
            .devices
            .iter()
            .position(|d| {
                d.kind == DeviceKind::Drive
                    && Some(d.serial.as_str()) == p.drive.drive_serial.as_deref()
            })
            .ok_or_else(|| refused("原驱动器不在配置中"))?;
        if !policy::drive_allowed(&view, &context.active.devices[drive].serial, 'm') {
            return Err(refused("原驱动器被运维策略禁止"));
        }
        self.evicted_drive = Some(drive);
        if !loaded {
            if !matches!(phase, AssignPhase::Prepared | AssignPhase::Moving) {
                return Err(refused("原任务已装带但介质不在原驱动器，禁止重装后格式化"));
            }
            match publish(
                &context,
                runtime,
                c.stage_media_assign(serial, route, owner, &self.request, AssignPhase::Moving),
            ) {
                Publication::Confirmed(task) => {
                    self.accept(task)?;
                }
                other => return Ok(other.map(|_| false)),
            }
            let fresh = Snapshot::capture(&context)?;
            if reconcile(&fresh, &p)? {
                return Ok(Publication::Confirmed(false));
            }
            let changer = context
                .active
                .devices
                .iter()
                .find(|d| d.kind == DeviceKind::Changer)
                .expect("verified changer");
            let mut changer = MediumChanger::new(changer.dev.as_ref());
            if changer.load_address_map()? != &p.map {
                return Err(refused("MOVE前地址map已改变，保留原分配计划"));
            }
            changer.move_medium(p.source.address, p.drive.address)?;
            return Ok(Publication::Confirmed(false));
        }
        if matches!(phase, AssignPhase::Prepared | AssignPhase::Moving) {
            match publish(
                &context,
                runtime,
                c.stage_media_assign(serial, route, owner, &self.request, AssignPhase::Threading),
            ) {
                Publication::Confirmed(task) => {
                    self.accept(task)?;
                }
                other => return Ok(other.map(|_| false)),
            }
            return Ok(Publication::Confirmed(false));
        }
        let inner = context.active.devices[drive].dev.as_ref();
        if phase == AssignPhase::Threading {
            let guard = DriveGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
            };
            if let Err(error) = TapeDrive::new(&guard).test_unit_ready() {
                if ownership_lost(&error) {
                    return Err(error);
                }
                if self.load_attempted || snapshot.at(p.drive.address)?.accessible != Some(true) {
                    return Err(refused(format!(
                        "原LOAD可能仍在执行，等待驱动器就绪：{error}"
                    )));
                }
                self.load_attempted = true;
                TapeDrive::new(&guard).load()?;
                return Ok(Publication::Confirmed(false));
            }
            return Ok(publish(
                &context,
                runtime,
                c.stage_media_assign(serial, route, owner, &self.request, AssignPhase::Inspecting),
            )
            .map(|task| {
                self.accept(task).expect("validated stage");
                false
            }));
        }
        if phase == AssignPhase::Inspecting {
            let guard = DriveGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
            };
            if TapeDrive::new(&guard).write_protected()? {
                self.reject = Some("磁带被写保护，未开始格式化".into());
                return Ok(Publication::Confirmed(false));
            }
            if self.request.value()["change"]["force_format"] != true {
                let safe = match crate::ltfs::volume::probe_format(&guard)? {
                    crate::ltfs::volume::FormatProbe::Blank => all_partitions_blank(&guard)?,
                    crate::ltfs::volume::FormatProbe::Ltfs => {
                        let volume = crate::ltfs::volume::LtfsVolume::mount(&guard)?;
                        volume.writable()
                            && volume.index().root.files.is_empty()
                            && volume.index().root.subdirs.is_empty()
                    }
                    _ => false,
                };
                if !safe {
                    self.reject=Some("磁带包含文件对象、非LTFS数据或受限索引；保留数据请用tape import，确认清除须--force-format".into());
                    return Ok(Publication::Confirmed(false));
                }
            }
            return Ok(publish(
                &context,
                runtime,
                c.stage_media_assign(serial, route, owner, &self.request, AssignPhase::Formatting),
            )
            .map(|task| {
                self.accept(task).expect("validated stage");
                false
            }));
        }
        if phase == AssignPhase::Formatting {
            let guard = DriveGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
            };
            let expected = uuid::Uuid::parse_str(
                self.request.value()["change"]["volume"]
                    .as_str()
                    .expect("volume"),
            )
            .expect("volume");
            if !self.format_attempted {
                self.format_attempted = true;
                info!(
                    "原生分配格式化 原任务={} 条码={} 新卷={} 驱动器={}",
                    self.request.operation(),
                    p.barcode,
                    expected,
                    context.active.devices[drive].serial
                );
                crate::ltfs::mkltfs::mkltfs(
                    &guard,
                    &crate::ltfs::mkltfs::MkltfsOptions {
                        volume_id: p.barcode.chars().take(6).collect(),
                        volume_uuid: Some(expected),
                        ..Default::default()
                    },
                )?;
            }
            TapeDrive::new(&guard).test_unit_ready()?;
            let mut volume = crate::ltfs::volume::LtfsVolume::mount(&guard)?;
            volume.set_reservation_guard(Some(context.active.key));
            let recovery = volume.recovery();
            if !volume.writable()
                || volume.label().volume_uuid != expected
                || !volume.index().root.files.is_empty()
                || !volume.index().root.subdirs.is_empty()
                || recovery.ip_debt
                || recovery.ip_ahead()
            {
                return Err(refused(
                    "原格式化结果未证明完整空卷，保留原UUID/检查点，禁止再次FORMAT",
                ));
            }
            let proof: ActiveIndexIdentity = volume.verified_committed_index_construct()?.into();
            if proof.volume_uuid != expected || proof.generation != 1 || proof.incremental {
                return Err(refused("原格式化卷UUID或初代索引不符，禁止再次FORMAT"));
            }
            let after = Snapshot::capture(&context)?;
            if !reconcile(&after, &p)? {
                return Err(refused("格式化后原物理位置尚未确认"));
            }
            self.proof = Some(proof);
            publisher.request_sample(owner, after.observation);
            self.publishing = true;
            return Ok(Publication::Confirmed(false));
        }
        Err(refused("分配任务执行阶段无效"))
    }
}

#[cfg(test)]
#[path = "executor_fdb_assign_tests.rs"]
mod tests;
