use super::*;
use serde_json::json;

struct Fixture {
    catalog: ManagedCatalog,
    serial: String,
    route: ActiveLibraryRoute,
    pool: Uuid,
}

impl Fixture {
    async fn queue(&self, owner: u64, path: &str) -> (NodeMutationRequest, NodeMutationTask) {
        let request = self
            .catalog
            .prepare_node_mutation_request(
                &self.serial,
                self.route,
                owner,
                &NodeMutationSpec {
                    pool: self.pool,
                    operation: Uuid::new_v4(),
                    path: path.into(),
                    expected_version: None,
                    mutation: NodeMutation::CreateDirectory,
                },
                "2026-11-01T00:30:00.000000000Z",
            )
            .await
            .unwrap();
        let task = self
            .catalog
            .queue_node_mutation_request(self.route, owner, &request, 4 << 20)
            .await
            .unwrap();
        (request, task)
    }

    async fn reject(
        &self,
        owner: u64,
        task: &NodeMutationTask,
        reason: &str,
    ) -> Result<NodeMutationTask> {
        self.catalog
            .reject_node_mutation_task(
                &self.serial,
                self.route,
                owner,
                task.request_epoch,
                task.operation,
                reason,
            )
            .await
    }

    async fn prepare(&self, owner: u64, task: &NodeMutationTask) -> NodeMutationTask {
        self.catalog
            .prepare_node_mutation_task(
                &self.serial,
                self.route,
                owner,
                task.request_epoch,
                task.operation,
                4 << 20,
            )
            .await
            .unwrap()
    }

    async fn gate(&self, owner: u64) -> Result<()> {
        self.catalog
            .check_active_pool_gate(&self.serial, self.route, owner, self.pool)
            .await
    }
}

async fn setup() -> Fixture {
    let catalog = ManagedCatalog::open(Some(&std::env::var("FDB_TEST_CLUSTER").unwrap())).unwrap();
    let namespace = Uuid::new_v4();
    let serial = format!("SIM-NODE-REJECT-{namespace}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|node|json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
        name: "node-rejection".into(),
        file_limit: 200_000,
        created_at_millis: 1_798_000_000_000,
        policy: PoolPolicy::default(),
        revision: 1,
    };
    catalog
        .create_active_pool(&serial, route, 100, &pool)
        .await
        .unwrap();
    Fixture {
        catalog,
        serial,
        route,
        pool: pool.id,
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；未写入节点拒绝的原子收尾、原原因与池占用，不操作介质"]
async fn unwritten_rejections_are_atomic_retained_and_do_not_release_later_holds() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(setup()).await;
    Box::pin(workflow(&fixture)).await;
}

async fn workflow(f: &Fixture) {
    let catalog = &f.catalog;
    let namespace = f.route.route.namespace;
    let (request, queued) = f.queue(100, "/first").await;
    let root = node_key(namespace, active_pools::root_id(namespace, f.pool));
    let tr = catalog.db.create_trx().unwrap();
    let original_root = tr.get(&root, false).await.unwrap().unwrap().to_vec();
    drop(tr);
    for reason in ["", "   ", "bad\nreason", &"x".repeat(501)] {
        assert!(matches!(
            f.reject(100, &queued, reason).await,
            Err(CatalogError::Invalid(_))
        ));
        assert_eq!(
            catalog
                .node_mutation_task(&f.serial, f.route, queued.request_epoch, queued.operation)
                .await
                .unwrap(),
            Some(queued.clone())
        );
        f.gate(100).await.unwrap();
    }
    assert!(matches!(
        f.reject(99, &queued, "stale owner").await,
        Err(CatalogError::OwnerFenced)
    ));
    let mut missing = queued.clone();
    missing.operation = Uuid::new_v4();
    assert!(matches!(
        f.reject(100, &missing, "missing").await,
        Err(CatalogError::Invalid(_))
    ));
    assert!(
        catalog
            .node_mutation_task(&f.serial, f.route, missing.request_epoch, missing.operation)
            .await
            .unwrap()
            .is_none()
    );
    let rejected = f.reject(100, &queued, "原节点前提已失效").await.unwrap();
    assert_eq!(rejected.state, NodeMutationTaskState::Rejected);
    assert_eq!(
        rejected.rejection_reason.as_deref(),
        Some("原节点前提已失效")
    );
    assert_eq!(rejected.completed_owner_epoch, Some(100));
    assert!(rejected.completed_at_millis >= Some(rejected.created_at_millis));
    let tr = catalog.db.create_trx().unwrap();
    for key in [
        identity_key(
            namespace,
            &f.serial,
            b'q',
            queued.request_epoch,
            queued.operation,
        ),
        global_pending_key(namespace, &f.serial, queued.request_epoch, queued.operation),
        hold_key(namespace, f.pool),
    ] {
        assert!(tr.get(&key, false).await.unwrap().is_none());
    }
    assert_eq!(
        tr.get(&root, false).await.unwrap().unwrap().as_ref(),
        original_root
    );
    drop(tr);

    let (_, second) = f.queue(100, "/second").await;
    let prepared = f.prepare(100, &second).await;
    assert_eq!(prepared.state, NodeMutationTaskState::Prepared);
    assert!(matches!(f.gate(100).await, Err(CatalogError::PoolBusy)));
    assert_eq!(
        f.reject(100, &queued, "another reason").await.unwrap(),
        rejected
    );
    assert_eq!(f.reject(100, &queued, "").await.unwrap(), rejected);
    assert_eq!(f.prepare(100, &queued).await, rejected);
    assert_eq!(
        catalog
            .cancel_node_mutation_task(
                &f.serial,
                f.route,
                100,
                queued.request_epoch,
                queued.operation
            )
            .await
            .unwrap(),
        rejected
    );
    assert_eq!(
        catalog
            .queue_node_mutation_request(f.route, 100, &request, 4 << 20)
            .await
            .unwrap(),
        rejected
    );
    assert!(matches!(f.gate(100).await, Err(CatalogError::PoolBusy)));
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        held_task_at(&tr, namespace, f.pool).await.unwrap(),
        Some(prepared.clone())
    );
    drop(tr);

    let second_rejected = f
        .reject(100, &prepared, "物理计划在写入前被拒绝")
        .await
        .unwrap();
    assert_eq!(
        second_rejected.prepared_owner_epoch,
        prepared.prepared_owner_epoch
    );
    assert_eq!(
        second_rejected.prepared_at_millis,
        prepared.prepared_at_millis
    );
    f.gate(100).await.unwrap();
    catalog
        .claim_active_owner_epoch(&f.serial, 101)
        .await
        .unwrap();
    assert_eq!(
        f.reject(101, &queued, "new owner retry").await.unwrap(),
        rejected
    );
    assert!(matches!(
        f.reject(100, &queued, "old owner").await,
        Err(CatalogError::OwnerFenced)
    ));
    let (_, third) = f.queue(101, "/third").await;
    let third = f.prepare(101, &third).await;
    assert_eq!(
        f.reject(101, &prepared, "retry").await.unwrap(),
        second_rejected
    );
    assert!(matches!(f.gate(101).await, Err(CatalogError::PoolBusy)));

    // A contradictory canonical record is corruption, never a repaired or
    // re-applied terminal. Preserve the unrelated current Prepared hold.
    let task_key = identity_key(
        namespace,
        &f.serial,
        b'o',
        queued.request_epoch,
        queued.operation,
    );
    let bytes = encode(&rejected).unwrap();
    let mut bad: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    bad["rejection_reason"] = json!(null);
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&task_key, &node_mutation::canonical(&bad).unwrap());
    tr.commit().await.unwrap();
    assert!(matches!(
        f.reject(101, &queued, "repair forbidden").await,
        Err(CatalogError::Corrupt(_))
    ));
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        held_task_at(&tr, namespace, f.pool).await.unwrap(),
        Some(third.clone())
    );
    tr.set(&task_key, &bytes);
    tr.commit().await.unwrap();
    let cancelled = catalog
        .cancel_node_mutation_task(
            &f.serial,
            f.route,
            101,
            third.request_epoch,
            third.operation,
        )
        .await
        .unwrap();
    assert_eq!(cancelled.state, NodeMutationTaskState::Cancelled);
    assert_eq!(f.reject(101, &third, "").await.unwrap(), cancelled);
    f.gate(101).await.unwrap();
    assert!(
        catalog
            .pending_node_mutation_tasks(&f.serial, f.route, 101, None, 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&root, false).await.unwrap().unwrap().as_ref(),
        original_root
    );
}
