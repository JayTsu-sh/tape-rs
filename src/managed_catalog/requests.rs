//! Explicit request generations: admission before media I/O, resolution before
//! expiry, and bounded collection without forgetting how to reject old retries.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestWindow {
    pub first_retained: u64,
    pub accepting: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowOutcome {
    Advanced(RequestWindow),
    Conflict(RequestWindow),
    PendingRequests,
    RetainedReferences,
    RetentionLimited,
}

pub(super) fn window_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'E');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn epoch_prefix(kind: u8, namespace: Uuid, epoch: u64) -> Vec<u8> {
    let mut out = key(kind);
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out
}

pub(super) fn intent_key(r: &CommitReceipt) -> Vec<u8> {
    let mut out = epoch_prefix(b'I', r.namespace, r.request_epoch);
    out.extend_from_slice(r.operation_id.as_bytes());
    out
}

pub(super) fn encode_intent(r: &CommitReceipt, permit: u16) -> Vec<u8> {
    let payload = serde_json::json!({
        "permit": permit,
        "parent": r.parent.to_string(), "name": r.name, "file": r.file_id.to_string(),
        "previous": r.expected_version, "copy": r.copy_id.to_string(),
        "serial": r.library_serial, "owner": r.owner_epoch, "volume": r.volume_id.to_string(),
        "pool": r.pool_id.map(|id| id.to_string()),
        "generation": r.volume_generation, "scan": r.volume_scan_id.to_string(),
        "size": r.size, "fileuid": r.ltfs_file_uid,
        "hash_kind": r.content_digest.tag(), "hash_bytes": r.content_digest.payload(),
        "attributes": r.metadata_digest,
    });
    let mut out = intent_digest(r, permit);
    out.extend_from_slice(payload.to_string().as_bytes());
    out
}

pub(super) fn decode_intent(
    bytes: &[u8],
    namespace: Uuid,
    request_epoch: u64,
    operation_id: Uuid,
) -> Result<CommitReceipt> {
    let corrupt = || CatalogError::Corrupt("请求准入记录字段或指纹");
    if !(33..=8192).contains(&bytes.len()) {
        return Err(corrupt());
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes[32..]).map_err(|_| corrupt())?;
    let number = |name: &str| value.get(name).and_then(|v| v.as_u64()).ok_or_else(corrupt);
    let id = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<Uuid>().ok())
            .ok_or_else(corrupt)
    };
    let r = CommitReceipt {
        namespace,
        request_epoch,
        operation_id,
        parent: id("parent")?,
        file_id: id("file")?,
        copy_id: id("copy")?,
        name: serde_json::from_value(value.get("name").ok_or_else(corrupt)?.clone())
            .map_err(|_| corrupt())?,
        expected_version: if value.get("previous").is_some_and(|v| v.is_null()) {
            None
        } else {
            Some(number("previous")?)
        },
        library_serial: value
            .get("serial")
            .and_then(|v| v.as_str())
            .ok_or_else(corrupt)?
            .into(),
        pool_id: if value.get("pool").is_some_and(|v| v.is_null()) {
            None
        } else {
            Some(id("pool")?)
        },
        owner_epoch: number("owner")?,
        volume_id: id("volume")?,
        volume_generation: number("generation")?,
        volume_scan_id: id("scan")?,
        ltfs_file_uid: number("fileuid")?,
        size: number("size")?,
        content_digest: ContentDigest::decode(
            u8::try_from(number("hash_kind")?).map_err(|_| corrupt())?,
            &serde_json::from_value::<[u8; 32]>(
                value.get("hash_bytes").ok_or_else(corrupt)?.clone(),
            )
            .map_err(|_| corrupt())?,
        )
        .map_err(|_| corrupt())?,
        metadata_digest: serde_json::from_value(
            value.get("attributes").ok_or_else(corrupt)?.clone(),
        )
        .map_err(|_| corrupt())?,
    };
    validate_receipt(&r).map_err(|_| corrupt())?;
    let permit = permit_from_json(&value)?;
    if r.copy_id != derive_copy_id(&r) || intent_digest(&r, permit) != bytes[..32] {
        return Err(corrupt());
    }
    Ok(r)
}

fn intent_digest(r: &CommitReceipt, permit: u16) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"tape-rs/admission/3");
    digest.update(receipt_fingerprint(r));
    digest.update(permit.to_be_bytes());
    digest.finalize().to_vec()
}
fn permit_from_json(value: &serde_json::Value) -> Result<u16> {
    value
        .get("permit")
        .and_then(|v| v.as_u64())
        .filter(|v| *v < 64)
        .map(|v| v as u16)
        .ok_or(CatalogError::Corrupt("请求准入配额分片"))
}
pub(super) fn matching_permit(bytes: &[u8], r: &CommitReceipt) -> Result<Option<u16>> {
    let registered = decode_intent(bytes, r.namespace, r.request_epoch, r.operation_id)?;
    if receipt_fingerprint(&registered) != receipt_fingerprint(r) {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes[32..]).map_err(|_| CatalogError::Corrupt("请求准入记录"))?;
    Ok(Some(permit_from_json(&value)?))
}
fn intent_matches(bytes: &[u8], r: &CommitReceipt) -> Result<bool> {
    Ok(matching_permit(bytes, r)?.is_some())
}

fn pin_key(namespace: Uuid, epoch: u64, id: Uuid) -> Vec<u8> {
    let mut out = epoch_prefix(b'F', namespace, epoch);
    out.extend_from_slice(id.as_bytes());
    out
}

pub(super) fn encode_window(window: RequestWindow) -> Vec<u8> {
    let mut out = window.first_retained.to_be_bytes().to_vec();
    out.extend_from_slice(&window.accepting.to_be_bytes());
    out
}

pub(super) fn decode_window(bytes: &[u8]) -> Result<RequestWindow> {
    if bytes.len() != 16 {
        return Err(CatalogError::Corrupt("请求代次窗口长度"));
    }
    let window = RequestWindow {
        first_retained: read_u64(bytes),
        accepting: read_u64(&bytes[8..]),
    };
    if window.first_retained == 0 || window.first_retained > window.accepting {
        return Err(CatalogError::Corrupt("请求代次窗口顺序"));
    }
    Ok(window)
}

pub(super) async fn window(tr: &Transaction, namespace: Uuid) -> Result<RequestWindow> {
    let bytes = tr.get(&window_key(namespace), false).await?;
    decode_window(
        bytes
            .as_deref()
            .ok_or(CatalogError::Corrupt("请求代次窗口缺失"))?,
    )
}

pub(super) fn terminal_result(bytes: &[u8], fingerprint: &[u8]) -> Result<PublishOutcome> {
    if bytes.len() != 41
        || bytes[0] > 1
        || (bytes[0] == 0 && read_u64(&bytes[33..]) == 0)
        || (bytes[0] == 1 && read_u64(&bytes[33..]) != 0)
    {
        return Err(CatalogError::Corrupt("请求终态标记"));
    }
    Ok(if &bytes[1..33] != fingerprint {
        PublishOutcome::OperationConflict
    } else if bytes[0] == 0 {
        PublishOutcome::AlreadyApplied(read_u64(&bytes[33..]))
    } else {
        PublishOutcome::RequestCancelled
    })
}

pub(super) fn terminal_value(fingerprint: &[u8], version: Option<u64>) -> Vec<u8> {
    let mut out = vec![u8::from(version.is_none())];
    out.extend_from_slice(fingerprint);
    out.extend_from_slice(&version.unwrap_or(0).to_be_bytes());
    out
}

pub(super) async fn prefix_has_rows(tr: &Transaction, prefix: &[u8]) -> Result<bool> {
    Ok(!read_page(tr, prefix, None, 1, false)
        .await?
        .items
        .is_empty())
}

pub(super) async fn has_namespace_holds(tr: &Transaction, namespace: Uuid) -> Result<bool> {
    for kind in *b"IF" {
        let mut prefix = key(kind);
        prefix.extend_from_slice(namespace.as_bytes());
        if prefix_has_rows(tr, &prefix).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn older_rows(
    tr: &Transaction,
    kind: u8,
    namespace: Uuid,
    before: u64,
) -> Result<bool> {
    let begin = epoch_prefix(kind, namespace, 0);
    let end = epoch_prefix(kind, namespace, before);
    let range = RangeOption {
        begin: KeySelector::first_greater_or_equal(begin),
        end: KeySelector::first_greater_or_equal(end),
        limit: Some(1),
        ..RangeOption::default()
    };
    Ok(!tr.get_range(&range, 1, false).await?.is_empty())
}

pub(super) async fn require_namespace(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    match state.as_deref().map(decode_namespace_state).transpose()? {
        Some(NamespaceState::Ready | NamespaceState::Building) => Ok(()),
        _ => Err(CatalogError::Incomplete),
    }
}

pub(super) async fn require_recorded_namespace(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    match state.as_deref().map(decode_namespace_state).transpose()? {
        Some(state) if state != NamespaceState::Discarded => Ok(()),
        _ => Err(CatalogError::Incomplete),
    }
}

async fn require_active_window_owner(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    owner_epoch: u64,
) -> Result<()> {
    control::validate_physical_serial(serial)?;
    if !routing::require_active_owner(tr, namespace, serial, owner_epoch).await? {
        return Err(CatalogError::MaintenanceFenced);
    }
    Ok(())
}

// Preserve FDB retry behavior even when helpers return domain errors.
pub(super) fn retry_result<T>(
    result: Result<T>,
) -> std::result::Result<Result<T>, FdbBindingError> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(CatalogError::Fdb(error)) => Err(error),
        Err(CatalogError::Client(error)) => Err(error.into()),
        Err(error) => Ok(Err(error)),
    }
}

impl ManagedCatalog {
    /// Recover admitted work from FDB without a node's SQLite/outbox. The saved
    /// owner credential must be replaced after real owner/PR takeover fencing.
    pub async fn pending_requests(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<CommitReceipt>> {
        let mut prefix = key(b'I');
        prefix.extend_from_slice(namespace.as_bytes());
        if !(1..=100).contains(&limit)
            || after.is_some_and(|k| !k.starts_with(&prefix) || k.len() != prefix.len() + 24)
        {
            return Err(CatalogError::Invalid("待处理请求分页参数无效"));
        }
        let tr = self.db.create_trx()?;
        require_recorded_namespace(&tr, namespace).await?;
        let active = tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..]);
        let w = window(&tr, namespace).await?;
        let page = read_page(&tr, &prefix, after, limit, true).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, value) in page.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("待处理请求键长度"));
            }
            let epoch = read_u64(&key[prefix.len()..]);
            if epoch < w.first_retained || epoch > w.accepting {
                return Err(CatalogError::Corrupt("待处理请求代次越界"));
            }
            let receipt = decode_intent(
                &value,
                namespace,
                epoch,
                uuid_from(&key[prefix.len() + 8..])?,
            )?;
            if active && let Some(claim) = tr.get(&active_batch::claim_key(&receipt), true).await? {
                let batch_id = uuid_from(&claim)?;
                if batch_id.is_nil() {
                    return Err(CatalogError::Corrupt("活动请求批次占用身份无效"));
                }
                let locator = active_batch::TargetLocator::from_receipt(batch_id, &receipt);
                if active_batch::published_at(&tr, locator).await? {
                    if !active_batch::target_claims_match(&tr, &receipt, batch_id).await? {
                        return Err(CatalogError::Corrupt("已发布请求目标占用不一致"));
                    }
                    continue;
                }
            }
            items.push(receipt);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }
    pub async fn request_window(&self, namespace: Uuid) -> Result<RequestWindow> {
        let tr = self.db.create_trx()?;
        require_recorded_namespace(&tr, namespace).await?;
        window(&tr, namespace).await
    }

    /// Persist before issuing media I/O. Unresolved admissions block expiry.
    /// Rotation closes new admission to an older epoch, not its pending results.
    pub async fn register_request(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        self.register_request_in_state(receipt, false).await
    }

    /// Admit work into an activated library only under its current FDB owner
    /// epoch. The executor must separately hold the device PR before media I/O.
    pub async fn register_active_request(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        self.register_request_in_state(receipt, true).await
    }

    async fn register_request_in_state(
        &self,
        receipt: &CommitReceipt,
        active: bool,
    ) -> Result<PublishOutcome> {
        validate_receipt(receipt)?;
        let r = receipt.clone();
        self.db
            .run(|tr, _| {
                let r = r.clone();
                async move {
                    retry_result(
                        async {
                            if active {
                                if tr.get(&namespace_key(r.namespace), false).await?.as_deref()
                                    != Some(&[NamespaceState::Active as u8][..])
                                {
                                    return Err(CatalogError::Incomplete);
                                }
                                if !routing::require_active_owner(
                                    &tr,
                                    r.namespace,
                                    &r.library_serial,
                                    r.owner_epoch,
                                )
                                .await?
                                {
                                    return Ok(PublishOutcome::StaleOwner);
                                }
                            } else {
                                require_namespace(&tr, r.namespace).await?;
                            }
                            let w = window(&tr, r.namespace).await?;
                            if r.request_epoch < w.first_retained {
                                return Ok(PublishOutcome::RequestExpired);
                            }
                            if r.request_epoch > w.accepting {
                                return Ok(PublishOutcome::RequestEpochClosed);
                            }
                            let fingerprint = receipt_fingerprint(&r);
                            if let Some(old) = tr
                                .get(&op_key(r.namespace, r.request_epoch, r.operation_id), false)
                                .await?
                            {
                                return terminal_result(&old, &fingerprint);
                            }
                            if !control::receipt_placement_matches(&tr, &r).await? {
                                return Ok(PublishOutcome::PlacementMismatch);
                            }
                            if active {
                                let Some(current) =
                                    active_volume::request_current_index(&tr, &r).await?
                                else {
                                    return Ok(PublishOutcome::VolumeNotReady);
                                };
                                if r.volume_generation <= current.generation {
                                    // A published batch is the durable terminal
                                    // result even when its volume generation is
                                    // no longer the next writable generation.
                                    if let Some(claim) =
                                        tr.get(&active_batch::claim_key(&r), false).await?
                                    {
                                        if claim.len() != 16 || uuid_from(&claim)?.is_nil() {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次请求占用标记",
                                            ));
                                        }
                                        let batch_id = uuid_from(&claim)?;
                                        let locator =
                                            active_batch::TargetLocator::from_receipt(batch_id, &r);
                                        if active_batch::published_at(&tr, locator).await? {
                                            let intent =
                                                tr.get(&intent_key(&r), false).await?.ok_or(
                                                    CatalogError::Corrupt("已发布请求准入意图缺失"),
                                                )?;
                                            if matching_permit(&intent, &r)?.is_none() {
                                                return Ok(PublishOutcome::OperationConflict);
                                            }
                                            // Reuse the full file-reader proof:
                                            // the small publication marker by
                                            // itself is not a valid media result.
                                            lookup::published_receipt_at(&tr, r.namespace, locator)
                                                .await?
                                                .ok_or(CatalogError::Corrupt(
                                                    "已发布请求完整证明缺失",
                                                ))?;
                                            let version = r.expected_version.unwrap_or(0) + 1;
                                            return Ok(PublishOutcome::AlreadyApplied(version));
                                        }
                                    }
                                    return Ok(PublishOutcome::GenerationConflict);
                                }
                                if current.generation.checked_add(1) != Some(r.volume_generation) {
                                    return Ok(PublishOutcome::VolumeNotReady);
                                }
                                if let Some(claim) =
                                    tr.get(&active_batch::claim_key(&r), false).await?
                                {
                                    if claim.len() != 16 || uuid_from(&claim)?.is_nil() {
                                        return Err(CatalogError::Corrupt("活动批次请求占用标记"));
                                    }
                                    return Ok(PublishOutcome::BatchClaimed);
                                }
                            }
                            let intent = intent_key(&r);
                            if let Some(old) = tr.get(&intent, false).await? {
                                return Ok(if intent_matches(&old, &r)? {
                                    PublishOutcome::Registered
                                } else {
                                    PublishOutcome::OperationConflict
                                });
                            }
                            if active && reservation::claim_conflicts(&tr, &r).await? {
                                return Ok(PublishOutcome::BatchClaimed);
                            }
                            if r.request_epoch != w.accepting {
                                return Ok(PublishOutcome::RequestEpochClosed);
                            }
                            if r.copy_id != derive_copy_id(&r) {
                                return Ok(PublishOutcome::InvalidCopyId);
                            }
                            let Some(permit) = maintenance::reserve(&tr, &r).await? else {
                                return Ok(PublishOutcome::AdmissionLimited);
                            };
                            tr.set(&intent, &encode_intent(&r, permit));
                            Ok(PublishOutcome::Registered)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Cancel only after the executor has stopped/fenced media I/O and proved
    /// there was no media commit. Committed work must publish; an unknown media
    /// result must remain pending for reconciliation.
    pub async fn cancel_request(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        validate_receipt(receipt)?;
        let r = receipt.clone();
        self.db
            .run(|tr, _| {
                let r = r.clone();
                async move {
                    retry_result(
                        async {
                            require_recorded_namespace(&tr, r.namespace).await?;
                            let active =
                                tr.get(&namespace_key(r.namespace), false).await?.as_deref()
                                    == Some(&[NamespaceState::Active as u8][..]);
                            if active
                                && !routing::require_active_owner(
                                    &tr,
                                    r.namespace,
                                    &r.library_serial,
                                    r.owner_epoch,
                                )
                                .await?
                            {
                                return Ok(PublishOutcome::StaleOwner);
                            }
                            let w = window(&tr, r.namespace).await?;
                            if r.request_epoch < w.first_retained {
                                return Ok(PublishOutcome::RequestExpired);
                            }
                            if r.request_epoch > w.accepting {
                                return Ok(PublishOutcome::RequestEpochClosed);
                            }
                            let fingerprint = receipt_fingerprint(&r);
                            let op = op_key(r.namespace, r.request_epoch, r.operation_id);
                            if let Some(old) = tr.get(&op, false).await? {
                                return terminal_result(&old, &fingerprint);
                            }
                            let intent = intent_key(&r);
                            let old = tr.get(&intent, false).await?;
                            let Some(old) = old else {
                                return Ok(PublishOutcome::RequestNotRegistered);
                            };
                            let Some(permit) = matching_permit(&old, &r)? else {
                                return Ok(PublishOutcome::OperationConflict);
                            };
                            let owner = tr.get(&owner_key(&r.library_serial), false).await?;
                            if owner.as_ref().is_some_and(|bytes| bytes.len() != 8) {
                                return Err(CatalogError::Corrupt("owner epoch 长度"));
                            }
                            if owner.as_ref().map(|b| read_u64(b)) != Some(r.owner_epoch) {
                                return Ok(PublishOutcome::StaleOwner);
                            }
                            if active {
                                let Some(current) =
                                    active_volume::request_current_index(&tr, &r).await?
                                else {
                                    return Ok(PublishOutcome::VolumeNotReady);
                                };
                                if r.volume_generation <= current.generation {
                                    return Ok(PublishOutcome::GenerationConflict);
                                }
                                if let Some(claim) =
                                    tr.get(&active_batch::claim_key(&r), false).await?
                                {
                                    if claim.len() != 16 || uuid_from(&claim)?.is_nil() {
                                        return Err(CatalogError::Corrupt("活动批次请求占用标记"));
                                    }
                                    return Ok(PublishOutcome::BatchClaimed);
                                }
                            }
                            if let Some((reserved, reserved_permit)) =
                                reservation::matching_at(&tr, &r).await?
                            {
                                if reserved_permit != permit
                                    || !reservation::claims_match(&tr, &reserved).await?
                                {
                                    return Err(CatalogError::Corrupt("取消请求的写前预约不一致"));
                                }
                                reservation::clear(&tr, &reserved);
                            }
                            let release = maintenance::release(&tr, r.namespace, permit).await?;
                            release.apply(&tr);
                            tr.set(&op, &terminal_value(&fingerprint, None));
                            tr.clear(&intent);
                            tr.clear(&reservation::upgraded_key_for(&r));
                            Ok(PublishOutcome::RequestCancelled)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub async fn rotate_request_epoch(
        &self,
        namespace: Uuid,
        expected: u64,
    ) -> Result<WindowOutcome> {
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_namespace(&tr, namespace).await?;
                        maintenance::rotate(&tr, namespace, expected).await
                    }
                    .await,
                )
            })
            .await?
    }

    /// Rotate an Active request window under the same route and owner fence as
    /// admission. A takeover or route change conflicts with this transaction.
    pub async fn rotate_active_request_epoch(
        &self,
        namespace: Uuid,
        serial: &str,
        owner_epoch: u64,
        expected: u64,
    ) -> Result<WindowOutcome> {
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_active_window_owner(&tr, namespace, serial, owner_epoch).await?;
                        maintenance::rotate(&tr, namespace, expected).await
                    }
                    .await,
                )
            })
            .await?
    }

    /// Explicitly expire resolved epochs. Outstanding requests and retained
    /// history references are checked transactionally, without global counters.
    pub async fn advance_request_floor(
        &self,
        namespace: Uuid,
        expected: u64,
        before: u64,
    ) -> Result<WindowOutcome> {
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_namespace(&tr, namespace).await?;
                        advance_floor_at(&tr, namespace, expected, before).await
                    }
                    .await,
                )
            })
            .await?
    }

    /// Expire resolved Active requests only while this library's current owner
    /// still holds the FDB authority. Unresolved intents and pins remain gates.
    pub async fn advance_active_request_floor(
        &self,
        namespace: Uuid,
        serial: &str,
        owner_epoch: u64,
        expected: u64,
        before: u64,
    ) -> Result<WindowOutcome> {
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_active_window_owner(&tr, namespace, serial, owner_epoch).await?;
                        advance_floor_at(&tr, namespace, expected, before).await
                    }
                    .await,
                )
            })
            .await?
    }

    /// Hold metadata history while a reader/exporter still needs it. Initial
    /// implementation pauses namespace history compaction while any pin exists.
    pub async fn pin_request_history(&self, namespace: Uuid, epoch: u64, id: Uuid) -> Result<bool> {
        if id.is_nil() {
            return Err(CatalogError::Invalid("历史引用 ID 不能为 nil"));
        }
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_namespace(&tr, namespace).await?;
                        let w = window(&tr, namespace).await?;
                        if epoch < w.first_retained || epoch > w.accepting {
                            return Err(CatalogError::Invalid("历史引用的请求代次已过期或未开放"));
                        }
                        let key = pin_key(namespace, epoch, id);
                        if tr.get(&key, false).await?.is_some() {
                            return Ok(false);
                        }
                        tr.set(&key, &[]);
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn release_request_history(
        &self,
        namespace: Uuid,
        epoch: u64,
        id: Uuid,
    ) -> Result<()> {
        if namespace.is_nil() || epoch == 0 || id.is_nil() {
            return Err(CatalogError::Invalid("历史引用参数无效"));
        }
        self.db
            .run(|tr, _| async move {
                tr.clear(&pin_key(namespace, epoch, id));
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn purge_expired_requests(&self, namespace: Uuid, limit: usize) -> Result<usize> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("请求清理批次必须在 1..=1000"));
        }
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_namespace(&tr, namespace).await?;
                        purge_expired_page(&tr, namespace, limit).await
                    }
                    .await,
                )
            })
            .await?
    }

    /// Bounded terminal-marker cleanup for an Active namespace. The floor must
    /// have advanced first; old retries are still rejected by the window.
    pub async fn purge_active_expired_requests(
        &self,
        namespace: Uuid,
        serial: &str,
        owner_epoch: u64,
        limit: usize,
    ) -> Result<usize> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("请求清理批次必须在 1..=1000"));
        }
        self.db
            .run(|tr, _| async move {
                retry_result(
                    async {
                        require_active_window_owner(&tr, namespace, serial, owner_epoch).await?;
                        purge_expired_page(&tr, namespace, limit).await
                    }
                    .await,
                )
            })
            .await?
    }
}

async fn advance_floor_at(
    tr: &Transaction,
    namespace: Uuid,
    expected: u64,
    before: u64,
) -> Result<WindowOutcome> {
    let mut w = window(tr, namespace).await?;
    if w.first_retained != expected {
        return Ok(WindowOutcome::Conflict(w));
    }
    if before <= w.first_retained || before > w.accepting {
        return Err(CatalogError::Invalid("过期水位必须前进且不能越过准入代次"));
    }
    if older_rows(tr, b'I', namespace, before).await? {
        return Ok(WindowOutcome::PendingRequests);
    }
    if older_rows(tr, b'0', namespace, before).await? {
        return Ok(WindowOutcome::PendingRequests);
    }
    if older_rows(tr, b'F', namespace, before).await? {
        return Ok(WindowOutcome::RetainedReferences);
    }
    w.first_retained = before;
    tr.set(&window_key(namespace), &encode_window(w));
    Ok(WindowOutcome::Advanced(w))
}

pub(super) async fn purge_expired_page(
    tr: &Transaction,
    namespace: Uuid,
    limit: usize,
) -> Result<usize> {
    let w = window(tr, namespace).await?;
    let range = RangeOption {
        begin: KeySelector::first_greater_or_equal(epoch_prefix(b'P', namespace, 0)),
        end: KeySelector::first_greater_or_equal(epoch_prefix(b'P', namespace, w.first_retained)),
        limit: Some(limit),
        ..RangeOption::default()
    };
    let rows = tr.get_range(&range, 1, false).await?;
    for row in rows.iter() {
        tr.clear(row.key());
    }
    Ok(rows.len())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn window_terminal_and_durable_intent_validation() {
        for bytes in [&[][..], &[0; 15], &[0; 16], &[0; 17]] {
            assert!(decode_window(bytes).is_err());
        }
        assert!(
            decode_window(&encode_window(RequestWindow {
                first_retained: 2,
                accepting: 1
            }))
            .is_err()
        );
        let fingerprint = [3; 32];
        assert_eq!(
            terminal_result(&terminal_value(&fingerprint, Some(1)), &fingerprint).unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        assert_eq!(
            terminal_result(&terminal_value(&fingerprint, None), &fingerprint).unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert!(terminal_result(&terminal_value(&fingerprint, Some(0)), &fingerprint).is_err());
        assert_eq!(
            terminal_result(&terminal_value(&fingerprint, Some(1)), &[4; 32]).unwrap(),
            PublishOutcome::OperationConflict
        );
        let r = receipt(
            Uuid::new_v4(),
            "serial",
            Uuid::new_v4(),
            VolumeScan {
                generation: 1,
                id: Uuid::new_v4(),
            },
            1,
            1,
            None,
        );
        let bytes = encode_intent(&r, 0);
        assert_eq!(
            decode_intent(&bytes, r.namespace, r.request_epoch, r.operation_id).unwrap(),
            r
        );
        for digest in [
            ContentDigest::Unknown,
            ContentDigest::Md5([9; 16]),
            ContentDigest::Sha256([0; 32]),
        ] {
            let mut variant = r.clone();
            variant.content_digest = digest;
            let encoded = encode_intent(&variant, 0);
            assert_eq!(
                decode_intent(&encoded, variant.namespace, 1, variant.operation_id).unwrap(),
                variant
            );
            assert_eq!(matching_permit(&encoded, &r).unwrap(), None);
        }
        for length in [0, 31, 32, bytes.len() - 1] {
            assert!(decode_intent(&bytes[..length], r.namespace, 1, r.operation_id).is_err());
        }
        let mut bad = bytes.clone();
        bad[0] ^= 1;
        assert!(decode_intent(&bad, r.namespace, 1, r.operation_id).is_err());
        assert!(decode_intent(&bytes, Uuid::new_v4(), 1, r.operation_id).is_err());
    }

    fn receipt(
        ns: Uuid,
        serial: &str,
        volume: Uuid,
        scan: VolumeScan,
        file: u128,
        epoch: u64,
        previous: Option<u64>,
    ) -> CommitReceipt {
        test_receipt(CommitReceipt {
            namespace: ns,
            request_epoch: epoch,
            parent: Uuid::nil(),
            name: format!("f-{file}").into_bytes(),
            file_id: Uuid::from_u128(file),
            expected_version: previous,
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::nil(),
            library_serial: serial.into(),
            pool_id: None,
            owner_epoch: 10,
            volume_id: volume,
            volume_generation: scan.generation,
            volume_scan_id: scan.id,
            ltfs_file_uid: 1,
            size: 1,
            content_digest: ContentDigest::Sha256([5; 32]),
            metadata_digest: None,
        })
    }

    async fn ready(c: &ManagedCatalog, serial: &str, volume: Uuid, scan: VolumeScan) {
        c.begin_volume_reconcile(serial, 10, volume, scan)
            .await
            .unwrap();
        super::super::tests::verify_scan(c, serial, 10, volume, scan, &[]).await;
        c.mark_volume_ready(serial, 10, volume, scan).await.unwrap();
    }

    async fn raw(c: &ManagedCatalog, key: &[u8]) -> Option<Vec<u8>> {
        c.db.create_trx()
            .unwrap()
            .get(key, true)
            .await
            .unwrap()
            .map(|v| v.to_vec())
    }

    pub(crate) async fn exercise(c: &ManagedCatalog, cluster_file: &str) {
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        let serial = format!("requests-{}", Uuid::new_v4());
        c.advance_owner_epoch(&serial, 10).await.unwrap();
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        ready(c, &serial, volume, scan).await;
        let mut published = Vec::new();
        for file in 1..=1001 {
            let r = receipt(ns, &serial, volume, scan, file, 1, None);
            assert_eq!(
                c.publish(&r).await.unwrap(),
                PublishOutcome::RequestNotRegistered
            );
            assert_eq!(
                c.register_request(&r).await.unwrap(),
                PublishOutcome::Registered
            );
            published.push(r);
        }
        let recovered = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = recovered
                .pending_requests(ns, cursor.as_deref(), 97)
                .await
                .unwrap();
            for r in &page.items {
                let source = &published[r.file_id.as_u128() as usize - 1];
                assert_eq!(r, source);
            }
            count += page.items.len();
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(count, 1001);
        for r in &published {
            assert_eq!(
                recovered.publish(r).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            assert!(raw(c, &intent_key(r)).await.is_none());
        }
        let first = &published[0];
        let id = derive_copy_id(first);
        let changed = CommitReceipt {
            owner_epoch: 11,
            ..first.clone()
        };
        assert_eq!(derive_copy_id(&changed), id);
        for changed in [
            CommitReceipt {
                request_epoch: 2,
                ..first.clone()
            },
            CommitReceipt {
                operation_id: Uuid::new_v4(),
                ..first.clone()
            },
            CommitReceipt {
                file_id: Uuid::new_v4(),
                ..first.clone()
            },
            CommitReceipt {
                expected_version: Some(1),
                ..first.clone()
            },
        ] {
            assert_ne!(derive_copy_id(&changed), id);
        }

        let pending = receipt(ns, &serial, volume, scan, 2000, 1, None);
        c.register_request(&pending).await.unwrap();
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let page = reopened.pending_requests(ns, None, 1).await.unwrap();
        assert_eq!(page.items, vec![pending.clone()]);
        assert!(page.next.is_none());
        assert_eq!(
            c.register_request(&pending).await.unwrap(),
            PublishOutcome::Registered
        );
        let in_flight = c.db.create_trx().unwrap();
        window(&in_flight, ns).await.unwrap();
        let late = receipt(ns, &serial, volume, scan, 2001, 1, None);
        in_flight.set(&intent_key(&late), &receipt_fingerprint(&late));
        assert_eq!(
            c.rotate_request_epoch(ns, 1).await.unwrap(),
            WindowOutcome::Advanced(RequestWindow {
                first_retained: 1,
                accepting: 2
            })
        );
        assert!(in_flight.commit().await.is_err());
        assert_eq!(
            c.register_request(&late).await.unwrap(),
            PublishOutcome::RequestEpochClosed
        );
        assert_eq!(
            c.rotate_request_epoch(ns, 1).await.unwrap(),
            WindowOutcome::Conflict(RequestWindow {
                first_retained: 1,
                accepting: 2
            })
        );
        assert_eq!(
            c.advance_request_floor(ns, 1, 2).await.unwrap(),
            WindowOutcome::PendingRequests
        );
        let stale = CommitReceipt {
            owner_epoch: 9,
            ..pending.clone()
        };
        assert_eq!(
            c.cancel_request(&stale).await.unwrap(),
            PublishOutcome::StaleOwner
        );
        assert_eq!(
            c.cancel_request(&pending).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert_eq!(
            c.cancel_request(&pending).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert_eq!(
            c.publish(&pending).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert_eq!(
            c.register_request(&pending).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert!(c.advance_request_floor(ns, 1, 3).await.is_err());
        let new_scan = VolumeScan {
            generation: 2,
            id: Uuid::new_v4(),
        };
        ready(c, &serial, volume, new_scan).await;
        let replacement = receipt(ns, &serial, volume, new_scan, 1, 2, Some(1));
        c.register_request(&replacement).await.unwrap();
        assert_eq!(
            c.publish(&replacement).await.unwrap(),
            PublishOutcome::Applied(2)
        );
        let pin = Uuid::new_v4();
        assert!(c.pin_request_history(ns, 1, pin).await.unwrap());
        assert!(!c.pin_request_history(ns, 1, pin).await.unwrap());
        assert!(matches!(
            c.compact_superseded_copies(ns, None, 100).await,
            Err(CatalogError::Incomplete)
        ));
        c.release_request_history(ns, 1, pin).await.unwrap();
        let page = c.compact_superseded_copies(ns, None, 100).await.unwrap();
        assert_eq!((page.compacted, page.forgotten), (1, 0));
        let old_copy = copy_key(ns, first.file_id, first.copy_id);
        assert_eq!(raw(c, &old_copy).await.unwrap().len(), 17);

        // A history pin racing the empty-range check must prevent expiry.
        let in_flight = c.db.create_trx().unwrap();
        window(&in_flight, ns).await.unwrap();
        assert!(!older_rows(&in_flight, b'I', ns, 2).await.unwrap());
        assert!(!older_rows(&in_flight, b'F', ns, 2).await.unwrap());
        in_flight.set(
            &window_key(ns),
            &encode_window(RequestWindow {
                first_retained: 2,
                accepting: 2,
            }),
        );
        c.pin_request_history(ns, 1, pin).await.unwrap();
        assert!(in_flight.commit().await.is_err());
        assert_eq!(
            c.advance_request_floor(ns, 1, 2).await.unwrap(),
            WindowOutcome::RetainedReferences
        );
        c.release_request_history(ns, 1, pin).await.unwrap();
        assert_eq!(
            c.advance_request_floor(ns, 1, 2).await.unwrap(),
            WindowOutcome::Advanced(RequestWindow {
                first_retained: 2,
                accepting: 2
            })
        );
        assert!(c.pin_request_history(ns, 1, Uuid::new_v4()).await.is_err());
        assert_eq!(
            c.publish(first).await.unwrap(),
            PublishOutcome::RequestExpired
        );
        assert_eq!(
            c.register_request(first).await.unwrap(),
            PublishOutcome::RequestExpired
        );
        assert_eq!(
            c.cancel_request(first).await.unwrap(),
            PublishOutcome::RequestExpired
        );
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let mut removed = reopened.purge_expired_requests(ns, 127).await.unwrap();
        assert!((1..=127).contains(&removed));
        loop {
            let n = reopened.purge_expired_requests(ns, 127).await.unwrap();
            assert!(n <= 127);
            removed += n;
            if n == 0 {
                break;
            }
        }
        assert_eq!(removed, 1002);
        assert_eq!(
            reopened.publish(first).await.unwrap(),
            PublishOutcome::RequestExpired
        );
        assert!(raw(c, &op_key(ns, 1, first.operation_id)).await.is_none());
        let replacement2 = receipt(ns, &serial, volume, new_scan, 2, 2, Some(1));
        c.register_request(&replacement2).await.unwrap();
        assert_eq!(
            c.publish(&replacement2).await.unwrap(),
            PublishOutcome::Applied(2)
        );
        let page = reopened
            .compact_superseded_copies(ns, None, 100)
            .await
            .unwrap();
        assert_eq!((page.compacted, page.forgotten), (0, 2));
        assert!(raw(c, &old_copy).await.is_none());
        assert!(
            raw(c, &copy_key(ns, published[1].file_id, published[1].copy_id))
                .await
                .is_none()
        );
        assert!(
            c.file_head(ns, published[2].file_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            raw(c, &copy_key(ns, published[2].file_id, published[2].copy_id))
                .await
                .unwrap()
                .len(),
            105
        );
        let reuse = CommitReceipt {
            request_epoch: 2,
            operation_id: Uuid::new_v4(),
            expected_version: Some(2),
            ..first.clone()
        };
        assert_eq!(
            c.register_request(&reuse).await.unwrap(),
            PublishOutcome::InvalidCopyId
        );
        assert_eq!(
            c.publish(&reuse).await.unwrap(),
            PublishOutcome::InvalidCopyId
        );
        assert_eq!(
            c.publish(&replacement).await.unwrap(),
            PublishOutcome::AlreadyApplied(2)
        );
        let race = receipt(ns, &serial, volume, new_scan, 3000, 2, None);
        c.register_request(&race).await.unwrap();
        let (publish, cancel) = tokio::join!(c.publish(&race), c.cancel_request(&race));
        let publish = publish.unwrap();
        let cancel = cancel.unwrap();
        assert!(matches!(
            (publish, cancel),
            (
                PublishOutcome::Applied(1),
                PublishOutcome::AlreadyApplied(1)
            ) | (
                PublishOutcome::RequestCancelled,
                PublishOutcome::RequestCancelled
            )
        ));
        assert!(raw(c, &intent_key(&race)).await.is_none());
        assert!(c.purge_expired_requests(ns, 0).await.is_err());
        assert!(c.purge_expired_requests(ns, 1001).await.is_err());
        let candidate = Uuid::new_v4();
        c.begin_import(candidate).await.unwrap();
        let r = receipt(candidate, &serial, volume, new_scan, 1, 1, None);
        c.register_request(&r).await.unwrap();
        assert!(c.discard_import(candidate).await.is_err());
        c.pin_request_history(candidate, 1, pin).await.unwrap();
        c.freeze_namespace(
            candidate,
            NamespaceSummary {
                nodes: 1,
                files: 0,
                digest: [0; 32],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            c.pending_requests(candidate, None, 10).await.unwrap().items,
            vec![r.clone()]
        );
        c.cancel_request(&r).await.unwrap();
        assert!(c.discard_import(candidate).await.is_err());
        c.release_request_history(candidate, 1, pin).await.unwrap();
        assert!(c.discard_import(candidate).await.unwrap());
    }
}
