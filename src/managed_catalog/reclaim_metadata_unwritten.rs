//! Close one original Writing attempt only after a fresh guarded clean read.
//! No new index, coverage, retry identity or device I/O is created here.
use super::*;

impl ManagedCatalog {
    pub async fn close_unwritten_reclaim_metadata_write(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        original: &ReclaimMetadataWrite,
        proof: &ReclaimMetadataUnwrittenEvidence,
    ) -> Result<ReclaimMetadataWrite> {
        request.validate()?;
        original.encode()?;
        let evidence = proof.require(original, owner)?;
        evidence.encode(&original.plan())?;
        if evidence.kind() != ReclaimMetadataObservationKind::Unchanged {
            return Err(CatalogError::Invalid("关闭缺少原任务本轮未写证明"));
        }
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
                            publication::require_original(original, &task, owner)?;
                            input_at(&tr, reservation, owner, file, request, &plan).await?;
                            if task.observation()?.as_ref() != Some(evidence) {
                                return Err(CatalogError::Invalid("关闭改变或绕过原已保存观察"));
                            }
                            if task.closed() {
                                return Ok(task);
                            }
                            if !task.writing() {
                                return Err(CatalogError::Invalid(
                                    "只有原 Writing 任务可按未写结果关闭",
                                ));
                            }
                            let (scan, index) = active_volume::observed_at(
                                &tr,
                                reservation.serial(),
                                plan.volume(),
                            )
                            .await?;
                            if scan.id.to_string() != task.0.scan
                                || index != evidence.current()
                                || active_volume::file_count_at(
                                    &tr,
                                    reservation.serial(),
                                    plan.volume(),
                                )
                                .await?
                                    != evidence.physical_files()
                                || active_volume::recovery_active(&tr, plan.volume()).await?
                            {
                                return Err(CatalogError::Corrupt(
                                    "关闭时原目标扫描、卷头、计数或恢复状态改变",
                                ));
                            }
                            let next = index
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("关闭时目标代次溢出"))?;
                            publication::require_empty_result_at(
                                &tr,
                                reservation.namespace(),
                                plan.volume(),
                                next,
                            )
                            .await?;
                            task.0.phase = Phase::Closed;
                            let raw = task.encode()?;
                            let header = registry::encode(reservation, &task)?;
                            let key = task_key(reservation, &task);
                            // load_at proved both exact own claims. All fallible work is done.
                            tr.set(&key, &raw);
                            tr.set(&registry::key(reservation), &header);
                            tr.clear(&ledger::own_key(reservation, b'A'));
                            tr.clear(&node_attempts::volume_key(plan.volume()));
                            Ok(task)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
