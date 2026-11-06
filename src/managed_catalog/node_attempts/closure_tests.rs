use super::*;
use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::cdb::opcode;
use crate::scsi::reservation::{ReservationKey, fence};
use crate::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::os::unix::fs::DirBuilderExt;

#[test]
fn closure_codec_keeps_original_scope_and_rejects_noncanonical_or_wrong_outcomes() {
    let namespace = Uuid::new_v4();
    let previous = ActiveIndexIdentity {
        volume_uuid: Uuid::new_v4(),
        generation: 2,
        location: crate::ltfs::index::IndexLocation {
            partition: 'b',
            start_block: 11,
        },
        sha256: [7; 32],
        incremental: false,
    };
    // Codec data, never presented as physical commit or absence evidence.
    let mut value = NodeMutationAttemptClosure {
        attempt: NodeMutationAttempt {
            id: Uuid::new_v4(),
            library: "CLOSURE-CODEC".into(),
            namespace,
            pool: Uuid::new_v4(),
            epoch: 1,
            operation: Uuid::new_v4(),
            logical_fingerprint: [5; 32],
            owner: 100,
            created_at_millis: 1_798_000_000_000,
            object: NodePlanObject::for_bytes("CLOSURE-CODEC", namespace, b"inert", 100).unwrap(),
            scan: Uuid::new_v4(),
            previous,
            pool_revision: 1,
            assignment_revision: 2,
            source_copy: None,
        },
        kind: NodeMutationAbsenceKind::Unchanged,
        current: previous,
        file_count: u64::MAX,
        closed_owner: 101,
        closed_at_millis: 1_798_000_000_001,
    };
    for kind in [
        NodeMutationAbsenceKind::Unchanged,
        NodeMutationAbsenceKind::Discarded,
    ] {
        value.kind = kind;
        value.current = if kind == NodeMutationAbsenceKind::Unchanged {
            previous
        } else {
            ActiveIndexIdentity {
                generation: 3,
                location: crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 15,
                },
                sha256: [8; 32],
                ..previous
            }
        };
        let bytes = encode(&value).unwrap();
        assert_eq!(decode(&bytes).unwrap(), value);
        for end in 0..bytes.len() {
            assert!(decode(&bytes[..end]).is_err());
        }
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for field in wire.as_object().unwrap().keys() {
            let mut bad = wire.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                decode(bad.to_string().as_bytes()).is_err(),
                "missing {field}"
            );
        }
        for (field, bad) in [
            ("version", json!(0)),
            ("version", json!(2)),
            ("legacy", json!(true)),
            ("kind", json!("file-batch")),
            ("attempt", json!("!")),
            ("current", json!("!")),
            ("closed_owner", json!(99)),
            ("closed_at_millis", json!(1_797_999_999_999_u64)),
            ("closed_at_millis", json!(253_402_300_800_000_u64)),
        ] {
            let mut bad_wire = wire.clone();
            bad_wire[field] = bad;
            assert!(decode(bad_wire.to_string().as_bytes()).is_err(), "{field}");
        }
        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(decode(&noncanonical).is_err());
        let mut wrong = value.clone();
        wrong.kind = if kind == NodeMutationAbsenceKind::Unchanged {
            NodeMutationAbsenceKind::Discarded
        } else {
            NodeMutationAbsenceKind::Unchanged
        };
        assert!(encode(&wrong).is_err());
    }
    assert!(decode(&vec![b' '; 2 * MAX_RECORD + 1]).is_err());
    value.current.incremental = true;
    assert!(encode(&value).is_err());
    assert_ne!(
        key(
            namespace,
            previous.volume_uuid,
            previous.generation,
            value.attempt.id
        ),
        key(
            namespace,
            previous.volume_uuid,
            previous.generation + 1,
            value.attempt.id
        )
    );
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim 未发生/修尾关闭与新尝试接任，不接触硬件"]
async fn absent_attempts_close_atomically_preserve_pool_holds_and_rebase_across_owners() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    for (tail, closed_before_takeover) in
        (0..3).flat_map(|tail| [false, true].map(|closed| (tail, closed)))
    {
        let namespace = Uuid::new_v4();
        let serial = format!("SIM-NODE-CLOSE-{namespace}");
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
            name: "node-close".into(),
            file_limit: 200_000,
            created_at_millis: management::now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let library = SimLibrary::new(1, 4, 1);
        library
            .insert_cartridge(SimCartridge::blank("CLOS01L8", 64 << 20), 0)
            .unwrap();
        library.load_into_drive("CLOS01L8", 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                volume_id: "CLOS01".into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
        fence(&device, ReservationKey::new(1, 100)).unwrap();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 100)));
        let previous: ActiveIndexIdentity =
            volume.verified_committed_index_construct().unwrap().into();
        let xml = volume.index().to_xml().unwrap();
        assert!(!previous.incremental);
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&xml)), previous.sha256);
        let root = std::env::temp_dir().join(format!("node-close-source-{}", Uuid::new_v4()));
        eprintln!("node_closure_workspace={}", root.display());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let expected = ExpectedIndex {
            volume_uuid: previous.volume_uuid,
            generation: previous.generation,
            location: previous.location,
            sha256: previous.sha256,
        };
        let spool_path = root.join("source.sqlite");
        let cache_path = root.join("cache.sqlite");
        ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
        VolumeExtentCache::build(xml.as_ref(), &cache_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let source = NativeMediaSource {
            identity: spool.identity().clone(),
            files: cache.file_count().unwrap(),
            paths: spool.candidate_stats().entries,
        };
        assert_eq!((source.files, source.paths), (0, 0));
        let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"CLOS01L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        catalog
            .admit_management(&serial, route, 100, &request)
            .await
            .unwrap();
        let intake = catalog
            .start_native_media(&serial, route, 100, 1, request.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .bind_native_media_source(&intake, 100, &source)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&intake, 100)
            .await
            .unwrap();
        catalog
            .finish_native_media(&intake, 100, &source)
            .await
            .unwrap();
        let spec = NodeMutationSpec {
            pool: pool.id,
            operation: Uuid::new_v4(),
            path: "/wanted".into(),
            expected_version: None,
            mutation: NodeMutation::CreateDirectory,
        };
        let request = catalog
            .prepare_node_mutation_request(
                &serial,
                route,
                100,
                &spec,
                "2026-11-01T00:00:00.000000000Z",
            )
            .await
            .unwrap();
        catalog
            .queue_node_mutation_request(route, 100, &request, 1 << 20)
            .await
            .unwrap();
        let prepared = catalog
            .prepare_node_mutation_task(&serial, route, 100, 1, spec.operation, 1 << 20)
            .await
            .unwrap();
        let inputs = catalog
            .capture_prepared_node_inputs(&serial, route, 100, 1, spec.operation, 1 << 20)
            .await
            .unwrap();
        let plan = NodeMutationPlan::prepare(
            NodeMutationContext {
                library_serial: serial.clone(),
                namespace,
                pool: pool.id,
                operation: spec.operation,
                request_epoch: 1,
                owner_epoch: 100,
                parent: inputs.baseline.parent.id,
                parent_version: inputs.baseline.parent.version,
                node: inputs.baseline.node,
                expected_version: None,
                scan: intake.scan(),
                previous,
                path: spec.path.clone(),
            },
            spec.mutation.clone(),
            volume.index(),
            request.time(),
            &inputs.ancestors,
            None,
        )
        .unwrap();
        let attempt = catalog
            .admit_node_mutation_attempt(route, 100, Uuid::new_v4(), &inputs, &plan, 1 << 20)
            .await
            .unwrap();
        assert!(
            catalog
                .node_mutation_attempt_closure(route, 100, &attempt)
                .await
                .unwrap()
                .is_none()
        );
        if tail != 0 {
            volume.stage_node_mutation(&plan).unwrap();
            if tail == 1 {
                volume
                    .append_file("/orphan", &mut std::io::Cursor::new(b"unindexed"))
                    .unwrap();
                crate::tape::commands::TapeDrive::new(&device)
                    .write_filemark(0)
                    .unwrap();
            } else {
                device.inject(Fault {
                    opcode: opcode::WRITE_FILEMARKS_6,
                    action: FaultAction::CheckCondition {
                        sense_key: 0x03,
                        asc: 0x0c,
                        ascq: 0,
                    },
                    after: 1,
                    remaining: 1,
                });
                assert!(volume.sync_batch().is_err());
                library.with_cartridge_mut("CLOS01L8", |cartridge| {
                    for partition in &mut cartridge.partitions {
                        partition.flushed = partition.objects.len();
                    }
                });
            }
        }
        drop(volume);
        let first = if closed_before_takeover {
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(1, 100)));
            let evidence = if tail == 0 {
                volume.node_mutation_absence_evidence(&plan).unwrap()
            } else {
                volume.discard_node_mutation_tail(&plan).unwrap()
            };
            Some(
                catalog
                    .close_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        fence(&device, ReservationKey::new(2, 101)).unwrap();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
        let evidence = if volume.writable() {
            volume.node_mutation_absence_evidence(&plan).unwrap()
        } else {
            volume.discard_node_mutation_tail(&plan).unwrap()
        };
        assert_eq!(
            evidence.kind(),
            if tail == 0 {
                NodeMutationAbsenceKind::Unchanged
            } else {
                NodeMutationAbsenceKind::Discarded
            }
        );
        assert!(matches!(
            catalog
                .close_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        if first.is_none() {
            // A conflicting target generation must not partially close Writing.
            let conflict =
                active_volume::generation_key(previous.volume_uuid, previous.generation + 1);
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&conflict, &[0]);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog
                    .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            assert_eq!(
                catalog
                    .node_mutation_task(&serial, route, 1, spec.operation)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                NodeMutationTaskState::Writing
            );
            assert_eq!(
                catalog
                    .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                    .await
                    .unwrap(),
                Some(attempt.clone())
            );
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                tr.get(&conflict, false).await.unwrap().unwrap().as_ref(),
                &[0]
            );
            assert!(
                tr.get(
                    &key(
                        namespace,
                        previous.volume_uuid,
                        previous.generation,
                        attempt.id
                    ),
                    false
                )
                .await
                .unwrap()
                .is_none()
            );
            tr.clear(&conflict);
            tr.commit().await.unwrap();
        }
        let closed = catalog
            .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
            .await
            .unwrap();
        if let Some(first) = first {
            assert_eq!(closed, first);
            assert_eq!(closed.closed_owner(), 100);
        } else {
            assert_eq!(closed.closed_owner(), 101);
        }
        assert_eq!(
            catalog
                .node_mutation_attempt_closure(route, 101, &attempt)
                .await
                .unwrap(),
            Some(closed.clone())
        );
        assert_eq!(closed.attempt(), &attempt);
        assert_eq!(closed.file_count(), 0);
        assert_eq!(closed.current(), evidence.current());
        assert_eq!(
            catalog
                .observed_active_volume_index(&serial, previous.volume_uuid)
                .await
                .unwrap(),
            evidence.current()
        );
        assert!(
            catalog
                .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .node_mutation_attempt(&serial, route, 101, 1, spec.operation)
                .await
                .unwrap()
                .is_none()
        );
        let returned = catalog
            .node_mutation_task(&serial, route, 1, spec.operation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(returned, prepared);
        let tr = catalog.db.create_trx().unwrap();
        assert!(matches!(
            require_pool_writable_at(&tr, namespace, pool.id).await,
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            tr.get(&attempt_key(&attempt), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            super::super::encode(&attempt).unwrap()
        );
        if tail != 0 {
            assert!(
                active_volume::observed_copy_index_at(
                    &tr,
                    &serial,
                    previous.volume_uuid,
                    evidence.current().generation
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(
                active_volume::observed_batch_at(
                    &tr,
                    &serial,
                    previous.volume_uuid,
                    evidence.current().generation
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        let path = pool_catalog_path(pool.id, spec.path.as_bytes()).unwrap();
        assert!(
            catalog
                .resolve_active_path_for_route(&serial, &path, route)
                .await
                .unwrap()
                .is_none()
        );
        let fresh = catalog
            .capture_prepared_node_inputs(&serial, route, 101, 1, spec.operation, 1 << 20)
            .await
            .unwrap();
        assert_eq!(fresh.request.encode().unwrap(), request.encode().unwrap());
        let mut context = plan.context().clone();
        context.owner_epoch = 101;
        context.previous = evidence.current();
        let rebased = NodeMutationPlan::prepare(
            context,
            spec.mutation.clone(),
            volume.index(),
            request.time(),
            &fresh.ancestors,
            None,
        )
        .unwrap();
        assert_ne!(rebased.fingerprint(), plan.fingerprint());
        assert!(matches!(
            catalog
                .admit_node_mutation_attempt(route, 101, attempt.id, &fresh, &rebased, 1 << 20)
                .await,
            Err(CatalogError::Invalid("已关闭节点尝试身份不能复用"))
        ));
        let next = catalog
            .admit_node_mutation_attempt(route, 101, Uuid::new_v4(), &fresh, &rebased, 1 << 20)
            .await
            .unwrap();
        assert_eq!(next.original_owner(), 101);
        assert_eq!(
            catalog
                .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
                .await
                .unwrap(),
            closed
        );
        assert_eq!(
            catalog
                .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                .await
                .unwrap(),
            Some(next.clone())
        );
        assert_eq!(
            catalog
                .load_node_mutation_attempt_plan(route, 101, &next, 1 << 20)
                .await
                .unwrap()
                .encode()
                .unwrap(),
            rebased.encode().unwrap()
        );
        // Removing the original live header must not break the retained closure
        // or its typed history, nor disturb the new attempt's independent hold.
        let tr = catalog.db.create_trx().unwrap();
        let original = tr
            .get(&attempt_key(&attempt), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.clear(&attempt_key(&attempt));
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .node_mutation_attempt_closure(route, 101, &attempt)
                .await
                .unwrap(),
            Some(closed.clone())
        );
        assert_eq!(
            catalog
                .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                .await
                .unwrap(),
            Some(next.clone())
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&attempt_key(&attempt), &original);
        tr.commit().await.unwrap();
        let closure_key = key(
            namespace,
            previous.volume_uuid,
            previous.generation,
            attempt.id,
        );
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&closure_key, false).await.unwrap().unwrap().to_vec();
        tr.clear(&closure_key);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .node_mutation_attempt_closure(route, 101, &attempt)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&closure_key, false).await.unwrap().is_none());
        tr.set(&closure_key, &original);
        tr.commit().await.unwrap();
        let mut protected = vec![closure_key.clone()];
        if tail != 0 {
            let history =
                active_volume::generation_key(previous.volume_uuid, evidence.current().generation);
            let mut count = history.clone();
            count.push(b'f');
            protected.extend([history[..history.len() - 8].to_vec(), count, history]);
        }
        for damaged in &protected {
            let tr = catalog.db.create_trx().unwrap();
            let original = tr.get(damaged, false).await.unwrap().unwrap().to_vec();
            for replacement in [None, Some(vec![0])] {
                let tr = catalog.db.create_trx().unwrap();
                match &replacement {
                    Some(bytes) => tr.set(damaged, bytes),
                    None => tr.clear(damaged),
                }
                tr.commit().await.unwrap();
                assert!(matches!(
                    catalog
                        .node_mutation_attempt_closure(route, 101, &attempt)
                        .await,
                    Err(CatalogError::Corrupt(_))
                ));
                assert!(
                    catalog
                        .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
                        .await
                        .is_err()
                );
                let tr = catalog.db.create_trx().unwrap();
                assert_eq!(
                    tr.get(damaged, false).await.unwrap().map(|v| v.to_vec()),
                    replacement
                );
                tr.set(damaged, &original);
                tr.commit().await.unwrap();
                assert_eq!(
                    catalog
                        .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                        .await
                        .unwrap(),
                    Some(next.clone())
                );
            }
        }
        if tail != 0 {
            let history =
                active_volume::generation_key(previous.volume_uuid, evidence.current().generation);
            let tr = catalog.db.create_trx().unwrap();
            let original = tr.get(&history, false).await.unwrap().unwrap().to_vec();
            let mut wrong = original.clone();
            wrong[0] = 1;
            tr.set(&history, &wrong);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog
                    .node_mutation_attempt_closure(route, 101, &attempt)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            assert!(
                catalog
                    .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
                    .await
                    .is_err()
            );
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                tr.get(&history, false).await.unwrap().unwrap().as_ref(),
                wrong
            );
            tr.set(&history, &original);
            tr.commit().await.unwrap();
        }
        volume.stage_node_mutation(&rebased).unwrap();
        volume.sync_batch().unwrap();
        let committed = volume.node_mutation_evidence(&rebased).unwrap();
        catalog
            .observe_node_mutation_attempt(route, 101, &next, &rebased, &committed)
            .await
            .unwrap();
        assert!(
            catalog
                .node_mutation_observation(route, 101, &attempt)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog
                .close_node_mutation_attempt(route, 101, &attempt, &plan, &evidence)
                .await
                .unwrap(),
            closed
        );
        assert_eq!(
            catalog
                .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                .await
                .unwrap(),
            Some(next)
        );
        drop(volume);
        drop(cache);
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }
}
