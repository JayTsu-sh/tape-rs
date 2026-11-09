//! Sealed relocation children. A UUID alone never opens the pool write gate.
use super::*;

fn child_fingerprint(value: &Value) -> String {
    let mut body = value.clone();
    if let Some(fields) = body.as_object_mut() {
        fields.remove("fingerprint");
    }
    format!("{:x}", Sha256::digest(body.to_string().as_bytes()))
}

fn identity_key(ns: Uuid, operation: Uuid) -> Vec<u8> {
    let mut key = domain(ns, b'i');
    key.extend_from_slice(operation.as_bytes());
    key
}
fn identity_value(child: &Value) -> Vec<u8> {
    json!({"version":1,"epoch":child["epoch"],"parent":child["parent"],"fingerprint":child["fingerprint"]})
        .to_string()
        .into_bytes()
}

/// Completion has an independent immutable receipt. Remove only reverse
/// identities whose exact original child and parent have both been checked.
pub(super) async fn cleanup_completed_page_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<bool> {
    let mut prefix = reservation.journal_key();
    prefix[domain(reservation.namespace(), b'j').len() - 1] = b'c';
    let page = read_page(tr, &prefix, None, 16, false).await?;
    let mut keys = Vec::new();
    for (key, raw) in page.items {
        if key.len() != prefix.len() + 16 {
            return Err(CatalogError::Corrupt("回收清理原子请求键长度"));
        }
        let operation = uuid_from(&key[prefix.len()..])?;
        let child = decode(&raw)?;
        let reverse = identity_key(reservation.namespace(), operation);
        if child["parent"] != reservation.token()
            || child["operation"] != operation.to_string()
            || child["fingerprint"] != child_fingerprint(&child)
            || tr.get(&reverse, false).await?.as_deref() != Some(identity_value(&child).as_slice())
        {
            return Err(CatalogError::Corrupt("回收清理子请求与原反向身份不符"));
        }
        let epoch = child["epoch"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or(CatalogError::Corrupt("回收清理子请求代次"))?;
        let bytes: Vec<u8> = serde_json::from_value(child["attempt"].clone())
            .map_err(|_| CatalogError::Corrupt("回收清理子请求编码"))?;
        let (intent, _) = reservation::decode(&bytes, reservation.namespace(), epoch, operation)?;
        if intent.library_serial != reservation.serial()
            || intent.pool_id != Some(reservation.spec().pool())
            || tr
                .get(&history_pin_key(&intent.request()), false)
                .await?
                .is_some()
        {
            return Err(CatalogError::Corrupt(
                "回收清理不能移除未结清或其他作用域的子请求",
            ));
        }
        keys.push((key, reverse));
    }
    let cleared = !keys.is_empty();
    for (key, reverse) in keys {
        tr.clear(&key);
        tr.clear(&reverse);
    }
    Ok(cleared)
}

fn original_epoch(raw: &[u8], parent: &Value) -> Result<u64> {
    let value = decode(raw)?;
    let fields = ["version", "epoch", "parent", "fingerprint"];
    if value["version"] != 1
        || !value["epoch"].as_u64().is_some_and(|n| n > 0)
        || !value["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        || !value.as_object().is_some_and(|m| {
            m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str()))
        })
    {
        return Err(CatalogError::Corrupt("回收子请求原代次索引损坏"));
    }
    if &value["parent"] != parent {
        return Err(CatalogError::Invalid("回收复制请求身份已被其他父任务使用"));
    }
    Ok(value["epoch"].as_u64().expect("checked original epoch"))
}

pub(super) fn history_pin_key(request: &ActiveFileRequest) -> Vec<u8> {
    let mut key = requests::epoch_prefix(b'F', request.namespace, request.request_epoch);
    key.extend_from_slice(request.operation_id.as_bytes());
    key
}

pub(super) fn child_key(ns: Uuid, token: &Value, operation: Uuid) -> Result<Vec<u8>> {
    let mut key = domain(ns, b'c');
    key.extend_from_slice(
        &token["epoch"]
            .as_u64()
            .ok_or(CatalogError::Corrupt("回收代次"))?
            .to_be_bytes(),
    );
    let parent = token["operation"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(CatalogError::Corrupt("回收父请求"))?;
    key.extend_from_slice(parent.as_bytes());
    key.extend_from_slice(operation.as_bytes());
    Ok(key)
}

pub(super) async fn require_no_children_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    let mut prefix = reservation.journal_key();
    let kind = domain(reservation.namespace(), b'j').len() - 1;
    prefix[kind] = b'c';
    if requests::prefix_has_rows(tr, &prefix).await? {
        return Err(CatalogError::PoolBusy);
    }
    source::require_no_data_at(tr, reservation).await?;
    source::require_no_metadata_at(tr, reservation).await?;
    prepared_cancel::require_no_cancellations_at(tr, reservation).await?;
    Ok(())
}

pub(super) async fn child_at(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    epoch: u64,
    operation: Uuid,
    owner: u64,
) -> Result<Option<(ActiveFileIntent, u16)>> {
    let identity = tr.get(&identity_key(ns, operation), false).await?;
    let Some(token) = pool_gate_at(tr, ns, pool).await? else {
        if identity.is_some() {
            return Err(CatalogError::Invalid(
                "回收复制子身份已保留，原父屏障不存在",
            ));
        }
        node_tasks::require_pool_without_reclaim_at(tr, ns, pool).await?;
        return Ok(None);
    };
    let raw = tr
        .get(&child_key(ns, &token, operation)?, false)
        .await?
        .ok_or(CatalogError::PoolBusy)?;
    let value = decode(&raw)?;
    let fields = [
        "version",
        "parent",
        "epoch",
        "operation",
        "attempt",
        "source_head",
        "source_copy",
        "source_read",
        "fingerprint",
    ];
    if value["version"] != 1
        || value["fingerprint"] != child_fingerprint(&value)
        || value["parent"] != token
        || value["epoch"] != epoch
        || value["operation"] != operation.to_string()
        || !value.as_object().is_some_and(|m| {
            m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str()))
        })
    {
        return Err(CatalogError::Corrupt("回收复制子请求身份或字段"));
    }
    if identity.as_deref() != Some(identity_value(&value).as_slice()) {
        return Err(CatalogError::Corrupt(
            "回收复制子请求索引缺失或与封存身份不符",
        ));
    }
    management::exclusion::LibraryTaskClaim::for_reclaim(&token)?
        .require_owned(tr)
        .await?;
    let bytes: Vec<u8> = serde_json::from_value(value["attempt"].clone())
        .map_err(|_| CatalogError::Corrupt("回收子请求编码"))?;
    let (intent, permit) = reservation::decode(&bytes, ns, epoch, operation)?;
    let source_head: Vec<u8> = serde_json::from_value(value["source_head"].clone())
        .map_err(|_| CatalogError::Corrupt("回收原文件头编码"))?;
    let source_head = decode_head(&source_head)?;
    let source_copy: Vec<u8> = serde_json::from_value(value["source_copy"].clone())
        .map_err(|_| CatalogError::Corrupt("回收原副本编码"))?;
    let source_copy = copy_records::decode_copy(&source_copy)?;
    if intent.pool_id != Some(pool)
        || intent.library_serial != token["library"].as_str().unwrap_or_default()
        || intent.expected_version != Some(source_head.version)
        || source_head.version != source_copy.version
        || source_head.size != source_copy.size
        || source_copy.size != intent.expected_size
        || source_copy.volume_id == intent.volume_id
    {
        return Err(CatalogError::Corrupt("回收子请求与原副本不一致"));
    }
    let mut journal = domain(ns, b'j');
    journal.extend_from_slice(
        &token["epoch"]
            .as_u64()
            .expect("validated epoch")
            .to_be_bytes(),
    );
    journal.extend_from_slice(
        Uuid::parse_str(token["operation"].as_str().expect("validated operation"))
            .expect("validated UUID")
            .as_bytes(),
    );
    let parent = decode(
        &tr.get(&journal, false)
            .await?
            .ok_or(CatalogError::Corrupt("回收父任务缺失"))?,
    )?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(parent["intent"].to_string().as_bytes())
    );
    let spec = ReclaimSpec::parse(&parent["intent"]["change"])
        .map_err(|_| CatalogError::Corrupt("回收父任务源身份"))?;
    if parent["version"] != 1
        || parent["fingerprint"] != token["fingerprint"]
        || parent["fingerprint"] != fingerprint
        || !matches!(parent["phase"].as_str(), Some("copying" | "reformatting"))
        || !parent["children"].as_u64().is_some_and(|n| n > 0)
        || parent["intent"]["namespace"] != ns.to_string()
        || parent["intent"]["library"] != token["library"]
        || parent["intent"]["epoch"] != token["epoch"]
        || parent["intent"]["operation"] != token["operation"]
        || spec.pool != pool
        || source_copy.volume_id != spec.volume
        || intent.barcode_constraint.as_deref() == Some(spec.barcode.as_str())
    {
        return Err(CatalogError::Corrupt("回收子请求缺少原父任务证明"));
    }
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let route = routing::decode_active(
        &tr.get(&routing::active_key(&intent.library_serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    routing::require_runtime_active_owner(tr, &intent.library_serial, route, owner).await?;
    let reservation = ReclaimReservation {
        intent: parent["intent"].clone(),
        route,
    };
    record_at(tr, &reservation, owner).await?;
    require_parent_pool_at(tr, &reservation).await?;
    ReclaimSourceRead::require_source(
        &value["source_read"],
        token["fingerprint"]
            .as_str()
            .ok_or(CatalogError::Corrupt("回收父摘要"))?,
        &source_head,
        &source_copy,
        &intent,
    )?;
    upload_retry::identity_at(
        tr,
        &reservation,
        &ReclaimFile::from_request(&intent.request())?,
    )
    .await?;
    Ok(Some((intent, permit)))
}

/// Restore the exact original parent from the child identity, without logical
/// path resolution or a new request window. This is read-only recovery input.
async fn parent_for_request_at(
    tr: &Transaction,
    request: &ActiveFileRequest,
    owner: u64,
) -> Result<Option<ReclaimReservation>> {
    request.validate()?;
    let reverse = tr
        .get(
            &identity_key(request.namespace, request.operation_id),
            false,
        )
        .await?;
    let Some(pool) = request.pool_id else {
        if reverse.is_some() {
            return Err(CatalogError::Invalid("原回收子请求不能删除 pool 作用域"));
        }
        return Ok(None);
    };
    let Some(token) = pool_gate_at(tr, request.namespace, pool).await? else {
        if reverse.is_some() {
            return Err(CatalogError::Corrupt("回收子身份存在但原父池屏障缺失"));
        }
        return Ok(None);
    };
    if token["library"] != request.library_serial || reverse.is_none() {
        return Err(CatalogError::PoolBusy);
    }
    let route = routing::decode_active(
        &tr.get(&routing::active_key(&request.library_serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    if route.route.namespace != request.namespace {
        return Err(CatalogError::OwnerFenced);
    }
    routing::require_runtime_active_owner(tr, &request.library_serial, route, owner).await?;
    let mut journal = domain(request.namespace, b'j');
    journal.extend_from_slice(
        &token["epoch"]
            .as_u64()
            .expect("validated epoch")
            .to_be_bytes(),
    );
    journal.extend_from_slice(
        node_mutation::id(token["operation"].as_str().expect("validated operation"))?.as_bytes(),
    );
    let parent = decode(
        &tr.get(&journal, false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let intent = &parent["intent"];
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
    if !intent
        .as_object()
        .is_some_and(|m| m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str())))
        || intent["version"] != 1
        || intent["library"] != request.library_serial
        || intent["namespace"] != request.namespace.to_string()
        || intent["epoch"] != token["epoch"]
        || intent["operation"] != token["operation"]
        || ReclaimSpec::parse(&intent["change"])?.pool() != pool
    {
        return Err(CatalogError::Corrupt("回收写入子身份与原父意图不同"));
    }
    let reservation = ReclaimReservation {
        intent: intent.clone(),
        route,
    };
    let parent = record_at(tr, &reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if parent["phase"] != "copying" {
        return Err(CatalogError::Invalid("只有原复制阶段可执行封存子请求"));
    }
    let (intent, _) = child_at(
        tr,
        request.namespace,
        pool,
        request.request_epoch,
        request.operation_id,
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if intent.request() != *request {
        return Err(CatalogError::Invalid("回收写入必须复用封存的完整子请求"));
    }
    source_state_at(tr, &reservation).await?;
    mechanics::require_parent_idle_at(tr, &reservation).await?;
    if tr
        .get(
            &native_device::library_gate(request.namespace, &request.library_serial),
            false,
        )
        .await?
        .is_some()
        || automatic_media::pending_at(tr, request.namespace, &request.library_serial)
            .await?
            .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    if let Some(layout) = layout::at(tr, &reservation).await?
        && (intent.volume_id != layout.destination.volume
            || intent.library_serial != request.library_serial)
    {
        return Err(CatalogError::Invalid("原回收子请求不属于固定目的布局"));
    }
    Ok(Some(reservation))
}

pub(in crate::managed_catalog::control) async fn require_write_group_lane_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    owner: u64,
    requests: &[ActiveFileRequest],
) -> Result<()> {
    if tr.get(&layout::gate(ns, serial), false).await?.is_none() {
        return management::moves::require_readable_at(tr, ns, serial).await;
    }
    if requests.is_empty() {
        return Err(CatalogError::PoolBusy);
    }
    for request in requests {
        if request.namespace != ns
            || request.library_serial != serial
            || parent_for_request_at(tr, request, owner).await?.is_none()
        {
            return Err(CatalogError::PoolBusy);
        }
        require_metadata_copy_idle_at(tr, ns, request.pool_id.ok_or(CatalogError::PoolBusy)?)
            .await?;
    }
    Ok(())
}

impl ManagedCatalog {
    /// Original sealed byte evidence for relocation attributes. Ordinary
    /// uploads return None; callers cannot provide hashes for this path.
    pub(crate) async fn reclaim_upload_read_source(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<Option<ReclaimReadSource>> {
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, &request.library_serial, route, owner).await?;
        let Some(parent) = parent_for_request_at(&tr, request, owner).await? else {
            return Ok(None);
        };
        Box::pin(read_source_at(&tr, &parent, owner, request))
            .await
            .map(Some)
    }

    pub async fn reclaim_upload_parent(
        &self,
        request: &ActiveFileRequest,
        owner: u64,
    ) -> Result<Option<ReclaimReservation>> {
        parent_for_request_at(&self.db.create_trx()?, request, owner).await
    }
    /// Authority for reconciling an already admitted original physical attempt.
    /// Owner adoption changes no physical input and never admits fresh data.
    /// Unlike the ordinary writer, this scope may observe the atomic transition
    /// from the old Q/I to the exact original terminal result.
    pub async fn require_reclaim_upload_recovery_lane(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let parent = parent_for_request_at(&tr, request, owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if parent.intent() != reservation.intent() || parent.route != reservation.route {
            return Err(CatalogError::OwnerFenced);
        }
        require_metadata_copy_idle_at(
            &tr,
            request.namespace,
            request.pool_id.ok_or(CatalogError::Incomplete)?,
        )
        .await?;
        let (mut original, _) = child_at(
            &tr,
            request.namespace,
            reservation.spec().pool(),
            request.request_epoch,
            request.operation_id,
            owner,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
        if let Some(current) = reservation::active_file_attempt_at(&tr, request).await? {
            if current.owner_epoch > owner {
                return Err(CatalogError::OwnerFenced);
            }
            original.owner_epoch = current.owner_epoch;
            if current != original {
                return Err(CatalogError::Invalid("恢复不能更改原物理尝试的目标或正文"));
            }
        } else {
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
                requests::reservation_terminal_result(&terminal, &request.fingerprint())?,
                PublishOutcome::AlreadyApplied(_) | PublishOutcome::RequestCancelled
            ) {
                return Err(CatalogError::Invalid("恢复缺少精确原上传终态"));
            }
        }
        Ok(())
    }

    pub async fn require_reclaim_upload_lane(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let parent = parent_for_request_at(&tr, request, owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if parent.intent() != reservation.intent() || parent.route != reservation.route {
            return Err(CatalogError::OwnerFenced);
        }
        require_metadata_copy_idle_at(
            &tr,
            request.namespace,
            request.pool_id.ok_or(CatalogError::Incomplete)?,
        )
        .await
    }
}

pub(in crate::managed_catalog) async fn require_request_at(
    tr: &Transaction,
    request: &ActiveFileRequest,
    owner: u64,
) -> Result<()> {
    if let Some(pool) = request.pool_id {
        require_metadata_copy_idle_at(tr, request.namespace, pool).await?;
    }
    require_request_identity_at(tr, request, owner).await
}

pub(super) async fn require_request_identity_at(
    tr: &Transaction,
    request: &ActiveFileRequest,
    owner: u64,
) -> Result<()> {
    if let Some(pool) = request.pool_id
        && let Some((saved, _)) = child_at(
            tr,
            request.namespace,
            pool,
            request.request_epoch,
            request.operation_id,
            owner,
        )
        .await?
        && saved.request() != *request
    {
        return Err(CatalogError::Invalid("回收复制必须复用封存的完整原请求"));
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn require_intent_at(
    tr: &Transaction,
    intent: &ActiveFileIntent,
) -> Result<Option<u16>> {
    if let Some(pool) = intent.pool_id {
        require_metadata_copy_idle_at(tr, intent.namespace, pool).await?;
    }
    if let Some(pool) = intent.pool_id
        && let Some((mut saved, permit)) = child_at(
            tr,
            intent.namespace,
            pool,
            intent.request_epoch,
            intent.operation_id,
            intent.owner_epoch,
        )
        .await?
    {
        saved.owner_epoch = intent.owner_epoch;
        if saved != *intent {
            return Err(CatalogError::Invalid("回收复制物理目标或完整请求改变"));
        }
        if tr
            .get(&history_pin_key(&intent.request()), false)
            .await?
            .as_deref()
            != Some(&[][..])
        {
            return Err(CatalogError::Corrupt("回收子请求的历史保留引用缺失或损坏"));
        }
        return Ok(Some(permit));
    }
    Ok(None)
}

pub(in crate::managed_catalog) async fn require_receipt_at(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<()> {
    if let Some(pool) = receipt.pool_id
        && let Some((saved, _)) = child_at(
            tr,
            receipt.namespace,
            pool,
            receipt.request_epoch,
            receipt.operation_id,
            receipt.owner_epoch,
        )
        .await?
        && !reservation::matches_receipt(&saved, receipt)
    {
        return Err(CatalogError::Invalid("回收复制收据与封存请求不一致"));
    }
    Ok(())
}

pub(super) async fn read_source_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    request: &ActiveFileRequest,
) -> Result<ReclaimReadSource> {
    request.validate()?;
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let spec = reservation.spec();
    if request.namespace != reservation.namespace() || request.pool_id != Some(spec.pool) {
        return Err(CatalogError::Invalid("回收读取不属于原池"));
    }
    let (saved, _) = child_at(
        tr,
        request.namespace,
        spec.pool,
        request.request_epoch,
        request.operation_id,
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    if saved.request() != *request {
        return Err(CatalogError::Invalid("回收读取原请求改变"));
    }
    let raw = tr
        .get(
            &child_key(
                request.namespace,
                &reservation.token(),
                request.operation_id,
            )?,
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let child = decode(&raw)?;
    let head = decode_head(
        &serde_json::from_value::<Vec<u8>>(child["source_head"].clone())
            .map_err(|_| CatalogError::Corrupt("回收原文件头"))?,
    )?;
    let copy = copy_records::decode_copy(
        &serde_json::from_value::<Vec<u8>>(child["source_copy"].clone())
            .map_err(|_| CatalogError::Corrupt("回收原副本"))?,
    )?;
    let proof = ReclaimSourceRead::require_source(
        &child["source_read"],
        &reservation.fingerprint(),
        &head,
        &copy,
        &saved,
    )?;
    source_state_at(tr, reservation).await?;
    let placement = control::verified_placement(tr, spec.volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let path = pool_catalog_path(spec.pool, &request.ltfs_path()?.into_bytes())?;
    let source = lookup::read_source_at(
        tr,
        reservation.serial(),
        &path,
        ResolvedPath::File {
            id: request.file_id,
            head,
            copy: Box::new(copy),
            placement,
            attributes: None,
            object_metadata: None,
        },
    )
    .await?;
    Ok(ReclaimReadSource::new(source, proof))
}

impl ManagedCatalog {
    /// Resolve the immutable original copy even after its logical path has
    /// published a destination. This never authorizes a read after unassignment.
    pub async fn reclaim_read_source(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<ReclaimReadSource> {
        let tr = self.db.create_trx()?;
        Box::pin(read_source_at(&tr, reservation, owner, request)).await
    }

    /// Atomically seal the original source/target and pin its result epoch.
    /// Q is admitted only after mount recovery, reusing this retained epoch.
    /// This admits copying only and never grants permission to erase media.
    pub async fn prepare_reclaim_upload(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        destination: &str,
        spec: &ActiveUploadSpec,
        source_read: &ReclaimSourceRead,
    ) -> Result<ActiveFileIntent> {
        let source = reservation.spec();
        if spec.pool_id != source.pool
            || destination == source.barcode
            || spec.expected_version.is_none()
        {
            return Err(CatalogError::Invalid(
                "回收复制必须保留原池及版本并写入其他磁带",
            ));
        }
        self.db.run(|tr, _| {
            let source = source.clone();
            Box::pin(async move { requests::retry_result(async {
                let mut parent = record_at(&tr, reservation, owner).await?;
                if parent["phase"] == "reformatting" { return Err(CatalogError::Invalid("源卷已进入格式化阶段，不能再准备迁移正文")); }
                if parent["owner_epoch"] != owner { return Err(CatalogError::OwnerFenced); }
                source::require_metadata_idle_at(&tr, reservation).await?;
                require_parent_pool_at(&tr, reservation).await?;
                let (_, source_index) = source_state_at(&tr, reservation).await?;
                let mut source_index_bytes = Vec::new(); active_volume::encode_index(&mut source_index_bytes, source_index);
                if source_read.index() != source_index_bytes { return Err(CatalogError::Invalid("回收源实读卷头与封存源不同")); }
                let read_value = source_read.value()?;
                let reverse = identity_key(reservation.namespace(), spec.operation_id);
                let original = tr.get(&reverse, false).await?;
                let epoch = if let Some(raw) = &original {
                    original_epoch(raw, &reservation.token())?
                } else {
                    requests::window(&tr, reservation.namespace()).await?.accepting
                };
                let key = child_key(reservation.namespace(), &reservation.token(), spec.operation_id)?;
                if let Some(raw) = tr.get(&key, false).await? {
                    if decode(&raw)?["source_read"] != read_value { return Err(CatalogError::Invalid("回收复制重试改变原源实读证明")); }
                    let (mut saved, _) = child_at(&tr, reservation.namespace(), source.pool, epoch, spec.operation_id, owner).await?
                        .ok_or(CatalogError::Corrupt("回收子请求失去池屏障"))?;
                    if saved.ltfs_path()?.as_bytes() != spec.path || saved.expected_size != spec.size
                        || saved.expected_digest != spec.digest || saved.expected_version != spec.expected_version
                        || saved.barcode_constraint.as_deref() != Some(destination) {
                        return Err(CatalogError::Invalid("回收复制重试改变原请求"));
                    }
                    saved.owner_epoch = owner;
                    return Ok(saved);
                }
                if original.is_some() { return Err(CatalogError::Corrupt("回收复制反向身份缺少原子请求")); }
                let path = pool_catalog_path(source.pool, &spec.path)?;
                let Some(ResolvedPath::File { id, head, copy, placement, .. }) = lookup::resolve_active_path_at_mode(
                    &tr, reservation.namespace(), lookup::path_components(&path)?, false).await? else {
                    return Err(CatalogError::Invalid("回收源路径不存在或不是文件"));
                };
                if copy.volume_id != source.volume || placement.barcode != source.barcode
                    || placement.pool_id != source.pool || Some(head.version) != spec.expected_version
                    || copy.size != spec.size || matches!(copy.content_digest, ContentDigest::Sha256(_)) && copy.content_digest != spec.digest {
                    return Err(CatalogError::Invalid("回收源身份、大小或内容摘要改变"));
                }
                let intent = upload::prepare_upload_at(&tr, reservation.namespace(), reservation.serial(), owner, destination, spec).await?;
                upload_retry::identity_at(&tr,reservation,&ReclaimFile::from_request(&intent.request())?).await?;
                if intent.file_id != id || intent.request_epoch != epoch { return Err(CatalogError::Invalid("回收原文件 inode 或请求代次改变")); }
                ReclaimSourceRead::require_source(&read_value, &reservation.fingerprint(), &head, &copy, &intent)?;
                let copy_raw = tr.get(&copy_key(reservation.namespace(), id, head.copy_id), false).await?
                    .ok_or(CatalogError::Corrupt("回收原副本丢失"))?;
                if copy_records::decode_copy(&copy_raw)? != *copy { return Err(CatalogError::Corrupt("回收原副本与路径不一致")); }
                let pin = history_pin_key(&intent.request());
                if tr.get(&pin, false).await?.is_some() { return Err(CatalogError::Invalid("回收子请求的历史引用身份已被使用")); }
                let mut value = json!({"version":1,"parent":reservation.token(),"epoch":epoch,"operation":spec.operation_id.to_string(),"attempt":reservation::encode(&intent,0),"source_head":encode_head(&head),"source_copy":copy_raw.to_vec(),"source_read":read_value});
                value["fingerprint"] = json!(child_fingerprint(&value));
                let raw = value.to_string();
                if raw.len() > 47_000 { return Err(CatalogError::Invalid("回收复制检查点过大")); }
                let count = parent["children"].as_u64().and_then(|n| n.checked_add(1))
                    .ok_or(CatalogError::Corrupt("回收子请求计数溢出"))?;
                parent["children"] = json!(count);
                parent["phase"] = json!("copying");
                let parent_bytes = parent.to_string();
                // Reserve the bounded budget in the first epoch, but do not
                // create Q until mounted-media recovery has completed. Nothing
                // fallible follows these quota mutations.
                let Some(permit) = maintenance::reserve_identity(&tr, intent.namespace, epoch, spec.operation_id).await? else {
                    return Err(CatalogError::PoolBusy);
                };
                value["attempt"] = json!(reservation::encode(&intent, permit));
                value["fingerprint"] = json!(child_fingerprint(&value));
                let raw = value.to_string();
                let identity = identity_value(&value);
                tr.set(&key, raw.as_bytes());
                tr.set(&reverse, &identity);
                tr.set(&pin, &[]);
                tr.set(&reservation.journal_key(), parent_bytes.as_bytes());
                Ok(intent)
            }.await) })
        }).await?
    }

    pub async fn check_active_upload_gate(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        request.validate()?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, &request.library_serial, route, owner)
            .await?;
        if request.namespace != ns {
            return Err(CatalogError::Invalid("写入请求命名空间改变"));
        }
        if let Some(pool) = request.pool_id {
            require_pool_library(&tr, ns, pool, &request.library_serial).await?;
        }
        require_request_at(&tr, request, owner).await
    }

    /// Return the sealed first physical attempt, without selecting another
    /// medium after a retry or takeover. Ordinary uploads have no such entry.
    pub async fn reclaim_upload_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<Option<ActiveFileIntent>> {
        self.check_active_upload_gate(route, owner, request).await?;
        let Some(pool) = request.pool_id else {
            return Ok(None);
        };
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, &request.library_serial, route, owner).await?;
        let saved = child_at(
            &tr,
            request.namespace,
            pool,
            request.request_epoch,
            request.operation_id,
            owner,
        )
        .await?;
        let mut saved = saved.map(|(intent, _)| intent);
        if let Some(intent) = &mut saved {
            if intent.request() != *request {
                return Err(CatalogError::Invalid("回收复制原请求改变"));
            }
            intent.owner_epoch = owner;
        }
        Ok(saved)
    }
}
