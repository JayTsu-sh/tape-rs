use super::*;
use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::ltfs::index::Xattr;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::reservation::{ReservationKey, fence};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use serde_json::json;
use std::os::unix::fs::DirBuilderExt;

#[path = "deletion_tests.rs"]
mod deletions;
#[path = "directory_target_tests.rs"]
mod directory_targets;
#[path = "new_link_tests.rs"]
mod new_links;
#[path = "node_media_tests.rs"]
mod node_media;
#[path = "node_takeover_tests.rs"]
mod node_takeover;

#[test]
fn publication_result_codec_is_strict_and_preserves_original_attempt_identity() {
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
    // Metadata codec fixture only, never used as device S4 evidence.
    let value = NodeMutationPublication {
        attempt: NodeMutationAttempt {
            id: Uuid::new_v4(),
            library: "PUBLISH-CODEC".into(),
            namespace,
            pool: Uuid::new_v4(),
            epoch: 1,
            operation: Uuid::new_v4(),
            logical_fingerprint: [5; 32],
            owner: 100,
            created_at_millis: 1_798_000_000_000,
            object: NodePlanObject::for_bytes("PUBLISH-CODEC", namespace, b"inert", 100).unwrap(),
            scan: Uuid::new_v4(),
            previous,
            pool_revision: 1,
            assignment_revision: 1,
            source_copy: None,
        },
        current: ActiveIndexIdentity {
            generation: 3,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 15,
            },
            sha256: [8; 32],
            ..previous
        },
        node: Uuid::new_v4(),
        version: u64::MAX,
        attributes: [3; 32],
        parent_version: 2,
        parent_attributes: [4; 32],
        copy: None,
        owner: 101,
        completed_at_millis: 1_798_000_000_001,
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
        ("kind", json!("file")),
        ("node", json!(Uuid::nil().to_string())),
        ("node_version", json!(0)),
        ("parent_version", json!(0)),
        ("owner", json!(99)),
        ("completed_at_millis", json!(1_797_999_999_999_u64)),
        ("attempt", json!("!")),
        ("current", json!("!")),
        ("legacy", json!(true)),
    ] {
        let mut bad_wire = wire.clone();
        bad_wire[field] = bad;
        assert!(decode(bad_wire.to_string().as_bytes()).is_err(), "{field}");
    }
    let mut bad = bytes.clone();
    bad.push(b' ');
    assert!(decode(&bad).is_err());
    assert_ne!(digest(&bad), digest(&bytes));

    let mut file = value.clone();
    file.version = 3;
    file.attempt.source_copy = Some((
        Uuid::new_v4(),
        PhysicalCopy {
            version: 2,
            origin: CopyOrigin::FileRequest { request_epoch: 7 },
            volume_id: previous.volume_uuid,
            volume_generation: previous.generation,
            volume_scan_id: file.attempt.scan,
            ltfs_file_uid: 19,
            size: 123,
            content_digest: ContentDigest::Sha256([9; 32]),
            media: Some(MediaProvenance {
                version: (55, 6),
                logical_file_uid: 19,
                physical_path: "body".into(),
                ee_alias: false,
                native_symlink: false,
            }),
        },
    ));
    file.copy = projected_copy(&file.attempt, file.node, file.version, file.current).unwrap();
    let bytes = encode(&file).unwrap();
    assert_eq!(decode(&bytes).unwrap(), file);
    assert_eq!(
        file.physical_copy().unwrap().1.origin,
        CopyOrigin::FileRequest { request_epoch: 7 }
    );
    assert_eq!(
        file.physical_copy()
            .unwrap()
            .1
            .media
            .as_ref()
            .unwrap()
            .version,
        (55, 6)
    );
    assert!(!file.is_directory());
    for field in [
        "origin",
        "uid",
        "size",
        "digest",
        "generation",
        "media",
        "id",
    ] {
        let mut bad = file.clone();
        let (id, copy) = bad.copy.as_mut().unwrap();
        match field {
            "origin" => copy.origin = CopyOrigin::FileRequest { request_epoch: 1 },
            "uid" => copy.ltfs_file_uid += 1,
            "size" => copy.size += 1,
            "digest" => copy.content_digest = ContentDigest::Unknown,
            "generation" => copy.volume_generation += 1,
            "media" => copy.media.as_mut().unwrap().version = (100, 1),
            "id" => *id = Uuid::new_v4(),
            _ => unreachable!(),
        }
        assert!(encode(&bad).is_err(), "changed {field}");
    }
    let mut old_directory = wire;
    old_directory.as_object_mut().unwrap().remove("copy");
    assert!(decode(old_directory.to_string().as_bytes()).is_err());

    let mut link = value;
    link.version = 1;
    link.copy = Some((
        copy_id(&link.attempt, link.node),
        PhysicalCopy {
            version: 1,
            origin: CopyOrigin::NodeLink {
                management_epoch: link.attempt.epoch,
                operation: link.attempt.operation,
            },
            volume_id: link.current.volume_uuid,
            volume_generation: link.current.generation,
            volume_scan_id: link.attempt.scan,
            ltfs_file_uid: 31,
            size: 0,
            content_digest: ContentDigest::Unknown,
            media: Some(MediaProvenance {
                version: (0, 0),
                logical_file_uid: 31,
                physical_path: "new-link".into(),
                ee_alias: false,
                native_symlink: true,
            }),
        },
    ));
    assert_eq!(decode(&encode(&link).unwrap()).unwrap(), link);
    for field in [
        "version",
        "origin",
        "generation",
        "operation",
        "id",
        "media",
    ] {
        let mut bad = link.clone();
        let (id, copy) = bad.copy.as_mut().unwrap();
        match field {
            "version" => {
                copy.version += 1;
                bad.version += 1;
            }
            "origin" => {
                copy.origin = CopyOrigin::FileRequest {
                    request_epoch: bad.attempt.epoch,
                }
            }
            "generation" => copy.volume_generation = bad.attempt.previous.generation,
            "operation" => {
                copy.origin = CopyOrigin::NodeLink {
                    management_epoch: bad.attempt.epoch,
                    operation: Uuid::new_v4(),
                }
            }
            "id" => *id = Uuid::new_v4(),
            "media" => copy.media.as_mut().unwrap().version = (1, 1),
            _ => unreachable!(),
        }
        assert!(encode(&bad).is_err(), "new link {field}");
    }
}

struct Scope<'a> {
    catalog: &'a ManagedCatalog,
    serial: &'a str,
    route: ActiveLibraryRoute,
    owner: u64,
    pool: Uuid,
    scan: Uuid,
}
impl Scope<'_> {
    async fn admit(
        &self,
        volume: &mut LtfsVolume<'_>,
        spec: &NodeMutationSpec,
        time: &str,
    ) -> (NodeMutationRequest, NodeMutationPlan, NodeMutationAttempt) {
        let request = self
            .catalog
            .prepare_node_mutation_request(self.serial, self.route, self.owner, spec, time)
            .await
            .unwrap();
        let epoch = request.context().request_epoch;
        self.catalog
            .queue_node_mutation_request(self.route, self.owner, &request, 4 << 20)
            .await
            .unwrap();
        self.catalog
            .prepare_node_mutation_task(
                self.serial,
                self.route,
                self.owner,
                epoch,
                spec.operation,
                4 << 20,
            )
            .await
            .unwrap();
        let inputs = self
            .catalog
            .capture_prepared_node_inputs(
                self.serial,
                self.route,
                self.owner,
                epoch,
                spec.operation,
                4 << 20,
            )
            .await
            .unwrap();
        let target = match &inputs.baseline.current {
            Some(ResolvedPath::Directory { attributes, .. }) => attributes
                .as_ref()
                .map(|attrs| (NodeKind::Directory, attrs)),
            Some(ResolvedPath::File { attributes, .. }) => {
                attributes.as_ref().map(|attrs| (NodeKind::File, attrs))
            }
            None => None,
        };
        let plan = NodeMutationPlan::prepare(
            NodeMutationContext {
                library_serial: self.serial.into(),
                namespace: inputs.baseline.namespace,
                pool: self.pool,
                operation: spec.operation,
                request_epoch: epoch,
                owner_epoch: self.owner,
                parent: inputs.baseline.parent.id,
                parent_version: inputs.baseline.parent.version,
                node: inputs.baseline.node,
                expected_version: spec.expected_version,
                scan: self.scan,
                previous: volume.verified_committed_index_construct().unwrap().into(),
                path: spec.path.clone(),
            },
            spec.mutation.clone(),
            volume.index(),
            time,
            &inputs.ancestors,
            target,
        )
        .unwrap();
        let attempt = self
            .catalog
            .admit_node_mutation_attempt(
                self.route,
                self.owner,
                Uuid::new_v4(),
                &inputs,
                &plan,
                4 << 20,
            )
            .await
            .unwrap();
        (request, plan, attempt)
    }
    async fn commit(
        &self,
        volume: &mut LtfsVolume<'_>,
        plan: &NodeMutationPlan,
        attempt: &NodeMutationAttempt,
    ) {
        volume.stage_node_mutation(plan).unwrap();
        volume.sync_batch().unwrap();
        let evidence = volume.node_mutation_evidence(plan).unwrap();
        self.catalog
            .observe_node_mutation_attempt(self.route, self.owner, attempt, plan, &evidence)
            .await
            .unwrap();
    }
    async fn publish(
        &self,
        request: &NodeMutationRequest,
        plan: &NodeMutationPlan,
        attempt: &NodeMutationAttempt,
    ) -> NodeMutationPublication {
        self.catalog
            .publish_node_mutation_attempt(self.route, self.owner, attempt, plan, request, 4 << 20)
            .await
            .unwrap()
    }
    fn spec(&self, path: &str, version: Option<u64>, mutation: NodeMutation) -> NodeMutationSpec {
        NodeMutationSpec {
            pool: self.pool,
            operation: Uuid::new_v4(),
            path: path.into(),
            expected_version: version,
            mutation,
        }
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim 目录 S4 后的原子发布、接管与大属性，不接触硬件"]
async fn directories_publish_atomically_with_versions_and_retained_results_across_takeover() {
    let _network = unsafe { foundationdb::boot() };
    assert!(
        publication_scenario(false, false, false, false)
            .await
            .is_none()
    );
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim 文件与原生链接属性发布、旧读取与数据身份保护，不接触硬件"]
async fn file_attributes_publish_without_changing_data_or_original_content_identity() {
    let _network = unsafe { foundationdb::boot() };
    assert!(
        publication_scenario(true, false, false, false)
            .await
            .is_none()
    );
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim EE 数据对象绑定、属性发布和历史数据读取，不接触硬件"]
async fn ee_alias_attributes_bind_direct_data_and_keep_original_body_read_source() {
    let _network = unsafe { foundationdb::boot() };
    assert!(
        publication_scenario(true, true, false, false)
            .await
            .is_none()
    );
}

// Return owned resources so extended workflows run after setup's large async
// polling frame has returned, within the default test-thread stack budget.
struct PublicationFixture {
    catalog: ManagedCatalog,
    serial: String,
    route: ActiveLibraryRoute,
    owner: u64,
    pool: Uuid,
    scan: Uuid,
    device: crate::scsi::sim::SimTransport,
    root: std::path::PathBuf,
}

async fn publication_scenario(
    with_files: bool,
    with_ee: bool,
    with_new_links: bool,
    with_deletions: bool,
) -> Option<PublicationFixture> {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let serial = format!("SIM-NODE-PUBLISH-{namespace}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
        name: "node-publication".into(),
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
        .insert_cartridge(SimCartridge::blank("PUBL01L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("PUBL01L8", 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "PUBL01".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    fence(&device, ReservationKey::new(1, 100)).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 100)));
    volume
        .set_root_xattr("foreign.private", "retained")
        .unwrap();
    if with_files {
        volume
            .append_file(
                "/body",
                &mut std::io::Cursor::new(b"immutable original body"),
            )
            .unwrap();
        volume.add_symlink("/link", "body").unwrap();
        volume
            .set_node_xattr(
                "/body",
                Xattr {
                    key: "foreign.original".into(),
                    value: "kept".into(),
                    base64: false,
                },
                true,
                false,
            )
            .unwrap();
    }
    if with_ee {
        volume
            .append_file(
                "/.LTFSEE_DATA/data",
                &mut std::io::Cursor::new(b"immutable original body"),
            )
            .unwrap();
        volume.add_symlink("/alias", ".LTFSEE_DATA/data").unwrap();
    }
    volume.sync().unwrap();
    let previous: ActiveIndexIdentity = volume.verified_committed_index_construct().unwrap().into();
    let xml = volume.index().to_xml().unwrap();
    assert!(!previous.incremental);
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&xml)), previous.sha256);
    let root = std::env::temp_dir().join(format!("node-publish-source-{}", Uuid::new_v4()));
    eprintln!("node_publication_workspace={}", root.display());
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
    let expected_files = if with_files { 2 } else { 0 };
    assert_eq!(
        (source.files, source.paths),
        (
            expected_files + if with_ee { 2 } else { 0 },
            expected_files + u64::from(with_ee)
        )
    );
    let intake_request=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"PUBL01L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
    catalog
        .admit_management(&serial, route, 100, &intake_request)
        .await
        .unwrap();
    let intake = catalog
        .start_native_media(&serial, route, 100, 1, intake_request.operation())
        .await
        .unwrap()
        .unwrap();
    catalog
        .bind_native_media_source(&intake, 100, &source)
        .await
        .unwrap();
    let entries = spool
        .candidate_page(None, 1000)
        .unwrap()
        .candidates
        .into_iter()
        .map(|candidate| intake.entry(&spool, candidate).unwrap())
        .collect::<Vec<_>>();
    for entry in &entries {
        catalog
            .project_native_media_entry(&intake, 100, entry, false)
            .await
            .unwrap();
    }
    catalog
        .begin_native_media_projection(&intake, 100)
        .await
        .unwrap();
    for entry in &entries {
        catalog
            .project_native_media_entry(&intake, 100, entry, true)
            .await
            .unwrap();
    }
    catalog
        .finish_native_media(&intake, 100, &source)
        .await
        .unwrap();
    let mut scope = Scope {
        catalog: &catalog,
        serial: &serial,
        route,
        owner: 100,
        pool: pool.id,
        scan: intake.scan(),
    };
    let pool_root = active_pools::root_id(namespace, pool.id);
    let tr = catalog.db.create_trx().unwrap();
    let root_before = tr
        .get(&node_key(namespace, pool_root), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    let root_meta = decode_node(&root_before)
        .unwrap()
        .attributes
        .unwrap()
        .meta()
        .unwrap()
        .clone();
    let spec = scope.spec("/parent", None, NodeMutation::CreateDirectory);
    let (request, plan, attempt) = scope
        .admit(&mut volume, &spec, "2026-11-01T00:00:00.000000000Z")
        .await;
    volume.stage_node_mutation(&plan).unwrap();
    volume.sync_batch().unwrap();
    let evidence = volume.node_mutation_evidence(&plan).unwrap();
    assert!(matches!(
        catalog
            .publish_node_mutation_attempt(route, 100, &attempt, &plan, &request, 4 << 20)
            .await,
        Err(CatalogError::Incomplete)
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
    catalog
        .observe_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
        .await
        .unwrap();
    // This deliberately forged result is metadata corruption, not device
    // evidence: the real opaque S4 above has only reached Writing/observed.
    // A result cannot substitute for an atomically completed task and digest.
    let projection = plan.publication_projection(&request).unwrap();
    let forged = NodeMutationPublication {
        attempt: attempt.clone(),
        current: evidence.current(),
        node: plan.context().node,
        version: 1,
        attributes: projection.after.digest(),
        parent_version: plan.context().parent_version + 1,
        parent_attributes: projection.parent_after.digest(),
        copy: plan_copy(&attempt, &plan, &projection, evidence.current()).unwrap(),
        owner: 100,
        completed_at_millis: management::now().max(attempt.created_at_millis),
    };
    let result_key = key(namespace, &serial, 1, spec.operation);
    let bytes = encode(&forged).unwrap();
    let tr = catalog.db.create_trx().unwrap();
    let volume_hold = volume_key(previous.volume_uuid);
    let held = tr.get(&volume_hold, false).await.unwrap().unwrap().to_vec();
    tr.set(&result_key, &bytes);
    tr.commit().await.unwrap();
    assert!(
        matches!(
            catalog
                .publish_node_mutation_attempt(route, 100, &attempt, &plan, &request, 4 << 20)
                .await,
            Err(CatalogError::Corrupt(_))
        ),
        "Writing must not accept a stray valid publication result"
    );
    assert!(matches!(
        catalog
            .node_mutation_task(&serial, route, 1, spec.operation)
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&result_key, false).await.unwrap().unwrap().as_ref(),
        bytes
    );
    assert_eq!(
        tr.get(&volume_hold, false).await.unwrap().unwrap().as_ref(),
        held
    );
    assert!(
        tr.get(&node_key(namespace, plan.context().node), false)
            .await
            .unwrap()
            .is_none()
    );
    tr.clear(&result_key);
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .node_mutation_task(&serial, route, 1, spec.operation)
            .await
            .unwrap()
            .unwrap()
            .state,
        NodeMutationTaskState::Writing
    );
    // A corrupt canonical parent or orphan file rows must not publish a piece.
    let mut bad_root = decode_node(&root_before).unwrap();
    bad_root.directory_version = Some(99);
    let child = child_key(namespace, pool_root, b"parent");
    let target = node_key(namespace, plan.context().node);
    let orphan_copy = copy_key(namespace, plan.context().node, Uuid::new_v4());
    for (damaged, bad) in [
        (node_key(namespace, pool_root), encode_node(&bad_root)),
        (head_key(namespace, plan.context().node), vec![0]),
        (orphan_copy, vec![0]),
    ] {
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&damaged, false).await.unwrap().map(|v| v.to_vec());
        tr.set(&damaged, &bad);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .publish_node_mutation_attempt(route, 100, &attempt, &plan, &request, 4 << 20)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&damaged, false).await.unwrap().unwrap().as_ref(),
            bad
        );
        assert!(tr.get(&child, false).await.unwrap().is_none());
        assert!(tr.get(&target, false).await.unwrap().is_none());
        assert!(
            tr.get(&key(namespace, &serial, 1, spec.operation), false)
                .await
                .unwrap()
                .is_none()
        );
        match original {
            Some(bytes) => tr.set(&damaged, &bytes),
            None => tr.clear(&damaged),
        };
        tr.commit().await.unwrap();
    }
    catalog
        .claim_active_owner_epoch(&serial, 101)
        .await
        .unwrap();
    fence(&device, ReservationKey::new(2, 101)).unwrap();
    assert!(matches!(
        catalog
            .publish_node_mutation_attempt(route, 100, &attempt, &plan, &request, 4 << 20)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    scope.owner = 101;
    let commands = device.command_log().len();
    let first = scope.publish(&request, &plan, &attempt).await;
    assert_eq!(
        device.command_log().len(),
        commands,
        "logical publication does no I/O"
    );
    assert_eq!(
        (
            first.version(),
            first.parent_version(),
            first.completed_owner()
        ),
        (1, 2, 101)
    );
    let completed = catalog
        .node_mutation_task(&serial, route, 1, spec.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.state, NodeMutationTaskState::Completed);
    assert!(completed.publication_digest.is_some());
    assert!(completed.active_attempt.is_none());
    assert_eq!(
        catalog
            .cancel_node_mutation_task(&serial, route, 101, 1, spec.operation)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(
        catalog
            .queue_node_mutation_request(route, 101, &request, 4 << 20)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(
        catalog
            .prepare_node_mutation_task(&serial, route, 101, 1, spec.operation, 4 << 20)
            .await
            .unwrap(),
        completed
    );
    assert!(
        catalog
            .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
            .await
            .unwrap()
            .is_none()
    );
    let tr = catalog.db.create_trx().unwrap();
    require_pool_writable_at(&tr, namespace, pool.id)
        .await
        .unwrap();
    let root_after = decode_node(
        &tr.get(&node_key(namespace, pool_root), false)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let after_meta = root_after.attributes.unwrap().meta().unwrap().clone();
    assert_eq!(after_meta.creation_time, root_meta.creation_time);
    assert_eq!(after_meta.file_uid, root_meta.file_uid);
    assert_eq!(root_after.directory_version, Some(2));
    let path = pool_catalog_path(pool.id, b"/parent").unwrap();
    let Some(ResolvedPath::Directory {
        id,
        version,
        attributes,
    }) = catalog
        .resolve_active_path_for_route(&serial, &path, route)
        .await
        .unwrap()
    else {
        panic!("published real directory")
    };
    assert_eq!((id, version), (first.node(), 1));
    assert_eq!(attributes.unwrap().digest(), first.attributes_digest());
    drop(volume);
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
    let nested = scope.spec("/parent/child", None, NodeMutation::CreateDirectory);
    let (child_request, child_plan, child_attempt) = scope
        .admit(&mut volume, &nested, "2026-11-01T00:00:01.000000000Z")
        .await;
    scope.commit(&mut volume, &child_plan, &child_attempt).await;
    let tr = catalog.db.create_trx().unwrap();
    let root_saved = tr
        .get(&node_key(namespace, pool_root), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    let mut wrong = decode_node(&root_saved).unwrap();
    wrong.directory_version = Some(99);
    tr.set(&node_key(namespace, pool_root), &encode_node(&wrong));
    tr.commit().await.unwrap();
    assert!(
        catalog
            .publish_node_mutation_attempt(
                route,
                101,
                &child_attempt,
                &child_plan,
                &child_request,
                4 << 20
            )
            .await
            .is_err()
    );
    let tr = catalog.db.create_trx().unwrap();
    assert!(
        tr.get(&node_key(namespace, child_plan.context().node), false)
            .await
            .unwrap()
            .is_none()
    );
    tr.set(&node_key(namespace, pool_root), &root_saved);
    tr.commit().await.unwrap();
    let child_result = scope
        .publish(&child_request, &child_plan, &child_attempt)
        .await;
    assert_eq!(
        (child_result.version(), child_result.parent_version()),
        (1, 2)
    );
    let big = scope.spec(
        "/parent",
        Some(2),
        NodeMutation::SetXattr {
            attribute: Xattr {
                key: "foreign.big".into(),
                value: "x".repeat(256 << 10),
                base64: false,
            },
            mode: node_mutation::XattrMode::Create,
        },
    );
    let (big_request, big_plan, big_attempt) = scope
        .admit(&mut volume, &big, "2026-11-01T00:00:02.000000000Z")
        .await;
    scope.commit(&mut volume, &big_plan, &big_attempt).await;
    let big_result = scope.publish(&big_request, &big_plan, &big_attempt).await;
    assert_eq!((big_result.version(), big_result.parent_version()), (3, 2));
    let tr = catalog.db.create_trx().unwrap();
    let old = decode_node(
        &tr.get(&node_key(namespace, first.node()), false)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap()
    .attributes
    .unwrap();
    assert!(old.is_external());
    let metadata = scope.spec(
        "/parent",
        Some(3),
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    );
    let (meta_request, meta_plan, meta_attempt) = scope
        .admit(&mut volume, &metadata, "2026-11-01T00:00:03.000000000Z")
        .await;
    scope.commit(&mut volume, &meta_plan, &meta_attempt).await;
    let metadata_result = scope
        .publish(&meta_request, &meta_plan, &meta_attempt)
        .await;
    assert_eq!(
        (metadata_result.version(), metadata_result.parent_version()),
        (4, 2)
    );
    let old = catalog.hydrate_attributes(namespace, old).await.unwrap();
    assert!(!old.meta().unwrap().readonly);
    assert_eq!(
        old.xattrs()
            .unwrap()
            .iter()
            .find(|a| a.key == "foreign.big")
            .unwrap()
            .value
            .len(),
        256 << 10
    );
    assert_eq!(scope.publish(&request, &plan, &attempt).await, first);
    assert_eq!(
        catalog
            .node_mutation_publication(&serial, route, 101, 1, spec.operation)
            .await
            .unwrap(),
        Some(first.clone())
    );
    // Retained operation results do not depend on current Serving media.
    let tr = catalog.db.create_trx().unwrap();
    let ready_key = ready_volume_key(previous.volume_uuid);
    let ready = tr.get(&ready_key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&ready_key);
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .node_mutation_publication(&serial, route, 101, 1, spec.operation)
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(scope.publish(&request, &plan, &attempt).await, first);
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&ready_key, &ready);
    tr.commit().await.unwrap();
    // A validly encoded result with a different version must fail its task hash.
    let result_key = key(namespace, &serial, 1, spec.operation);
    let tr = catalog.db.create_trx().unwrap();
    let original = tr.get(&result_key, false).await.unwrap().unwrap().to_vec();
    let mut tampered: serde_json::Value = serde_json::from_slice(&original).unwrap();
    tampered["node_version"] = json!(2);
    let bad = tampered.to_string().into_bytes();
    assert!(decode(&bad).is_ok());
    for replacement in [None, Some(bad)] {
        let tr = catalog.db.create_trx().unwrap();
        match &replacement {
            Some(bytes) => tr.set(&result_key, bytes),
            None => tr.clear(&result_key),
        };
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .node_mutation_publication(&serial, route, 101, 1, spec.operation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog
                .node_mutation_task(&serial, route, 1, spec.operation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&result_key, false)
                .await
                .unwrap()
                .map(|v| v.to_vec()),
            replacement
        );
        tr.set(&result_key, &original);
        tr.commit().await.unwrap();
    }
    let tr = catalog.db.create_trx().unwrap();
    tr.clear(&attempt_key(&attempt));
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .node_mutation_publication(&serial, route, 101, 1, spec.operation)
            .await
            .unwrap(),
        Some(first)
    );
    assert!(
        volume
            .index()
            .find_directory("/parent")
            .unwrap()
            .meta
            .readonly
    );
    assert!(volume.index().find_directory("/parent/child").is_some());
    assert_eq!(volume.root_xattr("foreign.private"), Some("retained"));
    if with_files {
        check_file_attributes(&mut scope, &mut volume, &device, &root, with_ee).await;
    }
    if with_new_links {
        new_links::check_new_links(&mut scope, &mut volume, &device, &root).await;
    }
    let owner = scope.owner;
    let scan = scope.scan;
    drop(volume);
    drop(cache);
    drop(spool);
    if with_deletions {
        return Some(PublicationFixture {
            catalog,
            serial,
            route,
            owner,
            pool: pool.id,
            scan,
            device,
            root,
        });
    }
    std::fs::remove_dir_all(root).unwrap();
    None
}

async fn check_file_attributes(
    scope: &mut Scope<'_>,
    volume: &mut LtfsVolume<'_>,
    device: &dyn crate::scsi::transport::TapeTransport,
    root: &std::path::Path,
    with_ee: bool,
) {
    let namespace = scope.route.route.namespace;
    let catalog = scope.catalog;
    // The management window is deliberately different from the upload window.
    let tr = catalog.db.create_trx().unwrap();
    tr.set(
        &management::prefix(namespace, b'w'),
        &requests::encode_window(RequestWindow {
            first_retained: 1,
            accepting: 9,
        }),
    );
    tr.commit().await.unwrap();
    let physical_before = volume.index().find_file("/body").unwrap().clone();
    let paths = if with_ee {
        vec!["/alias", "/body", "/link"]
    } else {
        vec!["/body", "/link"]
    };
    for path in paths {
        let full = pool_catalog_path(scope.pool, path.as_bytes()).unwrap();
        let Some(ResolvedPath::File {
            id,
            head: old_head,
            copy: old_copy,
            attributes,
            ..
        }) = catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("actual imported file or native link")
        };
        let old_attributes = attributes.unwrap();
        assert_eq!(old_head.version, 1);
        let old_key = copy_key(namespace, id, old_head.copy_id);
        let old_reverse = crate::managed_catalog::volume_key(
            old_copy.volume_id,
            old_copy.volume_scan_id,
            namespace,
            id,
            old_head.copy_id,
        );
        let tr = catalog.db.create_trx().unwrap();
        let old_record = tr.get(&old_key, false).await.unwrap().unwrap().to_vec();
        let old_reverse_record = tr.get(&old_reverse, false).await.unwrap().unwrap().to_vec();
        let inode_key = node_key(namespace, id);
        let old_node = tr.get(&inode_key, false).await.unwrap().unwrap().to_vec();
        let saved_read = if path != "/link" {
            Some(
                catalog
                    .resolve_active_read_source_for_owner(
                        scope.serial,
                        &full,
                        scope.route,
                        scope.owner,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .plan_read(&root.join("cache.sqlite"))
                    .unwrap(),
            )
        } else {
            assert!(
                catalog
                    .resolve_active_read_source_for_owner(
                        scope.serial,
                        &full,
                        scope.route,
                        scope.owner
                    )
                    .await
                    .is_err()
            );
            None
        };
        let spec = scope.spec(
            path,
            Some(1),
            NodeMutation::SetXattr {
                attribute: Xattr {
                    key: "foreign.big".into(),
                    value: "y".repeat(256 << 10),
                    base64: false,
                },
                mode: node_mutation::XattrMode::Create,
            },
        );
        let (request, plan, attempt) = scope
            .admit(volume, &spec, "2026-11-01T00:00:04.000000000Z")
            .await;
        assert_eq!(request.context().request_epoch, 9);
        if path == "/alias" {
            let wire: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
            for field in ["sha256", "uid", "length", "checksum"] {
                let mut bad = wire.clone();
                match field {
                    "sha256" => bad["source_data"][field] = json!(vec![0_u8; 32]),
                    "uid" => bad["source_data"][field] = json!(1),
                    "length" => bad["source_data"][field] = json!(1),
                    "checksum" => {
                        let byte = bad["source_data"][field][1].as_u64().unwrap();
                        bad["source_data"][field][1] = json!(byte ^ 1);
                    }
                    _ => unreachable!(),
                }
                let forged = NodeMutationPlan::decode(bad.to_string().as_bytes()).unwrap();
                assert!(
                    volume.stage_node_mutation(&forged).is_err(),
                    "forged {field}"
                );
                let unchanged = volume.node_mutation_absence_evidence(&plan).unwrap();
                assert_eq!(
                    unchanged.kind(),
                    crate::managed_catalog::NodeMutationAbsenceKind::Unchanged
                );
            }
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove("source_data");
            assert!(NodeMutationPlan::decode(missing.to_string().as_bytes()).is_err());
        }
        assert_eq!(old_copy.origin, CopyOrigin::Imported);
        scope.commit(volume, &plan, &attempt).await;
        let new_id = copy_id(&attempt, id);
        let new_key = copy_key(namespace, id, new_id);
        let new_reverse = crate::managed_catalog::volume_key(
            old_copy.volume_id,
            old_copy.volume_scan_id,
            namespace,
            id,
            new_id,
        );
        // Both orphan destinations and changed existing heads must be refused
        // before any visible mutation, including clearing Writing or its holds.
        for (fault_key, fault) in [
            (new_key.clone(), b"orphan-copy".to_vec()),
            (new_reverse.clone(), b"orphan-reverse".to_vec()),
            (
                head_key(namespace, id),
                encode_head(&FileHead {
                    version: 2,
                    ..old_head.clone()
                }),
            ),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            let previous = tr.get(&fault_key, false).await.unwrap().map(|v| v.to_vec());
            tr.set(&fault_key, &fault);
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
            assert_eq!(
                tr.get(&inode_key, false).await.unwrap().unwrap().as_ref(),
                old_node
            );
            assert_eq!(
                tr.get(&old_key, false).await.unwrap().unwrap().as_ref(),
                old_record
            );
            assert_eq!(
                tr.get(&fault_key, false).await.unwrap().unwrap().as_ref(),
                fault
            );
            assert!(
                tr.get(&key(namespace, scope.serial, 9, spec.operation), false)
                    .await
                    .unwrap()
                    .is_none()
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
            match previous {
                Some(value) => tr.set(&fault_key, &value),
                None => tr.clear(&fault_key),
            }
            tr.commit().await.unwrap();
        }
        if path == "/body" {
            let previous_owner = scope.owner;
            catalog
                .claim_active_owner_epoch(scope.serial, 102)
                .await
                .unwrap();
            fence(device, ReservationKey::new(3, 102)).unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(3, 102)));
            scope.owner = 102;
            assert!(matches!(
                catalog
                    .publish_node_mutation_attempt(
                        scope.route,
                        previous_owner,
                        &attempt,
                        &plan,
                        &request,
                        4 << 20
                    )
                    .await,
                Err(CatalogError::OwnerFenced)
            ));
        }
        let result = scope.publish(&request, &plan, &attempt).await;
        assert!(!result.is_directory());
        assert_eq!((result.version(), result.parent_version()), (2, 2));
        let (published_id, published_copy) = result.physical_copy().unwrap();
        assert_eq!(published_id, new_id);
        let mut expected = (*old_copy).clone();
        expected.version = 2;
        assert_eq!(expected.volume_generation, old_copy.volume_generation);
        assert_eq!(*published_copy, expected);
        assert_eq!(published_copy.origin, old_copy.origin);
        assert_eq!(published_copy.media, old_copy.media);
        let Some(ResolvedPath::File {
            head,
            copy,
            attributes,
            ..
        }) = catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("published file metadata")
        };
        assert_eq!(
            head,
            FileHead {
                version: 2,
                size: old_head.size,
                copy_id: new_id
            }
        );
        assert_eq!(*copy, expected);
        let first_attributes = attributes.unwrap();
        assert_eq!(first_attributes.digest(), result.attributes_digest());
        assert_eq!(
            first_attributes.meta().unwrap().file_uid,
            old_attributes.meta().unwrap().file_uid
        );
        assert_eq!(first_attributes.symlink(), old_attributes.symlink());
        assert!(!first_attributes.meta().unwrap().readonly);
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&old_key, false).await.unwrap().unwrap().as_ref(),
            old_record
        );
        assert_eq!(
            tr.get(&old_reverse, false).await.unwrap().unwrap().as_ref(),
            old_reverse_record
        );
        let record = tr.get(&new_key, false).await.unwrap().unwrap().to_vec();
        let mut reverse = 2_u64.to_be_bytes().to_vec();
        reverse.extend_from_slice(&Sha256::digest(&record));
        assert_eq!(
            tr.get(&new_reverse, false).await.unwrap().unwrap().as_ref(),
            reverse
        );
        let next = scope.spec(
            path,
            Some(2),
            NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
        );
        let (next_request, next_plan, next_attempt) = scope
            .admit(volume, &next, "2026-11-01T00:00:05.000000000Z")
            .await;
        scope.commit(volume, &next_plan, &next_attempt).await;
        let latest = scope
            .publish(&next_request, &next_plan, &next_attempt)
            .await;
        assert_eq!(latest.version(), 3);
        assert_eq!(latest.physical_copy().unwrap().1.origin, old_copy.origin);
        assert_eq!(scope.publish(&request, &plan, &attempt).await, result);
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
            Some(result)
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&new_key, false).await.unwrap().unwrap().as_ref(),
            record
        );
        assert_eq!(
            tr.get(&old_key, false).await.unwrap().unwrap().as_ref(),
            old_record
        );
        if let Some(saved_read) = saved_read {
            let read = catalog
                .resolve_active_read_source_for_owner(scope.serial, &full, scope.route, scope.owner)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read.source, saved_read.source);
            assert_ne!(read.source.generation, latest.current().generation);
            let cache = root.join(format!("file-{}.sqlite", Uuid::new_v4()));
            VolumeExtentCache::build_verified_from_media(
                &crate::tape::commands::TapeDrive::new(device),
                volume.label(),
                &crate::ltfs::recovery::RecoveryBudget::default(),
                &cache,
                ExpectedIndex {
                    volume_uuid: read.source.volume_uuid,
                    generation: read.source.generation,
                    location: read.source.location,
                    sha256: read.source.sha256,
                },
                read.source.incremental,
            )
            .unwrap();
            let read = read.plan_read(&cache).unwrap();
            assert_eq!(read.read.head.version, 3);
            assert_eq!(read.read.physical.extents, saved_read.read.physical.extents);
            for physical in [&saved_read.read.physical, &read.read.physical] {
                let mut bytes = Vec::new();
                crate::ltfs::volume::read_cached_file_to_writer(
                    device,
                    volume.label(),
                    physical,
                    &mut bytes,
                )
                .unwrap();
                assert_eq!(bytes, b"immutable original body");
            }
        } else {
            assert!(
                catalog
                    .resolve_active_read_source_for_owner(
                        scope.serial,
                        &full,
                        scope.route,
                        scope.owner
                    )
                    .await
                    .is_err()
            );
            assert_eq!(latest.physical_copy().unwrap().1.size, 0);
            assert_eq!(
                latest.physical_copy().unwrap().1.content_digest,
                ContentDigest::Unknown
            );
            assert!(
                latest
                    .physical_copy()
                    .unwrap()
                    .1
                    .media
                    .as_ref()
                    .unwrap()
                    .native_symlink
            );
        }
    }
    let physical_after = volume.index().find_file("/body").unwrap();
    assert_eq!(physical_after.extents, physical_before.extents);
    assert_eq!(physical_after.length, physical_before.length);
    assert_eq!(physical_after.meta.file_uid, physical_before.meta.file_uid);
    assert_eq!(
        ContentDigest::from_ltfs_xattrs(&physical_after.xattrs).unwrap(),
        ContentDigest::from_ltfs_xattrs(&physical_before.xattrs).unwrap()
    );
    assert!(
        physical_after
            .xattrs
            .iter()
            .any(|attr| attr.key == "foreign.original" && attr.value == "kept")
    );
}
