//! Unversioned historical-copy GC under the original reclaim parent. Each
//! transaction proves its complete page before retiring any exact C/V pair.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimObsoleteProgress {
    version: u8,
    parent: String,
    after: Option<Vec<u8>>,
    complete: bool,
    scanned: u64,
    compacted: u64,
}
impl ReclaimObsoleteProgress {
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn compacted(&self) -> u64 {
        self.compacted
    }
}
fn key(parent: &ReclaimReservation) -> Vec<u8> {
    let mut key = parent.journal_key();
    key[domain(parent.namespace(), b'j').len() - 1] = b'Q';
    key
}
fn volume_prefix(parent: &ReclaimReservation) -> Vec<u8> {
    let mut prefix = super::super::key(b'V');
    prefix.extend_from_slice(parent.spec().volume().as_bytes());
    prefix
}
async fn obsolete_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    reverse: &[u8],
    raw: &[u8],
) -> Result<Option<(Vec<u8>, Vec<u8>, Vec<u8>)>> {
    let prefix = volume_prefix(parent);
    // Unknown and foreign references remain blockers. This is never an
    // administrative clear-range operation or a malformed-row repair path.
    if reverse.len() != prefix.len() + 64 {
        return Ok(None);
    }
    let suffix = &reverse[prefix.len()..];
    let scan_id = uuid_from(&suffix[..16])?;
    let ns = uuid_from(&suffix[16..32])?;
    let file = uuid_from(&suffix[32..48])?;
    let id = uuid_from(&suffix[48..])?;
    if ns != parent.namespace() {
        return Ok(None);
    }
    let copy_key = copy_key(ns, file, id);
    let copy_raw = tr
        .get(&copy_key, false)
        .await?
        .ok_or(CatalogError::Corrupt("旧版本引用缺少副本"))?;
    let copy = copy_records::decode_copy(&copy_raw)?;
    // V stores a source-specific publication digest. Ordinary writes retain a
    // receipt fingerprint; imports/nodes retain a C digest. Preserve the full
    // original V value in our receipt and transaction conflict set.
    if raw.len() != 40
        || read_u64(raw) != copy.version
        || copy.volume_id != parent.spec().volume()
        || copy.volume_scan_id != scan_id
    {
        return Err(CatalogError::Corrupt("旧版本引用与原副本不符"));
    }
    // Published overlays must first be folded by their own original protocol.
    // Reading a plain H while an overlay exists would select the wrong head.
    if tr
        .get(&active_batch::file_claim_key_for(ns, file), false)
        .await?
        .is_some()
    {
        return Ok(None);
    }
    let node = tr
        .get(&node_key(ns, file), false)
        .await?
        .ok_or(CatalogError::Corrupt("旧版本 inode 缺失"))?;
    let head_raw = tr
        .get(&head_key(ns, file), false)
        .await?
        .ok_or(CatalogError::Corrupt("旧版本当前 head 缺失"))?;
    let head = decode_head(&head_raw)?;
    let window = requests::window(tr, ns).await?;
    if copy
        .origin
        .receipt_epoch()
        .is_some_and(|epoch| epoch > window.accepting)
    {
        return Err(CatalogError::Corrupt("旧版本副本请求代次未开放"));
    }
    let authority = if retired_nodes::from_node_record(ns, file, &node)?.is_some() {
        let retired = retired_nodes::deleted_copy_at(tr, ns, file, id, copy.version)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if retired.pool != parent.spec().pool()
            || (copy.version == head.version && copy.size != head.size)
        {
            return Err(CatalogError::Corrupt("删除历史改变原池"));
        }
        node_attempts::require_retirement_at(tr, parent.serial(), &retired, id, &copy).await?;
        json!({"kind":"deleted_inode","node":Sha256::digest(&node).to_vec(),"head":head_raw.to_vec(),"result":retired.result_digest})
    } else {
        if decode_node(&node)?.kind != NodeKind::File {
            return Err(CatalogError::Corrupt("旧版本 inode 类型不符"));
        }
        if copy.version > head.version || (id == head.copy_id && copy.version != head.version) {
            return Err(CatalogError::Corrupt("旧版本副本超前于当前 head"));
        }
        if copy.version == head.version {
            return Ok(None);
        }
        let primary_raw = tr
            .get(&super::super::copy_key(ns, file, head.copy_id), false)
            .await?
            .ok_or(CatalogError::Corrupt("旧版本当前副本缺失"))?;
        let primary = copy_records::decode_copy(&primary_raw)?;
        let placement = verified_placement(tr, primary.volume_id)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != ns
            || placement.library_serial != parent.serial()
            || placement.pool_id != parent.spec().pool()
        {
            return Err(CatalogError::Corrupt("旧版本当前副本改变原池或带库"));
        }
        active_volume::history_at(
            tr,
            parent.serial(),
            primary.volume_id,
            primary.volume_generation,
        )
        .await?;
        let reverse = tr
            .get(
                &volume_key(
                    primary.volume_id,
                    primary.volume_scan_id,
                    ns,
                    file,
                    head.copy_id,
                ),
                false,
            )
            .await?
            .ok_or(CatalogError::Corrupt("旧版本当前副本缺少确切反向引用"))?;
        if reverse.len() != 40 || read_u64(&reverse) != primary.version {
            return Err(CatalogError::Corrupt(
                "旧版本当前副本反向引用版本或长度不符",
            ));
        }
        if primary.version != head.version
            || primary.size != head.size
            || primary
                .origin
                .receipt_epoch()
                .is_some_and(|epoch| epoch > window.accepting)
        {
            return Err(CatalogError::Corrupt("旧版本当前副本或请求代次不符"));
        }
        json!({"kind":"superseded","head":head_raw.to_vec(),"primary":Sha256::digest(&primary_raw).to_vec(),"primary_reverse":Sha256::digest(&reverse).to_vec()})
    };
    let manifest = tr
        .get(&scan_key(copy.volume_id, scan_id), false)
        .await?
        .ok_or(CatalogError::Corrupt("旧版本原扫描记录缺失"))?;
    let (scan, _, state) = decode_scan_manifest(&manifest)?;
    let (_, source_head) = source_state_at(tr, parent).await?;
    if scan.id != scan_id
        || scan.generation > copy.volume_generation
        || copy.volume_generation > source_head.generation
        || !matches!(state, ScanState::Ready | ScanState::Retired)
    {
        return Err(CatalogError::Corrupt("旧版本原扫描身份或代次不符"));
    }
    let receipt =
        json!({"version":1,"parent":parent.fingerprint(),"copy":Sha256::digest(&copy_raw).to_vec(),
        "authority":authority,"reverse":Sha256::digest(raw).to_vec()})
        .to_string()
        .into_bytes();
    Ok(Some((
        copy_key,
        copy_records::encode_reclaimed_copy(copy.version, copy.origin),
        receipt,
    )))
}

impl ManagedCatalog {
    /// None means the full physical coverage/walk is not yet finished. A full
    /// sweep is not format authority: the independent global zero-V gate still
    /// runs immediately before initialization and publication.
    pub async fn step_reclaim_obsolete_copies(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimObsoleteProgress>> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let record = record_at(&tr, parent, owner).await?;
                            if record["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            if record["phase"] == "reserved" {
                                return Ok(None);
                            }
                            if record["phase"] != "copying" {
                                return Err(CatalogError::PoolBusy);
                            }
                            if !source::copy_coverage_complete_at(&tr, parent, owner).await?
                                || !walk::current_state_at(&tr, parent).await?.complete()
                            {
                                return Ok(None);
                            }
                            source::require_complete_coverage_at(&tr, parent, owner).await?;
                            layout::at(&tr, parent).await?;
                            mechanics::require_parent_idle_at(&tr, parent).await?;
                            upload_recovery::require_idle_at(&tr, parent).await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                parent.namespace(),
                                parent.spec().volume(),
                            )
                            .await?;
                            let key = key(parent);
                            let prefix = volume_prefix(parent);
                            let mut progress = if let Some(raw) = tr.get(&key, false).await? {
                                let value: ReclaimObsoleteProgress =
                                    serde_json::from_value(decode(&raw)?)
                                        .map_err(|_| CatalogError::Corrupt("旧版本回收游标结构"))?;
                                if value.version != 1
                                    || value.parent != parent.fingerprint()
                                    || value
                                        .after
                                        .as_ref()
                                        .is_some_and(|k| !k.starts_with(&prefix))
                                    || value.complete != value.after.is_none()
                                    || value.compacted > value.scanned
                                {
                                    return Err(CatalogError::Corrupt(
                                        "旧版本回收游标身份或计数不符",
                                    ));
                                }
                                value
                            } else {
                                ReclaimObsoleteProgress {
                                    version: 1,
                                    parent: parent.fingerprint(),
                                    after: None,
                                    complete: false,
                                    scanned: 0,
                                    compacted: 0,
                                }
                            };
                            let page =
                                read_page(&tr, &prefix, progress.after.as_deref(), 16, false)
                                    .await?;
                            let mut mutations = Vec::new();
                            for (reverse, raw) in &page.items {
                                if let Some((copy, marker, receipt)) =
                                    obsolete_at(&tr, parent, reverse, raw).await?
                                {
                                    let mut receipt_key = key.clone();
                                    receipt_key.push(b'r');
                                    receipt_key.extend_from_slice(&reverse[prefix.len()..]);
                                    if tr.get(&receipt_key, false).await?.is_some() {
                                        return Err(CatalogError::Corrupt(
                                            "旧版本已有回收回执却仍保留 V",
                                        ));
                                    }
                                    mutations.push((reverse, copy, marker, receipt_key, receipt));
                                }
                            }
                            progress.scanned = progress
                                .scanned
                                .checked_add(page.items.len() as u64)
                                .ok_or(CatalogError::Incomplete)?;
                            progress.compacted = progress
                                .compacted
                                .checked_add(mutations.len() as u64)
                                .ok_or(CatalogError::Incomplete)?;
                            progress.complete = page.next.is_none();
                            progress.after = page.next;
                            let raw = serde_json::to_value(&progress)
                                .map_err(|_| CatalogError::Corrupt("旧版本回收游标编码"))?
                                .to_string();
                            for (reverse, copy, marker, receipt_key, receipt) in mutations {
                                tr.set(&copy, &marker);
                                tr.clear(reverse);
                                tr.set(&receipt_key, &receipt);
                            }
                            tr.set(&key, raw.as_bytes());
                            Ok(Some(progress))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}
