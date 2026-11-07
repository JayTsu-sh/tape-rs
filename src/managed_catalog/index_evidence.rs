//! Immutable on-media index identity for each managed volume scan.
//! The value is small and durable; physical extents remain in a rebuildable local cache.

use std::path::Path;

use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};

use super::*;
use crate::ltfs::recovery::IndexSourceIdentity;
use crate::ltfs::volume::CommittedIndexConstruct;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOutcome {
    Recorded,
    AlreadyRecorded,
    StaleOwner,
    NotReconciling,
    SourceConflict,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalVolumeSources<'a> {
    pub extent_cache: &'a Path,
    pub import_spool: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSourceBinding {
    pub placement: VolumePlacement,
    pub scan: VolumeScan,
    pub source: ExpectedIndex,
}

fn validate_source(volume: Uuid, scan: VolumeScan, source: ExpectedIndex) -> Result<()> {
    validate_scan(volume, scan)?;
    if source.volume_uuid != volume
        || source.generation != scan.generation
        || !matches!(source.location.partition, 'a' | 'b')
        || source.location.start_block == 0
    {
        return Err(CatalogError::Invalid("卷索引证据与扫描身份或位置不符"));
    }
    Ok(())
}

pub(super) fn encode_source(source: &IndexSourceIdentity, file_count: u64) -> Vec<u8> {
    let mut value = Vec::new();
    value.push(1);
    value.extend_from_slice(&file_count.to_be_bytes());
    value.extend_from_slice(&source.to_bytes());
    value
}

pub(super) fn decode_source(
    volume: Uuid,
    scan: VolumeScan,
    value: &[u8],
) -> Result<(IndexSourceIdentity, u64)> {
    if value.len() < 77 || value[0] != 1 {
        return Err(CatalogError::Corrupt("卷索引证据版本或长度"));
    }
    let identity = IndexSourceIdentity::from_bytes(&value[9..])
        .map_err(|_| CatalogError::Corrupt("卷索引来源身份或完整链"))?;
    validate_source(volume, scan, expected_index(&identity))
        .map_err(|_| CatalogError::Corrupt("卷索引证据身份或位置"))?;
    Ok((identity, read_u64(&value[1..9])))
}

pub(super) fn expected_index(identity: &IndexSourceIdentity) -> ExpectedIndex {
    let head = identity.head();
    ExpectedIndex {
        volume_uuid: head.volume_uuid,
        generation: head.generation,
        location: head.location,
        sha256: head.sha256,
    }
}

pub(super) async fn require_source(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<ExpectedIndex> {
    let identity = require_identity(tr, volume, scan).await?;
    if identity.head().incremental {
        return Err(CatalogError::Invalid("Building 导入要求原始 Full 来源"));
    }
    Ok(expected_index(&identity))
}

pub(super) async fn require_identity(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<IndexSourceIdentity> {
    let value = tr.get(&source_key(volume, scan.id), false).await?;
    decode_source(
        volume,
        scan,
        value.as_deref().ok_or(CatalogError::Incomplete)?,
    )
    .map(|source| source.0)
}

pub(super) async fn require_source_file_count(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<u64> {
    let raw = tr
        .get(&source_key(volume, scan.id), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    Ok(decode_source(volume, scan, &raw)?.1)
}

pub(super) async fn managed_source_ready(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<bool> {
    if !control::volume_is_managed(tr, volume).await? {
        return Ok(true);
    }
    match require_identity(tr, volume, scan).await {
        Ok(_) => Ok(true),
        Err(CatalogError::Incomplete) => Ok(false),
        Err(error) => Err(error),
    }
}

impl ManagedCatalog {
    /// Check one Building import source against the registered cartridge,
    /// ready scan and immutable index identity in a single FDB read version.
    /// This proves catalog identity, not current owner/PR execution authority.
    pub async fn verify_import_source_binding(&self, binding: &ImportSourceBinding) -> Result<()> {
        let p = &binding.placement;
        validate_source(p.volume_id, binding.scan, binding.source)?;
        let tr = self.db.create_trx()?;
        if tr
            .get(&control::managed_key(p.namespace), false)
            .await?
            .as_deref()
            != Some(&[][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let placement = control::verified_import_placement(&tr, p.volume_id)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement != *p {
            return Err(CatalogError::Incomplete);
        }
        require_volume_ready(&tr, p.volume_id, Some(binding.scan)).await?;
        let ready = tr
            .get(&ready_volume_key(p.volume_id), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let (_, serial) = decode_ready_volume(&ready)?;
        if serial != p.library_serial
            || require_source(&tr, p.volume_id, binding.scan).await? != binding.source
        {
            return Err(CatalogError::Incomplete);
        }
        Ok(())
    }

    /// Record an immutable index identity for the pending scan. Both local
    /// projections must come from the same recovery-confirmed full index and
    /// agree on every file before the source becomes durable in FDB. This does
    /// not itself operate the tape or prove the caller's owner/PR guard.
    pub async fn record_volume_source(
        &self,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        source: ExpectedIndex,
        local: LocalVolumeSources<'_>,
    ) -> Result<SourceOutcome> {
        validate_serial(serial)?;
        validate_source(volume, scan, source)?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("owner epoch 必须有效"));
        }
        let cache = VolumeExtentCache::open(local.extent_cache, source)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let spool = ImportSpool::open(local.import_spool, source)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        spool
            .verify_extent_cache(&cache)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let file_count = cache
            .file_count()
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let key = source_key(volume, scan.id);
        let identity = IndexSourceIdentity::new(
            CommittedIndexConstruct {
                volume_uuid: source.volume_uuid,
                generation: source.generation,
                location: source.location,
                sha256: source.sha256,
                incremental: false,
            },
            Vec::new(),
        )
        .map_err(|_| CatalogError::Invalid("Full 来源身份无效"))?;
        let value = encode_source(&identity, file_count);
        let pending = encode_ready_volume(scan, serial);
        self.db
            .run(|tr, _| {
                let key = key.clone();
                let value = value.clone();
                let pending = pending.clone();
                async move {
                    let owner = tr.get(&owner_key(serial), false).await?;
                    if owner.as_ref().is_some_and(|bytes| bytes.len() != 8) {
                        return Ok(Err(CatalogError::Corrupt("owner epoch 长度")));
                    }
                    if owner.as_ref().map(|bytes| read_u64(bytes)) != Some(epoch) {
                        return Ok(Ok(SourceOutcome::StaleOwner));
                    }
                    if tr
                        .get(&reconcile_volume_key(volume), false)
                        .await?
                        .as_deref()
                        != Some(pending.as_slice())
                    {
                        return Ok(Ok(SourceOutcome::NotReconciling));
                    }
                    let manifest = tr.get(&scan_key(volume, scan.id), false).await?;
                    let state = match manifest.as_deref().map(decode_scan_manifest).transpose() {
                        Ok(Some((saved, saved_serial, state)))
                            if saved == scan && saved_serial == serial =>
                        {
                            state
                        }
                        Ok(_) => return Ok(Err(CatalogError::Corrupt("卷索引证据扫描状态"))),
                        Err(error) => return Ok(Err(error)),
                    };
                    if let Some(old) = tr.get(&key, false).await? {
                        match decode_source(volume, scan, &old) {
                            Ok(_) => {}
                            Err(error) => return Ok(Err(error)),
                        };
                        return Ok(Ok(if old.as_ref() == value.as_slice() {
                            SourceOutcome::AlreadyRecorded
                        } else {
                            SourceOutcome::SourceConflict
                        }));
                    }
                    if state != ScanState::Building {
                        return Ok(Ok(SourceOutcome::NotReconciling));
                    }
                    tr.set(&key, &value);
                    Ok(Ok(SourceOutcome::Recorded))
                }
            })
            .await?
    }

    /// Return the source identity needed to open the local cache for a ready
    /// managed volume. A missing source fails closed in `require_volume_ready`.
    pub async fn volume_source(&self, volume: Uuid, scan: VolumeScan) -> Result<ExpectedIndex> {
        let tr = self.db.create_trx()?;
        require_volume_ready(&tr, volume, Some(scan)).await?;
        require_source(&tr, volume, scan).await
    }

    /// Open a rebuildable local extent cache only for the exact index identity
    /// that FDB has made ready. The caller still checks the requested file UID,
    /// path and length against its logical metadata before planning a tape read.
    pub async fn open_volume_cache(
        &self,
        volume: Uuid,
        scan: VolumeScan,
        path: &Path,
    ) -> Result<VolumeExtentCache> {
        let source = self.volume_source(volume, scan).await?;
        VolumeExtentCache::open(path, source)
            .map_err(|error| CatalogError::Cache(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::IndexLocation;

    #[test]
    fn source_codec_binds_location_and_rejects_bad_records() {
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 9,
            id: Uuid::new_v4(),
        };
        let source = ExpectedIndex {
            volume_uuid: volume,
            generation: 9,
            location: IndexLocation {
                partition: 'b',
                start_block: u64::MAX,
            },
            sha256: [7; 32],
        };
        let identity = IndexSourceIdentity::new(
            CommittedIndexConstruct {
                volume_uuid: source.volume_uuid,
                generation: source.generation,
                location: source.location,
                sha256: source.sha256,
                incremental: false,
            },
            Vec::new(),
        )
        .unwrap();
        let encoded = encode_source(&identity, 37);
        assert_eq!(
            decode_source(volume, scan, &encoded).unwrap(),
            (identity, 37)
        );
        assert!(decode_source(volume, scan, &[1; 50]).is_err());
        for end in 0..encoded.len() {
            assert!(decode_source(volume, scan, &encoded[..end]).is_err());
        }
        let mut invalid = encoded.clone();
        invalid[34] = b'z';
        assert!(decode_source(volume, scan, &invalid).is_err());
        invalid = encoded;
        invalid[35..43].fill(0);
        assert!(decode_source(volume, scan, &invalid).is_err());
        assert!(validate_source(Uuid::new_v4(), scan, source).is_err());
    }
}
