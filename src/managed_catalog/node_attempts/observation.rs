//! A durable node S4 observation is separate from both the Writing header and
//! logical publication. Historical reads must survive later task retirement.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationObservation {
    attempt: NodeMutationAttempt,
    current: ActiveIndexIdentity,
    file_count: u64,
    observed_owner: u64,
    observed_at_millis: u64,
}
impl NodeMutationObservation {
    pub fn attempt(&self) -> &NodeMutationAttempt {
        &self.attempt
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn observed_owner(&self) -> u64 {
        self.observed_owner
    }
    pub(super) fn observed_at_millis(&self) -> u64 {
        self.observed_at_millis
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    attempt: String,
    current: String,
    file_count: u64,
    observed_owner: u64,
    observed_at_millis: u64,
}
fn key(namespace: Uuid, volume: Uuid, generation: u64) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b's');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    key
}
fn encode(value: &NodeMutationObservation) -> Result<Vec<u8>> {
    let before = value.attempt.previous;
    if value.current.volume_uuid != before.volume_uuid
        || before.generation.checked_add(1) != Some(value.current.generation)
        || value.current.location.partition != 'b'
        || value.current.location.start_block <= before.location.start_block
        || value.observed_owner < value.attempt.owner
        || !(value.attempt.created_at_millis..=253_402_300_799_999)
            .contains(&value.observed_at_millis)
    {
        return Err(CatalogError::Corrupt("节点 S4 观察身份、位置或时间不符"));
    }
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(before.volume_uuid, &current)?;
    let bytes = node_mutation::canonical(&Wire {
        version: 1,
        attempt: URL_SAFE_NO_PAD.encode(super::encode(&value.attempt)?),
        current: URL_SAFE_NO_PAD.encode(current),
        file_count: value.file_count,
        observed_owner: value.observed_owner,
        observed_at_millis: value.observed_at_millis,
    })?;
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点 S4 观察记录过长"));
    }
    Ok(bytes)
}
fn decode(bytes: &[u8]) -> Result<NodeMutationObservation> {
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点 S4 观察记录过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点 S4 观察编码"))?;
    let attempt = super::decode(
        &URL_SAFE_NO_PAD
            .decode(&wire.attempt)
            .map_err(|_| CatalogError::Corrupt("节点 S4 尝试引用编码"))?,
    )?;
    let current = active_volume::decode_index(
        attempt.previous.volume_uuid,
        &URL_SAFE_NO_PAD
            .decode(&wire.current)
            .map_err(|_| CatalogError::Corrupt("节点 S4 索引编码"))?,
    )?;
    let value = NodeMutationObservation {
        attempt,
        current,
        file_count: wire.file_count,
        observed_owner: wire.observed_owner,
        observed_at_millis: wire.observed_at_millis,
    };
    if wire.version != 1 || encode(&value)? != bytes {
        return Err(CatalogError::Corrupt("节点 S4 观察必须为规范版本1"));
    }
    Ok(value)
}
pub(super) async fn at(
    tr: &Transaction,
    attempt: &NodeMutationAttempt,
) -> Result<Option<NodeMutationObservation>> {
    let raw = tr
        .get(
            &key(
                attempt.namespace,
                attempt.previous.volume_uuid,
                attempt.previous.generation + 1,
            ),
            false,
        )
        .await?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = decode(&raw)?;
    if value.attempt != *attempt {
        return Err(CatalogError::Corrupt("节点 S4 观察与原尝试不符"));
    }
    Ok(Some(value))
}

/// The observation contains the immutable original header. It deliberately
/// does not require an active task or retained plan chunks for historical I/O.
pub(in crate::managed_catalog) async fn require_observation_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let raw = tr
        .get(
            &key(placement.namespace, index.volume_uuid, index.generation),
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("节点历史索引缺少 S4 观察"))?;
    let value = decode(&raw)?;
    let attempt = &value.attempt;
    if value.current != index
        || attempt.id != id
        || attempt.library != serial
        || attempt.library != placement.library_serial
        || attempt.namespace != placement.namespace
        || attempt.pool != placement.pool_id
        || require_volume_ready(tr, index.volume_uuid, None).await?.id != attempt.scan
        || active_volume::stored_file_count_at(tr, index).await? != Some(value.file_count)
    {
        return Err(CatalogError::Corrupt("节点历史索引与 S4 观察或文件数不符"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Record a real, PR-checked node S4. No file receipt/batch is synthesized,
    /// and no logical node, version, task state or pending hold is published.
    pub async fn observe_node_mutation_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
        plan: &NodeMutationPlan,
        evidence: &NodeMutationEvidence,
    ) -> Result<NodeMutationObservation> {
        if !attempt.matches_plan(plan)
            || evidence.fingerprint() != attempt.object.digest()
            || evidence.context() != plan.context()
        {
            return Err(CatalogError::Invalid("节点 S4 证明与原物理尝试不符"));
        }
        let now = management::now().max(attempt.created_at_millis);
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            256_000,
                        ))?;
                        let task = current_task_at(&tr, route, owner, plan).await?;
                        if require_task_at(&tr, &task).await? != *attempt {
                            return Err(CatalogError::Invalid("节点物理尝试已改变"));
                        }
                        let placement = verified_placement(&tr, attempt.previous.volume_uuid)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if placement.namespace != attempt.namespace
                            || placement.pool_id != attempt.pool
                            || placement.library_serial != attempt.library
                        {
                            return Err(CatalogError::Corrupt("节点 S4 卷归属已改变"));
                        }
                        let (scan, current) = active_volume::observed_at(
                            &tr,
                            &attempt.library,
                            attempt.previous.volume_uuid,
                        )
                        .await?;
                        if scan.id != attempt.scan {
                            return Err(CatalogError::Corrupt("节点 S4 原扫描已改变"));
                        }
                        if let Some(saved) = at(&tr, attempt).await? {
                            if saved.current != evidence.current()
                                || saved.file_count != evidence.file_count()
                                || current != saved.current
                                || active_volume::history_at(
                                    &tr,
                                    &attempt.library,
                                    current.volume_uuid,
                                    current.generation,
                                )
                                .await?
                                .0 != active_volume::IndexKind::NodeMutation(attempt.id)
                            {
                                return Err(CatalogError::Corrupt("节点 S4 重试与已观察结果不符"));
                            }
                            return Ok(saved);
                        }
                        if current != attempt.previous {
                            return Err(CatalogError::Invalid("节点 S4 前代与当前游标不符"));
                        }
                        let current = evidence.current();
                        let count = active_volume::file_count_at(
                            &tr,
                            &attempt.library,
                            current.volume_uuid,
                        )
                        .await?;
                        let expected_count = match plan.mutation() {
                            NodeMutation::CreateSymlink { .. } => count.checked_add(1),
                            NodeMutation::Unlink => count.checked_sub(1),
                            _ => Some(count),
                        }
                        .ok_or(CatalogError::Corrupt("节点 S4 文件数变化溢出"))?;
                        if expected_count != evidence.file_count() {
                            return Err(CatalogError::Corrupt("节点 S4 实际文件数与变更不符"));
                        }
                        if active_volume::recovery_active(&tr, current.volume_uuid).await?
                            || tr
                                .get(
                                    &active_batch::batch_key(
                                        current.volume_uuid,
                                        current.generation,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                            || tr
                                .get(
                                    &active_volume::generation_key(
                                        current.volume_uuid,
                                        current.generation,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                            || active_volume::stored_file_count_at(&tr, current)
                                .await?
                                .is_some()
                        {
                            return Err(CatalogError::Corrupt("节点 S4 目标代数与其他记录冲突"));
                        }
                        let value = NodeMutationObservation {
                            attempt: attempt.clone(),
                            current,
                            file_count: evidence.file_count(),
                            observed_owner: owner,
                            observed_at_millis: now,
                        };
                        let encoded = encode(&value)?;
                        let target =
                            key(attempt.namespace, current.volume_uuid, current.generation);
                        // All fallible work precedes this single atomic observation.
                        active_volume::set_node_observation_at(
                            &tr,
                            attempt.previous,
                            current,
                            attempt.id,
                            value.file_count,
                        );
                        tr.set(&target, &encoded);
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Discover an already observed result before any replay. The active
    /// ownership check is current; the saved observation owner/time stay fixed.
    pub async fn node_mutation_observation(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
    ) -> Result<Option<NodeMutationObservation>> {
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_runtime_active_owner(&tr, &attempt.library, route, owner).await?;
        if namespace != attempt.namespace {
            return Err(CatalogError::Invalid("节点观察与活动命名空间不符"));
        }
        node_tasks::retained(&tr, namespace, attempt.epoch).await?;
        if let Some(closed) = closure::at(&tr, attempt).await? {
            closure::require_correlation_at(&tr, &closed).await?;
            return Ok(None);
        }
        let value = at(&tr, attempt).await?;
        let generation = attempt.previous.generation + 1;
        if tr
            .get(
                &active_volume::generation_key(attempt.previous.volume_uuid, generation),
                false,
            )
            .await?
            .is_some()
        {
            let (kind, index) = active_volume::history_at(
                &tr,
                &attempt.library,
                attempt.previous.volume_uuid,
                generation,
            )
            .await?;
            if let Some(value) = &value {
                if kind != active_volume::IndexKind::NodeMutation(attempt.id)
                    || index != value.current
                {
                    return Err(CatalogError::Corrupt("节点 S4 观察缺少对应历史身份"));
                }
                let (_, current) =
                    active_volume::observed_at(&tr, &attempt.library, index.volume_uuid).await?;
                if current.generation < index.generation {
                    return Err(CatalogError::Corrupt("节点 S4 观察尚未关联当前游标"));
                }
            }
        } else if value.is_some() {
            return Err(CatalogError::Corrupt("节点 S4 观察缺少历史记录"));
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::ltfs::volume::LtfsVolume;
    use crate::scsi::reservation::{ReservationKey, fence};
    use crate::scsi::sim::{SimCartridge, SimLibrary};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn observation_codec_is_canonical_scoped_and_keeps_the_original_attempt() {
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
        // Metadata codec fixture only; it is never a device commit proof.
        let value = NodeMutationObservation {
            attempt: NodeMutationAttempt {
                id: Uuid::new_v4(),
                library: "OBSERVATION-CODEC".into(),
                namespace,
                pool: Uuid::new_v4(),
                epoch: 1,
                operation: Uuid::new_v4(),
                logical_fingerprint: [5; 32],
                owner: 100,
                created_at_millis: 1_798_000_000_000,
                object: NodePlanObject::for_bytes("OBSERVATION-CODEC", namespace, b"inert", 100)
                    .unwrap(),
                scan: Uuid::new_v4(),
                previous,
                pool_revision: 1,
                assignment_revision: 2,
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
            file_count: u64::MAX,
            observed_owner: 101,
            observed_at_millis: 1_798_000_000_001,
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
            ("attempt", json!("!")),
            ("current", json!("!")),
            ("observed_owner", json!(99)),
            ("observed_at_millis", json!(1_797_999_999_999_u64)),
            ("observed_at_millis", json!(253_402_300_800_000_u64)),
        ] {
            let mut bad_wire = wire.clone();
            bad_wire[field] = bad;
            assert!(decode(bad_wire.to_string().as_bytes()).is_err(), "{field}");
        }
        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(decode(&noncanonical).is_err());
        assert!(decode(&vec![b' '; 2 * MAX_RECORD + 1]).is_err());
        for current in [
            ActiveIndexIdentity {
                volume_uuid: Uuid::new_v4(),
                ..value.current
            },
            ActiveIndexIdentity {
                generation: 4,
                ..value.current
            },
            ActiveIndexIdentity {
                location: previous.location,
                ..value.current
            },
        ] {
            assert!(
                encode(&NodeMutationObservation {
                    current,
                    ..value.clone()
                })
                .is_err()
            );
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim PR/S4 节点观察，不发布逻辑节点，不接触硬件"]
    async fn node_s4_observation_is_atomic_invisible_typed_and_recoverable() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        for (scenario, observed_before_takeover) in
            (0..4).flat_map(|scenario| [false, true].map(|observed| (scenario, observed)))
        {
            let namespace = Uuid::new_v4();
            let serial = format!("SIM-NODE-S4-{namespace}");
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
                name: "node-s4".into(),
                file_limit: if scenario == 3 { 1 } else { 200_000 },
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
                .insert_cartridge(SimCartridge::blank("OBSV01L8", 64 << 20), 0)
                .unwrap();
            library.load_into_drive("OBSV01L8", 0).unwrap();
            let device = library.drive(0);
            mkltfs(
                &device,
                &MkltfsOptions {
                    volume_id: "OBSV01".into(),
                    block_size: 64 << 10,
                    ..Default::default()
                },
            )
            .unwrap();
            let guard = ReservationKey::new(1, 100);
            fence(&device, guard).unwrap();
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume.set_reservation_guard(Some(guard));
            if scenario >= 2 {
                volume.add_symlink("/existing", "target").unwrap();
                volume.commit().unwrap();
            }
            let previous: ActiveIndexIdentity =
                volume.verified_committed_index_construct().unwrap().into();
            assert!(!previous.incremental);
            // Use only bytes identical to the real committed construct. This
            // is a Sim source fixture, not a fabricated empty file receipt.
            let xml = volume.index().to_xml().unwrap();
            assert_eq!(<[u8; 32]>::from(Sha256::digest(&xml)), previous.sha256);
            let root = std::env::temp_dir().join(format!("node-s4-source-{}", Uuid::new_v4()));
            eprintln!("node_s4_workspace={}", root.display());
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
            let source_files = u64::from(scenario >= 2);
            assert_eq!((source.files, source.paths), (source_files, source_files));
            let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"OBSV01L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
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
            let spec = NodeMutationSpec {
                pool: pool.id,
                operation: Uuid::new_v4(),
                path: if scenario == 2 {
                    "/existing"
                } else {
                    "/created"
                }
                .into(),
                expected_version: if scenario == 2 { Some(1) } else { None },
                mutation: match scenario {
                    0 => NodeMutation::CreateDirectory,
                    2 => NodeMutation::Unlink,
                    _ => NodeMutation::CreateSymlink {
                        target: "target".into(),
                    },
                },
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
            catalog
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
                    expected_version: spec.expected_version,
                    scan: intake.scan(),
                    previous,
                    path: spec.path.clone(),
                },
                spec.mutation.clone(),
                volume.index(),
                request.time(),
                &inputs.ancestors,
                match &inputs.baseline.current {
                    Some(ResolvedPath::File { attributes, .. }) => {
                        attributes.as_ref().map(|attrs| (NodeKind::File, attrs))
                    }
                    _ => None,
                },
            )
            .unwrap();
            if scenario == 3 {
                assert!(matches!(
                    catalog
                        .admit_node_mutation_attempt(
                            route,
                            100,
                            Uuid::new_v4(),
                            &inputs,
                            &plan,
                            1 << 20
                        )
                        .await,
                    Err(CatalogError::Invalid("新建链接超过池的磁带文件数上限"))
                ));
                assert_eq!(
                    catalog
                        .node_mutation_task(&serial, route, 1, spec.operation)
                        .await
                        .unwrap()
                        .unwrap()
                        .state,
                    NodeMutationTaskState::Prepared
                );
                assert!(
                    catalog
                        .pending_node_mutation_attempt(&serial, route, 100, previous.volume_uuid)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(volume.index().find_file("/created").is_none());
                drop(volume);
                drop(cache);
                drop(spool);
                std::fs::remove_dir_all(root).unwrap();
                continue;
            }
            let attempt = catalog
                .admit_node_mutation_attempt(route, 100, Uuid::new_v4(), &inputs, &plan, 1 << 20)
                .await
                .unwrap();
            assert!(
                catalog
                    .node_mutation_observation(route, 100, &attempt)
                    .await
                    .unwrap()
                    .is_none()
            );
            volume.stage_node_mutation(&plan).unwrap();
            assert!(volume.node_mutation_evidence(&plan).is_err());
            volume.sync_batch().unwrap();
            let evidence = volume.node_mutation_evidence(&plan).unwrap();
            let expected_files = u64::from(scenario == 1);
            assert_eq!(evidence.file_count(), expected_files);
            match scenario {
                0 => assert!(volume.index().find_directory("/created").is_some()),
                1 => assert_eq!(
                    volume
                        .index()
                        .find_file("/created")
                        .unwrap()
                        .symlink
                        .as_deref(),
                    Some("target")
                ),
                2 => assert!(volume.index().find_file("/existing").is_none()),
                _ => unreachable!(),
            }
            let observation_key = key(
                namespace,
                previous.volume_uuid,
                evidence.current().generation,
            );
            let history_key =
                active_volume::generation_key(previous.volume_uuid, evidence.current().generation);
            let mut count_key = history_key.clone();
            count_key.push(b'f');
            let cursor_key = history_key[..history_key.len() - 8].to_vec();
            let observation_keys = [&observation_key, &history_key, &count_key, &cursor_key];
            // Conflicting target rows must not create a partial observation.
            for conflict in [&history_key, &count_key] {
                let tr = catalog.db.create_trx().unwrap();
                assert!(tr.get(conflict, false).await.unwrap().is_none());
                tr.set(conflict, &[0]);
                tr.commit().await.unwrap();
                let tr = catalog.db.create_trx().unwrap();
                let mut before = Vec::new();
                for key in observation_keys {
                    before.push(tr.get(key, false).await.unwrap().map(|v| v.to_vec()));
                }
                assert!(matches!(
                    catalog
                        .observe_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                        .await,
                    Err(CatalogError::Corrupt(_))
                ));
                let tr = catalog.db.create_trx().unwrap();
                for (key, bytes) in observation_keys.into_iter().zip(before) {
                    assert_eq!(tr.get(key, false).await.unwrap().map(|v| v.to_vec()), bytes);
                }
                tr.clear(conflict);
                tr.commit().await.unwrap();
            }
            let first = if observed_before_takeover {
                let observation = catalog
                    .observe_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                    .await
                    .unwrap();
                assert_eq!(observation.observed_owner(), 100);
                assert_eq!(
                    catalog
                        .observe_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                        .await
                        .unwrap(),
                    observation
                );
                Some(observation)
            } else {
                None
            };
            catalog
                .claim_active_owner_epoch(&serial, 101)
                .await
                .unwrap();
            fence(&device, ReservationKey::new(2, 101)).unwrap();
            assert!(volume.node_mutation_evidence(&plan).is_err());
            assert!(matches!(
                catalog
                    .observe_node_mutation_attempt(route, 100, &attempt, &plan, &evidence)
                    .await,
                Err(CatalogError::OwnerFenced)
            ));
            drop(volume);
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
            let saved_plan = catalog
                .load_node_mutation_attempt_plan(route, 101, &attempt, 1 << 20)
                .await
                .unwrap();
            assert_eq!(saved_plan.encode().unwrap(), plan.encode().unwrap());
            let evidence = volume.node_mutation_evidence(&saved_plan).unwrap();
            let observation = catalog
                .observe_node_mutation_attempt(route, 101, &attempt, &saved_plan, &evidence)
                .await
                .unwrap();
            if let Some(first) = first {
                assert_eq!(observation, first);
            } else {
                assert_eq!(observation.observed_owner(), 101);
            }
            assert_eq!(
                catalog
                    .node_mutation_observation(route, 101, &attempt)
                    .await
                    .unwrap(),
                Some(observation.clone())
            );
            assert_eq!(
                catalog
                    .observed_active_volume_index(&serial, previous.volume_uuid)
                    .await
                    .unwrap(),
                evidence.current()
            );
            assert_eq!(
                catalog
                    .active_volume_file_count(&serial, route, 101, previous.volume_uuid)
                    .await
                    .unwrap(),
                expected_files
            );
            let tr = catalog.db.create_trx().unwrap();
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
            assert_eq!(
                active_volume::observed_copy_index_at(
                    &tr,
                    &serial,
                    previous.volume_uuid,
                    evidence.current().generation
                )
                .await
                .unwrap(),
                Some(evidence.current())
            );
            assert_eq!(
                catalog
                    .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                    .await
                    .unwrap(),
                Some(attempt.clone())
            );
            assert!(matches!(
                catalog
                    .begin_active_volume_recovery(&serial, 101, evidence.current())
                    .await,
                Err(CatalogError::PoolBusy)
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
            let path = pool_catalog_path(pool.id, spec.path.as_bytes()).unwrap();
            let logical = catalog
                .resolve_active_path_for_route(&serial, &path, route)
                .await
                .unwrap();
            assert_eq!(logical.is_some(), scenario == 2);
            // The generation, observation, count and cursor are correlated.
            // An error must leave each deliberately damaged test row untouched.
            for damaged in observation_keys {
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
                            .node_mutation_observation(route, 101, &attempt)
                            .await,
                        Err(CatalogError::Corrupt(_))
                    ));
                    assert!(matches!(
                        catalog
                            .observe_node_mutation_attempt(
                                route,
                                101,
                                &attempt,
                                &saved_plan,
                                &evidence
                            )
                            .await,
                        Err(CatalogError::Corrupt(_))
                    ));
                    let tr = catalog.db.create_trx().unwrap();
                    assert_eq!(
                        tr.get(damaged, false).await.unwrap().map(|v| v.to_vec()),
                        replacement
                    );
                    tr.set(damaged, &original);
                    tr.commit().await.unwrap();
                }
            }
            let tr = catalog.db.create_trx().unwrap();
            let original = tr.get(&history_key, false).await.unwrap().unwrap().to_vec();
            let mut wrong_kind = original.clone();
            wrong_kind[0] = 1; // Same UUID and index, different protocol.
            tr.set(&history_key, &wrong_kind);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog
                    .node_mutation_observation(route, 101, &attempt)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            assert!(matches!(
                catalog
                    .observe_node_mutation_attempt(route, 101, &attempt, &saved_plan, &evidence)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                tr.get(&history_key, false).await.unwrap().unwrap().as_ref(),
                wrong_kind
            );
            tr.set(&history_key, &original);
            tr.commit().await.unwrap();
            // Historical serving proof is self-contained. Removing the live
            // attempt header must break pending recovery, but not history reads.
            let record = attempt_key(&attempt);
            let tr = catalog.db.create_trx().unwrap();
            let original = tr.get(&record, false).await.unwrap().unwrap().to_vec();
            tr.clear(&record);
            tr.commit().await.unwrap();
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                active_volume::observed_copy_index_at(
                    &tr,
                    &serial,
                    previous.volume_uuid,
                    evidence.current().generation
                )
                .await
                .unwrap(),
                Some(evidence.current())
            );
            assert!(matches!(
                catalog
                    .pending_node_mutation_attempt(&serial, route, 101, previous.volume_uuid)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert!(tr.get(&record, false).await.unwrap().is_none());
            tr.set(&record, &original);
            tr.commit().await.unwrap();
            drop(volume);
            drop(cache);
            drop(spool);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
