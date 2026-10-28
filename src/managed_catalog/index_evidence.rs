//! Immutable on-media index identity for each managed volume scan.
//! The value is small and durable; physical extents remain in a rebuildable local cache.

use std::path::Path;

use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::ltfs::index::IndexLocation;

use super::*;

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

fn encode_source(source: ExpectedIndex) -> Vec<u8> {
    let mut value = Vec::with_capacity(42);
    value.push(1);
    value.push(source.location.partition as u8);
    value.extend_from_slice(&source.location.start_block.to_be_bytes());
    value.extend_from_slice(&source.sha256);
    value
}

pub(super) fn decode_source(volume: Uuid, scan: VolumeScan, value: &[u8]) -> Result<ExpectedIndex> {
    if value.len() != 42 || value[0] != 1 {
        return Err(CatalogError::Corrupt("卷索引证据版本或长度"));
    }
    let source = ExpectedIndex {
        volume_uuid: volume,
        generation: scan.generation,
        location: IndexLocation {
            partition: value[1] as char,
            start_block: read_u64(&value[2..10]),
        },
        sha256: value[10..42].try_into().expect("validated length"),
    };
    validate_source(volume, scan, source)
        .map_err(|_| CatalogError::Corrupt("卷索引证据身份或位置"))?;
    Ok(source)
}

pub(super) async fn require_source(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<ExpectedIndex> {
    let value = tr.get(&source_key(volume, scan.id), false).await?;
    decode_source(
        volume,
        scan,
        value.as_deref().ok_or(CatalogError::Incomplete)?,
    )
}

pub(super) async fn managed_source_ready(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
) -> Result<bool> {
    if !control::volume_is_managed(tr, volume).await? {
        return Ok(true);
    }
    match require_source(tr, volume, scan).await {
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
        let key = source_key(volume, scan.id);
        let value = encode_source(source);
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
                        let old = match decode_source(volume, scan, &old) {
                            Ok(old) => old,
                            Err(error) => return Ok(Err(error)),
                        };
                        return Ok(Ok(if old == source {
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
        let encoded = encode_source(source);
        assert_eq!(decode_source(volume, scan, &encoded).unwrap(), source);
        for end in 0..encoded.len() {
            assert!(decode_source(volume, scan, &encoded[..end]).is_err());
        }
        let mut invalid = encoded.clone();
        invalid[1] = b'z';
        assert!(decode_source(volume, scan, &invalid).is_err());
        invalid = encoded;
        invalid[2..10].fill(0);
        assert!(decode_source(volume, scan, &invalid).is_err());
        assert!(validate_source(Uuid::new_v4(), scan, source).is_err());
    }
}
