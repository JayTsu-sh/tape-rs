//! Claim durable parent reservations before recovering any of their Q/I/S4
//! children. Waiting for Serving here would deadlock takeover recovery.
use super::*;

impl ManagedCatalog {
    /// Enumerate original children, including ones whose Q/I or ordinary P
    /// records have already disappeared. A crash after generic cancellation
    /// must not hide the remaining parent bookkeeping from recovery.
    pub async fn reclaim_uploads_page(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ActiveFileRequest>> {
        if !(1..=16).contains(&limit) {
            return Err(CatalogError::Invalid("回收子请求分页必须在 1..=16"));
        }
        let mut prefix = reservation.journal_key();
        prefix[domain(reservation.namespace(), b'j').len() - 1] = b'c';
        if after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 16) {
            return Err(CatalogError::Invalid("回收子请求分页游标不属于原父任务"));
        }
        let tr = self.db.create_trx()?;
        let parent = record_at(&tr, reservation, owner).await?;
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let page = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, raw) in page.items {
            if key.len() != prefix.len() + 16 {
                return Err(CatalogError::Corrupt("回收原子请求键长度"));
            }
            let operation = uuid_from(&key[prefix.len()..])?;
            let value = decode(&raw)?;
            let epoch = value["epoch"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(CatalogError::Corrupt("回收原子请求代次"))?;
            let (intent, _) = children::child_at(
                &tr,
                reservation.namespace(),
                reservation.spec().pool,
                epoch,
                operation,
                owner,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            items.push(intent.request());
        }
        if after.is_none() && items.is_empty() && parent["children"] != 0 {
            return Err(CatalogError::Corrupt("父任务记录的原子请求已丢失"));
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Called after the runtime owner fence and before opening loaded media.
    /// Each transaction claims at most one bounded page. No device I/O or
    /// destructive phase transition is performed by claiming a reservation.
    pub async fn claim_reclaim_reservations_page(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ReclaimReservation>> {
        if !(1..=16).contains(&limit) {
            return Err(CatalogError::Invalid("回收接任分页必须在 1..=16"));
        }
        let prefix = domain(route.route.namespace, b'p');
        if after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 16) {
            return Err(CatalogError::Invalid("回收接任分页游标无效"));
        }
        self.db
            .run(|tr, _| {
                let prefix = prefix.clone();
                async move {
                    requests::retry_result(
                        async {
                            let ns =
                                routing::require_runtime_active_owner(&tr, serial, route, owner)
                                    .await?;
                            let page = read_page(&tr, &prefix, after, limit, false).await?;
                            let mut claims = Vec::with_capacity(page.items.len());
                            for (key, _) in page.items {
                                if key.len() != prefix.len() + 16 {
                                    return Err(CatalogError::Corrupt("回收池屏障键长度"));
                                }
                                let pool = uuid_from(&key[prefix.len()..])?;
                                require_pool_library(&tr, ns, pool, serial).await?;
                                let token = pool_gate_at(&tr, ns, pool)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("回收屏障在接任页中消失"))?;
                                if token["library"] != serial {
                                    return Err(CatalogError::Corrupt("回收屏障属于其他带库"));
                                }
                                let mut journal = domain(ns, b'j');
                                journal.extend_from_slice(
                                    &token["epoch"]
                                        .as_u64()
                                        .expect("validated epoch")
                                        .to_be_bytes(),
                                );
                                journal.extend_from_slice(
                                    Uuid::parse_str(
                                        token["operation"].as_str().expect("validated operation"),
                                    )
                                    .expect("validated UUID")
                                    .as_bytes(),
                                );
                                let raw = tr
                                    .get(&journal, false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("回收屏障缺少父检查点"))?;
                                let stored = decode(&raw)?;
                                let reservation = ReclaimReservation {
                                    intent: stored["intent"].clone(),
                                    route,
                                };
                                // Validate fields before using the private, structurally
                                // trusted handle's infallible accessors.
                                let fields = [
                                    "version",
                                    "library",
                                    "namespace",
                                    "epoch",
                                    "operation",
                                    "change",
                                    "scan",
                                    "scan_generation",
                                    "index",
                                ];
                                let intent = &reservation.intent;
                                if intent["version"] != 1
                                    || intent["library"] != serial
                                    || intent["namespace"] != ns.to_string()
                                    || intent["epoch"] != token["epoch"]
                                    || intent["operation"] != token["operation"]
                                    || !intent.as_object().is_some_and(|m| {
                                        m.len() == fields.len()
                                            && m.keys().all(|k| fields.contains(&k.as_str()))
                                    })
                                {
                                    return Err(CatalogError::Corrupt("回收接任意图结构"));
                                }
                                let spec = ReclaimSpec::parse(&intent["change"])
                                    .map_err(|_| CatalogError::Corrupt("回收接任原磁带身份"))?;
                                if spec.pool != pool {
                                    return Err(CatalogError::Corrupt("回收接任池身份"));
                                }
                                let mut value = record_at(&tr, &reservation, owner).await?;
                                if value["owner_epoch"]
                                    .as_u64()
                                    .is_none_or(|saved| saved > owner)
                                {
                                    return Err(CatalogError::OwnerFenced);
                                }
                                let assignment =
                                    Self::verified_assignment(&tr, serial, &spec.barcode)
                                        .await?
                                        .ok_or(CatalogError::Incomplete)?;
                                spec.require_assignment(&assignment, ns, serial)?;
                                let (scan, index) =
                                    active_volume::observed_at(&tr, serial, spec.volume).await?;
                                let mut bytes = Vec::new();
                                active_volume::encode_index(&mut bytes, index);
                                if intent["scan"] != scan.id.to_string()
                                    || intent["scan_generation"] != scan.generation
                                    || intent["index"] != json!(bytes)
                                {
                                    return Err(CatalogError::Invalid(
                                        "回收原源索引改变，停止接任",
                                    ));
                                }
                                value["owner_epoch"] = json!(owner);
                                claims.push((journal, value.to_string(), reservation));
                            }
                            // retry_result commits domain errors. Prove the whole page
                            // before writing even the first parent's new owner fence.
                            for (key, value, _) in &claims {
                                tr.set(key, value.as_bytes());
                            }
                            Ok(Page {
                                items: claims.into_iter().map(|(_, _, r)| r).collect(),
                                next: page.next,
                            })
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}
