//! Prepare metadata changes before the first mutation. Task result, notification,
//! and pool indexes commit together; device work belongs to another actor.
use super::*;

enum Mutation {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}
struct PreparedChange {
    result: Value,
    mutations: Vec<Mutation>,
}
fn pool_id(change: &Value) -> Uuid {
    Uuid::parse_str(change["id"].as_str().expect("validated pool UUID"))
        .expect("validated pool UUID")
}
fn next_revision(change: &Value) -> Result<u64> {
    change["revision"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .ok_or(CatalogError::Invalid("池修订号已耗尽"))
}
fn patch(pool: &mut PoolRecord, fields: &Value) {
    for (key, value) in fields.as_object().expect("validated patch") {
        match key.as_str() {
            "name" => pool.name = value.as_str().expect("validated name").into(),
            "file_limit" => pool.file_limit = value.as_u64().expect("validated limit"),
            "media_restriction" => {
                pool.policy.media_restriction =
                    value.as_str().expect("validated media restriction").into()
            }
            "mount_limit" => {
                pool.policy.mount_limit = value.as_u64().expect("validated mount limit") as u16
            }
            "low_space_warning_enabled" => {
                pool.policy.low_space_warning_enabled = value.as_bool().expect("validated flag")
            }
            "low_space_warning_threshold" => {
                pool.policy.low_space_warning_threshold =
                    value.as_u64().expect("validated threshold") as u16
            }
            "no_space_warning_enabled" => {
                pool.policy.no_space_warning_enabled = value.as_bool().expect("validated flag")
            }
            _ => unreachable!("validated pool patch"),
        }
    }
}
async fn prepare(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    change: &Value,
) -> Result<PreparedChange> {
    let id = pool_id(change);
    if change["kind"] == "pool-delete" {
        native_device::require_library_available(tr, ns, serial).await?;
        require_pool_writable_at(tr, ns, id).await?;
    } else {
        native_device::require_pool_metadata_available(tr, ns, serial, id).await?;
    }
    let saved = tr.get(&pool_key(ns, id), false).await?;
    if retired_pool_revision_at(tr, ns, id).await?.is_some() {
        return Err(if saved.is_some() {
            CatalogError::Corrupt("退役池仍有主记录")
        } else {
            CatalogError::Invalid("池已退役")
        });
    }
    let mut pool = decode_pool(id, &saved.ok_or(CatalogError::Invalid("池不存在"))?)?;
    if pool.library_serial != serial {
        return Err(CatalogError::Invalid("池不属于目标带库"));
    }
    require_pool_library(tr, ns, id, serial).await?;
    active_pools::require_indexes(tr, ns, &pool).await?;
    if change["revision"] != pool.revision {
        return Err(CatalogError::Invalid("池修订号已改变"));
    }
    let revision = next_revision(change)?;
    if change["kind"] == "pool-delete" {
        let root = active_pools::root_id(ns, id);
        if requests::prefix_has_rows(tr, &child_prefix(ns, root)).await? {
            return Err(CatalogError::Invalid("池目录非空，不能删除"));
        }
        for prefix in [
            reservation::path_prefix_for(ns, root),
            active_batch::path_claim_prefix_for(ns, root),
        ] {
            if requests::prefix_has_rows(tr, &prefix).await? {
                return Err(CatalogError::Invalid("池仍有活动写入目标，不能删除"));
            }
        }
        let assignments = read_page(tr, &pool_assignment_prefix(ns, id), None, 1, false).await?;
        if let Some((key, raw)) = assignments.items.first() {
            let (indexed_pool, library, barcode) = decode_namespace_assignment(ns, key, raw)?;
            let a = ManagedCatalog::verified_assignment(tr, &library, &barcode)
                .await?
                .ok_or(CatalogError::Corrupt("池归属索引缺少磁带主记录"))?;
            if indexed_pool != id
                || a.pool_id != id
                || a.namespace != ns
                || a.library_serial != serial
            {
                return Err(CatalogError::Corrupt("池归属索引与磁带主记录不符"));
            }
            return Err(CatalogError::Invalid("池仍有磁带归属，不能删除"));
        }
        let retirement = encode_pool_retirement(revision)?.to_vec();
        pool.revision = revision;
        return Ok(PreparedChange {
            result: json!({"pool":pool.to_json()?,"deleted":true}),
            mutations: vec![
                Mutation::Delete(pool_key(ns, id)),
                Mutation::Delete(pool_name_key(ns, &pool.name)),
                Mutation::Delete(child_key(ns, Uuid::nil(), id.to_string().as_bytes())),
                Mutation::Delete(node_key(ns, root)),
                Mutation::Put(retired_pool_key(ns, id), retirement),
            ],
        });
    }
    let old_name = pool.name.clone();
    patch(&mut pool, &change["patch"]);
    pool.revision = revision;
    validate_pool(&pool)?;
    let name = pool_name_key(ns, &pool.name);
    if old_name != pool.name
        && let Some(raw) = tr.get(&name, false).await?
    {
        let occupied = uuid_from(&raw)?;
        if occupied.is_nil() || occupied == id {
            return Err(CatalogError::Corrupt("目标名称存在孤立池引用"));
        }
        let raw = tr
            .get(&pool_key(ns, occupied), false)
            .await?
            .ok_or(CatalogError::Corrupt("目标名称引用的池不存在"))?;
        let other = decode_pool(occupied, &raw)?;
        if other.name != pool.name {
            return Err(CatalogError::Corrupt("目标名称与池主记录不符"));
        }
        require_pool_library(tr, ns, occupied, &other.library_serial).await?;
        active_pools::require_indexes(tr, ns, &other).await?;
        return Err(CatalogError::Invalid("池名称已被使用"));
    }
    let mut mutations = vec![Mutation::Put(pool_key(ns, id), encode_pool(&pool))];
    if old_name != pool.name {
        mutations.push(Mutation::Delete(pool_name_key(ns, &old_name)));
        mutations.push(Mutation::Put(name, id.as_bytes().to_vec()));
    }
    Ok(PreparedChange {
        result: json!({"pool":pool.to_json()?}),
        mutations,
    })
}

impl ManagedCatalog {
    /// Execute only queued pool policy/delete intents. Other kinds remain queued
    /// for their dedicated actor; no hardware action is implied by this method.
    pub async fn apply_pool_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Value> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = read_task_at(&tr, ns, serial, epoch, operation)
                            .await?
                            .ok_or(CatalogError::Invalid("管理请求不存在"))?;
                        let request = ManagementRequest::parse(task["request"].clone())?;
                        let change = &request.value()["change"];
                        if !matches!(change["kind"].as_str(), Some("pool" | "pool-delete")) {
                            return Err(CatalogError::Invalid(
                                "此入口仅执行池元数据管理，其他任务须由专用执行器处理",
                            ));
                        }
                        if task["status"] != "queued" {
                            return Ok(task);
                        }
                        let prepared = match prepare(&tr, ns, serial, change).await {
                            Ok(change) => {
                                task["status"] = json!("succeeded");
                                task["result"] = change.result.clone();
                                Some(change)
                            }
                            Err(CatalogError::Invalid(reason)) => {
                                task["status"] = json!("failed");
                                task["result"] = json!({"error":reason});
                                None
                            }
                            Err(error) => return Err(error),
                        };
                        task["owner_epoch"] = json!(owner);
                        task["completed_at_millis"] = json!(
                            now().max(task["created_at_millis"].as_u64().expect("validated time"))
                        );
                        // write_task wraps notification failures in an aborting domain error.
                        // Every pool validation is complete; only infallible mutations follow.
                        write_task(&tr, &request, &task).await?;
                        if let Some(prepared) = prepared {
                            for mutation in prepared.mutations {
                                match mutation {
                                    Mutation::Put(key, value) => tr.set(&key, &value),
                                    Mutation::Delete(key) => tr.clear(&key),
                                }
                            }
                        }
                        tr.clear(&identity_key(ns, serial, b'q', epoch, operation));
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
#[path = "management_pools_tests.rs"]
mod tests;
