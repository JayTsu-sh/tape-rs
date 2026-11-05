//! Read shared authority before opening a new takeover's devices.
//! This is an admission check, never a replacement for PR fencing or owner claim.
use super::super::permission::Phase;
use super::*;

fn local(
    control: &FdbOwnerControl,
    permission: &Permission,
    status: &SharedStatus,
    round: u64,
) -> Result<()> {
    let status = status.lock().unwrap_or_else(|e| e.into_inner());
    if round == 0
        || round > 0xFFFF_FFFF_FFFF
        || status.shutting_down
        || status.catalog_pending
        || status.catalog_frozen_index != Some(control.route.frozen_index)
        || !permission.allows(round, Phase::Fencing)
    {
        return Err(TapeError::NotReady(
            "FDB 接管前置检查：冻结点、停机状态或多数派 Fencing 许可不符".into(),
        ));
    }
    Ok(())
}

pub(super) async fn check(
    control: &FdbOwnerControl,
    permission: &Permission,
    status: &SharedStatus,
    round: u64,
) -> Result<()> {
    local(control, permission, status, round)?;
    let query = control.catalog.active_library_owner(&control.serial);
    tokio::pin!(query);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        local(control, permission, status, round)?;
        if Instant::now() >= deadline {
            return Err(TapeError::NotReady(
                "FDB 接管前置检查：共享事实源核验超时，未打开接管设备".into(),
            ));
        }
        tokio::select! {
            result = &mut query => {
                local(control, permission, status, round)?;
                let actual = result.map_err(|error| TapeError::Catalog(format!(
                    "FDB 接管前置检查：共享事实源读取失败：{error}"
                )))?;
                match actual {
                    Some((route, owner)) if route == control.route
                        && owner >= route.initial_owner_epoch
                        && owner <= round && owner > 0 => return Ok(()),
                    _ => return Err(TapeError::Catalog(
                        "FDB 接管前置检查：活动路由不符、owner epoch 无效或已有更新轮次".into(),
                    )),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}
