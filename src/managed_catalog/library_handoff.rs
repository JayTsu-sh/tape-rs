use super::*;
use crate::scsi::reservation::ReservationKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LibraryHandoffPhase {
    Releasing,
    Released,
    Loading,
    Blocked,
    Online,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryHandoff {
    pub library_serial: String,
    pub namespace: Uuid,
    pub operation: Uuid,
    pub release: Option<Uuid>,
    pub phase: LibraryHandoffPhase,
    pub release_key: u64,
    pub release_retry_used: bool,
    pub attempt_key: Option<u64>,
    pub owner_epoch: u64,
    pub devices: Vec<String>,
    pub detail: Option<String>,
}

fn state_key(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = library_prefix(ns, serial, b'E');
    key.push(b'h');
    key
}

fn receipt_key(ns: Uuid, serial: &str, operation: Uuid) -> Vec<u8> {
    let mut key = state_key(ns, serial);
    key.extend_from_slice(operation.as_bytes());
    key
}

fn volume_key(ns: Uuid, serial: &str, operation: Uuid, volume: Uuid) -> Vec<u8> {
    let mut key = receipt_key(ns, serial, operation);
    key.extend_from_slice(volume.as_bytes());
    key
}

fn fence_prefix(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = library_prefix(ns, serial, b'E');
    key.push(b'f');
    key
}

fn fence_key(ns: Uuid, serial: &str, epoch: u64) -> Vec<u8> {
    let mut key = fence_prefix(ns, serial);
    key.extend_from_slice(&epoch.to_be_bytes());
    key
}

pub(in crate::managed_catalog) async fn read_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    operation: Option<Uuid>,
) -> Result<Option<LibraryHandoff>> {
    let key = operation.map_or_else(|| state_key(ns, serial), |id| receipt_key(ns, serial, id));
    let Some(raw) = tr.get(&key, false).await? else {
        return Ok(None);
    };
    let value: LibraryHandoff =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("带库交还记录损坏"))?;
    let release_key = ReservationKey(value.release_key);
    if value.library_serial != serial
        || value.namespace != ns
        || value.operation.is_nil()
        || operation.is_some_and(|id| id != value.operation)
        || value
            .release
            .is_some_and(|id| id.is_nil() || id == value.operation)
        || value.owner_epoch == 0
        || !release_key.is_ours()
        || release_key.node() == 0
        || release_key.round() == 0
        || (value.release.is_none() && release_key.round() != value.owner_epoch)
        || value.devices.is_empty()
        || value.attempt_key.is_some_and(|key| {
            let key = ReservationKey(key);
            !matches!(
                value.phase,
                LibraryHandoffPhase::Loading
                    | LibraryHandoffPhase::Blocked
                    | LibraryHandoffPhase::Online
            ) || !key.is_ours()
                || key.node() == 0
                || key.round() != value.owner_epoch
                || key.round() <= release_key.round()
        })
        || (value.phase == LibraryHandoffPhase::Online && value.attempt_key.is_none())
        || !value.devices.windows(2).all(|pair| pair[0] < pair[1])
        || (matches!(
            value.phase,
            LibraryHandoffPhase::Loading
                | LibraryHandoffPhase::Blocked
                | LibraryHandoffPhase::Online
        ) != value.release.is_some())
        || (operation.is_none() && value.phase == LibraryHandoffPhase::Failed)
    {
        return Err(CatalogError::Corrupt("带库交还记录与原始身份不符"));
    }
    if operation.is_none()
        && tr
            .get(&receipt_key(ns, serial, value.operation), false)
            .await?
            .as_deref()
            != Some(raw.as_ref())
    {
        return Err(CatalogError::Corrupt("当前带库交还状态与原回执不一致"));
    }
    Ok(Some(value))
}

fn save(tr: &Transaction, value: &LibraryHandoff) {
    let raw = serde_json::to_vec(value).expect("带库交还记录可编码");
    tr.set(
        &receipt_key(value.namespace, &value.library_serial, value.operation),
        &raw,
    );
    if value.phase != LibraryHandoffPhase::Failed {
        tr.set(&state_key(value.namespace, &value.library_serial), &raw);
    }
}

async fn require_protected_route(tr: &Transaction, serial: &str) -> Result<()> {
    let route = tr.get(&routing::active_key(serial), false).await?;
    if !route
        .as_deref()
        .and_then(|raw| raw.first().copied())
        .is_some_and(routing::supports_handoff)
    {
        return Err(CatalogError::Invalid(
            "带库缺少离线启用的交还路由保护，须先确认旧执行器和设备命令已排空",
        ));
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn require_drained_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<()> {
    let mut uploads = key(b'0');
    uploads.extend_from_slice(ns.as_bytes());
    let mut blocking = selected_pool::pending_ids_at(tr, ns, serial, 16).await?;
    for prefix in [
        library_prefix(ns, serial, b'q'),
        control::pending_node_task_prefix(ns, serial),
        uploads,
    ] {
        for (key, _) in read_page(tr, &prefix, None, 16, false).await?.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("带库交还阻塞操作索引损坏"));
            }
            blocking.push(uuid_from(&key[prefix.len() + 8..])?);
        }
    }
    if !blocking.is_empty() {
        return Err(CatalogError::LibraryHandoffBusy(blocking));
    }
    exclusion::require_available_at(tr, ns, serial).await?;
    moves::require_readable_at(tr, ns, serial).await?;
    if !read_page(tr, &fence_prefix(ns, serial), None, 1, false)
        .await?
        .items
        .is_empty()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn allows_owner(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    owner: u64,
) -> Result<bool> {
    Ok(match read_at(tr, ns, serial, None).await? {
        None => true,
        Some(value) => match value.phase {
            LibraryHandoffPhase::Online => true,
            LibraryHandoffPhase::Loading => {
                require_protected_route(tr, serial).await?;
                value.attempt_key.is_some() && owner == value.owner_epoch
            }
            _ => false,
        },
    })
}

impl ManagedCatalog {
    /// A protected route serializes an intent to issue ordinary PR commands
    /// against release admission. Each round keeps its own unresolved intent.
    pub(crate) async fn begin_handoff_fence(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        attempt: Uuid,
    ) -> Result<bool> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        let raw = tr
                            .get(&routing::active_key(serial), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if !routing::supports_handoff(raw[0]) {
                            return Ok(false);
                        }
                        if read_at(&tr, ns, serial, None)
                            .await?
                            .is_some_and(|r| r.phase != LibraryHandoffPhase::Online)
                        {
                            return Err(CatalogError::OwnerFenced);
                        }
                        let owner = tr
                            .get(&owner_key(serial), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if owner.len() != 8 || read_u64(&owner) > epoch {
                            return Err(CatalogError::OwnerFenced);
                        }
                        let key = fence_key(ns, serial, epoch);
                        if let Some(saved) = tr.get(&key, false).await? {
                            return if saved.as_ref() == attempt.as_bytes() {
                                Ok(true)
                            } else {
                                Err(CatalogError::OwnerFenced)
                            };
                        }
                        if read_page(&tr, &fence_prefix(ns, serial), None, 16, false)
                            .await?
                            .items
                            .len()
                            >= 16
                        {
                            return Err(CatalogError::PoolBusy);
                        }
                        tr.set(&key, attempt.as_bytes());
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }

    pub(crate) async fn finish_handoff_fence(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        attempt: Uuid,
        fenced: bool,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        let key = fence_key(ns, serial, epoch);
                        if let Some(saved) = tr.get(&key, false).await? {
                            if saved.as_ref() != attempt.as_bytes() {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if fenced {
                                // Publish the confirmed device epoch before removing
                                // the interlock. An old owner must not admit a release
                                // between these two writes.
                                let owner = owner_key(serial);
                                let observed = tr
                                    .get(&owner, false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                if observed.len() != 8 {
                                    return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
                                }
                                if read_u64(&observed) < epoch {
                                    tr.set(&owner, &epoch.to_be_bytes());
                                }
                            }
                            tr.clear(&key);
                        }
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }

    /// Call only after all executors with device access have stopped and
    /// outstanding device commands have completed. This changes the route
    /// encoding permanently, without changing its identity or the owner epoch.
    pub async fn enable_library_handoff_offline(
        &self,
        serial: &str,
        namespace: Uuid,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        if routing::require_expected_active_namespace(&tr, serial, None).await?
                            != namespace
                        {
                            return Err(CatalogError::Invalid("离线启用的带库 namespace 不符"));
                        }
                        let key = routing::active_key(serial);
                        let saved = tr.get(&key, false).await?.ok_or(CatalogError::Incomplete)?;
                        if read_at(&tr, namespace, serial, None)
                            .await?
                            .is_some_and(|r| r.phase != LibraryHandoffPhase::Online)
                        {
                            return Err(CatalogError::Invalid("旧版交还尚未结清，不能启用新协议"));
                        }
                        // Offline maintenance has excluded all outstanding PR
                        // commands, so only here may an unresolved fence be removed.
                        let prefix = fence_prefix(namespace, serial);
                        tr.clear_range(&prefix, &prefix_end(&prefix));
                        if routing::supports_handoff(saved[0]) {
                            return Ok(());
                        }
                        let mut upgraded = saved.to_vec();
                        upgraded[0] = routing::HANDOFF_ROUTE_VERSION;
                        tr.set(&key, &upgraded);
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }

    /// First write/recovery after external use requires the exact preexisting
    /// FDB index and a complete on-media tail. Later normal recovery may
    /// settle work originally issued by this installation.
    pub(crate) async fn verify_reloaded_volume(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        index: ActiveIndexIdentity,
        complete: bool,
    ) -> Result<bool> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let Some(current) = read_at(&tr, ns, serial, None).await? else {
                            return Ok(true);
                        };
                        if current.phase != LibraryHandoffPhase::Online {
                            return Err(CatalogError::OwnerFenced);
                        }
                        let key = volume_key(ns, serial, current.operation, index.volume_uuid);
                        if let Some(marker) = tr.get(&key, false).await? {
                            if marker.as_ref() != [1] {
                                return Err(CatalogError::Corrupt("重新加载卷校验记录损坏"));
                            }
                            return Ok(true);
                        }
                        if !complete
                            || active_volume::observed_at(&tr, serial, index.volume_uuid)
                                .await?
                                .1
                                != index
                        {
                            return Ok(false);
                        }
                        tr.set(&key, &[1]);
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn library_handoff(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        operation: Option<Uuid>,
    ) -> Result<Option<LibraryHandoff>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let receipt = read_at(&tr, ns, serial, operation).await?;
        if operation.is_none()
            && receipt
                .as_ref()
                .is_some_and(|row| row.phase != LibraryHandoffPhase::Online)
        {
            require_protected_route(&tr, serial).await?;
        }
        Ok(receipt)
    }

    pub(crate) async fn begin_library_handoff(
        &self,
        original: &LibraryHandoff,
        route: ActiveLibraryRoute,
    ) -> Result<LibraryHandoff> {
        let serial = original.library_serial.as_str();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        if let Some(saved) =
                            read_at(&tr, ns, serial, Some(original.operation)).await?
                        {
                            if saved.release.is_none() {
                                return Ok(saved);
                            }
                            return Err(CatalogError::Invalid("操作 ID 已用于加载"));
                        }
                        routing::require_runtime_active_owner(
                            &tr,
                            serial,
                            route,
                            original.owner_epoch,
                        )
                        .await?;
                        if read_at(&tr, ns, serial, None)
                            .await?
                            .is_some_and(|r| r.phase != LibraryHandoffPhase::Online)
                        {
                            return Err(CatalogError::PoolBusy);
                        }
                        require_drained_at(&tr, ns, serial).await?;
                        require_protected_route(&tr, serial).await?;
                        save(&tr, original);
                        Ok(original.clone())
                    }
                    .await,
                )
            })
            .await?
    }

    pub(crate) async fn begin_library_reload(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        operation: Uuid,
        release: Uuid,
    ) -> Result<LibraryHandoff> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        require_protected_route(&tr, serial).await?;
                        if let Some(mut saved) = read_at(&tr, ns, serial, Some(operation)).await? {
                            if saved.release != Some(release) {
                                return Err(CatalogError::Invalid("操作 ID 已用于其他交还或加载"));
                            }
                            if saved.phase == LibraryHandoffPhase::Blocked
                                && saved.attempt_key.is_none()
                                && read_at(&tr, ns, serial, None).await?.as_ref() == Some(&saved)
                            {
                                saved.phase = LibraryHandoffPhase::Loading;
                                save(&tr, &saved);
                            }
                            return Ok(saved);
                        }
                        let mut value = read_at(&tr, ns, serial, None)
                            .await?
                            .filter(|r| {
                                r.phase == LibraryHandoffPhase::Released && r.operation == release
                            })
                            .ok_or(CatalogError::Invalid("必须引用当前已完成的交还回执"))?;
                        value.operation = operation;
                        value.release = Some(release);
                        value.phase = LibraryHandoffPhase::Loading;
                        value.detail = None;
                        save(&tr, &value);
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }

    pub(crate) async fn update_library_handoff(
        &self,
        route: ActiveLibraryRoute,
        original: &LibraryHandoff,
        updated: &LibraryHandoff,
    ) -> Result<()> {
        if original.operation != updated.operation
            || original.release != updated.release
            || original.library_serial != updated.library_serial
            || original.namespace != updated.namespace
            || original.release_key != updated.release_key
            || original.devices != updated.devices
            || !matches!(
                (original.phase, updated.phase),
                (
                    LibraryHandoffPhase::Releasing,
                    LibraryHandoffPhase::Releasing | LibraryHandoffPhase::Released
                ) | (
                    LibraryHandoffPhase::Loading,
                    LibraryHandoffPhase::Loading
                        | LibraryHandoffPhase::Blocked
                        | LibraryHandoffPhase::Online
                )
            )
        {
            return Err(CatalogError::Invalid("带库交还状态转换与原操作不符"));
        }
        let serial = original.library_serial.as_str();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        require_protected_route(&tr, serial).await?;
                        let current = read_at(&tr, ns, serial, None).await?;
                        if current.as_ref() == Some(updated) {
                            return Ok(());
                        }
                        // The original release receipt remains authoritative
                        // even if reload advanced the current-state pointer.
                        if updated.phase == LibraryHandoffPhase::Released
                            && read_at(&tr, ns, serial, Some(updated.operation))
                                .await?
                                .as_ref()
                                == Some(updated)
                        {
                            return Ok(());
                        }
                        if current.as_ref() != Some(original) {
                            return Err(CatalogError::OwnerFenced);
                        }
                        if updated.phase == LibraryHandoffPhase::Online {
                            logical_libraries::require_acquire_reconciliation(&tr, updated).await?;
                        }
                        save(&tr, updated);
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }
}
