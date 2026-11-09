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
fn encode(row: &Coverage) -> Result<Vec<u8>> {
    row.source.encode()?;
    let raw = serde_json::to_vec(row).map_err(cache)?;
    if row.version != 1 || raw.len() > 90_000 {
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
    let actual = match &row.proof {
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
async fn put_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    source: Object,
    proof: Proof,
) -> Result<()> {
    let manifest = base_at(tr, reservation, owner).await?;
    let row = Coverage {
        version: 1,
        parent: reservation.fingerprint(),
        inventory: manifest.summary.digest,
        source,
        proof,
    };
    let raw = encode(&row)?;
    verify_at(tr, reservation, owner, &manifest, &row).await?;
    let key = key(reservation, row.source.uid);
    if let Some(old) = tr.get(&key, false).await? {
        if old.as_ref() != raw {
            return Err(CatalogError::Invalid("原始源节点已有不同覆盖证明"));
        }
        return Ok(());
    }
    tr.set(&key, &raw);
    Ok(())
}
async fn progress_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    manifest: &ReclaimSourceProgress,
) -> Result<ReclaimCoverageProgress> {
    let Some(raw) = tr.get(&ledger::own_key(reservation, b'y'), false).await? else {
        return Ok(ReclaimCoverageProgress {
            version: 1,
            seal: ReclaimSourceProgress {
                version: 1,
                parent: reservation.fingerprint(),
                summary: manifest.summary.clone(),
                counts: Counts::default(),
                after: None,
                digest: manifest.summary.seed(),
                complete: false,
            },
        });
    };
    let progress: ReclaimCoverageProgress = serde_json::from_slice(&raw).map_err(cache)?;
    if progress.version != 1
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
        .chain_update(b"tape-rs/reclaim-complete-coverage/1")
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
