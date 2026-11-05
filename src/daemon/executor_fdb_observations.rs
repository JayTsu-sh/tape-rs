//! Complete historical observations from the current guarded device executor.
//! A pending publication keeps the original sample until its result is known.
use super::super::permission::Phase;
use super::*;
use crate::managed_catalog::CatalogError;
use serde_json::Value;

const INTERVAL: Duration = Duration::from_secs(30);
const RPC_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub active: &'a Active,
    pub permission: &'a Permission,
    pub control: &'a FdbOwnerControl,
    pub status: &'a SharedStatus,
    pub node: u8,
}
impl Context<'_> {
    pub(super) fn ready(&self) -> bool {
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        self.active.serving
            && !status.shutting_down
            && !status.catalog_pending
            && status.id == u64::from(self.node)
            && status.executor_ready
            && status.catalog_frozen_index == Some(self.control.route.frozen_index)
            && status.executor.as_ref().is_some_and(|e| {
                e.node == u64::from(self.node) && e.round == self.active.round && e.fenced
            })
            && self.permission.allows(self.active.round, Phase::Serving)
    }
    pub(super) fn identity_error(&self) -> Option<String> {
        if self.active.key.node() != self.node || self.active.key.round() != self.active.round {
            return Some(format!(
                "预留键节点/轮次为{}/{}，期望{}/{}",
                self.active.key.node(),
                self.active.key.round(),
                self.node,
                self.active.round
            ));
        }
        let changers = self
            .active
            .devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Changer)
            .collect::<Vec<_>>();
        if changers.len() != 1 {
            return Some(format!("当前设备组换带器数量为{}，期望1", changers.len()));
        }
        if changers[0].serial != self.control.serial {
            return Some(format!(
                "设备换带器序列号{}与目标库{}不符",
                changers[0].serial, self.control.serial
            ));
        }
        self.active
            .devices
            .iter()
            .find(|d| {
                d.dev.execution_guard()
                    != Some((
                        self.active.round,
                        self.permission as *const Permission as usize,
                    ))
            })
            .map(|d| {
                format!(
                    "设备{}（{}）缺少轮次{}的同源执行守护",
                    d.name, d.serial, self.active.round
                )
            })
    }
    pub(super) fn lost(&self, reason: String) -> TapeError {
        TapeError::OwnershipLost {
            device: self.control.serial.clone(),
            detail: format!("库观测发布已失去原执行权：{reason}"),
        }
    }
}
struct Attempt {
    captured_at: String,
    inventory: Option<Value>,
    inventory_confirmed: bool,
    error: Option<String>,
}
impl Attempt {
    fn capture(context: &Context<'_>) -> Result<Self> {
        match management::observe_devices(
            &context.active.devices,
            context.node,
            context.active.round,
        ) {
            Ok(value) => Ok(Self {
                captured_at: value["observed_at"].as_str().expect("capture time").into(),
                inventory: Some(value),
                inventory_confirmed: false,
                error: None,
            }),
            Err(error) if ownership_lost(&error) => Err(error),
            Err(error) => {
                warn!(
                    "带库{} 库存采样失败（轮次{}）：{}",
                    context.control.serial, context.active.round, error
                );
                Ok(Self {
                    captured_at: crate::ltfs::label::ltfs_time_now(),
                    inventory: None,
                    inventory_confirmed: true,
                    error: Some(bounded_error(&format!("库存采样失败：{error}"))),
                })
            }
        }
    }
}
pub(super) fn bounded_error(text: &str) -> String {
    if text.len() <= 2048 {
        return text.into();
    }
    let mut end = 2045;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
pub(super) enum Publication<T> {
    Confirmed(T),
    Paused,
    Lost(String),
    Failed { detail: String, permanent: bool },
}

impl<T> Publication<T> {
    pub(super) fn map<U>(self, f: impl FnOnce(T) -> U) -> Publication<U> {
        match self {
            Self::Confirmed(value) => Publication::Confirmed(f(value)),
            Self::Paused => Publication::Paused,
            Self::Lost(reason) => Publication::Lost(reason),
            Self::Failed { detail, permanent } => Publication::Failed { detail, permanent },
        }
    }
}

fn catalog_result<T>(result: std::result::Result<T, CatalogError>) -> Publication<T> {
    let error = match result {
        Ok(value) => return Publication::Confirmed(value),
        Err(error) => error,
    };
    let domain = error.domain();
    if matches!(
        domain,
        CatalogError::OwnerFenced | CatalogError::MaintenanceFenced
    ) {
        return Publication::Lost(error.to_string());
    }
    let permanent = matches!(
        domain,
        CatalogError::Invalid(_)
            | CatalogError::Corrupt(_)
            | CatalogError::Cache(_)
            | CatalogError::Fdb(foundationdb::FdbBindingError::CustomError(_))
    );
    Publication::Failed {
        detail: error.to_string(),
        permanent,
    }
}
pub(super) fn publish<T>(
    context: &Context<'_>,
    runtime: &tokio::runtime::Runtime,
    future: impl std::future::Future<Output = std::result::Result<T, CatalogError>>,
) -> Publication<T> {
    runtime.block_on(async {
        let mut future = std::pin::pin!(future);
        let deadline = Instant::now() + RPC_BUDGET;
        loop {
            if !context.ready() {
                return Publication::Paused;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Publication::Failed {
                    detail: "FDB回执等待超时，保留原请求和观测核对；不视为未提交".into(),
                    permanent: false,
                };
            }
            match tokio::time::timeout(left.min(Duration::from_millis(25)), &mut future).await {
                Ok(result) => {
                    return if context.ready() {
                        catalog_result(result)
                    } else {
                        Publication::Paused
                    };
                }
                Err(_) => continue,
            }
        }
    })
}

pub(super) struct Completion {
    pub captured_at: String,
    pub error: Option<String>,
}

pub(super) struct Publisher {
    round: u64,
    next: Instant,
    pending: Option<Attempt>,
    completed: Option<Completion>,
    failures: u32,
    blocked: bool,
}
impl Default for Publisher {
    fn default() -> Self {
        Self {
            round: 0,
            next: Instant::now(),
            pending: None,
            completed: None,
            failures: 0,
            blocked: false,
        }
    }
}
pub(super) fn retry_delay(failures: u32) -> Duration {
    let millis = (500u64 << failures.min(6)).min(30_000);
    Duration::from_millis(millis + u64::from(uuid::Uuid::new_v4().as_bytes()[0]))
}
impl Publisher {
    pub(super) fn idle(&self) -> bool {
        !self.blocked && self.pending.is_none()
    }
    pub(super) fn request_fresh(&mut self) {
        assert!(self.idle(), "先结清原观测");
        self.completed = None;
        self.next = Instant::now();
    }
    /// Publish the exact reconciled reply; do not take an unrelated second sample.
    pub(super) fn request_sample(&mut self, round: u64, inventory: Value) {
        assert!(self.idle(), "先结清原观测");
        self.round = round;
        self.completed = None;
        self.next = Instant::now();
        self.pending = Some(Attempt {
            captured_at: inventory["observed_at"]
                .as_str()
                .expect("capture time")
                .into(),
            inventory: Some(inventory),
            inventory_confirmed: false,
            error: None,
        });
    }
    pub(super) fn take_completion(&mut self) -> Option<Completion> {
        self.completed.take()
    }
    fn accept(
        &mut self,
        context: &Context<'_>,
        stage: &str,
        result: Publication<()>,
    ) -> Result<bool> {
        match result {
            Publication::Confirmed(()) => Ok(true),
            Publication::Paused => Ok(false),
            Publication::Lost(reason) => Err(context.lost(reason)),
            Publication::Failed { detail, permanent } => {
                self.failures = self.failures.saturating_add(1);
                self.next = Instant::now() + retry_delay(self.failures);
                self.blocked = permanent;
                warn!(
                    "带库{} 观测发布失败（节点{} 轮次{} 采样{} 阶段{}，保留原采样，需修复持久事实={}）：{}",
                    context.control.serial,
                    context.node,
                    self.round,
                    self.pending
                        .as_ref()
                        .map(|a| a.captured_at.as_str())
                        .unwrap_or("未采集"),
                    stage,
                    permanent,
                    detail
                );
                Ok(false)
            }
        }
    }
    pub(super) fn poll(
        &mut self,
        context: Context<'_>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<()> {
        if context.active.round < self.round {
            return Ok(());
        }
        if self.round != context.active.round {
            *self = Self {
                round: context.active.round,
                ..Default::default()
            };
        }
        if self.blocked || Instant::now() < self.next || !context.ready() {
            return Ok(());
        }
        if let Some(reason) = context.identity_error() {
            self.blocked = true;
            error!(
                "带库{} 观测发布停止（轮次{}）：{}",
                context.control.serial, self.round, reason
            );
            return Ok(());
        }
        if self.pending.is_none() {
            self.pending = Some(Attempt::capture(&context)?);
        }
        if !context.ready() {
            return Ok(());
        }
        let attempt = self.pending.as_ref().expect("captured attempt");
        if !attempt.inventory_confirmed {
            let observation = attempt
                .inventory
                .as_ref()
                .expect("unconfirmed complete sample");
            let result = publish(
                &context,
                runtime,
                context.control.catalog.record_active_library_inventory(
                    &context.control.serial,
                    context.control.route,
                    self.round,
                    observation,
                ),
            );
            if !self.accept(&context, "inventory", result)? {
                return Ok(());
            }
            self.pending
                .as_mut()
                .expect("same attempt")
                .inventory_confirmed = true;
        }
        if !context.ready() {
            return Ok(());
        }
        let attempt = self.pending.as_ref().expect("same attempt");
        let result = publish(
            &context,
            runtime,
            context.control.catalog.record_library_monitor(
                &context.control.serial,
                context.control.route,
                self.round,
                &attempt.captured_at,
                attempt.error.as_deref(),
            ),
        );
        if !self.accept(&context, "monitor", result)? {
            return Ok(());
        }
        let attempt = self.pending.take().expect("confirmed monitor");
        let failed = attempt.error.is_some();
        self.completed = Some(Completion {
            captured_at: attempt.captured_at,
            error: attempt.error,
        });
        if failed {
            self.failures = self.failures.saturating_add(1);
        } else {
            self.failures = 0;
        }
        // Failed device polls retry less often than the normal cadence. Metadata
        // retries above do not acquire another device sample or replace its time.
        let delay = if failed {
            INTERVAL.saturating_mul(1u32 << self.failures.min(2)) + retry_delay(0)
        } else {
            INTERVAL
        };
        self.next = Instant::now() + delay;
        Ok(())
    }
}

#[cfg(test)]
#[path = "executor_fdb_observations_tests.rs"]
mod tests;
