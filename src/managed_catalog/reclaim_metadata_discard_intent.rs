//! Journal the original deterministic Discard goal, without issuing a CDB.
use super::*;

impl ManagedCatalog {
    /// Persist only the original attempt's explicit cleanup phase. A retained
    /// response is not a repeat-write permit; the device actor must freshly
    /// classify original media under the resulting Discarding task guard.
    pub async fn begin_reclaim_metadata_discard(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
        evidence: &ReclaimMetadataTailEvidence,
    ) -> Result<ReclaimMetadataWrite> {
        request.validate()?;
        original.encode()?;
        evidence.require(original, owner)?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, reservation, owner).await?;
                            let plan = original.plan();
                            let mut task =
                                load_id_at(&tr, reservation, plan.source_uid(), original.id())
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                            if task.owner() != owner || original.owner() != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if task.plan() != plan {
                                return Err(CatalogError::Invalid("修尾意图改变原固定计划"));
                            }
                            require_phase_history(original, &task)?;
                            task.0.context.require(reservation, file, request)?;
                            let input =
                                input_at(&tr, reservation, owner, file, request, &plan).await?;
                            if task.discarding() {
                                return Ok(task);
                            }
                            if !task.writing()
                                || task != *original
                                || task.observation()?.is_some_and(|value| {
                                    value.kind() == ReclaimMetadataObservationKind::Committed
                                })
                            {
                                return Err(CatalogError::Invalid(
                                    "修尾意图要求原未决 Writing 快照",
                                ));
                            }
                            if target_policy_at(&tr, reservation, &plan, &input, false)
                                .await?
                                .to_string()
                                != task.0.scan
                            {
                                return Err(CatalogError::Corrupt("修尾意图改变原目标扫描"));
                            }
                            if active_volume::recovery_active(&tr, plan.volume()).await? {
                                return Err(CatalogError::PoolBusy);
                            }
                            let next = plan
                                .previous_source()?
                                .head()
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("修尾下一代溢出"))?;
                            publication::require_empty_result_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                                next,
                            )
                            .await?;
                            task.0.phase = Phase::Discarding;
                            let raw = task.encode()?;
                            let header = registry::encode(reservation, &task)?;
                            tr.set(&task_key(reservation, &task), &raw);
                            tr.set(&registry::key(reservation), &header);
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
