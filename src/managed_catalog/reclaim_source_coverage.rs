//! Independently cover every sealed physical node, including the root and
//! nodes absent from the logical namespace. This is one of two format gates;
//! it never grants device authority or replaces zero-reference validation.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Proof {
    RawFile {
        attempt: String,
    },
    Node {
        observation: [u8; 32],
    },
    EeAlias {
        file: ReclaimFile,
        epoch: u64,
        request: [u8; 32],
    },
    GroupFile {
        group: String,
        physical_attempt: String,
        batch: String,
        ordinal: u8,
        plan: [u8; 32],
        observation: [u8; 32],
        receipt: [u8; 32],
    },
    GroupRoot {
        reference: copy_groups::GroupRootReference,
    },
}

impl Proof {
    fn version(&self) -> u8 {
        match self {
            Self::GroupRoot { .. } => 3,
            Self::GroupFile { .. } => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Coverage {
    version: u8,
    parent: String,
    inventory: [u8; 32],
    source: Object,
    proof: Proof,
}

/// Bounded progress is durable, but neither this value nor a caller-supplied
/// counter is accepted as a source-format permit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimCoverageProgress {
    version: u8,
    seal: ReclaimSourceProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coverage_digest: Option<[u8; 32]>,
}
impl ReclaimCoverageProgress {
    pub fn complete(&self) -> bool {
        self.seal.complete()
    }
    pub fn objects(&self) -> u64 {
        self.seal.objects()
    }
    pub fn total_objects(&self) -> u64 {
        self.seal.total_objects()
    }
}

fn key(reservation: &ReclaimReservation, uid: u64) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'q');
    key.extend_from_slice(&uid.to_be_bytes());
    key
}
#[cfg(test)]
pub(in crate::managed_catalog) fn group_coverage_test_key(
    parent: &ReclaimReservation,
    uid: u64,
) -> Vec<u8> {
    key(parent, uid)
}
fn encode(row: &Coverage) -> Result<Vec<u8>> {
    row.source.encode()?;
    let raw = serde_json::to_vec(row).map_err(cache)?;
    if row.version != row.proof.version() || raw.len() > 90_000 {
        return Err(CatalogError::Corrupt("原始节点覆盖版本或长度无效"));
    }
    Ok(raw)
}
async fn base_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<ReclaimSourceProgress> {
    if record_at(tr, reservation, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let (_, index) = source_state_at(tr, reservation).await?;
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    let mut head = Vec::new();
    active_volume::encode_index(&mut head, index);
    if head != manifest.summary.head || index.volume_uuid.to_string() != manifest.summary.volume {
        return Err(CatalogError::Corrupt("覆盖依据与原封存源索引不同"));
    }
    Ok(manifest)
}
async fn verify_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    manifest: &ReclaimSourceProgress,
    row: &Coverage,
) -> Result<()> {
    Box::pin(verify_in_at(
        tr,
        reservation,
        owner,
        manifest,
        row,
        GroupCoverageMode::Stored,
    ))
    .await
}
async fn verify_in_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    manifest: &ReclaimSourceProgress,
    row: &Coverage,
    mode: GroupCoverageMode<'_>,
) -> Result<()> {
    if row.parent != reservation.fingerprint()
        || row.inventory != manifest.summary.digest
        || tr
            .get(&ledger::object_key(reservation, row.source.uid), false)
            .await?
            .as_deref()
            != Some(row.source.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("覆盖节点不属于原完整源清单"));
    }
    if row.version != row.proof.version() {
        return Err(CatalogError::Corrupt("源覆盖版本与proof kind不同"));
    }
    let actual = match &row.proof {
        Proof::GroupRoot { reference } => {
            Box::pin(copy_groups::require_group_root_coverage_at(
                tr,
                reservation,
                reference,
            ))
            .await?
            .0
        }
        Proof::GroupFile {
            group,
            physical_attempt,
            batch,
            ordinal,
            plan,
            observation,
            receipt,
        } => {
            let reference = GroupFileReference {
                group: group.clone(),
                physical_attempt: physical_attempt.clone(),
                batch: batch.clone(),
                ordinal: *ordinal,
                plan: *plan,
                observation: *observation,
                receipt: *receipt,
            };
            group_member_coverage_in_at(
                tr,
                reservation,
                owner,
                manifest.summary.digest,
                row.source.uid,
                &reference,
                mode,
            )
            .await?
            .0
        }
        Proof::RawFile { attempt } => {
            let id = node_mutation::id(attempt)?;
            Box::pin(metadata_write::coverage_at(
                tr,
                reservation,
                owner,
                row.source.uid,
                id,
            ))
            .await?
        }
        Proof::Node { observation } => {
            if reservation.groups_archive_root() && row.source.path.is_empty() {
                return Err(CatalogError::Invalid("归档根覆盖必须来自原关闭组"));
            }
            node_metadata::coverage_at(tr, reservation, manifest, row.source.uid, *observation)
                .await?
        }
        Proof::EeAlias {
            file,
            epoch,
            request,
        } => {
            file.validate()?;
            let (intent, _) = children::child_at(
                tr,
                reservation.namespace(),
                reservation.spec().pool,
                *epoch,
                file.operation(),
                owner,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            let original = intent.request();
            if original.fingerprint() != *request {
                return Err(CatalogError::Corrupt("别名覆盖改变原子请求"));
            }
            Box::pin(metadata::alias_coverage_at(
                tr,
                reservation,
                owner,
                file,
                &original,
            ))
            .await?
        }
    };
    if actual != row.source {
        return Err(CatalogError::Corrupt("覆盖证明改变原始源节点"));
    }
    Ok(())
}
async fn put_bytes_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    source: Object,
    proof: Proof,
) -> Result<Vec<u8>> {
    Box::pin(put_bytes_in_at(
        tr,
        reservation,
        owner,
        source,
        proof,
        GroupCoverageMode::Stored,
    ))
    .await
}
async fn put_bytes_in_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    source: Object,
    proof: Proof,
    mode: GroupCoverageMode<'_>,
) -> Result<Vec<u8>> {
    let manifest = base_at(tr, reservation, owner).await?;
    let row = Coverage {
        version: proof.version(),
        parent: reservation.fingerprint(),
        inventory: manifest.summary.digest,
        source,
        proof,
    };
    let raw = encode(&row)?;
    verify_in_at(tr, reservation, owner, &manifest, &row, mode).await?;
    let key = key(reservation, row.source.uid);
    if let Some(old) = tr.get(&key, false).await? {
        if old.as_ref() != raw {
            return Err(CatalogError::Invalid("原始源节点已有不同覆盖证明"));
        }
        return Ok(raw);
    }
    tr.set(&key, &raw);
    Ok(raw)
}
async fn put_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    source: Object,
    proof: Proof,
) -> Result<()> {
    put_bytes_at(tr, reservation, owner, source, proof)
        .await
        .map(|_| ())
}
fn group_proof(reference: &GroupFileReference) -> Proof {
    Proof::GroupFile {
        group: reference.group.clone(),
        physical_attempt: reference.physical_attempt.clone(),
        batch: reference.batch.clone(),
        ordinal: reference.ordinal,
        plan: reference.plan,
        observation: reference.observation,
        receipt: reference.receipt,
    }
}
fn group_row_digest(raw: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-member-coverage/2")
        .chain_update(raw)
        .finalize()
        .into()
}

pub(in crate::managed_catalog::control::reclaim) enum GroupRootEvidence {
    Closing,
    Closed,
    Completed([u8; 32]),
}

/// Close validates the original source ledger. After terminal format, the
/// independently retained receipt can reproduce the same coverage digest.
pub(in crate::managed_catalog::control::reclaim) async fn group_root_bundle_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    entry: &IndexEntry,
    reference: &copy_groups::GroupRootReference,
    evidence: GroupRootEvidence,
) -> Result<[u8; 32]> {
    let source = Object::capture(entry.clone())?;
    if !parent.groups_archive_root() || !source.path.is_empty() {
        return Err(CatalogError::Corrupt("归档根覆盖不是原完整源清单的根目录"));
    }
    let inventory = match evidence {
        GroupRootEvidence::Completed(inventory) => inventory,
        _ => {
            let manifest = ledger::require_complete_at(tr, parent).await?;
            if tr
                .get(&ledger::object_key(parent, source.uid), false)
                .await?
                .as_deref()
                != Some(source.encode()?.as_slice())
            {
                return Err(CatalogError::Corrupt("归档根覆盖不是原完整源清单的根目录"));
            }
            manifest.summary.digest
        }
    };
    let key = key(parent, source.uid);
    let raw = encode(&Coverage {
        version: 3,
        parent: parent.fingerprint(),
        inventory,
        source,
        proof: Proof::GroupRoot {
            reference: reference.clone(),
        },
    })?;
    if !matches!(evidence, GroupRootEvidence::Completed(_)) {
        match (evidence, tr.get(&key, false).await?) {
            (GroupRootEvidence::Closed, Some(saved)) if saved.as_ref() == raw => {}
            (GroupRootEvidence::Closing, None) => tr.set(&key, &raw),
            _ => return Err(CatalogError::Corrupt("原关闭组缺正确的归档根覆盖")),
        }
    }
    Ok(Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-root-coverage/3")
        .chain_update(&raw)
        .finalize()
        .into())
}
pub(in crate::managed_catalog) async fn put_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    node: ReclaimSourceNode,
    reference: &GroupFileReference,
    pins: &GroupRetirementPins<'_>,
) -> Result<[u8; 32]> {
    let raw = put_bytes_in_at(
        tr,
        parent,
        owner,
        node.0,
        group_proof(reference),
        GroupCoverageMode::Staged(pins),
    )
    .await?;
    Ok(group_row_digest(&raw))
}
pub(in crate::managed_catalog) async fn group_bundle_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    uid: u64,
    reference: &GroupFileReference,
) -> Result<[u8; 32]> {
    let manifest = base_at(tr, parent, owner).await?;
    let raw = tr
        .get(&key(parent, uid), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
    if encode(&row)?.as_slice() != raw.as_ref()
        || row.source.uid != uid
        || row.proof != group_proof(reference)
    {
        return Err(CatalogError::Corrupt("原分组成员Rq bundle改变"));
    }
    verify_at(tr, parent, owner, &manifest, &row).await?;
    Ok(group_row_digest(&raw))
}
/// Canonical identity only. Callers must independently validate original
/// publication, retirement and member binding; this helper grants no coverage.
pub(in crate::managed_catalog) async fn group_bundle_identity_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    uid: u64,
    reference: &GroupFileReference,
) -> Result<[u8; 32]> {
    let manifest = base_at(tr, parent, owner).await?;
    let raw = tr
        .get(&key(parent, uid), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
    if encode(&row)?.as_slice() != raw.as_ref()
        || row.version != 2
        || row.parent != parent.fingerprint()
        || row.inventory != manifest.summary.digest
        || row.source.uid != uid
        || row.proof != group_proof(reference)
        || tr
            .get(&ledger::object_key(parent, uid), false)
            .await?
            .as_deref()
            != Some(row.source.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("持久原成员Rq canonical identity改变"));
    }
    Ok(group_row_digest(&raw))
}
pub(in crate::managed_catalog) async fn require_group_absent_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    uid: u64,
) -> Result<()> {
    if tr.get(&key(parent, uid), false).await?.is_some() {
        return Err(CatalogError::Corrupt("未结清成员出现半套Rq记录"));
    }
    Ok(())
}

fn coverage_seed(parent: &str, inventory: [u8; 32]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-covered-proofs/2")
        .chain_update((parent.len() as u64).to_be_bytes())
        .chain_update(parent.as_bytes())
        .chain_update(inventory)
        .finalize()
        .into()
}
fn coverage_step(previous: [u8; 32], ordinal: u64, canonical_rq: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-covered-proof-row/2")
        .chain_update(previous)
        .chain_update(ordinal.to_be_bytes())
        .chain_update((canonical_rq.len() as u64).to_be_bytes())
        .chain_update(canonical_rq)
        .finalize()
        .into()
}
fn progress_version(reservation: &ReclaimReservation) -> Result<u8> {
    match reservation.intent()["version"].as_u64() {
        Some(1) => Ok(1),
        Some(2) => Ok(2),
        _ => Err(CatalogError::Corrupt("源覆盖父协议版本无效")),
    }
}

async fn progress_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    manifest: &ReclaimSourceProgress,
) -> Result<ReclaimCoverageProgress> {
    let version = progress_version(reservation)?;
    let seed =
        (version == 2).then(|| coverage_seed(&reservation.fingerprint(), manifest.summary.digest));
    let Some(raw) = tr.get(&ledger::own_key(reservation, b'y'), false).await? else {
        return Ok(ReclaimCoverageProgress {
            version,
            seal: ReclaimSourceProgress {
                version: 1,
                parent: reservation.fingerprint(),
                summary: manifest.summary.clone(),
                counts: Counts::default(),
                after: None,
                digest: manifest.summary.seed(),
                complete: false,
            },
            coverage_digest: seed,
        });
    };
    let progress: ReclaimCoverageProgress = serde_json::from_slice(&raw).map_err(cache)?;
    if progress.version != version
        || progress.coverage_digest.is_some() != (version == 2)
        || (progress.seal.counts.objects == 0 && progress.coverage_digest != seed)
        || progress.seal.summary != manifest.summary
        || serde_json::to_vec(&progress).map_err(cache)?.as_slice() != raw.as_ref()
    {
        return Err(CatalogError::Corrupt("原始源覆盖进度损坏或清单改变"));
    }
    ledger::validate(&progress.seal, reservation)?;
    Ok(progress)
}

pub(in super::super) async fn require_complete_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<[u8; 32]> {
    let manifest = base_at(tr, reservation, owner).await?;
    require_metadata_idle_at(tr, reservation).await?;
    let progress = progress_at(tr, reservation, &manifest).await?;
    if !progress.complete() {
        return Err(CatalogError::Incomplete);
    }
    Ok(Sha256::new()
        .chain_update(if progress.version == 1 {
            b"tape-rs/reclaim-complete-coverage/1".as_slice()
        } else {
            b"tape-rs/reclaim-complete-coverage/2".as_slice()
        })
        .chain_update(serde_json::to_vec(&progress).map_err(cache)?)
        .finalize()
        .into())
}

pub(in super::super) async fn completed_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<bool> {
    if tr
        .get(&ledger::own_key(reservation, b'y'), false)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    let manifest = base_at(tr, reservation, owner).await?;
    Ok(progress_at(tr, reservation, &manifest).await?.complete())
}

impl ManagedCatalog {
    /// Read the original durable coverage cursor without advancing it or
    /// manufacturing a coverage/format proof from a caller-provided counter.
    pub(crate) async fn reclaim_source_coverage_progress(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<ReclaimCoverageProgress> {
        let tr = self.db.create_trx()?;
        let manifest = base_at(&tr, reservation, owner).await?;
        progress_at(&tr, reservation, &manifest).await
    }

    /// Cover one full-fidelity raw file only after its metadata commit or
    /// recovered original commit, never after Discard/Checkpoint/Closed.
    pub async fn seal_reclaim_raw_file_coverage(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        uid: u64,
        attempt: Uuid,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let source =
                                metadata_write::coverage_at(&tr, reservation, owner, uid, attempt)
                                    .await?;
                            put_at(
                                &tr,
                                reservation,
                                owner,
                                source,
                                Proof::RawFile {
                                    attempt: attempt.to_string(),
                                },
                            )
                            .await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    /// Read observations must already be durably recorded. Merely specifying
    /// a destination UID or presenting a cached observation is insufficient.
    pub async fn seal_reclaim_node_coverage(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        observation: &ReclaimNodeMetadata,
    ) -> Result<()> {
        let fingerprint = observation.fingerprint()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            put_at(
                                &tr,
                                reservation,
                                owner,
                                observation.source_object().clone(),
                                Proof::Node {
                                    observation: fingerprint,
                                },
                            )
                            .await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    /// EE links are materialized as ordinary files by the data path; their
    /// original user attributes and exact body relocation must both be sealed.
    pub async fn seal_reclaim_alias_coverage(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let source =
                                metadata::alias_coverage_at(&tr, reservation, owner, file, request)
                                    .await?;
                            put_at(
                                &tr,
                                reservation,
                                owner,
                                source,
                                Proof::EeAlias {
                                    file: file.clone(),
                                    epoch: request.request_epoch,
                                    request: request.fingerprint(),
                                },
                            )
                            .await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    /// Revalidate one retained independent source-coverage row. A committed
    /// receipt is reused without issuing a different observation at a newer head.
    pub async fn reclaim_source_node_covered(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
        uid: u64,
    ) -> Result<bool> {
        let tr = self.db.create_trx()?;
        let manifest = base_at(&tr, reservation, owner).await?;
        if inventory.summary != manifest.summary {
            return Err(CatalogError::Invalid("覆盖查询改变原清单"));
        }
        let original = Object::capture(
            inventory
                .source
                .entry_by_uid(uid)
                .map_err(cache)?
                .ok_or(CatalogError::Incomplete)?,
        )?;
        let Some(raw) = tr.get(&key(reservation, uid), false).await? else {
            let progress = progress_at(&tr, reservation, &manifest).await?;
            if progress
                .seal
                .after
                .as_ref()
                .is_some_and(|after| after >= &original.path)
            {
                return Err(CatalogError::Corrupt("已检查源节点的覆盖行缺失"));
            }
            return Ok(false);
        };
        let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
        if row.source != original || encode(&row)?.as_slice() != raw.as_ref() {
            return Err(CatalogError::Corrupt("原覆盖行改变源节点"));
        }
        verify_at(&tr, reservation, owner, &manifest, &row).await?;
        Ok(true)
    }

    /// Return missing work from the next bounded coverage page. Existing rows
    /// are revalidated; neither a missing row nor this query advances progress.
    pub async fn reclaim_uncovered_source_nodes(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
    ) -> Result<Vec<ReclaimSourceNode>> {
        let tr = self.db.create_trx()?;
        let manifest = base_at(&tr, reservation, owner).await?;
        require_metadata_idle_at(&tr, reservation).await?;
        if inventory.summary != manifest.summary {
            return Err(CatalogError::Invalid("覆盖查询必须使用原实际源清单"));
        }
        let progress = progress_at(&tr, reservation, &manifest).await?;
        if progress.complete() {
            return Ok(Vec::new());
        }
        let mut pending = Vec::new();
        for object in inventory.page(progress.seal.after.as_deref(), 16)? {
            if let Some(raw) = tr.get(&key(reservation, object.uid), false).await? {
                let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
                if row.source != object || encode(&row)?.as_slice() != raw.as_ref() {
                    return Err(CatalogError::Corrupt("源覆盖查询发现原节点被改变"));
                }
                verify_at(&tr, reservation, owner, &manifest, &row).await?;
            } else {
                pending.push(ReclaimSourceNode(object));
            }
        }
        Ok(pending)
    }

    /// Stream at most sixteen source nodes in physical path order. Every
    /// original node, including unknown-to-FDB files and empty directories,
    /// needs an independently checked destination; no node can be skipped.
    pub async fn step_reclaim_source_coverage(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
    ) -> Result<ReclaimCoverageProgress> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let manifest = base_at(&tr, reservation, owner).await?;
                            require_metadata_idle_at(&tr, reservation).await?;
                            if inventory.summary != manifest.summary {
                                return Err(CatalogError::Invalid(
                                    "覆盖检查必须使用原实际捕获源索引",
                                ));
                            }
                            let mut progress = progress_at(&tr, reservation, &manifest).await?;
                            if progress.complete() {
                                return Ok(progress);
                            }
                            let rows = inventory.page(progress.seal.after.as_deref(), 16)?;
                            if rows.is_empty() {
                                return Err(CatalogError::Corrupt("源覆盖在原对象数之前结束"));
                            }
                            for object in rows {
                                let raw = tr
                                    .get(&key(reservation, object.uid), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
                                if row.source != object || encode(&row)?.as_slice() != raw.as_ref()
                                {
                                    return Err(CatalogError::Corrupt("源覆盖行改变原清单节点"));
                                }
                                verify_at(&tr, reservation, owner, &manifest, &row).await?;
                                progress.seal.counts.add(&object)?;
                                progress.seal.digest = advance_digest(
                                    progress.seal.digest,
                                    progress.seal.counts.objects,
                                    &object.encode()?,
                                );
                                if let Some(previous) = progress.coverage_digest {
                                    progress.coverage_digest = Some(coverage_step(
                                        previous,
                                        progress.seal.counts.objects,
                                        &raw,
                                    ));
                                }
                                progress.seal.after = Some(object.path);
                            }
                            progress.seal.complete =
                                progress.seal.counts.objects == manifest.summary.counts.objects;
                            ledger::validate(&progress.seal, reservation)?;
                            let raw = serde_json::to_vec(&progress).map_err(cache)?;
                            tr.set(&ledger::own_key(reservation, b'y'), &raw);
                            Ok(progress)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

// Planning is readonly and may address the root before walk.current exists.
// A missing row behind the checked coverage cursor remains corruption.
pub(in super::super) async fn require_uncovered_uid_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    original: &ReclaimSourceNode,
) -> Result<()> {
    let manifest = base_at(tr, reservation, owner).await?;
    if tr
        .get(&ledger::object_key(reservation, original.uid()), false)
        .await?
        .as_deref()
        != Some(original.0.encode()?.as_slice())
    {
        return Err(CatalogError::Invalid("规划节点不属于原封存源清单"));
    }
    if let Some(raw) = tr.get(&key(reservation, original.uid()), false).await? {
        let row: Coverage = serde_json::from_slice(&raw).map_err(cache)?;
        if row.source != original.0 || encode(&row)?.as_slice() != raw.as_ref() {
            return Err(CatalogError::Corrupt("规划源覆盖行改变原节点"));
        }
        verify_at(tr, reservation, owner, &manifest, &row).await?;
        return Err(CatalogError::Invalid("规划源节点已经完成覆盖"));
    }
    let progress = progress_at(tr, reservation, &manifest).await?;
    if progress
        .seal
        .after
        .as_ref()
        .is_some_and(|after| after >= &original.0.path)
    {
        return Err(CatalogError::Corrupt("已检查源节点的覆盖行缺失"));
    }
    Ok(())
}

#[cfg(test)]
mod coverage_progress_tests {
    use super::*;
    fn progress() -> ReclaimCoverageProgress {
        let summary = Summary {
            volume: Uuid::nil().to_string(),
            head: vec![],
            identity: [1; 32],
            dp_eod: 10,
            ip_generation: Some(1),
            history_depth: 1,
            counts: Counts::default(),
            digest: [2; 32],
        };
        ReclaimCoverageProgress {
            version: 1,
            seal: ReclaimSourceProgress {
                version: 1,
                parent: "parent".into(),
                digest: summary.seed(),
                summary,
                counts: Counts::default(),
                after: None,
                complete: false,
            },
            coverage_digest: None,
        }
    }
    #[test]
    fn group_file_coverage_cannot_use_legacy_or_unknown_version() {
        let mut row = Coverage {
            version: 2,
            parent: "parent".into(),
            inventory: [1; 32],
            source: Object {
                path: "source".into(),
                uid: 2,
                kind: ReclaimSourceNodeKind::File,
                length: Some(4),
                sha256: [3; 32],
            },
            proof: Proof::GroupFile {
                group: Uuid::new_v4().to_string(),
                physical_attempt: Uuid::new_v4().to_string(),
                batch: Uuid::new_v4().to_string(),
                ordinal: 0,
                plan: [4; 32],
                observation: [5; 32],
                receipt: [6; 32],
            },
        };
        let raw = encode(&row).unwrap();
        assert_eq!(serde_json::from_slice::<Coverage>(&raw).unwrap(), row);
        row.version = 1;
        assert!(encode(&row).is_err());
        row.version = 3;
        assert!(encode(&row).is_err());
        row.proof = Proof::Node {
            observation: [7; 32],
        };
        row.version = 2;
        assert!(encode(&row).is_err());
        row.version = 1;
        assert!(encode(&row).is_ok());
    }

    #[test]
    fn legacy_coverage_progress_keeps_exact_bytes_without_new_field() {
        let progress = progress();
        let expected = json!({"version":1,"seal":progress.seal});
        // Preserve struct field order as well as omitting an absent extension.
        let encoded = serde_json::to_vec(&progress).unwrap();
        let legacy = format!(
            "{{\"version\":1,\"seal\":{}}}",
            serde_json::to_string(&progress.seal).unwrap()
        );
        assert_eq!(encoded, legacy.as_bytes());
        let decoded: ReclaimCoverageProgress = serde_json::from_value(expected).unwrap();
        assert_eq!(decoded, progress);
        assert!(decoded.coverage_digest.is_none());
    }
    #[test]
    fn proof_digest_binds_canonical_rq_values_order_and_parent() {
        let seed = coverage_seed("parent", [2; 32]);
        assert_ne!(seed, coverage_seed("other-parent", [2; 32]));
        let first = coverage_step(seed, 1, b"original-proof");
        assert_ne!(first, coverage_step(seed, 1, b"different-proof"));
        assert_ne!(first, coverage_step(seed, 2, b"original-proof"));
        assert_ne!(
            coverage_step(first, 2, b"second-proof"),
            coverage_step(
                coverage_step(seed, 1, b"second-proof"),
                2,
                b"original-proof"
            )
        );
        let mut upgraded = progress();
        upgraded.version = 2;
        upgraded.coverage_digest = Some(first);
        let raw = serde_json::to_vec(&upgraded).unwrap();
        assert_eq!(
            serde_json::from_slice::<ReclaimCoverageProgress>(&raw).unwrap(),
            upgraded
        );
    }
}
