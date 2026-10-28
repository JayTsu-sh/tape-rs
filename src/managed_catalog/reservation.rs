//! Write-before-data reservation for an Active LTFS file. The full physical
//! receipt is unavailable until the working index has assigned a fileuid.
//! Top-level `0` and `1` are reserved for the epoch rows and target claims;
//! older `Q` and `q` belong to import reconciliation and projection.

use super::*;
use physical::{ActiveFileIntent, validate_active_file_intent};
use sha2::{Digest, Sha256};

fn reservation_key(intent: &ActiveFileIntent) -> Vec<u8> {
    let mut out = requests::epoch_prefix(b'0', intent.namespace, intent.request_epoch);
    out.extend_from_slice(intent.operation_id.as_bytes());
    out
}

// A volume-local reverse index keeps takeover recovery proportional to the
// interrupted batch, not to every file in a large namespace.
fn volume_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'2');
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn volume_key(intent: &ActiveFileIntent) -> Vec<u8> {
    let mut out = volume_prefix(intent.volume_id, intent.volume_generation);
    out.extend_from_slice(&intent.request_epoch.to_be_bytes());
    out.extend_from_slice(intent.operation_id.as_bytes());
    out
}

fn upgraded_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'4');
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn upgraded_key(intent: &ActiveFileIntent) -> Vec<u8> {
    let mut out = upgraded_prefix(intent.volume_id, intent.volume_generation);
    out.extend_from_slice(&intent.request_epoch.to_be_bytes());
    out.extend_from_slice(intent.operation_id.as_bytes());
    out
}

pub(super) fn upgraded_key_for(receipt: &CommitReceipt) -> Vec<u8> {
    let mut out = upgraded_prefix(receipt.volume_id, receipt.volume_generation);
    out.extend_from_slice(&receipt.request_epoch.to_be_bytes());
    out.extend_from_slice(receipt.operation_id.as_bytes());
    out
}

pub(super) async fn intent_for_upgraded_at(
    tr: &Transaction,
    receipt: &CommitReceipt,
    permit: u16,
) -> Result<ActiveFileIntent> {
    let raw = tr
        .get(&upgraded_key_for(receipt), false)
        .await?
        .ok_or(CatalogError::Corrupt("封存批次缺少升级意图倒排"))?;
    let (intent, saved_permit) = decode(
        &raw,
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    )?;
    if saved_permit != permit || !matches_receipt(&intent, receipt) {
        return Err(CatalogError::Corrupt("封存批次升级意图与收据不一致"));
    }
    Ok(intent)
}

fn path_key(intent: &ActiveFileIntent) -> Vec<u8> {
    path_key_for(intent.namespace, intent.parent, &intent.name)
}

fn path_key_for(namespace: Uuid, parent: Uuid, name: &[u8]) -> Vec<u8> {
    let mut out = key(b'1');
    out.extend_from_slice(b"l/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(parent.as_bytes());
    append_component(&mut out, name);
    out
}

fn file_key(intent: &ActiveFileIntent) -> Vec<u8> {
    file_key_for(intent.namespace, intent.file_id)
}

fn file_key_for(namespace: Uuid, file: Uuid) -> Vec<u8> {
    let mut out = key(b'1');
    out.extend_from_slice(b"n/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(file.as_bytes());
    out
}

fn claim_value(intent: &ActiveFileIntent) -> [u8; 24] {
    let mut value = [0; 24];
    value[..8].copy_from_slice(&intent.request_epoch.to_be_bytes());
    value[8..].copy_from_slice(intent.operation_id.as_bytes());
    value
}

fn same_logical(left: &ActiveFileIntent, right: &ActiveFileIntent) -> bool {
    // An owner takeover changes the execution fence, not the upload request.
    ActiveFileIntent {
        owner_epoch: right.owner_epoch,
        ..left.clone()
    } == *right
}

fn encode(intent: &ActiveFileIntent, permit: u16) -> Vec<u8> {
    let payload = serde_json::json!({
        "version": 1, "permit": permit,
        "parent": intent.parent.to_string(),
        "parent_path": std::str::from_utf8(&intent.parent_path).expect("validated UTF-8 path"),
        "name": intent.name,
        "file": intent.file_id.to_string(), "previous": intent.expected_version,
        "serial": intent.library_serial, "pool": intent.pool_id.map(|id| id.to_string()),
        "owner": intent.owner_epoch, "volume": intent.volume_id.to_string(),
        "generation": intent.volume_generation, "scan": intent.volume_scan_id.to_string(),
        "size": intent.expected_size, "hash_kind": intent.expected_digest.tag(),
        "hash_bytes": intent.expected_digest.payload(),
    });
    let raw = payload.to_string();
    let mut digest = Sha256::new();
    digest.update(b"tape-rs/active-reservation/1");
    digest.update(intent.namespace.as_bytes());
    digest.update(intent.request_epoch.to_be_bytes());
    digest.update(intent.operation_id.as_bytes());
    digest.update(raw.as_bytes());
    let mut out = digest.finalize().to_vec();
    out.extend_from_slice(raw.as_bytes());
    out
}

pub(super) fn decode(
    bytes: &[u8],
    namespace: Uuid,
    request_epoch: u64,
    operation_id: Uuid,
) -> Result<(ActiveFileIntent, u16)> {
    let corrupt = || CatalogError::Corrupt("活动写前预约字段或指纹");
    if !(33..=8192).contains(&bytes.len()) {
        return Err(corrupt());
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes[32..]).map_err(|_| corrupt())?;
    let id = |field: &str| -> Result<Uuid> {
        v.get(field)
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse().ok())
            .ok_or_else(corrupt)
    };
    let number = |field: &str| -> Result<u64> {
        v.get(field)
            .and_then(|value| value.as_u64())
            .ok_or_else(corrupt)
    };
    if number("version")? != 1 {
        return Err(corrupt());
    }
    let previous = match v.get("previous") {
        Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(corrupt)?),
        None => return Err(corrupt()),
    };
    let pool = match v.get("pool") {
        Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(corrupt)?,
        ),
        None => return Err(corrupt()),
    };
    let intent = ActiveFileIntent {
        namespace,
        parent: id("parent")?,
        parent_path: v
            .get("parent_path")
            .and_then(|value| value.as_str())
            .ok_or_else(corrupt)?
            .as_bytes()
            .to_vec(),
        name: serde_json::from_value(v.get("name").ok_or_else(corrupt)?.clone())
            .map_err(|_| corrupt())?,
        file_id: id("file")?,
        expected_version: previous,
        operation_id,
        request_epoch,
        library_serial: v
            .get("serial")
            .and_then(|s| s.as_str())
            .ok_or_else(corrupt)?
            .into(),
        pool_id: pool,
        owner_epoch: number("owner")?,
        volume_id: id("volume")?,
        volume_generation: number("generation")?,
        volume_scan_id: id("scan")?,
        expected_size: number("size")?,
        expected_digest: ContentDigest::decode(
            u8::try_from(number("hash_kind")?).map_err(|_| corrupt())?,
            &serde_json::from_value::<[u8; 32]>(v.get("hash_bytes").ok_or_else(corrupt)?.clone())
                .map_err(|_| corrupt())?,
        )
        .map_err(|_| corrupt())?,
    };
    validate_active_file_intent(&intent).map_err(|_| corrupt())?;
    let permit = u16::try_from(number("permit")?).map_err(|_| corrupt())?;
    if permit >= 64 || encode(&intent, permit) != bytes {
        return Err(corrupt());
    }
    Ok((intent, permit))
}

fn receipt_stub(intent: &ActiveFileIntent) -> CommitReceipt {
    let mut receipt = CommitReceipt {
        namespace: intent.namespace,
        parent: intent.parent,
        name: intent.name.clone(),
        file_id: intent.file_id,
        expected_version: intent.expected_version,
        operation_id: intent.operation_id,
        request_epoch: intent.request_epoch,
        copy_id: Uuid::nil(),
        library_serial: intent.library_serial.clone(),
        pool_id: intent.pool_id,
        owner_epoch: intent.owner_epoch,
        volume_id: intent.volume_id,
        volume_generation: intent.volume_generation,
        volume_scan_id: intent.volume_scan_id,
        ltfs_file_uid: 1,
        size: intent.expected_size,
        content_digest: intent.expected_digest,
        metadata_digest: None,
    };
    receipt.copy_id = derive_copy_id(&receipt);
    receipt
}

fn cancellation_fingerprint(intent: &ActiveFileIntent) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"tape-rs/active-reservation-cancel/1");
    digest.update(receipt_fingerprint(&receipt_stub(intent)));
    digest.update((intent.parent_path.len() as u32).to_be_bytes());
    digest.update(&intent.parent_path);
    digest.finalize().into()
}

pub(super) async fn parent_matches_at(tr: &Transaction, intent: &ActiveFileIntent) -> Result<bool> {
    let components = lookup::path_components(&intent.parent_path)?;
    Ok(matches!(
        lookup::resolve_path_at(tr, intent.namespace, components, false).await?,
        Some(lookup::ResolvedPath::Directory { id, .. }) if id == intent.parent
    ))
}

fn matches_receipt(intent: &ActiveFileIntent, receipt: &CommitReceipt) -> bool {
    let stub = receipt_stub(intent);
    receipt.namespace == stub.namespace
        && receipt.parent == stub.parent
        && receipt.name == stub.name
        && receipt.file_id == stub.file_id
        && receipt.expected_version == stub.expected_version
        && receipt.operation_id == stub.operation_id
        && receipt.request_epoch == stub.request_epoch
        && receipt.copy_id == stub.copy_id
        && receipt.library_serial == stub.library_serial
        && receipt.pool_id == stub.pool_id
        && receipt.volume_id == stub.volume_id
        && receipt.volume_generation == stub.volume_generation
        && receipt.volume_scan_id == stub.volume_scan_id
        && receipt.size == stub.size
        && receipt.content_digest == stub.content_digest
}

pub(super) async fn matching_at(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<Option<(ActiveFileIntent, u16)>> {
    let key = requests::epoch_prefix(b'0', receipt.namespace, receipt.request_epoch);
    let mut key = key;
    key.extend_from_slice(receipt.operation_id.as_bytes());
    let Some(bytes) = tr.get(&key, false).await? else {
        return Ok(None);
    };
    let (intent, permit) = decode(
        &bytes,
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    )?;
    if !matches_receipt(&intent, receipt) {
        return Err(CatalogError::Invalid("物理收据与写前预约不一致"));
    }
    Ok(Some((intent, permit)))
}

pub(super) async fn claims_match(tr: &Transaction, intent: &ActiveFileIntent) -> Result<bool> {
    for key in [path_key(intent), file_key(intent)] {
        if tr.get(&key, false).await?.as_deref() != Some(&claim_value(intent)[..]) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) async fn claim_conflicts(tr: &Transaction, receipt: &CommitReceipt) -> Result<bool> {
    let path = path_key_for(receipt.namespace, receipt.parent, &receipt.name);
    let file = file_key_for(receipt.namespace, receipt.file_id);
    Ok(tr.get(&path, false).await?.is_some() || tr.get(&file, false).await?.is_some())
}

pub(super) fn clear(tr: &Transaction, intent: &ActiveFileIntent) {
    tr.clear(&reservation_key(intent));
    tr.clear(&volume_key(intent));
    tr.clear(&path_key(intent));
    tr.clear(&file_key(intent));
}

pub(super) async fn volume_reservations_empty(
    tr: &Transaction,
    volume: Uuid,
    generation: u64,
) -> Result<bool> {
    for prefix in [
        volume_prefix(volume, generation),
        upgraded_prefix(volume, generation),
    ] {
        if !read_page(tr, &prefix, None, 1, false)
            .await?
            .items
            .is_empty()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl ManagedCatalog {
    /// Recover the original physical path of one outstanding observed receipt.
    /// Legacy synthetic requests without a write-before-data reservation are
    /// deliberately unsupported here; media recovery requires the durable
    /// per-volume reverse record written by `complete_active_file`.
    pub async fn recover_upgraded_active_file_intent(
        &self,
        receipt: &CommitReceipt,
        owner_epoch: u64,
    ) -> Result<ActiveFileIntent> {
        validate_receipt(receipt)?;
        if owner_epoch == 0 {
            return Err(CatalogError::Invalid("恢复文件 owner 代数无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, &receipt.library_serial).await?;
        if namespace != receipt.namespace
            || !routing::require_active_owner(&tr, namespace, &receipt.library_serial, owner_epoch)
                .await?
        {
            return Err(CatalogError::Invalid("恢复文件 owner 或命名空间不一致"));
        }
        let placement = control::verified_placement(&tr, receipt.volume_id)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != receipt.library_serial {
            return Err(CatalogError::Invalid("恢复文件卷放置不一致"));
        }
        let raw = tr
            .get(&requests::intent_key(receipt), false)
            .await?
            .ok_or(CatalogError::Corrupt("恢复文件 I 收据缺失"))?;
        let permit = requests::matching_permit(&raw, receipt)?
            .ok_or(CatalogError::Corrupt("恢复文件 I 收据不一致"))?;
        intent_for_upgraded_at(&tr, receipt, permit).await
    }

    /// List only the interrupted volume's write-before-data reservations.
    pub async fn pending_active_volume_reservations(
        &self,
        serial: &str,
        volume: Uuid,
        generation: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ActiveFileIntent>> {
        control::validate_physical_serial(serial)?;
        let prefix = volume_prefix(volume, generation);
        if volume.is_nil()
            || generation == 0
            || !(1..=100).contains(&limit)
            || after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24)
        {
            return Err(CatalogError::Invalid("活动卷预约分页参数无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("活动卷预约与带库不一致"));
        }
        let page = read_page(&tr, &prefix, after, limit, true).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, value) in page.items {
            if key.len() != prefix.len() + 24 || value.len() != 16 {
                return Err(CatalogError::Corrupt("活动卷预约倒排键或值长度"));
            }
            let epoch = read_u64(&key[prefix.len()..]);
            let operation = uuid_from(&key[prefix.len() + 8..])?;
            let row_key = requests::epoch_prefix(b'0', namespace, epoch);
            let mut row_key = row_key;
            row_key.extend_from_slice(operation.as_bytes());
            let row = tr
                .get(&row_key, false)
                .await?
                .ok_or(CatalogError::Corrupt("活动卷预约倒排缺少原记录"))?;
            let (intent, _) = decode(&row, namespace, epoch, operation)?;
            if intent.volume_id != volume
                || intent.volume_generation != generation
                || intent.library_serial != serial
                || value.as_slice() != namespace.as_bytes()
                || !claims_match(&tr, &intent).await?
            {
                return Err(CatalogError::Corrupt("活动卷预约倒排与原记录不一致"));
            }
            items.push(intent);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Locate upgraded I receipts even after an interrupted Open batch page
    /// has atomically removed its original Q reservation.
    pub async fn pending_active_volume_upgrades(
        &self,
        serial: &str,
        volume: Uuid,
        generation: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ActiveFileIntent>> {
        control::validate_physical_serial(serial)?;
        let prefix = upgraded_prefix(volume, generation);
        if volume.is_nil()
            || generation == 0
            || !(1..=100).contains(&limit)
            || after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24)
        {
            return Err(CatalogError::Invalid("活动卷升级收据分页参数无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("活动卷升级收据与带库不一致"));
        }
        let page = read_page(&tr, &prefix, after, limit, true).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, value) in page.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("活动卷升级收据倒排键长度"));
            }
            let epoch = read_u64(&key[prefix.len()..]);
            let operation = uuid_from(&key[prefix.len() + 8..])?;
            let (intent, permit) = decode(&value, namespace, epoch, operation)?;
            if intent.volume_id != volume
                || intent.volume_generation != generation
                || intent.library_serial != serial
            {
                return Err(CatalogError::Corrupt("活动卷升级收据倒排与卷身份不一致"));
            }
            let receipt_key = requests::intent_key(&receipt_stub(&intent));
            let receipt_bytes = tr
                .get(&receipt_key, false)
                .await?
                .ok_or(CatalogError::Corrupt("活动卷升级倒排缺少 I 收据"))?;
            let receipt = requests::decode_intent(&receipt_bytes, namespace, epoch, operation)?;
            if !matches_receipt(&intent, &receipt)
                || requests::matching_permit(&receipt_bytes, &receipt)? != Some(permit)
            {
                return Err(CatalogError::Corrupt("活动卷升级收据倒排与 I 不一致"));
            }
            items.push(intent);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Recover logical reservations after owner takeover. A returned request
    /// may already have unindexed data on tape; inspect the fenced medium
    /// before cancellation or retrying the append.
    pub async fn pending_active_file_reservations(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<ActiveFileIntent>> {
        let mut prefix = key(b'0');
        prefix.extend_from_slice(namespace.as_bytes());
        if !(1..=100).contains(&limit)
            || after.is_some_and(|k| !k.starts_with(&prefix) || k.len() != prefix.len() + 24)
        {
            return Err(CatalogError::Invalid("写前预约分页参数无效"));
        }
        let tr = self.db.create_trx()?;
        if tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Active as u8][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let window = requests::window(&tr, namespace).await?;
        let page = read_page(&tr, &prefix, after, limit, true).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, value) in page.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("写前预约键长度"));
            }
            let epoch = read_u64(&key[prefix.len()..]);
            if epoch < window.first_retained || epoch > window.accepting {
                return Err(CatalogError::Corrupt("写前预约代次越界"));
            }
            let (intent, _) = decode(
                &value,
                namespace,
                epoch,
                uuid_from(&key[prefix.len() + 8..])?,
            )?;
            if !claims_match(&tr, &intent).await? {
                return Err(CatalogError::Corrupt("写前预约目标占用缺失"));
            }
            items.push(intent);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Durable logical reservation. The executor must obtain the drive PR and
    /// verify the medium separately; Registered permits data records, not S4.
    pub async fn reserve_active_file(&self, intent: &ActiveFileIntent) -> Result<PublishOutcome> {
        validate_active_file_intent(intent)?;
        let intent = intent.clone();
        self.db
            .run(|tr, _| {
                let intent = intent.clone();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                intent.namespace,
                                &intent.library_serial,
                                intent.owner_epoch,
                            )
                            .await?
                            {
                                return Ok(PublishOutcome::StaleOwner);
                            }
                            let w = requests::window(&tr, intent.namespace).await?;
                            if intent.request_epoch < w.first_retained {
                                return Ok(PublishOutcome::RequestExpired);
                            }
                            if intent.request_epoch > w.accepting {
                                return Ok(PublishOutcome::RequestEpochClosed);
                            }
                            let stub = receipt_stub(&intent);
                            if let Some(terminal) = tr
                                .get(
                                    &op_key(
                                        intent.namespace,
                                        intent.request_epoch,
                                        intent.operation_id,
                                    ),
                                    false,
                                )
                                .await?
                            {
                                return requests::terminal_result(
                                    &terminal,
                                    &cancellation_fingerprint(&intent),
                                );
                            }
                            if !control::receipt_placement_matches(&tr, &stub).await? {
                                return Ok(PublishOutcome::PlacementMismatch);
                            }
                            if !parent_matches_at(&tr, &intent).await? {
                                return Ok(PublishOutcome::MissingParent);
                            }
                            let Some(current) =
                                active_volume::request_current_index(&tr, &stub).await?
                            else {
                                return Ok(PublishOutcome::VolumeNotReady);
                            };
                            if intent.volume_generation <= current.generation {
                                return Ok(PublishOutcome::GenerationConflict);
                            }
                            if current.generation.checked_add(1) != Some(intent.volume_generation) {
                                return Ok(PublishOutcome::VolumeNotReady);
                            }
                            if active_volume::recovery_active(&tr, intent.volume_id).await? {
                                return Ok(PublishOutcome::VolumeNotReady);
                            }
                            if tr
                                .get(&active_batch::claim_key(&stub), false)
                                .await?
                                .is_some()
                            {
                                return Ok(PublishOutcome::BatchClaimed);
                            }
                            let key = reservation_key(&intent);
                            if let Some(existing) = tr.get(&key, false).await? {
                                let (saved, _) = decode(
                                    &existing,
                                    intent.namespace,
                                    intent.request_epoch,
                                    intent.operation_id,
                                )?;
                                if !same_logical(&saved, &intent) {
                                    return Ok(PublishOutcome::OperationConflict);
                                }
                                if !claims_match(&tr, &intent).await? {
                                    return Err(CatalogError::Corrupt("活动写前预约目标占用缺失"));
                                }
                                if tr.get(&volume_key(&intent), false).await?.as_deref()
                                    != Some(intent.namespace.as_bytes())
                                {
                                    return Err(CatalogError::Corrupt("活动写前预约卷倒排缺失"));
                                }
                                return Ok(PublishOutcome::Registered);
                            }
                            if tr.get(&requests::intent_key(&stub), false).await?.is_some() {
                                return Ok(PublishOutcome::OperationConflict);
                            }
                            if intent.request_epoch != w.accepting {
                                return Ok(PublishOutcome::RequestEpochClosed);
                            }
                            if !active_batch::target_matches_baseline(&tr, &stub).await? {
                                return Ok(PublishOutcome::VersionConflict);
                            }
                            for claim in [path_key(&intent), file_key(&intent)] {
                                if tr.get(&claim, false).await?.is_some() {
                                    return Ok(PublishOutcome::NameConflict);
                                }
                            }
                            for claim in [
                                active_batch::path_claim_key_for(
                                    intent.namespace,
                                    intent.parent,
                                    &intent.name,
                                ),
                                active_batch::file_claim_key_for(intent.namespace, intent.file_id),
                            ] {
                                if tr.get(&claim, false).await?.is_some() {
                                    return Ok(PublishOutcome::BatchClaimed);
                                }
                            }
                            let Some(permit) = maintenance::reserve_identity(
                                &tr,
                                intent.namespace,
                                intent.request_epoch,
                                intent.operation_id,
                            )
                            .await?
                            else {
                                return Ok(PublishOutcome::AdmissionLimited);
                            };
                            tr.set(&key, &encode(&intent, permit));
                            tr.set(&volume_key(&intent), intent.namespace.as_bytes());
                            for claim in [path_key(&intent), file_key(&intent)] {
                                tr.set(&claim, &claim_value(&intent));
                            }
                            Ok(PublishOutcome::Registered)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Upgrade a logical reservation after LTFS has appended data and supplied
    /// its exact fileuid and attributes. The reservation's permit is reused.
    pub async fn complete_active_file(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        validate_receipt(receipt)?;
        let receipt = receipt.clone();
        self.db
            .run(|tr, _| {
                let receipt = receipt.clone();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                receipt.namespace,
                                &receipt.library_serial,
                                receipt.owner_epoch,
                            )
                            .await?
                            {
                                return Ok(PublishOutcome::StaleOwner);
                            }
                            if tr
                                .get(
                                    &op_key(
                                        receipt.namespace,
                                        receipt.request_epoch,
                                        receipt.operation_id,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                            {
                                return Ok(PublishOutcome::OperationConflict);
                            }
                            let Some((intent, permit)) = matching_at(&tr, &receipt).await? else {
                                return Ok(PublishOutcome::RequestNotRegistered);
                            };
                            if !claims_match(&tr, &intent).await? {
                                return Err(CatalogError::Corrupt("活动写前预约目标占用缺失"));
                            }
                            let Some(current) =
                                active_volume::request_current_index(&tr, &receipt).await?
                            else {
                                return Ok(PublishOutcome::VolumeNotReady);
                            };
                            if current.generation.checked_add(1) != Some(receipt.volume_generation)
                            {
                                return Ok(PublishOutcome::GenerationConflict);
                            }
                            if active_volume::recovery_active(&tr, receipt.volume_id).await? {
                                return Ok(PublishOutcome::VolumeNotReady);
                            }
                            if tr
                                .get(&active_batch::claim_key(&receipt), false)
                                .await?
                                .is_some()
                            {
                                return Ok(PublishOutcome::BatchClaimed);
                            }
                            let key = requests::intent_key(&receipt);
                            let reverse_key = upgraded_key(&intent);
                            let reverse_value = encode(&intent, permit);
                            if let Some(old) = tr.get(&key, false).await? {
                                if tr.get(&reverse_key, false).await?.as_deref()
                                    != Some(reverse_value.as_slice())
                                {
                                    return Err(CatalogError::Corrupt("活动升级收据卷倒排缺失"));
                                }
                                return Ok(
                                    if requests::matching_permit(&old, &receipt)? == Some(permit) {
                                        PublishOutcome::Registered
                                    } else {
                                        PublishOutcome::OperationConflict
                                    },
                                );
                            }
                            tr.set(&key, &requests::encode_intent(&receipt, permit));
                            tr.set(&reverse_key, &reverse_value);
                            Ok(PublishOutcome::Registered)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Caller must prove no S4 commit and fence all media I/O before cancelling.
    pub async fn cancel_active_file_reservation(
        &self,
        intent: &ActiveFileIntent,
    ) -> Result<PublishOutcome> {
        self.cancel_active_reservation_inner(intent, false).await
    }

    /// Only after the fenced media has been proven to contain no S4 file
    /// generation. This also clears an upgraded I receipt left before sealing.
    pub async fn cancel_recovering_active_file(
        &self,
        intent: &ActiveFileIntent,
    ) -> Result<PublishOutcome> {
        self.cancel_active_reservation_inner(intent, true).await
    }

    async fn cancel_active_reservation_inner(
        &self,
        intent: &ActiveFileIntent,
        recovery: bool,
    ) -> Result<PublishOutcome> {
        validate_active_file_intent(intent)?;
        let intent = intent.clone();
        self.db
            .run(|tr, _| {
                let intent = intent.clone();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                intent.namespace,
                                &intent.library_serial,
                                intent.owner_epoch,
                            )
                            .await?
                            {
                                return Ok(PublishOutcome::StaleOwner);
                            }
                            let key = reservation_key(&intent);
                            let reserved = tr.get(&key, false).await?;
                            let upgraded_original = if recovery {
                                tr.get(&upgraded_key(&intent), false).await?
                            } else {
                                None
                            };
                            let Some(bytes) = reserved.as_deref().or(upgraded_original.as_deref())
                            else {
                                return if let Some(terminal) = tr
                                    .get(
                                        &op_key(
                                            intent.namespace,
                                            intent.request_epoch,
                                            intent.operation_id,
                                        ),
                                        false,
                                    )
                                    .await?
                                {
                                    requests::terminal_result(
                                        &terminal,
                                        &cancellation_fingerprint(&intent),
                                    )
                                } else {
                                    Ok(PublishOutcome::RequestNotRegistered)
                                };
                            };
                            let (saved, permit) = decode(
                                bytes,
                                intent.namespace,
                                intent.request_epoch,
                                intent.operation_id,
                            )?;
                            if upgraded_original
                                .as_deref()
                                .is_some_and(|original| original != bytes)
                            {
                                return Err(CatalogError::Corrupt("升级倒排与原预约不一致"));
                            }
                            if !same_logical(&saved, &intent) {
                                return Ok(PublishOutcome::OperationConflict);
                            }
                            if tr
                                .get(
                                    &op_key(
                                        intent.namespace,
                                        intent.request_epoch,
                                        intent.operation_id,
                                    ),
                                    false,
                                )
                                .await?
                                .is_some()
                            {
                                return Err(CatalogError::Corrupt("写前预约与请求终态同时存在"));
                            }
                            let stub = receipt_stub(&intent);
                            if tr
                                .get(&active_batch::claim_key(&stub), false)
                                .await?
                                .is_some()
                            {
                                return Ok(PublishOutcome::BatchClaimed);
                            }
                            if recovery
                                && (!active_volume::recovery_active(&tr, intent.volume_id).await?
                                    || tr
                                        .get(
                                            &active_batch::batch_key(
                                                intent.volume_id,
                                                intent.volume_generation,
                                            ),
                                            false,
                                        )
                                        .await?
                                        .is_some())
                            {
                                return Ok(PublishOutcome::BatchClaimed);
                            }
                            let upgraded = tr.get(&requests::intent_key(&stub), false).await?;
                            if let Some(bytes) = upgraded.as_deref() {
                                if !recovery {
                                    return Ok(PublishOutcome::BatchClaimed);
                                }
                                if upgraded_original.is_none() {
                                    return Err(CatalogError::Corrupt("升级 I 收据缺少卷倒排"));
                                }
                                let receipt = requests::decode_intent(
                                    bytes,
                                    intent.namespace,
                                    intent.request_epoch,
                                    intent.operation_id,
                                )?;
                                if !matches_receipt(&saved, &receipt)
                                    || requests::matching_permit(bytes, &receipt)? != Some(permit)
                                {
                                    return Err(CatalogError::Corrupt("活动收尾收据与预约不一致"));
                                }
                            } else if upgraded_original.is_some() {
                                return Err(CatalogError::Corrupt("升级卷倒排缺少 I 收据"));
                            }
                            let Some(current) =
                                active_volume::request_current_index(&tr, &stub).await?
                            else {
                                return Ok(PublishOutcome::VolumeNotReady);
                            };
                            if current.generation >= intent.volume_generation {
                                return Ok(PublishOutcome::GenerationConflict);
                            }
                            if reserved.is_some() {
                                if !claims_match(&tr, &intent).await? {
                                    return Err(CatalogError::Corrupt("活动写前预约目标占用缺失"));
                                }
                                if tr.get(&volume_key(&intent), false).await?.as_deref()
                                    != Some(intent.namespace.as_bytes())
                                {
                                    return Err(CatalogError::Corrupt("活动写前预约卷倒排缺失"));
                                }
                            }
                            let release =
                                maintenance::release(&tr, intent.namespace, permit).await?;
                            release.apply(&tr);
                            tr.set(
                                &op_key(
                                    intent.namespace,
                                    intent.request_epoch,
                                    intent.operation_id,
                                ),
                                &requests::terminal_value(&cancellation_fingerprint(&intent), None),
                            );
                            if upgraded.is_some() {
                                tr.clear(&requests::intent_key(&stub));
                                tr.clear(&upgraded_key(&intent));
                            }
                            if reserved.is_some() {
                                clear(&tr, &intent);
                            }
                            Ok(PublishOutcome::RequestCancelled)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_record_binds_logical_identity_and_detects_corruption() {
        let intent = ActiveFileIntent {
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            parent_path: b"/".to_vec(),
            name: b"name".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 4,
            library_serial: "TS4300-SERIAL".into(),
            pool_id: Some(Uuid::new_v4()),
            owner_epoch: 6,
            volume_id: Uuid::new_v4(),
            volume_generation: 3,
            volume_scan_id: Uuid::new_v4(),
            expected_size: 123,
            expected_digest: ContentDigest::Sha256([9; 32]),
        };
        let record = encode(&intent, 7);
        assert_eq!(
            decode(
                &record,
                intent.namespace,
                intent.request_epoch,
                intent.operation_id
            )
            .unwrap(),
            (intent.clone(), 7)
        );
        let mut damaged = record.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(
            decode(
                &damaged,
                intent.namespace,
                intent.request_epoch,
                intent.operation_id
            )
            .is_err()
        );
        assert!(
            decode(
                &record,
                intent.namespace,
                intent.request_epoch,
                Uuid::new_v4()
            )
            .is_err()
        );
        let mut takeover = intent.clone();
        takeover.owner_epoch += 1;
        assert!(same_logical(&intent, &takeover));
        assert_eq!(
            cancellation_fingerprint(&intent),
            cancellation_fingerprint(&takeover)
        );
        assert_ne!(
            cancellation_fingerprint(&intent).to_vec(),
            receipt_fingerprint(&receipt_stub(&intent))
        );
        let mut moved = intent.clone();
        moved.parent_path = b"/moved".to_vec();
        assert_ne!(
            cancellation_fingerprint(&intent),
            cancellation_fingerprint(&moved)
        );
        moved.parent_path = vec![b'/', 0xff];
        assert!(validate_active_file_intent(&moved).is_err());
        takeover.volume_generation += 1;
        assert!(!same_logical(&intent, &takeover));
    }
}
