//! Original group-only uncertainty, never a cancellation or media permission.
use super::*;

// 0x83 is qualification; 0x84 is reserved for native-successor admission.
const DISPOSITION_KIND: u8 = 0x85;
const MAX_DISPOSITION_BYTES: usize = 16 << 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Subject {
    Group {
        group: String,
        attempt: String,
        batch: String,
        issuer: u64,
        identity: [u8; 32],
    },
    Metadata {
        task: String,
        first: String,
        origin: String,
        recovery_epoch: u64,
        plan: [u8; 32],
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Disposition {
    ObservationStarted { owner: u64, nonce: String },
    BlockedUnknown { owner: u64, reason: String },
    ObservedKnownFull { owner: u64, closed: [u8; 32] },
}
impl Disposition {
    fn owner(&self) -> u64 {
        match self {
            Self::ObservationStarted { owner, .. }
            | Self::BlockedUnknown { owner, .. }
            | Self::ObservedKnownFull { owner, .. } => *owner,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    version: u8,
    parent: String,
    source: [u8; 32],
    subject: Subject,
    disposition: Disposition,
}

/// Classification alone cannot authorize a device operation.
pub(crate) enum ReclaimExecutionGate {
    Fresh,
    KnownFullCandidate { group: Uuid },
    Blocked { reason: String },
}
/// Only a successful original-subject ObservationStarted CAS constructs this.
/// It is intentionally neither Clone nor Deserialize.
pub(crate) struct OriginalObservationPermit {
    scope: OriginalAttemptObservation,
    row: Row,
    encoded: Vec<u8>,
}
impl OriginalObservationPermit {
    pub(crate) fn scope(&self) -> &OriginalAttemptObservation {
        &self.scope
    }
    pub(crate) fn parent(&self) -> &ReclaimReservation {
        self.scope.parent()
    }
    pub(crate) fn owner(&self) -> u64 {
        self.scope.owner()
    }
    pub(crate) fn group_id(&self) -> Uuid {
        self.scope.group_id()
    }
}
impl OriginalObservationPermit {
    async fn require_at(&self, tr: &Transaction) -> Result<()> {
        live_parent_at(tr, self.parent(), self.owner()).await?;
        if tr
            .get(&disposition_key(self.parent()), false)
            .await?
            .as_deref()
            != Some(self.encoded.as_slice())
            || !matches!(self.row.disposition, Disposition::ObservationStarted { .. })
        {
            return Err(CatalogError::OwnerFenced);
        }
        group_attempt::require_scope_at(tr, &self.scope).await?;
        let group = evidence_group_at(tr, self.parent(), self.group_id()).await?;
        if group_subject_at(tr, self.parent(), &group).await? != self.row.subject {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(())
    }
}
impl ManagedCatalog {
    pub(crate) async fn check_original_reclaim_observation(
        &self,
        permit: &OriginalObservationPermit,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        permit.require_at(&tr).await
    }
    pub(crate) async fn check_original_reclaim_group_observation(
        &self,
        permit: &OriginalObservationPermit,
        scope: &ReclaimGroupObservation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        permit.require_at(&tr).await?;
        if scope.parent().intent() != permit.parent().intent()
            || scope.owner() != permit.owner()
            || scope.group().id() != permit.group_id()
        {
            return Err(CatalogError::OwnerFenced);
        }
        scope.require_at(&tr).await
    }
    pub(crate) async fn check_fresh_reclaim_group_observation(
        &self,
        fresh: &ReclaimGroupExecution,
        scope: &ReclaimGroupObservation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        require_fresh_at(&tr, fresh.parent()).await?;
        if fresh.parent().intent() != scope.parent().intent()
            || fresh.execution_epoch() != scope.owner()
            || fresh.group().id() != scope.group().id()
            || fresh.batch_id() != scope.group().batch_id()
            || fresh.intent().intent != scope.intent().intent
            || fresh.intent().members != scope.intent().members
        {
            return Err(CatalogError::OwnerFenced);
        }
        scope.require_at(&tr).await
    }
    pub(crate) async fn finish_original_reclaim_observation(
        &self,
        permit: &OriginalObservationPermit,
    ) -> Result<()> {
        self.db.run(|tr,_|Box::pin(async move {requests::retry_result(async {
            live_parent_at(&tr, permit.parent(), permit.owner()).await?;
            let (closed, subject) = closed_reference_at(&tr, permit.parent(), permit.group_id()).await?;
            if subject != permit.row.subject { return Err(CatalogError::OwnerFenced); }
            if tr.get(&disposition_key(permit.parent()), false).await?.as_deref() != Some(permit.encoded.as_slice()) {
                let row = row_at(&tr, permit.parent()).await?.ok_or(CatalogError::Incomplete)?;
                if row.subject == subject && matches!(row.disposition,Disposition::ObservedKnownFull { closed:c,.. } if c==closed) { return Ok(()); }
                return Err(CatalogError::OwnerFenced);
            }
            let mut row = permit.row.clone();
            row.disposition = Disposition::ObservedKnownFull { owner: permit.owner(), closed };
            tr.set(&disposition_key(permit.parent()), &encode_row(&row)?);
            Ok(())
        }.await.map_err(notifications::atomic_error))})).await?
    }
    pub(crate) async fn block_original_reclaim_observation(
        &self,
        permit: &OriginalObservationPermit,
        reason: &str,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            live_parent_at(&tr, permit.parent(), permit.owner()).await?;
                            if tr
                                .get(&disposition_key(permit.parent()), false)
                                .await?
                                .as_deref()
                                != Some(permit.encoded.as_slice())
                            {
                                return Err(CatalogError::OwnerFenced);
                            }
                            block_at(
                                &tr,
                                permit.parent(),
                                permit.owner(),
                                permit.row.subject.clone(),
                                reason,
                            )
                            .await?;
                            Ok(())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

fn disposition_key(parent: &ReclaimReservation) -> Vec<u8> {
    let mut out = domain(parent.namespace(), DISPOSITION_KIND);
    out.extend_from_slice(
        &parent.intent()["epoch"]
            .as_u64()
            .expect("validated epoch")
            .to_be_bytes(),
    );
    out.extend_from_slice(
        Uuid::parse_str(
            parent.intent()["operation"]
                .as_str()
                .expect("validated operation"),
        )
        .expect("validated operation")
        .as_bytes(),
    );
    out
}
fn source_fingerprint(parent: &ReclaimReservation) -> Result<[u8; 32]> {
    digest(
        b"tape-rs/reclaim-isolation-source/1",
        &(
            parent.spec().volume().to_string(),
            &parent.intent()["scan"],
            &parent.intent()["scan_generation"],
            &parent.intent()["index"],
        ),
    )
}
fn encode_row(row: &Row) -> Result<Vec<u8>> {
    if row.version != 1
        || row.parent.len() != 64
        || !row
            .parent
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || row.disposition.owner() == 0
    {
        return Err(CatalogError::Corrupt("reclaim隔离记录版本或身份无效"));
    }
    match &row.subject {
        Subject::Group {
            group,
            attempt,
            batch,
            issuer,
            ..
        } => {
            parse_uuid(group)?;
            parse_uuid(attempt)?;
            parse_uuid(batch)?;
            if *issuer == 0 || *issuer > row.disposition.owner() {
                return Err(CatalogError::Corrupt("reclaim隔离记录原issuer无效"));
            }
        }
        Subject::Metadata {
            task,
            first,
            origin,
            ..
        } => {
            parse_uuid(task)?;
            parse_uuid(first)?;
            parse_uuid(origin)?;
        }
    }
    match &row.disposition {
        Disposition::ObservationStarted { nonce, .. } => {
            parse_uuid(nonce)?;
        }
        Disposition::BlockedUnknown { reason, .. } if reason.is_empty() || reason.len() > 2000 => {
            return Err(CatalogError::Corrupt("reclaim隔离原因无效"));
        }
        _ => {}
    }
    let mut bytes = serde_json::to_vec(row).map_err(cache)?;
    if bytes.len() > MAX_DISPOSITION_BYTES {
        return Err(CatalogError::Invalid("reclaim隔离记录超过固定预算"));
    }
    let sha = Sha256::new()
        .chain_update(b"tape-rs/reclaim-isolation/1")
        .chain_update(&bytes)
        .finalize();
    bytes.extend_from_slice(&sha);
    Ok(bytes)
}
fn decode_row(raw: &[u8]) -> Result<Row> {
    if raw.len() < 32 || raw.len() > MAX_DISPOSITION_BYTES + 32 {
        return Err(CatalogError::Corrupt("reclaim隔离记录长度无效"));
    }
    let row: Row = serde_json::from_slice(&raw[..raw.len() - 32]).map_err(cache)?;
    if encode_row(&row)?.as_slice() != raw {
        return Err(CatalogError::Corrupt("reclaim隔离记录校验或规范编码改变"));
    }
    Ok(row)
}
async fn row_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<Option<Row>> {
    let Some(raw) = tr.get(&disposition_key(parent), false).await? else {
        return Ok(None);
    };
    let row = decode_row(&raw)?;
    if !parent.spec().group_only()
        || row.parent != parent.fingerprint()
        || row.source != source_fingerprint(parent)?
    {
        return Err(CatalogError::Corrupt("reclaim隔离记录改变原parent/source"));
    }
    Ok(Some(row))
}
fn blocked(reason: impl Into<String>) -> ReclaimExecutionGate {
    ReclaimExecutionGate::Blocked {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    fn row() -> Row {
        Row {
            version: 1,
            parent: "a".repeat(64),
            source: [1; 32],
            subject: Subject::Group {
                group: Uuid::from_u128(1).to_string(),
                attempt: Uuid::from_u128(1).to_string(),
                batch: Uuid::from_u128(1).to_string(),
                issuer: 7,
                identity: [2; 32],
            },
            disposition: Disposition::ObservationStarted {
                owner: 8,
                nonce: Uuid::from_u128(2).to_string(),
            },
        }
    }
    #[test]
    fn isolation_codec_preserves_original_issuer_and_rejects_tamper() {
        let original = row();
        let bytes = encode_row(&original).unwrap();
        assert_eq!(decode_row(&bytes).unwrap(), original);
        let mut bad = bytes.clone();
        bad[5] ^= 1;
        assert!(decode_row(&bad).is_err());
        let mut bad = original.clone();
        bad.version = 2;
        assert!(encode_row(&bad).is_err());
        let mut bad = original.clone();
        bad.disposition = Disposition::BlockedUnknown {
            owner: 6,
            reason: "unknown".into(),
        };
        assert!(encode_row(&bad).is_err());
    }
    #[test]
    fn isolation_codec_rejects_missing_fields_unknown_fields_and_noncanonical_nonce() {
        let original = row();
        for change in 0..3 {
            let mut json = serde_json::to_value(&original).unwrap();
            match change {
                0 => {
                    json.as_object_mut().unwrap().remove("source");
                }
                1 => {
                    json["extra"] = json!(true);
                }
                _ => {
                    json["disposition"]["nonce"] = json!(Uuid::nil().to_string());
                }
            }
            let raw = serde_json::to_vec(&json).unwrap();
            let mut encoded = raw.clone();
            let hash = Sha256::new()
                .chain_update(b"tape-rs/reclaim-isolation/1")
                .chain_update(&raw)
                .finalize();
            encoded.extend_from_slice(&hash);
            assert!(decode_row(&encoded).is_err());
        }
    }
}

async fn live_parent_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
) -> Result<Value> {
    let value = record_at(tr, parent, owner).await?;
    if value["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(value)
}
async fn group_subject_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<Subject> {
    Ok(Subject::Group {
        group: group.id().to_string(),
        attempt: group
            .header
            .current_attempt
            .clone()
            .ok_or(CatalogError::Incomplete)?,
        batch: group.batch_id().to_string(),
        issuer: group
            .header
            .execution_epoch
            .ok_or(CatalogError::Incomplete)?,
        identity: group_attempt::fingerprint_at(tr, parent, group).await?,
    })
}
fn metadata_subject(task: &source::ReclaimMetadataWrite) -> Result<Subject> {
    Ok(Subject::Metadata {
        task: task.id().to_string(),
        first: task.first_attempt().to_string(),
        origin: task.plan_origin().to_string(),
        recovery_epoch: task.recovery_epoch(),
        plan: task.plan().fingerprint()?,
    })
}
async fn closed_reference_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: Uuid,
) -> Result<([u8; 32], Subject)> {
    let original = evidence_group_at(tr, parent, group).await?;
    if original.header.phase != Phase::Closed {
        return Err(CatalogError::Incomplete);
    }
    group_close::require_closed_evidence_at(tr, parent, &original).await?;
    let raw = tr
        .get(
            &group_layout::canonical_key(parent, group, b'c', None),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    Ok((
        Sha256::digest(&raw).into(),
        group_subject_at(tr, parent, &original).await?,
    ))
}
async fn require_finished_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    row: &Row,
) -> Result<()> {
    let (Subject::Group { group, .. }, Disposition::ObservedKnownFull { closed, .. }) =
        (&row.subject, &row.disposition)
    else {
        return Err(CatalogError::Invalid("BlockedUnknown：原观察尚未完整结清"));
    };
    let (actual, subject) = closed_reference_at(tr, parent, parse_uuid(group)?).await?;
    if subject != row.subject || actual != *closed {
        return Err(CatalogError::Corrupt("原KnownFull关闭证明改变"));
    }
    Ok(())
}
async fn block_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    subject: Subject,
    reason: &str,
) -> Result<ReclaimExecutionGate> {
    let row = Row {
        version: 1,
        parent: parent.fingerprint(),
        source: source_fingerprint(parent)?,
        subject,
        disposition: Disposition::BlockedUnknown {
            owner,
            reason: reason.chars().take(400).collect(),
        },
    };
    tr.set(&disposition_key(parent), &encode_row(&row)?);
    Ok(blocked(format!(
        "BlockedUnknown：{reason}；原操作和源数据保护保持，等待显式恢复"
    )))
}
/// Read-only candidate qualification. No synthetic physical index is created.
async fn known_candidate_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<bool> {
    let Some(actual) = actual_for_attempt_at(tr, parent, group).await? else {
        return Ok(false);
    };
    actual.intent.require_full_dp_checkpoint_policy()?;
    let Some(raw) = tr
        .get(&key(parent, b'b', Some(group.batch_id()), None), false)
        .await?
    else {
        return Ok(false);
    };
    let binding: BatchBinding = native_decode(&raw)?;
    let mut files = Vec::with_capacity(group.members.len());
    for ordinal in 0..group.header.count {
        files.push(original_member_receipt_at(tr, parent, owner, group, &actual, ordinal).await?);
    }
    let mut receipts = files
        .iter()
        .map(|f| receipt_fingerprint(&f.receipt))
        .collect::<Vec<_>>();
    receipts.sort();
    if binding.version != 1
        || binding.batch != group.header.batch
        || binding.plan != actual.fingerprint()?
        || binding.receipts != digest(b"tape-rs/reclaim-index-batch-receipts/1", &receipts)?
    {
        return Err(CatalogError::Corrupt("原观察候选binding与Actual/收据不同"));
    }
    let Some(route_raw) = tr
        .get(
            &batch_route_key(parent.namespace(), group.batch_id()),
            false,
        )
        .await?
    else {
        return Ok(false);
    };
    let route: BatchRoute = native_decode(&route_raw)?;
    if route.version != 2
        || route.parent != *parent.intent()
        || route.group != group.header.group
        || Some(route.physical_attempt.as_str()) != group.header.current_attempt.as_deref()
        || route.batch != group.header.batch
        || route.manifest != binding.manifest
        || route.plan != binding.plan
        || tr
            .get(
                &group_layout::control_key(parent, b'b', Some(group.batch_id())),
                false,
            )
            .await?
            .as_deref()
            != Some(route_raw.as_ref())
    {
        return Err(CatalogError::Corrupt("原观察候选mandatory route改变"));
    }
    let receipts = files.into_iter().map(|f| f.receipt).collect::<Vec<_>>();
    let issuer = group
        .header
        .execution_epoch
        .ok_or(CatalogError::Incomplete)?;
    if let Some(raw) = tr
        .get(&key(parent, b'p', Some(group.id()), None), false)
        .await?
    {
        let observed: Observed = native_decode(&raw)?;
        observed.require_full_dp(&actual)?;
        let index = active_volume::decode_index(group.header.destination.volume, &observed.index)?;
        active_batch::require_original_checkpoint_header_at(
            tr,
            parent.serial(),
            owner,
            issuer,
            group.batch_id(),
            binding.manifest,
            index,
            &receipts,
        )
        .await?;
        return Ok(true);
    }
    active_batch::original_checkpoint_candidate_header_at(
        tr,
        parent.serial(),
        owner,
        issuer,
        group.batch_id(),
        binding.manifest,
        group.header.destination.volume,
        actual.actual.generation,
        &receipts,
    )
    .await
}

impl ManagedCatalog {
    /// Claim-time metadata-only classification, before any task cache/device IO.
    pub(crate) async fn classify_reclaim_execution(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<ReclaimExecutionGate> {
        if !parent.spec().group_only() {
            return Ok(ReclaimExecutionGate::Fresh);
        }
        self.db.run(|tr,_|Box::pin(async move {requests::retry_result(async {
            let live=live_parent_at(&tr,parent,owner).await?;
            if let Some(row)=row_at(&tr,parent).await? {
                match &row.disposition {
                    Disposition::BlockedUnknown{reason,..}=>return Ok(blocked(format!("BlockedUnknown：{reason}；保留原操作"))),
                    Disposition::ObservationStarted{..}=>return block_at(&tr,parent,owner,row.subject,"原有限观察已开始但未完整结清，禁止自动重探").await,
                    Disposition::ObservedKnownFull{..}=>require_finished_at(&tr,parent,&row).await?,
                }
            }
            if matches!(live["phase"].as_str(),Some("reserved"|"copying")) {
                if let Some(group)=load_at(&tr,parent,owner).await? {
                    if matches!(group.header.phase,Phase::Admitted|Phase::Settling) {
                        let subject=group_subject_at(&tr,parent,&group).await?;
                        if !known_candidate_at(&tr,parent,owner,&group).await? {return block_at(&tr,parent,owner,subject,"原attempt缺完整Actual/Header3/binding，不能确认写入结果").await;}
                        return Ok(ReclaimExecutionGate::KnownFullCandidate{group:group.id()});
                    }
                    for member in &group.members {if member.child.is_some(){let intent=original_child_at(&tr,parent,owner,member).await?;reservation::require_unstarted_at(&tr,&intent).await?;}}
                }
                if let Some(task)=source::isolation_checkpoint_at(&tr,parent,owner).await? {
                    if parent.groups_archive_root() && task.plan().is_source_root() {
                        return block_at(&tr,parent,owner,metadata_subject(&task)?,"归档根随组协议与原独立目录意图冲突").await;
                    }
                    if task.writing()||task.recovering() {return block_at(&tr,parent,owner,metadata_subject(&task)?,"原root/目录MetadataTask写入或修复结果未结清").await;}
                }
            }
            Ok(ReclaimExecutionGate::Fresh)
        }.await.map_err(notifications::atomic_error))})).await?
    }
    /// An observation response lost after commit is never reconstructed as fresh.
    pub(crate) async fn begin_original_reclaim_observation(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
    ) -> Result<OriginalObservationPermit> {
        let nonce = Uuid::new_v4();
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            live_parent_at(&tr, parent, owner).await?;
                            if !parent.spec().group_only() || row_at(&tr, parent).await?.is_some() {
                                return Err(CatalogError::Invalid(
                                    "原有限观察已有持久状态，禁止重入",
                                ));
                            }
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || !matches!(group.header.phase, Phase::Admitted | Phase::Settling)
                                || !known_candidate_at(&tr, parent, owner, &group).await?
                            {
                                return Err(CatalogError::Invalid(
                                    "原attempt不具备KnownFull只读观察候选条件",
                                ));
                            }
                            group_attempt::adopt_at(&tr, parent, &mut group, owner).await?;
                            save_header(&tr, parent, &group)?;
                            let scope = group_attempt::scope_at(&tr, parent, owner, &group).await?;
                            let row = Row {
                                version: 1,
                                parent: parent.fingerprint(),
                                source: source_fingerprint(parent)?,
                                subject: group_subject_at(&tr, parent, &group).await?,
                                disposition: Disposition::ObservationStarted {
                                    owner,
                                    nonce: nonce.to_string(),
                                },
                            };
                            let encoded = encode_row(&row)?;
                            tr.set(&disposition_key(parent), &encoded);
                            Ok(OriginalObservationPermit {
                                scope,
                                row,
                                encoded,
                            })
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

/// Idempotent released-parent APIs have no live journal. They still cannot
/// bypass an original unresolved disposition merely because other rows vanished.
pub(in crate::managed_catalog) async fn require_settled_disposition_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    if let Some(row) = row_at(tr, parent).await? {
        require_finished_at(tr, parent, &row).await?;
    }
    Ok(())
}

/// Fresh physical writes require the original in-memory admission and no
/// disposition. An existing may-have-started intent never mints that admission.
pub(in crate::managed_catalog) async fn require_fresh_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    if row_at(tr, parent).await?.is_some() {
        return Err(CatalogError::Invalid(
            "BlockedUnknown：隔离中的原任务不得新写",
        ));
    }
    Ok(())
}
/// A completed original group may be followed only by one of its sealed,
/// still-uncovered original directories. This never admits another body group.
pub(in crate::managed_catalog) async fn require_metadata_successor_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    plan: &source::ReclaimMetadataPlan,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    if parent.groups_archive_root() && plan.is_source_root() {
        return Err(CatalogError::Invalid("原归档根只能随回收组检查点写入"));
    }
    let Some(row) = row_at(tr, parent).await? else {
        if parent.groups_archive_root() {
            let id = source::group_only::planned_group_at(tr, parent)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let group = evidence_group_at(tr, parent, id).await?;
            if group.header.phase != Phase::Closed {
                return Err(CatalogError::Invalid("回收独立目录须等待原组关闭"));
            }
            return source::group_only::require_directory_successor_at(tr, parent, owner, id, plan)
                .await;
        }
        return Ok(());
    };
    require_finished_at(tr, parent, &row).await?;
    let Subject::Group { group, .. } = &row.subject else {
        return Err(CatalogError::Invalid(
            "BlockedUnknown：目录后继缺少原组证明",
        ));
    };
    source::group_only::require_directory_successor_at(tr, parent, owner, parse_uuid(group)?, plan)
        .await
}
/// Guards ordinary cancellation, reaping, initialization and cleanup before any
/// mutation. Admission itself supplies the distinct one-use Fresh capability.
pub(in crate::managed_catalog) async fn require_normal_mutation_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    if let Some(row) = row_at(tr, parent).await? {
        require_finished_at(tr, parent, &row).await?;
    }
    // A terminal receipt has already released the parent journal and claim.
    // Isolation, if present, was checked above; ordinary completed cleanup
    // must not demand a live reserved/copying journal.
    if format::result_at(tr, parent).await?.is_some() {
        return Ok(());
    }
    let live = live_parent_at(tr, parent, owner).await?;
    if matches!(live["phase"].as_str(), Some("reserved" | "copying")) {
        if let Some(group) = load_at(tr, parent, owner).await?
            && matches!(group.header.phase, Phase::Admitted | Phase::Settling)
        {
            return Err(CatalogError::Invalid(
                "BlockedUnknown：原已准入attempt须先结清，禁止释放或格式化",
            ));
        }
        if let Some(task) = source::isolation_checkpoint_at(tr, parent, owner).await?
            && (task.writing() || task.recovering())
        {
            return Err(CatalogError::Invalid(
                "BlockedUnknown：原MetadataTask写入结果未结清",
            ));
        }
    }
    Ok(())
}
/// Only existing private, validated group facts may settle within the single
/// observation window. Reapers/cancel/format never use this entry.
pub(in crate::managed_catalog) async fn require_group_action_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    if let Some(row) = row_at(tr, parent).await? {
        match &row.disposition {
            Disposition::ObservationStarted { owner: holder, .. }
                if *holder == owner
                    && group_subject_at(tr, parent, group).await? == row.subject => {}
            Disposition::ObservedKnownFull { .. } => require_finished_at(tr, parent, &row).await?,
            _ => {
                return Err(CatalogError::Invalid(
                    "BlockedUnknown：原组已隔离，拒绝发布/退休/结清",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::managed_catalog) fn directory_unclosed_header_for_test(
    raw: &[u8],
) -> Result<Vec<u8>> {
    let mut header: super::super::Header = native_decode(raw)?;
    header.phase = Phase::Admitted;
    super::super::encode(&header)
}
