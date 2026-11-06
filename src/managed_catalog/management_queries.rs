//! Bounded management views of current FDB authority and historical observations.
//! No device operations or automatic repair belong in these readers.
use super::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

async fn snapshot(tr: &Transaction, serial: &str, route: ActiveLibraryRoute) -> Result<Value> {
    let ns = routing::require_expected_active_namespace(tr, serial, Some(route)).await?;
    let owner = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Corrupt("活动 owner epoch 缺失"))?;
    if owner.len() != 8 {
        return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
    }
    let owner = read_u64(&owner);
    if owner < route.initial_owner_epoch || owner == 0 || owner > 0xFFFF_FFFF_FFFF {
        return Err(CatalogError::Corrupt("活动 owner epoch 范围"));
    }
    let (inventory, monitor) = library_observations::observations_at(tr, ns, serial).await?;
    Ok(
        json!({"version":1,"library_serial":serial,"namespace":ns.to_string(),"owner_epoch":owner,"inventory":inventory,"monitor":monitor}),
    )
}
fn volumes(view: &Value) -> Result<BTreeMap<String, Value>> {
    if view["inventory"].is_null() {
        return Ok(BTreeMap::new());
    }
    view["inventory"]["volumes"]
        .as_array()
        .ok_or(CatalogError::Corrupt("完整库存缺少介质数组"))?
        .iter()
        .map(|v| {
            let barcode = v["barcode"]
                .as_str()
                .ok_or(CatalogError::Corrupt("库存条码缺失"))?;
            Ok((barcode.to_owned(), v.clone()))
        })
        .collect()
}
async fn tape(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    view: &Value,
    barcode: &str,
    location: Option<&Value>,
) -> Result<Option<Value>> {
    // Inputs were validated by this reader. A stored cross-namespace record or
    // an Invalid result from its referenced pool is corruption, not bad input.
    if let Some(raw) = tr.get(&assignment_key(serial, barcode), false).await? {
        let record = decode_assignment(serial, barcode, &raw)?;
        if record.namespace != route.route.namespace {
            return Err(CatalogError::Corrupt("磁带归属不在当前带库命名空间"));
        }
    }
    let assignment = ManagedCatalog::verified_assignment(tr, serial, barcode)
        .await
        .map_err(|error| match error {
            CatalogError::Invalid(detail) => CatalogError::Corrupt(detail),
            other => other,
        })?;
    if assignment.is_none() && location.is_none() {
        return Ok(None);
    }
    let pool = if let Some(a) = &assignment {
        let raw = tr
            .get(&pool_key(a.namespace, a.pool_id), false)
            .await?
            .ok_or(CatalogError::Corrupt("磁带归属的池缺失"))?;
        let pool = decode_pool(a.pool_id, &raw)?;
        active_pools::require_indexes(tr, a.namespace, &pool).await?;
        Some(pool.to_json()?)
    } else {
        None
    };
    let observation=(!view["inventory"].is_null()).then(||json!({
        "node":view["inventory"]["owner"],"round":view["inventory"]["round"],"observed_at":view["inventory"]["observed_at"],
        "owner_current":view["inventory"]["round"]==view["owner_epoch"],
    }));
    let details =
        match inventory::inventory_entry(tr, serial, route.route.namespace, barcode).await? {
            Some(details) => details,
            // A physical, unassigned cartridge has no policy or monitor until an
            // explicit operation samples it. This is an unknown observation,
            // whereas a missing assigned record is still corruption.
            None if assignment.is_none() && location.is_some() => json!({
                "monitor":null,"task_id":null,"task_progress":null,
            }),
            None => return Err(CatalogError::Corrupt("当前磁带库存详情缺失")),
        };
    Ok(Some(
        json!({"monitor":details["monitor"],"task_id":details["task_id"],"task_progress":details["task_progress"],"barcode":barcode,"library_serial":serial,"namespace":route.route.namespace.to_string(),
        "presence":if view["inventory"].is_null(){"unknown"}else if location.is_some(){"observed"}else{"not_observed"},
        "location":location,"observation":observation,"assignment":assignment.map(|a|a.to_json()),"pool":pool,
        "health":null,"capacity":null}),
    ))
}
fn assignment_prefix(serial: &str) -> Vec<u8> {
    let mut prefix = key(b'd');
    append_component(&mut prefix, serial.as_bytes());
    prefix
}
fn pool_library_prefix(ns: Uuid, pool: Uuid, serial: &str) -> Vec<u8> {
    let mut prefix = pool_assignment_prefix(ns, pool);
    append_component(&mut prefix, serial.as_bytes());
    prefix
}
impl ManagedCatalog {
    /// One read version for the owner and both separately published observations.
    /// Different observation timestamps remain visible; they do not imply corruption.
    pub async fn active_library_management_view(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<Value> {
        validate_physical_serial(serial)?;
        snapshot(&self.db.create_trx()?, serial, route).await
    }
    /// Barcode order; pages read current facts, not a retained historical snapshot.
    pub async fn active_tape_page_for_route(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        pool: Option<Uuid>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        validate_physical_serial(serial)?;
        if !(1..=100).contains(&limit) || pool.is_some_and(|p| p.is_nil()) {
            return Err(CatalogError::Invalid("磁带分页大小或池UUID无效"));
        }
        if let Some(after) = after {
            validate_barcode(after)?;
        }
        let tr = self.db.create_trx()?;
        let view = snapshot(&tr, serial, route).await?;
        let physical = volumes(&view)?;
        let prefix = if let Some(pool) = pool {
            require_pool_library(&tr, route.route.namespace, pool, serial).await?;
            pool_library_prefix(route.route.namespace, pool, serial)
        } else {
            assignment_prefix(serial)
        };
        let cursor = after.map(|after| {
            let mut key = prefix.clone();
            append_component(&mut key, after.as_bytes());
            key
        });
        let page = read_page(&tr, &prefix, cursor.as_deref(), limit, false).await?;
        let mut candidates = BTreeSet::new();
        for (row, raw) in page.items {
            let barcode = String::from_utf8(decode_name(&row[prefix.len()..])?)
                .map_err(|_| CatalogError::Corrupt("归属键条码编码"))?;
            validate_barcode(&barcode).map_err(|_| CatalogError::Corrupt("归属键条码"))?;
            if let Some(pool) = pool {
                let (actual_pool, actual_serial, actual_barcode) =
                    decode_namespace_assignment(route.route.namespace, &row, &raw)?;
                if actual_pool != pool || actual_serial != serial || actual_barcode != barcode {
                    return Err(CatalogError::Corrupt("池磁带索引作用域"));
                }
            } else {
                let assigned = decode_assignment(serial, &barcode, &raw)?;
                if assigned.namespace != route.route.namespace {
                    return Err(CatalogError::Corrupt("磁带主归属不在活动命名空间"));
                }
            }
            candidates.insert(barcode);
        }
        if pool.is_none() {
            candidates.extend(
                physical
                    .keys()
                    .filter(|b| after.is_none_or(|a| b.as_str() > a))
                    .cloned(),
            );
        }
        let more = page.next.is_some() || candidates.len() > limit;
        let mut items = Vec::new();
        for barcode in candidates.into_iter().take(limit) {
            let value = tape(&tr, serial, route, &view, &barcode, physical.get(&barcode))
                .await?
                .ok_or(CatalogError::Corrupt("磁带索引缺少归属或库存记录"))?;
            if pool.is_some_and(|p| value["assignment"]["pool"] != p.to_string()) {
                return Err(CatalogError::Corrupt("池磁带索引与主归属不符"));
            }
            items.push(value);
        }
        let next = if more {
            items.last().map(|v| v["barcode"].clone())
        } else {
            None
        };
        Ok(
            json!({"version":1,"library_serial":serial,"namespace":route.route.namespace.to_string(),"owner_epoch":view["owner_epoch"],"inventory_available":!view["inventory"].is_null(),"pool":pool.map(|p|p.to_string()),"items":items,"next":next}),
        )
    }
    pub async fn active_tape_for_route(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        barcode: &str,
    ) -> Result<Option<Value>> {
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        let view = snapshot(&tr, serial, route).await?;
        let physical = volumes(&view)?;
        tape(&tr, serial, route, &view, barcode, physical.get(barcode)).await
    }
}
