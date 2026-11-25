use super::*;
use crate::daemon::executor::OfflineLogicalLibraryInventory;

fn receipt_key(ns: Uuid, serial: &str, operation: Uuid) -> Vec<u8> {
    by_id(at(ns, serial, b'E'), operation)
}

async fn receipt_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    operation: Uuid,
) -> Result<Option<Value>> {
    let Some(raw) = tr.get(&receipt_key(ns, serial, operation), false).await? else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("逻辑带库启用回执损坏"))?;
    if value["library_serial"] != serial
        || value["namespace"] != json!(ns)
        || value["operation"] != json!(operation)
        || value["status"] != "enabled"
    {
        return Err(CatalogError::Corrupt("逻辑带库启用回执的身份不符"));
    }
    Ok(Some(value))
}

fn check_original(value: Value, release: Uuid, expected_plan: Option<&str>) -> Result<Value> {
    if value["release"] != json!(release)
        || expected_plan.is_some_and(|plan| value["plan_sha256"] != plan)
    {
        return Err(CatalogError::Invalid(
            "启用 UUID 已用于不同的交还或迁移计划",
        ));
    }
    Ok(value)
}

impl ManagedCatalog {
    pub(crate) async fn find_original_logical_activation(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        operation: Uuid,
        release: Uuid,
        expected_plan: Option<&str>,
    ) -> Result<Option<Value>> {
        self.logical_library_activation_receipt(serial, route, operation)
            .await?
            .map(|value| check_original(value, release, expected_plan))
            .transpose()
    }

    pub(crate) async fn logical_library_activation_receipt(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        operation: Uuid,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        receipt_at(&tr, ns, serial, operation).await
    }

    pub(crate) async fn publish_logical_library_activation(
        &self,
        route: ActiveLibraryRoute,
        proof: &OfflineLogicalLibraryInventory,
        operation: Uuid,
        expected_plan: Option<&str>,
    ) -> Result<Value> {
        let release = &proof.release;
        let serial = release.library_serial.as_str();
        let ns = release.namespace;
        let mut observation = library_observations::canonical_inventory(
            serial,
            release.owner_epoch,
            &proof.observation,
        )?;
        if observation["slot_layout"].is_null() {
            return Err(CatalogError::Incomplete);
        }
        let mut logical = CreateLogicalLibrary {
            version: 1,
            id: operation,
            name: "default".into(),
            resources: observed_resources(&observation),
        };
        logical.normalize().map_err(CatalogError::Invalid)?;
        let encoded = serde_json::to_vec(&logical)
            .map_err(|_| CatalogError::Invalid("默认逻辑带库无法编码"))?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(CatalogError::Invalid("默认逻辑带库的资源清单超过大小限制"));
        }
        // 预览和提交都读设备，忽略两次采集的时钟和节点号。
        for transient in ["observed_at", "owner"] {
            observation
                .as_object_mut()
                .expect("validated observation")
                .remove(transient);
        }
        self.db.run(|tr, _| {
            let (logical, encoded, observation) = (&logical, &encoded, &observation);
            async move {
                requests::retry_result(
                    async {
                        if routing::require_expected_active_namespace(&tr, serial, Some(route)).await? != ns {
                            return Err(CatalogError::Invalid("启用请求命名空间已改变"));
                        }
                        if let Some(saved) = receipt_at(&tr, ns, serial, operation).await? {
                            return check_original(saved, release.operation, expected_plan);
                        }
                        let active = routing::active_key(serial);
                        let raw = tr.get(&active, false).await?.ok_or(CatalogError::Incomplete)?;
                        if raw[0] != routing::HANDOFF_ROUTE_VERSION {
                            return Err(CatalogError::Invalid("首次启用逻辑带库须采用受保护的交还路由"));
                        }
                        if release.phase != LibraryHandoffPhase::Released
                            || management::handoff::read_at(&tr, ns, serial, None).await?.as_ref() != Some(release)
                        {
                            return Err(CatalogError::Invalid("首次启用逻辑带库须引用当前已完成的原交还"));
                        }
                        management::handoff::require_drained_at(&tr, ns, serial).await?;
                        for kind in [b'l', b'n', b'r', b'p', b'm', b'c', b'E'] {
                            let prefix = at(ns, serial, kind);
                            if !read_page(&tr, &prefix, None, 1, false).await?.items.is_empty() {
                                return Err(CatalogError::Invalid("物理库已有逻辑带库配置，不能执行首次迁移"));
                            }
                        }

                        let pool_prefix = pool_prefix(ns);
                        let pool_rows = read_page(&tr, &pool_prefix, None, MAX_RESOURCES, false).await?;
                        if pool_rows.next.is_some() {
                            return Err(CatalogError::Invalid("物理带库的池数量超过一次迁移容量"));
                        }
                        let mut pools = Vec::new();
                        for (key, raw) in pool_rows.items {
                            let pool = decode_pool(uuid_from(&key[pool_prefix.len()..])?, &raw)?;
                            if pool.library_serial != serial { continue; }
                            require_pool_library(&tr, ns, pool.id, serial).await?;
                            active_pools::require_indexes(&tr, ns, &pool).await?;
                            pools.push(pool);
                        }
                        for medium in observation["volumes"].as_array().expect("validated volumes") {
                            let barcode = medium["barcode"].as_str().expect("validated barcode");
                            let Some(assignment) = ManagedCatalog::verified_assignment(&tr, serial, barcode).await? else { continue; };
                            if assignment.namespace != ns || !pools.iter().any(|pool| pool.id == assignment.pool_id) {
                                return Err(CatalogError::Corrupt("迁移库存的介质归属不在原物理带库池清单中"));
                            }
                            let planned = ManagedCatalog::native_device_home_at(&tr, ns, serial, barcode, Some(&assignment)).await?;
                            let observed_home = (medium["element_type"] == 4).then(|| medium["source_address"].as_u64()).flatten();
                            if planned.is_some_and(|home| !logical.resources.storage_slots.contains(&home))
                                || observed_home.is_some_and(|home| !logical.resources.storage_slots.contains(&(home as u16)))
                                || planned.zip(observed_home).is_some_and(|(home, observed)| u64::from(home) != observed)
                            {
                                return Err(CatalogError::Invalid("原介质的 home 与现场库存不一致，须先结清"));
                            }
                        }
                        let pool_list: Vec<_> = pools.iter().map(|p| json!({"id":p.id,"name":p.name,"revision":p.revision})).collect();
                        let mut report = json!({"version":1,"library_serial":serial,"namespace":ns,"operation":operation,
                            "status":"preview","release":release.operation,"logical_library":definition(logical, 1),
                            "pools":pool_list,"pool_count":pools.len()});
                        let plan = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!({"report":report,"inventory":observation}))
                            .expect("已校验预览记录可编码")));
                        report["plan_sha256"] = json!(plan);
                        let Some(expected_plan) = expected_plan else { return Ok(report) };
                        if expected_plan != plan {
                            return Err(CatalogError::Invalid("库存或待迁移池已改变，请核对原操作 UUID 后重新预览"));
                        }

                        for key in resource_keys(&at(ns, serial, b'r'), &logical.resources) {
                            tr.set(&key, logical.id.as_bytes());
                        }
                        tr.set(&by_name(at(ns, serial, b'n'), &logical.name), logical.id.as_bytes());
                        for kind in [b'l', b'c'] {
                            tr.set(&by_id(at(ns, serial, kind), logical.id), encoded);
                        }
                        tr.set(&by_id(at(ns, serial, b'v'), logical.id), &1u64.to_be_bytes());
                        for pool in &pools {
                            tr.set(&pool_binding_key(ns, serial, pool.id), logical.id.as_bytes());
                            tr.set(&pool_membership_key(ns, serial, logical.id, pool.id), b"");
                        }
                        let mut upgraded = raw.to_vec();
                        upgraded[0] = routing::LOGICAL_ROUTE_VERSION;
                        tr.set(&active, &upgraded);
                        report["status"] = json!("enabled");
                        report.as_object_mut().expect("回执为 JSON 对象").remove("pools");
                        tr.set(&receipt_key(ns, serial, operation), report.to_string().as_bytes());
                        Ok(report)
                    }
                    .await,
                )
            }
        }).await?
    }
}
