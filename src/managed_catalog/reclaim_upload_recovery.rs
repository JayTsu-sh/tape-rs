//! Reentrant validation finish for an original child. The ordinary terminal
//! and monitor observation may commit separately; this bookmark bridges them.
use super::*;

fn key(parent: &ReclaimReservation, request: &ActiveFileRequest) -> Vec<u8> {
    let mut key = parent.journal_key();
    key[domain(parent.namespace(), b'j').len() - 1] = b'Y';
    key.extend_from_slice(request.operation_id.as_bytes());
    key
}
async fn input_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    request: &ActiveFileRequest,
) -> Result<(Value, ActiveFileIntent)> {
    request.validate()?;
    if record_at(tr, parent, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let (intent, permit) = children::child_at(
        tr,
        parent.namespace(),
        parent.spec().pool(),
        request.request_epoch,
        request.operation_id,
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if intent.request() != *request {
        return Err(CatalogError::Invalid("原复制恢复收尾改变封存请求"));
    }
    let input = json!({"version":1,"parent":parent.fingerprint(),"request":request.fingerprint(),"attempt":Sha256::digest(reservation::encode(&intent, permit)).to_vec(),"volume":intent.volume_id.to_string()});
    Ok((input, intent))
}
fn check(raw: &[u8], input: &Value, owner: u64) -> Result<()> {
    let value = decode(raw)?;
    if value.as_object().is_none_or(|m| m.len() != 2)
        || value["input"] != *input
        || !value["issued_owner"]
            .as_u64()
            .is_some_and(|v| v > 0 && v <= owner)
    {
        return Err(CatalogError::Corrupt("原复制恢复收尾检查点与输入不同"));
    }
    Ok(())
}
async fn require_pin_at(tr: &Transaction, request: &ActiveFileRequest) -> Result<()> {
    if tr
        .get(&children::history_pin_key(request), false)
        .await?
        .as_deref()
        != Some(&[][..])
    {
        return Err(CatalogError::Corrupt("原复制恢复收尾丢失历史保留引用"));
    }
    Ok(())
}
pub(super) async fn require_idle_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<()> {
    let mut prefix = parent.journal_key();
    prefix[domain(parent.namespace(), b'j').len() - 1] = b'Y';
    if requests::prefix_has_rows(tr, &prefix).await? {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
pub(super) async fn require_child_idle_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    request: &ActiveFileRequest,
) -> Result<()> {
    if tr.get(&key(parent, request), false).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
impl ManagedCatalog {
    pub async fn reclaim_upload_recovery_pending(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<bool> {
        let tr = self.db.create_trx()?;
        let (input, _) = input_at(&tr, parent, owner, request).await?;
        let Some(raw) = tr.get(&key(parent, request), false).await? else {
            return Ok(false);
        };
        check(&raw, &input, owner)?;
        require_pin_at(&tr, request).await?;
        Ok(true)
    }
    pub async fn begin_reclaim_upload_recovery(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let (input, mut original) =
                                input_at(&tr, parent, owner, request).await?;
                            source::require_metadata_idle_at(&tr, parent).await?;
                            mechanics::require_parent_idle_at(&tr, parent).await?;
                            let key = key(parent, request);
                            require_pin_at(&tr, request).await?;
                            if let Some(raw) = tr.get(&key, false).await? {
                                return check(&raw, &input, owner);
                            }
                            let current = reservation::active_file_attempt_at(&tr, request)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if current.owner_epoch > owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            original.owner_epoch = current.owner_epoch;
                            if current != original {
                                return Err(CatalogError::Corrupt(
                                    "原复制恢复缺少原尝试或历史保留引用",
                                ));
                            }
                            tr.set(
                                &key,
                                json!({"input":input,"issued_owner":owner})
                                    .to_string()
                                    .as_bytes(),
                            );
                            Ok(())
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    pub async fn complete_reclaim_upload_recovery(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
        index: ActiveIndexIdentity,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let (input, _) = input_at(&tr, parent, owner, request).await?;
                            let key = key(parent, request);
                            let raw = tr.get(&key, false).await?.ok_or(CatalogError::Incomplete)?;
                            check(&raw, &input, owner)?;
                            require_pin_at(&tr, request).await?;
                            let volume = node_mutation::id(
                                input["volume"].as_str().ok_or(CatalogError::Incomplete)?,
                            )?;
                            if index.volume_uuid != volume
                                || active_volume::observed_at(&tr, parent.serial(), volume)
                                    .await?
                                    .1
                                    != index
                                || reservation::active_file_attempt_at(&tr, request)
                                    .await?
                                    .is_some()
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            let terminal = tr
                                .get(
                                    &op_key(
                                        request.namespace,
                                        request.request_epoch,
                                        request.operation_id,
                                    ),
                                    false,
                                )
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if !matches!(
                                requests::reservation_terminal_result(
                                    &terminal,
                                    &request.fingerprint()
                                )?,
                                PublishOutcome::AlreadyApplied(_)
                                    | PublishOutcome::RequestCancelled
                            ) {
                                return Err(CatalogError::Incomplete);
                            }
                            let placement = verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let monitor = tape_monitor::read_at(
                                &tr,
                                parent.namespace(),
                                parent.serial(),
                                &placement.barcode,
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if monitor.import_required
                                || monitor.requires_validation
                                || monitor.verified_generation != Some(index.generation)
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            tr.clear(&key);
                            Ok(())
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
