//! Read-only logical preflight for node mutations. The result is a snapshot,
//! not admission, a lock, permission to write media, or a publication proof.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationSpec {
    pub pool: Uuid,
    pub operation: Uuid,
    pub path: String,
    pub expected_version: Option<u64>,
    pub mutation: NodeMutation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationParent {
    pub id: Uuid,
    pub version: u64,
    /// Exact attribute value/reference at the read version. Canonical inputs
    /// reject missing values; formatting times must not be substituted.
    pub attributes: Option<NodeAttributes>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationBaseline {
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool: Uuid,
    pub operation: Uuid,
    pub owner_epoch: u64,
    /// Management window; independent of upload admission quotas and epochs.
    pub request_epoch: u64,
    pub path: String,
    pub parent: NodeMutationParent,
    /// Stable new inode for a create, or the current target inode otherwise.
    pub node: Uuid,
    /// Includes the current file head/copy/placement or directory revision.
    /// Large attributes remain immutable references; hydrate them separately.
    pub current: Option<ResolvedPath>,
}

pub(super) fn validate_spec(spec: &NodeMutationSpec) -> Result<Vec<u8>> {
    if spec.operation.is_nil()
        || spec.path == "/"
        || !crate::ltfs::xml_text_valid(&spec.path)
        || matches!(spec.expected_version, Some(0 | u64::MAX))
    {
        return Err(CatalogError::Invalid("节点变更路径、身份或版本无效"));
    }
    let creating = matches!(
        spec.mutation,
        NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. }
    );
    if creating != spec.expected_version.is_none() {
        return Err(CatalogError::Invalid("节点变更创建/现有版本条件不符"));
    }
    spec.mutation.validate_input()?;
    pool_catalog_path(spec.pool, spec.path.as_bytes())
}

pub(super) fn target_id(
    spec: &NodeMutationSpec,
    current: Option<&ResolvedPath>,
) -> Result<Option<Uuid>> {
    let (id, version, directory) = match current {
        None => {
            return if spec.expected_version.is_none() {
                Ok(None)
            } else {
                Err(CatalogError::Invalid("节点变更目标不存在"))
            };
        }
        Some(ResolvedPath::Directory { id, version, .. }) => (*id, *version, true),
        Some(ResolvedPath::File { id, head, .. }) => (*id, head.version, false),
    };
    if spec.expected_version != Some(version) {
        return Err(CatalogError::Invalid("节点变更目标已存在或版本已改变"));
    }
    match spec.mutation {
        NodeMutation::Unlink if directory => {
            Err(CatalogError::Invalid("unlink 目标不是文件或链接"))
        }
        NodeMutation::RemoveDirectory if !directory => {
            Err(CatalogError::Invalid("rmdir 目标不是目录"))
        }
        _ => Ok(Some(id)),
    }
}

pub(super) fn new_inode(namespace: Uuid, request_epoch: u64, operation: Uuid) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/node-mutation-inode/1");
    hash.update(namespace.as_bytes());
    hash.update(request_epoch.to_be_bytes());
    hash.update(operation.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("SHA prefix");
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Uuid::from_bytes(bytes)
}

/// All reads use one transaction. Future admission must call this again and
/// compare its saved baseline before installing persistent mutation claims.
pub(super) async fn baseline_at(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    spec: &NodeMutationSpec,
) -> Result<NodeMutationBaseline> {
    baseline_for_epoch_at(tr, serial, route, owner, spec, None).await
}

pub(super) async fn baseline_for_epoch_at(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    spec: &NodeMutationSpec,
    request_epoch: Option<u64>,
) -> Result<NodeMutationBaseline> {
    let catalog_path = validate_spec(spec)?;
    control::validate_physical_serial(serial)?;
    let namespace = routing::require_runtime_active_owner(tr, serial, route, owner).await?;
    control::require_pool_library(tr, namespace, spec.pool, serial).await?;
    let pool = control::decode_pool(
        spec.pool,
        &tr.get(&control::pool_key(namespace, spec.pool), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    active_pools::require_indexes(tr, namespace, &pool).await?;
    control::native_media::require_pool_readable(tr, namespace, spec.pool).await?;
    let components = lookup::path_components(&catalog_path)?;
    let (&name, parents) = components.split_last().expect("nonroot pool path");
    let parent =
        match lookup::resolve_path_at_mode(tr, namespace, parents.to_vec(), false, true).await? {
            Some(ResolvedPath::Directory {
                id,
                version,
                attributes,
            }) => NodeMutationParent {
                id,
                version,
                attributes,
            },
            _ => return Err(CatalogError::Invalid("节点变更父目录不存在或不是目录")),
        };
    if parent.version == u64::MAX && spec.mutation.changes_parent_attributes() {
        return Err(CatalogError::Invalid("节点变更父目录版本已耗尽"));
    }
    for claim in [
        reservation::path_key_for(namespace, parent.id, name),
        active_batch::path_claim_key_for(namespace, parent.id, name),
    ] {
        if tr.get(&claim, false).await?.is_some() {
            return Err(CatalogError::Incomplete);
        }
    }
    let current = lookup::resolve_path_at_mode(tr, namespace, components, false, true).await?;
    if let Some(ResolvedPath::File { placement, .. }) = &current
        && (placement.namespace != namespace
            || placement.pool_id != spec.pool
            || placement.library_serial != serial)
    {
        return Err(CatalogError::Corrupt("节点变更副本与池或带库归属不一致"));
    }
    let existing = target_id(spec, current.as_ref())?;
    if let Some(id) = existing {
        for claim in [
            reservation::file_key_for(namespace, id),
            active_batch::file_claim_key_for(namespace, id),
        ] {
            if tr.get(&claim, false).await?.is_some() {
                return Err(CatalogError::Incomplete);
            }
        }
        if spec.mutation == NodeMutation::RemoveDirectory {
            if requests::prefix_has_rows(tr, &child_prefix(namespace, id)).await? {
                return Err(CatalogError::Invalid("rmdir 目标目录非空"));
            }
            for prefix in [
                reservation::path_prefix_for(namespace, id),
                active_batch::path_claim_prefix_for(namespace, id),
            ] {
                if requests::prefix_has_rows(tr, &prefix).await? {
                    return Err(CatalogError::Incomplete);
                }
            }
        }
    }
    let window = control::management::window(tr, namespace).await?;
    let request_epoch = request_epoch.unwrap_or(window.accepting);
    if request_epoch < window.first_retained || request_epoch > window.accepting {
        return Err(CatalogError::Invalid("节点请求代次不在保留窗口"));
    }
    let node = existing.unwrap_or_else(|| new_inode(namespace, request_epoch, spec.operation));
    if node == parent.id {
        return Err(CatalogError::Corrupt("节点变更目标与父目录身份冲突"));
    }
    if existing.is_none() {
        if tr.get(&node_key(namespace, node), false).await?.is_some() {
            return Err(CatalogError::Invalid(
                "节点变更操作已占用 inode，请查询原请求",
            ));
        }
        if tr.get(&head_key(namespace, node), false).await?.is_some() {
            return Err(CatalogError::Corrupt("新建节点 inode 存在孤立 head"));
        }
        for claim in [
            reservation::file_key_for(namespace, node),
            active_batch::file_claim_key_for(namespace, node),
        ] {
            if tr.get(&claim, false).await?.is_some() {
                return Err(CatalogError::Incomplete);
            }
        }
    }
    if existing.is_none() || matches!(current, Some(ResolvedPath::Directory { .. })) {
        if tr.get(&head_key(namespace, node), false).await?.is_some() {
            return Err(CatalogError::Corrupt("目录或新建节点存在孤立文件 head"));
        }
        let mut copies = copy_prefix(namespace);
        copies.extend_from_slice(node.as_bytes());
        if requests::prefix_has_rows(tr, &copies).await? {
            return Err(CatalogError::Corrupt("目录或新建节点存在孤立文件副本"));
        }
    }
    Ok(NodeMutationBaseline {
        library_serial: serial.into(),
        namespace,
        pool: spec.pool,
        operation: spec.operation,
        owner_epoch: owner,
        request_epoch,
        path: spec.path.clone(),
        parent,
        node,
        current,
    })
}

impl ManagedCatalog {
    /// No admission, mutation, tape selection or I/O. Revalidate in the actual
    /// admission transaction; do not treat this snapshot as a write permit.
    pub async fn prepare_node_mutation_baseline(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        spec: &NodeMutationSpec,
    ) -> Result<NodeMutationBaseline> {
        baseline_at(&self.db.create_trx()?, serial, route, owner, spec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{NodeMeta, Xattr};
    use serde_json::json;

    fn spec(path: &str, expected_version: Option<u64>, mutation: NodeMutation) -> NodeMutationSpec {
        NodeMutationSpec {
            pool: Uuid::new_v4(),
            operation: Uuid::new_v4(),
            path: path.into(),
            expected_version,
            mutation,
        }
    }

    #[test]
    fn logical_preflight_rejects_bad_paths_versions_and_mutation_syntax() {
        let create = spec("/valid", None, NodeMutation::CreateDirectory);
        assert!(validate_spec(&create).is_ok());
        for path in [
            "", "relative", "/", "/a/", "/a//b", "/a/../b", "/a/.", "/a/\0", "/a/\u{1}",
        ] {
            let bad = NodeMutationSpec {
                path: path.into(),
                ..create.clone()
            };
            assert!(validate_spec(&bad).is_err(), "{path:?}");
        }
        for path in [
            format!("/{}", "x".repeat(256)),
            format!("/{}", "x".repeat(2049)),
            format!("/{}", vec!["x"; usize::from(MAX_DEPTH)].join("/")),
        ] {
            assert!(
                validate_spec(&NodeMutationSpec {
                    path,
                    ..create.clone()
                })
                .is_err()
            );
        }
        assert!(
            validate_spec(&NodeMutationSpec {
                path: format!("/{}", vec!["x"; usize::from(MAX_DEPTH) - 1].join("/")),
                ..create.clone()
            })
            .is_ok()
        );
        for bad in [
            NodeMutationSpec {
                pool: Uuid::nil(),
                ..create.clone()
            },
            NodeMutationSpec {
                operation: Uuid::nil(),
                ..create.clone()
            },
            NodeMutationSpec {
                expected_version: Some(0),
                ..create.clone()
            },
            NodeMutationSpec {
                expected_version: Some(1),
                ..create.clone()
            },
            NodeMutationSpec {
                expected_version: Some(u64::MAX),
                ..create.clone()
            },
            NodeMutationSpec {
                mutation: NodeMutation::Unlink,
                ..create.clone()
            },
        ] {
            assert!(validate_spec(&bad).is_err());
        }
        for mutation in [
            NodeMutation::CreateSymlink {
                target: String::new(),
            },
            NodeMutation::CreateSymlink {
                target: "bad\u{1}".into(),
            },
            NodeMutation::RemoveXattr {
                key: "ltfs.hash.sha256sum".into(),
            },
            NodeMutation::RemoveXattr {
                key: "tapers.fdb.nodeMutation".into(),
            },
            NodeMutation::ChangeMetadata {
                readonly: None,
                access_time: None,
                modify_time: None,
            },
            NodeMutation::ChangeMetadata {
                readonly: None,
                access_time: Some("yesterday".into()),
                modify_time: None,
            },
            NodeMutation::SetXattr {
                attribute: Xattr {
                    key: "unknown".into(),
                    value: "?".into(),
                    base64: true,
                },
                mode: XattrMode::Upsert,
            },
            NodeMutation::SetXattr {
                attribute: Xattr {
                    key: "unknown".into(),
                    value: "bad\u{1}".into(),
                    base64: false,
                },
                mode: XattrMode::Upsert,
            },
        ] {
            assert!(mutation.validate_input().is_err(), "{mutation:?}");
        }
        let d = ResolvedPath::Directory {
            id: Uuid::new_v4(),
            version: 7,
            attributes: None,
        };
        assert!(target_id(&create, Some(&d)).is_err());
        assert!(
            target_id(
                &spec("/d", Some(6), NodeMutation::RemoveDirectory),
                Some(&d)
            )
            .is_err()
        );
        assert!(target_id(&spec("/d", Some(7), NodeMutation::Unlink), Some(&d)).is_err());
        assert!(
            target_id(
                &spec("/d", Some(7), NodeMutation::RemoveDirectory),
                Some(&d)
            )
            .unwrap()
            .is_some()
        );
        assert!(target_id(&spec("/d", Some(7), NodeMutation::RemoveDirectory), None).is_err());
        let ns = Uuid::new_v4();
        let op = Uuid::new_v4();
        let first = new_inode(ns, 1, op);
        assert!(!first.is_nil());
        assert_eq!(first, new_inode(ns, 1, op));
        assert_ne!(first, new_inode(ns, 2, op));
        assert_ne!(first, new_inode(Uuid::new_v4(), 1, op));
        assert_ne!(first, new_inode(ns, 1, Uuid::new_v4()));
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；目录夹具、单读版本和版本/归属预检，不操作介质"]
    async fn logical_node_preflight_is_coherent_scoped_read_only_and_not_admission() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-NODE-BASELINE-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
            name: "baseline".into(),
            file_limit: 200_000,
            created_at_millis: 1_798_000_000_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let root = active_pools::root_id(ns, pool.id);
        let create = NodeMutationSpec {
            pool: pool.id,
            ..spec("/new", None, NodeMutation::CreateDirectory)
        };
        macro_rules! prepare {
            ($spec:expr) => {
                catalog.prepare_node_mutation_baseline(&serial, route, 100, $spec)
            };
        }
        let first = prepare!(&create).await.unwrap();
        assert_eq!(first, prepare!(&create).await.unwrap());
        assert_eq!(first.parent.id, root);
        assert_eq!(first.parent.version, 1);
        let root_meta = first.parent.attributes.as_ref().unwrap().meta().unwrap();
        assert_eq!(root_meta.file_uid, 1);
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&root_meta.creation_time)
                .unwrap()
                .timestamp_millis(),
            pool.created_at_millis as i64
        );
        assert_eq!(first.request_epoch, 1);
        assert!(first.current.is_none());
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&node_key(ns, first.node), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );

        // Reusing an operation for another path must not overwrite the inode
        // it already named, or ignore a pending/orphan inode-level row.
        for row in [
            node_key(ns, first.node),
            head_key(ns, first.node),
            reservation::file_key_for(ns, first.node),
            active_batch::file_claim_key_for(ns, first.node),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            let collision = Node {
                kind: NodeKind::Directory,
                directory_version: Some(1),
                parent: root,
                name: b"occupied".to_vec(),
                attributes: None,
            };
            if row == node_key(ns, first.node) {
                tr.set(&row, &encode_node(&collision));
                tr.set(&child_key(ns, root, b"occupied"), first.node.as_bytes());
            } else {
                tr.set(&row, b"fault-injected-inode-row");
            }
            tr.commit().await.unwrap();
            assert!(
                prepare!(&create).await.is_err(),
                "新建 inode 已被占用: {row:?}"
            );
            let tr = catalog.db.create_trx().unwrap();
            tr.clear(&row);
            tr.clear(&child_key(ns, root, b"occupied"));
            tr.commit().await.unwrap();
        }

        // Real Directory-shaped logical fixtures only: no fake files, receipts,
        // extents, batch sealing or claim of on-tape publication.
        let parent = Uuid::new_v4();
        let empty = Uuid::new_v4();
        let full = Uuid::new_v4();
        let nested = Uuid::new_v4();
        let time = "2026-10-31T08:00:00.000000000Z";
        let attrs = NodeAttributes::new(
            NodeMeta {
                file_uid: 7,
                readonly: true,
                creation_time: time.into(),
                change_time: time.into(),
                modify_time: time.into(),
                access_time: time.into(),
                backup_time: time.into(),
            },
            vec![Xattr {
                key: "unknown".into(),
                value: "preserve".into(),
                base64: false,
            }],
            None,
        )
        .unwrap();
        let parent_node = Node {
            kind: NodeKind::Directory,
            directory_version: Some(7),
            parent: root,
            name: b"parent".to_vec(),
            attributes: Some(attrs.clone()),
        };
        let tr = catalog.db.create_trx().unwrap();
        for (id, node) in [
            (parent, parent_node.clone()),
            (
                empty,
                Node {
                    kind: NodeKind::Directory,
                    directory_version: Some(3),
                    parent: root,
                    name: b"empty".to_vec(),
                    attributes: None,
                },
            ),
            (
                full,
                Node {
                    kind: NodeKind::Directory,
                    directory_version: Some(2),
                    parent: root,
                    name: b"full".to_vec(),
                    attributes: None,
                },
            ),
            (
                nested,
                Node {
                    kind: NodeKind::Directory,
                    directory_version: Some(1),
                    parent: full,
                    name: b"nested".to_vec(),
                    attributes: None,
                },
            ),
        ] {
            tr.set(&node_key(ns, id), &encode_node(&node));
            tr.set(&child_key(ns, node.parent, &node.name), id.as_bytes());
        }
        tr.commit().await.unwrap();
        let child = NodeMutationSpec {
            path: "/parent/new".into(),
            ..create.clone()
        };
        let old = prepare!(&child).await.unwrap();
        assert_eq!(old.parent.id, parent);
        assert_eq!(old.parent.version, 7);
        assert_eq!(old.parent.attributes, Some(attrs.clone()));
        let rm = NodeMutationSpec {
            path: "/empty".into(),
            expected_version: Some(3),
            mutation: NodeMutation::RemoveDirectory,
            ..create.clone()
        };
        assert_eq!(prepare!(&rm).await.unwrap().node, empty);
        assert!(
            prepare!(&NodeMutationSpec {
                path: "/full".into(),
                expected_version: Some(2),
                ..rm.clone()
            })
            .await
            .is_err()
        );
        assert!(
            prepare!(&NodeMutationSpec {
                expected_version: Some(2),
                ..rm.clone()
            })
            .await
            .is_err()
        );
        assert!(
            prepare!(&NodeMutationSpec {
                mutation: NodeMutation::Unlink,
                ..rm.clone()
            })
            .await
            .is_err()
        );
        assert!(
            prepare!(&NodeMutationSpec {
                path: "/missing/new".into(),
                ..create.clone()
            })
            .await
            .is_err()
        );

        // Unknown pending child claims must never be interpreted as empty.
        for claim in [
            reservation::path_key_for(ns, empty, b"incoming"),
            active_batch::path_claim_key_for(ns, empty, b"incoming"),
            reservation::path_key_for(ns, root, b"empty"),
            active_batch::path_claim_key_for(ns, root, b"empty"),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&claim, b"fault-injected-claim");
            tr.commit().await.unwrap();
            assert!(matches!(prepare!(&rm).await, Err(CatalogError::Incomplete)));
            let tr = catalog.db.create_trx().unwrap();
            tr.clear(&claim);
            tr.commit().await.unwrap();
        }
        assert!(prepare!(&rm).await.is_ok());
        // Read-version pinning must not mix a new revision with old attributes.
        let pinned = catalog.db.create_trx().unwrap();
        pinned.get_read_version().await.unwrap();
        let mut changed = parent_node.clone();
        changed.directory_version = Some(9);
        changed.attributes = None;
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&node_key(ns, parent), &encode_node(&changed));
        tr.commit().await.unwrap();
        let old = baseline_at(&pinned, &serial, route, 100, &child)
            .await
            .unwrap();
        assert_eq!(old.parent.version, 7);
        assert_eq!(old.parent.attributes, Some(attrs));
        let current = prepare!(&child).await.unwrap();
        assert_eq!(current.parent.version, 9);
        assert!(current.parent.attributes.is_none());

        // This is only a validator. Demonstrate real read/write conflicts,
        // without making its result a persistent authorization or publication.
        let validation = catalog.db.create_trx().unwrap();
        baseline_at(&validation, &serial, route, 100, &child)
            .await
            .unwrap();
        changed.directory_version = Some(10);
        let concurrent = catalog.db.create_trx().unwrap();
        concurrent.set(&node_key(ns, parent), &encode_node(&changed));
        concurrent.commit().await.unwrap();
        let probe = management::prefix(ns, b'x');
        validation.set(&probe, b"must-not-commit");
        let error = validation.commit().await.unwrap_err();
        assert_eq!(error.code(), 1020);
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&probe, false)
                .await
                .unwrap()
                .is_none()
        );
        for pending in [false, true] {
            let validation = catalog.db.create_trx().unwrap();
            baseline_at(&validation, &serial, route, 100, &rm)
                .await
                .unwrap();
            let key = if pending {
                reservation::path_key_for(ns, empty, b"racing")
            } else {
                child_key(ns, empty, b"racing")
            };
            let concurrent = catalog.db.create_trx().unwrap();
            concurrent.set(&key, b"fault-injected-child");
            concurrent.commit().await.unwrap();
            validation.set(&probe, b"must-not-commit");
            assert_eq!(validation.commit().await.unwrap_err().code(), 1020);
            let cleanup = catalog.db.create_trx().unwrap();
            cleanup.clear(&key);
            cleanup.commit().await.unwrap();
        }

        let tr = catalog.db.create_trx().unwrap();
        let root_bytes = tr
            .get(&node_key(ns, root), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut root_node = decode_node(&root_bytes).unwrap();
        root_node.directory_version = Some(u64::MAX);
        tr.set(&node_key(ns, root), &encode_node(&root_node));
        tr.commit().await.unwrap();
        assert!(matches!(
            prepare!(&create).await,
            Err(CatalogError::Invalid(_))
        ));
        assert!(matches!(prepare!(&rm).await, Err(CatalogError::Invalid(_))));
        let metadata = NodeMutationSpec {
            mutation: NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
            ..rm.clone()
        };
        assert!(prepare!(&metadata).await.is_ok());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&node_key(ns, root), &root_bytes);
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 2,
            }),
        );
        tr.set(
            &requests::window_key(ns),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 9,
            }),
        );
        tr.commit().await.unwrap();
        let next = prepare!(&create).await.unwrap();
        assert_eq!(next.request_epoch, 2);
        assert_ne!(next.node, first.node);
        assert_eq!(
            requests::window(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
                .accepting,
            9
        );
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            prepare!(&create).await,
            Err(CatalogError::OwnerFenced)
        ));
        let adopted = catalog
            .prepare_node_mutation_baseline(&serial, route, 101, &create)
            .await
            .unwrap();
        assert_eq!(adopted.node, next.node);
        assert!(
            catalog
                .prepare_node_mutation_baseline("OTHER-LIBRARY", route, 101, &create)
                .await
                .is_err()
        );
        let wrong = NodeMutationSpec {
            pool: Uuid::new_v4(),
            ..create.clone()
        };
        assert!(
            catalog
                .prepare_node_mutation_baseline(&serial, route, 101, &wrong)
                .await
                .is_err()
        );
        assert!(
            !requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            !requests::prefix_has_rows(&tr, &management::prefix(ns, b'q'))
                .await
                .unwrap()
        );
        assert!(
            tr.get(&node_key(ns, adopted.node), false)
                .await
                .unwrap()
                .is_none()
        );
    }
}
