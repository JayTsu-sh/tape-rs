//! Durable preparation only. No execution admission exists in this kernel.
//! Prepared can therefore be closed without claiming any device result.
use super::*;
use metadata_plan::Plan;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    plan: Plan,
    owner: u64,
    phase: Phase,
}

/// A retained fixed plan, never a device I/O permit or migration receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataWrite(Record);

impl ReclaimMetadataWrite {
    pub fn plan(&self) -> ReclaimMetadataPlan {
        ReclaimMetadataPlan(self.0.plan.clone())
    }
    pub fn owner(&self) -> u64 {
        self.0.owner
    }
    pub fn closed(&self) -> bool {
        self.0.phase == Phase::Closed
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.plan().encode()?;
        if self.0.version != 1 || self.0.owner == 0 || self.0.owner > 0xFFFF_FFFF_FFFF {
            return Err(CatalogError::Corrupt("回收元数据任务版本或 owner 无效"));
        }
        let raw = serde_json::to_vec(&self.0).map_err(cache)?;
        if raw.len() > 60_000 {
            return Err(CatalogError::Invalid("回收元数据任务超出预算"));
        }
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() > 60_000 {
            return Err(CatalogError::Corrupt("回收元数据任务过大"));
        }
        let value = Self(serde_json::from_slice(raw).map_err(cache)?);
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("回收元数据任务编码不规范"));
        }
        Ok(value)
    }
}

fn latest_key(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'T');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn task_prefix(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'W');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn task_key(reservation: &ReclaimReservation, plan: &ReclaimMetadataPlan) -> Result<Vec<u8>> {
    let mut key = task_prefix(reservation, plan.0.source.uid);
    key.extend_from_slice(&plan.fingerprint()?);
    Ok(key)
}
fn claim(reservation: &ReclaimReservation, plan: &ReclaimMetadataPlan) -> Result<Vec<u8>> {
    node_claims::encode(&node_claims::NodeVolumeClaim::ReclaimMetadata(Box::new(
        node_claims::ReclaimMetadataVolumeClaim {
            namespace: reservation.namespace().to_string(),
            volume: plan.0.volume.clone(),
            task: task_key(reservation, plan)?,
            fingerprint: plan.fingerprint()?,
        },
    )))
}

pub(in super::super) async fn require_idle_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    if tr
        .get(&ledger::own_key(reservation, b'A'), false)
        .await?
        .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

async fn parent_at(tr: &Transaction, reservation: &ReclaimReservation, owner: u64) -> Result<()> {
    if record_at(tr, reservation, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}

async fn load_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    uid: u64,
) -> Result<Option<ReclaimMetadataWrite>> {
    if uid == 0 {
        return Err(CatalogError::Invalid("回收元数据源 UID 不能为零"));
    }
    let Some(fp) = tr.get(&latest_key(reservation, uid), false).await? else {
        if requests::prefix_has_rows(tr, &task_prefix(reservation, uid)).await? {
            return Err(CatalogError::Corrupt("回收元数据任务缺少原身份索引"));
        }
        return Ok(None);
    };
    if fp.len() != 32 {
        return Err(CatalogError::Corrupt("回收元数据任务身份长度无效"));
    }
    let mut key = task_prefix(reservation, uid);
    key.extend_from_slice(&fp);
    let raw = tr
        .get(&key, false)
        .await?
        .ok_or(CatalogError::Corrupt("回收元数据任务缺失"))?;
    let task = ReclaimMetadataWrite::decode(&raw)?;
    let plan = task.plan();
    if plan.0.parent != reservation.fingerprint()
        || plan.0.source.uid != uid
        || plan.fingerprint()?.as_slice() != fp.as_ref()
    {
        return Err(CatalogError::Corrupt("回收元数据任务与原父身份不符"));
    }
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    if plan.0.inventory != manifest.summary.digest
        || plan.0.source_index != manifest.summary.head
        || reservation.intent["index"] != json!(plan.0.source_index)
        || tr
            .get(&ledger::object_key(reservation, uid), false)
            .await?
            .as_deref()
            != Some(plan.0.source.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("回收元数据任务与原始封存清单不同"));
    }
    let expected = claim(reservation, &plan)?;
    let active = tr.get(&ledger::own_key(reservation, b'A'), false).await?;
    let held = tr
        .get(&node_attempts::volume_key(plan.volume()), false)
        .await?;
    if (!task.closed()
        && (active.as_deref() != Some(expected.as_slice())
            || held.as_deref() != Some(expected.as_slice())))
        || (task.closed()
            && (active.as_deref() == Some(expected.as_slice())
                || held.as_deref() == Some(expected.as_slice())))
    {
        return Err(CatalogError::Corrupt("回收元数据任务与卷占用不一致"));
    }
    Ok(Some(task))
}

impl ManagedCatalog {
    /// Fix the new raw node and one physical-file slot atomically, with no CDB.
    /// A duplicate returns the same Prepared/Closed record; it never retries I/O.
    pub async fn prepare_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        plan: &ReclaimMetadataPlan,
    ) -> Result<ReclaimMetadataWrite> {
        request.validate()?;
        plan.encode()?;
        if plan.0.parent != reservation.fingerprint() {
            return Err(CatalogError::Invalid("回收元数据计划不属于原父任务"));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let input = Box::pin(relocation::metadata_input_at(
                                &tr,
                                reservation,
                                owner,
                                file,
                                request,
                            ))
                            .await?;
                            if input.fingerprint()? != plan.0.input
                                || input.source().byte_proof().object() != &plan.0.source
                            {
                                return Err(CatalogError::Invalid(
                                    "回收元数据计划改变原正文或目标证明",
                                ));
                            }
                            if let Some(saved) =
                                load_at(&tr, reservation, plan.0.source.uid).await?
                            {
                                if saved.plan() != *plan {
                                    return Err(CatalogError::Invalid(
                                        "回收元数据重试改变固定计划",
                                    ));
                                }
                                if saved.owner() != owner {
                                    return Err(CatalogError::OwnerFenced);
                                }
                                return Ok(saved);
                            }
                            require_idle_at(&tr, reservation).await?;
                            require_pool_without_reclaim_at(
                                &tr,
                                reservation.namespace(),
                                reservation.spec().pool,
                            )
                            .await?;
                            let (_, index) = active_volume::observed_at(
                                &tr,
                                reservation.serial(),
                                plan.volume(),
                            )
                            .await?;
                            let mut encoded = Vec::new();
                            active_volume::encode_index(&mut encoded, index);
                            let count = active_volume::file_count_at(
                                &tr,
                                reservation.serial(),
                                plan.volume(),
                            )
                            .await?;
                            if encoded != plan.0.previous || count != plan.0.physical_files {
                                return Err(CatalogError::Invalid(
                                    "回收元数据准备时目标卷头或物理文件计数改变",
                                ));
                            }
                            let pool_raw = tr
                                .get(
                                    &pool_key(reservation.namespace(), reservation.spec().pool),
                                    false,
                                )
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let pool = decode_pool(reservation.spec().pool, &pool_raw)?;
                            if count.checked_add(1).is_none_or(|n| n > pool.file_limit) {
                                return Err(CatalogError::Incomplete);
                            }
                            tape_monitor::require_data_writable(
                                &tr,
                                reservation.namespace(),
                                reservation.serial(),
                                &input.destination().placement().barcode,
                            )
                            .await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                            )
                            .await?;
                            let task = ReclaimMetadataWrite(Record {
                                version: 1,
                                plan: plan.0.clone(),
                                owner,
                                phase: Phase::Prepared,
                            });
                            let raw = task.encode()?;
                            let key = task_key(reservation, plan)?;
                            let fp = plan.fingerprint()?;
                            let held = claim(reservation, plan)?;
                            // All fallible validation/encoding precedes the first mutation.
                            tr.set(&key, &raw);
                            tr.set(&latest_key(reservation, plan.0.source.uid), &fp);
                            tr.set(&ledger::own_key(reservation, b'A'), &held);
                            tr.set(&node_attempts::volume_key(plan.volume()), &held);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Read the retained original task. Missing/corrupt slots are never repaired.
    pub async fn reclaim_metadata_write_status(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
    ) -> Result<Option<ReclaimMetadataWrite>> {
        let tr = self.db.create_trx()?;
        parent_at(&tr, reservation, owner).await?;
        let task = load_at(&tr, reservation, source_uid).await?;
        if task.as_ref().is_some_and(|t| t.owner() != owner) {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(task)
    }

    /// HA may claim only after the same parent has already been claimed.
    pub async fn claim_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
    ) -> Result<ReclaimMetadataWrite> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let mut task = load_at(&tr, reservation, source_uid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if task.owner() > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            task.0.owner = owner;
                            let raw = task.encode()?;
                            tr.set(&task_key(reservation, &task.plan())?, &raw);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Close only preparation: this kernel has no transition that admits I/O.
    /// Keep the original plan/receipt; clear only its exact verified own claims.
    pub async fn close_prepared_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataWrite> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let mut task = load_at(&tr, reservation, original.0.plan.source.uid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner || original.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != original.plan() {
                                return Err(CatalogError::Invalid("关闭改变原元数据计划"));
                            }
                            if task.closed() {
                                return Ok(task);
                            }
                            task.0.phase = Phase::Closed;
                            let raw = task.encode()?;
                            let key = task_key(reservation, &task.plan())?;
                            tr.set(&key, &raw);
                            tr.clear(&ledger::own_key(reservation, b'A'));
                            tr.clear(&node_attempts::volume_key(task.plan().volume()));
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
