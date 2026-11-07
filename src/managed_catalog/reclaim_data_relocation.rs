//! Data relocation is distinct from metadata coverage and final erase proof.
use super::super::{children, retirement, walk};
use super::*;

/// One FDB read version binds the original bytes to their already verified donor.
/// This historical input is not a new physical read, metadata write or erase permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataInput {
    source: ReclaimReadSource,
    destination: ReclaimBodyDestination,
}
impl ReclaimMetadataInput {
    pub fn source(&self) -> &ReclaimReadSource {
        &self.source
    }
    pub fn destination(&self) -> &ReclaimBodyDestination {
        &self.destination
    }
    pub(super) fn fingerprint(&self) -> Result<[u8; 32]> {
        let mut index = Vec::new();
        active_volume::encode_index(&mut index, self.destination.index());
        let value = json!({"source":self.source.byte_proof().value()?,
            "copy":copy_records::encode_physical_copy(self.destination.copy())?,
            "head":encode_head(self.destination.head()),"index":index,
            "verification":self.destination.verification_sha256(),
            "retirement":self.destination.retirement()});
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-input/1");
        hash.update(value.to_string().as_bytes());
        Ok(hash.finalize().into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimDataProgress {
    version: u8,
    parent: String,
    inventory: [u8; 32],
    files: u64,
    bytes: u64,
    total_files: u64,
    total_bytes: u64,
}
impl ReclaimDataProgress {
    pub fn files(&self) -> u64 {
        self.files
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn total_files(&self) -> u64 {
        self.total_files
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

fn body_key(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'b');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
fn destination_key(reservation: &ReclaimReservation, uid: u64, operation: Uuid) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'd');
    key.extend_from_slice(&uid.to_be_bytes());
    key.extend_from_slice(operation.as_bytes());
    key
}
pub(in super::super) async fn require_no_data_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    for kind in *b"bdg" {
        if requests::prefix_has_rows(tr, &ledger::own_key(reservation, kind)).await? {
            return Err(CatalogError::Corrupt(
                "回收数据迁移账本在预期空状态下仍有记录",
            ));
        }
    }
    Ok(())
}
async fn progress_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    manifest: &ReclaimSourceProgress,
) -> Result<ReclaimDataProgress> {
    let raw = tr.get(&ledger::own_key(reservation, b'g'), false).await?;
    let Some(raw) = raw else {
        require_no_data_at(tr, reservation).await?;
        return Ok(ReclaimDataProgress {
            version: 1,
            parent: reservation.fingerprint(),
            inventory: manifest.summary.digest,
            files: 0,
            bytes: 0,
            total_files: manifest.summary.counts.files,
            total_bytes: manifest.summary.counts.data_bytes,
        });
    };
    let progress: ReclaimDataProgress = serde_json::from_slice(&raw).map_err(cache)?;
    if serde_json::to_vec(&progress).map_err(cache)?.as_slice() != raw.as_ref()
        || progress.version != 1
        || progress.parent != reservation.fingerprint()
        || progress.inventory != manifest.summary.digest
        || progress.total_files != manifest.summary.counts.files
        || progress.total_bytes != manifest.summary.counts.data_bytes
        || progress.files > progress.total_files
        || progress.bytes > progress.total_bytes
        || progress.files == 0 && progress.bytes != 0
    {
        return Err(CatalogError::Corrupt("回收数据迁移进度与原清单不一致"));
    }
    Ok(progress)
}

fn body_record(
    reservation: &ReclaimReservation,
    manifest: &ReclaimSourceProgress,
    evidence: &ReclaimSourceRead,
) -> Result<Vec<u8>> {
    let object = evidence.object();
    Ok(
        json!({"version":1,"parent":reservation.fingerprint(),"inventory":manifest.summary.digest,
        "uid":object.uid,"object_sha256":Sha256::digest(object.encode()?).to_vec(),
        "sha256":evidence.sha256(),"length":object.length})
        .to_string()
        .into_bytes(),
    )
}
fn destination_record(
    reservation: &ReclaimReservation,
    uid: u64,
    request: &ActiveFileRequest,
    body_raw: &[u8],
    verification_sha256: [u8; 32],
    retirement: &Value,
) -> Vec<u8> {
    json!({"version":1,"parent":reservation.fingerprint(),"uid":uid,
        "request":request.fingerprint(),"body_sha256":Sha256::digest(body_raw).to_vec(),
        "target_proof_sha256":verification_sha256.to_vec(),"retirement":retirement})
    .to_string()
    .into_bytes()
}

pub(super) async fn metadata_input_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
) -> Result<ReclaimMetadataInput> {
    let source = Box::pin(children::read_source_at(tr, reservation, owner, request)).await?;
    let destination = Box::pin(retirement::retired_destination_at(
        tr,
        reservation,
        owner,
        file,
        request,
    ))
    .await?;
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    let (_, original_index) = source_state_at(tr, reservation).await?;
    let mut original_head = Vec::new();
    active_volume::encode_index(&mut original_head, original_index);
    if manifest.summary.head != original_head
        || manifest.summary.volume != original_index.volume_uuid.to_string()
        || source.byte_proof().index() != original_head.as_slice()
    {
        return Err(CatalogError::Corrupt("元数据写入来源清单与原封存卷头不同"));
    }
    let evidence = source.byte_proof();
    let object = evidence.object();
    if evidence.inventory() != manifest.summary.digest
        || tr
            .get(&ledger::object_key(reservation, object.uid), false)
            .await?
            .as_deref()
            != Some(object.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("元数据写入来源与原源封存清单不同"));
    }
    let body = body_record(reservation, &manifest, evidence)?;
    let target = destination_record(
        reservation,
        object.uid,
        request,
        &body,
        destination.verification_sha256(),
        destination.retirement(),
    );
    if tr
        .get(&body_key(reservation, object.uid), false)
        .await?
        .as_deref()
        != Some(body.as_slice())
        || tr
            .get(
                &destination_key(reservation, object.uid, request.operation_id),
                false,
            )
            .await?
            .as_deref()
            != Some(target.as_slice())
    {
        return Err(CatalogError::Corrupt(
            "元数据写入缺少原正文或目标实读封存证明",
        ));
    }
    let progress = progress_at(tr, reservation, &manifest).await?;
    if progress.files == 0 || progress.bytes < object.length.ok_or(CatalogError::Incomplete)? {
        return Err(CatalogError::Corrupt("元数据写入的原正文迁移进度不完整"));
    }
    let assignment = ManagedCatalog::verified_assignment(
        tr,
        reservation.serial(),
        &destination.placement().barcode,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if assignment.volume_id != Some(destination.copy().volume_id)
        || assignment.namespace != reservation.namespace()
        || assignment.pool_id != reservation.spec().pool
        || !assignment.allows_io()
    {
        return Err(CatalogError::MediaUnavailable);
    }
    Ok(ReclaimMetadataInput {
        source,
        destination,
    })
}

impl ManagedCatalog {
    /// Resolve original source and sealed, retired destination coherently.
    /// Missing/corrupt receipts are rejected without repairing rows or I/O.
    pub async fn reclaim_metadata_input(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<ReclaimMetadataInput> {
        request.validate()?;
        let tr = self.db.create_trx()?;
        Box::pin(metadata_input_at(&tr, reservation, owner, file, request)).await
    }

    /// Seal source read, destination readback and exact retirement atomically.
    /// Resolve a lost reply with the same parent/child, without another tape I/O.
    /// Shared EE data UIDs count once; alias metadata remains separate work.
    pub async fn seal_reclaim_data_relocation(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<ReclaimDataProgress> {
        request.validate()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            Box::pin(seal_data_at(&tr, reservation, owner, file, request)).await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

/// All ledger validation finishes before the shared retirement's first write.
pub(super) async fn seal_data_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
) -> Result<ReclaimDataProgress> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    let (_, index) = source_state_at(tr, reservation).await?;
    let mut index_bytes = Vec::new();
    active_volume::encode_index(&mut index_bytes, index);
    if index_bytes != manifest.summary.head {
        return Err(CatalogError::Invalid("回收数据迁移的原卷头已改变"));
    }
    let (intent, _) = children::child_at(
        tr,
        reservation.namespace(),
        reservation.spec().pool,
        request.request_epoch,
        request.operation_id,
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if intent.request() != *request {
        return Err(CatalogError::Invalid("数据迁移必须复用封存的原子请求"));
    }
    let raw = tr
        .get(
            &children::child_key(
                reservation.namespace(),
                &reservation.token(),
                request.operation_id,
            )?,
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let child = decode(&raw)?;
    let source_raw: Vec<u8> =
        serde_json::from_value(child["source_copy"].clone()).map_err(cache)?;
    let source = copy_records::decode_copy(&source_raw)?;
    let head = decode_head(
        &serde_json::from_value::<Vec<u8>>(child["source_head"].clone()).map_err(cache)?,
    )?;
    let evidence = ReclaimSourceRead::require_source(
        &child["source_read"],
        &reservation.fingerprint(),
        &head,
        &source,
        &intent,
    )?;
    let object = evidence.object();
    let object_raw = object.encode()?;
    if evidence.inventory() != manifest.summary.digest
        || evidence.index() != manifest.summary.head
        || object.kind != ReclaimSourceNodeKind::File
        || tr
            .get(&ledger::object_key(reservation, object.uid), false)
            .await?
            .as_deref()
            != Some(object_raw.as_slice())
    {
        return Err(CatalogError::Corrupt("回收数据实读与完整源对象清单不一致"));
    }
    let target_raw = tr
        .get(&walk::verification_key(reservation, file), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let receipt =
        retirement::retirement_receipt(reservation, file, request, &source_raw, &target_raw);
    let body_raw = body_record(reservation, &manifest, &evidence)?;
    let destination_raw = destination_record(
        reservation,
        object.uid,
        request,
        &body_raw,
        Sha256::digest(&target_raw).into(),
        &receipt,
    );
    let body_key = body_key(reservation, object.uid);
    let destination_key = destination_key(reservation, object.uid, request.operation_id);
    let mut progress = progress_at(tr, reservation, &manifest).await?;
    let previous_body = tr.get(&body_key, false).await?;
    if let Some(previous) = &previous_body {
        if previous.as_ref() != body_raw {
            return Err(CatalogError::Corrupt("同一数据 UID 的原正文证明改变"));
        }
    } else {
        progress.files = progress
            .files
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("迁移数据计数溢出"))?;
        progress.bytes = progress
            .bytes
            .checked_add(object.length.ok_or(CatalogError::Incomplete)?)
            .ok_or(CatalogError::Corrupt("迁移数据字节数溢出"))?;
        if progress.files > progress.total_files || progress.bytes > progress.total_bytes {
            return Err(CatalogError::Corrupt("迁移数据超出原源清单"));
        }
    }
    if let Some(previous) = tr.get(&destination_key, false).await?
        && (previous.as_ref() != destination_raw || previous_body.is_none())
    {
        return Err(CatalogError::Corrupt(
            "原数据迁移目的回执冲突或缺少正文证明",
        ));
    }
    let progress_raw = serde_json::to_vec(&progress).map_err(cache)?;
    // Finish every fallible ledger proof before retirement mutates X1/V.
    // No read, validation or encoding may follow that shared helper.
    Box::pin(retirement::retire_at(tr, reservation, owner, file, request)).await?;
    tr.set(&body_key, &body_raw);
    tr.set(&destination_key, &destination_raw);
    tr.set(&ledger::own_key(reservation, b'g'), &progress_raw);
    Ok(progress)
}
