//! Bounded cleanup after an independent completion receipt. The queue survives
//! HA; physical-index receipts and the output UUID reservation remain immutable.
use super::*;

pub(super) fn key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = b'C';
    key
}
impl ManagedCatalog {
    pub async fn reclaim_cleanup_page(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ReclaimReservation>> {
        if !(1..=16).contains(&limit) {
            return Err(CatalogError::Invalid("回收完成清理分页须在 1..=16"));
        }
        let ns = route.route.namespace;
        let prefix = domain(ns, b'C');
        if after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24) {
            return Err(CatalogError::Invalid("回收完成清理游标无效"));
        }
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let page = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::new();
        for (pending, raw) in page.items {
            if pending.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("回收完成清理键无效"));
            }
            let token = decode(&raw)?;
            // Other libraries in this namespace retain their own queue entry.
            if token["library"] != serial {
                continue;
            }
            let mut receipt = pending.clone();
            receipt[prefix.len() - 1] = b'f';
            let result = ReclaimFormatResult::decode(
                &tr.get(&receipt, false)
                    .await?
                    .ok_or(CatalogError::Corrupt("回收清理缺少独立完成回执"))?,
            )?;
            let reservation = ReclaimReservation {
                intent: result.0.intent.clone(),
                route,
            };
            if result.0.namespace != ns.to_string()
                || result.0.library != serial
                || key(&reservation) != pending
                || reservation.token() != token
            {
                return Err(CatalogError::Corrupt("回收清理队列改变原完成身份"));
            }
            items.push(reservation);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// One transaction removes at most sixteen ledger rows (plus their checked
    /// reverse identities). It never clears catalog references or tape history.
    pub async fn step_reclaim_cleanup(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<bool> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            routing::require_runtime_active_owner(
                                &tr,
                                reservation.serial(),
                                reservation.route,
                                owner,
                            )
                            .await?;
                            result_at(&tr, reservation)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            management::exclusion::LibraryTaskClaim::for_reclaim(
                                &reservation.token(),
                            )?
                            .require_released(&tr)
                            .await?;
                            if let Some(gate) = pool_gate_at(
                                &tr,
                                reservation.namespace(),
                                reservation.spec().pool(),
                            )
                            .await?
                                && gate == reservation.token()
                            {
                                return Err(CatalogError::Corrupt("已完成回收仍持有原池屏障"));
                            }
                            let pending = tr.get(&key(reservation), false).await?;
                            let Some(pending) = pending else {
                                return Ok(true);
                            };
                            if decode(&pending)? != reservation.token() {
                                return Err(CatalogError::Corrupt("回收清理原队列改变"));
                            }
                            let mut active = reservation.journal_key();
                            active[domain(reservation.namespace(), b'j').len() - 1] = b'A';
                            if tr.get(&active, false).await?.is_some() {
                                return Err(CatalogError::Corrupt(
                                    "已完成回收仍持有未结清元数据任务",
                                ));
                            }
                            if children::cleanup_completed_page_at(&tr, reservation).await? {
                                return Ok(false);
                            }
                            // All of these prefixes include the complete original parent
                            // epoch and operation. Rf/RH/RJ and the permanent UUID claim do
                            // not use these temporary prefixes and are intentionally kept.
                            for kind in *b"vxyzsbenqdghkmWTUVZlFNDQr" {
                                let mut prefix = reservation.journal_key();
                                prefix[domain(reservation.namespace(), b'j').len() - 1] = kind;
                                let page = read_page(&tr, &prefix, None, 16, false).await?;
                                if !page.items.is_empty() {
                                    for (key, _) in page.items {
                                        tr.clear(&key);
                                    }
                                    return Ok(false);
                                }
                            }
                            tr.clear(&reservation.journal_key());
                            tr.clear(&key(reservation));
                            Ok(true)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}
