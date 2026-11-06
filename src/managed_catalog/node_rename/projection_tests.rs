use super::*;
use crate::ltfs::index::NodeMeta;

fn attrs(uid: u64) -> NodeAttributes {
    NodeAttributes::new(
        NodeMeta {
            file_uid: uid,
            ..Default::default()
        },
        vec![],
        None,
    )
    .unwrap()
}
fn directory(path: &str, version: u64) -> NodeRenameSubtreeEntry {
    NodeRenameSubtreeEntry {
        path: path.into(),
        destination: path.replacen("/src", "/dst", 1),
        node: ResolvedPath::Directory {
            id: Uuid::new_v4(),
            version,
            attributes: Some(attrs(7)),
        },
    }
}
fn request_for(root: &NodeRenameSubtreeEntry) -> NodeRenameRequest {
    let namespace = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let parent_version = NodeRenameRevision {
        namespace,
        inode: parent,
        version: 1,
    };
    let spec = NodeRenameSpec {
        pool: Uuid::new_v4(),
        operation: Uuid::new_v4(),
        from: "/src".into(),
        to: "/dst".into(),
        source: revision(namespace, &root.node),
        source_parent: parent_version,
        destination_parent: parent_version,
        target: None,
        no_replace: true,
    };
    let parent = NodeMutationParent {
        id: parent,
        version: 1,
        attributes: Some(attrs(1)),
    };
    NodeRenameRequest::from_baseline(
        &NodeRenameBaseline {
            library_serial: "SIM-PROJECTION".into(),
            owner_epoch: 1,
            request_epoch: 1,
            spec,
            source_parent: parent.clone(),
            destination_parent: parent,
            source: root.node.clone(),
            target: None,
        },
        "2026-11-02T10:00:00.000000000Z",
    )
    .unwrap()
}
fn file(
    request: &NodeRenameRequest,
    path: &str,
    volume: Uuid,
    metadata_uid: u64,
    data_uid: u64,
) -> NodeRenameSubtreeEntry {
    let alias = metadata_uid != data_uid;
    let copy = PhysicalCopy {
        version: 1,
        origin: if alias {
            CopyOrigin::Imported
        } else {
            CopyOrigin::FileRequest { request_epoch: 1 }
        },
        volume_id: volume,
        volume_generation: 2,
        volume_scan_id: Uuid::new_v4(),
        ltfs_file_uid: data_uid,
        size: 4,
        content_digest: ContentDigest::Sha256([3; 32]),
        media: alias.then(|| MediaProvenance {
            version: (1, 1),
            logical_file_uid: metadata_uid,
            physical_path: ".LTFSEE_DATA/object".into(),
            ee_alias: true,
            native_symlink: false,
        }),
    };
    NodeRenameSubtreeEntry {
        path: path.into(),
        destination: path.replacen("/src", "/dst", 1),
        node: ResolvedPath::File {
            id: Uuid::new_v4(),
            head: FileHead {
                version: 1,
                size: 4,
                copy_id: Uuid::new_v4(),
            },
            copy: Box::new(copy),
            placement: VolumePlacement {
                volume_id: volume,
                namespace: request.spec().source.namespace,
                pool_id: request.spec().pool,
                library_serial: request.library().into(),
                barcode: "PROJ0L8".into(),
            },
            attributes: Some(attrs(metadata_uid)),
            object_metadata: None,
        },
    }
}
fn summary(rows: &[NodeRenameSubtreeEntry]) -> NodeRenameSubtreeSummary {
    let directories = rows
        .iter()
        .filter(|r| matches!(r.node, ResolvedPath::Directory { .. }))
        .count() as u64;
    NodeRenameSubtreeSummary {
        entries: rows.len() as u64,
        files: rows.len() as u64 - directories,
        directories,
        encoded_bytes: rows
            .iter()
            .map(|r| r.encode().unwrap().len() as u64 + 8)
            .sum(),
        sha256: [4; 32],
        complete: true,
    }
}
fn built(
    request: &NodeRenameRequest,
    rows: &[NodeRenameSubtreeEntry],
) -> NodeRenameProjectionSpool {
    let mut spool = NodeRenameProjectionSpool::new(request, &summary(rows), 16 << 20).unwrap();
    assert!(spool.directories_page(None, 1000, 8 << 20).is_err());
    spool.append(rows).unwrap();
    spool.seal().unwrap();
    spool
}

#[test]
fn disk_projection_groups_exact_metadata_uids_and_preserves_unsigned_data_and_alias_identity() {
    let root = directory("/src", 3);
    let request = request_for(&root);
    let a = Uuid::from_bytes([1; 16]);
    let b = Uuid::from_bytes([2; 16]);
    let rows = vec![
        root,
        directory("/src/a", 1),
        directory("/src/a/x", 1),
        directory("/src/a-", 1),
        directory("/src/d", 1),
        file(&request, "/src/d/one", a, 22, 11),
        file(&request, "/src/d/two", a, 23, 11),
        file(&request, "/src/d/high", b, u64::MAX, u64::MAX),
        directory("/src/empty", 1),
    ];
    let spool = built(&request, &rows);
    assert_eq!(
        (
            spool.stats().nodes,
            spool.stats().files,
            spool.stats().directories
        ),
        (9, 3, 6)
    );
    assert!(spool.stats().database_bytes <= 16 << 20);
    let first = spool.volumes_page(None, 1).unwrap();
    assert_eq!(
        first.items,
        [NodeRenameProjectionVolume {
            volume: a,
            files: 2
        }]
    );
    let second = spool.volumes_page(first.next.as_deref(), 1).unwrap();
    assert_eq!(
        second.items,
        [NodeRenameProjectionVolume {
            volume: b,
            files: 1
        }]
    );
    assert!(second.next.is_none());
    assert_eq!(spool.metadata_node(a, 22).unwrap(), Some(rows[5].clone()));
    assert_eq!(spool.metadata_node(a, 23).unwrap(), Some(rows[6].clone()));
    assert!(
        spool.metadata_node(a, 11).unwrap().is_none(),
        "data UID must not identify either EE alias"
    );
    assert_eq!(
        spool.metadata_node(b, u64::MAX).unwrap(),
        Some(rows[7].clone())
    );
    assert!(spool.metadata_node(a, 0).is_err());
    let page = spool.volume_files_page(a, None, 1, 8 << 20).unwrap();
    assert_eq!(page.items, [rows[5].clone()]);
    let tail = spool
        .volume_files_page(a, page.next.as_deref(), 1, 8 << 20)
        .unwrap();
    assert_eq!(tail.items, [rows[6].clone()]);
    assert!(tail.next.is_none());
    assert!(
        spool
            .volume_files_page(b, page.next.as_deref(), 1, 8 << 20)
            .is_err()
    );
    assert!(
        spool
            .directories_page(page.next.as_deref(), 1, 8 << 20)
            .is_err()
    );
    assert!(
        spool
            .volume_files_page(Uuid::new_v4(), None, 1, 8 << 20)
            .is_err()
    );
    assert!(spool.volumes_page(None, 1001).is_err());
    assert!(spool.directories_page(None, 1, 1).is_err());
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = spool
            .directories_page(cursor.as_deref(), 1, 8 << 20)
            .unwrap();
        actual.extend(page.items);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let mut expected = rows
        .iter()
        .filter(|r| matches!(r.node, ResolvedPath::Directory { .. }))
        .cloned()
        .collect::<Vec<_>>();
    expected.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    assert_eq!(
        actual, expected,
        "SQL directory pages use complete bytewise path order"
    );
    assert_eq!(spool.directory("/src/a").unwrap(), Some(rows[1].clone()));
    assert!(spool.directory("/outside").is_err());
    assert!(
        spool.conn.execute("DELETE FROM nodes", []).is_err(),
        "sealed cache is query-only"
    );
}

#[test]
fn disk_projection_rejects_partial_parent_foreign_scope_and_duplicate_metadata_without_partial_rows()
 {
    let root = directory("/src", 3);
    let request = request_for(&root);
    let volume = Uuid::new_v4();
    let good = file(&request, "/src/one", volume, 22, 11);
    let duplicate = file(&request, "/src/two", volume, 22, 11);
    let mut foreign = good.clone();
    if let ResolvedPath::File { placement, .. } = &mut foreign.node {
        placement.namespace = Uuid::new_v4();
    }
    let child = directory("/src/missing/child", 1);
    for rows in [
        vec![root.clone(), good.clone(), duplicate],
        vec![root.clone(), foreign],
        vec![root.clone(), child],
        vec![root.clone(), good.clone(), directory("/src/one/child", 1)],
    ] {
        let mut spool =
            NodeRenameProjectionSpool::new(&request, &summary(&rows), 16 << 20).unwrap();
        assert!(spool.append(&rows).is_err());
        assert_eq!(spool.stats().nodes, 0);
        assert_eq!(
            spool
                .conn
                .query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            spool
                .conn
                .query_row("SELECT count(*) FROM volumes", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert!(spool.seal().is_err());
    }
    let mut rows = vec![root.clone()];
    rows.extend((0..200).map(|n| directory(&format!("/src/n{n:04}"), 1)));
    let mut small = NodeRenameProjectionSpool::new(&request, &summary(&rows), 64 << 10).unwrap();
    assert!(
        small.append(&rows).is_err(),
        "database page budget must refuse a large view"
    );
    assert_eq!(small.stats().nodes, 0);
    assert_eq!(
        small
            .conn
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    let empty = built(&request, std::slice::from_ref(&root));
    assert!(empty.volumes_page(None, 1000).unwrap().items.is_empty());
    assert_eq!(
        empty.directories_page(None, 1000, 8 << 20).unwrap().items,
        [root]
    );
}

#[test]
fn disk_projection_cursors_and_row_hashes_bind_the_original_view() {
    let root = directory("/src", 3);
    let request = request_for(&root);
    let rows = vec![root, directory("/src/z", 1)];
    let spool = built(&request, &rows);
    let page = spool.directories_page(None, 1, 8 << 20).unwrap();
    let other_request = request_for(&rows[0]);
    let other = built(&other_request, &rows);
    assert!(
        other
            .directories_page(page.next.as_deref(), 1, 8 << 20)
            .is_err()
    );
    spool.conn.pragma_update(None, "query_only", false).unwrap();
    let original = rows[0].encode().unwrap();
    let mut changed = original.clone();
    changed[0] ^= 0x01;
    spool
        .conn
        .execute("UPDATE nodes SET body=? WHERE path='/src'", [&changed])
        .unwrap();
    assert!(spool.directory("/src").is_err());
    assert!(spool.directories_page(None, 1000, 8 << 20).is_err());
    spool
        .conn
        .execute("UPDATE nodes SET body=? WHERE path='/src'", [&original])
        .unwrap();
    assert_eq!(spool.directory("/src").unwrap(), Some(rows[0].clone()));
}
