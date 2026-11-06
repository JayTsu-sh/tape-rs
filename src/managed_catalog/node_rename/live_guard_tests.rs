use super::*;
use crate::ltfs::index::NodeMeta;

pub(crate) fn setup() -> (NodeRenameRequest, Binding) {
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let root = Uuid::new_v4();
    let source = Uuid::new_v4();
    let volume = Uuid::new_v4();
    let scan = Uuid::new_v4();
    let attributes = NodeAttributes::new(
        NodeMeta {
            file_uid: 7,
            ..Default::default()
        },
        vec![],
        None,
    )
    .unwrap();
    let parent = NodeMutationParent {
        id: root,
        version: 1,
        attributes: Some(attributes.clone()),
    };
    let revision = |inode| NodeRenameRevision {
        namespace,
        inode,
        version: 1,
    };
    let request = NodeRenameRequest::from_baseline(
        &NodeRenameBaseline {
            library_serial: "SIM-UIDS".into(),
            owner_epoch: 1,
            request_epoch: 1,
            spec: NodeRenameSpec {
                pool,
                operation: Uuid::new_v4(),
                from: "/src".into(),
                to: "/dst".into(),
                source: revision(source),
                source_parent: revision(root),
                destination_parent: revision(root),
                target: None,
                no_replace: true,
            },
            source_parent: parent.clone(),
            destination_parent: parent,
            source: ResolvedPath::Directory {
                id: source,
                version: 1,
                attributes: Some(attributes),
            },
            target: None,
        },
        "2026-11-02T10:00:00.000000000Z",
    )
    .unwrap();
    let binding = Binding {
        source: ActiveIndexIdentity {
            volume_uuid: volume,
            generation: 5,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 100,
            },
            sha256: [1; 32],
            incremental: false,
        },
        scan: VolumeScan {
            generation: 1,
            id: scan,
        },
        placement: VolumePlacement {
            volume_id: volume,
            namespace,
            pool_id: pool,
            library_serial: request.library().into(),
            barcode: "UID0L8".into(),
        },
        pool_revision: 1,
        assignment_revision: 1,
    };
    (request, binding)
}
pub(crate) fn file(
    binding: &Binding,
    metadata_uid: u64,
    data_uid: u64,
    role: NodeRenameReferenceRole,
    link: bool,
) -> (NodeRenameLiveFile, PhysicalCopy) {
    let alias = metadata_uid != data_uid;
    let copy = PhysicalCopy {
        version: 1,
        origin: if link {
            CopyOrigin::NodeLink {
                management_epoch: 1,
                operation: Uuid::new_v4(),
            }
        } else if alias {
            CopyOrigin::Imported
        } else {
            CopyOrigin::FileRequest { request_epoch: 1 }
        },
        volume_id: binding.source.volume_uuid,
        volume_generation: 2,
        volume_scan_id: binding.scan.id,
        ltfs_file_uid: data_uid,
        size: if link { 0 } else { 4 },
        content_digest: if link {
            ContentDigest::Unknown
        } else {
            ContentDigest::Sha256([2; 32])
        },
        media: (link || alias).then(|| MediaProvenance {
            version: (1, 1),
            logical_file_uid: metadata_uid,
            physical_path: if alias {
                ".LTFSEE_DATA/object".into()
            } else {
                "native-link".into()
            },
            ee_alias: alias,
            native_symlink: link,
        }),
    };
    (
        NodeRenameLiveFile {
            node: NodeRenameRevision {
                namespace: binding.placement.namespace,
                inode: Uuid::new_v4(),
                version: 1,
            },
            copy_id: Uuid::new_v4(),
            metadata_uid,
            data_uid: (!link).then_some(data_uid),
            role,
        },
        copy,
    )
}

#[test]
fn live_guard_retains_role_counts_shared_data_unsigned_uids_and_page_independent_digest() {
    use NodeRenameReferenceRole::*;
    let (request, binding) = setup();
    let files = vec![
        file(&binding, 22, 11, Source, false),
        file(&binding, 23, 11, Target, false),
        file(&binding, 24, 11, Other, false),
        file(&binding, 25, 25, Other, true),
        file(&binding, u64::MAX, u64::MAX, Other, false),
    ];
    let mut guard =
        NodeRenameVolumeGuard::new(&request, [3; 32], binding.clone(), 16 << 20).unwrap();
    assert!(guard.metadata(22).is_err());
    guard.append(&files).unwrap();
    guard.append(std::slice::from_ref(&files[0])).unwrap();
    assert!(
        guard.seal(2, 1).is_err(),
        "a missing canonical source must not be treated as absent"
    );
    guard.seal(1, 1).unwrap();
    assert_eq!(
        (
            guard.stats().files,
            guard.stats().source_files,
            guard.stats().target_files,
            guard.stats().other_files,
            guard.stats().data_uids
        ),
        (5, 1, 1, 3, 2)
    );
    assert_eq!(
        guard.data(11).unwrap(),
        Some(NodeRenameDataReferences {
            source: 1,
            target: 1,
            other: 1
        })
    );
    assert!(
        guard.data(25).unwrap().is_none(),
        "a native link is not a data object reference"
    );
    assert_eq!(guard.metadata(u64::MAX).unwrap(), Some(files[4].0.clone()));
    assert_eq!(
        guard.data(u64::MAX).unwrap(),
        Some(NodeRenameDataReferences {
            other: 1,
            ..Default::default()
        })
    );
    assert!(
        guard.metadata(11).unwrap().is_none(),
        "data UID must not replace an EE metadata UID"
    );
    assert!(guard.metadata(0).is_err());
    assert!(guard.data(0).is_err());
    assert!(guard.conn.execute("DELETE FROM metadata", []).is_err());
    let mut other = NodeRenameVolumeGuard::new(&request, [3; 32], binding, 16 << 20).unwrap();
    for file in files.iter().rev() {
        other.append(std::slice::from_ref(file)).unwrap();
    }
    other.seal(1, 1).unwrap();
    assert_eq!(other.stats().sha256, guard.stats().sha256);
}

#[test]
fn live_guard_rejects_conflicting_metadata_missing_roles_and_budget_without_partial_counters() {
    let (request, binding) = setup();
    let source = file(&binding, 22, 11, NodeRenameReferenceRole::Source, false);
    let collision = file(&binding, 22, 11, NodeRenameReferenceRole::Other, false);
    let mut guard =
        NodeRenameVolumeGuard::new(&request, [3; 32], binding.clone(), 16 << 20).unwrap();
    assert!(guard.append(&[source.clone(), collision]).is_err());
    assert_eq!(guard.stats().files, 0);
    assert_eq!(
        guard
            .conn
            .query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        guard
            .conn
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    guard.append(std::slice::from_ref(&source)).unwrap();
    assert!(guard.seal(1, 1).is_err());
    guard.seal(1, 0).unwrap();
    let mut wrong = source.clone();
    wrong.0.node.namespace = Uuid::new_v4();
    let mut invalid =
        NodeRenameVolumeGuard::new(&request, [3; 32], binding.clone(), 16 << 20).unwrap();
    assert!(invalid.append(&[wrong]).is_err());
    let many = (1000..1500)
        .map(|uid| file(&binding, uid, uid, NodeRenameReferenceRole::Other, false))
        .collect::<Vec<_>>();
    let mut small = NodeRenameVolumeGuard::new(&request, [3; 32], binding, 64 << 10).unwrap();
    assert!(small.append(&many).is_err());
    assert_eq!(small.stats().files, 0);
    assert_eq!(
        small
            .conn
            .query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        small
            .conn
            .query_row("SELECT count(*) FROM data", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn live_guard_checks_counter_indexes_and_row_digests_after_capture() {
    let (request, binding) = setup();
    let files = vec![
        file(&binding, 22, 11, NodeRenameReferenceRole::Source, false),
        file(&binding, 23, 11, NodeRenameReferenceRole::Other, false),
    ];
    let mut guard = NodeRenameVolumeGuard::new(&request, [3; 32], binding, 16 << 20).unwrap();
    guard.append(&files).unwrap();
    guard.seal(1, 0).unwrap();
    guard.conn.pragma_update(None, "query_only", false).unwrap();
    guard
        .conn
        .execute(
            "UPDATE data SET source=0 WHERE uid=?",
            [11u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(guard.data(11).is_err());
    guard
        .conn
        .execute(
            "UPDATE data SET source=1 WHERE uid=?",
            [11u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(guard.data(11).unwrap().is_some());
    guard
        .conn
        .execute(
            "UPDATE metadata SET sha=? WHERE uid=?",
            params![[0u8; 32].as_slice(), 22u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(guard.metadata(22).is_err());
    assert!(guard.data(11).is_err());
}

#[test]
#[ignore = "需要隔离 FDB；退休 inode 的原历史 head 不得作为有效 UID 引用"]
fn current_guard_validates_retired_history_without_counting_it_as_live() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (_, binding) = setup();
        let namespace = binding.placement.namespace;
        let node = Uuid::new_v4();
        let head = FileHead {
            version: 3,
            size: 39,
            copy_id: Uuid::new_v4(),
        };
        let retired = retired_nodes::NodeRetirement {
            namespace,
            pool: binding.placement.pool_id,
            node,
            path: "/deleted".into(),
            kind: NodeKind::File,
            deleted_version: 4,
            previous_attributes: [1; 32],
            previous_head: Some(head.clone()),
            management_epoch: 1,
            operation: Uuid::new_v4(),
            result_digest: [2; 32],
        };
        let row = VolumeCopy {
            namespace,
            file_id: node,
            copy_id: head.copy_id,
            version: head.version,
        };
        let node_key = node_key(namespace, node);
        let head_key = head_key(namespace, node);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&node_key, &retired_nodes::encode(&retired).unwrap());
        tr.set(&head_key, &encode_head(&head));
        tr.commit().await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            current_at(&tr, namespace, &row, &binding).await.unwrap(),
            None,
            "unlink 的原历史 head 不代表仍存在当前文件"
        );
        for bad in [
            FileHead {
                version: head.version + 1,
                ..head.clone()
            },
            FileHead {
                size: head.size + 1,
                ..head.clone()
            },
            FileHead {
                copy_id: Uuid::new_v4(),
                ..head.clone()
            },
        ] {
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&head_key, &encode_head(&bad));
            tr.commit().await.unwrap();
            let tr = catalog.db.create_trx().unwrap();
            assert!(current_at(&tr, namespace, &row, &binding).await.is_err());
        }
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&head_key);
        tr.commit().await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            current_at(&tr, namespace, &row, &binding).await.unwrap(),
            None,
            "rename 目标退休并清除 head 后仍应排除其原引用"
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&node_key);
        tr.commit().await.unwrap();
    });
}
