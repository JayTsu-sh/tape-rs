//! Retire only an exact sealed source after its destination was read back.
//! Other versions, scans and namespaces remain blockers for later erase proof.
use super::*;

pub(super) fn retirement_receipt(
    reservation: &ReclaimReservation,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
    source_raw: &[u8],
    target_raw: &[u8],
) -> Value {
    json!({"version":1,"parent":reservation.fingerprint(),"request":request.fingerprint(),"file":file,
        "source_copy_sha256":format!("{:x}",Sha256::digest(source_raw)),
        "target_proof_sha256":format!("{:x}",Sha256::digest(target_raw))})
}

/// Every failing proof precedes the exact X1/V/receipt mutations.
pub(super) async fn retire_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
) -> Result<bool> {
    request.validate()?;
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let ns = reservation.namespace();
    let spec = reservation.spec();
    let (saved, _) = children::child_at(
        tr,
        ns,
        spec.pool,
        request.request_epoch,
        request.operation_id,
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if saved.request() != *request
        || file.operation() != request.operation_id
        || file.file_id() != request.file_id
        || Some(file.version()) != request.expected_version
        || file.path() != request.ltfs_path()?
        || file.size() != request.expected_size
    {
        return Err(CatalogError::Invalid("退休必须复用封存的回收文件请求"));
    }
    let verification_key = walk::verification_key(reservation, file);
    let raw_proof = tr
        .get(&verification_key, false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let proof = decode(&raw_proof)?;
    let version = file
        .version()
        .checked_add(1)
        .ok_or(CatalogError::Invalid("回收目标版本溢出"))?;
    let terminal = walk::terminal_evidence_at(tr, request, &saved, version, Some(&proof)).await?;
    let full = pool_catalog_path(spec.pool, file.path().as_bytes())?;
    let Some(ResolvedPath::File {
        id,
        head,
        copy,
        placement,
        ..
    }) =
        lookup::resolve_active_path_at_mode(tr, ns, lookup::path_components(&full)?, false).await?
    else {
        return Err(CatalogError::Incomplete);
    };
    let index_bytes: Vec<u8> = serde_json::from_value(proof["index"].clone())
        .map_err(|_| CatalogError::Corrupt("回收目标验证索引编码"))?;
    let verified_index = active_volume::decode_index(saved.volume_id, &index_bytes)?;
    let expected_proof = json!({"version":1,"parent":reservation.fingerprint(),
        "request":request.fingerprint(),"file":file,"head":encode_head(&head),
        "copy":copy_records::encode_physical_copy(&copy)?,"index":index_bytes,"terminal":terminal});
    if proof != expected_proof
        || id != file.file_id()
        || head.version != version
        || copy.size != file.size()
        || copy.content_digest != saved.expected_digest
        || copy.volume_id != saved.volume_id
        || copy.volume_id == spec.volume
        || copy.volume_generation != saved.volume_generation
        || copy.volume_scan_id != saved.volume_scan_id
        || verified_index.generation != copy.volume_generation
        || placement.pool_id != spec.pool
        || placement.library_serial != reservation.serial()
        || Some(placement.barcode.as_str()) != saved.barcode_constraint.as_deref()
    {
        return Err(CatalogError::Invalid("回收目标已改变或缺少完整实读证明"));
    }
    let (target_scan, target_index) =
        active_volume::observed_at(tr, reservation.serial(), saved.volume_id).await?;
    if target_scan.id != copy.volume_scan_id
        || target_index.generation < verified_index.generation
        || (target_index.generation == verified_index.generation && target_index != verified_index)
        || (target_index.generation > verified_index.generation
            && active_volume::history_at(
                tr,
                reservation.serial(),
                saved.volume_id,
                verified_index.generation,
            )
            .await?
            .1 != verified_index)
    {
        return Err(CatalogError::Invalid("回收目标不再属于原已验证索引链"));
    }
    // Compaction readers consult ordinary head/C rows. A visible
    // S4 overlay alone cannot make an old ordinary head disposable.
    if tr.get(&head_key(ns, id), false).await?.as_deref() != Some(encode_head(&head).as_slice())
        || tr
            .get(&copy_key(ns, id, head.copy_id), false)
            .await?
            .as_deref()
            != Some(copy_records::encode_physical_copy(&copy)?.as_slice())
    {
        return Err(CatalogError::Incomplete);
    }
    let (source_scan, _) = source_state_at(tr, reservation).await?;
    native_device::require_volume_quiescent_at(tr, ns, spec.volume).await?;
    let child = decode(
        &tr.get(
            &children::child_key(ns, &reservation.token(), request.operation_id)?,
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?,
    )?;
    let source_head = decode_head(
        &serde_json::from_value::<Vec<u8>>(child["source_head"].clone())
            .map_err(|_| CatalogError::Corrupt("退休原文件头编码"))?,
    )?;
    let source_raw: Vec<u8> = serde_json::from_value(child["source_copy"].clone())
        .map_err(|_| CatalogError::Corrupt("退休原副本编码"))?;
    let source = copy_records::decode_copy(&source_raw)?;
    if source_head.version != file.version()
        || source.version != source_head.version
        || source_head.copy_id == head.copy_id
        || source.volume_id != spec.volume
        || source.volume_scan_id != source_scan.id
        || source.size != file.size()
    {
        return Err(CatalogError::Corrupt("退休原副本与封存身份不符"));
    }
    let source_key = copy_key(ns, id, source_head.copy_id);
    let reverse = volume_key(
        spec.volume,
        source.volume_scan_id,
        ns,
        id,
        source_head.copy_id,
    );
    let source_now = tr.get(&source_key, false).await?;
    let reverse_now = tr.get(&reverse, false).await?;
    let marker = copy_records::encode_reclaimed_copy(source.version, source.origin);
    let receipt = retirement_receipt(reservation, file, request, &source_raw, &raw_proof);
    let mut retired_key = verification_key;
    retired_key[domain(ns, b'v').len() - 1] = b'x';
    if let Some(old) = tr.get(&retired_key, false).await? {
        if decode(&old)? != receipt
            || source_now.as_deref() != Some(marker.as_slice())
            || reverse_now.is_some()
        {
            return Err(CatalogError::Corrupt("退休回执与源副本/倒排不一致"));
        }
        return Ok(false);
    }
    if source_now.as_deref() != Some(source_raw.as_slice())
        || !reverse_now
            .as_deref()
            .is_some_and(|v| v.len() == 40 && read_u64(v) == source.version)
    {
        return Err(CatalogError::Corrupt("退休前原副本或倒排已改变"));
    }
    // All reads, encodings and fallible proofs precede mutations:
    // retry_result commits domain errors returned as Ok(Err(...)).
    let receipt = receipt.to_string();
    tr.set(&source_key, &marker);
    tr.clear(&reverse);
    tr.set(&retired_key, receipt.as_bytes());
    Ok(true)
}

impl ManagedCatalog {
    /// Retire only the original source after destination readback. It never
    /// releases the parent fence or authorizes format of the remaining source.
    pub async fn retire_reclaim_file_source(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<bool> {
        request.validate()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        Box::pin(retire_at(&tr, reservation, owner, file, request)).await,
                    )
                })
            })
            .await?
    }
    /// Check all retained scans/namespaces and pending volume claims at one
    /// FDB read version. This catalog-only check is not permission to format:
    /// the physical source manifest still needs independent complete coverage.
    pub async fn check_reclaim_source_unreferenced(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let parent = record_at(&tr, reservation, owner).await?;
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        source_state_at(&tr, reservation).await?;
        native_device::require_volume_unreferenced_at(
            &tr,
            reservation.namespace(),
            reservation.spec().volume,
        )
        .await
    }
}
