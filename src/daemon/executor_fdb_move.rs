//! Explicit mechanical maintenance on the existing guarded device thread.
//! A persisted phase is an intent, never proof that a CDB did not execute.
use super::fdb_observations::{Context, Publication, Publisher, bounded_error, publish};
use super::*;
use crate::changer::element::{ElementAddressMap, ElementStatus};
use crate::managed_catalog::{
    CatalogError, ManagementRequest, MoveExecution, MoveLocation, MoveLocationKind, MovePhase,
    MovePlan,
};
use serde_json::Value;

fn refused(detail: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: detail.into(),
    }
}
struct Snapshot {
    map: ElementAddressMap,
    rows: Vec<ElementStatus>,
    observation: Value,
}
impl Snapshot {
    fn capture(context: &Context<'_>) -> Result<Self> {
        if !context.ready() {
            return Err(context.lost("机械操作缺少当前 Serving 许可".into()));
        }
        if let Some(reason) = context.identity_error() {
            return Err(context.lost(reason));
        }
        for device in &context.active.devices {
            reservation::verify_holder(device.dev.as_ref(), context.active.key)?;
        }
        let (map, rows) = management::elements(&context.active.devices)?;
        let observation = management::observation_from_elements(
            &context.active.devices,
            context.node,
            context.active.round,
            &map,
            &rows,
        )?;
        // DVCID may contain vendor/model text. Bind each DTE through the same
        // strict unique configured-drive mapping used by the observation.
        let inventory = inventory_elements(&context.active.devices, &rows, true)?;
        let mut rows = rows;
        for drive in inventory.drives {
            rows.iter_mut()
                .find(|e| e.address == drive.addr)
                .expect("mapped DTE")
                .drive_id = Some(context.active.devices[drive.dev].serial.clone());
        }
        Ok(Self {
            map,
            rows,
            observation,
        })
    }
    fn at(&self, address: u16) -> Result<&ElementStatus> {
        self.rows
            .iter()
            .find(|e| e.address == address)
            .ok_or_else(|| refused(format!("完整库存缺少元素 {address:#06x}")))
    }
    fn barcode(&self, barcode: &str) -> Result<&ElementStatus> {
        self.rows
            .iter()
            .find(|e| e.full && e.volume_tag.as_deref() == Some(barcode))
            .ok_or_else(|| refused(format!("完整库存中未找到条码 {barcode}")))
    }
    fn enabled(element: &ElementStatus, access: bool) -> Result<()> {
        if element.exception || element.disabled || access && element.accessible != Some(true) {
            return Err(refused(format!(
                "元素{:#06x}不可移动：Access={:?} Except={} ED={} ASC/ASCQ={:02x}/{:02x}",
                element.address,
                element.accessible,
                element.exception,
                element.disabled,
                element.asc,
                element.ascq,
            )));
        }
        Ok(())
    }
    fn location(element: &ElementStatus) -> Result<MoveLocation> {
        let kind = match element.element_type {
            ElementType::Storage => MoveLocationKind::Storage,
            ElementType::ImportExport => MoveLocationKind::ImportExport,
            ElementType::DataTransfer => MoveLocationKind::Drive,
            ElementType::Transport => return Err(refused("机械手中的介质必须先核实原搬带结果")),
        };
        Ok(MoveLocation {
            address: element.address,
            kind,
            drive_serial: if kind == MoveLocationKind::Drive {
                element.drive_id.clone()
            } else {
                None
            },
        })
    }
    fn robot(&self) -> Result<()> {
        for transport in self
            .rows
            .iter()
            .filter(|e| e.element_type == ElementType::Transport)
        {
            Self::enabled(transport, false)?;
            if transport.full {
                return Err(refused("机械手被占用，保留原计划核对"));
            }
        }
        Ok(())
    }
    fn plan(&self, request: &ManagementRequest, home: Option<&MovePlan>) -> Result<MovePlan> {
        self.robot()?;
        let barcode = request.value()["change"]["barcode"]
            .as_str()
            .expect("barcode");
        let source = self.barcode(barcode)?;
        if source.is_cleaning() {
            return Err(refused("清洗带不支持运维搬带"));
        }
        Self::enabled(source, source.element_type != ElementType::DataTransfer)?;
        let source_location = Self::location(source)?;
        if home.is_some_and(|h| h.map != self.map) {
            return Err(refused("当前地址map与原home证明不符"));
        }
        let learned = match source.element_type {
            ElementType::Storage => Some(source.address),
            ElementType::DataTransfer => source.source_address.filter(|a| {
                u32::from(*a) >= u32::from(self.map.storage_start)
                    && u32::from(*a)
                        < u32::from(self.map.storage_start) + u32::from(self.map.storage_count)
            }),
            _ => None,
        };
        let home_address = home.and_then(|h| h.home_address).or(learned);
        if learned.zip(home_address).is_some_and(|(a, b)| a != b) {
            return Err(refused("介质当前来源与已保存home不一致"));
        }
        let destination = match request.value()["change"]["destination"].as_str() {
            Some("homeslot") => {
                self.at(home_address.ok_or_else(|| refused("没有可信home，不能任意选择存储槽"))?)?
            }
            Some("ieslot") if source.element_type == ElementType::ImportExport => source,
            Some("ieslot") => self
                .rows
                .iter()
                .filter(|e| e.element_type == ElementType::ImportExport && !e.full)
                .filter(|e| Self::enabled(e, true).is_ok())
                .min_by_key(|e| e.address)
                .ok_or_else(|| refused("没有可访问的空I/E槽"))?,
            _ => return Err(refused("未知搬带目标")),
        };
        Self::enabled(destination, true)?;
        if destination.full && destination.address != source.address {
            return Err(refused("原定目标槽已占用"));
        }
        let plan = MovePlan {
            version: 1,
            barcode: barcode.into(),
            captured_at: self.observation["observed_at"]
                .as_str()
                .expect("time")
                .into(),
            map: self.map.clone(),
            source: source_location,
            destination: Self::location(destination)?,
            home_address,
        };
        plan.validate().map_err(|e| refused(e.to_string()))?;
        Ok(plan)
    }
    fn reconcile(&self, plan: &MovePlan) -> Result<bool> {
        if self.map != plan.map {
            return Err(refused("地址map改变，保留原计划，禁止重选目标"));
        }
        self.robot()?;
        let actual = self.barcode(&plan.barcode)?;
        if actual.is_cleaning() {
            return Err(refused("原条码现在被识别为清洗带"));
        }
        let destination = self.at(plan.destination.address)?;
        Self::enabled(destination, true)?;
        if Self::location(actual)? == plan.destination {
            if plan.source.address != plan.destination.address && self.at(plan.source.address)?.full
            {
                return Err(refused("目标已有原条码但原源位置被占用，需核对"));
            }
            return Ok(true);
        }
        if Self::location(actual)? != plan.source || destination.full {
            return Err(refused("原条码不在原源位置或原目标被占用，保留检查点核对"));
        }
        Self::enabled(actual, actual.element_type != ElementType::DataTransfer)?;
        Ok(false)
    }
}

pub(super) struct Job {
    pub request: ManagementRequest,
    plan: Option<MovePlan>,
    execution: Option<MoveExecution>,
    diagnostic: Option<String>,
    completion: Option<super::fdb_observations::Completion>,
    publishing: bool,
    pub evicted_drive: Option<usize>,
}
impl Job {
    pub(super) fn new(request: ManagementRequest, task: &Value) -> Result<Self> {
        let execution = if task["execution"].is_null() {
            None
        } else {
            Some(
                MoveExecution::parse(task["execution"].clone(), &request)
                    .map_err(|e| refused(e.to_string()))?,
            )
        };
        Ok(Self {
            request,
            plan: execution.as_ref().map(|e| e.plan.clone()),
            execution,
            diagnostic: None,
            completion: None,
            publishing: false,
            evicted_drive: None,
        })
    }
    fn accept(&mut self, receipt: Value) -> Publication<bool> {
        if receipt["status"] != "running" {
            return Publication::Confirmed(true);
        }
        match MoveExecution::parse(receipt["execution"].clone(), &self.request) {
            Ok(execution) => {
                self.execution = Some(execution);
                Publication::Confirmed(false)
            }
            Err(error) => Publication::Failed {
                detail: error.to_string(),
                permanent: true,
            },
        }
    }
    pub(super) fn poll(
        &mut self,
        context: Context<'_>,
        publisher: &mut Publisher,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Publication<bool>> {
        let catalog = &context.control.catalog;
        let serial = &context.control.serial;
        let route = context.control.route;
        let round = context.active.round;
        if let Some(detail) = self.diagnostic.clone() {
            if self.execution.is_none() {
                return Ok(self.fail_preparation(&context, runtime, &detail));
            }
            let result = publish(
                &context,
                runtime,
                catalog.defer_media_move(serial, route, round, &self.request, &detail),
            );
            return Ok(match result {
                Publication::Confirmed(_) => {
                    self.diagnostic = None;
                    Publication::Failed {
                        detail: detail.clone(),
                        permanent: false,
                    }
                }
                other => other.map(|_| false),
            });
        }
        if self.plan.is_none() {
            let home = match publish(
                &context,
                runtime,
                catalog.active_tape_home_plan(
                    serial,
                    route,
                    round,
                    self.request.value()["change"]["barcode"]
                        .as_str()
                        .expect("barcode"),
                ),
            ) {
                Publication::Confirmed(home) => home,
                other => return Ok(other.map(|_| false)),
            };
            match Snapshot::capture(&context).and_then(|s| s.plan(&self.request, home.as_ref())) {
                Ok(plan) => self.plan = Some(plan),
                Err(error) if ownership_lost(&error) => return Err(error),
                Err(error) => {
                    self.diagnostic = Some(bounded_error(&error.to_string()));
                    return Ok(Publication::Confirmed(false));
                }
            }
        }
        // Reconfirm/adopt the original plan, including after a lost prepare ACK.
        let result = publish(&context, runtime, async {
            match catalog
                .prepare_media_move(
                    serial,
                    route,
                    round,
                    &self.request,
                    self.plan.as_ref().expect("plan"),
                )
                .await
            {
                Err(error) if matches!(error.domain(), CatalogError::Invalid(_)) => {
                    Ok(Err(error.to_string()))
                }
                result => result.map(Ok),
            }
        });
        match result {
            Publication::Confirmed(Err(detail)) => {
                self.diagnostic = Some(bounded_error(&detail));
                return Ok(Publication::Confirmed(false));
            }
            Publication::Confirmed(Ok(receipt)) => match self.accept(receipt) {
                Publication::Confirmed(true) => return Ok(Publication::Confirmed(true)),
                Publication::Confirmed(false) => {}
                other => return Ok(other),
            },
            other => return Ok(other.map(|_| false)),
        }
        if self.publishing {
            if self.completion.is_none() {
                publisher.poll(context, runtime)?;
                let Some(completion) = publisher.take_completion() else {
                    return Ok(Publication::Confirmed(false));
                };
                self.completion = Some(completion);
            }
            let completion = self.completion.as_ref().expect("original confirmed sample");
            return Ok(publish(
                &context,
                runtime,
                catalog.complete_media_move(
                    serial,
                    route,
                    round,
                    &self.request,
                    &completion.captured_at,
                ),
            )
            .map(|_| true));
        }
        let step = self.advance(&context, runtime);
        match step {
            Ok(Some(snapshot)) => {
                publisher.request_sample(round, snapshot.observation);
                self.publishing = true;
                Ok(Publication::Confirmed(false))
            }
            Ok(None) => Ok(Publication::Confirmed(false)),
            Err(error) if ownership_lost(&error) => Err(error),
            Err(error) => {
                self.diagnostic = Some(bounded_error(&error.to_string()));
                Ok(Publication::Confirmed(false))
            }
        }
    }
    fn fail_preparation(
        &self,
        context: &Context<'_>,
        runtime: &tokio::runtime::Runtime,
        detail: &str,
    ) -> Publication<bool> {
        publish(
            context,
            runtime,
            context.control.catalog.fail_unprepared_media_move(
                &context.control.serial,
                context.control.route,
                context.active.round,
                &self.request,
                detail,
            ),
        )
        .map(|_| true)
    }
    fn advance(
        &mut self,
        context: &Context<'_>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Option<Snapshot>> {
        let original_plan = self.plan.clone().expect("prepared plan");
        let plan = &original_plan;
        let snapshot = Snapshot::capture(context)?;
        if snapshot.reconcile(plan)? {
            return Ok(Some(snapshot));
        }
        let phase = self.execution.as_ref().expect("running execution").phase;
        let source = snapshot.at(plan.source.address)?;
        let drive = if plan.source.kind == MoveLocationKind::Drive {
            let serial = plan.source.drive_serial.as_ref().expect("drive serial");
            Some(
                context
                    .active
                    .devices
                    .iter()
                    .position(|d| d.kind == DeviceKind::Drive && &d.serial == serial)
                    .ok_or_else(|| refused("原驱动序列号不在配置设备组中"))?,
            )
        } else {
            None
        };
        // No other executor operation runs during this poll. The controller drops
        // this mount before returning to the read/idle-unload dispatch loop, even
        // when a CDB fails. The shared gate already excludes new hardware reads.
        self.evicted_drive = drive;
        let next = if drive.is_some()
            && (phase == MovePhase::Prepared || source.accessible != Some(true))
        {
            MovePhase::Unloading
        } else {
            MovePhase::Moving
        };
        if phase == MovePhase::Moving && next == MovePhase::Unloading {
            return Err(refused(
                "MOVE阶段驱动再次不可访问，禁止回退阶段或盲目重放UNLOAD",
            ));
        }
        match publish(
            context,
            runtime,
            context.control.catalog.stage_media_move(
                &context.control.serial,
                context.control.route,
                context.active.round,
                &self.request,
                next,
            ),
        ) {
            Publication::Confirmed(receipt) => match self.accept(receipt) {
                Publication::Confirmed(false) => {}
                Publication::Confirmed(true) => {
                    return Err(refused("机械阶段已终结，禁止继续发送命令"));
                }
                _ => return Err(refused("原机械阶段回执无效")),
            },
            Publication::Lost(reason) => return Err(context.lost(reason)),
            Publication::Paused => return Ok(None),
            Publication::Failed { detail, .. } => return Err(refused(detail)),
        }
        // The phase ACK may have taken seconds: sample again and check the exact
        // original source/target before issuing this one command.
        let fresh = Snapshot::capture(context)?;
        if fresh.reconcile(plan)? {
            return Ok(Some(fresh));
        }
        let source = fresh.at(plan.source.address)?;
        if next == MovePhase::Unloading {
            if source.accessible == Some(true) && phase != MovePhase::Prepared {
                return Ok(None);
            }
            TapeDrive::new(
                context.active.devices[drive.expect("unload drive")]
                    .dev
                    .as_ref(),
            )
            .unload()?;
        } else {
            Snapshot::enabled(source, true)?;
            let changer = context
                .active
                .devices
                .iter()
                .find(|d| d.kind == DeviceKind::Changer)
                .expect("verified changer");
            let mut changer = MediumChanger::new(changer.dev.as_ref());
            if changer.load_address_map()? != &plan.map {
                return Err(refused("MOVE前地址map已改变，保留原计划"));
            }
            changer.move_medium(plan.source.address, plan.destination.address)?;
        }
        // Never issue UNLOAD and MOVE together. The next poll first rechecks
        // ownership, the saved phase, and a fresh complete inventory.
        Ok(None)
    }
}

#[cfg(test)]
#[path = "executor_fdb_move_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "executor_fdb_move_fault_tests.rs"]
mod fault_tests;
