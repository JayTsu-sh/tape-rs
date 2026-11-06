use super::*;
use crate::managed_catalog::link_kind::{LinkKind, NATIVE_SYMLINK};

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim 新链接创建、管理来源、接管、原子发布与重新捕获，不接触硬件"]
async fn native_links_publish_with_real_s4_and_keep_type_origin_and_first_result() {
    let _network = unsafe { foundationdb::boot() };
    assert!(
        publication_scenario(false, false, true, false)
            .await
            .is_none()
    );
}

pub(super) async fn check_new_links(
    scope: &mut Scope<'_>,
    volume: &mut LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
) {
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    let tr = catalog.db.create_trx().unwrap();
    tr.set(
        &management::prefix(namespace, b'w'),
        &requests::encode_window(RequestWindow {
            first_retained: 1,
            accepting: 9,
        }),
    );
    tr.commit().await.unwrap();
    for (offset, target) in ["../missing", ".LTFSEE_DATA/missing"]
        .into_iter()
        .enumerate()
    {
        let path = format!("/created-link-{offset}");
        let full = pool_catalog_path(scope.pool, path.as_bytes()).unwrap();
        let spec = scope.spec(
            &path,
            None,
            NodeMutation::CreateSymlink {
                target: target.into(),
            },
        );
        let (request, plan, attempt) = scope
            .admit(volume, &spec, "2026-11-01T00:01:00.000000000Z")
            .await;
        assert_eq!(attempt.epoch, 9);
        assert!(attempt.source_copy.is_none());
        assert!(
            catalog
                .resolve_active_path_for_route(scope.serial, &full, scope.route)
                .await
                .unwrap()
                .is_none()
        );
        // The first attempt is taken over before physical S4, the second after.
        let old_owner = scope.owner;
        if offset == 0 {
            scope.owner += 1;
            catalog
                .claim_active_owner_epoch(scope.serial, scope.owner)
                .await
                .unwrap();
            let key = ReservationKey::new(u8::try_from(scope.owner - 99).unwrap(), scope.owner);
            fence(device, key).unwrap();
            volume.set_reservation_guard(Some(key));
        }
        volume.stage_node_mutation(&plan).unwrap();
        volume.sync_batch().unwrap();
        let mut evidence = volume.node_mutation_evidence(&plan).unwrap();
        assert_eq!(evidence.file_count(), offset as u64 + 1);
        let real = volume.index().find_file(&path).unwrap();
        assert_eq!(real.length, 0);
        assert!(real.extents.is_empty());
        assert_eq!(real.symlink.as_deref(), Some(target));
        assert_eq!(
            NodeAttributes::from_file(real)
                .unwrap()
                .link_kind()
                .unwrap(),
            LinkKind::Native
        );
        let uid = real.meta.file_uid;
        if offset == 1 {
            scope.owner += 1;
            catalog
                .claim_active_owner_epoch(scope.serial, scope.owner)
                .await
                .unwrap();
            let key = ReservationKey::new(u8::try_from(scope.owner - 99).unwrap(), scope.owner);
            fence(device, key).unwrap();
            volume.set_reservation_guard(Some(key));
        }
        evidence = volume.node_mutation_evidence(&plan).unwrap();
        assert!(matches!(
            catalog
                .observe_node_mutation_attempt(scope.route, old_owner, &attempt, &plan, &evidence)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        catalog
            .observe_node_mutation_attempt(scope.route, scope.owner, &attempt, &plan, &evidence)
            .await
            .unwrap();
        let copy_id = super::copy_id(&attempt, plan.context().node);
        let target_key = copy_key(namespace, plan.context().node, copy_id);
        let reverse = crate::managed_catalog::volume_key(
            evidence.current().volume_uuid,
            attempt.scan,
            namespace,
            plan.context().node,
            copy_id,
        );
        // Each orphan row must remain untouched; no other logical row or
        // completion result may leak out of a failed publication transaction.
        let inode = node_key(namespace, plan.context().node);
        let head = head_key(namespace, plan.context().node);
        let child = child_key(namespace, plan.context().parent, &path.as_bytes()[1..]);
        let rows = [inode, head, target_key, reverse];
        for damaged in &rows {
            let tr = catalog.db.create_trx().unwrap();
            tr.set(damaged, b"orphan");
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .publish_node_mutation_attempt(
                        scope.route,
                        scope.owner,
                        &attempt,
                        &plan,
                        &request,
                        4 << 20
                    )
                    .await
                    .is_err()
            );
            let tr = catalog.db.create_trx().unwrap();
            for row in &rows {
                assert_eq!(
                    tr.get(row, false).await.unwrap().map(|v| v.to_vec()),
                    if row == damaged {
                        Some(b"orphan".to_vec())
                    } else {
                        None
                    }
                );
            }
            assert!(tr.get(&child, false).await.unwrap().is_none());
            assert!(
                tr.get(&key(namespace, scope.serial, 9, spec.operation), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                catalog
                    .pending_node_mutation_attempt(
                        scope.serial,
                        scope.route,
                        scope.owner,
                        evidence.current().volume_uuid
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                catalog
                    .node_mutation_task(scope.serial, scope.route, 9, spec.operation)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                NodeMutationTaskState::Writing
            );
            assert!(
                require_pool_writable_at(&tr, namespace, scope.pool)
                    .await
                    .is_err()
            );
            tr.clear(damaged);
            tr.commit().await.unwrap();
        }
        let commands = device.command_log().len();
        let first = scope.publish(&request, &plan, &attempt).await;
        assert_eq!(device.command_log().len(), commands);
        assert_eq!(
            (first.version(), first.parent_version()),
            (1, plan.context().parent_version + 1)
        );
        let (_, copy) = first.physical_copy().unwrap();
        assert_eq!(
            copy.origin,
            CopyOrigin::NodeLink {
                management_epoch: 9,
                operation: spec.operation
            }
        );
        assert_eq!(copy.volume_generation, evidence.current().generation);
        assert_eq!(copy.ltfs_file_uid, uid);
        let Some(ResolvedPath::File {
            head,
            copy: current,
            attributes,
            ..
        }) = catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("created native link")
        };
        assert_eq!(head.version, 1);
        assert_eq!(current.as_ref(), copy);
        let attributes = attributes.unwrap();
        assert_eq!(attributes.symlink(), Some(target));
        assert_eq!(attributes.meta().unwrap().file_uid, uid);
        assert_eq!(attributes.link_kind().unwrap(), LinkKind::Native);
        assert!(
            catalog
                .resolve_active_read_source_for_owner(scope.serial, &full, scope.route, scope.owner)
                .await
                .is_err()
        );
        assert_eq!(
            catalog.request_window(namespace).await.unwrap().accepting,
            1
        );
        let created_copy = copy.clone();
        let metadata = scope.spec(
            &path,
            Some(1),
            NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
        );
        let (meta_request, meta_plan, meta_attempt) = scope
            .admit(volume, &metadata, "2026-11-01T00:01:01.000000000Z")
            .await;
        scope.commit(volume, &meta_plan, &meta_attempt).await;
        let result = scope
            .publish(&meta_request, &meta_plan, &meta_attempt)
            .await;
        let mut expected_copy = created_copy;
        expected_copy.version = 2;
        assert_eq!(result.physical_copy().unwrap().1, &expected_copy);
        assert_eq!(result.parent_version(), first.parent_version());
        assert_eq!(scope.publish(&request, &plan, &attempt).await, first);
        assert_eq!(
            catalog
                .node_mutation_publication(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    9,
                    spec.operation
                )
                .await
                .unwrap(),
            Some(first)
        );
        for mutation in [
            NodeMutation::SetXattr {
                attribute: crate::managed_catalog::link_kind::native_marker(),
                mode: node_mutation::XattrMode::Replace,
            },
            NodeMutation::RemoveXattr {
                key: NATIVE_SYMLINK.into(),
            },
        ] {
            let bad = scope.spec(&path, Some(2), mutation);
            let commands = device.command_log().len();
            assert!(
                catalog
                    .prepare_node_mutation_request(
                        scope.serial,
                        scope.route,
                        scope.owner,
                        &bad,
                        "2026-11-01T00:01:02.000000000Z"
                    )
                    .await
                    .is_err()
            );
            assert_eq!(device.command_log().len(), commands);
        }
        capture_link_source(volume, device, root, offset, &path, target);
    }
}

fn capture_link_source(
    volume: &LtfsVolume<'_>,
    device: &dyn crate::scsi::transport::TapeTransport,
    root: &std::path::Path,
    offset: usize,
    path: &str,
    target: &str,
) {
    use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
    use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
    use crate::tape::commands::TapeDrive;
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.into();
    budget.index_scratch.max_bytes = 64 << 20;
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_index_source(&TapeDrive::new(device), volume.label(), &budget).unwrap();
    assert_eq!(captured.construct(), volume.committed_index_construct());
    let storage = ReadStorageBudget {
        cache_bytes: 128 << 20,
        headroom_bytes: 0,
        ..Default::default()
    };
    let spool_path = root.join(format!("new-link-{offset}-spool.sqlite"));
    let cache_path = root.join(format!("new-link-{offset}-cache.sqlite"));
    ImportSpool::build_captured(
        &captured,
        &spool_path,
        &mut CacheBuild::new(root, storage).unwrap(),
    )
    .unwrap();
    VolumeExtentCache::build_captured(
        &captured,
        &cache_path,
        &mut CacheBuild::new(root, storage).unwrap(),
    )
    .unwrap();
    let identity = captured.identity().unwrap();
    let spool = ImportSpool::open(
        &spool_path,
        crate::managed_catalog::index_evidence::expected_index(&identity),
    )
    .unwrap();
    let cache = VolumeExtentCache::open_captured(&cache_path, &identity).unwrap();
    spool.verify_extent_cache(&cache).unwrap();
    let candidate = spool
        .candidate_page(None, 1000)
        .unwrap()
        .candidates
        .into_iter()
        .find(|c| c.logical_path == path)
        .unwrap();
    let (attributes, _, data) =
        crate::managed_catalog::staged_import::source_evidence(&spool, &candidate).unwrap();
    assert_eq!(attributes.link_kind().unwrap(), LinkKind::Native);
    assert_eq!(attributes.symlink(), Some(target));
    assert!(matches!(
        data,
        crate::managed_catalog::StagedSourceData::NativeSymlink
    ));
    assert!(
        spool
            .resolve_file_data(path.trim_start_matches('/'))
            .is_err()
    );
}
