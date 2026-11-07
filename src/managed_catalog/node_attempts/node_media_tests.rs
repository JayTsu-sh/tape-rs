use super::*;
use crate::daemon::fdb_node_media::{NodeMediaResult, execute_node_mutation};
use crate::scsi::cdb::opcode;
use crate::scsi::sim::{Fault, FaultAction, SimTransport};

const BUDGET: usize = 4 << 20;
const TIME: &str = "2026-11-01T00:10:00.000000000Z";

async fn queue(scope: &Scope<'_>, spec: NodeMutationSpec) -> NodeMutationTask {
    let request = scope
        .catalog
        .prepare_node_mutation_request(scope.serial, scope.route, scope.owner, &spec, TIME)
        .await
        .unwrap();
    scope
        .catalog
        .queue_node_mutation_request(scope.route, scope.owner, &request, BUDGET)
        .await
        .unwrap()
}

fn mounted(device: &SimTransport, owner: u64) -> LtfsVolume<'_> {
    let key = ReservationKey::new(3, owner);
    fence(device, key).unwrap();
    let mut volume = LtfsVolume::mount(device).unwrap();
    volume.set_reservation_guard(Some(key));
    volume
}

async fn run<'a>(
    scope: &Scope<'_>,
    volume: LtfsVolume<'a>,
    task: &NodeMutationTask,
) -> crate::error::Result<crate::daemon::fdb_node_media::NodeMediaCommit<'a>> {
    Box::pin(execute_node_mutation(
        scope.catalog,
        volume,
        scope.route,
        ReservationKey::new(3, scope.owner),
        task,
        BUDGET,
    ))
    .await
}

fn writes(device: &SimTransport) -> usize {
    device
        .command_log()
        .iter()
        .filter(|&&command| {
            matches!(
                command,
                opcode::WRITE_6 | opcode::WRITE_FILEMARKS_6 | opcode::WRITE_ATTRIBUTE
            )
        })
        .count()
}

fn medium_error(command: u8) -> Fault {
    Fault {
        opcode: command,
        after: 0,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0c,
            ascq: 0,
        },
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；通过节点执行接口完成创建、修改、删除和旧结果重试，不接触硬件"]
async fn queued_nodes_execute_and_historical_results_do_not_repeat_device_io() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(false, false, false, true))
        .await
        .unwrap();
    Box::pin(fresh_workflow(&fixture)).await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn fresh_workflow(fixture: &PublicationFixture) {
    let scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    let file_epoch = catalog.request_window(namespace).await.unwrap().accepting;
    let mut volume = mounted(&fixture.device, scope.owner);
    let start = volume.committed_index_construct();
    let stale = queue(
        &scope,
        scope.spec("/stale", None, NodeMutation::CreateDirectory),
    )
    .await;
    let directory = queue(
        &scope,
        scope.spec("/auto", None, NodeMutation::CreateDirectory),
    )
    .await;
    let committed = run(&scope, volume, &directory).await.unwrap();
    let NodeMediaResult::Published(created_directory) = committed.result().clone() else {
        panic!("directory publication")
    };
    assert_eq!(created_directory.version(), 1);
    assert_eq!(created_directory.current().generation, start.generation + 1);
    let completed = catalog
        .node_mutation_task(
            scope.serial,
            scope.route,
            directory.request_epoch,
            directory.operation,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        catalog
            .reject_node_mutation_task(
                scope.serial,
                scope.route,
                scope.owner,
                directory.request_epoch,
                directory.operation,
                ""
            )
            .await
            .unwrap(),
        completed
    );
    assert!(matches!(
        catalog
            .prepare_node_mutation_task(
                scope.serial,
                scope.route,
                scope.owner,
                stale.request_epoch,
                stale.operation,
                BUDGET
            )
            .await,
        Err(CatalogError::Invalid(_))
    ));
    let rejected = catalog
        .reject_node_mutation_task(
            scope.serial,
            scope.route,
            scope.owner,
            stale.request_epoch,
            stale.operation,
            "父目录版本已改变",
        )
        .await
        .unwrap();
    assert_eq!(rejected.state, NodeMutationTaskState::Rejected);
    // A valid-looking Qr is metadata corruption for a Rejected task, never
    // physical evidence or permission to replace the retained refusal.
    let mut stray = created_directory.clone();
    stray.attempt.operation = stale.operation;
    stray.attempt.epoch = stale.request_epoch;
    let result_key = super::key(
        namespace,
        scope.serial,
        stale.request_epoch,
        stale.operation,
    );
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&result_key, &super::encode(&stray).unwrap());
    tr.commit().await.unwrap();
    assert!(matches!(
        catalog
            .node_mutation_task(
                scope.serial,
                scope.route,
                stale.request_epoch,
                stale.operation
            )
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    let tr = catalog.db.create_trx().unwrap();
    tr.clear(&result_key);
    tr.commit().await.unwrap();
    volume = committed.into_volume();
    let commands = fixture.device.command_log();
    assert!(
        matches!(run(&scope, volume, &stale).await, Err(crate::error::TapeError::Refused { reason }) if reason.contains("父目录版本已改变"))
    );
    assert_eq!(fixture.device.command_log(), commands);
    volume = mounted(&fixture.device, scope.owner);
    let commands = fixture.device.command_log();
    let repeated = run(&scope, volume, &directory).await.unwrap();
    assert_eq!(
        repeated.result(),
        &NodeMediaResult::Published(created_directory)
    );
    assert_eq!(fixture.device.command_log(), commands);
    volume = repeated.into_volume();

    let link = queue(
        &scope,
        scope.spec(
            "/auto/link",
            None,
            NodeMutation::CreateSymlink {
                target: ".LTFSEE_DATA/missing".into(),
            },
        ),
    )
    .await;
    let committed = run(&scope, volume, &link).await.unwrap();
    let NodeMediaResult::Published(created_link) = committed.result().clone() else {
        panic!("native link publication")
    };
    let original_copy = created_link.physical_copy().unwrap().1.clone();
    assert!(matches!(original_copy.origin, CopyOrigin::NodeLink { .. }));
    assert!(original_copy.media.as_ref().unwrap().native_symlink);
    volume = committed.into_volume();
    let metadata = queue(
        &scope,
        scope.spec(
            "/auto/link",
            Some(1),
            NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
        ),
    )
    .await;
    let committed = run(&scope, volume, &metadata).await.unwrap();
    let NodeMediaResult::Published(changed) = committed.result().clone() else {
        panic!("metadata publication")
    };
    assert_eq!(changed.version(), 2);
    let copy = changed.physical_copy().unwrap().1;
    assert_eq!(copy.origin, original_copy.origin);
    assert_eq!(copy.volume_generation, original_copy.volume_generation);
    volume = committed.into_volume();
    let unlink = queue(
        &scope,
        scope.spec("/auto/link", Some(2), NodeMutation::Unlink),
    )
    .await;
    let committed = run(&scope, volume, &unlink).await.unwrap();
    let NodeMediaResult::Deleted(deleted) = committed.result() else {
        panic!("typed deletion")
    };
    assert_eq!(deleted.version(), 3);
    assert_eq!(deleted.node(), created_link.node());
    volume = committed.into_volume();
    let path = pool_catalog_path(scope.pool, b"/auto/link").unwrap();
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &path, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .node(namespace, created_link.node())
            .await
            .unwrap()
            .is_none()
    );
    let commands = fixture.device.command_log();
    let repeated = run(&scope, volume, &link).await.unwrap();
    assert_eq!(repeated.result(), &NodeMediaResult::Published(created_link));
    assert_eq!(fixture.device.command_log(), commands);
    volume = repeated.into_volume();
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &path, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    let parent = pool_catalog_path(scope.pool, b"/auto").unwrap();
    let Some(ResolvedPath::Directory { version, .. }) = catalog
        .resolve_active_path_for_route(scope.serial, &parent, scope.route)
        .await
        .unwrap()
    else {
        panic!("live parent")
    };
    let rmdir = queue(
        &scope,
        scope.spec("/auto", Some(version), NodeMutation::RemoveDirectory),
    )
    .await;
    let committed = run(&scope, volume, &rmdir).await.unwrap();
    assert!(matches!(committed.result(), NodeMediaResult::Deleted(_)));
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &parent, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog.request_window(namespace).await.unwrap().accepting,
        file_epoch
    );
    let mut files = 0;
    committed.volume().index().walk_files(|_, _| files += 1);
    assert_eq!(files, 0);
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；通过执行接口修改并删除普通文件、原生链接和 EE 别名，不接触硬件"]
async fn imported_file_and_ee_mutations_preserve_original_body_identity() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(true, true, false, true))
        .await
        .unwrap();
    Box::pin(imported_workflow(&fixture)).await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn imported_workflow(fixture: &PublicationFixture) {
    let scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    let mut volume = mounted(&fixture.device, scope.owner);
    for path in ["/body", "/link", "/alias"] {
        let full = pool_catalog_path(scope.pool, path.as_bytes()).unwrap();
        let Some(ResolvedPath::File { copy, .. }) = scope
            .catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("imported file")
        };
        let version = copy.version;
        let task = queue(
            &scope,
            scope.spec(
                path,
                Some(version),
                NodeMutation::ChangeMetadata {
                    readonly: Some(true),
                    access_time: None,
                    modify_time: None,
                },
            ),
        )
        .await;
        let committed = run(&scope, volume, &task).await.unwrap();
        let NodeMediaResult::Published(changed) = committed.result().clone() else {
            panic!("file publication")
        };
        assert_eq!(changed.version(), version + 1);
        let current = changed.physical_copy().unwrap().1;
        assert_eq!(current.origin, copy.origin);
        assert_eq!(current.volume_generation, copy.volume_generation);
        assert_eq!(current.ltfs_file_uid, copy.ltfs_file_uid);
        assert_eq!(current.size, copy.size);
        assert_eq!(current.content_digest, copy.content_digest);
        assert_eq!(current.media, copy.media);
        let before = fixture.device.command_log().len();
        let attributes = Box::pin(attributes_http(&scope, path)).await;
        assert_eq!(fixture.device.command_log().len(), before);
        assert_eq!(attributes["version"], version + 1);
        assert_eq!(attributes["metadata"]["readonly"], true);
        if path == "/alias" {
            assert_eq!(attributes["kind"], "file");
            assert!(attributes["symlink_target"].is_null());
        } else if path == "/link" {
            assert_eq!(attributes["kind"], "symlink");
            assert!(attributes["symlink_target"].is_string());
        }
        volume = committed.into_volume();
        let unlink = queue(
            &scope,
            scope.spec(path, Some(version + 1), NodeMutation::Unlink),
        )
        .await;
        let committed = run(&scope, volume, &unlink).await.unwrap();
        let NodeMediaResult::Deleted(deleted) = committed.result() else {
            panic!("file deletion")
        };
        assert_eq!(deleted.version(), version + 2);
        assert_eq!(deleted.node(), changed.node());
        volume = committed.into_volume();
        assert!(
            scope
                .catalog
                .resolve_active_path_for_route(scope.serial, &full, scope.route)
                .await
                .unwrap()
                .is_none()
        );
    }
    // EE alias deletion keeps the actual data object and records its directory
    // ownership; it does not turn this body into an empty native link.
    let internal = volume.index().find_directory("/.LTFSEE_DATA").unwrap();
    assert_eq!(internal.files.len(), 1);
    assert!(
        internal
            .xattrs
            .iter()
            .any(|attribute| attribute.key == "tapers.fdb.eeInternalDirectory")
    );
}

async fn attributes_http(scope: &Scope<'_>, path: &str) -> serde_json::Value {
    use base64::Engine as _;
    use http_body_util::BodyExt as _;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let router = crate::daemon::fdb_http::owner_router(
        std::sync::Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap()),
        std::collections::BTreeMap::from([(scope.serial.to_string(), scope.route)]),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let request = axum::http::Request::builder()
        .uri(format!(
            "/v1/fdb/attributes/{}?pool={}&path={}",
            scope.serial,
            scope.pool,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path.as_bytes())
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let service = hyper_util::service::TowerToHyperService::new(router);
    let response = hyper::service::Service::call(&service, request)
        .await
        .unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        code,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 S4 后故障与接管，不重复追加，不接触硬件"]
async fn unobserved_s4_is_published_after_takeover_without_reappending() {
    recovery_case(RecoveryCase::Committed).await;
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；写入故障后关闭原尝试并重建，不接触硬件"]
async fn incomplete_attempt_is_closed_and_rebased_after_takeover() {
    recovery_case(RecoveryCase::Incomplete).await;
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；未知更高 S4 保留原 Writing 和卷占用，不接触硬件"]
async fn unknown_s4_keeps_the_original_attempt_without_writing_or_publishing() {
    recovery_case(RecoveryCase::Unknown).await;
}

#[derive(Clone, Copy)]
enum RecoveryCase {
    Committed,
    Incomplete,
    Unknown,
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；取消、脏视图、PR 丢失和旧 owner 均不开始节点写入，不接触硬件"]
async fn invalid_execution_authority_does_not_admit_or_write_a_node_attempt() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(false, false, false, true))
        .await
        .unwrap();
    Box::pin(authority_workflow(&fixture)).await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn authority_workflow(fixture: &PublicationFixture) {
    let mut scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    let volume = mounted(&fixture.device, scope.owner);
    let source = volume.committed_index_construct();
    let cancelled = queue(
        &scope,
        scope.spec("/cancelled", None, NodeMutation::CreateDirectory),
    )
    .await;
    scope
        .catalog
        .cancel_node_mutation_task(
            scope.serial,
            scope.route,
            scope.owner,
            cancelled.request_epoch,
            cancelled.operation,
        )
        .await
        .unwrap();
    let commands = fixture.device.command_log();
    assert!(run(&scope, volume, &cancelled).await.is_err());
    assert_eq!(fixture.device.command_log(), commands);

    let task = queue(
        &scope,
        scope.spec("/authorized", None, NodeMutation::CreateDirectory),
    )
    .await;
    let mut volume = mounted(&fixture.device, scope.owner);
    volume.set_root_xattr("local.dirty", "uncommitted").unwrap();
    let written = writes(&fixture.device);
    assert!(run(&scope, volume, &task).await.is_err());
    assert_eq!(writes(&fixture.device), written);
    let volume = mounted(&fixture.device, scope.owner);
    fence(&fixture.device, ReservationKey::new(2, scope.owner + 1)).unwrap();
    let written = writes(&fixture.device);
    assert!(matches!(
        run(&scope, volume, &task).await,
        Err(crate::error::TapeError::OwnershipLost { .. })
    ));
    assert_eq!(writes(&fixture.device), written);
    let volume = mounted(&fixture.device, scope.owner);
    scope
        .catalog
        .claim_active_owner_epoch(scope.serial, scope.owner + 1)
        .await
        .unwrap();
    let commands = fixture.device.command_log();
    assert!(matches!(
        run(&scope, volume, &task).await,
        Err(crate::error::TapeError::OwnershipLost { .. })
    ));
    assert_eq!(fixture.device.command_log(), commands);
    scope.owner += 1;
    assert_eq!(
        scope
            .catalog
            .node_mutation_task(
                scope.serial,
                scope.route,
                task.request_epoch,
                task.operation
            )
            .await
            .unwrap()
            .unwrap()
            .state,
        NodeMutationTaskState::Queued
    );
    assert!(
        scope
            .catalog
            .pending_node_mutation_attempt(
                scope.serial,
                scope.route,
                scope.owner,
                source.volume_uuid
            )
            .await
            .unwrap()
            .is_none()
    );
    let volume = mounted(&fixture.device, scope.owner);
    let committed = run(&scope, volume, &task).await.unwrap();
    let NodeMediaResult::Published(result) = committed.result() else {
        panic!("new owner publication")
    };
    assert_eq!(result.attempt().owner, scope.owner);
    let commands = fixture.device.command_log();
    scope.owner -= 1;
    assert!(matches!(
        run(&scope, committed.into_volume(), &task).await,
        Err(crate::error::TapeError::OwnershipLost { .. })
    ));
    assert_eq!(fixture.device.command_log(), commands);
}

async fn recovery_case(case: RecoveryCase) {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(false, false, false, true))
        .await
        .unwrap();
    Box::pin(recovery_workflow(&fixture, case)).await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn recovery_workflow(fixture: &PublicationFixture, case: RecoveryCase) {
    let mut scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    let volume = mounted(&fixture.device, scope.owner);
    let before: ActiveIndexIdentity = volume.committed_index_construct().into();
    let task = queue(
        &scope,
        scope.spec("/recover", None, NodeMutation::CreateDirectory),
    )
    .await;
    fixture
        .device
        .inject(medium_error(if matches!(case, RecoveryCase::Incomplete) {
            opcode::WRITE_6
        } else {
            opcode::WRITE_ATTRIBUTE
        }));
    assert!(run(&scope, volume, &task).await.is_err());
    fixture.device.clear_faults();
    let attempt = scope
        .catalog
        .pending_node_mutation_attempt(scope.serial, scope.route, scope.owner, before.volume_uuid)
        .await
        .unwrap()
        .unwrap();
    let commands = fixture.device.command_log();
    assert!(matches!(
        scope
            .catalog
            .reject_node_mutation_task(
                scope.serial,
                scope.route,
                scope.owner,
                task.request_epoch,
                task.operation,
                "Writing 不能直接拒绝"
            )
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(fixture.device.command_log(), commands);
    assert_eq!(
        scope
            .catalog
            .pending_node_mutation_attempt(
                scope.serial,
                scope.route,
                scope.owner,
                before.volume_uuid
            )
            .await
            .unwrap(),
        Some(attempt.clone())
    );
    assert!(
        scope
            .catalog
            .node_mutation_observation(scope.route, scope.owner, &attempt)
            .await
            .unwrap()
            .is_none()
    );
    if matches!(case, RecoveryCase::Unknown) {
        let mut foreign = LtfsVolume::mount(&fixture.device).unwrap();
        foreign.set_reservation_guard(Some(ReservationKey::new(3, scope.owner)));
        foreign
            .set_root_xattr("foreign.change", "unknown S4")
            .unwrap();
        foreign.sync_batch().unwrap();
    }
    scope.owner += 1;
    scope
        .catalog
        .claim_active_owner_epoch(scope.serial, scope.owner)
        .await
        .unwrap();
    let volume = mounted(&fixture.device, scope.owner);
    let current: ActiveIndexIdentity = volume.committed_index_construct().into();
    let command_writes = writes(&fixture.device);
    let result = run(&scope, volume, &task).await;
    let path = pool_catalog_path(scope.pool, b"/recover").unwrap();
    if matches!(case, RecoveryCase::Unknown) {
        assert!(result.is_err());
        assert_eq!(writes(&fixture.device), command_writes);
        assert_eq!(
            scope
                .catalog
                .pending_node_mutation_attempt(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    before.volume_uuid
                )
                .await
                .unwrap(),
            Some(attempt)
        );
        assert!(
            scope
                .catalog
                .resolve_active_path_for_route(scope.serial, &path, scope.route)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            scope
                .catalog
                .node_mutation_task(
                    scope.serial,
                    scope.route,
                    task.request_epoch,
                    task.operation
                )
                .await
                .unwrap()
                .unwrap()
                .state,
            NodeMutationTaskState::Writing
        );
        return;
    }
    let committed = result.unwrap();
    let NodeMediaResult::Published(published) = committed.result() else {
        panic!("recovered publication")
    };
    assert_eq!(published.completed_owner(), scope.owner);
    assert!(
        scope
            .catalog
            .resolve_active_path_for_route(scope.serial, &path, scope.route)
            .await
            .unwrap()
            .is_some()
    );
    if matches!(case, RecoveryCase::Committed) {
        assert_eq!(published.attempt(), &attempt);
        assert_eq!(published.current(), current);
        assert_eq!(writes(&fixture.device), command_writes);
    } else {
        assert_ne!(published.attempt().id, attempt.id);
        let closed = scope
            .catalog
            .node_mutation_attempt_closure(scope.route, scope.owner, &attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closed.attempt(), &attempt);
        assert_eq!(published.attempt().previous, closed.current());
        assert!(writes(&fixture.device) > command_writes);
    }
    assert!(
        scope
            .catalog
            .pending_node_mutation_attempt(
                scope.serial,
                scope.route,
                scope.owner,
                before.volume_uuid
            )
            .await
            .unwrap()
            .is_none()
    );
}
