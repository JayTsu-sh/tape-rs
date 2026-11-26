use super::*;
use crate::managed_catalog::{CatalogError, LibraryHandoff, LibraryHandoffPhase as Phase};
use crate::scsi::{cdb, transport::retry_unit_attention};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

fn scope(devices: &[ManagedDevice], serial: &str) -> Result<Vec<String>> {
    let mut ids: Vec<_> = devices.iter().map(|d| d.serial.clone()).collect();
    ids.sort_unstable();
    if ids.len() < 2
        || ids.windows(2).any(|p| p[0] == p[1])
        || devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Changer)
            .count()
            != 1
        || !devices
            .iter()
            .any(|d| d.kind == DeviceKind::Changer && d.serial == serial)
    {
        return Err(TapeError::NotReady("带库机械手或驱动器身份不完整".into()));
    }
    Ok(ids)
}

fn ready(device: &ManagedDevice) -> Result<()> {
    retry_unit_attention("带库交还健康检查", || {
        device.dev.execute_no_data(&cdb::test_unit_ready(), 10_000)
    })
    .map(|_| ())
}

pub(super) fn health(devices: &[ManagedDevice]) -> Result<()> {
    let changer = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .ok_or_else(|| TapeError::NotReady("带库机械手缺失".into()))?;
    ready(changer)?;
    let mut mc = MediumChanger::new(changer.dev.as_ref());
    mc.load_address_map()?;
    let elements = mc.read_all_status()?;
    if let Some(item) = elements.iter().find(|e| {
        e.disabled
            || (e.exception
                && !(matches!(
                    e.element_type,
                    ElementType::Storage | ElementType::ImportExport
                ) && e.full
                    && e.asc == 0x30
                    && e.ascq == 0x03
                    && e.is_cleaning()))
    }) {
        return Err(TapeError::NotReady(format!(
            "元素 {:#06x} 不健康，ASC/ASCQ={:02x}/{:02x}",
            item.address, item.asc, item.ascq
        )));
    }
    let inv = inventory_elements(devices, &elements, true)?;
    if inv.drives.len() + 1 != devices.len() {
        return Err(TapeError::NotReady("库存未覆盖全部配置驱动器".into()));
    }
    for slot in inv.drives {
        let device = &devices[slot.dev];
        match (slot.loaded.is_some(), ready(device)) {
            (true, Ok(()))
            | (
                false,
                Err(TapeError::ScsiCommand {
                    sense_key: 2,
                    asc: 0x3a,
                    ascq: 0,
                    ..
                }),
            ) => {}
            (false, Ok(())) => {
                return Err(TapeError::NotReady(format!(
                    "{} 驱动器报告有带而库存为空",
                    device.serial
                )));
            }
            (_, Err(error)) if ownership_lost(&error) => return Err(error),
            (_, Err(error)) => {
                return Err(TapeError::NotReady(format!(
                    "{} 健康检查失败: {error}",
                    device.serial
                )));
            }
        }
    }
    Ok(())
}

fn catalog(error: CatalogError) -> ReadError {
    match error.domain() {
        CatalogError::PoolBusy => ReadError::Busy("带库存在未结清的管理、回收或机械任务".into()),
        CatalogError::LibraryHandoffBusy(ids) => {
            ReadError::Busy(format!("带库存在未结清操作：{ids:?}"))
        }
        CatalogError::Fdb(_)
        | CatalogError::Client(_)
        | CatalogError::OwnerFenced
        | CatalogError::Incomplete
        | CatalogError::Corrupt(_) => ReadError::Lost(format!(
            "交还准入或执行权无法确认，请用原操作 ID 查询：{error}"
        )),
        error => ReadError::Failed(error.to_string()),
    }
}

pub(super) async fn prepare(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    operation: uuid::Uuid,
) -> std::result::Result<LibraryHandoff, ReadError> {
    if !permit.allows(owner.round, crate::daemon::permission::Phase::Serving) {
        return Err(ReadError::Busy("带库多数派服务许可已失效".into()));
    }
    let devices =
        scope(&owner.devices, &control.serial).map_err(|e| ReadError::Failed(e.to_string()))?;
    let checked = (|| -> Result<()> {
        health(&owner.devices)?;
        for device in &owner.devices {
            reservation::verify_holder(device.dev.as_ref(), owner.key)?;
        }
        Ok(())
    })();
    let diagnosis = match checked {
        Err(error) if ownership_lost(&error) => return Err(ReadError::Lost(error.to_string())),
        Err(error) => Some(error.to_string()),
        Ok(()) => None,
    };
    let original = LibraryHandoff {
        library_serial: control.serial.clone(),
        namespace: control.route.route.namespace,
        operation,
        release: None,
        phase: if diagnosis.is_some() {
            Phase::Failed
        } else {
            Phase::Releasing
        },
        release_key: owner.key.0,
        release_retry_used: false,
        attempt_key: None,
        owner_epoch: owner.round,
        devices,
        detail: diagnosis,
    };
    control
        .catalog
        .begin_library_handoff(&original, control.route)
        .await
        .map_err(catalog)
}

fn released(devices: &[ManagedDevice], original: &LibraryHandoff) -> Result<()> {
    if scope(devices, &original.library_serial)? != original.devices {
        return Err(TapeError::NotReady("交还设备身份或作用范围变化".into()));
    }
    for device in devices {
        let observed = reservation::read_handoff_status(device.dev.as_ref())?;
        if observed
            .keys
            .iter()
            .any(|&key| ReservationKey(key).is_ours())
            || observed
                .holder
                .is_some_and(|(key, _)| ReservationKey(key).is_ours())
        {
            return Err(TapeError::NotReady(format!(
                "{} 仍持有原交还轮次预留或注册",
                device.serial
            )));
        }
    }
    Ok(())
}

/// The caller must keep all device executors stopped and all previously
/// submitted commands drained throughout this read-only observation.
pub async fn verify_library_release_offline(
    catalog: &crate::managed_catalog::ManagedCatalog,
    serial: &str,
    namespace: uuid::Uuid,
    operation: uuid::Uuid,
    provider: &dyn DeviceProvider,
) -> Result<LibraryHandoff> {
    let catalog_error = |error: CatalogError| TapeError::Catalog(error.to_string());
    let route = catalog
        .active_library_route(serial)
        .await
        .map_err(catalog_error)?
        .filter(|route| route.route.namespace == namespace)
        .ok_or_else(|| TapeError::NotReady("离线核验的带库 namespace 不符".into()))?;
    let current = catalog
        .library_handoff(serial, route, None)
        .await
        .map_err(catalog_error)?
        .filter(|row| {
            row.operation == operation && matches!(row.phase, Phase::Releasing | Phase::Released)
        })
        .ok_or_else(|| TapeError::NotReady("须指定当前交还的原操作 ID".into()))?;
    if current.phase == Phase::Released {
        return Ok(current);
    }
    let devices = provider.open_all()?;
    released(&devices, &current)?;
    drop(devices);
    let mut verified = current.clone();
    verified.phase = Phase::Released;
    verified.detail = None;
    catalog
        .update_library_handoff(route, &current, &verified)
        .await
        .map_err(catalog_error)?;
    Ok(verified)
}

pub(crate) struct OfflineLogicalLibraryInventory {
    pub(crate) release: LibraryHandoff,
    pub(crate) observation: serde_json::Value,
}

/// 在原设备组已交还且所有执行器离线时，预览或提交一次性迁移。
pub async fn activate_logical_library_offline(
    catalog: &crate::managed_catalog::ManagedCatalog,
    serial: &str,
    namespace: uuid::Uuid,
    operation: uuid::Uuid,
    release: uuid::Uuid,
    expected_plan: Option<&str>,
    provider: &dyn DeviceProvider,
) -> Result<serde_json::Value> {
    match activate_logical_library_offline_checked(
        catalog,
        serial,
        namespace,
        operation,
        release,
        expected_plan,
        provider,
    )
    .await
    {
        Err(error)
            if expected_plan.is_none()
                && !operation.is_nil()
                && !release.is_nil()
                && operation != release =>
        {
            catalog
                .preview_blocked_logical_activation(
                    serial,
                    namespace,
                    operation,
                    release,
                    &error.to_string(),
                )
                .await
                .map_err(|e| TapeError::Catalog(e.to_string()))
        }
        result => result,
    }
}

async fn activate_logical_library_offline_checked(
    catalog: &crate::managed_catalog::ManagedCatalog,
    serial: &str,
    namespace: uuid::Uuid,
    operation: uuid::Uuid,
    release: uuid::Uuid,
    expected_plan: Option<&str>,
    provider: &dyn DeviceProvider,
) -> Result<serde_json::Value> {
    if operation.is_nil()
        || release.is_nil()
        || operation == release
        || expected_plan.is_some_and(|sha| {
            sha.len() != 64
                || !sha
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
    {
        return Err(TapeError::NotReady(
            "原启用、交还 UUID 或计划摘要无效".into(),
        ));
    }
    let catalog_error = |error: CatalogError| TapeError::Catalog(error.to_string());
    let route = catalog
        .active_library_route(serial)
        .await
        .map_err(catalog_error)?
        .filter(|route| route.route.namespace == namespace)
        .ok_or_else(|| TapeError::NotReady("启用逻辑带库的原 namespace 不符".into()))?;
    if let Some(receipt) = catalog
        .find_original_logical_activation(serial, route, operation, release, expected_plan)
        .await
        .map_err(catalog_error)?
    {
        return Ok(receipt);
    }
    let original = catalog
        .library_handoff(serial, route, None)
        .await
        .map_err(catalog_error)?
        .filter(|row| row.operation == release && row.phase == Phase::Released)
        .ok_or_else(|| TapeError::NotReady("须先确认原整库交还已完成".into()))?;
    let devices = provider.open_all()?;
    if scope(&devices, serial)? != original.devices {
        return Err(TapeError::NotReady("当前设备组与原交还设备组不符".into()));
    }
    let ensure_unreserved = || -> Result<()> {
        for device in &devices {
            let status = reservation::read_handoff_status(device.dev.as_ref())?;
            if !status.keys.is_empty() || status.holder.is_some() {
                return Err(TapeError::NotReady(format!(
                    "{} 仍有持有者或注册，暂不能启用逻辑带库",
                    device.serial
                )));
            }
        }
        Ok(())
    };
    ensure_unreserved()?;
    let node = ReservationKey(original.release_key).node();
    let observation = management::observe_devices(&devices, node, original.owner_epoch)?;
    ensure_unreserved()?;
    let proof = OfflineLogicalLibraryInventory {
        release: original,
        observation,
    };
    catalog
        .publish_logical_library_activation(route, &proof, operation, expected_plan)
        .await
        .map_err(catalog_error)
}

pub(super) fn release(owner: &Active, original: &LibraryHandoff) -> Result<()> {
    if owner.key.0 != original.release_key || owner.round != original.owner_epoch {
        return Err(TapeError::NotReady("交还执行器与原意图不符".into()));
    }
    resume_release(&owner.devices, original)
}

pub(super) fn resume_release(devices: &[ManagedDevice], original: &LibraryHandoff) -> Result<()> {
    if scope(devices, &original.library_serial)? != original.devices {
        return Err(TapeError::NotReady("交还设备身份或作用范围变化".into()));
    }
    clear_reload(devices, ReservationKey(original.release_key))?;
    released(devices, original)
}

// Only created by the actor after the last device command has returned and
// the actor has dropped the device handles. Retrying this value is FDB-only.
pub(super) struct ReleaseReceipt {
    original: LibraryHandoff,
    updated: LibraryHandoff,
    next_try: Instant,
    attempts: u32,
}

// This local checkpoint attests only to completed device work. FDB still
// decides whether the exact original transition can be committed.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CompletedRelease {
    version: u8,
    route_certificate: [u8; 32],
    original: LibraryHandoff,
}

const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;

impl ReleaseReceipt {
    pub(super) fn checkpoint_path(
        files: &FileService,
        control: &FdbOwnerControl,
        node: u8,
    ) -> PathBuf {
        let namespace = control.route.route.namespace;
        files
            .spool_dir()
            .with_file_name(format!(".fdb-release-{namespace}-{node}.json"))
    }

    pub(super) fn restore(
        path: &Path,
        control: &FdbOwnerControl,
        node: u8,
    ) -> io::Result<Option<Self>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("交还检查点不是普通文件"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CHECKPOINT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(io::Error::other("交还检查点超过大小限制"));
        }
        let checkpoint: CompletedRelease = serde_json::from_slice(&bytes)?;
        let original = checkpoint.original;
        if checkpoint.version != 1
            || checkpoint.route_certificate != control.route.certificate_sha256
            || original.namespace != control.route.route.namespace
            || original.library_serial != control.serial
            || original.phase != Phase::Releasing
            || original.release.is_some()
            || original.attempt_key.is_some()
            || original.release_retry_used
            || original.detail.is_some()
            || ReservationKey(original.release_key).node() != node
        {
            return Err(io::Error::other("交还检查点与原库、节点或操作不符"));
        }
        Ok(Some(Self::new(original, Ok(()))))
    }

    pub(super) fn persist(&self, path: &Path, control: &FdbOwnerControl) -> io::Result<()> {
        if self.updated.phase != Phase::Released {
            return Ok(());
        }
        let checkpoint = CompletedRelease {
            version: 1,
            route_certificate: control.route.certificate_sha256,
            original: self.original.clone(),
        };
        let bytes = serde_json::to_vec(&checkpoint)?;
        if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(io::Error::other("交还检查点超过大小限制"));
        }
        let temp = path.with_extension("new");
        // The daemon holds the data directory lock across restart cleanup.
        remove_checkpoint(&temp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
        File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
        Ok(())
    }

    pub(super) fn new(original: LibraryHandoff, result: Result<()>) -> Self {
        let mut updated = original.clone();
        match result {
            Ok(()) => {
                updated.phase = Phase::Released;
                updated.detail = None;
            }
            Err(error) => updated.detail = Some(format!("交还结果未知: {error}")),
        }
        Self {
            original,
            updated,
            next_try: Instant::now(),
            attempts: 0,
        }
    }

    pub(super) async fn poll(&mut self, control: &FdbOwnerControl, path: &Path) -> bool {
        if Instant::now() < self.next_try {
            return false;
        }
        match control
            .catalog
            .update_library_handoff(control.route, &self.original, &self.updated)
            .await
        {
            Ok(()) => {
                if self.updated.phase == Phase::Released
                    && let Err(error) = remove_checkpoint(path)
                {
                    warn!(
                        "带库 {} 交还 {} 检查点清理失败: {error}",
                        control.serial, self.original.operation
                    );
                }
                true
            }
            Err(error) => {
                self.attempts += 1;
                let stop = self.attempts >= 6
                    || !matches!(
                        error.domain(),
                        CatalogError::Fdb(_) | CatalogError::Client(_)
                    );
                let delay = Duration::from_secs(1 << self.attempts.min(5));
                self.next_try = Instant::now() + delay;
                warn!(
                    "带库 {} 交还 {} 回执保存失败: {error}",
                    control.serial, self.original.operation
                );
                stop
            }
        }
    }
}

fn remove_checkpoint(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) async fn record_reload_failure(
    control: &FdbOwnerControl,
    original: &LibraryHandoff,
    error: &TapeError,
    cleared: bool,
) {
    let mut update = original.clone();
    if cleared {
        update.attempt_key = None;
    }
    update.phase = Phase::Blocked;
    update.detail = Some(error.to_string());
    if let Err(error) = control
        .catalog
        .update_library_handoff(control.route, original, &update)
        .await
    {
        warn!(
            "带库 {} 加载 {} 回执保存失败: {error}",
            control.serial, original.operation
        );
    }
}

pub(super) fn preflight_reload(devices: &[ManagedDevice], request: &LibraryHandoff) -> Result<()> {
    if scope(devices, &request.library_serial)? != request.devices {
        return Err(TapeError::NotReady("重新加载设备身份与交还记录不符".into()));
    }
    if request.attempt_key.is_some() {
        return Err(TapeError::NotReady("原加载的设备命令结果未结清".into()));
    }
    for device in devices {
        let status = reservation::read_handoff_status(device.dev.as_ref())?;
        if !status.keys.is_empty() || status.holder.is_some() {
            return Err(TapeError::NotReady(format!(
                "{} 仍有预留或注册",
                device.serial
            )));
        }
    }
    Ok(())
}

// These errors confirm that the acquisition command was rejected, or that
// the device answers arrived and failed the readback predicate. A transport
// or permission failure cannot provide the same completion proof.
pub(super) fn reload_can_clear(error: &TapeError) -> bool {
    matches!(
        error,
        TapeError::NotReady(_) | TapeError::Busy { .. } | TapeError::ReservationConflict { .. }
    )
}

pub(super) fn isolate_reload(devices: &[ManagedDevice], key: ReservationKey) -> Result<()> {
    for device in devices {
        reservation::reserve_for_reload(device.dev.as_ref(), key)?;
    }
    Ok(())
}

pub(super) fn clear_reload(devices: &[ManagedDevice], key: ReservationKey) -> Result<()> {
    let mut failure = None;
    for device in devices {
        if let Err(error) = reservation::clear_reload_registration(device.dev.as_ref(), key) {
            warn!(
                "{} 加载轮次 {} 注销结果未知: {error}",
                device.serial,
                key.round()
            );
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
#[path = "executor_library_handoff_tests.rs"]
mod tests;
