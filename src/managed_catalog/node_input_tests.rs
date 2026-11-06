use super::*;
use crate::ltfs::index::{NodeMeta, Xattr};
use serde_json::json;

const TIME: &str = "2026-10-31T08:00:00.000000000Z";

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；验证规范祖先同事务重核，不访问介质"]
async fn canonical_chain_revalidation_detects_ancestor_changes_and_conflicts() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let serial = format!("SIM-NODE-INPUTS-{namespace}");
    let challenge = Uuid::new_v4();
    let replies = (1..=3).map(|node| json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let route = catalog
        .initialize_native_library(
            namespace,
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
        name: "inputs".into(),
        file_limit: 200_000,
        created_at_millis: 1_798_000_000_000,
        policy: PoolPolicy::default(),
        revision: 1,
    };
    catalog
        .create_active_pool(&serial, route, 100, &pool)
        .await
        .unwrap();
    let root = active_pools::root_id(namespace, pool.id);
    let grand = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let attributes = NodeAttributes::new(
        NodeMeta {
            file_uid: 7,
            creation_time: TIME.into(),
            change_time: TIME.into(),
            modify_time: TIME.into(),
            access_time: TIME.into(),
            backup_time: TIME.into(),
            readonly: false,
        },
        vec![Xattr {
            key: "unknown".into(),
            value: "original".into(),
            base64: false,
        }],
        None,
    )
    .unwrap();
    let grand_node = Node {
        kind: NodeKind::Directory,
        parent: root,
        name: b"grand".to_vec(),
        directory_version: Some(7),
        attributes: Some(attributes.clone()),
    };
    let parent_node = Node {
        kind: NodeKind::Directory,
        parent: grand,
        name: b"parent".to_vec(),
        directory_version: Some(8),
        attributes: Some(attributes.clone()),
    };
    let tr = catalog.db.create_trx().unwrap();
    for (id, node) in [(grand, &grand_node), (parent, &parent_node)] {
        tr.set(&node_key(namespace, id), &encode_node(node));
        tr.set(
            &child_key(namespace, node.parent, &node.name),
            id.as_bytes(),
        );
    }
    tr.commit().await.unwrap();
    let spec = NodeMutationSpec {
        pool: pool.id,
        operation: Uuid::new_v4(),
        path: "/grand/parent/new".into(),
        expected_version: None,
        mutation: NodeMutation::CreateDirectory,
    };
    let request = catalog
        .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
        .await
        .unwrap();
    catalog
        .queue_node_mutation_request(route, 100, &request, 1 << 20)
        .await
        .unwrap();
    let task = catalog
        .prepare_node_mutation_task(&serial, route, 100, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    let inputs = catalog
        .capture_prepared_node_inputs(&serial, route, 100, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    catalog
        .check_prepared_node_inputs(route, 100, &inputs)
        .await
        .unwrap();
    assert_eq!(inputs.ancestors.len(), 3);

    // The request's original baseline binds the direct parent, not every
    // ancestor. The final canonical check must independently bind all C/N.
    let mut changed = grand_node.clone();
    let mut meta = attributes.meta().unwrap().clone();
    meta.readonly = true;
    changed.attributes =
        Some(NodeAttributes::new(meta, attributes.xattrs().unwrap().to_vec(), None).unwrap());
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&node_key(namespace, grand), &encode_node(&changed));
    tr.commit().await.unwrap();
    catalog
        .check_node_mutation_request(route, 100, &request)
        .await
        .unwrap();
    assert!(
        catalog
            .check_prepared_node_inputs(route, 100, &inputs)
            .await
            .is_err(),
        "unchanged Prepared status is insufficient when an earlier ancestor changed"
    );
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&node_key(namespace, grand), &encode_node(&grand_node));
    tr.commit().await.unwrap();

    for fault in [false, true] {
        let key = if fault {
            head_key(namespace, grand)
        } else {
            child_key(namespace, root, b"grand")
        };
        let tr = catalog.db.create_trx().unwrap();
        if fault {
            tr.set(&key, b"orphan-file-head");
        } else {
            tr.clear(&key);
        }
        tr.commit().await.unwrap();
        assert!(
            catalog
                .check_prepared_node_inputs(route, 100, &inputs)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        if fault {
            // A failed validator must not repair or delete the bad row.
            assert_eq!(
                tr.get(&key, false).await.unwrap().as_deref(),
                Some(&b"orphan-file-head"[..])
            );
            tr.clear(&key);
        } else {
            assert!(tr.get(&key, false).await.unwrap().is_none());
            tr.set(&key, grand.as_bytes());
        }
        tr.commit().await.unwrap();
    }

    // These non-snapshot reads must conflict with changes before admission's
    // first mutation. An error cannot release the original task's pool hold.
    let pinned = catalog.db.create_trx().unwrap();
    require_inputs_at(&pinned, route, 100, &inputs)
        .await
        .unwrap();
    changed = grand_node.clone();
    changed.directory_version = Some(9);
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&node_key(namespace, grand), &encode_node(&changed));
    tr.commit().await.unwrap();
    pinned.set(&management::prefix(namespace, b'x'), b"must-not-commit");
    assert_eq!(pinned.commit().await.unwrap_err().code(), 1020);
    let tr = catalog.db.create_trx().unwrap();
    assert!(
        tr.get(&management::prefix(namespace, b'x'), false)
            .await
            .unwrap()
            .is_none()
    );
    tr.set(&node_key(namespace, grand), &encode_node(&grand_node));
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .node_mutation_task(&serial, route, 1, spec.operation)
            .await
            .unwrap(),
        Some(task.clone())
    );

    for bad in 0..5 {
        let mut corrupted = inputs.clone();
        match bad {
            0 => {
                corrupted.ancestors.remove(1);
            }
            1 => corrupted.ancestors.swap(0, 1),
            2 => corrupted.ancestors[1].id = parent,
            3 => corrupted.ancestors[1].version += 1,
            _ => corrupted.baseline.owner_epoch = 101,
        }
        assert!(
            catalog
                .check_prepared_node_inputs(route, 100, &corrupted)
                .await
                .is_err()
        );
    }
    catalog
        .claim_active_owner_epoch(&serial, 101)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .check_prepared_node_inputs(route, 100, &inputs)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert!(
        catalog
            .check_prepared_node_inputs(route, 101, &inputs)
            .await
            .is_err()
    );
    let resumed = catalog
        .capture_prepared_node_inputs(&serial, route, 101, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    catalog
        .check_prepared_node_inputs(route, 101, &resumed)
        .await
        .unwrap();
    assert_eq!(resumed.task, task);
    assert_eq!(resumed.request, request);
    catalog
        .cancel_node_mutation_task(&serial, route, 101, 1, spec.operation)
        .await
        .unwrap();
    assert!(
        catalog
            .check_prepared_node_inputs(route, 101, &resumed)
            .await
            .is_err()
    );
}
