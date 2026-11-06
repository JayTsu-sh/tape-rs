//! Read-only inventory joins. Physical observations never establish ownership.
use super::*;

pub(super) async fn read_at(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
) -> Result<Option<serde_json::Value>> {
    Ok(library_observations::observations_at(tr, ns, serial)
        .await?
        .0)
}
pub(super) async fn record_at(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    owner: u64,
    value: &serde_json::Value,
) -> Result<()> {
    library_observations::record_inventory_at(tr, serial, ns, owner, value).await
}

impl ManagedCatalog {
    pub async fn active_tape_inventory_entry(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        barcode: &str,
    ) -> Result<Option<serde_json::Value>> {
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        inventory_entry(&tr, serial, ns, barcode).await
    }

    /// Pool membership comes only from the authoritative assignment index.
    /// Unassigned inventory and cartridges from other pools are excluded.
    pub async fn active_pool_tape_inventory(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        pool: Uuid,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<serde_json::Value>> {
        if !(1..=1000).contains(&limit) {
            return Err(CatalogError::Invalid("磁带分页大小须在 1..=1000"));
        }
        if let Some(after) = after {
            validate_barcode(after)?;
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        require_pool_library(&tr, ns, pool, serial).await?;
        let prefix = pool_assignment_prefix(ns, pool);
        let cursor = after.map(|barcode| namespace_assignment_key(ns, pool, serial, barcode));
        let page = read_page(&tr, &prefix, cursor.as_deref(), limit, false).await?;
        let mut items = Vec::new();
        let mut last = None;
        for (key, value) in page.items {
            let (assigned_pool, library, barcode) = decode_namespace_assignment(ns, &key, &value)?;
            if assigned_pool != pool || library != serial {
                return Err(CatalogError::Corrupt("池磁带索引跨库或跨池"));
            }
            let row = inventory_entry(&tr, serial, ns, &barcode)
                .await?
                .ok_or(CatalogError::Corrupt("池磁带索引缺少归属"))?;
            if row["pool"]["id"] != pool.to_string() {
                return Err(CatalogError::Corrupt("池磁带归属不一致"));
            }
            last = Some(barcode.into_bytes());
            items.push(row);
        }
        Ok(Page {
            items,
            next: page.next.and(last),
        })
    }

    /// Merge one bounded physical-inventory page with authoritative assignments.
    /// Assigned cartridges remain visible when absent from the last observation.
    pub async fn active_tape_inventory(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        observed: &[String],
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<serde_json::Value>> {
        validate_physical_serial(serial)?;
        if !(1..=1000).contains(&limit) || observed.len() > 65536 {
            return Err(CatalogError::Invalid("磁带库存分页或观测数量超限"));
        }
        if let Some(after) = after {
            validate_barcode(after)?;
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let mut prefix = key(b'd');
        append_component(&mut prefix, serial.as_bytes());
        let cursor = after.map(|barcode| {
            let mut key = prefix.clone();
            append_component(&mut key, barcode.as_bytes());
            key
        });
        let page = read_page(&tr, &prefix, cursor.as_deref(), limit + 1, false).await?;
        let mut names = std::collections::BTreeSet::new();
        for (key, _) in &page.items {
            let name = key
                .strip_prefix(prefix.as_slice())
                .and_then(|s| s.strip_suffix(&[0, 0]))
                .and_then(|s| std::str::from_utf8(s).ok())
                .ok_or(CatalogError::Corrupt("磁带库存归属键"))?;
            validate_barcode(name).map_err(|_| CatalogError::Corrupt("磁带库存归属条码"))?;
            names.insert(name.to_owned());
        }
        for name in observed {
            validate_barcode(name)?;
            if after.is_none_or(|after| name.as_str() > after) {
                names.insert(name.clone());
            }
        }
        let more = names.len() > limit || page.next.is_some();
        let mut items = Vec::new();
        let mut last = None;
        for name in names.into_iter().take(limit) {
            let row = inventory_entry(&tr, serial, ns, &name)
                .await?
                .unwrap_or_else(
                    || serde_json::json!({"barcode":name,"assignment":null,"pool":null}),
                );
            last = Some(name.into_bytes());
            items.push(row);
        }
        Ok(Page {
            items,
            next: more.then_some(last).flatten(),
        })
    }
}

pub(super) async fn inventory_entry(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    barcode: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(assignment) = ManagedCatalog::verified_assignment(tr, serial, barcode).await? else {
        return Ok(tape_monitor::read_at(tr, ns, serial, barcode).await?.map(|monitor|
            serde_json::json!({"barcode":barcode,"assignment":null,"pool":null,"monitor":monitor.to_json()})));
    };
    if assignment.namespace != ns {
        return Err(CatalogError::Corrupt("磁带库存与 Active namespace 不符"));
    }
    let raw = tr
        .get(&pool_key(ns, assignment.pool_id), false)
        .await?
        .ok_or(CatalogError::Corrupt("磁带库存池记录缺失"))?;
    let pool = decode_pool(assignment.pool_id, &raw)?;
    active_pools::require_indexes(tr, ns, &pool).await?;
    let task = native_media::inventory_task(tr, serial, ns, assignment.pool_id, barcode).await?;
    let monitor = tape_monitor::read_at(tr, ns, serial, barcode)
        .await?
        .map(|r| r.to_json());
    Ok(Some(
        serde_json::json!({"barcode":barcode,"assignment":tape_policy::view_at(tr, &assignment).await?,"pool":pool.to_json()?,
        "monitor":monitor,"task_id":task.as_ref().and_then(|v|v["task_id"].as_str()),"task_progress":task.as_ref().map(|v|&v["progress"])}),
    ))
}
