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
use std::cell::{Cell, RefCell};

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
fn nonforce_empty(
    device: &dyn TapeTransport,
    budget: &crate::ltfs::recovery::RecoveryBudget,
) -> Result<bool> {
    match crate::ltfs::volume::probe_format(device)? {
        crate::ltfs::volume::FormatProbe::Blank => all_partitions_blank(device),
        crate::ltfs::volume::FormatProbe::Ltfs => {
            let label = crate::ltfs::volume::read_label_only(device)?;
            crate::ltfs::recovery::probe_empty_volume(&TapeDrive::new(device), &label, budget)
        }
        crate::ltfs::volume::FormatProbe::Foreign(_) => Ok(false),
    }
}
/// Assignment proves the exact format it requested. Ordinary mount reads only
/// the IP label; that alone cannot certify a newly formatted two-partition volume.
fn verify_assigned_labels(
    device: &dyn TapeTransport,
    expected: uuid::Uuid,
    barcode: &str,
) -> Result<()> {
    use crate::ltfs::label::{
        LTFS_VERSION, LtfsLabel, PART_DATA, PART_INDEX, VOL1_IMPL_ID, Vol1Label,
    };
    let drive = TapeDrive::new(device);
    let mut buffer = vec![0; 64 * 1024];
    let mut canonical = None;
    for (partition, location) in [(0, PART_INDEX), (1, PART_DATA)] {
        drive.locate(partition, crate::ltfs::volume::VOL1_BLOCK, true)?;
        let n = drive.read_block(&mut buffer)?;
        let record = buffer.get(..n).ok_or(TapeError::InvalidResponse {
            expected: buffer.len(),
            actual: n,
        })?;
        let vol1 = Vol1Label::parse(record)?;
        if n != 80
            || &buffer[24..37] != VOL1_IMPL_ID
            || vol1.volume_id != barcode.chars().take(6).collect::<String>()
            || !vol1.owner.is_empty()
        {
            return Err(refused(format!("分区{partition}的VOL1与原格式化参数不符")));
        }
        if drive.read_block(&mut buffer)? != 0 {
            return Err(refused(format!("分区{partition}的VOL1缺少闭合文件标记")));
        }
        drive.locate(partition, crate::ltfs::volume::LABEL_BLOCK, true)?;
        let n = drive.read_block(&mut buffer)?;
        let record = buffer.get(..n).ok_or(TapeError::InvalidResponse {
            expected: buffer.len(),
            actual: n,
        })?;
        let mut label = LtfsLabel::parse(record)?;
        if label.volume_uuid != expected
            || label.location != location
            || label.index_partition != PART_INDEX
            || label.data_partition != PART_DATA
            || label.version != LTFS_VERSION
            || label.creator != crate::ltfs::mkltfs::APP_NAME
            || label.blocksize != crate::ltfs::volume::DEFAULT_BLOCK_SIZE
            || label.compression
        {
            return Err(refused(format!(
                "分区{partition}的LTFS标签与原卷UUID或格式参数不符"
            )));
        }
        if drive.read_block(&mut buffer)? != 0 {
            return Err(refused(format!(
                "分区{partition}的LTFS标签缺少闭合文件标记"
            )));
        }
        label.location = PART_INDEX;
        let normalized = label.to_xml()?;
        if canonical.as_ref().is_some_and(|first| first != &normalized) {
            return Err(refused("两分区LTFS标签公共字段不一致"));
        }
        canonical = Some(normalized);
    }
    Ok(())
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

fn send_move(
    context: Context<'_>,
    runtime: &tokio::runtime::Runtime,
    request: &ManagementRequest,
    p: &AssignPlan,
    permanent: &Cell<bool>,
) -> Result<()> {
    let changer = context
        .active
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .expect("verified changer");
    let guard = CommandGuard {
        context,
        runtime,
        request,
        phase: AssignPhase::Moving,
        inner: changer.dev.as_ref(),
        permanent,
        drive_serial: p.drive.drive_serial.as_deref().expect("drive serial"),
        probe_fault: None,
    };
    let mut changer = MediumChanger::new(&guard);
    if changer.load_address_map()? != &p.map {
        return Err(refused("MOVE前地址map已改变，保留原分配计划"));
    }
    changer.move_medium(p.source.address, p.drive.address)?;
    Ok(())
}

/// Acknowledged task ownership and policy, FDB availability, local permission
/// and physical PR are checked before each mechanical/mkltfs/probe command,
/// including all writes following a long FORMAT. This wraps the domain transport, not SG_IO.
struct CommandGuard<'a> {
    context: Context<'a>,
    runtime: &'a tokio::runtime::Runtime,
    request: &'a ManagementRequest,
    phase: AssignPhase,
    inner: &'a dyn TapeTransport,
    permanent: &'a Cell<bool>,
    drive_serial: &'a str,
    probe_fault: Option<&'a RefCell<Option<ProbeFault>>>,
}
struct ProbeFault {
    detail: String,
    lost: bool,
}
fn probe_read_completed(error: &TapeError) -> bool {
    matches!(error, TapeError::ScsiCommand {status:2,sense_key,asc,ascq,sense}
        if !matches!(sense_key,2|6|0x0b)
            && sense.as_deref().and_then(|s| s.current_condition())
                == Some((*sense_key,*asc,*ascq)))
}
impl CommandGuard<'_> {
    fn record_fault(&self, error: &TapeError, guard_failed: bool) {
        if !guard_failed && probe_read_completed(error) {
            return;
        }
        if let Some(cell) = self.probe_fault {
            let mut fault = cell.borrow_mut();
            let lost = ownership_lost(error);
            if fault.is_none() || lost {
                *fault = Some(ProbeFault {
                    detail: error.to_string(),
                    lost,
                });
            }
        }
    }
    fn invoke(&self, operation: impl FnOnce() -> Result<ScsiResult>) -> Result<ScsiResult> {
        if let Some(cell) = self.probe_fault
            && let Some(fault) = cell.borrow().as_ref()
        {
            return Err(if fault.lost {
                self.context.lost(fault.detail.clone())
            } else {
                refused(format!(
                    "本次只读检查已中断，保留原结果核对：{}",
                    fault.detail
                ))
            });
        }
        if let Err(error) = self.check() {
            self.record_fault(&error, true);
            return Err(error);
        }
        let result = operation();
        if let Err(error) = &result {
            self.record_fault(error, false);
        }
        result
    }
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
        for device in &self.context.active.devices {
            reservation::verify_holder(device.dev.as_ref(), self.context.active.key)?;
        }
        if !self.context.ready() {
            return Err(self.context.lost("设备命令前Serving许可失效".into()));
        }
        Ok(())
    }
}
impl TapeTransport for CommandGuard<'_> {
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
        self.invoke(|| self.inner.execute_no_data(cdb, timeout))
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(|| self.inner.execute_read(cdb, buf, timeout))
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(|| self.inner.execute_write(cdb, buf, timeout))
    }
}

pub(super) struct Job {
    request: ManagementRequest,
    plan: Option<AssignPlan>,
    execution: Option<AssignExecution>,
    load_attempted: bool,
    format_attempted: bool,
    proof: Option<crate::managed_catalog::NativeMediaSource>,
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
    pub(super) fn verified_mount(&self, active: &Active) -> Option<(usize, fdb_loading::Mount)> {
        if !self.publishing || self.completion.is_none() || self.reject.is_some() {
            return None;
        }
        let proof = self.proof.as_ref()?;
        let plan = self.plan.as_ref()?;
        let drive = active.devices.iter().position(|device| {
            device.kind == DeviceKind::Drive
                && Some(device.serial.as_str()) == plan.drive.drive_serial.as_deref()
        })?;
        let pool = self.request.value()["change"]["pool"]
            .as_str()?
            .parse()
            .ok()?;
        Some((
            drive,
            fdb_loading::Mount {
                barcode: plan.barcode.clone(),
                volume: proof.index().volume_uuid,
                pool,
                last_used: Instant::now(),
            },
        ))
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
        if let Some((detail, permanent)) = self.diagnostic.clone() {
            return Ok(
                match publish(
                    &context,
                    runtime,
                    context.control.catalog.defer_media_assign(
                        &context.control.serial,
                        context.control.route,
                        context.active.round,
                        &self.request,
                        &detail,
                    ),
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
                    self.proof.as_ref().expect("proof"),
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
            send_move(context, runtime, &self.request, &p, &self.permanent)?;
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
            let guard = CommandGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
                probe_fault: None,
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
            let probe_fault = RefCell::new(None);
            let guard = CommandGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
                probe_fault: Some(&probe_fault),
            };
            if TapeDrive::new(&guard).write_protected()? {
                self.reject = Some("磁带被写保护，未开始格式化".into());
                return Ok(Publication::Confirmed(false));
            }
            if self.request.value()["change"]["force_format"] != true {
                let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
                budget.index_scratch.directory = context.active.files.spool_dir().to_path_buf();
                budget.index_scratch.max_bytes = context.control.storage.index_bytes;
                budget.index_scratch.headroom_bytes = context.control.storage.headroom_bytes;
                let result = nonforce_empty(&guard, &budget);
                // Recovery may deliberately fall back after a failed read. A
                // transport/authority failure is never proof of invalid media,
                // even when that fallback returns a semantic error or a view.
                if let Some(fault) = probe_fault.into_inner() {
                    return Err(if fault.lost {
                        context.lost(fault.detail)
                    } else {
                        refused(format!("非强制索引检查结果未确认：{}", fault.detail))
                    });
                }
                let safe = match result {
                    Ok(safe) => safe,
                    Err(
                        error @ (TapeError::LocalCapacity { .. }
                        | TapeError::LocalSpace { .. }
                        | TapeError::Io(_)
                        | TapeError::CatalogDb(_)),
                    ) => return Err(error),
                    Err(error) => {
                        self.reject = Some(bounded_error(&format!(
                            "非强制检查未能证明安全空卷：{error}；未开始格式化"
                        )));
                        return Ok(Publication::Confirmed(false));
                    }
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
            let guard = CommandGuard {
                context,
                runtime,
                request: &self.request,
                phase,
                inner,
                permanent: &self.permanent,
                drive_serial: &context.active.devices[drive].serial,
                probe_fault: None,
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
            verify_assigned_labels(&guard, expected, &p.barcode)?;
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
            let baseline = fdb_intake::capture_empty_baseline(
                &guard,
                context.active.files.spool_dir(),
                context.control.storage,
            )?;
            if ActiveIndexIdentity::from(baseline.identity.head()) != proof {
                return Err(refused("空卷捕获来源与格式化读回索引不符"));
            }
            self.proof = Some(baseline);
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
