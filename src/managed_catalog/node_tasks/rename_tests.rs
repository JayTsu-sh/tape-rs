use super::*;
use serde_json::json;

#[tokio::test]
#[ignore = "需要隔离FDB；rename与原节点共用任务/池占用/接任，无介质操作"]
async fn rename_tasks_keep_original_kind_and_hold_across_retries_corruption_and_takeover() {
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
    let other_pool = PoolRecord {
        id: Uuid::new_v4(),
        name: "other".into(),
        ..pool.clone()
    };
    catalog
        .create_active_pool(&serial, route, 100, &other_pool)
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
    let queued = catalog
        .queue_node_rename_request(route, 100, &request, 1 << 20)
        .await
        .unwrap();
    assert_eq!(queued.kind, NodeMutationTaskKind::Rename);
    assert_eq!(
        catalog
            .queue_node_rename_request(route, 100, &request, 1 << 20)
            .await
            .unwrap(),
        queued
    );
    assert!(
        catalog
            .node_mutation_task_request(&serial, route, 100, 1, spec.operation, 1 << 20)
            .await
            .is_err()
    );
    assert!(
        catalog
            .cancel_node_mutation_task(&serial, route, 100, 1, spec.operation)
            .await
            .is_err()
    );
    let single = catalog
        .prepare_node_mutation_request(
            &serial,
            route,
            100,
            &NodeMutationSpec {
                pool: pool.id,
                operation: spec.operation,
                path: "/single".into(),
                expected_version: None,
                mutation: NodeMutation::CreateDirectory,
            },
            "2026-11-02T10:00:00.000000000Z",
        )
        .await
        .unwrap();
    assert!(
        catalog
            .queue_node_mutation_request(route, 100, &single, 1 << 20)
            .await
            .is_err(),
        "same original identity cannot change protocol"
    );
    let mut changed = spec.clone();
    changed.to = "/changed".into();
    let changed = catalog
        .prepare_node_rename_request(&serial, route, 100, &changed, request.time())
        .await
        .unwrap();
    assert!(
        catalog
            .queue_node_rename_request(route, 100, &changed, 1 << 20)
            .await
            .is_err()
    );
    let prepared = catalog
        .prepare_node_rename_task(&serial, route, 100, 1, spec.operation, 1 << 20)
        .await
        .unwrap();
    assert_eq!(prepared.state, NodeMutationTaskState::Prepared);
    assert_eq!(
        catalog
            .prepare_node_rename_task(&serial, route, 100, 1, spec.operation, 1 << 20)
            .await
            .unwrap(),
        prepared
    );
    let tr = catalog.db.create_trx().unwrap();
    assert!(requests::has_namespace_holds(&tr, ns).await.unwrap());
    drop(tr);
    assert!(matches!(
        catalog
            .check_active_pool_gate(&serial, route, 100, pool.id)
            .await,
        Err(CatalogError::PoolBusy)
    ));
    catalog
        .check_active_pool_gate(&serial, route, 100, other_pool.id)
        .await
        .unwrap();
    let other_spec = NodeMutationSpec {
        operation: Uuid::new_v4(),
        ..single.spec()
    };
    let other = catalog
        .prepare_node_mutation_request(&serial, route, 100, &other_spec, request.time())
        .await
        .unwrap();
    catalog
        .queue_node_mutation_request(route, 100, &other, 1 << 20)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .prepare_node_mutation_task(&serial, route, 100, 1, other_spec.operation, 1 << 20)
            .await,
        Err(CatalogError::PoolBusy)
    ));
    assert!(
        catalog
            .node_rename_task_request(&serial, route, 100, 1, other_spec.operation, 1 << 20)
            .await
            .is_err()
    );
    let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
    catalog
        .claim_active_owner_epoch(&serial, 101)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .cancel_node_rename_task(&serial, route, 100, 1, spec.operation)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert_eq!(
        reopened
            .prepare_node_rename_task(&serial, route, 101, 1, spec.operation, 1 << 20)
            .await
            .unwrap(),
        prepared
    );
    assert_eq!(
        reopened
            .node_rename_task_request(&serial, route, 101, 1, spec.operation, 1 << 20)
            .await
            .unwrap(),
        (prepared.clone(), request.clone())
    );
    let task_key = identity_key(ns, &serial, b'o', 1, spec.operation);
    let hold = hold_key(ns, pool.id);
    let tr = catalog.db.create_trx().unwrap();
    let task_bytes = tr.get(&task_key, false).await.unwrap().unwrap().to_vec();
    let hold_bytes = tr.get(&hold, false).await.unwrap().unwrap().to_vec();
    drop(tr);
    for key in [
        identity_key(ns, &serial, b'q', 1, spec.operation),
        global_pending_key(ns, &serial, 1, spec.operation),
        hold.clone(),
    ] {
        let tr = catalog.db.create_trx().unwrap();
        let bytes = tr.get(&key, false).await.unwrap().unwrap().to_vec();
        tr.clear(&key);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .cancel_node_rename_task(&serial, route, 101, 1, spec.operation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&task_key, false).await.unwrap().unwrap().as_ref(),
            task_bytes
        );
        assert!(tr.get(&key, false).await.unwrap().is_none());
        tr.set(&key, &bytes);
        tr.commit().await.unwrap();
    }
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&hold, b"corrupt");
    tr.commit().await.unwrap();
    assert!(matches!(
        catalog
            .cancel_node_rename_task(&serial, route, 101, 1, spec.operation)
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&hold, false).await.unwrap().unwrap().as_ref(),
        b"corrupt"
    );
    assert_eq!(
        tr.get(&task_key, false).await.unwrap().unwrap().as_ref(),
        task_bytes
    );
    tr.set(&hold, &hold_bytes);
    tr.commit().await.unwrap();
    let cancelled = catalog
        .cancel_node_rename_task(&serial, route, 101, 1, spec.operation)
        .await
        .unwrap();
    assert_eq!(cancelled.state, NodeMutationTaskState::Cancelled);
    catalog
        .prepare_node_mutation_task(&serial, route, 101, 1, other_spec.operation, 1 << 20)
        .await
        .unwrap();
    let tr = catalog.db.create_trx().unwrap();
    let other_hold = tr.get(&hold, false).await.unwrap().unwrap().to_vec();
    drop(tr);
    let mut refusal_spec = spec.clone();
    refusal_spec.operation = Uuid::new_v4();
    refusal_spec.to = "/refused".into();
    let refusal = catalog
        .prepare_node_rename_request(&serial, route, 101, &refusal_spec, request.time())
        .await
        .unwrap();
    let refused_queued = catalog
        .queue_node_rename_request(route, 101, &refusal, 1 << 20)
        .await
        .unwrap();
    assert!(
        catalog
            .reject_node_rename_task(
                &serial,
                route,
                101,
                1,
                refusal_spec.operation,
                "bad\nreason"
            )
            .await
            .is_err()
    );
    assert_eq!(
        catalog
            .node_mutation_task(&serial, route, 1, refusal_spec.operation)
            .await
            .unwrap()
            .unwrap(),
        refused_queued
    );
    let rejected = catalog
        .reject_node_rename_task(
            &serial,
            route,
            101,
            1,
            refusal_spec.operation,
            "原 rename 已拒绝",
        )
        .await
        .unwrap();
    assert_eq!(rejected.state, NodeMutationTaskState::Rejected);
    assert_eq!(
        rejected.rejection_reason.as_deref(),
        Some("原 rename 已拒绝")
    );
    assert_eq!(
        catalog
            .reject_node_rename_task(&serial, route, 101, 1, refusal_spec.operation, "后来原因")
            .await
            .unwrap(),
        rejected
    );
    let tr = catalog.db.create_trx().unwrap();
    tr.clear(&child_key(ns, root, b"source"));
    tr.clear(&node_key(ns, source));
    tr.commit().await.unwrap();
    assert!(
        catalog
            .check_node_rename_request(route, 101, &request)
            .await
            .is_err()
    );
    assert_eq!(
        catalog
            .queue_node_rename_request(route, 101, &request, 1 << 20)
            .await
            .unwrap(),
        cancelled
    );
    assert_eq!(
        catalog
            .prepare_node_rename_task(&serial, route, 101, 1, spec.operation, 1 << 20)
            .await
            .unwrap(),
        cancelled
    );
    assert_eq!(
        catalog
            .cancel_node_rename_task(&serial, route, 101, 1, spec.operation)
            .await
            .unwrap(),
        cancelled
    );
    assert_eq!(
        catalog
            .queue_node_rename_request(route, 101, &refusal, 1 << 20)
            .await
            .unwrap(),
        rejected
    );
    assert_eq!(
        catalog
            .cancel_node_rename_task(&serial, route, 101, 1, refusal_spec.operation)
            .await
            .unwrap(),
        rejected
    );
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&hold, false).await.unwrap().unwrap().as_ref(),
        other_hold
    );
    drop(tr);
    catalog
        .cancel_node_mutation_task(&serial, route, 101, 1, other_spec.operation)
        .await
        .unwrap();
}

#[test]
fn rename_task_kind_is_required_and_writing_needs_an_active_group_and_completion_digest() {
    let ns = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let serial = "RENAME-CODEC";
    let mut task = NodeMutationTask {
        kind: NodeMutationTaskKind::Rename,
        library_serial: serial.into(),
        namespace: ns,
        pool: Uuid::new_v4(),
        request_epoch: 1,
        operation,
        object: NodePlanObject::for_bytes(serial, ns, b"inert", 100).unwrap(),
        fingerprint: [7; 32],
        state: NodeMutationTaskState::Queued,
        accepted_owner_epoch: 100,
        created_at_millis: 1_798_000_000_000,
        prepared_owner_epoch: None,
        prepared_at_millis: None,
        active_attempt: None,
        completed_owner_epoch: None,
        completed_at_millis: None,
        publication_digest: None,
        rejection_reason: None,
    };
    let bytes = encode(&task).unwrap();
    assert_eq!(decode(&bytes, serial, ns, 1, operation).unwrap(), task);
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value.as_object_mut().unwrap().remove("kind");
    assert!(decode(value.to_string().as_bytes(), serial, ns, 1, operation).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["kind"] = json!("unknown");
    assert!(decode(value.to_string().as_bytes(), serial, ns, 1, operation).is_err());
    task.state = NodeMutationTaskState::Prepared;
    task.prepared_owner_epoch = Some(100);
    task.prepared_at_millis = Some(task.created_at_millis);
    let tr_guard = Uuid::new_v4();
    assert!(encode(&task).is_ok());
    task.state = NodeMutationTaskState::Writing;
    assert!(encode(&task).is_err());
    task.active_attempt = Some(tr_guard);
    assert_eq!(
        decode(&encode(&task).unwrap(), serial, ns, 1, operation).unwrap(),
        task
    );
    task.state = NodeMutationTaskState::Completed;
    task.active_attempt = None;
    task.completed_owner_epoch = Some(100);
    task.completed_at_millis = Some(task.created_at_millis);
    task.publication_digest = Some([1; 32]);
    assert_eq!(
        decode(&encode(&task).unwrap(), serial, ns, 1, operation).unwrap(),
        task
    );
    task.publication_digest = None;
    assert!(encode(&task).is_err());
}
