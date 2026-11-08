//! Page sealing is an internal read-proof ledger, never an erase checkpoint.
use super::*;

pub(super) fn own_key(reservation: &ReclaimReservation, kind: u8) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = kind;
    key
}
pub(super) fn object_key(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = own_key(reservation, b'e');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn encode(progress: &ReclaimSourceProgress) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(progress).map_err(cache)?;
    if raw.len() > 90_000 {
        return Err(CatalogError::Invalid("回收源清单检查点超出预算"));
    }
    Ok(raw)
}
fn validate(progress: &ReclaimSourceProgress, reservation: &ReclaimReservation) -> Result<()> {
    let counts = &progress.counts;
    let expected = &progress.summary.counts;
    if progress.version != 1
        || progress.parent != reservation.fingerprint()
        || progress.summary.volume != reservation.spec().volume.to_string()
        || expected.directories == 0
        || counts.objects > expected.objects
        || counts.directories > expected.directories
        || counts.files > expected.files
        || counts.symlinks > expected.symlinks
        || counts.data_bytes > expected.data_bytes
        || counts
            .directories
            .checked_add(counts.files)
            .and_then(|n| n.checked_add(counts.symlinks))
            != Some(counts.objects)
        || expected
            .directories
            .checked_add(expected.files)
            .and_then(|n| n.checked_add(expected.symlinks))
            != Some(expected.objects)
        || (progress.after.is_none() != (counts.objects == 0))
        || counts.objects == 0 && progress.digest != progress.summary.seed()
        || progress.complete != (counts.objects == expected.objects)
        || progress.complete && (counts != expected || progress.digest != progress.summary.digest)
    {
        return Err(CatalogError::Corrupt("回收源清单计数、摘要或父身份不一致"));
    }
    if let Some(after) = &progress.after {
        if (counts.objects == 1) != after.is_empty() {
            return Err(CatalogError::Corrupt("回收源首项不是根目录"));
        }
        if !after.is_empty() {
            import_spool::validate_path(after).map_err(cache)?;
        }
    }
    Ok(())
}
async fn state_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    inventory: &ReclaimSourceInventory<'_>,
) -> Result<ReclaimSourceProgress> {
    let Some(raw) = tr.get(&own_key(reservation, b'm'), false).await? else {
        if requests::prefix_has_rows(tr, &own_key(reservation, b'e')).await? {
            return Err(CatalogError::Corrupt("回收源清单有对象但缺少检查点"));
        }
        return Ok(ReclaimSourceProgress {
            version: 1,
            parent: reservation.fingerprint(),
            summary: inventory.summary.clone(),
            counts: Counts::default(),
            after: None,
            digest: inventory.summary.seed(),
            complete: false,
        });
    };
    let progress: ReclaimSourceProgress = serde_json::from_slice(&raw).map_err(cache)?;
    if encode(&progress)?.as_slice() != raw.as_ref() || progress.summary != inventory.summary {
        return Err(CatalogError::Corrupt("回收源清单编码或原实读来源改变"));
    }
    validate(&progress, reservation)?;
    Ok(progress)
}

pub(super) async fn require_complete_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<ReclaimSourceProgress> {
    let raw = tr
        .get(&own_key(reservation, b'm'), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let progress: ReclaimSourceProgress = serde_json::from_slice(&raw).map_err(cache)?;
    if encode(&progress)?.as_slice() != raw.as_ref() {
        return Err(CatalogError::Corrupt("回收源清单编码"));
    }
    validate(&progress, reservation)?;
    if !progress.complete {
        return Err(CatalogError::Incomplete);
    }
    Ok(progress)
}

/// The caller has already proved the original parent is reserved with zero
/// children. Clearing only its read-proof prefix cannot erase tape contents.
pub(in super::super) fn clear_reserved_at(tr: &Transaction, reservation: &ReclaimReservation) {
    tr.clear(&own_key(reservation, b'm'));
    for kind in *b"en" {
        let prefix = own_key(reservation, kind);
        tr.clear_range(&prefix, &prefix_end(&prefix));
    }
}

async fn require_inventory_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    inventory: &ReclaimSourceInventory<'_>,
) -> Result<()> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let (_, index) = source_state_at(tr, reservation).await?;
    let progress = require_complete_at(tr, reservation).await?;
    let mut head = Vec::new();
    active_volume::encode_index(&mut head, index);
    if progress.summary != inventory.summary
        || progress.summary.volume != index.volume_uuid.to_string()
        || progress.summary.head != head
    {
        return Err(CatalogError::Corrupt(
            "回收源清单与原实际捕获或封存卷索引不一致",
        ));
    }
    Ok(())
}

async fn require_object_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    object: &Object,
) -> Result<()> {
    let expected = object.encode()?;
    if tr
        .get(&object_key(reservation, object.uid), false)
        .await?
        .as_deref()
        != Some(expected.as_slice())
    {
        return Err(CatalogError::Corrupt(
            "回收源对象缺失或与原实际封存节点不一致",
        ));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Resolve 1..32 distinct original nodes in one FDB read version, with
    /// exactly the same complete source seal checks as bounded enumeration.
    pub async fn reclaim_source_nodes(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
        uids: &[u64],
    ) -> Result<Vec<ReclaimSourceNode>> {
        if !(1..=PAGE).contains(&uids.len()) {
            return Err(CatalogError::Invalid("精确读取回收源节点必须为 1..=32 项"));
        }
        let mut seen = std::collections::HashSet::new();
        let mut objects = Vec::with_capacity(uids.len());
        for uid in uids {
            if *uid == 0 || !seen.insert(*uid) {
                return Err(CatalogError::Invalid("回收源节点 UID 不能为零或重复"));
            }
            let entry = inventory
                .source
                .entry_by_uid(*uid)
                .map_err(cache)?
                .ok_or(CatalogError::Incomplete)?;
            objects.push(Object::capture(entry)?);
        }
        let tr = self.db.create_trx()?;
        require_inventory_at(&tr, reservation, owner, inventory).await?;
        for object in &objects {
            require_object_at(&tr, reservation, object).await?;
        }
        Ok(objects.into_iter().map(ReclaimSourceNode).collect())
    }

    /// Read original nodes in physical path order using the immutable actual
    /// source capture. Every returned node must still have its exact sealed
    /// record; missing or canonically altered rows cannot shrink the inventory.
    /// Neither a page nor exhaustion proves relocation or permission to format.
    pub async fn reclaim_source_nodes_page(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ReclaimSourceNode>> {
        if !(1..=PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("回收源对象分页必须在 1..=32"));
        }
        let prefix = own_key(reservation, b'e');
        let cursor = if let Some(key) = after {
            if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 {
                return Err(CatalogError::Invalid("回收源对象游标不属于原父任务"));
            }
            let uid =
                u64::from_be_bytes(key[prefix.len()..].try_into().expect("checked UID length"));
            let entry = inventory
                .source
                .entry_by_uid(uid)
                .map_err(cache)?
                .ok_or(CatalogError::Invalid("回收源游标节点不在原实际索引中"))?;
            Some(Object::capture(entry)?)
        } else {
            None
        };
        // The source is immutable and budgeted. Capture rows before opening the
        // FDB read version, so local XML/SQLite work does not age a transaction.
        let rows = inventory.page(cursor.as_ref().map(|node| node.path.as_str()), limit)?;
        let next = if rows.len() == limit
            && !inventory
                .page(Some(&rows.last().expect("nonempty page").path), 1)?
                .is_empty()
        {
            Some(object_key(
                reservation,
                rows.last().expect("nonempty page").uid,
            ))
        } else {
            None
        };
        let tr = self.db.create_trx()?;
        require_inventory_at(&tr, reservation, owner, inventory).await?;
        for object in cursor.iter().chain(&rows) {
            require_object_at(&tr, reservation, object).await?;
        }
        Ok(Page {
            items: rows.into_iter().map(ReclaimSourceNode).collect(),
            next,
        })
    }

    /// Seal at most 32 original nodes atomically. Root, empty directories,
    /// symlinks and EE internal objects are included without interpretation.
    /// No field here claims these nodes have a safe destination or are retired.
    pub async fn seal_reclaim_source_inventory_page(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
    ) -> Result<ReclaimSourceProgress> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let parent = record_at(&tr, reservation, owner).await?;
                            if parent["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let (_, index) = source_state_at(&tr, reservation).await?;
                            let mut head = Vec::new();
                            active_volume::encode_index(&mut head, index);
                            if inventory.summary.volume != index.volume_uuid.to_string()
                                || inventory.summary.head != head
                            {
                                return Err(CatalogError::Invalid(
                                    "实读源清单与原封存卷索引不一致",
                                ));
                            }
                            let mut progress = state_at(&tr, reservation, inventory).await?;
                            if progress.complete {
                                return Ok(progress);
                            }
                            let rows = inventory.page(progress.after.as_deref(), PAGE)?;
                            if rows.is_empty() {
                                return Err(CatalogError::Corrupt(
                                    "回收源清单在预计对象数之前结束",
                                ));
                            }
                            let mut writes = Vec::with_capacity(rows.len());
                            for object in rows {
                                if progress
                                    .after
                                    .as_ref()
                                    .is_some_and(|p| object.path.as_bytes() <= p.as_bytes())
                                    || progress.after.is_none()
                                        && (!object.path.is_empty()
                                            || object.kind != ReclaimSourceNodeKind::Directory)
                                {
                                    return Err(CatalogError::Corrupt(
                                        "回收源清单分页顺序改变或缺失根目录",
                                    ));
                                }
                                let raw = object.encode()?;
                                let key = object_key(reservation, object.uid);
                                if tr.get(&key, false).await?.is_some() {
                                    return Err(CatalogError::Corrupt(
                                        "回收源清单 UID 重复或超前存在",
                                    ));
                                }
                                progress.counts.add(&object)?;
                                progress.digest =
                                    advance_digest(progress.digest, progress.counts.objects, &raw);
                                progress.after = Some(object.path);
                                writes.push((key, raw));
                            }
                            progress.complete =
                                progress.counts.objects == progress.summary.counts.objects;
                            validate(&progress, reservation)?;
                            let raw = encode(&progress)?;
                            // retry_result commits domain errors; no mutation may precede
                            // the final count/digest/order/identity checks and serialization.
                            for (key, value) in writes {
                                tr.set(&key, &value);
                            }
                            tr.set(&own_key(reservation, b'm'), &raw);
                            Ok(progress)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
