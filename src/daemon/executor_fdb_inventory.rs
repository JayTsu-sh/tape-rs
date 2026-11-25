use super::*;

pub(super) async fn refresh(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
) -> std::result::Result<(), ReadError> {
    fdb_loading::guard(owner, permit, control).await?;
    let observation = management::observe(owner, opts)
        .map_err(|e| ReadError::Lost(format!("带库库存采集失败: {e}")))?;
    fdb_loading::guard(owner, permit, control).await?;
    control
        .catalog
        .record_active_library_inventory(&control.serial, control.route, owner.round, &observation)
        .await
        .map_err(|error| match error.domain() {
            crate::managed_catalog::CatalogError::Incomplete => {
                ReadError::Busy("库存缺少完整的逻辑带库对账证据".into())
            }
            _ => ReadError::Lost(format!("带库库存发布失败: {error}")),
        })?;
    fdb_loading::guard(owner, permit, control).await
}
