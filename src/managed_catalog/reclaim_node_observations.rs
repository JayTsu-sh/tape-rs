//! Durable read observations, never logical relocation or erase checkpoints.
//! The fingerprint is part of the immutable key and of the caller's task
//! result; a corrupt value is diagnosed rather than repaired or overwritten.
use super::*;
use crate::managed_catalog::control::verified_placement;

fn key(reservation: &ReclaimReservation, uid: u64, fingerprint: [u8; 32]) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'n');
    key.extend_from_slice(&uid.to_be_bytes());
    key.extend_from_slice(&fingerprint);
    key
}

async fn base_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<ReclaimSourceProgress> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let (_, index) = source_state_at(tr, reservation).await?;
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    let mut head = Vec::new();
    active_volume::encode_index(&mut head, index);
    if manifest.summary.head != head || manifest.summary.volume != index.volume_uuid.to_string() {
        return Err(CatalogError::Corrupt("原始节点观察的源封存索引不一致"));
    }
    Ok(manifest)
}

async fn require_source_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    manifest: &ReclaimSourceProgress,
    evidence: &NodeEvidence,
) -> Result<()> {
    if evidence.version != 1
        || evidence.parent != reservation.fingerprint()
        || evidence.inventory != manifest.summary.digest
        || !matches!(
            evidence.source.kind,
            ReclaimSourceNodeKind::Directory | ReclaimSourceNodeKind::Symlink
        )
        || evidence.target.kind != evidence.source.kind
        || evidence.target.length != evidence.source.length
    {
        return Err(CatalogError::Corrupt("原始节点观察身份、类型或原清单改变"));
    }
    evidence.target.encode()?;
    let expected = evidence.source.encode()?;
    if tr
        .get(&ledger::object_key(reservation, evidence.source.uid), false)
        .await?
        .as_deref()
        != Some(expected.as_slice())
    {
        return Err(CatalogError::Corrupt("原始节点观察与源封存行不符"));
    }
    Ok(())
}

async fn require_target_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    evidence: &NodeEvidence,
) -> Result<()> {
    let spec = reservation.spec();
    let volume = Uuid::parse_str(&evidence.target_volume)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == evidence.target_volume)
        .ok_or(CatalogError::Corrupt("原始节点观察目的卷编码"))?;
    if volume == spec.volume {
        return Err(CatalogError::Corrupt("原始节点观察目的不能是源卷"));
    }
    let verified = active_volume::decode_index(volume, &evidence.target_index)?;
    let placement = verified_placement(tr, volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.namespace != reservation.namespace()
        || placement.pool_id != spec.pool
        || placement.library_serial != reservation.serial()
        || placement.barcode != evidence.target_barcode
    {
        return Err(CatalogError::Invalid("原始节点观察目的归属已改变"));
    }
    let assignment =
        ManagedCatalog::verified_assignment(tr, reservation.serial(), &placement.barcode)
            .await?
            .ok_or(CatalogError::Incomplete)?;
    if assignment.volume_id != Some(volume) || !assignment.allows_io() {
        return Err(CatalogError::Invalid("原始节点观察目的磁带当前不可用"));
    }
    let (scan, current) = active_volume::observed_at(tr, reservation.serial(), volume).await?;
    if verified.generation < scan.generation || verified.generation > current.generation {
        return Err(CatalogError::Invalid("原始节点观察不再属于原目的索引链"));
    }
    // Imported baselines are anchored by their source identity, not an active
    // generation record. Later commits must not hide their observations merely
    // because no history key exists at that baseline.
    let known = if verified.generation == scan.generation {
        index_evidence::require_identity(tr, volume, scan)
            .await?
            .head()
            .into()
    } else if verified.generation == current.generation {
        current
    } else {
        active_volume::history_at(tr, reservation.serial(), volume, verified.generation)
            .await?
            .1
    };
    if known != verified {
        return Err(CatalogError::Invalid("原始节点观察的目的索引身份改变"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Record 1..32 observations from one target capture. This is idempotent
    /// by content fingerprint and does not publish nodes, retire references,
    /// count coverage, notify completion, or authorize any device operation.
    pub async fn record_reclaim_node_observations(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        observations: &[ReclaimNodeMetadata],
    ) -> Result<Vec<[u8; 32]>> {
        if !(1..=PAGE).contains(&observations.len()) {
            return Err(CatalogError::Invalid(
                "原始节点观察记录批次必须为 1..=32 项",
            ));
        }
        let first = &observations[0].0;
        let mut seen = std::collections::HashSet::new();
        let mut prepared = Vec::with_capacity(observations.len());
        for observation in observations {
            let proof = &observation.0;
            if !seen.insert(proof.source.uid)
                || proof.target_volume != first.target_volume
                || proof.target_barcode != first.target_barcode
                || proof.target_index != first.target_index
            {
                return Err(CatalogError::Invalid(
                    "原始节点观察批次重复或来自不同目的构造",
                ));
            }
            let raw = encode_observation(proof)?;
            let fingerprint = observation_fingerprint(&raw);
            prepared.push((
                key(reservation, proof.source.uid, fingerprint),
                raw,
                fingerprint,
            ));
        }
        self.db
            .run(|tr, _| {
                let prepared = &prepared;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let manifest = base_at(&tr, reservation, owner).await?;
                            require_target_at(&tr, reservation, first).await?;
                            for (observation, (key, raw, _)) in observations.iter().zip(prepared) {
                                require_source_at(&tr, reservation, &manifest, &observation.0)
                                    .await?;
                                if let Some(old) = tr.get(key, false).await? {
                                    if old.as_ref() != raw.as_slice() {
                                        return Err(CatalogError::Corrupt(
                                            "原始节点观察指纹键存在不同内容",
                                        ));
                                    }
                                }
                            }
                            let fingerprints = prepared.iter().map(|(_, _, fp)| *fp).collect();
                            // retry_result commits domain errors. Validate the entire
                            // batch and prepare its result before the first mutation.
                            for (key, raw, _) in prepared {
                                tr.set(key, raw);
                            }
                            Ok(fingerprints)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Reuse a historical observation after a lost reply or takeover. Current
    /// owner/source/assignment/index-chain checks run again, without tape I/O.
    /// This is not a new physical read, logical publication or erase proof.
    pub async fn reclaim_node_observation(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source_uid: u64,
        fingerprint: [u8; 32],
    ) -> Result<Option<ReclaimNodeMetadata>> {
        if source_uid == 0 {
            return Err(CatalogError::Invalid("原始节点观察源 UID 不能为零"));
        }
        let tr = self.db.create_trx()?;
        let manifest = base_at(&tr, reservation, owner).await?;
        let Some(raw) = tr
            .get(&key(reservation, source_uid, fingerprint), false)
            .await?
        else {
            return Ok(None);
        };
        if raw.len() > 90_000 || observation_fingerprint(&raw) != fingerprint {
            return Err(CatalogError::Corrupt("原始节点观察内容与指纹键不符"));
        }
        let proof: NodeEvidence = serde_json::from_slice(&raw).map_err(cache)?;
        if encode_observation(&proof)?.as_slice() != raw.as_ref() || proof.source.uid != source_uid
        {
            return Err(CatalogError::Corrupt("原始节点观察编码或源 UID 不符"));
        }
        require_source_at(&tr, reservation, &manifest, &proof).await?;
        require_target_at(&tr, reservation, &proof).await?;
        Ok(Some(ReclaimNodeMetadata(proof)))
    }
}
