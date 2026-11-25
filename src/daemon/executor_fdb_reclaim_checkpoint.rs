//! One original body/archive S4, then full physical observation and publication.
//! No caller may substitute a new batch when one of these steps is unknown.
use super::*;
use crate::daemon::fdb_media::{
    ActiveMediaUpload, commit_reclaim_media_checkpoint, publish_active_media_batch,
};
use crate::ltfs::volume::{CommittedIndexConstruct, LtfsVolume, VolumeSession};
use crate::managed_catalog::{
    ActiveIndexOutcome, PreparedActiveFile, ReclaimCopyGroup, ReclaimGroupExecution,
    ReclaimIndexAugmentation,
};
use uuid::Uuid;
#[path = "executor_fdb_reclaim_mam.rs"]
mod original_mam;
use original_mam::read_original_mam;
#[cfg(test)]
#[path="executor_fdb_reclaim_probe_test_hook.rs"]
mod probe_seal_test_hook;
#[cfg(test)]
pub(in crate::daemon::executor) use probe_seal_test_hook::arm_original_probe_seal_for_test;
#[cfg(test)]
#[path = "executor_fdb_reclaim_readback_test_hook.rs"]
mod readback_seal_test_hook;
#[cfg(test)]
pub(in crate::daemon::executor) use readback_seal_test_hook::{
    ReadbackSealMode, arm_readback_seal_for_test, observe_group_read_for_test,
};

#[cfg(test)]
pub(super) fn record_prepared_source_used(parent: &ReclaimReservation) {
    readback_seal_test_hook::record_prepared_source_used(parent);
}

#[cfg(test)]
#[path = "executor_fdb_reclaim_s4_test_hook.rs"]
mod s4_test_hook;
#[cfg(test)]
pub(in crate::daemon::executor) use s4_test_hook::{arm_original_checkpoint_s4_stop_for_test, OriginalS4StopFacts};

trait CheckedTransport {
    fn transport(&self) -> &dyn crate::scsi::transport::TapeTransport;
    fn verify(&self) -> crate::error::Result<()>;
}
impl<F: Fn() -> crate::error::Result<()>> CheckedTransport for fdb_operation_guard::Guard<'_, F> {
    fn transport(&self) -> &dyn crate::scsi::transport::TapeTransport {
        self
    }
    fn verify(&self) -> crate::error::Result<()> {
        self.verify_proof()
    }
}

/// Only this executor module can choose the original device and its complete
/// group authority. The writer receives no independently substitutable scope,
/// device, callback, or mounted volume.
pub(in crate::daemon) struct FreshReclaimGroupIo<'a> {
    catalog: &'a crate::managed_catalog::ManagedCatalog,
    scope: &'a ReclaimGroupExecution,
    uploads: &'a [ActiveMediaUpload],
    key: crate::scsi::reservation::ReservationKey,
    guard: Box<dyn CheckedTransport + 'a>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    prepared_mount: std::cell::Cell<Option<PreparedTargetMount>>,
}

impl<'a> FreshReclaimGroupIo<'a> {
    pub(crate) fn catalog(&self) -> &crate::managed_catalog::ManagedCatalog {
        self.catalog
    }
    pub(crate) fn scope(&self) -> &ReclaimGroupExecution {
        self.scope
    }
    pub(crate) fn uploads(&self) -> &[ActiveMediaUpload] {
        self.uploads
    }
    pub(crate) fn key(&self) -> crate::scsi::reservation::ReservationKey {
        self.key
    }
    pub(crate) fn verify(&self) -> crate::error::Result<()> {
        self.guard.verify()
    }
    /// A confirmed reservation strengthens the original member expectation
    /// before its first CDB; disappearance may never be mistaken for fresh.
    pub(crate) fn confirm_reserved(
        &self,
        intent: &crate::managed_catalog::ActiveFileIntent,
    ) -> crate::error::Result<()> {
        let mut matches = self
            .uploads
            .iter()
            .enumerate()
            .filter(|(_, upload)| upload.intent == *intent);
        let ordinal = matches
            .next()
            .map(|(ordinal, _)| ordinal)
            .ok_or_else(|| TapeError::Catalog("确认预约不是原回收组完整请求".into()))?;
        if matches.next().is_some() {
            return Err(TapeError::Catalog("原回收组预约请求重复".into()));
        }
        {
            let mut seen = self
                .seen
                .lock()
                .map_err(|_| TapeError::Catalog("回收组预约观察锁损坏".into()))?;
            *seen
                .get_mut(ordinal)
                .ok_or_else(|| TapeError::Catalog("原回收组预约ordinal缺失".into()))? = true;
        }
        self.verify()
    }
    pub(crate) fn mount(
        &self,
        previous: CommittedIndexConstruct,
    ) -> crate::error::Result<LtfsVolume<'_>> {
        let transport = self.guard.transport();
        if let Some(PreparedTargetMount { binding, session }) = self.prepared_mount.take() {
            if !binding.matches_group(self.scope.parent(), self.scope.group())
                || !binding.matches_drive(transport)
                || !session.matches_device(transport, self.key)
                || session.committed_index_construct() != previous
            {
                return Err(TapeError::Refused {
                    reason: "回收目标预挂载失去原批次、卷头或设备会话".into(),
                });
            }
            self.verify()?;
            let volume = session.resume(transport, self.key)?;
            let append_position =
                volume
                    .recovery()
                    .append_position
                    .ok_or_else(|| TapeError::Refused {
                        reason: "回收目标预挂载缺少已验证的追加位置".into(),
                    })?;
            let at_append_position = crate::tape::commands::TapeDrive::new(transport)
                .position_matches(1, append_position)?;
            self.verify()?;
            if !at_append_position || !binding.matches_drive(transport) {
                return Err(TapeError::Refused {
                    reason: "回收目标预挂载实际追加位置或设备会话已改变".into(),
                });
            }
            log::info!(
                "回收组已复用原预挂载: 操作={} 组={} 驱动器={}",
                self.scope.parent().intent()["operation"],
                self.scope.group().id(),
                self.scope.destination_drive_serial(),
            );
            return Ok(volume);
        }
        let mut volume = LtfsVolume::mount(transport)?;
        volume.set_reservation_guard(Some(self.key));
        Ok(volume)
    }
}

fn observe_reservations(seen: &mut [bool], current: &[bool]) -> crate::error::Result<()> {
    if seen.len() != current.len() || seen.is_empty() {
        return Err(TapeError::Catalog("回收组预约集合长度改变".into()));
    }
    for (seen, current) in seen.iter_mut().zip(current) {
        if *seen && !*current {
            return Err(TapeError::Catalog(
                "原回收组已观察预约或收据消失，必须核验原操作".into(),
            ));
        }
        *seen |= *current;
    }
    Ok(())
}

fn fresh_group_io<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    scope: &'a ReclaimGroupExecution,
    uploads: &'a [ActiveMediaUpload],
    prepared_mount: Option<PreparedTargetMount>,
) -> std::result::Result<FreshReclaimGroupIo<'a>, ReadError> {
    if uploads.is_empty()
        || scope.parent().serial() != control.serial
        || scope.parent().intent()["namespace"] != control.route.route.namespace.to_string()
        || owner.key.round() != owner.round
        || scope.execution_epoch() != owner.round
        || uploads
            .iter()
            .any(|upload| upload.intent.owner_epoch != owner.round)
    {
        return Err(lost("原回收组设备范围为空或带库不同"));
    }
    let drive = device(owner, scope.destination_drive_serial())?;
    let intents = uploads
        .iter()
        .map(|upload| upload.intent.clone())
        .collect::<Vec<_>>();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![false; intents.len()]));
    let observed_seen = std::sync::Arc::clone(&seen);
    let transport = owner.devices[drive].dev.as_ref();
    let check_catalog = move || {
        let current =
            futures_executor::block_on(control.catalog.check_reclaim_write_group(scope, &intents))
                .map_err(|error| TapeError::Catalog(error.to_string()))?;
        let mut seen = observed_seen
            .lock()
            .map_err(|_| TapeError::Catalog("回收组预约观察锁损坏".into()))?;
        observe_reservations(&mut seen, &current)
    };
    let guard: Box<dyn CheckedTransport + 'a> = if let Some(prepared) = &prepared_mount {
        let execution = prepared.binding.execution;
        let key = owner.key;
        Box::new(fdb_operation_guard::Guard::new_bracketed_read(
            transport,
            move || {
                if !permit.allows(key.round(), crate::daemon::permission::Phase::Serving)
                    || transport.execution_guard() != Some(execution)
                {
                    return Err(TapeError::OwnershipLost {
                        device: transport.identity().into(),
                        detail: "回收组预挂载原执行许可失效".into(),
                    });
                }
                crate::scsi::reservation::verify_holder_without_ua_retry(transport, key)?;
                check_catalog()
            },
        ))
    } else {
        Box::new(fdb_loading::operation_guard(
            owner,
            permit,
            transport,
            check_catalog,
        ))
    };
    Ok(FreshReclaimGroupIo {
        catalog: &control.catalog,
        scope,
        uploads,
        key: owner.key,
        guard,
        seen,
        prepared_mount: std::cell::Cell::new(prepared_mount),
    })
}

/// Observe-only adapter: a reconstructed writable Volume can never turn
/// observation authority into a body/FM/MAM write or a mechanical operation.
struct ReadOnlyGroupTransport<'a> {
    guard: Box<dyn CheckedTransport + 'a>,
}
impl crate::scsi::transport::TapeTransport for ReadOnlyGroupTransport<'_> {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> crate::error::Result<crate::scsi::device::ScsiResult> {
        if !matches!(cdb.first(), Some(0x00 | 0x01 | 0x11 | 0x2b | 0x92)) {
            return Err(TapeError::Refused {
                reason: "原回收Observe范围只允许读取和定位".into(),
            });
        }
        self.guard.transport().execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        data: &mut [u8],
        timeout: u32,
    ) -> crate::error::Result<crate::scsi::device::ScsiResult> {
        if !matches!(
            cdb.first(),
            Some(0x05 | 0x08 | 0x12 | 0x1a | 0x34 | 0x4d | 0x5a | 0x5e | 0x8c | 0xa3)
        ) {
            return Err(TapeError::Refused {
                reason: "原回收Observe范围拒绝非读取CDB".into(),
            });
        }
        self.guard.transport().execute_read(cdb, data, timeout)
    }
    fn execute_write(
        &self,
        _cdb: &[u8],
        _data: &[u8],
        _timeout: u32,
    ) -> crate::error::Result<crate::scsi::device::ScsiResult> {
        Err(TapeError::Refused {
            reason: "原回收Observe范围禁止写入".into(),
        })
    }
    fn identity(&self) -> &str {
        self.guard.transport().identity()
    }
    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.guard.transport().medium_change_token()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.guard.transport().session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.guard.transport().execution_guard()
    }
}

pub(in crate::daemon) struct ObserveReclaimGroupIo<'a> {
    catalog: &'a crate::managed_catalog::ManagedCatalog,
    scope: &'a crate::managed_catalog::ReclaimGroupObservation,
    key: crate::scsi::reservation::ReservationKey,
    batch: Uuid,
    previous: crate::managed_catalog::ActiveIndexIdentity,
    current: crate::managed_catalog::ActiveIndexIdentity,
    plan: ReclaimIndexAugmentation,
    transport: ReadOnlyGroupTransport<'a>,
}
impl ObserveReclaimGroupIo<'_> {
    pub(crate) fn catalog(&self) -> &crate::managed_catalog::ManagedCatalog {
        self.catalog
    }
    pub(crate) fn scope(&self) -> &crate::managed_catalog::ReclaimGroupObservation {
        self.scope
    }
    pub(crate) fn key(&self) -> crate::scsi::reservation::ReservationKey {
        self.key
    }
    pub(crate) fn transport(&self) -> &dyn crate::scsi::transport::TapeTransport {
        &self.transport
    }
    pub(crate) fn plan(&self) -> &ReclaimIndexAugmentation {
        &self.plan
    }
    pub(crate) fn verify(&self) -> crate::error::Result<()> {
        self.transport.guard.verify()
    }
    pub(crate) fn require_checkpoint(
        &self,
        batch: Uuid,
        previous: crate::managed_catalog::ActiveIndexIdentity,
        current: crate::managed_catalog::ActiveIndexIdentity,
        plan: &ReclaimIndexAugmentation,
    ) -> crate::error::Result<()> {
        if self.batch != batch
            || self.previous != previous
            || self.current != current
            || self
                .plan
                .fingerprint()
                .map_err(|e| TapeError::Catalog(e.to_string()))?
                != plan
                    .fingerprint()
                    .map_err(|e| TapeError::Catalog(e.to_string()))?
        {
            return Err(TapeError::Refused {
                reason: "Observe设备不能替换原checkpoint完整身份".into(),
            });
        }
        plan.require_checkpoint_binding(self.scope, batch, previous, current)
            .map_err(|e| TapeError::Catalog(e.to_string()))
    }
    pub(crate) fn resume_volume(
        &self,
        session: crate::ltfs::volume::VolumeSession,
    ) -> crate::error::Result<crate::ltfs::volume::LtfsVolume<'_>> {
        if !session.matches_device(&self.transport, self.key)
            || crate::managed_catalog::ActiveIndexIdentity::from(
                session.committed_index_construct(),
            ) != self.current
        {
            return Err(TapeError::Refused {
                reason: "Observe卷会话不是原设备/owner/checkpoint".into(),
            });
        }
        self.verify()?;
        session.resume(&self.transport, self.key)
    }
}

#[derive(Clone, Copy)]
enum ObserveAuthority<'a> {
    Fresh(&'a ReclaimGroupExecution),
    Known(&'a crate::managed_catalog::OriginalObservationPermit),
    Legacy,
}

fn observe_group_io<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    scope: &'a crate::managed_catalog::ReclaimGroupObservation,
    batch: Uuid,
    previous: crate::managed_catalog::ActiveIndexIdentity,
    current: crate::managed_catalog::ActiveIndexIdentity,
    plan: ReclaimIndexAugmentation,
    authority: ObserveAuthority<'a>,
) -> std::result::Result<ObserveReclaimGroupIo<'a>, ReadError> {
    if scope.owner() != owner.round
        || owner.key.round() != owner.round
        || scope.parent().serial() != control.serial
        || scope.parent().intent()["namespace"] != control.route.route.namespace.to_string()
    {
        return Err(lost("Observe设备必须绑定当前owner与原带库范围"));
    }
    plan.require_checkpoint_binding(scope, batch, previous, current)
        .map_err(parent_error)?;
    let drive = device(owner, scope.destination_drive_serial())?;
    let transport = owner.devices[drive].dev.as_ref();
    let original = move || {
        futures_executor::block_on(async {
            match authority {
                ObserveAuthority::Fresh(fresh) => {
                    control
                        .catalog
                        .check_fresh_reclaim_group_observation(fresh, scope)
                        .await
                }
                ObserveAuthority::Known(original) => {
                    control
                        .catalog
                        .check_original_reclaim_group_observation(original, scope)
                        .await
                }
                ObserveAuthority::Legacy if !scope.parent().spec().group_only() => {
                    control.catalog.check_reclaim_group_observation(scope).await
                }
                ObserveAuthority::Legacy => Err(crate::managed_catalog::CatalogError::Invalid(
                    "group-only原观察缺一次持久许可",
                )),
            }
        })
        .map_err(|e: crate::managed_catalog::CatalogError| TapeError::Catalog(e.to_string()))
    };
    let guard: Box<dyn CheckedTransport + 'a> = if matches!(authority, ObserveAuthority::Fresh(_)) {
        let round = owner.round;
        let key = owner.key;
        let medium = transport.medium_change_token();
        Box::new(fdb_operation_guard::Guard::new_bracketed_read(
            transport,
            move || {
                use crate::daemon::permission::Phase;
                if !(permit.allows(round, Phase::Serving) || permit.allows(round, Phase::Execution))
                    || transport.medium_change_token() != medium
                {
                    return Err(TapeError::OwnershipLost {
                        device: transport.identity().into(),
                        detail: "原回收目标观察多数派许可或介质身份已失效".into(),
                    });
                }
                crate::scsi::reservation::verify_holder_without_ua_retry(transport, key)?;
                original()
            },
        ))
    } else {
        Box::new(fdb_loading::operation_guard(
            owner, permit, transport, original,
        ))
    };
    Ok(ObserveReclaimGroupIo {
        catalog: &control.catalog,
        scope,
        key: owner.key,
        batch,
        previous,
        current,
        plan,
        transport: ReadOnlyGroupTransport { guard },
    })
}

pub(super) fn capture_prepared_group<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    group: &'a crate::managed_catalog::ReclaimCopyGroup,
    layout: &'a ReclaimLayout,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<CapturedIndexSource, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        if group.preparation_owner() != owner.round || owner.key.round() != owner.round {
            return Err(lost("准备捕获必须属于当前owner与原组"));
        }
        let drive = device(owner, group.destination_drive_serial())?;
        let checked =
            fdb_loading::operation_guard(owner, permit, owner.devices[drive].dev.as_ref(), || {
                futures_executor::block_on(control.catalog.check_reclaim_group_preparation(
                    parent,
                    owner.round,
                    group,
                ))
                .map_err(|e| TapeError::Catalog(e.to_string()))
            });
        let readonly = ReadOnlyGroupTransport {
            guard: Box::new(checked),
        };
        let budget = budget(owner, control);
        let hint = control
            .catalog
            .load_reclaim_group_destination_history_hint(
                parent,
                owner.round,
                layout,
                &budget.index_scratch,
            )
            .await
            .map_err(parent_error)?;
        let label = crate::ltfs::volume::read_label_only(&readonly).map_err(lost)?;
        let capture =
            crate::ltfs::recovery::capture_reclaim_preparation_index_source_with_history_hint(
                &crate::tape::commands::TapeDrive::new(&readonly),
                &label,
                &budget,
                hint,
            )
            .map_err(lost)?;
        readonly.guard.verify().map_err(lost)?;
        Ok(capture)
    })
}

pub(super) struct TargetPreflight {
    capture: CapturedIndexSource,
    binding: TargetPreflightBinding,
    scan: crate::managed_catalog::VolumeScan,
    session: Option<VolumeSession>,
}

#[derive(Clone, Copy)]
pub(super) enum TargetPreflightScope<'a> {
    Selecting(
        (
            Uuid,
            crate::managed_catalog::VolumeScan,
            crate::managed_catalog::ActiveIndexIdentity,
        ),
    ),
    Selected(&'a ReclaimCopyGroup),
}

struct TargetPreflightBinding {
    parent: serde_json::Value,
    group: Uuid,
    batch: Uuid,
    drive: String,
    execution: (u64, usize),
    epoch: u64,
    medium: Option<[u8; 32]>,
}

impl TargetPreflightBinding {
    fn matches_group(&self, parent: &ReclaimReservation, group: &ReclaimCopyGroup) -> bool {
        self.parent == *parent.intent()
            && self.group == group.id()
            && self.batch == group.batch_id()
            && self.drive == group.destination_drive_serial()
            && self.execution.0 == group.preparation_owner()
    }

    fn matches_drive(&self, drive: &dyn crate::scsi::transport::TapeTransport) -> bool {
        drive.execution_guard() == Some(self.execution)
            && drive.session_epoch() == Some(self.epoch)
            && drive.medium_change_token() == self.medium
    }
}

/// The session remains on the same continuously owned drive, in this walk.
/// Only the fresh admitted group can consume it after rechecking the position.
pub(super) struct PreparedTargetMount {
    binding: TargetPreflightBinding,
    session: VolumeSession,
}

pub(super) fn start_target_preflight<'a>(
    owner: &'a mut Active,
    permit: &Arc<Permission>,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    scope: TargetPreflightScope<'_>,
    layout: &ReclaimLayout,
    index_bytes: u64,
) -> std::result::Result<parallel::DriveWorker<'a, Option<TargetPreflight>>, ReadError> {
    if owner.key.round() != owner.round
        || matches!(scope, TargetPreflightScope::Selected(group)
            if group.preparation_owner() != owner.round
                || group.destination_drive_serial() != layout.destination.drive_serial)
    {
        return Err(lost("目标预检必须属于当前owner与原组"));
    }
    let drive = device(owner, &layout.destination.drive_serial)?;
    let mut budget = budget(owner, control);
    budget.index_scratch.max_bytes = index_bytes;
    let catalog = control.catalog.clone();
    let permit = permit.clone();
    let round = owner.round;
    let key = owner.key;
    let parent = parent.clone();
    let (group_id, batch, selected, selecting) = match scope {
        TargetPreflightScope::Selecting(plan) => (plan.0, plan.0, None, Some(plan)),
        TargetPreflightScope::Selected(group) => {
            (group.id(), group.batch_id(), Some(group.clone()), None)
        }
    };
    let layout = layout.clone();
    parallel::DriveWorker::start(owner, drive, move |transport| {
        let check_catalog = || {
            futures_executor::block_on(async {
                catalog
                    .require_reclaim_read_lane(&parent, round, layout.destination.volume)
                    .await?;
                if let Some(group) = selected.as_ref() {
                    return catalog
                        .check_reclaim_group_destination_preflight(&parent, round, group, &layout)
                        .await;
                }
                let observed = catalog
                    .check_early_reclaim_destination_preflight(&parent, round, &layout)
                    .await?;
                if observed != selecting {
                    return Err(crate::managed_catalog::CatalogError::Invalid(
                        "选择回收组期间原预检计划、扫描或卷头改变",
                    ));
                }
                let (_, scan, head) = observed.expect("selecting scope");
                Ok((scan, head))
            })
            .map_err(|error| TapeError::Catalog(error.to_string()))
        };
        let expected = check_catalog().map_err(lost)?;
        let _activity = permit.begin_device_activity(round).map_err(lost)?;
        let medium = transport.medium_change_token();
        let execution = (round, Arc::as_ptr(&permit) as usize);
        let checked = fdb_operation_guard::Guard::new_bracketed_read(transport, || {
            if !permit.allows(round, crate::daemon::permission::Phase::Serving)
                || transport.execution_guard() != Some(execution)
                || transport.medium_change_token() != medium
                || check_catalog()? != expected
            {
                return Err(TapeError::OwnershipLost {
                    device: transport.identity().into(),
                    detail: "回收目标预检执行权或原卷状态已经改变".into(),
                });
            }
            crate::scsi::reservation::verify_holder_without_ua_retry(transport, key)
        });
        let start = std::time::Instant::now();
        log::info!(
            "回收组目标预检开始: 操作={} 组={} 驱动器={} 选择并行={}",
            parent.intent()["operation"],
            group_id,
            layout.destination.drive_serial,
            selecting.is_some(),
        );
        let hint = futures_executor::block_on(catalog.load_reclaim_group_destination_history_hint(
            &parent,
            round,
            &layout,
            &budget.index_scratch,
        ))
        .map_err(parent_error)?;
        let label = crate::ltfs::volume::read_label_only(&checked).map_err(lost)?;
        if label.volume_uuid != layout.destination.volume {
            return Err(lost("回收目标预检原固定卷标签改变"));
        }
        let capture =
            crate::ltfs::recovery::capture_reclaim_preparation_index_source_with_history_hint(
                &crate::tape::commands::TapeDrive::new(&checked),
                &label,
                &budget,
                hint,
            );
        checked.verify_proof().map_err(lost)?;
        let capture = match capture {
            Ok(capture) => capture,
            Err(TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. }) => {
                log::info!(
                    "回收组目标预检暂存不足: 操作={} 组={} 驱动器={} 耗时={:.3}s",
                    parent.intent()["operation"],
                    group_id,
                    layout.destination.drive_serial,
                    start.elapsed().as_secs_f64(),
                );
                return Ok(None);
            }
            Err(error) => return Err(lost(error)),
        };
        if crate::managed_catalog::ActiveIndexIdentity::from(capture.construct()) != expected.1 {
            return Err(lost("回收目标预检实读卷头和原FDB卷头不同"));
        }
        let session = prepare_target_mount(&checked, key, &capture, label, index_bytes);
        checked.verify_proof().map_err(lost)?;
        let session = session.map_err(lost)?;
        let epoch = transport
            .session_epoch()
            .ok_or_else(|| lost("回收目标预检设备会话失效"))?;
        log::info!(
            "回收组目标预检完成: 操作={} 组={} 驱动器={} 耗时={:.3}s 位置提示命中={}",
            parent.intent()["operation"],
            group_id,
            layout.destination.drive_serial,
            start.elapsed().as_secs_f64(),
            capture.history_hint_used(),
        );
        Ok(Some(TargetPreflight {
            capture,
            binding: TargetPreflightBinding {
                parent: parent.intent().clone(),
                group: group_id,
                batch,
                drive: layout.destination.drive_serial,
                execution,
                epoch,
                medium,
            },
            scan: expected.0,
            session,
        }))
    })
}

fn prepare_target_mount(
    transport: &dyn crate::scsi::transport::TapeTransport,
    key: crate::scsi::reservation::ReservationKey,
    capture: &CapturedIndexSource,
    label: crate::ltfs::label::LtfsLabel,
    index_bytes: u64,
) -> crate::error::Result<Option<VolumeSession>> {
    use crate::ltfs::recovery::IndexSourceScope;
    // Limit each additional in-memory index construct on the parallel path.
    const MAX_PARALLEL_MOUNT_INDEX_BYTES: u64 = 16 << 20;
    let mount_bytes = index_bytes
        .saturating_sub(capture.scratch_reserved_bytes())
        .min(MAX_PARALLEL_MOUNT_INDEX_BYTES);
    let IndexSourceScope::Current {
        dp_eod,
        ip_generation,
        history_truncated: false,
        ..
    } = capture.scope()
    else {
        return Ok(None);
    };
    if capture.construct().incremental
        || capture.source_bytes() > mount_bytes
        || dp_eod > u64::from(u32::MAX)
    {
        return Ok(None);
    }
    let mut volume = match LtfsVolume::mount_for_reclaim_preflight(
        transport,
        label,
        capture.construct(),
        dp_eod,
        mount_bytes as usize,
    ) {
        Ok(volume) => volume,
        Err(TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    volume.set_reservation_guard(Some(key));
    let mounted_head = volume.verified_committed_index_construct()?;
    let recovery = volume.recovery();
    if mounted_head != capture.construct()
        || recovery.append_position != Some(dp_eod)
        || recovery.ip.last_index.as_ref().map(|i| i.index.generation) != ip_generation
    {
        return Err(TapeError::Refused {
            reason: "回收目标预挂载与原预检实读卷头或带尾不符".into(),
        });
    }
    volume
        .into_session()
        .map(Some)
        .ok_or_else(|| TapeError::Refused {
            reason: "回收目标预挂载无法保留原设备会话".into(),
        })
}

impl TargetPreflight {
    pub(super) async fn confirm_selection(
        &self,
        owner: &Active,
        permit: &Arc<Permission>,
        control: &FdbOwnerControl,
        parent: &ReclaimReservation,
        group: &ReclaimCopyGroup,
        layout: &ReclaimLayout,
    ) -> std::result::Result<(), ReadError> {
        let drive = owner.devices[device(owner, group.destination_drive_serial())?]
            .dev
            .as_ref();
        let live = || {
            self.binding.matches_group(parent, group)
                && self.binding.execution == (owner.round, Arc::as_ptr(permit) as usize)
                && self.binding.matches_drive(drive)
                && permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
        };
        if !live() {
            return Err(lost("回收预检选择绑定期间原驱动会话或组身份改变"));
        }
        let observed = control
            .catalog
            .check_reclaim_group_destination_preflight(parent, owner.round, group, layout)
            .await
            .map_err(parent_error)?;
        if observed != (self.scan, self.capture.construct().into()) {
            return Err(lost("回收预检选择绑定期间原扫描或卷头改变"));
        }
        crate::scsi::reservation::verify_holder_without_ua_retry(drive, owner.key).map_err(lost)?;
        if !live() {
            return Err(lost("回收预检选择绑定期间原驱动会话或组身份改变"));
        }
        Ok(())
    }

    pub(super) async fn bind(
        self,
        owner: &Active,
        permit: &Arc<Permission>,
        control: &FdbOwnerControl,
        parent: &ReclaimReservation,
        group: &ReclaimCopyGroup,
    ) -> std::result::Result<(CapturedIndexSource, Option<PreparedTargetMount>), ReadError> {
        let drive = owner.devices[device(owner, group.destination_drive_serial())?]
            .dev
            .as_ref();
        if !self.binding.matches_group(parent, group)
            || self.binding.execution != (owner.round, Arc::as_ptr(permit) as usize)
            || !self.binding.matches_drive(drive)
            || !permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
        {
            return Err(lost("回收目标预检失去原驱动会话或组身份"));
        }
        control
            .catalog
            .check_reclaim_group_preparation(parent, owner.round, group)
            .await
            .map_err(parent_error)?;
        crate::scsi::reservation::verify_holder_without_ua_retry(drive, owner.key).map_err(lost)?;
        if !self.binding.matches_drive(drive)
            || !permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
        {
            return Err(lost("回收目标预检复用期间设备或执行许可改变"));
        }
        log::info!(
            "回收组目标预检已复用: 操作={} 组={} 驱动器={}",
            parent.intent()["operation"],
            group.id(),
            group.destination_drive_serial(),
        );
        let prepared = self.session.map(|session| PreparedTargetMount {
            binding: self.binding,
            session,
        });
        Ok((self.capture, prepared))
    }
}

fn checkpoint_error(stage: &str, error: crate::managed_catalog::CatalogError) -> ReadError {
    match parent_error(error) {
        ReadError::Busy(reason) => ReadError::Busy(format!("回收检查点 {stage}: {reason}")),
        ReadError::Lost(reason) => ReadError::Lost(format!("回收检查点 {stage}: {reason}")),
        other => other,
    }
}

/// Only the executor's future catalog-adoption factory can construct this
/// probe capability. It deliberately has no VolumeSession/Actual requirement.
/// The original issuing epoch is not the current adopted owner round.
pub(in crate::daemon) struct OriginalAttemptProbeIo<'a> {
    scope: &'a crate::managed_catalog::OriginalAttemptObservation,
    key: crate::scsi::reservation::ReservationKey,
    transport: ReadOnlyGroupTransport<'a>,
}
impl OriginalAttemptProbeIo<'_> {
    pub(in crate::daemon) fn scope(&self)->&crate::managed_catalog::OriginalAttemptObservation {self.scope}
    pub(in crate::daemon) fn key(&self)->crate::scsi::reservation::ReservationKey {self.key}
    pub(in crate::daemon) fn transport(&self)->&dyn crate::scsi::transport::TapeTransport {&self.transport}
    pub(in crate::daemon) fn verify(&self)->crate::error::Result<()> {self.transport.guard.verify()}
}

fn original_attempt_probe_io<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    scope: &'a crate::managed_catalog::OriginalAttemptObservation,
    observation: Option<&'a crate::managed_catalog::OriginalObservationPermit>,
) -> std::result::Result<OriginalAttemptProbeIo<'a>, ReadError> {
    if scope.owner()!=owner.round || owner.key.round()!=owner.round
        || scope.parent().serial()!=control.serial
        || scope.namespace()!=control.route.route.namespace {
        return Err(lost("原attempt只读探测必须绑定当前owner/原带库"));
    }
    if scope.parent().spec().group_only() && observation.is_none() {
        return Err(lost("group-only原探测缺一次持久观察许可"));
    }
    let drive=device(owner,scope.destination_drive_serial())?;
    if owner.devices[drive].dev.session_epoch().is_none() {
        return Err(lost("原attempt设备没有可信session epoch，禁止探测"));
    }
    let guard = fdb_loading::operation_guard(
        owner,
        permit,
        owner.devices[drive].dev.as_ref(),
        move || {
            futures_executor::block_on(async {
                match observation {
                    Some(original) => {
                        control
                            .catalog
                            .check_original_reclaim_observation(original)
                            .await
                    }
                    None => control.catalog.check_reclaim_original_attempt(scope).await,
                }
            })
            .map_err(|error: crate::managed_catalog::CatalogError| {
                TapeError::Catalog(error.to_string())
            })
        },
    );
    let io = OriginalAttemptProbeIo {
        scope,
        key: owner.key,
        transport: ReadOnlyGroupTransport {
            guard: Box::new(guard),
        },
    };
    io.verify().map_err(lost)?;
    Ok(io)
}
/// Original Header3 may not yet exist. This returns read-only physical facts,
/// never classifies NoCommit or grants a repair/append/publication capability.
pub(in crate::daemon::executor) async fn probe_original<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    id: Uuid,
) -> std::result::Result<crate::daemon::fdb_media::CapturedAttemptEvidence, ReadError> {
    let scope = control
        .catalog
        .adopt_reclaim_original_attempt(parent, owner.round, id)
        .await
        .map_err(parent_error)?;
    let io = original_attempt_probe_io(owner, permit, control, &scope, None)?;
    let evidence = crate::daemon::fdb_media::probe_original_attempt(&io, &budget(owner, control))
        .await
        .map_err(lost)?;
    #[cfg(test)]
    probe_seal_test_hook::record(parent,io.transport());
    // The media finalizer already checked authority against its sealed Some
    // epoch. Only pure binding checks may follow that final physical proof.
    evidence.require_scope(&scope).map_err(lost)?;
    Ok(evidence)
}

pub(super) struct CompletedCheckpoint {
    pub(super) plan: ReclaimIndexAugmentation,
    pub(super) files: Vec<PreparedActiveFile>,
    pub(super) readbacks: Vec<crate::daemon::fdb_read::ReclaimGroupMemberReadback>,
    pub(super) prepared_format_source: Option<Box<PreparedReclaimFormatSource>>,
}

pub(super) fn execute<'a>(
    owner: &'a mut Active,
    permit: &'a Arc<Permission>,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    scope: ReclaimGroupExecution,
    uploads: &'a [ActiveMediaUpload],
    mut target: CapturedIndexSource,
    prepared_mount: Option<PreparedTargetMount>,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<CompletedCheckpoint, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        if scope.parent().intent() != parent.intent() {
            return Err(lost("回收检查点必须使用原父执行范围"));
        }
        if layout.destination.drive_serial != scope.destination_drive_serial() {
            return Err(lost("回收检查点不能替换原目的驱动器"));
        }
        let drive = device(owner, scope.destination_drive_serial())?;
        let io = fresh_group_io(owner, permit, control, &scope, uploads, prepared_mount)?;
        let pending = commit_reclaim_media_checkpoint(
            &io,
            &target,
            usize::try_from(control.storage.index_bytes).unwrap_or(usize::MAX),
        )
        .await
        .map_err(lost)?;
        #[cfg(test)]
        if s4_test_hook::after_s4(parent, scope.group().id(), pending.batch_id(), pending.previous_index(), pending.current_index(), drive, pending.files()) {
            return Err(lost("测试仅中断原Full S4成功后的调用，保留原attempt等待接管"));
        }
        io.verify().map_err(lost)?;
        let batch = pending.batch_id();
        let previous = pending.previous_index();
        let current = pending.current_index();
        let expected_plan = pending.plan().clone();
        let session = pending.into_session().map_err(lost)?;
        drop(io);
        let history = target.take_full_history_hint();
        drop(target);

        let result = if parent.spec().group_only()
            && permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
            && let Some(storage) = source_preparation_budget(control.storage)
        {
            let source_drive = device(owner, &layout.source.drive_serial)?;
            if source_drive == drive {
                return Err(lost("回收并行读取须使用两台不同的原固定驱动器"));
            }
            let source_parent = parent.clone();
            let source_layout = layout.clone();
            let source_permit = permit.clone();
            let source_control = FdbOwnerControl {
                catalog: control.catalog.clone(),
                serial: control.serial.clone(),
                route: control.route,
                storage,
            };
            let spool = owner.files.spool_dir().to_path_buf();
            let key = owner.key;
            let (mut worker, start) =
                super::parallel::DriveWorker::start_paused(owner, source_drive, move |source| {
                    prepare_format_source(
                        source,
                        &source_permit,
                        &source_control,
                        &source_parent,
                        &source_layout,
                        key,
                        &spool,
                    )
                })?;
            let mut result = observe_checkpoint(
                worker.owner(),
                permit,
                control,
                parent,
                scope,
                drive,
                batch,
                previous,
                current,
                expected_plan,
                session,
                history,
                Some(start),
            )
            .await?;
            result.prepared_format_source = worker.finish()?;
            result
        } else {
            observe_checkpoint(
                owner,
                permit,
                control,
                parent,
                scope,
                drive,
                batch,
                previous,
                current,
                expected_plan,
                session,
                history,
                None,
            )
            .await?
        };
        #[cfg(test)]
        readback_seal_test_hook::record(parent, owner.devices[drive].dev.as_ref(), false);
        #[cfg(test)]
        readback_seal_test_hook::before_monitor(parent, owner, control);
        // Both drives have returned. Only FDB publication work may follow
        // the target's sealed final epoch before settlement.
        fdb_monitor::index_state(owner, control, &layout.destination.barcode, current, false)
            .await?;
        #[cfg(test)]
        readback_seal_test_hook::record(parent, owner.devices[drive].dev.as_ref(), true);
        Ok(result)
    })
}

fn source_preparation_budget(
    mut storage: crate::local_read_storage::ReadStorageBudget,
) -> Option<crate::local_read_storage::ReadStorageBudget> {
    // Full capture can keep the history hint, head replay and history audit
    // while verifying DP/IP. Each of these opens a 16 MiB scratch reservation.
    const SMALL_FULL_CAPTURE_BYTES: u64 = 4 * (16 << 20);
    // Each speculative scratch expansion must leave room for the target
    // readback's entire original limit, even if the target allocates it later.
    let target_index_bytes = storage.index_bytes;
    storage.index_bytes = SMALL_FULL_CAPTURE_BYTES;
    storage.headroom_bytes = storage.headroom_bytes.checked_add(target_index_bytes)?;
    (target_index_bytes >= SMALL_FULL_CAPTURE_BYTES).then_some(storage)
}

#[cfg(test)]
#[test]
fn source_preparation_reserves_the_original_target_index_budget() {
    use crate::local_read_storage::ReadStorageBudget;

    let target = ReadStorageBudget {
        index_bytes: 64 << 20,
        headroom_bytes: 16 << 20,
        ..ReadStorageBudget::default()
    };
    let source = source_preparation_budget(target).unwrap();
    assert_eq!(source.index_bytes, 64 << 20);
    assert_eq!(source.headroom_bytes, 80 << 20);
    assert!(
        source_preparation_budget(ReadStorageBudget {
            index_bytes: (64 << 20) - 1,
            ..target
        })
        .is_none()
    );
    assert!(
        source_preparation_budget(ReadStorageBudget {
            headroom_bytes: u64::MAX,
            ..target
        })
        .is_none()
    );
}

fn prepare_format_source(
    source: &dyn crate::scsi::transport::TapeTransport,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    key: crate::scsi::reservation::ReservationKey,
    spool: &std::path::Path,
) -> std::result::Result<Option<Box<PreparedReclaimFormatSource>>, ReadError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(lost)?;
    runtime.block_on(async {
        let require_source = || async {
            control
                .catalog
                .require_reclaim_read_lane(parent, key.round(), layout.source.volume)
                .await?;
            if control
                .catalog
                .reclaim_layout(parent, key.round())
                .await?
                .as_ref()
                != Some(layout)
            {
                return Err(crate::managed_catalog::CatalogError::Invalid(
                    "并行读取的原固定回收布局改变",
                ));
            }
            Ok::<(), crate::managed_catalog::CatalogError>(())
        };
        require_source().await.map_err(parent_error)?;
        let checked = fdb_operation_guard::Guard::new_bracketed_read(source, || {
            futures_executor::block_on(require_source())
                .map_err(|error| TapeError::Catalog(error.to_string()))
        });
        let reader = ActiveMediaReader {
            catalog: &control.catalog,
            device: &checked,
            serial: &control.serial,
            route: control.route,
            key,
            permission: permit,
            storage: control.storage,
        };
        let started = std::time::Instant::now();
        log::info!(
            "回收组并行读取源格式化来源开始: 操作={} 驱动器={}",
            parent.intent()["operation"],
            layout.source.drive_serial,
        );
        let proof = reader.read_reclaim_format_source(parent, spool).await;
        let proof = match proof.map_err(crate::local_read_storage::classify_error) {
            Ok(proof) => proof,
            Err(error @ (TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. })) => {
                checked.verify_proof().map_err(lost)?;
                log::debug!("回收组并行读取源格式化来源暂存预算不足: {error}");
                return Ok(None);
            }
            Err(error) => return Err(lost(error)),
        };
        checked.verify_proof().map_err(lost)?;
        let Some(device_epoch) = source.session_epoch() else {
            return Ok(None);
        };
        log::info!(
            "回收组并行读取源格式化来源完成: 操作={} 驱动器={} 耗时={:.3}s",
            parent.intent()["operation"],
            layout.source.drive_serial,
            started.elapsed().as_secs_f64(),
        );
        Ok(Some(Box::new(PreparedReclaimFormatSource {
            proof,
            parent: parent.intent().clone(),
            round: key.round(),
            source: layout.source.clone(),
            device: source.identity().into(),
            device_epoch,
        })))
    })
}
// Fresh and Observe hold different device guards and cannot execute together.
// Construct the later phase on the heap after dropping the Fresh guard, rather
// than reserving all its poll temporaries while the body checkpoint is active.
#[allow(clippy::too_many_arguments)]
fn observe_checkpoint<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    scope: ReclaimGroupExecution,
    drive: usize,
    batch: Uuid,
    previous: crate::managed_catalog::ActiveIndexIdentity,
    current: crate::managed_catalog::ActiveIndexIdentity,
    expected_plan: ReclaimIndexAugmentation,
    session: crate::daemon::fdb_media::ReclaimCheckpointSession,
    history: Option<crate::ltfs::recovery::FullHistoryDirectory>,
    start_source: Option<super::parallel::StartReadWorker>,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<CompletedCheckpoint, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        // Once a proof is saved, Fresh is intentionally invalid. This new
        // guard validates the original attempt without requiring its proof
        // before capture, and remains valid after the index CAS.
        let observation = control
            .catalog
            .open_reclaim_group_observation(parent, owner.round, scope.group().id())
            .await
            .map_err(|error| checkpoint_error("open_observation", error))?;
        let io = observe_group_io(
            owner,
            permit,
            control,
            &observation,
            batch,
            previous,
            current,
            expected_plan,
            ObserveAuthority::Fresh(&scope),
        )?;
        let pending = session.resume(&io).map_err(lost)?;
        let capture = capture_fresh_reclaim_index(
            io.transport(),
            pending.volume().label(),
            &budget(owner, control),
            history,
        )
        .map_err(lost)?;
        log::debug!(
            "回收组目标写后索引实读: 操作={} 组={} 位置提示命中={}",
            parent.intent()["operation"],
            scope.group().id(),
            capture.history_hint_used(),
        );
        let proof = pending
            .plan()
            .observe(&capture)
            .map_err(|error| checkpoint_error("capture_a_plan", error))?;
        io.verify().map_err(lost)?;
        control
            .catalog
            .record_reclaim_index_observation(parent, owner.round, scope.group().id(), &proof)
            .await
            .map_err(|error| checkpoint_error("record_observation", error))?;
        io.verify().map_err(lost)?;
        let first = pending
            .files()
            .first()
            .ok_or_else(|| lost("原回收检查点成员为空"))?;
        match control
            .catalog
            .record_active_volume_index(
                parent.serial(),
                owner.round,
                first.receipt.volume_id,
                pending.batch_id(),
                pending.previous_index(),
                pending.current_index(),
                pending.plan().target_file_count(),
            )
            .await
            .map_err(|error| checkpoint_error("record_index", error))?
        {
            ActiveIndexOutcome::Recorded | ActiveIndexOutcome::AlreadyRecorded => {}
            outcome => return Err(lost(format!("原回收检查点索引登记未成功: {outcome:?}"))),
        }
        io.verify().map_err(lost)?;
        let (commit, plan) = pending.into_parts();
        let committed = publish_active_media_batch(&control.catalog, commit)
            .await
            .map_err(lost)?;
        if let Err(error) = control
            .catalog
            .save_reclaim_group_destination_history_hint(&observation, &capture)
            .await
        {
            log::warn!(
                "回收组目标位置提示未确认: 操作={} 组={} 错误={error}",
                parent.intent()["operation"],
                scope.group().id(),
            );
        }
        io.verify().map_err(lost)?;
        let files = committed.files().to_vec();
        let original = committed
            .into_volume()
            .into_session()
            .ok_or_else(|| lost("原回收提交卷会话失效，必须恢复原checkpoint"))?;
        let mut contexts = Vec::with_capacity(files.len());
        for ordinal in scope.group().ordered_ordinals() {
            contexts.push(
                control
                    .catalog
                    .reclaim_group_member_read_context(
                        parent,
                        owner.round,
                        scope.group().id(),
                        batch,
                        ordinal,
                    )
                    .await
                    .map_err(|error| {
                        checkpoint_error(&format!("member_context ordinal={ordinal}"), error)
                    })?,
            );
        }
        #[cfg(test)]
        readback_seal_test_hook::before_destination_read(parent, drive);
        let concurrent = start_source.is_some();
        let started = std::time::Instant::now();
        if let Some(start) = start_source {
            io.verify().map_err(lost)?;
            log::info!(
                "回收组并行目标正文与组尾核验开始: 操作={} 驱动器={}",
                parent.intent()["operation"],
                scope.destination_drive_serial(),
            );
            start.start();
        }
        let readbacks = reader(owner, permit, control, drive)
            .read_reclaim_group_destination(
                &io,
                original,
                capture,
                &plan,
                contexts,
                owner.files.spool_dir(),
            )
            .await
            .map_err(lost)?;
        if concurrent {
            log::info!(
                "回收组并行目标正文与组尾核验完成: 操作={} 驱动器={} 耗时={:.3}s",
                parent.intent()["operation"],
                scope.destination_drive_serial(),
                started.elapsed().as_secs_f64(),
            );
        }
        Ok(CompletedCheckpoint {
            plan,
            files,
            readbacks,
            prepared_format_source: None,
        })
    })
}

fn capture_fresh_reclaim_index(
    transport: &dyn crate::scsi::transport::TapeTransport,
    session_label: &crate::ltfs::label::LtfsLabel,
    budget: &crate::ltfs::recovery::RecoveryBudget,
    history: Option<crate::ltfs::recovery::FullHistoryDirectory>,
) -> crate::error::Result<CapturedIndexSource> {
    // The original write session ended in DP. Its label is only provisional
    // parsing geometry until the physical P0 label is read and compared.
    let capture = crate::ltfs::recovery::capture_complete_index_source_with_history_hint(
        &crate::tape::commands::TapeDrive::new(transport),
        session_label,
        budget,
        history,
    )?;
    let actual = crate::ltfs::volume::read_label_only(transport)?;
    if actual.to_xml()? != session_label.to_xml()? {
        return Err(TapeError::Ltfs("回收写入会话与目标实际介质标签不同".into()));
    }
    Ok(capture)
}

/// Recover only the exact persisted Full S4. Missing Actual/current or a
/// mismatching head stays unresolved; this lane has no append/repair authority.
pub(super) fn resume_original<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    group: Uuid,
    observation: Option<&'a crate::managed_catalog::OriginalObservationPermit>,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<CompletedCheckpoint, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        let legacy;
        let original = if let Some(observation) = observation {
            if observation.parent().intent() != parent.intent() || observation.group_id() != group {
                return Err(lost("原观察许可不属于此父任务/组"));
            }
            observation.scope()
        } else {
            if parent.spec().group_only() {
                return Err(lost("group-only原恢复缺一次持久观察许可"));
            }
            legacy = control
                .catalog
                .adopt_reclaim_original_attempt(parent, owner.round, group)
                .await
                .map_err(|e| checkpoint_error("adopt_original", e))?;
            &legacy
        };
        if original.destination_drive_serial() != layout.destination.drive_serial
            || original.destination_barcode() != layout.destination.barcode
        {
            return Err(lost("原S4恢复不能替换目的驱动器或磁带"));
        }
        let facts = if let Some(observation) = observation {
            let io =
                original_attempt_probe_io(owner, permit, control, original, Some(observation))?;
            let mam = read_original_mam(&io).map_err(lost)?;
            let facts =
                crate::daemon::fdb_media::probe_original_attempt(&io, &budget(owner, control))
                    .await
                    .map_err(lost)?;
            facts.require_scope(original).map_err(lost)?;
            mam.require_facts(&facts).map_err(lost)?;
            facts
        } else {
            probe_original(owner, permit, control, parent, group).await?
        };
        facts.require_scope(original).map_err(lost)?;
        let plan = original
            .actual()
            .ok_or_else(|| lost("原attempt尚无Actual；保持未结清，禁止重写正文"))?
            .clone();
        let actual = facts
            .current()
            .ok_or_else(|| lost("原attempt没有干净Current；保持未结清，禁止修尾或新attempt"))?;
        let expected = plan
            .observe(actual)
            .map_err(|e| checkpoint_error("classify_known_full", e))?;
        let context = control
            .catalog
            .open_original_checkpoint(&original, &expected)
            .await
            .map_err(|e| checkpoint_error("open_original_checkpoint", e))?;
        let current = context.current_index();
        let batch = context.batch_id();
        let drive = device(owner, original.destination_drive_serial())?;
        let io = observe_group_io(
            owner,
            permit,
            control,
            context.scope(),
            batch,
            context.previous_index(),
            current,
            plan.clone(),
            observation
                .map(ObserveAuthority::Known)
                .unwrap_or(ObserveAuthority::Legacy),
        )?;
        let committed = crate::daemon::fdb_media::resume_original_reclaim_checkpoint(
            &io,
            &context,
            &budget(owner, control),
        )
        .await
        .map_err(lost)?;
        let label = crate::ltfs::volume::read_label_only(io.transport()).map_err(lost)?;
        let capture = crate::ltfs::recovery::capture_complete_index_source(
            &crate::tape::commands::TapeDrive::new(io.transport()),
            &label,
            &budget(owner, control),
        )
        .map_err(lost)?;
        context.require_capture(&capture).map_err(parent_error)?;
        let proof = plan.observe(&capture).map_err(parent_error)?;
        io.verify().map_err(lost)?;
        control
            .catalog
            .record_reclaim_index_observation(parent, owner.round, group, &proof)
            .await
            .map_err(|e| checkpoint_error("recover_record_observation", e))?;
        io.verify().map_err(lost)?;
        match control
            .catalog
            .record_active_volume_index(
                parent.serial(),
                owner.round,
                current.volume_uuid,
                batch,
                context.previous_index(),
                current,
                plan.target_file_count(),
            )
            .await
            .map_err(|e| checkpoint_error("recover_record_index", e))?
        {
            ActiveIndexOutcome::Recorded | ActiveIndexOutcome::AlreadyRecorded => {}
            outcome => return Err(lost(format!("原S4恢复索引登记未成功: {outcome:?}"))),
        }
        io.verify().map_err(lost)?;
        let committed = crate::daemon::fdb_media::publish_original_reclaim_checkpoint(
            &control.catalog,
            &context,
            committed,
        )
        .await
        .map_err(lost)?;
        io.verify().map_err(lost)?;
        let files = committed.files().to_vec();
        let session = committed
            .into_volume()
            .into_session()
            .ok_or_else(|| lost("原S4恢复干净卷会话失效"))?;
        let mut contexts = Vec::with_capacity(files.len());
        for ordinal in context.scope().group().ordered_ordinals() {
            contexts.push(
                control
                    .catalog
                    .reclaim_group_member_read_context(parent, owner.round, group, batch, ordinal)
                    .await
                    .map_err(|e| checkpoint_error("recover_member_context", e))?,
            );
        }
        let readbacks = reader(owner, permit, control, drive)
            .read_reclaim_group_destination(
                &io,
                session,
                capture,
                &plan,
                contexts,
                owner.files.spool_dir(),
            )
            .await
            .map_err(lost)?;
        // No CDB follows the reader's sealed Some(epoch) proof.
        fdb_monitor::index_state(owner, control, &layout.destination.barcode, current, false)
            .await?;
        #[cfg(test)]
        s4_test_hook::resumed(parent, batch, &files);
        Ok(CompletedCheckpoint {
            plan,
            files,
            readbacks,
            prepared_format_source: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::ltfs::recovery::RecoveryBudget;
    use crate::ltfs::volume::{LABEL_BLOCK, LtfsVolume};
    use crate::scsi::cdb;
    use crate::scsi::device::ScsiResult;
    use crate::scsi::reservation::{
        FenceOutcome, ReservationKey, fence, verify_holder_without_ua_retry,
    };
    use crate::scsi::sim::{LogicalObject, SimCartridge, SimLibrary, SimTransport};
    use crate::scsi::transport::TapeTransport;
    use std::cell::RefCell;

    struct ObservationTrace<'a> {
        device: &'a SimTransport,
        locates: RefCell<Vec<[u8; 16]>>,
        fail_label_locate: bool,
    }

    impl TapeTransport for ObservationTrace<'_> {
        fn execute_no_data(
            &self,
            command: &[u8],
            timeout: u32,
        ) -> crate::error::Result<ScsiResult> {
            if command.first() == Some(&cdb::opcode::LOCATE_16) {
                self.locates.borrow_mut().push(command.try_into().unwrap());
                if self.fail_label_locate && command == cdb::locate_16(0, LABEL_BLOCK, true, false)
                {
                    let mut sense = [0; 18];
                    sense[0] = 0x70;
                    sense[2] = 6;
                    sense[7] = 10;
                    sense[12] = 0x28;
                    return crate::scsi::device::finish_command(
                        2,
                        crate::scsi::sense::SenseInfo::from_bytes(&sense),
                        0,
                        0,
                        self.identity(),
                    );
                }
            }
            self.device.execute_no_data(command, timeout)
        }

        fn execute_read(
            &self,
            command: &[u8],
            buf: &mut [u8],
            timeout: u32,
        ) -> crate::error::Result<ScsiResult> {
            self.device.execute_read(command, buf, timeout)
        }

        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> crate::error::Result<ScsiResult> {
            panic!("fresh checkpoint observation must remain read-only")
        }

        fn identity(&self) -> &str {
            self.device.identity()
        }
    }

    fn observe_fresh_reclaim_index(
        replace_label: bool,
        fail_label_locate: bool,
        reuse_preflight: bool,
    ) -> crate::error::Result<CapturedIndexSource> {
        let barcode = "CHECK0L8";
        let library = SimLibrary::new(1, 2, 1);
        library
            .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
            .unwrap();
        library.load_into_drive(barcode, 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                volume_id: "CHECK0".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let key = ReservationKey::new(1, 7);
        assert!(matches!(
            fence(&device, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let budget = RecoveryBudget::default();
        let history = if reuse_preflight {
            let label = crate::ltfs::volume::read_label_only(&device).unwrap();
            crate::ltfs::recovery::capture_reclaim_preparation_index_source(
                &crate::tape::commands::TapeDrive::new(&device),
                &label,
                &budget,
            )
            .unwrap()
            .take_full_history_hint()
        } else {
            None
        };
        let (label, expected) = {
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume
                .append_bytes("/payload", b"fresh checkpoint")
                .unwrap();
            volume.sync().unwrap();
            (volume.label().clone(), volume.committed_index_construct())
        };
        if replace_label {
            let mut replaced = label.clone();
            replaced.creator.push_str(" changed");
            library.with_cartridge_mut(barcode, |cart| {
                cart.partitions[0].objects[LABEL_BLOCK as usize] =
                    LogicalObject::Record(replaced.to_xml().unwrap().to_vec());
            });
        }
        let trace = ObservationTrace {
            device: &device,
            locates: RefCell::new(Vec::new()),
            fail_label_locate,
        };
        let checked = fdb_operation_guard::Guard::new_bracketed_read(&trace, || {
            verify_holder_without_ua_retry(&trace, key)
        });
        let capture = capture_fresh_reclaim_index(&checked, &label, &budget, history);
        let locates = trace.locates.borrow();
        assert_eq!(locates.first(), Some(&cdb::locate_eod_16(1)));
        let ip_eod = locates
            .iter()
            .position(|cdb| *cdb == cdb::locate_eod_16(0))
            .unwrap();
        assert!(locates[ip_eod..].iter().all(|cdb| cdb[3] == 0));
        assert_eq!(
            locates.last(),
            Some(&cdb::locate_16(0, LABEL_BLOCK, true, false))
        );
        if let Ok(capture) = &capture {
            assert_eq!(capture.construct(), expected);
            assert_eq!(capture.history_hint_used(), reuse_preflight);
            checked.verify_proof().unwrap();
        }
        if fail_label_locate {
            assert!(
                checked.verify_proof().is_err(),
                "late UA must close the guard"
            );
        }
        capture
    }

    #[test]
    fn fresh_checkpoint_observes_dp_before_returning_to_ip() {
        observe_fresh_reclaim_index(false, false, false).unwrap();
    }

    #[test]
    fn fresh_checkpoint_reuses_preflight_history_for_actual_full_successor() {
        observe_fresh_reclaim_index(false, false, true).unwrap();
    }

    #[test]
    fn fresh_checkpoint_cannot_seal_after_late_label_mismatch_or_ua() {
        assert!(matches!(
            observe_fresh_reclaim_index(true, false, true),
            Err(TapeError::Ltfs(message)) if message == "回收写入会话与目标实际介质标签不同"
        ));
        assert!(matches!(
            observe_fresh_reclaim_index(false, true, true),
            Err(TapeError::OwnershipLost { .. })
        ));
    }

    #[test]
    fn reservation_observation_never_forgets_an_original_attempt() {
        let mut seen = [false, false];
        observe_reservations(&mut seen, &[false, false]).unwrap();
        observe_reservations(&mut seen, &[true, false]).unwrap();
        assert!(observe_reservations(&mut seen, &[false, false]).is_err());
        assert_eq!(seen, [true, false]);
        observe_reservations(&mut seen, &[true, true]).unwrap();
        assert!(observe_reservations(&mut seen, &[true, false]).is_err());
        assert!(observe_reservations(&mut seen, &[true]).is_err());
    }
}
