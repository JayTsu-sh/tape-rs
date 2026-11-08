//! Atomic physical-index publication. No logical file head, source coverage
//! or erase permission is produced. History uses an independent immutable row.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    namespace: String,
    library: String,
    pool: String,
    task: Record,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataPublication {
    wire: Wire,
    observation: ReclaimMetadataObservation,
}
impl ReclaimMetadataPublication {
    pub fn id(&self) -> Uuid {
        Uuid::parse_str(&self.wire.task.id).expect("validated metadata publication")
    }
    pub fn plan(&self) -> ReclaimMetadataPlan {
        ReclaimMetadataPlan(self.wire.task.plan.clone())
    }
    pub fn observation(&self) -> &ReclaimMetadataObservation {
        &self.observation
    }
    pub fn observed_owner(&self) -> u64 {
        self.wire.task.owner
    }
    fn from_wire(wire: Wire) -> Result<Self> {
        node_mutation::id(&wire.namespace)?;
        node_mutation::id(&wire.pool)?;
        validate_serial(&wire.library)?;
        let task = ReclaimMetadataWrite(wire.task.clone());
        task.encode()?;
        if wire.version != 1 || !task.completed() {
            return Err(CatalogError::Corrupt("元数据发布回执缺少原提交终态"));
        }
        let observation = task.observation()?.ok_or(CatalogError::Incomplete)?;
        Ok(Self { wire, observation })
    }
    fn checksum(raw: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-publication/1");
        hash.update(raw);
        hash.finalize().into()
    }
    fn encode(&self) -> Result<Vec<u8>> {
        if Self::from_wire(self.wire.clone())? != *self {
            return Err(CatalogError::Corrupt("元数据发布回执与原观察不符"));
        }
        let mut raw = serde_json::to_vec(&self.wire).map_err(cache)?;
        if raw.len() > 65_000 {
            return Err(CatalogError::Invalid("元数据发布回执超出预算"));
        }
        raw.extend_from_slice(&Self::checksum(&raw));
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < 32 || raw.len() > 65_032 {
            return Err(CatalogError::Corrupt("元数据发布回执长度无效"));
        }
        let (body, sum) = raw.split_at(raw.len() - 32);
        if Self::checksum(body) != sum {
            return Err(CatalogError::Corrupt("元数据发布回执完整性校验失败"));
        }
        let value = Self::from_wire(serde_json::from_slice(body).map_err(cache)?)?;
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("元数据发布回执不是规范编码"));
        }
        Ok(value)
    }
}

fn receipt_key(namespace: Uuid, index: ActiveIndexIdentity) -> Vec<u8> {
    let mut key = domain(namespace, b'H');
    key.extend_from_slice(index.volume_uuid.as_bytes());
    key.extend_from_slice(&index.generation.to_be_bytes());
    key
}
async fn at(
    tr: &Transaction,
    namespace: Uuid,
    index: ActiveIndexIdentity,
) -> Result<Option<ReclaimMetadataPublication>> {
    tr.get(&receipt_key(namespace, index), false)
        .await?
        .map(|raw| ReclaimMetadataPublication::decode(&raw))
        .transpose()
}

pub(super) async fn require_empty_result_at(
    tr: &Transaction,
    namespace: Uuid,
    volume: Uuid,
    generation: u64,
) -> Result<()> {
    let mut key = domain(namespace, b'H');
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    if tr.get(&key, false).await?.is_some() {
        return Err(CatalogError::Corrupt(
            "新元数据代次已有结果回执，不得覆盖或关闭",
        ));
    }
    active_volume::require_empty_generation(tr, volume, generation).await
}

/// A retained physical history must not depend on the current parent/ledger.
pub(in crate::managed_catalog) async fn require_history_at(
    tr: &Transaction,
    serial: &str,
    id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let placement = verified_placement(tr, index.volume_uuid)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let value = at(tr, placement.namespace, index)
        .await?
        .ok_or(CatalogError::Corrupt("元数据历史索引缺少独立发布回执"))?;
    if value.id() != id
        || value.observation.current() != index
        || value.wire.namespace != placement.namespace.to_string()
        || value.wire.library != serial
        || value.wire.library != placement.library_serial
        || value.wire.pool != placement.pool_id.to_string()
        || value.wire.task.scan
            != require_volume_ready(tr, index.volume_uuid, None)
                .await?
                .id
                .to_string()
        || active_volume::stored_file_count_at(tr, index).await?
            != Some(value.observation.physical_files())
    {
        return Err(CatalogError::Corrupt(
            "元数据历史索引与原发布回执或计数不同",
        ));
    }
    Ok(())
}

pub(super) async fn require_task_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataPublication> {
    let observation = task.observation()?.ok_or(CatalogError::Incomplete)?;
    let value = at(tr, reservation.namespace(), observation.current())
        .await?
        .ok_or(CatalogError::Corrupt("已完成元数据任务缺少原发布回执"))?;
    if value.id() != task.id()
        || value.plan() != task.plan()
        || value.wire.task.scan != task.0.scan
        || value.observation != observation
        || value.observed_owner() > task.owner()
        || value.wire.namespace != reservation.namespace().to_string()
        || value.wire.library != reservation.serial()
        || value.wire.pool != reservation.spec().pool.to_string()
        || Box::pin(active_volume::history_at(
            tr,
            reservation.serial(),
            task.plan().volume(),
            observation.current().generation,
        ))
        .await?
            != (
                active_volume::IndexKind::ReclaimMetadata(task.id()),
                observation.current(),
            )
    {
        return Err(CatalogError::Corrupt("原元数据终态与发布回执或历史不符"));
    }
    Ok(value)
}

pub(super) fn require_original(
    original: &ReclaimMetadataWrite,
    current: &ReclaimMetadataWrite,
    owner: u64,
) -> Result<()> {
    if original.owner() != owner || current.owner() != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if original.plan() != current.plan() {
        return Err(CatalogError::Invalid("元数据发布改变原固定计划"));
    }
    require_phase_history(original, current)
}

impl ManagedCatalog {
    /// Resolve a completed original attempt without a device command. Mutable
    /// write policy cannot erase a retained physical result.
    pub async fn reclaim_metadata_publication(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
    ) -> Result<Option<ReclaimMetadataPublication>> {
        request.validate()?;
        original.encode()?;
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let task = Box::pin(load_id_at(
            &tr,
            reservation,
            original.plan().source_uid(),
            original.id(),
        ))
        .await?
        .ok_or(CatalogError::Incomplete)?;
        if original.plan() != task.plan() || original.owner() > owner || task.owner() > owner {
            return Err(CatalogError::Invalid("历史元数据发布改变原计划或 owner"));
        }
        require_phase_history(original, &task)?;
        if !task.completed() {
            return Ok(None);
        }
        input_at(&tr, reservation, owner, file, request, &task.plan()).await?;
        Ok(Some(require_task_at(&tr, reservation, &task).await?))
    }

    /// Publish the actual fixed-node S4, its history/count, terminal task and
    /// exact own-claim release atomically. This does not cover the whole source.
    pub async fn publish_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
        proof: &ReclaimMetadataCommitEvidence,
    ) -> Result<ReclaimMetadataPublication> {
        request.validate()?;
        original.encode()?;
        let evidence = proof.require(original, owner)?;
        evidence.encode(&original.plan())?;
        if evidence.kind() != ReclaimMetadataObservationKind::Committed {
            return Err(CatalogError::Invalid(
                "未观察到原元数据提交，不能发布或释放",
            ));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let mut task = Box::pin(load_id_at(
                                &tr,
                                reservation,
                                plan.source_uid(),
                                original.id(),
                            ))
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            require_original(original, &task, owner)?;
                            if task.observation()?.as_ref() != Some(evidence) {
                                return Err(CatalogError::Invalid("发布改变或绕过原已保存观察"));
                            }
                            // Replays retain the original child request as well as
                            // the physical attempt; terminal state cannot bypass it.
                            input_at(&tr, reservation, owner, file, request, &plan).await?;
                            if task.completed() {
                                return require_task_at(&tr, reservation, &task).await;
                            }
                            if !task.writing() {
                                return Err(CatalogError::Invalid("只有原 Writing 任务可以发布"));
                            }
                            let (scan, previous) = active_volume::observed_at(
                                &tr,
                                reservation.serial(),
                                plan.volume(),
                            )
                            .await?;
                            if scan.id.to_string() != task.0.scan
                                || previous
                                    != active_volume::decode_index(plan.volume(), &plan.0.previous)?
                                || active_volume::file_count_at(
                                    &tr,
                                    reservation.serial(),
                                    plan.volume(),
                                )
                                .await?
                                    != plan.0.physical_files
                                || active_volume::recovery_active(&tr, plan.volume()).await?
                            {
                                return Err(CatalogError::Corrupt(
                                    "发布时原目标卷头、计数、扫描或结果槽改变",
                                ));
                            }
                            require_empty_result_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                                evidence.current().generation,
                            )
                            .await?;
                            task.0.phase = Phase::Committed;
                            let value = ReclaimMetadataPublication::from_wire(Wire {
                                version: 1,
                                namespace: reservation.namespace().to_string(),
                                library: reservation.serial().into(),
                                pool: reservation.spec().pool.to_string(),
                                task: task.0.clone(),
                            })?;
                            let receipt = value.encode()?;
                            let raw = task.encode()?;
                            let header = registry::encode(reservation, &task)?;
                            let task_key = task_key(reservation, &task);
                            let receipt_key =
                                receipt_key(reservation.namespace(), evidence.current());
                            // All fallible validation and encoding precedes the first set.
                            tr.set(&receipt_key, &receipt);
                            active_volume::set_reclaim_observation_at(
                                &tr,
                                previous,
                                evidence.current(),
                                task.id(),
                                evidence.physical_files(),
                            );
                            tr.set(&task_key, &raw);
                            tr.set(&registry::key(reservation), &header);
                            tr.clear(&ledger::own_key(reservation, b'A'));
                            tr.clear(&node_attempts::volume_key(plan.volume()));
                            Ok(value)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
