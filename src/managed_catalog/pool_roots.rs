//! Canonical metadata for a pool's virtual root, distinct from a tape root.
use super::*;
use crate::ltfs::index::NodeMeta;

pub(in crate::managed_catalog) fn initial_pool_root(pool: &PoolRecord) -> Result<Node> {
    validate_pool(pool)?;
    let time = chrono::DateTime::from_timestamp_millis(pool.created_at_millis as i64)
        .ok_or(CatalogError::Invalid("池创建时间无法转换为 UTC"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let attributes = NodeAttributes::new(
        NodeMeta {
            // This is the virtual root's metadata UID, not an on-tape identity or
            // evidence that a physical volume's root has the same fileuid.
            file_uid: 1,
            readonly: false,
            creation_time: time.clone(),
            change_time: time.clone(),
            modify_time: time.clone(),
            access_time: time.clone(),
            backup_time: time,
        },
        vec![],
        None,
    )?;
    Ok(Node {
        kind: NodeKind::Directory,
        directory_version: Some(1),
        parent: Uuid::nil(),
        name: pool.id.to_string().into_bytes(),
        attributes: Some(attributes),
    })
}

pub(super) async fn require_metadata(tr: &Transaction, namespace: Uuid, node: &Node) -> Result<()> {
    let attributes = node
        .attributes
        .as_ref()
        .ok_or(CatalogError::Corrupt("池虚拟根缺少创建事件元数据"))?;
    let saved = object_metadata::read_at(tr, namespace, Some(attributes.digest())).await?;
    if attributes.meta().is_some()
        && saved.as_ref() != Some(&ObjectMetadata::from_attributes(attributes)?)
    {
        return Err(CatalogError::Corrupt("池根元数据投影与属性不一致"));
    }
    Ok(())
}

/// Generic directory creation cannot act as an implicit pool-root constructor.
pub(in crate::managed_catalog) async fn reject_generic_pool_root_at(
    tr: &Transaction,
    namespace: Uuid,
    parent: Uuid,
    name: &[u8],
) -> Result<()> {
    if !parent.is_nil() {
        return Ok(());
    }
    let Some(pool) = std::str::from_utf8(name)
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
    else {
        return Ok(());
    };
    match tr.get(&managed_key(namespace), false).await?.as_deref() {
        Some([]) => {
            require_managed_layout(tr, namespace).await?;
            if tr.get(&pool_key(namespace, pool), false).await?.is_some() {
                return Err(CatalogError::Invalid(
                    "池虚拟根必须通过专用池根初始化流程创建",
                ));
            }
        }
        Some(_) => return Err(CatalogError::Corrupt("受管命名空间标记")),
        None => {}
    }
    Ok(())
}

impl ManagedCatalog {
    /// Create a new Ready/Building pool root from its registered creation event.
    /// Existing prototype/mismatched roots are rejected, never backfilled.
    pub async fn create_pool_root(&self, namespace: Uuid, pool: Uuid) -> Result<DirectoryOutcome> {
        if namespace.is_nil() || pool.is_nil() {
            return Err(CatalogError::Invalid("池根身份不能为 nil"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let state = tr
                            .get(&namespace_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if !matches!(
                            decode_namespace_state(&state)?,
                            NamespaceState::Building | NamespaceState::Ready
                        ) {
                            return Ok(DirectoryOutcome::NamespaceNotReady);
                        }
                        if tr.get(&managed_key(namespace), false).await?.as_deref() != Some(&[]) {
                            return Err(CatalogError::Invalid("池根初始化要求受管命名空间"));
                        }
                        require_managed_layout(&tr, namespace).await?;
                        let pool = decode_pool(
                            pool,
                            &tr.get(&pool_key(namespace, pool), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?,
                        )?;
                        require_pool_library(&tr, namespace, pool.id, &pool.library_serial).await?;
                        if tr
                            .get(&pool_name_key(namespace, &pool.name), false)
                            .await?
                            .as_deref()
                            != Some(pool.id.as_bytes().as_slice())
                        {
                            return Err(CatalogError::Corrupt("池根初始化名称索引不一致"));
                        }
                        let root = active_pools::root_id(namespace, pool.id);
                        let value = encode_node(&initial_pool_root(&pool)?);
                        let child =
                            child_key(namespace, Uuid::nil(), pool.id.to_string().as_bytes());
                        let inode = node_key(namespace, root);
                        let namespace_root = decode_node(
                            &tr.get(&node_key(namespace, Uuid::nil()), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("命名空间根缺失"))?,
                        )?;
                        if namespace_root.kind != NodeKind::Directory
                            || !namespace_root.parent.is_nil()
                            || !namespace_root.name.is_empty()
                        {
                            return Err(CatalogError::Corrupt("命名空间根损坏"));
                        }
                        active_pools::require_no_file_claims(&tr, namespace, pool.id).await?;
                        let old_child = tr.get(&child, false).await?;
                        let old_node = tr.get(&inode, false).await?;
                        match (old_child, old_node) {
                            (Some(c), Some(n))
                                if c.as_ref() == root.as_bytes()
                                    && n.as_ref() == value.as_slice() =>
                            {
                                pool_roots::require_metadata(&tr, namespace, &decode_node(&n)?)
                                    .await?;
                                Ok(DirectoryOutcome::AlreadyExists)
                            }
                            (None, None) => {
                                if requests::prefix_has_rows(&tr, &child_prefix(namespace, root))
                                    .await?
                                {
                                    return Err(CatalogError::Corrupt(
                                        "缺失池根仍有孤立子项，拒绝补写",
                                    ));
                                }
                                // Materialization is covered by the staged projection
                                // proof; it does not change source declaration revision.
                                let node = decode_node(&value)?;
                                object_metadata::ensure_at(
                                    &tr,
                                    namespace,
                                    node.attributes.as_ref().expect("initial root has metadata"),
                                )
                                .await?;
                                tr.set(&child, root.as_bytes());
                                tr.set(&inode, &value);
                                Ok(DirectoryOutcome::Created)
                            }
                            _ => Err(CatalogError::Corrupt("已有池根缺失或元数据不符，拒绝补写")),
                        }
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use serde_json::json;

    fn pool(id: Uuid, serial: &str, created_at_millis: u64) -> PoolRecord {
        PoolRecord {
            id,
            library_serial: serial.into(),
            name: "root".into(),
            file_limit: 200_000,
            created_at_millis,
            policy: PoolPolicy::default(),
            revision: 1,
        }
    }

    #[test]
    fn virtual_root_metadata_uses_one_creation_event_and_rejects_invalid_times() {
        for millis in [1, 123, 1_798_000_000_123, 253_402_300_799_999] {
            let record = pool(Uuid::new_v4(), "ROOT-META", millis);
            let node = initial_pool_root(&record).unwrap();
            assert_eq!(node.kind, NodeKind::Directory);
            assert_eq!(node.directory_version, Some(1));
            assert!(node.parent.is_nil());
            assert_eq!(node.name, record.id.to_string().as_bytes());
            let attrs = node.attributes.unwrap();
            let meta = attrs.meta().unwrap();
            assert_eq!(meta.file_uid, 1);
            assert!(!meta.readonly);
            assert!(attrs.xattrs().unwrap().is_empty());
            let event = chrono::DateTime::from_timestamp_millis(millis as i64)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            for field in [
                &meta.creation_time,
                &meta.change_time,
                &meta.modify_time,
                &meta.access_time,
                &meta.backup_time,
            ] {
                assert_eq!(*field, event);
            }
        }
        for millis in [0, 253_402_300_800_000, u64::MAX] {
            assert!(initial_pool_root(&pool(Uuid::new_v4(), "ROOT-META", millis)).is_err());
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Building/Ready 池根初始化、索引与拒绝修补，不操作介质"]
    async fn declared_pool_roots_are_initialized_explicitly_and_never_repaired() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        for building in [true, false] {
            let ns = Uuid::new_v4();
            let serial = format!("ROOT-DECL-{ns}");
            if building {
                catalog.begin_managed_import(ns).await.unwrap();
            } else {
                catalog.create_managed_namespace(ns).await.unwrap();
            }
            catalog
                .register_physical_library(ns, &serial)
                .await
                .unwrap();
            let record = pool(Uuid::new_v4(), &serial, 1_798_000_000_123);
            catalog.create_pool(ns, &record).await.unwrap();
            let root = active_pools::root_id(ns, record.id);
            let name = record.id.to_string();
            let tr = catalog.db.create_trx().unwrap();
            let source_before = source_revision(&tr, ns).await.unwrap();
            let generic = if building {
                catalog
                    .create_import_directory(ns, Uuid::nil(), name.as_bytes(), root)
                    .await
            } else {
                catalog
                    .create_directory(ns, Uuid::nil(), name.as_bytes(), root)
                    .await
            };
            assert!(matches!(generic, Err(CatalogError::Invalid(_))));
            assert!(
                catalog
                    .db
                    .create_trx()
                    .unwrap()
                    .get(&node_key(ns, root), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                catalog.create_pool_root(ns, record.id).await.unwrap(),
                DirectoryOutcome::Created
            );
            assert_eq!(
                catalog.create_pool_root(ns, record.id).await.unwrap(),
                DirectoryOutcome::AlreadyExists
            );
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(source_revision(&tr, ns).await.unwrap(), source_before);
            let inode = node_key(ns, root);
            let child = child_key(ns, Uuid::nil(), name.as_bytes());
            let original = tr.get(&inode, false).await.unwrap().unwrap().to_vec();
            assert_eq!(original, encode_node(&initial_pool_root(&record).unwrap()));
            for key in [&inode, &child] {
                let tr = catalog.db.create_trx().unwrap();
                let saved = tr.get(key, false).await.unwrap().unwrap().to_vec();
                tr.clear(key);
                tr.commit().await.unwrap();
                assert!(matches!(
                    catalog.create_pool_root(ns, record.id).await,
                    Err(CatalogError::Corrupt(_))
                ));
                let tr = catalog.db.create_trx().unwrap();
                assert!(tr.get(key, false).await.unwrap().is_none());
                tr.set(key, &saved);
                tr.commit().await.unwrap();
            }
            let mut missing = decode_node(&original).unwrap();
            missing.attributes = None;
            let missing = encode_node(&missing);
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&inode, &missing);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog.create_pool_root(ns, record.id).await,
                Err(CatalogError::Corrupt(_))
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                tr.get(&inode, false).await.unwrap().unwrap().as_ref(),
                missing.as_slice()
            );
            tr.set(&inode, &original);
            tr.commit().await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；池虚拟根事件元数据与缺失拒绝，不操作介质"]
    async fn pool_roots_have_creation_event_metadata_and_never_backfill_missing_attrs() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-POOL-ROOT-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "root".into(),
            file_limit: 200_000,
            created_at_millis: 1_798_000_000_123,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let path = pool_catalog_path(pool.id, b"/").unwrap();
        let Some(ResolvedPath::Directory {
            version,
            attributes,
            ..
        }) = catalog
            .resolve_active_path_for_route(&serial, &path, route)
            .await
            .unwrap()
        else {
            panic!("pool directory");
        };
        let attributes = attributes.expect("池根必须保存 FDB 创建事件元数据");
        let meta = attributes.meta().unwrap();
        assert_eq!(version, 1);
        assert_eq!(meta.file_uid, 1);
        assert!(!meta.readonly);
        assert!(attributes.xattrs().unwrap().is_empty());
        assert!(attributes.symlink().is_none());
        let event = chrono::DateTime::from_timestamp_millis(pool.created_at_millis as i64)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for actual in [
            &meta.creation_time,
            &meta.change_time,
            &meta.modify_time,
            &meta.access_time,
            &meta.backup_time,
        ] {
            assert_eq!(*actual, event);
        }
        let mut retry = pool.clone();
        retry.created_at_millis += 1000;
        assert_eq!(
            catalog
                .create_active_pool(&serial, route, 100, &retry)
                .await
                .unwrap(),
            ActivePoolCreation::AlreadyExists(pool.clone())
        );
        assert_eq!(
            catalog
                .resolve_active_path_for_route(&serial, &path, route)
                .await
                .unwrap(),
            Some(ResolvedPath::Directory {
                id: active_pools::root_id(ns, pool.id),
                version: 1,
                attributes: Some(attributes)
            })
        );
        let key = node_key(ns, active_pools::root_id(ns, pool.id));
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&key, false).await.unwrap().unwrap().to_vec();
        let mut missing = decode_node(&original).unwrap();
        missing.attributes = None;
        let missing = encode_node(&missing);
        tr.set(&key, &missing);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.active_pool_for_route(&serial, route, pool.id).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog.create_active_pool(&serial, route, 100, &pool).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert_eq!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            missing.as_slice()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&key, &original);
        tr.commit().await.unwrap();

        // An inline attribute value without its immutable projection is also
        // corruption. Neither a read nor a create retry may synthesize it.
        let mut changed = decode_node(&original).unwrap();
        let mut meta = changed.attributes.as_ref().unwrap().meta().unwrap().clone();
        meta.modify_time = "2030-01-02T03:04:05.123456789Z".into();
        let attributes = NodeAttributes::new(meta, vec![], None).unwrap();
        let digest = attributes.digest();
        changed.attributes = Some(attributes);
        let changed = encode_node(&changed);
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            object_metadata::read_at(&tr, ns, Some(digest))
                .await
                .is_err()
        );
        tr.set(&key, &changed);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.active_pool_for_route(&serial, route, pool.id).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog.create_active_pool(&serial, route, 100, &pool).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            object_metadata::read_at(&tr, ns, Some(digest))
                .await
                .is_err()
        );
        assert_eq!(
            tr.get(&key, false).await.unwrap().unwrap().as_ref(),
            changed.as_slice()
        );
        tr.set(&key, &original);
        tr.commit().await.unwrap();
    }
}
