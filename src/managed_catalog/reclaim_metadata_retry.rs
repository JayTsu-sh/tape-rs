//! An explicit next attempt keeps the original closed row and one immutable
//! successor edge. It never rewrites an uncertain Writing operation.
use super::*;

impl ManagedCatalog {
    /// Restore a handle by its original persisted identity after a restart.
    /// The returned historical owner is a fact, never a current I/O permit.
    pub fn reclaim_metadata_attempt<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        source_uid: u64,
        attempt: Uuid,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let task = load_id_at(&tr, reservation, source_uid, attempt)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            if task.owner() > owner {
                return Err(CatalogError::OwnerFenced);
            }
            Ok(task)
        })
    }

    /// Claim exactly the discovered handle; a later attempt must not replace it.
    pub async fn claim_reclaim_metadata_attempt(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataWrite> {
        original.encode()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            registry::read_at(&tr, reservation).await?;
                            let mut task = load_id_at(
                                &tr,
                                reservation,
                                original.0.plan.source.uid,
                                original.id(),
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if task.owner() > owner || original.owner() > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != original.plan() {
                                return Err(CatalogError::Invalid("元数据领取改变原计划"));
                            }
                            require_phase_history(original, &task)?;
                            task.0.owner = owner;
                            let raw = task.encode()?;
                            let header =
                                registry::encode_if_current_at(&tr, reservation, &task).await?;
                            let key = task_key(reservation, &task);
                            tr.set(&key, &raw);
                            if let Some(header) = header {
                                tr.set(&registry::key(reservation), &header);
                            }
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Query one original handle, including a terminal predecessor of latest.
    /// This is not an owner claim or permission to execute that attempt.
    pub fn reclaim_metadata_attempt_status<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataWrite>> + 'a>>
    {
        Box::pin(async move {
            original.encode()?;
            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let task = load_id_at(&tr, reservation, original.0.plan.source.uid, original.id())
                .await?
                .ok_or(CatalogError::Incomplete)?;
            if task.plan() != original.plan() || task.owner() > owner {
                return Err(CatalogError::Invalid("历史元数据尝试改变原计划或 owner"));
            }
            require_phase_history(original, &task)?;
            Ok(task)
        })
    }

    /// Explicitly retry only a settled Closed attempt while its exact plan's
    /// target is still unchanged. A duplicate returns its retained successor.
    pub async fn retry_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
    ) -> Result<ReclaimMetadataWrite> {
        request.validate()?;
        original.encode()?;
        if !original.closed() {
            return Err(CatalogError::Invalid("新尝试要求原任务已明确关闭"));
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let uid = plan.source_uid();
                            let old = load_id_at(&tr, reservation, uid, original.id())
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            require_phase_history(original, &old)?;
                            if !old.closed() || old.plan() != plan || old.owner() > owner {
                                return Err(CatalogError::Invalid("元数据新尝试改变原关闭事实"));
                            }
                            old.0.context.require(reservation, file, request)?;
                            let input =
                                input_at(&tr, reservation, owner, file, request, &plan).await?;
                            let edge = registry::edge_key(reservation, uid, old.id());
                            if let Some(raw) = tr.get(&edge, false).await? {
                                let id = uuid_from(&raw)?;
                                if id.is_nil() {
                                    return Err(CatalogError::Corrupt("原元数据后继身份为空"));
                                }
                                let saved = load_id_at(&tr, reservation, uid, id)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("原后继边缺少新尝试"))?;
                                if saved.previous_attempt() != Some(old.id())
                                    || saved.plan() != plan
                                    || saved.owner() != owner
                                {
                                    return Err(CatalogError::Invalid(
                                        "元数据重试改变原后继或 owner",
                                    ));
                                }
                                return Ok(saved);
                            }
                            require_idle_at(&tr, reservation).await?;
                            if tr
                                .get(&latest_key(reservation, uid), false)
                                .await?
                                .as_deref()
                                != Some(old.id().as_bytes())
                            {
                                return Err(CatalogError::Corrupt(
                                    "原元数据最新索引改变却缺少后继边",
                                ));
                            }
                            let scan = target_at(&tr, reservation, &plan, &input).await?;
                            if scan.to_string() != old.0.scan {
                                return Err(CatalogError::Corrupt("元数据新尝试改变原扫描"));
                            }
                            native_device::require_volume_quiescent_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                            )
                            .await?;
                            let task = ReclaimMetadataWrite(Record {
                                version: 5,
                                id: Uuid::new_v4().to_string(),
                                first: old.0.first.clone(),
                                previous: Some(old.id().to_string()),
                                scan: old.0.scan.clone(),
                                plan: plan.0.clone(),
                                context: old.0.context.clone(),
                                owner,
                                phase: Phase::Prepared,
                                observation: None,
                            });
                            let raw = task.encode()?;
                            let held = claim(reservation, &task)?;
                            let header = registry::encode(reservation, &task)?;
                            let task_key = task_key(reservation, &task);
                            // No fallible work after the first mutation.
                            tr.set(&task_key, &raw);
                            tr.set(&edge, task.id().as_bytes());
                            tr.set(&latest_key(reservation, uid), task.id().as_bytes());
                            tr.set(&registry::key(reservation), &header);
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
}
