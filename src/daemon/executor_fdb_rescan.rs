//! One device-thread controller owns periodic observations and explicit rescans.
//! Admission, sampling and final receipts never run as independent producers.
use super::fdb_observations::{Completion, Context, Publication, Publisher, publish, retry_delay};
use super::*;
use crate::managed_catalog::ManagementRequest;

struct Task {
    request: ManagementRequest,
    running: bool,
    completion: Option<Completion>,
}
pub(super) struct Observer {
    round: u64,
    publisher: Publisher,
    task: Option<Task>,
    mechanical: Option<fdb_move::Job>,
    assignment: Option<fdb_assign::Job>,
    evicted: Vec<usize>,
    installed: Option<(usize, fdb_loading::Mount)>,
    cleared_imports: Vec<String>,
    cursor: Option<(u64, uuid::Uuid)>,
    next: Instant,
    failures: u32,
    blocked: bool,
}
impl Default for Observer {
    fn default() -> Self {
        Self {
            round: 0,
            publisher: Publisher::default(),
            task: None,
            mechanical: None,
            assignment: None,
            evicted: Vec::new(),
            installed: None,
            cleared_imports: Vec::new(),
            cursor: None,
            next: Instant::now(),
            failures: 0,
            blocked: false,
        }
    }
}
impl Observer {
    fn confirmed<T>(
        &mut self,
        context: &Context<'_>,
        stage: &str,
        result: Publication<T>,
    ) -> Result<Option<T>> {
        match result {
            Publication::Confirmed(value) => {
                self.failures = 0;
                Ok(Some(value))
            }
            Publication::Paused => Ok(None),
            Publication::Lost(reason) => {
                Err(context.lost(format!("{stage} 原任务{}：{reason}", self.task_identity())))
            }
            Publication::Failed { detail, permanent } => {
                self.failures = self.failures.saturating_add(1);
                self.next = Instant::now() + retry_delay(self.failures);
                self.blocked = permanent;
                warn!(
                    "带库{} 管理阶段{}失败（轮次{}，原任务{}，需修复持久事实={}）：{}",
                    context.control.serial,
                    stage,
                    self.round,
                    self.task_identity(),
                    permanent,
                    detail
                );
                Ok(None)
            }
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.task.is_some()
            || self.mechanical.is_some()
            || self.assignment.is_some()
            || !self.publisher.idle()
    }
    fn task_identity(&self) -> String {
        self.mechanical
            .as_ref()
            .map(fdb_move::Job::diagnostic_identity)
            .or_else(|| {
                self.assignment
                    .as_ref()
                    .map(fdb_assign::Job::diagnostic_identity)
            })
            .or_else(|| {
                self.task
                    .as_ref()
                    .map(|t| t.request.operation().to_string())
            })
            .unwrap_or_else(|| "未选取".into())
    }
    pub(super) fn poll_device(
        &mut self,
        active: &mut Active,
        permission: &Permission,
        control: &FdbOwnerControl,
        status: &SharedStatus,
        node: u8,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<()> {
        let result = self.poll(
            Context {
                active,
                permission,
                control,
                status,
                node,
            },
            runtime,
        );
        // Single device thread: invalidate the original mount before any queued
        // read or idle-unload can run, including after an unknown CDB outcome.
        for drive in self.evicted.drain(..) {
            active.fdb_reads.remove(&drive);
        }
        if let Some((drive, mount)) = self.installed.take()
            && result.is_ok()
        {
            active.fdb_reads.insert(drive, mount);
        }
        for barcode in self.cleared_imports.drain(..) {
            if result.is_ok() {
                active.fdb_pending_import.remove(&barcode);
            }
        }
        result
    }
    pub(super) fn poll(
        &mut self,
        context: Context<'_>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<()> {
        let round = context.active.round;
        if round < self.round {
            return Ok(());
        }
        if round != self.round {
            *self = Self {
                round,
                ..Default::default()
            };
        }
        if !context.ready() || self.blocked {
            return Ok(());
        }
        // A pending final receipt pins the original published timestamp. In
        // particular, periodic sampling cannot overwrite it while its ACK is lost.
        if self.task.is_none() && self.mechanical.is_none() && self.assignment.is_none() {
            // Settle an existing sample, but never start a new periodic CDB
            // before checking a persistent mechanical recovery checkpoint.
            if !self.publisher.idle() {
                self.publisher.poll(context, runtime)?;
                self.publisher.take_completion();
                if !self.publisher.idle() {
                    return Ok(());
                }
            }
            if Instant::now() < self.next {
                return Ok(());
            }
            let pending = publish(
                &context,
                runtime,
                context.control.catalog.active_library_mechanical_task(
                    &context.control.serial,
                    context.control.route,
                    round,
                ),
            );
            let Some(pending) = self.confirmed(&context, "机械恢复检查", pending)? else {
                return Ok(());
            };
            if let Some(task) = pending {
                let request = ManagementRequest::parse(task["request"].clone())
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                if request.value()["change"]["kind"] == "media-assign" {
                    self.assignment = Some(fdb_assign::Job::new(request, &task)?);
                } else if request.value()["change"]["kind"] == "media-move" {
                    self.mechanical = Some(fdb_move::Job::new(request, &task)?);
                } else {
                    // The original native task owns the library until its
                    // checkpoint is settled by its sole actor.
                    return Ok(());
                }
            }
        }
        if self.task.is_none() && self.mechanical.is_none() && self.assignment.is_none() {
            self.publisher.poll(context, runtime)?;
            self.publisher.take_completion();
            if !self.publisher.idle() || Instant::now() < self.next {
                return Ok(());
            }
            self.next = Instant::now() + Duration::from_millis(250);
            let selected = publish(&context, runtime, async {
                let catalog = &context.control.catalog;
                let serial = &context.control.serial;
                let route = context.control.route;
                let window = catalog.active_management_window(serial, route).await?;
                let cursor = self.cursor.filter(|(epoch, _)| {
                    (window.first_retained..=window.accepting).contains(epoch)
                });
                let Some((epoch, operation)) = catalog
                    .next_management(serial, route, round, cursor)
                    .await?
                else {
                    return Ok(None);
                };
                let task = catalog
                    .management_task(serial, route, epoch, operation)
                    .await?
                    .ok_or(crate::managed_catalog::CatalogError::Corrupt(
                        "rescan队列原任务缺失",
                    ))?;
                Ok(Some((
                    (epoch, operation),
                    ManagementRequest::parse(task["request"].clone())?,
                    task,
                )))
            });
            let Some(selected) = self.confirmed(&context, "选取", selected)? else {
                return Ok(());
            };
            let Some((cursor, request, saved)) = selected else {
                return Ok(());
            };
            self.cursor = Some(cursor);
            if request.value()["change"]["kind"] == "media-move" {
                self.mechanical = Some(fdb_move::Job::new(request, &saved)?);
            } else if request.value()["change"]["kind"] == "media-assign" {
                self.assignment = Some(fdb_assign::Job::new(request, &saved)?);
            } else if request.value()["change"]["kind"] == "library-rescan" {
                self.task = Some(Task {
                    request,
                    running: false,
                    completion: None,
                });
            } else {
                return Ok(());
            }
            self.next = Instant::now();
        }
        if Instant::now() < self.next {
            return Ok(());
        }
        if let Some(mut job) = self.assignment.take() {
            let result = job.poll(context, &mut self.publisher, runtime);
            if let Some(drive) = job.evicted_drive.take() {
                self.evicted.push(drive);
            }
            self.assignment = Some(job);
            let result = result.map_err(|error| {
                context.lost(format!("分配原任务{}：{error}", self.task_identity()))
            })?;
            if self.confirmed(&context, "分配", result)? == Some(true) {
                self.installed = self
                    .assignment
                    .as_ref()
                    .and_then(|job| job.verified_mount(context.active));
                self.assignment = None;
                self.next = Instant::now() + Duration::from_millis(250);
            }
            return Ok(());
        }
        if let Some(mut job) = self.mechanical.take() {
            let result = job.poll(context, &mut self.publisher, runtime);
            if let Some(drive) = job.evicted_drive.take() {
                self.evicted.push(drive);
            }
            self.mechanical = Some(job);
            let result = result.map_err(|error| {
                context.lost(format!("搬带原任务{}：{error}", self.task_identity()))
            })?;
            if self.confirmed(&context, "搬带", result)? == Some(true) {
                if let Some(job) = &self.mechanical
                    && job.request.value()["change"]["pool"].is_null()
                    && job.request.value()["change"]["volume"].is_null()
                {
                    self.cleared_imports.push(
                        job.request.value()["change"]["barcode"]
                            .as_str()
                            .expect("barcode")
                            .to_owned(),
                    );
                }
                self.mechanical = None;
                self.next = Instant::now() + Duration::from_millis(250);
            }
            return Ok(());
        }
        let task = self.task.as_ref().expect("selected task");
        if !task.running {
            let result = publish(
                &context,
                runtime,
                context.control.catalog.begin_library_rescan(
                    &context.control.serial,
                    context.control.route,
                    round,
                    &task.request,
                ),
            );
            let Some(receipt) = self.confirmed(&context, "running确认", result)? else {
                return Ok(());
            };
            if receipt["status"] != "running" {
                self.task = None; // A cancelled/terminal original task never samples again.
                return Ok(());
            }
            self.task.as_mut().expect("same task").running = true;
            self.publisher.request_fresh();
        }
        if self.task.as_ref().expect("same task").completion.is_none() {
            self.publisher.poll(context, runtime)?;
            let Some(completion) = self.publisher.take_completion() else {
                return Ok(());
            };
            self.task.as_mut().expect("same task").completion = Some(completion);
        }
        let task = self.task.as_ref().expect("same task");
        let completion = task.completion.as_ref().expect("confirmed original sample");
        let result = publish(
            &context,
            runtime,
            context.control.catalog.complete_library_rescan(
                &context.control.serial,
                context.control.route,
                round,
                &task.request,
                &completion.captured_at,
                completion.error.as_deref(),
            ),
        );
        if self.confirmed(&context, "终态确认", result)?.is_some() {
            self.task = None;
            self.next = Instant::now() + Duration::from_millis(250);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "executor_fdb_rescan_tests.rs"]
mod tests;
