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
            Publication::Lost(reason) => Err(context.lost(reason)),
            Publication::Failed { detail, permanent } => {
                self.failures = self.failures.saturating_add(1);
                self.next = Instant::now() + retry_delay(self.failures);
                self.blocked = permanent;
                warn!(
                    "带库{} rescan阶段{}失败（轮次{}，原任务{}，需修复持久事实={}）：{}",
                    context.control.serial,
                    stage,
                    self.round,
                    self.task
                        .as_ref()
                        .map(|t| t.request.operation().to_string())
                        .unwrap_or_else(|| "未选取".into()),
                    permanent,
                    detail
                );
                Ok(None)
            }
        }
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
        if self.task.is_none() {
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
                )))
            });
            let Some(selected) = self.confirmed(&context, "选取", selected)? else {
                return Ok(());
            };
            let Some((cursor, request)) = selected else {
                return Ok(());
            };
            self.cursor = Some(cursor);
            if request.value()["change"]["kind"] != "library-rescan" {
                return Ok(());
            }
            self.task = Some(Task {
                request,
                running: false,
                completion: None,
            });
            self.next = Instant::now();
        }
        if Instant::now() < self.next {
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
