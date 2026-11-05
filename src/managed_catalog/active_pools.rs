//! Active pool metadata and its virtual root are published in one transaction.
//! No device work is involved; media assignment is a separate operation.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivePoolCreation {
    Created(PoolRecord),
    AlreadyExists(PoolRecord),
    IdConflict,
    NameConflict,
    Retired,
}

pub(super) fn root_id(namespace: Uuid, pool: Uuid) -> Uuid {
    import_materialize::import_inode(namespace, pool, "/")
}

pub(super) async fn require_no_file_claims(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
) -> Result<()> {
    let root = root_id(namespace, pool);
    for key in [
        head_key(namespace, root),
        active_batch::file_claim_key_for(namespace, root),
        reservation::file_key_for(namespace, root),
        reservation::path_key_for(namespace, Uuid::nil(), pool.to_string().as_bytes()),
        active_batch::path_claim_key_for(namespace, Uuid::nil(), pool.to_string().as_bytes()),
    ] {
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::Corrupt("池虚拟根与文件或活动目标冲突"));
        }
    }
    Ok(())
}

pub(super) async fn require_indexes(
    tr: &Transaction,
    namespace: Uuid,
    pool: &PoolRecord,
) -> Result<()> {
    if tr
        .get(&pool_name_key(namespace, &pool.name), false)
        .await?
        .as_deref()
        != Some(pool.id.as_bytes().as_slice())
    {
        return Err(CatalogError::Corrupt("池名称索引缺失或不一致"));
    }
    let root = root_id(namespace, pool.id);
    let name = pool.id.to_string().into_bytes();
    if tr
        .get(&child_key(namespace, Uuid::nil(), &name), false)
        .await?
        .as_deref()
        != Some(root.as_bytes().as_slice())
    {
        return Err(CatalogError::Corrupt("池虚拟根目录项缺失或不一致"));
    }
    let raw = tr
        .get(&node_key(namespace, root), false)
        .await?
        .ok_or(CatalogError::Corrupt("池虚拟根 inode 缺失"))?;
    let node = decode_node(&raw)?;
    if node.kind != NodeKind::Directory || !node.parent.is_nil() || node.name != name {
        return Err(CatalogError::Corrupt("池虚拟根 inode 不一致"));
    }
    pool_roots::require_metadata(tr, namespace, &node).await?;
    require_no_file_claims(tr, namespace, pool.id).await
}

async fn checked_pool(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    saved: PoolRecord,
) -> Result<Option<PoolRecord>> {
    if saved.library_serial != serial {
        return Ok(None);
    }
    require_pool_library(tr, namespace, saved.id, serial).await?;
    require_indexes(tr, namespace, &saved).await?;
    Ok(Some(saved))
}

impl ManagedCatalog {
    pub async fn active_pool_for_route(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: Uuid,
    ) -> Result<Option<PoolRecord>> {
        validate_physical_serial(serial)?;
        if id.is_nil() {
            return Err(CatalogError::Invalid("pool ID 不能为 nil"));
        }
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let Some(raw) = tr.get(&pool_key(namespace, id), false).await? else {
            return Ok(None);
        };
        checked_pool(&tr, namespace, serial, decode_pool(id, &raw)?).await
    }

    /// Exact name-index lookup; no first-page scan or inferred alias.
    pub async fn active_pool_named_for_route(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        name: &str,
    ) -> Result<Option<PoolRecord>> {
        validate_physical_serial(serial)?;
        validate_pool_name(name)?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let Some(raw) = tr.get(&pool_name_key(namespace, name), false).await? else {
            return Ok(None);
        };
        let id = uuid_from(&raw)?;
        let raw = tr
            .get(&pool_key(namespace, id), false)
            .await?
            .ok_or(CatalogError::Corrupt("池名称索引缺少主记录"))?;
        let saved = decode_pool(id, &raw)?;
        if saved.name != name {
            return Err(CatalogError::Corrupt("池名称索引与主记录不符"));
        }
        checked_pool(&tr, namespace, serial, saved).await
    }

    /// UUID and namespace are supplied by the caller before sending the request.
    /// Repeats never replace metadata, timestamps, roots or existing contents.
    pub async fn create_active_pool(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        pool: &PoolRecord,
    ) -> Result<ActivePoolCreation> {
        validate_pool(pool)?;
        if pool.library_serial != serial || pool.revision != 1 {
            return Err(CatalogError::Invalid("新池带库或初始修订号无效"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let namespace =
                            routing::require_runtime_active_owner(&tr, serial, route, epoch)
                                .await?;
                        let record = pool_key(namespace, pool.id);
                        native_device::require_library_available(&tr, namespace, serial).await?;
                        let named = pool_name_key(namespace, &pool.name);
                        if retired_pool_revision_at(&tr, namespace, pool.id)
                            .await?
                            .is_some()
                        {
                            if tr.get(&record, false).await?.is_some() {
                                return Err(CatalogError::Corrupt("退役池仍存在主记录"));
                            }
                            return Ok(ActivePoolCreation::Retired);
                        }
                        if let Some(raw) = tr.get(&record, false).await? {
                            let saved = decode_pool(pool.id, &raw)?;
                            if saved.library_serial != serial {
                                return Ok(ActivePoolCreation::IdConflict);
                            }
                            require_pool_library(&tr, namespace, saved.id, serial).await?;
                            require_indexes(&tr, namespace, &saved).await?;
                            return Ok(
                                if saved.name == pool.name
                                    && saved.file_limit == pool.file_limit
                                    && saved.policy == pool.policy
                                {
                                    ActivePoolCreation::AlreadyExists(saved)
                                } else {
                                    ActivePoolCreation::IdConflict
                                },
                            );
                        }
                        if let Some(raw) = tr.get(&named, false).await? {
                            let owner = uuid_from(&raw)?;
                            if owner == pool.id {
                                return Err(CatalogError::Corrupt("池名称索引缺少主记录"));
                            }
                            let raw = tr
                                .get(&pool_key(namespace, owner), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("池名称索引孤立"))?;
                            if decode_pool(owner, &raw)?.name != pool.name {
                                return Err(CatalogError::Corrupt("池名称索引指向不同名称"));
                            }
                            return Ok(ActivePoolCreation::NameConflict);
                        }
                        let root = root_id(namespace, pool.id);
                        let child =
                            child_key(namespace, Uuid::nil(), pool.id.to_string().as_bytes());
                        let inode = node_key(namespace, root);
                        for key in [&child, &inode] {
                            if tr.get(key, false).await?.is_some() {
                                return Err(CatalogError::Corrupt("新池存在孤立索引或虚拟根"));
                            }
                        }
                        if requests::prefix_has_rows(&tr, &child_prefix(namespace, root)).await? {
                            return Err(CatalogError::Corrupt("新池根仍有孤立子项，拒绝补写"));
                        }
                        require_no_file_claims(&tr, namespace, pool.id).await?;
                        let raw = tr
                            .get(&node_key(namespace, Uuid::nil()), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        let parent = decode_node(&raw)?;
                        if parent.kind != NodeKind::Directory
                            || !parent.parent.is_nil()
                            || !parent.name.is_empty()
                        {
                            return Err(CatalogError::Corrupt("命名空间根目录无效"));
                        }
                        let root_metadata = initial_pool_root(pool)?;
                        let root_node = encode_node(&root_metadata);
                        object_metadata::ensure_at(
                            &tr,
                            namespace,
                            root_metadata
                                .attributes
                                .as_ref()
                                .expect("initial root has metadata"),
                        )
                        .await?;
                        tr.set(&record, &encode_pool(pool));
                        tr.set(&named, pool.id.as_bytes());
                        tr.set(&child, root.as_bytes());
                        tr.set(&inode, &root_node);
                        Ok(ActivePoolCreation::Created(pool.clone()))
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；仅池元数据、事务冲突与虚拟根，不访问设备"]
    async fn active_pool_creation_is_atomic_fenced_and_preserves_time() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        let serial = format!("SIM-POOLS-{namespace}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|id| json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,
            "role":if id==2 {"Leader"} else {"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let proof = verify_native_barriers(&serial, challenge, &replies).unwrap();
        let route = catalog
            .initialize_native_library(namespace, &proof)
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "原生池".into(),
            file_limit: 200_000,
            created_at_millis: 1_793_405_000_123,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &pool)
                .await
                .unwrap(),
            ActivePoolCreation::Created(pool.clone())
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap(),
            Some(pool.clone())
        );
        let path = pool_catalog_path(pool.id, b"/").unwrap();
        let root = match catalog
            .resolve_active_metadata_for_route(&serial, &path, route)
            .await
            .unwrap()
            .unwrap()
        {
            ResolvedPath::Directory { id, version: 1, .. } => id,
            _ => panic!("新池必须发布明确的目录属性版本"),
        };
        let root_key = node_key(namespace, root);
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&root_key, false).await.unwrap().unwrap().to_vec();
        let mut updated = decode_node(&original).unwrap();
        updated.directory_version = Some(7);
        tr.set(&root_key, &encode_node(&updated));
        tr.commit().await.unwrap();
        assert!(
            matches!(
                catalog
                    .resolve_active_metadata_for_route(&serial, &path, route)
                    .await
                    .unwrap(),
                Some(ResolvedPath::Directory { version: 7, .. })
            ),
            "查询应返回持久版本，不能补为1"
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap(),
            Some(pool.clone())
        );
        let tr = catalog.db.create_trx().unwrap();
        updated.directory_version = Some(0);
        tr.set(&root_key, &encode_node(&updated));
        tr.commit().await.unwrap();
        assert!(
            matches!(
                catalog
                    .resolve_active_metadata_for_route(&serial, &path, route)
                    .await,
                Err(CatalogError::Corrupt(_))
            ),
            "损坏的目录版本不得读取或回填"
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&root_key, &original);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .resolve_active_metadata_for_route(&serial, &path, route)
                .await
                .unwrap(),
            Some(ResolvedPath::Directory { .. })
        ));
        assert!(
            catalog
                .list_active_children_for_route(&serial, &path, None, 10, route)
                .await
                .unwrap()
                .unwrap()
                .items
                .is_empty()
        );
        let later = PoolRecord {
            created_at_millis: pool.created_at_millis + 99_999,
            ..pool.clone()
        };
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &later)
                .await
                .unwrap(),
            ActivePoolCreation::AlreadyExists(pool.clone())
        );
        assert!(
            catalog.create_pool(namespace, &pool).await.is_err(),
            "导入阶段接口不能绕过 Active gate"
        );
        let changed = PoolRecord {
            revision: 2,
            ..later
        };
        assert!(
            catalog
                .update_pool(namespace, &pool, &changed)
                .await
                .is_err(),
            "创建时间不可变"
        );
        let conflicting = PoolRecord {
            id: Uuid::new_v4(),
            ..pool.clone()
        };
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &conflicting)
                .await
                .unwrap(),
            ActivePoolCreation::NameConflict
        );
        let conflicting = PoolRecord {
            name: "其他名字".into(),
            ..pool.clone()
        };
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &conflicting)
                .await
                .unwrap(),
            ActivePoolCreation::IdConflict
        );

        // Concurrent unique-name admission publishes exactly one complete root.
        let a = PoolRecord {
            id: Uuid::new_v4(),
            name: "争用名称".into(),
            ..pool.clone()
        };
        let b = PoolRecord {
            id: Uuid::new_v4(),
            ..a.clone()
        };
        let (ra, rb) = tokio::join!(
            catalog.create_active_pool(&serial, route, 100, &a),
            catalog.create_active_pool(&serial, route, 100, &b)
        );
        let (winner, loser) = match (ra.unwrap(), rb.unwrap()) {
            (ActivePoolCreation::Created(_), ActivePoolCreation::NameConflict) => (a, b),
            (ActivePoolCreation::NameConflict, ActivePoolCreation::Created(_)) => (b, a),
            other => panic!("{other:?}"),
        };
        assert!(
            catalog
                .active_pool_for_route(&serial, route, winner.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, loser.id)
                .await
                .unwrap(),
            None
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(
                &child_key(namespace, Uuid::nil(), loser.id.to_string().as_bytes()),
                false
            )
            .await
            .unwrap()
            .is_none()
        );

        // A retry cannot repair a missing root or hide corruption as an empty page.
        let child = child_key(namespace, Uuid::nil(), pool.id.to_string().as_bytes());
        tr.clear(&child);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .create_active_pool(&serial, route, 100, &pool)
                .await
                .is_err()
        );
        assert!(
            catalog
                .list_active_pools_for_route(&serial, route, None, 100)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&child, false).await.unwrap().is_none());
        tr.set(&child, root_id(namespace, pool.id).as_bytes());
        tr.commit().await.unwrap();

        let named = pool_name_key(namespace, &pool.name);
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&named);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .is_err()
        );
        assert!(
            catalog
                .create_active_pool(&serial, route, 100, &pool)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&named, false).await.unwrap().is_none());
        tr.set(&named, pool.id.as_bytes());
        tr.commit().await.unwrap();

        let busy = PoolRecord {
            id: Uuid::new_v4(),
            name: "维护期间新池".into(),
            ..pool.clone()
        };
        let mut gate = management::prefix(namespace, b'K');
        gate.push(b'l');
        append_component(&mut gate, serial.as_bytes());
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&gate, false).await.unwrap().is_none());
        tr.set(&gate, b"retained-maintenance");
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.create_active_pool(&serial, route, 100, &busy).await,
            Err(CatalogError::PoolBusy)
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&pool_key(namespace, busy.id), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tr.get(&node_key(namespace, root_id(namespace, busy.id)), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tr.get(&gate, false).await.unwrap().unwrap().as_ref(),
            b"retained-maintenance"
        );
        tr.clear(&gate);
        tr.commit().await.unwrap();
        let retired = PoolRecord {
            id: Uuid::new_v4(),
            name: "已退役身份".into(),
            ..pool.clone()
        };
        let retired_key = retired_pool_key(namespace, retired.id);
        let mut retired_value = vec![1];
        retired_value.extend_from_slice(&2_u64.to_be_bytes());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&retired_key, &retired_value);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &retired)
                .await
                .unwrap(),
            ActivePoolCreation::Retired
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&pool_key(namespace, retired.id), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tr.get(&retired_key, false).await.unwrap().unwrap().as_ref(),
            retired_value.as_slice()
        );

        // A missing root with retained children is corruption, not a new empty pool.
        let with_children = PoolRecord {
            id: Uuid::new_v4(),
            name: "孤立子项".into(),
            ..pool.clone()
        };
        let pending_root = root_id(namespace, with_children.id);
        let leftover = child_key(namespace, pending_root, b"retained");
        let retained_inode = Uuid::new_v4();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&leftover, retained_inode.as_bytes());
        tr.commit().await.unwrap();
        let outcome = catalog
            .create_active_pool(&serial, route, 100, &with_children)
            .await;
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&pool_key(namespace, with_children.id), false)
                .await
                .unwrap()
                .is_none(),
            "不能接纳孤立子项；结果 {outcome:?}"
        );
        assert!(matches!(outcome, Err(CatalogError::Corrupt(_))));
        assert!(
            tr.get(&pool_name_key(namespace, &with_children.name), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tr.get(&node_key(namespace, pending_root), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tr.get(
                &child_key(
                    namespace,
                    Uuid::nil(),
                    with_children.id.to_string().as_bytes()
                ),
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            tr.get(&leftover, false).await.unwrap().unwrap().as_ref(),
            retained_inode.as_bytes()
        );

        let orphan = PoolRecord {
            id: Uuid::new_v4(),
            name: "孤立记录".into(),
            ..pool.clone()
        };
        let inode = node_key(namespace, root_id(namespace, orphan.id));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&inode, b"orphan");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .create_active_pool(&serial, route, 100, &orphan)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&inode, false).await.unwrap().as_deref(),
            Some(b"orphan".as_slice())
        );
        assert!(
            tr.get(&pool_key(namespace, orphan.id), false)
                .await
                .unwrap()
                .is_none()
        );

        let dangling = PoolRecord {
            id: Uuid::new_v4(),
            name: "孤立名称".into(),
            ..pool.clone()
        };
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &pool_name_key(namespace, &dangling.name),
            Uuid::new_v4().as_bytes(),
        );
        tr.commit().await.unwrap();
        assert!(
            catalog
                .create_active_pool(&serial, route, 100, &dangling)
                .await
                .is_err()
        );
        assert!(
            catalog
                .active_pool_for_route(&serial, route, dangling.id)
                .await
                .unwrap()
                .is_none()
        );

        catalog
            .claim_active_owner_epoch(&serial, 200)
            .await
            .unwrap();
        let stale = PoolRecord {
            id: Uuid::new_v4(),
            name: "失效请求".into(),
            ..pool
        };
        assert!(matches!(
            catalog
                .create_active_pool(&serial, route, 100, &stale)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, stale.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            catalog.active_library_route(&serial).await.unwrap(),
            Some(route)
        );
    }
}
