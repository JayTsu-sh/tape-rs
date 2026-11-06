use super::*;
use serde_json::json;

#[tokio::test]
#[ignore = "需要隔离FDB；可信清单原页持久化/中断恢复/腐败拒绝/接任，不访问介质"]
async fn canonical_manifest_resumes_exact_pages_without_trusting_uploaded_objects_or_repairing_corruption()
 {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("RENAME-{}", Uuid::new_v4());
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|node|json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
    for offset in 0..70 {
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
    let mut capture = catalog
        .resume_node_rename_manifest_capture(route, 100, &inputs)
        .await
        .unwrap();
    assert_eq!(capture.pages(), 0);
    assert!(
        catalog
            .finish_node_rename_manifest_capture(route, 100, &capture)
            .await
            .is_err()
    );
    let mut unknown = NodeRenameManifestCapture {
        inputs: capture.inputs.clone(),
        cursor: capture.cursor.clone(),
        header: capture.header.clone(),
    };
    let first = catalog
        .next_node_rename_manifest_page(route, 100, &mut capture)
        .await
        .unwrap();
    assert_eq!(first.entries, 64);
    assert!(!first.complete);
    assert_eq!(capture.pages(), 1);
    let original_header = node_mutation::canonical(&capture.header).unwrap();
    assert_eq!(
        catalog
            .next_node_rename_manifest_page(route, 100, &mut unknown)
            .await
            .unwrap(),
        first,
        "unknown original commit returns the same persisted page, not a second page"
    );
    assert_eq!(unknown.header, capture.header);
    let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
    let resumed = reopened
        .resume_node_rename_manifest_capture(route, 100, &inputs)
        .await
        .unwrap();
    assert_eq!(resumed.header, capture.header);
    drop(resumed);
    let tr = catalog.db.create_trx().unwrap();
    let original_page = tr
        .get(&page_key(&inputs.task, 0), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    drop(tr);
    let mut bad_header = capture.header.clone();
    bad_header.version = 2;
    let mut bad_page: StoredPage = decode(&original_page).unwrap();
    bad_page.limit = 0;
    let mut foreign_page: StoredPage = decode(&original_page).unwrap();
    let inert =
        NodePlanObject::for_bytes(&serial, ns, b"arbitrary caller upload", PAGE_BYTES).unwrap();
    catalog
        .upload_node_plan_object(route, 100, &inert, &mut &b"arbitrary caller upload"[..])
        .await
        .unwrap();
    foreign_page.object = URL_SAFE_NO_PAD.encode(inert.encode());
    let marker = header_key(&inputs.task);
    let page0 = page_key(&inputs.task, 0);
    let extra = page_key(&inputs.task, 8);
    for (key, broken, restore) in [
        (marker.clone(), None, Some(original_header.clone())),
        (
            marker.clone(),
            Some(node_mutation::canonical(&bad_header).unwrap()),
            Some(original_header.clone()),
        ),
        (page0.clone(), None, Some(original_page.clone())),
        (
            page0.clone(),
            Some(node_mutation::canonical(&bad_page).unwrap()),
            Some(original_page.clone()),
        ),
        (
            page0.clone(),
            Some(node_mutation::canonical(&foreign_page).unwrap()),
            Some(original_page.clone()),
        ),
        (extra, Some(original_page.clone()), None),
    ] {
        let tr = catalog.db.create_trx().unwrap();
        match &broken {
            Some(raw) => tr.set(&key, raw),
            None => tr.clear(&key),
        };
        tr.commit().await.unwrap();
        assert!(
            reopened
                .resume_node_rename_manifest_capture(route, 100, &inputs)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&key, false).await.unwrap().map(|v| v.to_vec()),
            broken
        );
        match restore {
            Some(raw) => tr.set(&key, &raw),
            None => tr.clear(&key),
        };
        tr.commit().await.unwrap();
    }
    catalog
        .claim_active_owner_epoch(&serial, 101)
        .await
        .unwrap();
    assert!(
        catalog
            .next_node_rename_manifest_page(route, 100, &mut capture)
            .await
            .is_err()
    );
    let inputs = reopened
        .capture_prepared_node_rename_inputs(&serial, route, 101, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    let mut takeover = reopened
        .resume_node_rename_manifest_capture(route, 101, &inputs)
        .await
        .unwrap();
    assert_eq!(takeover.header, capture.header);
    assert_eq!(takeover.header.creator_owner, 100);
    while !takeover.summary().complete {
        reopened
            .next_node_rename_manifest_page(route, 101, &mut takeover)
            .await
            .unwrap();
    }
    let manifest = reopened
        .finish_node_rename_manifest_capture(route, 101, &takeover)
        .await
        .unwrap();
    assert_eq!(manifest.summary().entries, 73);
    assert_eq!(manifest.pages(), 2);
    assert_eq!(manifest.creator_owner_epoch(), 100);
    let sealed = reopened
        .resume_node_rename_manifest_capture(route, 101, &inputs)
        .await
        .unwrap();
    assert_eq!(
        sealed.header, takeover.header,
        "replay must retain the final page's original requested limit"
    );
    let mut reader = reopened
        .open_node_rename_manifest_reader(route, 101, &manifest)
        .await
        .unwrap();
    let mut total = 0;
    while let Some(entries) = reopened
        .next_node_rename_manifest_entries(route, 101, &mut reader)
        .await
        .unwrap()
    {
        total += entries.len();
        for row in entries {
            assert_eq!(
                NodeRenameSubtreeEntry::decode(&row.encode().unwrap()).unwrap(),
                row
            );
        }
    }
    assert!(reader.complete());
    assert_eq!(reader.verified_pages(), 2);
    assert_eq!(total, 73);
    let spool = reopened
        .build_node_rename_projection_spool(route, 101, &manifest, 16 << 20)
        .await
        .unwrap();
    assert_eq!(
        (
            spool.stats().nodes,
            spool.stats().directories,
            spool.stats().files
        ),
        (73, 73, 0)
    );
    assert!(spool.volumes_page(None, 1000).unwrap().items.is_empty());
    assert_eq!(
        spool
            .directories_page(None, 1000, 8 << 20)
            .unwrap()
            .items
            .len(),
        73
    );
    fn require_send<T: Send>(_: T) {}
    require_send(reopened.build_node_rename_projection_spool(route, 101, &manifest, 16 << 20));

    let good: StoredPage = decode(&original_page).unwrap();
    let reference = object(&good, &inputs.task).unwrap();
    // Explicit corruption of the existing object wire format, not a product bypass.
    let mut object_prefix = key(b'@');
    object_prefix.extend_from_slice(ns.as_bytes());
    object_prefix.push(b'b');
    object_prefix.extend_from_slice(&(serial.len() as u16).to_be_bytes());
    object_prefix.extend_from_slice(serial.as_bytes());
    object_prefix.extend_from_slice(&reference.digest());
    object_prefix.extend_from_slice(&reference.byte_len().to_be_bytes());
    let mut chunk = object_prefix.clone();
    chunk.push(1);
    chunk.extend_from_slice(&0u16.to_be_bytes());
    let mut object_marker = object_prefix;
    object_marker.push(0);
    for corrupt_key in [chunk, object_marker] {
        let tr = catalog.db.create_trx().unwrap();
        let raw = tr.get(&corrupt_key, false).await.unwrap().unwrap().to_vec();
        let mut changed = raw.clone();
        changed[0] ^= 0x80;
        tr.set(&corrupt_key, &changed);
        tr.commit().await.unwrap();
        let mut bad_reader = reopened
            .open_node_rename_manifest_reader(route, 101, &manifest)
            .await
            .unwrap();
        assert!(
            reopened
                .next_node_rename_manifest_entries(route, 101, &mut bad_reader)
                .await
                .is_err()
        );
        assert_eq!(bad_reader.verified_pages(), 0);
        assert!(!bad_reader.complete());
        assert!(
            reopened
                .resume_node_rename_manifest_capture(route, 101, &inputs)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&corrupt_key, false).await.unwrap().unwrap().to_vec(),
            changed
        );
        tr.set(&corrupt_key, &raw);
        tr.commit().await.unwrap();
    }
    catalog
        .cancel_node_rename_task(&serial, route, 101, 1, spec.operation)
        .await
        .unwrap();
    assert!(
        reopened
            .resume_node_rename_manifest_capture(route, 101, &inputs)
            .await
            .is_err()
    );
    assert!(
        reopened
            .next_node_rename_manifest_entries(route, 101, &mut reader)
            .await
            .is_err()
    );
}
