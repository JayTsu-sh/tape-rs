use super::*;

fn request(ns: Uuid, serial: &str, change: Value) -> ManagementRequest {
    ManagementRequest::parse(
        json!({"version":1,"namespace":ns.to_string(),"library":serial,
        "epoch":1,"operation":Uuid::new_v4().to_string(),"change":change}),
    )
    .unwrap()
}
async fn create(
    c: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    name: &str,
) -> PoolRecord {
    let pool = PoolRecord {
        id: Uuid::new_v4(),
        library_serial: serial.into(),
        name: name.into(),
        file_limit: 100,
        created_at_millis: 1_793_405_000_123,
        revision: 1,
        policy: PoolPolicy::default(),
    };
    assert_eq!(
        c.create_active_pool(serial, route, owner, &pool)
            .await
            .unwrap(),
        ActivePoolCreation::Created(pool.clone())
    );
    pool
}
async fn submit(
    c: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    change: Value,
) -> ManagementRequest {
    let r = request(route.route.namespace, serial, change);
    c.admit_management(serial, route, owner, &r).await.unwrap();
    r
}
async fn apply(
    c: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    r: &ManagementRequest,
) -> Value {
    c.apply_pool_management(serial, route, owner, r.epoch(), r.operation())
        .await
        .unwrap()
}
async fn rows(c: &ManagedCatalog, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
    let tr = c.db.create_trx().unwrap();
    let mut values = Vec::new();
    for key in keys {
        values.push(tr.get(key, false).await.unwrap().map(|v| v.to_vec()));
    }
    values
}
fn task_keys(ns: Uuid, serial: &str, r: &ManagementRequest) -> Vec<Vec<u8>> {
    let mut head = key(b'~');
    head.extend_from_slice(ns.as_bytes());
    append_component(&mut head, serial.as_bytes());
    head.push(b'h');
    vec![
        identity_key(ns, serial, b'o', r.epoch(), r.operation()),
        identity_key(ns, serial, b'q', r.epoch(), r.operation()),
        head,
    ]
}
fn pool_keys(ns: Uuid, p: &PoolRecord) -> Vec<Vec<u8>> {
    vec![
        pool_key(ns, p.id),
        pool_name_key(ns, &p.name),
        node_key(ns, active_pools::root_id(ns, p.id)),
        child_key(ns, Uuid::nil(), p.id.to_string().as_bytes()),
        retired_pool_key(ns, p.id),
    ]
}

#[test]
fn pool_json_validates_public_records_and_preserves_creation_timestamp() {
    let mut p = PoolRecord {
        id: Uuid::new_v4(),
        library_serial: "LIB".into(),
        name: "pool".into(),
        file_limit: 100,
        created_at_millis: 1_793_405_000_123,
        revision: 1,
        policy: PoolPolicy::default(),
    };
    let v = p.to_json().unwrap();
    assert_eq!(v["created_at_millis"], p.created_at_millis);
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(v["created_at"].as_str().unwrap())
            .unwrap()
            .timestamp_millis() as u64,
        p.created_at_millis
    );
    p.created_at_millis = u64::MAX;
    assert!(p.to_json().is_err());
}

#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；只验证原子池元数据/任务通知、冲突和损坏，不访问设备"]
async fn pool_management_applies_atomically_fences_owners_and_preserves_corruption() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("POOL-APPLY-{ns}");
    let challenge = Uuid::new_v4();
    let replies:Vec<_>=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,
        "term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},
        "members":{"1":"127.0.0.1:8001","2":"127.0.0.1:8002","3":"127.0.0.1:8003"},
        "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,
        "catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
    let route = c
        .initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &replies).unwrap(),
        )
        .await
        .unwrap();
    let owner = route.initial_owner_epoch;
    let pool = create(&c, &serial, route, owner, "original").await;
    let root = active_pools::root_id(ns, pool.id);
    let original_root = rows(&c, &[node_key(ns, root)]).await;
    let consumer = c
        .create_notify_consumer(
            &serial,
            route,
            "tasks",
            &NotifyFilter {
                pool: Some(pool.id.to_string()),
                event_types: vec!["task.status.changed".into()],
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    let generation = Uuid::parse_str(consumer["generation"].as_str().unwrap()).unwrap();
    let first=submit(&c,&serial,route,owner,json!({"kind":"pool","id":pool.id.to_string(),"revision":1,
        "patch":{"name":"renamed","file_limit":222,"media_restriction":"L8","mount_limit":7,
        "low_space_warning_enabled":false,"low_space_warning_threshold":30,"no_space_warning_enabled":false}})).await;
    let applied = apply(&c, &serial, route, owner, &first).await;
    assert_eq!(applied["status"], "succeeded");
    let updated = c
        .active_pool_for_route(&serial, route, pool.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.created_at_millis, pool.created_at_millis);
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.file_limit, 222);
    assert_eq!(
        updated.policy,
        PoolPolicy {
            media_restriction: "L8".into(),
            mount_limit: 7,
            low_space_warning_enabled: false,
            low_space_warning_threshold: 30,
            no_space_warning_enabled: false
        }
    );
    assert_eq!(applied["result"]["pool"], updated.to_json().unwrap());
    assert_eq!(rows(&c, &[node_key(ns, root)]).await, original_root);
    assert_eq!(rows(&c, &[pool_name_key(ns, &pool.name)]).await, vec![None]);
    assert_eq!(apply(&c, &serial, route, owner, &first).await, applied);
    assert_eq!(
        c.admit_management(&serial, route, owner, &first)
            .await
            .unwrap(),
        applied
    );
    let batch = c
        .notify_batch(&serial, route, "tasks", generation)
        .await
        .unwrap();
    assert_eq!(batch["through"], 2);
    assert_eq!(batch["events"].as_array().unwrap().len(), 2);
    assert_eq!(batch["events"][0]["data"]["current"]["status"], "queued");
    assert_eq!(batch["events"][1]["data"]["current"]["status"], "succeeded");
    assert_eq!(batch["events"][1]["injected"], false);
    // Optimistic CAS makes exactly one simultaneous request succeed.
    let a = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool","id":pool.id.to_string(),"revision":2,"patch":{"file_limit":11}}),
    )
    .await;
    let b = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool","id":pool.id.to_string(),"revision":2,"patch":{"file_limit":13}}),
    )
    .await;
    let (a, b) = tokio::join!(
        apply(&c, &serial, route, owner, &a),
        apply(&c, &serial, route, owner, &b)
    );
    let mut statuses = [a["status"].as_str().unwrap(), b["status"].as_str().unwrap()];
    statuses.sort();
    assert_eq!(statuses, ["failed", "succeeded"]);
    let updated = c
        .active_pool_for_route(&serial, route, pool.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.revision, 3);
    assert!([11, 13].contains(&updated.file_limit));
    // A name collision records failure without touching either pool or index.
    let occupied = create(&c, &serial, route, owner, "occupied").await;
    let keys = [pool_keys(ns, &updated), pool_keys(ns, &occupied)].concat();
    let before = rows(&c, &keys).await;
    let collision = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool","id":pool.id.to_string(),"revision":3,"patch":{"name":"occupied"}}),
    )
    .await;
    assert_eq!(
        apply(&c, &serial, route, owner, &collision).await["status"],
        "failed"
    );
    assert_eq!(rows(&c, &keys).await, before);
    // Orphan target name rows are errors, not a reason to rewrite/repair them.
    for target in [pool.id, occupied.id, Uuid::new_v4()] {
        let name = format!("orphan-{target}");
        let alias = pool_name_key(ns, &name);
        let tr = c.db.create_trx().unwrap();
        tr.set(&alias, target.as_bytes());
        tr.commit().await.unwrap();
        let r = submit(
            &c,
            &serial,
            route,
            owner,
            json!({"kind":"pool","id":pool.id.to_string(),"revision":3,"patch":{"name":name}}),
        )
        .await;
        let keys = [
            pool_keys(ns, &updated),
            task_keys(ns, &serial, &r),
            vec![alias.clone()],
        ]
        .concat();
        let before = rows(&c, &keys).await;
        assert!(matches!(
            c.apply_pool_management(&serial, route, owner, 1, r.operation())
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert_eq!(rows(&c, &keys).await, before);
        let tr = c.db.create_trx().unwrap();
        tr.clear(&alias);
        tr.commit().await.unwrap();
    }
    // Library maintenance leaves a queued request available to its original ID.
    let r = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool","id":pool.id.to_string(),"revision":3,"patch":{"file_limit":14}}),
    )
    .await;
    let mut gate = prefix(ns, b'K');
    gate.push(b'l');
    append_component(&mut gate, serial.as_bytes());
    let tr = c.db.create_trx().unwrap();
    tr.set(&gate, b"maintenance");
    tr.commit().await.unwrap();
    let keys = [
        pool_keys(ns, &updated),
        task_keys(ns, &serial, &r),
        vec![gate.clone()],
    ]
    .concat();
    let before = rows(&c, &keys).await;
    assert!(matches!(
        c.apply_pool_management(&serial, route, owner, 1, r.operation())
            .await,
        Err(CatalogError::PoolBusy)
    ));
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.clear(&gate);
    tr.commit().await.unwrap();
    assert_eq!(
        apply(&c, &serial, route, owner, &r).await["status"],
        "succeeded"
    );
    // A valid but nonempty virtual root cannot be removed.
    let d = create(&c, &serial, route, owner, "delete").await;
    let droot = active_pools::root_id(ns, d.id);
    let child = child_key(ns, droot, b"child");
    let inode = Uuid::new_v4();
    let node = node_key(ns, inode);
    let tr = c.db.create_trx().unwrap();
    tr.set(&child, inode.as_bytes());
    tr.set(
        &node,
        &encode_node(&Node {
            kind: NodeKind::Directory,
            directory_version: Some(1),
            parent: droot,
            name: b"child".to_vec(),
            attributes: None,
        }),
    );
    tr.commit().await.unwrap();
    let r = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let keys = [pool_keys(ns, &d), vec![child.clone(), node.clone()]].concat();
    let before = rows(&c, &keys).await;
    assert_eq!(
        apply(&c, &serial, route, owner, &r).await["status"],
        "failed"
    );
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.clear(&child);
    tr.clear(&node);
    tr.commit().await.unwrap();
    // Either kind of in-flight root path claim blocks deletion, even if its
    // counterpart has not materialized. The proof is presence, not repair.
    for claim in [
        reservation::path_key_for(ns, droot, b"pending"),
        active_batch::path_claim_key_for(ns, droot, b"pending"),
    ] {
        let tr = c.db.create_trx().unwrap();
        tr.set(&claim, b"retained-intent");
        tr.commit().await.unwrap();
        let r = submit(
            &c,
            &serial,
            route,
            owner,
            json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
        )
        .await;
        let keys = [pool_keys(ns, &d), vec![claim.clone()]].concat();
        let before = rows(&c, &keys).await;
        assert_eq!(
            apply(&c, &serial, route, owner, &r).await["status"],
            "failed"
        );
        assert_eq!(rows(&c, &keys).await, before);
        let tr = c.db.create_trx().unwrap();
        tr.clear(&claim);
        tr.commit().await.unwrap();
    }
    // Seed a canonical pending assignment, the state a future device actor owns.
    // No generic import-stage mutator is allowed to bypass the Active gate.
    let barcode = "PENDINGL8";
    let assigned = assignment_key(&serial, barcode);
    let index = namespace_assignment_key(ns, d.id, &serial, barcode);
    let tr = c.db.create_trx().unwrap();
    tr.set(&assigned, &encode_assignment(ns, d.id, None));
    tr.set(&index, &encode_namespace_assignment(&serial, barcode));
    tr.commit().await.unwrap();
    let r = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let keys = [pool_keys(ns, &d), vec![assigned.clone(), index.clone()]].concat();
    let before = rows(&c, &keys).await;
    assert_eq!(
        apply(&c, &serial, route, owner, &r).await["status"],
        "failed"
    );
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.clear(&assigned);
    tr.commit().await.unwrap();
    let r = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let keys = [
        pool_keys(ns, &d),
        task_keys(ns, &serial, &r),
        vec![index.clone()],
    ]
    .concat();
    let before = rows(&c, &keys).await;
    assert!(matches!(
        c.apply_pool_management(&serial, route, owner, 1, r.operation())
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.clear(&index);
    tr.commit().await.unwrap();
    // An old owner cannot execute; a new owner completes the original request.
    let r = submit(
        &c,
        &serial,
        route,
        owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let new_owner = owner + 10;
    c.claim_active_owner_epoch(&serial, new_owner)
        .await
        .unwrap();
    let keys = [pool_keys(ns, &d), task_keys(ns, &serial, &r)].concat();
    let before = rows(&c, &keys).await;
    assert!(matches!(
        c.apply_pool_management(&serial, route, owner, 1, r.operation())
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert_eq!(rows(&c, &keys).await, before);
    let deleted = apply(&c, &serial, route, new_owner, &r).await;
    assert_eq!(deleted["status"], "succeeded");
    assert_eq!(deleted["result"]["pool"]["revision"], 2);
    assert_eq!(
        c.active_pool_for_route(&serial, route, d.id).await.unwrap(),
        None
    );
    assert_eq!(apply(&c, &serial, route, new_owner, &r).await, deleted);
    assert_eq!(
        c.create_active_pool(&serial, route, new_owner, &d)
            .await
            .unwrap(),
        ActivePoolCreation::Retired
    );
    let replacement = PoolRecord {
        id: Uuid::new_v4(),
        ..d.clone()
    };
    assert_eq!(
        c.create_active_pool(&serial, route, new_owner, &replacement)
            .await
            .unwrap(),
        ActivePoolCreation::Created(replacement.clone())
    );
    // Unsupported device work remains queued, with its event and identity intact.
    let device = submit(
        &c,
        &serial,
        route,
        new_owner,
        json!({"kind":"library-rescan"}),
    )
    .await;
    let keys = task_keys(ns, &serial, &device);
    let before = rows(&c, &keys).await;
    assert!(matches!(
        c.apply_pool_management(&serial, route, new_owner, 1, device.operation())
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(rows(&c, &keys).await, before);
    // Corruption of an original task projection cannot be papered over by a
    // successful pool patch. Both pool and queued receipt stay byte-identical.
    let p = c
        .active_pool_for_route(&serial, route, pool.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.revision, 4);
    let r = submit(
        &c,
        &serial,
        route,
        new_owner,
        json!({"kind":"pool","id":p.id.to_string(),"revision":4,"patch":{"file_limit":15}}),
    )
    .await;
    let mut projection = key(b'~');
    projection.extend_from_slice(ns.as_bytes());
    append_component(&mut projection, serial.as_bytes());
    projection.push(b's');
    append_component(&mut projection, task_entity(1, r.operation()).as_bytes());
    let tr = c.db.create_trx().unwrap();
    let saved = tr.get(&projection, false).await.unwrap().unwrap().to_vec();
    tr.set(&projection, b"corrupt");
    tr.commit().await.unwrap();
    let keys = [
        pool_keys(ns, &p),
        task_keys(ns, &serial, &r),
        vec![projection.clone()],
    ]
    .concat();
    let before = rows(&c, &keys).await;
    assert!(
        c.apply_pool_management(&serial, route, new_owner, 1, r.operation())
            .await
            .is_err()
    );
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.set(&projection, &saved);
    tr.commit().await.unwrap();
    assert_eq!(
        apply(&c, &serial, route, new_owner, &r).await["status"],
        "succeeded"
    );
    // Existing virtual roots and name indexes are requirements, never repaired.
    let p = c
        .active_pool_for_route(&serial, route, pool.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.revision, 5);
    for damaged in [pool_name_key(ns, &p.name), node_key(ns, root)] {
        let r = submit(
            &c,
            &serial,
            route,
            new_owner,
            json!({"kind":"pool","id":p.id.to_string(),"revision":5,"patch":{"file_limit":16}}),
        )
        .await;
        let tr = c.db.create_trx().unwrap();
        let saved = tr.get(&damaged, false).await.unwrap().unwrap().to_vec();
        tr.clear(&damaged);
        tr.commit().await.unwrap();
        let keys = [pool_keys(ns, &p), task_keys(ns, &serial, &r)].concat();
        let before = rows(&c, &keys).await;
        assert!(matches!(
            c.apply_pool_management(&serial, route, new_owner, 1, r.operation())
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert_eq!(rows(&c, &keys).await, before);
        let tr = c.db.create_trx().unwrap();
        tr.set(&damaged, &saved);
        tr.commit().await.unwrap();
    }
    // Force notification append failure after write_task has staged its terminal
    // receipt: the binding-level abort must preserve both receipt and pool.
    let r = submit(
        &c,
        &serial,
        route,
        new_owner,
        json!({"kind":"pool","id":p.id.to_string(),"revision":5,"patch":{"file_limit":16}}),
    )
    .await;
    let head = task_keys(ns, &serial, &r).pop().unwrap();
    let tr = c.db.create_trx().unwrap();
    let saved = tr.get(&head, false).await.unwrap().unwrap().to_vec();
    tr.set(&head, &u64::MAX.to_be_bytes());
    tr.commit().await.unwrap();
    let keys = [pool_keys(ns, &p), task_keys(ns, &serial, &r)].concat();
    let before = rows(&c, &keys).await;
    let error = c
        .apply_pool_management(&serial, route, new_owner, 1, r.operation())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("通知序号耗尽"), "{error}");
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.set(&head, &saved);
    tr.commit().await.unwrap();
    assert_eq!(
        apply(&c, &serial, route, new_owner, &r).await["status"],
        "succeeded"
    );
    // Two deletes cannot both retire or increment the same pool generation.
    let d = create(&c, &serial, route, new_owner, "delete-race").await;
    let a = submit(
        &c,
        &serial,
        route,
        new_owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let b = submit(
        &c,
        &serial,
        route,
        new_owner,
        json!({"kind":"pool-delete","id":d.id.to_string(),"revision":1}),
    )
    .await;
    let (a, b) = tokio::join!(
        apply(&c, &serial, route, new_owner, &a),
        apply(&c, &serial, route, new_owner, &b)
    );
    let mut statuses = [a["status"].as_str().unwrap(), b["status"].as_str().unwrap()];
    statuses.sort();
    assert_eq!(statuses, ["failed", "succeeded"]);
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        retired_pool_revision_at(&tr, ns, d.id).await.unwrap(),
        Some(2)
    );
}
