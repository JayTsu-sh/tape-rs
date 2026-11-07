use super::*;
use serde_json::json;

#[tokio::test]
#[ignore = "需要隔离FDB；损坏子树页失败不推进原游标，不访问介质"]
async fn subtree_capture_rejects_corrupt_edges_claims_and_orphans_without_advancing() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("RENAME-{}", Uuid::new_v4());
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
        name: "rename".into(),
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
    let source = Uuid::new_v4();
    let source_node = Node {
        kind: NodeKind::Directory,
        directory_version: Some(1),
        parent: root,
        name: b"source".to_vec(),
        attributes: Some(
            NodeAttributes::new(
                crate::ltfs::index::NodeMeta {
                    file_uid: 2,
                    ..Default::default()
                },
                vec![],
                None,
            )
            .unwrap(),
        ),
    };
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&node_key(ns, source), &encode_node(&source_node));
    tr.set(&child_key(ns, root, b"source"), source.as_bytes());
    tr.commit().await.unwrap();
    let revision = |inode| NodeRenameRevision {
        namespace: ns,
        inode,
        version: 1,
    };
    let spec = NodeRenameSpec {
        pool: pool.id,
        operation: Uuid::new_v4(),
        from: "/source".into(),
        to: "/destination".into(),
        source: revision(source),
        source_parent: revision(root),
        destination_parent: revision(root),
        target: None,
        no_replace: true,
    };
    let request = catalog
        .prepare_node_rename_request(&serial, route, 100, &spec, "2026-11-02T10:00:00.000000000Z")
        .await
        .unwrap();
    let child = Uuid::new_v4();
    let grand = Uuid::new_v4();
    let child_node = Node {
        parent: source,
        name: b"child".to_vec(),
        ..source_node.clone()
    };
    let grand_node = Node {
        parent: child,
        name: b"grand".to_vec(),
        ..source_node.clone()
    };
    let tr = catalog.db.create_trx().unwrap();
    for (id, node) in [(child, &child_node), (grand, &grand_node)] {
        tr.set(&node_key(ns, id), &encode_node(node));
        tr.set(&child_key(ns, node.parent, &node.name), id.as_bytes());
    }
    for offset in 0..1001 {
        let id = Uuid::new_v4();
        let node = Node {
            parent: source,
            name: format!("z{offset:04}").into_bytes(),
            ..source_node.clone()
        };
        tr.set(&node_key(ns, id), &encode_node(&node));
        tr.set(&child_key(ns, source, &node.name), id.as_bytes());
    }
    tr.commit().await.unwrap();
    catalog
        .queue_node_rename_request(route, 100, &request, 1 << 20)
        .await
        .unwrap();
    catalog
        .prepare_node_rename_task(&serial, route, 100, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    let inputs = catalog
        .capture_prepared_node_rename_inputs(&serial, route, 100, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    let mut cursor = catalog
        .begin_node_rename_subtree_capture(route, 100, &inputs)
        .await
        .unwrap();
    let original = cursor.summary();
    let first = catalog
        .next_node_rename_subtree_page(route, 100, &mut cursor, 1, 1 << 20)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 1);
    let after_root = cursor.summary();
    let mut partial = catalog
        .begin_node_rename_subtree_capture(route, 100, &inputs)
        .await
        .unwrap();
    let root_bytes = first.entries[0].encode().unwrap().len() + 8;
    assert!(
        catalog
            .next_node_rename_subtree_page(route, 100, &mut partial, 3, root_bytes + 1)
            .await
            .is_err()
    );
    assert_eq!(
        partial.summary(),
        original,
        "failure after an accepted row rolls back the whole page"
    );
    let child_edge = child_key(ns, source, b"child");
    let child_key = node_key(ns, child);
    let mut bad_parent = child_node.clone();
    bad_parent.parent = root;
    let mut no_attributes = child_node.clone();
    no_attributes.attributes = None;
    let mut copies = copy_prefix(ns);
    copies.extend_from_slice(child.as_bytes());
    copies.extend_from_slice(Uuid::new_v4().as_bytes());
    let path_claim = active_batch::path_claim_key_for(ns, child, b"pending");
    let reserved = reservation::path_key_for(ns, child, b"pending");
    for (key, value, restore) in [
        (
            child_edge.clone(),
            vec![1, 2, 3],
            Some(child.as_bytes().to_vec()),
        ),
        (
            child_edge.clone(),
            source.as_bytes().to_vec(),
            Some(child.as_bytes().to_vec()),
        ),
        (
            child_key.clone(),
            encode_node(&bad_parent),
            Some(encode_node(&child_node)),
        ),
        (
            child_key.clone(),
            encode_node(&no_attributes),
            Some(encode_node(&child_node)),
        ),
        (head_key(ns, child), b"orphan".to_vec(), None),
        (copies, b"orphan".to_vec(), None),
        (path_claim, b"pending".to_vec(), None),
        (reserved, b"pending".to_vec(), None),
    ] {
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&key, &value);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .next_node_rename_subtree_page(route, 100, &mut cursor, 2, 1 << 20)
                .await
                .is_err()
        );
        assert_eq!(cursor.summary(), after_root);
        let tr = catalog.db.create_trx().unwrap();
        match restore {
            Some(bytes) => tr.set(&key, &bytes),
            None => tr.clear(&key),
        }
        tr.commit().await.unwrap();
    }
    while !cursor.summary().complete {
        catalog
            .next_node_rename_subtree_page(route, 100, &mut cursor, 1000, 8 << 20)
            .await
            .unwrap();
    }
    assert_eq!(cursor.summary().entries, 1004);
    let mut all = catalog
        .begin_node_rename_subtree_capture(route, 100, &inputs)
        .await
        .unwrap();
    while !all.summary().complete {
        let page = catalog
            .next_node_rename_subtree_page(route, 100, &mut all, 64, 1 << 20)
            .await
            .unwrap();
        assert!(page.entries.len() <= 64);
    }
    assert_eq!(all.summary(), cursor.summary());
    catalog
        .cancel_node_rename_task(&serial, route, 100, 1, spec.operation)
        .await
        .unwrap();
    assert!(
        catalog
            .next_node_rename_subtree_page(route, 100, &mut cursor, 1, 1 << 20)
            .await
            .is_err()
    );
}
