//! FDB 只读 owner 的机械调度。只记本轮已确认的挂载；失权时丢弃状态，不做补偿移动。
use super::*;
use crate::managed_catalog::ActiveReadSource;

pub(super) struct Mount {
    pub barcode: String,
    pub volume: uuid::Uuid,
    pub pool: uuid::Uuid,
    pub last_used: Instant,
}

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("FDB 只读装卸未能确认: {error}"))
}

async fn guard(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
) -> std::result::Result<(), ReadError> {
    if !(permit.allows(owner.round, super::super::permission::Phase::Serving)
        || permit.allows(owner.round, super::super::permission::Phase::Execution))
    {
        return Err(lost("多数派许可已失效"));
    }
    if control
        .catalog
        .active_library_owner(&control.serial)
        .await
        .map_err(lost)?
        != Some((control.route, owner.round))
    {
        return Err(lost("FDB route/owner epoch 已改变"));
    }
    for device in &owner.devices {
        reservation::verify_holder(device.dev.as_ref(), owner.key).map_err(lost)?;
    }
    Ok(())
}

async fn binding(
    owner: &Active,
    control: &FdbOwnerControl,
    mount: &Mount,
) -> std::result::Result<(), ReadError> {
    let head = control
        .catalog
        .active_loaded_volume_head(&control.serial, &mount.barcode, control.route, owner.round)
        .await
        .map_err(lost)?
        .ok_or_else(|| lost("磁带已无当前带库归属"))?;
    if head.volume_id != mount.volume || head.pool_id != mount.pool {
        return Err(lost("卷或池身份已变化"));
    }
    Ok(())
}

fn policy(status: &SharedStatus) -> Arc<serde_json::Value> {
    super::super::management::policy_view(&status.lock().unwrap_or_else(|e| e.into_inner()))
        .management
}

pub(super) async fn unload(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    drive: usize,
) -> std::result::Result<(), ReadError> {
    let mount = owner
        .fdb_reads
        .get(&drive)
        .ok_or_else(|| lost("没有本轮挂载身份"))?;
    let management = policy(status);
    if !super::super::management::tape_allowed(&management, &mount.barcode)
        || !super::super::management::drive_allowed(
            &management,
            &owner.devices[drive].serial,
            u64::from(owner.key.node()),
            &mount.pool.to_string(),
            'r',
        )
    {
        return Err(ReadError::Busy("只读卸带被磁带或驱动策略禁止".into()));
    }
    guard(owner, permit, control).await?;
    binding(owner, control, mount).await?;
    let inventory = inventory_inner(&owner.devices, true).map_err(lost)?;
    let slot = inventory
        .drives
        .iter()
        .find(|slot| slot.dev == drive)
        .ok_or_else(|| lost("挂载驱动器不在库存中"))?;
    let (barcode, home) = slot
        .loaded
        .as_ref()
        .filter(|(barcode, _)| barcode == &mount.barcode)
        .ok_or_else(|| lost("卸带前库存与本轮挂载身份不一致"))?;
    if inventory.cleaning.contains(barcode) {
        return Err(lost("禁止卸载清洗带"));
    }
    let destination = home
        .filter(|home| inventory.empty_slots.contains(home))
        .or_else(|| inventory.empty_slots.first().copied())
        .ok_or_else(|| ReadError::Busy("没有空存储槽位可卸带".into()))?;
    let label =
        crate::ltfs::volume::read_label_only(owner.devices[drive].dev.as_ref()).map_err(lost)?;
    if label.volume_uuid != mount.volume {
        return Err(lost("卸带前介质 UUID 不匹配"));
    }
    guard(owner, permit, control).await?;
    binding(owner, control, mount).await?;
    TapeDrive::new(owner.devices[drive].dev.as_ref())
        .unload()
        .map_err(lost)?;
    // UNLOAD 与 MOVE 分开核对；UNLOAD 成功后失权时绝不继续移动。
    guard(owner, permit, control).await?;
    binding(owner, control, mount).await?;
    let before_move = inventory_inner(&owner.devices, true).map_err(lost)?;
    if !before_move.empty_slots.contains(&destination)
        || !before_move.drives.iter().any(|current| {
            current.dev == drive
                && current.addr == slot.addr
                && current
                    .loaded
                    .as_ref()
                    .is_some_and(|(loaded, _)| loaded == barcode)
        })
    {
        return Err(lost("退带后库存改变，停止 MOVE"));
    }
    info!("FDB 只读卸带: {} -> 槽位 {destination:#06x}", barcode);
    move_medium(&owner.devices, slot.addr, destination).map_err(lost)?;
    guard(owner, permit, control).await?;
    binding(owner, control, mount).await?;
    let after_move = inventory_inner(&owner.devices, true).map_err(lost)?;
    if after_move.slots.get(barcode) != Some(&destination)
        || !after_move
            .drives
            .iter()
            .any(|current| current.dev == drive && current.loaded.is_none())
    {
        return Err(lost("卸带后的库存未确认移动结果"));
    }
    owner.fdb_reads.remove(&drive);
    Ok(())
}

pub(super) async fn select(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    path: &str,
    source: &ActiveReadSource,
) -> std::result::Result<usize, ReadError> {
    let placement = source.placement();
    let barcode = &placement.barcode;
    let pool = placement.pool_id.to_string();
    let management = policy(status);
    if !super::super::management::tape_allowed(&management, barcode) {
        return Err(ReadError::Failed("磁带为清洗带、离线或在黑名单中".into()));
    }
    guard(owner, permit, control).await?;
    let inventory = inventory_inner(&owner.devices, true).map_err(lost)?;
    if inventory.cleaning.contains(barcode) {
        return Err(ReadError::Failed("清洗带不能执行 LTFS 读取".into()));
    }
    let allowed = |drive: usize| {
        super::super::management::drive_allowed(
            &management,
            &owner.devices[drive].serial,
            u64::from(owner.key.node()),
            &pool,
            'r',
        )
    };
    if let Some(slot) = inventory.drives.iter().find(|slot| {
        slot.loaded
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == barcode)
    }) {
        if !allowed(slot.dev) {
            return Err(ReadError::Busy("目标驱动器没有读取资格".into()));
        }
        // 恢复阶段已验证并登记已有受管介质；没有挂载身份时不可自动把它当作可淘汰读带。
        if let Some(mount) = owner.fdb_reads.get_mut(&slot.dev) {
            if mount.barcode != *barcode
                || mount.volume != source.source.volume_uuid
                || mount.pool != placement.pool_id
            {
                return Err(lost("已装载介质与本轮挂载记录不一致"));
            }
            mount.last_used = Instant::now();
        }
        return Ok(slot.dev);
    }
    let from = *inventory
        .slots
        .get(barcode)
        .ok_or_else(|| ReadError::Busy("目标磁带不在可调度存储槽位中".into()))?;
    let mut compatible = Vec::new();
    for slot in inventory.drives.iter().filter(|slot| allowed(slot.dev)) {
        let raw =
            crate::scsi::transport::retry_unit_attention("FDB 读取密度能力查询", || {
                TapeDrive::new(owner.devices[slot.dev].dev.as_ref()).report_density_support(false)
            });
        match raw {
            Ok(raw)
                if super::super::scheduling::densities(&raw).is_some_and(|densities| {
                    super::super::scheduling::compatible(&densities, barcode, false)
                }) =>
            {
                compatible.push(slot.dev)
            }
            Err(error) if ownership_lost(&error) => return Err(lost(error)),
            Err(error) => warn!("FDB 只读调度跳过未知密度能力驱动器: {error}"),
            _ => {}
        }
    }
    let limit = management["pools"][&pool]["mountlimit"]
        .as_str()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    // Physical occupancy consumes quota even when policy forbids reading or
    // retiring that mount. The eviction registry is not an occupancy census.
    let mut occupied = 0;
    if limit != 0 {
        for slot in &inventory.drives {
            if let Some((loaded, _)) = &slot.loaded
                && let Some(assignment) = control
                    .catalog
                    .tape_assignment(&control.serial, loaded)
                    .await
                    .map_err(lost)?
            {
                if assignment.namespace != control.route.route.namespace {
                    return Err(lost("已装载磁带属于另一个命名空间"));
                }
                occupied += usize::from(assignment.pool_id == placement.pool_id);
            }
        }
    }
    let at_limit = limit != 0 && occupied >= limit;
    let empty = inventory
        .drives
        .iter()
        .find(|slot| slot.loaded.is_none() && compatible.contains(&slot.dev))
        .map(|slot| slot.dev);
    let drive = if let Some(empty) = empty.filter(|_| !at_limit) {
        empty
    } else {
        let victim = owner
            .fdb_reads
            .iter()
            .filter(|(drive, mount)| {
                // An empty compatible drive can receive the target after we
                // retire a same-pool mount on a different-generation drive.
                (empty.is_some() && at_limit || compatible.contains(drive))
                    && (!at_limit || mount.pool == placement.pool_id)
                    && super::super::management::tape_allowed(&management, &mount.barcode)
                    && super::super::management::drive_allowed(
                        &management,
                        &owner.devices[**drive].serial,
                        u64::from(owner.key.node()),
                        &mount.pool.to_string(),
                        'r',
                    )
            })
            .min_by_key(|(_, mount)| mount.last_used)
            .map(|(drive, _)| *drive)
            .ok_or_else(|| ReadError::Busy("没有兼容空驱动器或可卸下的本轮受管读带".into()))?;
        unload(owner, permit, control, status, victim).await?;
        empty.unwrap_or(victim)
    };
    guard(owner, permit, control).await?;
    if control
        .catalog
        .resolve_active_read_source_for_owner(
            &control.serial,
            path.as_bytes(),
            control.route,
            owner.round,
        )
        .await
        .map_err(lost)?
        .as_ref()
        != Some(source)
    {
        return Err(ReadError::Busy("文件副本在装载前改变，请重试".into()));
    }
    let before = inventory_inner(&owner.devices, true).map_err(lost)?;
    let destination = before
        .drives
        .iter()
        .find(|slot| slot.dev == drive && slot.loaded.is_none())
        .ok_or_else(|| lost("装载前目标驱动器已非空"))?
        .addr;
    if before.slots.get(barcode) != Some(&from) || before.cleaning.contains(barcode) {
        return Err(lost("装载前目标条码或槽位改变"));
    }
    info!("FDB 只读装载: {barcode} -> {}", owner.devices[drive].serial);
    move_medium(&owner.devices, from, destination).map_err(lost)?;
    guard(owner, permit, control).await?;
    let after = inventory_inner(&owner.devices, true).map_err(lost)?;
    if !after.empty_slots.contains(&from)
        || !after.drives.iter().any(|slot| {
            slot.dev == drive
                && slot.addr == destination
                && slot
                    .loaded
                    .as_ref()
                    .is_some_and(|(loaded, _)| loaded == barcode)
        })
    {
        return Err(lost("装载后的库存未确认移动结果"));
    }
    TapeDrive::new(owner.devices[drive].dev.as_ref())
        .load()
        .map_err(lost)?;
    wait_ready(owner.devices[drive].dev.as_ref()).map_err(lost)?;
    guard(owner, permit, control).await?;
    let label =
        crate::ltfs::volume::read_label_only(owner.devices[drive].dev.as_ref()).map_err(lost)?;
    if label.volume_uuid != source.source.volume_uuid {
        return Err(lost("装载后磁带 UUID 不匹配"));
    }
    let mount = Mount {
        barcode: barcode.clone(),
        volume: label.volume_uuid,
        pool: placement.pool_id,
        last_used: Instant::now(),
    };
    binding(owner, control, &mount).await?;
    owner.fdb_reads.insert(drive, mount);
    Ok(drive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changer::element::ElementStatus;

    fn devices() -> Vec<ManagedDevice> {
        vec![ManagedDevice {
            name: "drive".into(),
            serial: "D001".into(),
            kind: DeviceKind::Drive,
            dev: Box::new(ClosedTransport),
        }]
    }
    fn drive() -> ElementStatus {
        ElementStatus {
            address: 20,
            element_type: ElementType::DataTransfer,
            full: false,
            medium_type: None,
            volume_tag: None,
            source_address: None,
            drive_id: Some("D001".into()),
        }
    }
    fn cartridge() -> ElementStatus {
        ElementStatus {
            address: 1000,
            element_type: ElementType::Storage,
            full: true,
            medium_type: Some(1),
            volume_tag: Some("TEST01L8".into()),
            source_address: None,
            drive_id: None,
        }
    }
    #[test]
    fn strict_inventory_distinguishes_empty_from_unknown_and_rejects_duplicates() {
        let devices = devices();
        let good = inventory_elements(&devices, &[drive(), cartridge()], true).unwrap();
        assert_eq!(good.slots["TEST01L8"], 1000);
        assert!(good.drives[0].loaded.is_none());
        let mut unknown = drive();
        unknown.full = true;
        assert!(inventory_elements(&devices, &[unknown, cartridge()], true).is_err());
        let mut duplicate = drive();
        duplicate.full = true;
        duplicate.volume_tag = Some("TEST01L8".into());
        assert!(inventory_elements(&devices, &[duplicate, cartridge()], true).is_err());
        let mut duplicate_address = cartridge();
        duplicate_address.address = 20;
        assert!(inventory_elements(&devices, &[drive(), duplicate_address], true).is_err());
    }
    #[test]
    fn strict_inventory_requires_unique_configured_drive_mapping_and_tracks_cleaning() {
        let mut devices = devices();
        let mut cleaning = cartridge();
        cleaning.medium_type = Some(2);
        assert!(
            inventory_elements(&devices, &[drive(), cleaning], true)
                .unwrap()
                .cleaning
                .contains("TEST01L8")
        );
        let mut missing = drive();
        missing.drive_id = None;
        assert!(inventory_elements(&devices, &[missing], true).is_err());
        let mut second = drive();
        second.address = 21;
        assert!(inventory_elements(&devices, &[drive(), second], true).is_err());
        devices.push(ManagedDevice {
            name: "other".into(),
            serial: "D001-extra".into(),
            kind: DeviceKind::Drive,
            dev: Box::new(ClosedTransport),
        });
        assert!(inventory_elements(&devices, &[drive()], true).is_err());
    }
}
