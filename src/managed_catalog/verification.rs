//! Frozen scan verification uses bounded transactions and a persisted hash chain.
//! The expected summary must come from the independently sorted source scan.

use super::*;
use crate::catalog::volume_extents::VolumeExtentCache;
use sha2::{Digest, Sha256};
use std::path::Path;

const MAX_VERIFY_PAGE: usize = 100;
const DIGEST_DOMAIN: &[u8] = b"tape-rs/scan-summary/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    pub entries: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationOutcome {
    Frozen,
    Progress(ScanSummary),
    Verified(ScanSummary),
    Mismatch {
        expected: ScanSummary,
        actual: ScanSummary,
    },
    StaleOwner,
    NotVerifying,
    SourceMissing,
    CacheRequired,
    ExpectationConflict,
}

/// Feed source receipts in ascending `(namespace, file_id, copy_id)` byte order.
/// This is a metadata manifest digest, not a readback of the file contents.
pub struct ScanDigest {
    volume: Uuid,
    scan: VolumeScan,
    summary: ScanSummary,
    last: Option<[u8; 48]>,
}

impl ScanDigest {
    pub fn new(volume: Uuid, scan: VolumeScan) -> Result<Self> {
        validate_scan(volume, scan)?;
        let mut hash = Sha256::new();
        hash.update(DIGEST_DOMAIN);
        hash.update(volume.as_bytes());
        hash.update(scan.id.as_bytes());
        hash.update(scan.generation.to_be_bytes());
        Ok(Self {
            volume,
            scan,
            summary: ScanSummary {
                entries: 0,
                digest: hash.finalize().into(),
            },
            last: None,
        })
    }

    pub fn entry_key(receipt: &CommitReceipt) -> [u8; 48] {
        let mut out = [0; 48];
        out[..16].copy_from_slice(receipt.namespace.as_bytes());
        out[16..32].copy_from_slice(receipt.file_id.as_bytes());
        out[32..].copy_from_slice(receipt.copy_id.as_bytes());
        out
    }

    pub fn push(&mut self, receipt: &CommitReceipt) -> Result<()> {
        if receipt.volume_id != self.volume
            || receipt.volume_generation != self.scan.generation
            || receipt.volume_scan_id != self.scan.id
            || receipt.namespace.is_nil()
            || receipt.file_id.is_nil()
            || receipt.copy_id.is_nil()
        {
            return Err(CatalogError::Invalid("摘要条目不属于此卷扫描"));
        }
        validate_name(&receipt.name)?;
        validate_serial(&receipt.library_serial)?;
        let entry = Self::entry_key(receipt);
        if self.last.is_some_and(|last| last >= entry) {
            return Err(CatalogError::Invalid("扫描摘要必须有序且无重复条目"));
        }
        let version = receipt
            .expected_version
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(CatalogError::Invalid("文件版本溢出"))?;
        self.summary = fold(
            self.summary,
            &entry,
            &receipt_fingerprint(receipt),
            &encode_copy(receipt, version),
        )?;
        self.last = Some(entry);
        Ok(())
    }

    pub fn summary(&self) -> ScanSummary {
        self.summary
    }
}

#[derive(Clone)]
struct Progress {
    expected: ScanSummary,
    actual: ScanSummary,
    cursor: Option<[u8; 48]>,
}

pub(super) fn progress_key(volume: Uuid, scan: VolumeScan) -> Vec<u8> {
    let mut out = key(b'J');
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(scan.id.as_bytes());
    out
}

fn encode_progress(progress: &Progress) -> Vec<u8> {
    let mut out = Vec::with_capacity(129);
    for summary in [progress.expected, progress.actual] {
        out.extend_from_slice(&summary.entries.to_be_bytes());
        out.extend_from_slice(&summary.digest);
    }
    out.push(u8::from(progress.cursor.is_some()));
    if let Some(cursor) = progress.cursor {
        out.extend_from_slice(&cursor);
    }
    out
}

fn decode_progress(bytes: &[u8]) -> Result<Progress> {
    let cursor = match bytes.len() {
        81 if bytes[80] == 0 => None,
        129 if bytes[80] == 1 => Some(bytes[81..].try_into().expect("fixed length")),
        _ => return Err(CatalogError::Corrupt("扫描校验进度长度或游标")),
    };
    let actual = ScanSummary {
        entries: read_u64(&bytes[40..]),
        digest: bytes[48..80].try_into().expect("fixed length"),
    };
    if (actual.entries == 0) != cursor.is_none() {
        return Err(CatalogError::Corrupt("扫描校验计数与游标不一致"));
    }
    Ok(Progress {
        expected: ScanSummary {
            entries: read_u64(bytes),
            digest: bytes[8..40].try_into().expect("fixed length"),
        },
        actual,
        cursor,
    })
}

fn fold(
    previous: ScanSummary,
    entry: &[u8],
    fingerprint: &[u8],
    copy: &[u8],
) -> Result<ScanSummary> {
    let entries = previous
        .entries
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("扫描条目计数溢出"))?;
    let mut hash = Sha256::new();
    hash.update(DIGEST_DOMAIN);
    hash.update(previous.digest);
    hash.update(entry);
    hash.update(fingerprint);
    hash.update(copy);
    Ok(ScanSummary {
        entries,
        digest: hash.finalize().into(),
    })
}

fn check_cached_copy(cache: &VolumeExtentCache, copy: &PhysicalCopy) -> Result<()> {
    let indexed = cache
        .required_file_by_uid(copy.ltfs_file_uid)
        .map_err(|error| CatalogError::Cache(error.to_string()))?;
    if indexed.length != copy.size {
        return Err(CatalogError::Corrupt("扫描文件长度与物理索引不一致"));
    }
    if let Some(media) = &copy.media
        && (indexed.path != media.physical_path
            || media.native_symlink != indexed.symlink.is_some()
            || (media.native_symlink
                && (copy.size != 0 || copy.content_digest != super::ContentDigest::Unknown))
            || (media.native_symlink
                && indexed
                    .symlink
                    .as_ref()
                    .is_some_and(|target| target.contains(".LTFSEE_DATA/"))))
    {
        return Err(CatalogError::Corrupt("扫描文件路径与物理副本来源不一致"));
    }
    Ok(())
}

fn terminal(progress: &Progress, state: ScanState) -> VerificationOutcome {
    if state == ScanState::Verified {
        VerificationOutcome::Verified(progress.actual)
    } else {
        VerificationOutcome::Mismatch {
            expected: progress.expected,
            actual: progress.actual,
        }
    }
}

impl ManagedCatalog {
    /// Freeze the immutable candidate set and bind its source expectation.
    /// Retrying cannot replace the expectation or admit more imported entries.
    pub async fn freeze_volume_scan(
        &self,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        expected: ScanSummary,
    ) -> Result<VerificationOutcome> {
        validate_serial(serial)?;
        validate_scan(volume, scan)?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("owner epoch 必须有效"));
        }
        let initial = ScanDigest::new(volume, scan)?.summary();
        self.db
            .run(|tr, _| async move {
                let owner = tr.get(&owner_key(serial), false).await?;
                if owner.as_ref().is_some_and(|bytes| bytes.len() != 8) {
                    return Ok(Err(CatalogError::Corrupt("owner epoch 长度")));
                }
                if owner.as_ref().map(|bytes| read_u64(bytes)) != Some(epoch) {
                    return Ok(Ok(VerificationOutcome::StaleOwner));
                }
                let pending = tr.get(&reconcile_volume_key(volume), false).await?;
                if pending.as_deref() != Some(encode_ready_volume(scan, serial).as_slice()) {
                    return Ok(Ok(VerificationOutcome::NotVerifying));
                }
                let manifest_key = scan_key(volume, scan.id);
                let manifest = tr.get(&manifest_key, false).await?;
                let state = match manifest.as_deref().map(decode_scan_manifest).transpose() {
                    Ok(Some((saved, saved_serial, state)))
                        if saved == scan && saved_serial == serial =>
                    {
                        state
                    }
                    Ok(_) => return Ok(Err(CatalogError::Corrupt("扫描状态与候选不一致"))),
                    Err(error) => return Ok(Err(error)),
                };
                let progress_key = progress_key(volume, scan);
                if state == ScanState::Building {
                    let source_ready =
                        match index_evidence::managed_source_ready(&tr, volume, scan).await {
                            Ok(ready) => ready,
                            Err(CatalogError::Fdb(error)) => return Err(error),
                            Err(CatalogError::Client(error)) => return Err(error.into()),
                            Err(error) => return Ok(Err(error)),
                        };
                    if !source_ready {
                        return Ok(Ok(VerificationOutcome::SourceMissing));
                    }
                    let progress = Progress {
                        expected,
                        actual: initial,
                        cursor: None,
                    };
                    tr.set(&progress_key, &encode_progress(&progress));
                    tr.set(
                        &manifest_key,
                        &encode_scan_manifest(scan, serial, ScanState::Verifying),
                    );
                    return Ok(Ok(VerificationOutcome::Frozen));
                }
                if !matches!(
                    state,
                    ScanState::Verifying | ScanState::Verified | ScanState::Rejected
                ) {
                    return Ok(Ok(VerificationOutcome::NotVerifying));
                }
                let progress = tr.get(&progress_key, false).await?;
                let progress = match progress.as_deref().map(decode_progress).transpose() {
                    Ok(Some(progress)) => progress,
                    Ok(None) => return Ok(Err(CatalogError::Corrupt("扫描校验进度缺失"))),
                    Err(error) => return Ok(Err(error)),
                };
                if progress.expected != expected {
                    return Ok(Ok(VerificationOutcome::ExpectationConflict));
                }
                Ok(Ok(if state == ScanState::Verifying {
                    VerificationOutcome::Frozen
                } else {
                    terminal(&progress, state)
                }))
            })
            .await?
    }

    /// Verify up to 100 immutable entries per transaction. Progress commits
    /// atomically with its cursor, so retries and owner takeover cannot skip rows.
    pub async fn verify_volume_scan_page(
        &self,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        limit: usize,
    ) -> Result<VerificationOutcome> {
        self.verify_volume_scan_page_inner(serial, epoch, volume, scan, limit, None)
            .await
    }

    /// Managed scans must check every imported physical copy against a cache
    /// built from the independently confirmed full LTFS index.
    pub async fn verify_volume_scan_page_with_cache(
        &self,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        limit: usize,
        cache_path: &Path,
    ) -> Result<VerificationOutcome> {
        self.verify_volume_scan_page_inner(serial, epoch, volume, scan, limit, Some(cache_path))
            .await
    }

    async fn verify_volume_scan_page_inner(
        &self,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        limit: usize,
        cache_path: Option<&Path>,
    ) -> Result<VerificationOutcome> {
        validate_serial(serial)?;
        validate_scan(volume, scan)?;
        if epoch == 0 || !(1..=MAX_VERIFY_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid(
                "owner epoch 必须有效，校验批次必须在 1..=100",
            ));
        }
        self.db
            .run(|tr, _| async move {
                let owner = tr.get(&owner_key(serial), false).await?;
                if owner.as_ref().is_some_and(|bytes| bytes.len() != 8) {
                    return Ok(Err(CatalogError::Corrupt("owner epoch 长度")));
                }
                if owner.as_ref().map(|bytes| read_u64(bytes)) != Some(epoch) {
                    return Ok(Ok(VerificationOutcome::StaleOwner));
                }
                let pending = tr.get(&reconcile_volume_key(volume), false).await?;
                if pending.as_deref() != Some(encode_ready_volume(scan, serial).as_slice()) {
                    return Ok(Ok(VerificationOutcome::NotVerifying));
                }
                let manifest_key = scan_key(volume, scan.id);
                let manifest = tr.get(&manifest_key, false).await?;
                let state = match manifest.as_deref().map(decode_scan_manifest).transpose() {
                    Ok(Some((saved, saved_serial, state)))
                        if saved == scan && saved_serial == serial =>
                    {
                        state
                    }
                    Ok(_) => return Ok(Err(CatalogError::Corrupt("扫描状态与候选不一致"))),
                    Err(error) => return Ok(Err(error)),
                };
                if !matches!(
                    state,
                    ScanState::Verifying | ScanState::Verified | ScanState::Rejected
                ) {
                    return Ok(Ok(VerificationOutcome::NotVerifying));
                }
                let managed = match control::volume_is_managed(&tr, volume).await {
                    Ok(managed) => managed,
                    Err(CatalogError::Fdb(error)) => return Err(error),
                    Err(CatalogError::Client(error)) => return Err(error.into()),
                    Err(error) => return Ok(Err(error)),
                };
                let source = if managed {
                    match index_evidence::require_source(&tr, volume, scan).await {
                        Ok(source) => Some(source),
                        Err(CatalogError::Incomplete) => {
                            return Ok(Ok(VerificationOutcome::SourceMissing));
                        }
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    }
                } else {
                    None
                };
                if managed && cache_path.is_none() {
                    return Ok(Ok(VerificationOutcome::CacheRequired));
                }
                let progress_key = progress_key(volume, scan);
                let bytes = tr.get(&progress_key, false).await?;
                let mut progress = match bytes.as_deref().map(decode_progress).transpose() {
                    Ok(Some(progress)) => progress,
                    Ok(None) => return Ok(Err(CatalogError::Corrupt("扫描校验进度缺失"))),
                    Err(error) => return Ok(Err(error)),
                };
                if state != ScanState::Verifying {
                    return Ok(Ok(terminal(&progress, state)));
                }
                let prefix = volume_prefix(volume, scan.id);
                let cursor = progress.cursor.map(|entry| {
                    let mut cursor = prefix.clone();
                    cursor.extend_from_slice(&entry);
                    cursor
                });
                let page = read_page(&tr, &prefix, cursor.as_deref(), limit, false).await?;
                let mut lookups = Vec::with_capacity(page.items.len());
                for (key, value) in page.items {
                    let entry = &key[prefix.len()..];
                    if entry.len() != 48 || value.len() != 40 || read_u64(&value) == 0 {
                        return Ok(Err(CatalogError::Corrupt("扫描反向索引长度或版本")));
                    }
                    let mut copy_key = super::key(b'R');
                    copy_key.extend_from_slice(entry);
                    let entry: [u8; 48] = entry.try_into().expect("validated length");
                    // FDB's get submits the C request when creating the future.
                    // Issue this bounded page together, then fold in key order.
                    lookups.push((entry, value, tr.get(&copy_key, false)));
                }
                let cache = match (source, cache_path) {
                    (Some(source), Some(path)) => match VolumeExtentCache::open(path, source) {
                        Ok(cache) => Some(cache),
                        Err(error) => return Ok(Err(CatalogError::Cache(error.to_string()))),
                    },
                    _ => None,
                };
                for (entry, value, lookup) in lookups {
                    let copy = lookup.await?;
                    let Some(copy) = copy else {
                        return Ok(Err(CatalogError::Corrupt("扫描物理副本缺失")));
                    };
                    let physical = match decode_copy(&copy) {
                        Ok(copy) => copy,
                        Err(error) => return Ok(Err(error)),
                    };
                    if physical.version != read_u64(&value)
                        || physical.volume_id != volume
                        || physical.volume_generation != scan.generation
                        || physical.volume_scan_id != scan.id
                    {
                        return Ok(Err(CatalogError::Corrupt("扫描物理副本与索引不一致")));
                    }
                    if let Some(cache) = &cache
                        && let Err(error) = check_cached_copy(cache, &physical)
                    {
                        return Ok(Err(error));
                    }
                    progress.actual = match fold(progress.actual, &entry, &value[8..], &copy) {
                        Ok(summary) => summary,
                        Err(error) => return Ok(Err(error)),
                    };
                    progress.cursor = Some(entry);
                }
                tr.set(&progress_key, &encode_progress(&progress));
                if page.next.is_some() {
                    return Ok(Ok(VerificationOutcome::Progress(progress.actual)));
                }
                let state = if progress.expected == progress.actual {
                    ScanState::Verified
                } else {
                    ScanState::Rejected
                };
                tr.set(&manifest_key, &encode_scan_manifest(scan, serial, state));
                Ok(Ok(terminal(&progress, state)))
            })
            .await?
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn physical_copy_must_match_confirmed_index_uid_and_length() {
        use crate::catalog::volume_extents::ExpectedIndex;
        use crate::ltfs::index::{FileNode, LtfsIndex, NodeMeta};

        let volume = Uuid::new_v4();
        let mut index = LtfsIndex::empty(volume, "copy check".into(), 'b');
        index.self_location.start_block = 5;
        index.highest_file_uid = 77;
        index.root.files.push(FileNode {
            name: "file".into(),
            length: 3,
            meta: NodeMeta {
                file_uid: 77,
                ..Default::default()
            },
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let source = ExpectedIndex {
            volume_uuid: volume,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = std::env::temp_dir().join(format!("tape-rs-copy-check-{}.db", Uuid::new_v4()));
        VolumeExtentCache::build(xml.as_ref(), &path, source).unwrap();
        let cache = VolumeExtentCache::open(&path, source).unwrap();
        let mut copy = PhysicalCopy {
            version: 1,
            request_epoch: 1,
            volume_id: volume,
            volume_generation: index.generation,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 77,
            size: 3,
            content_digest: ContentDigest::Unknown,
            media: None,
        };
        check_cached_copy(&cache, &copy).unwrap();
        copy.size = 4;
        assert!(matches!(
            check_cached_copy(&cache, &copy),
            Err(CatalogError::Corrupt(_))
        ));
        copy.size = 3;
        copy.ltfs_file_uid = 78;
        assert!(matches!(
            check_cached_copy(&cache, &copy),
            Err(CatalogError::Cache(_))
        ));
        copy.ltfs_file_uid = 77;
        copy.media = Some(MediaProvenance {
            version: (0, 0),
            logical_file_uid: 77,
            physical_path: "other".into(),
            ee_alias: false,
            native_symlink: false,
        });
        assert!(matches!(
            check_cached_copy(&cache, &copy),
            Err(CatalogError::Corrupt(_))
        ));
        copy.media.as_mut().unwrap().physical_path = "file".into();
        check_cached_copy(&cache, &copy).unwrap();
        drop(cache);
        std::fs::remove_file(path).unwrap();
    }

    fn receipt(
        serial: &str,
        namespace: Uuid,
        volume: Uuid,
        scan: VolumeScan,
        n: u128,
    ) -> CommitReceipt {
        test_receipt(CommitReceipt {
            request_epoch: 1,
            namespace,
            parent: Uuid::nil(),
            name: format!("f{n:04}").into_bytes(),
            file_id: Uuid::from_u128(n),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::new_v4(),
            library_serial: serial.into(),
            pool_id: None,
            owner_epoch: 10,
            volume_id: volume,
            volume_generation: scan.generation,
            volume_scan_id: scan.id,
            ltfs_file_uid: 1,
            size: n as u64,
            content_digest: ContentDigest::Sha256([n as u8; 32]),
            metadata_digest: None,
        })
    }

    #[test]
    fn source_digest_rejects_duplicate_order_and_other_scan() {
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        let namespace = Uuid::new_v4();
        let first = receipt("serial", namespace, volume, scan, 1);
        let second = receipt("serial", namespace, volume, scan, 2);
        let mut digest = ScanDigest::new(volume, scan).unwrap();
        digest.push(&first).unwrap();
        let before = digest.summary();
        assert!(digest.push(&first).is_err());
        assert_eq!(digest.summary(), before);
        digest.push(&second).unwrap();
        assert!(digest.push(&first).is_err());
        let mut other = second.clone();
        other.volume_scan_id = Uuid::new_v4();
        assert!(digest.push(&other).is_err());
        let mut replay = first.clone();
        replay.owner_epoch += 1;
        replay.operation_id = Uuid::new_v4();
        let mut original = ScanDigest::new(volume, scan).unwrap();
        let mut recovered = ScanDigest::new(volume, scan).unwrap();
        original.push(&first).unwrap();
        recovered.push(&replay).unwrap();
        assert_eq!(original.summary(), recovered.summary());
        replay.name.push(b'x');
        let mut changed = ScanDigest::new(volume, scan).unwrap();
        changed.push(&replay).unwrap();
        assert_ne!(original.summary(), changed.summary());
    }

    #[test]
    fn progress_rejects_truncation_and_inconsistent_cursor() {
        let summary = ScanSummary {
            entries: 1,
            digest: [4; 32],
        };
        let progress = Progress {
            expected: summary,
            actual: summary,
            cursor: Some([2; 48]),
        };
        let bytes = encode_progress(&progress);
        assert_eq!(decode_progress(&bytes).unwrap().actual, summary);
        for end in 0..bytes.len() {
            assert!(decode_progress(&bytes[..end]).is_err());
        }
        let invalid = Progress {
            cursor: None,
            ..progress
        };
        assert!(decode_progress(&encode_progress(&invalid)).is_err());
    }

    pub(crate) async fn exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let serial = format!("verification-{}", Uuid::new_v4());
        let namespace = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 9,
            id: Uuid::new_v4(),
        };
        assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
        assert!(catalog.begin_import(namespace).await.unwrap());
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let mut digest = ScanDigest::new(volume, scan).unwrap();
        let mut first = None;
        for n in 1..=1001 {
            let entry = receipt(&serial, namespace, volume, scan, n);
            digest.push(&entry).unwrap();
            assert_eq!(
                test_publish_imported(catalog, &entry).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            if n == 1 {
                first = Some(entry);
            }
        }
        let expected = digest.summary();
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::NotVerified
        );
        assert_eq!(
            catalog
                .freeze_volume_scan(&serial, 9, volume, scan, expected)
                .await
                .unwrap(),
            VerificationOutcome::StaleOwner
        );
        let in_flight_import = catalog.db.create_trx().unwrap();
        assert!(
            in_flight_import
                .get(&scan_key(volume, scan.id), false)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog
                .freeze_volume_scan(&serial, 10, volume, scan, expected)
                .await
                .unwrap(),
            VerificationOutcome::Frozen
        );
        in_flight_import.set(
            &volume_key(volume, scan.id, namespace, Uuid::new_v4(), Uuid::new_v4()),
            &[0; 40],
        );
        assert!(in_flight_import.commit().await.is_err());
        let mut wrong = expected;
        wrong.entries += 1;
        assert_eq!(
            catalog
                .freeze_volume_scan(&serial, 10, volume, scan, wrong)
                .await
                .unwrap(),
            VerificationOutcome::ExpectationConflict
        );
        let mut late = first.unwrap();
        late.operation_id = Uuid::new_v4();
        late.copy_id = derive_copy_id(&late);
        assert_eq!(
            test_publish_imported(catalog, &late).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        assert!(matches!(
            catalog
                .verify_volume_scan_page(&serial, 10, volume, scan, 100)
                .await
                .unwrap(),
            VerificationOutcome::Progress(ScanSummary { entries: 100, .. })
        ));
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::NotVerified
        );
        assert!(catalog.advance_owner_epoch(&serial, 11).await.unwrap());
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        assert_eq!(
            reopened
                .verify_volume_scan_page(&serial, 10, volume, scan, 100)
                .await
                .unwrap(),
            VerificationOutcome::StaleOwner
        );
        assert_eq!(
            reopened
                .freeze_volume_scan(&serial, 11, volume, scan, expected)
                .await
                .unwrap(),
            VerificationOutcome::Frozen
        );
        let mut previous = 100;
        loop {
            match reopened
                .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                .await
                .unwrap()
            {
                VerificationOutcome::Progress(actual) => {
                    assert_eq!(actual.entries, previous + 100);
                    previous = actual.entries;
                }
                VerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, expected);
                    break;
                }
                unexpected => panic!("unexpected verification: {unexpected:?}"),
            }
        }
        assert_eq!(
            reopened
                .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                .await
                .unwrap(),
            VerificationOutcome::Verified(expected)
        );
        assert_eq!(
            reopened
                .freeze_volume_scan(&serial, 11, volume, scan, expected)
                .await
                .unwrap(),
            VerificationOutcome::Verified(expected)
        );
        assert_eq!(
            reopened
                .mark_volume_ready(&serial, 11, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert_eq!(
            catalog
                .copies_on_volume(volume, None, 1000)
                .await
                .unwrap()
                .items
                .len(),
            1000
        );

        for failure in 0..3 {
            let volume = Uuid::new_v4();
            let scan = VolumeScan {
                generation: 1,
                id: Uuid::new_v4(),
            };
            let namespace = Uuid::new_v4();
            assert!(catalog.begin_import(namespace).await.unwrap());
            assert_eq!(
                catalog
                    .begin_volume_reconcile(&serial, 11, volume, scan)
                    .await
                    .unwrap(),
                VolumeOutcome::Reconciling
            );
            let mut entry = receipt(&serial, namespace, volume, scan, 1);
            entry.owner_epoch = 11;
            assert_eq!(
                test_publish_imported(catalog, &entry).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            let mut digest = ScanDigest::new(volume, scan).unwrap();
            digest.push(&entry).unwrap();
            let actual = digest.summary();
            let mut expected = actual;
            if failure == 0 {
                expected.entries += 1;
            }
            if failure == 1 {
                expected.digest[0] ^= 1;
            }
            assert_eq!(
                catalog
                    .freeze_volume_scan(&serial, 11, volume, scan, expected)
                    .await
                    .unwrap(),
                VerificationOutcome::Frozen
            );
            if failure == 2 {
                catalog
                    .db
                    .run(|tr, _| {
                        let key = copy_key(namespace, entry.file_id, entry.copy_id);
                        async move {
                            tr.clear(&key);
                            Ok(())
                        }
                    })
                    .await
                    .unwrap();
                assert!(matches!(
                    catalog
                        .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                        .await,
                    Err(CatalogError::Corrupt(_))
                ));
            } else {
                let rejected = VerificationOutcome::Mismatch { expected, actual };
                assert_eq!(
                    catalog
                        .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                        .await
                        .unwrap(),
                    rejected
                );
                assert_eq!(
                    catalog
                        .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                        .await
                        .unwrap(),
                    rejected
                );
                assert_eq!(
                    catalog
                        .freeze_volume_scan(&serial, 11, volume, scan, expected)
                        .await
                        .unwrap(),
                    rejected
                );
            }
            assert_eq!(
                catalog
                    .mark_volume_ready(&serial, 11, volume, scan)
                    .await
                    .unwrap(),
                VolumeOutcome::NotVerified
            );
            assert!(matches!(
                catalog.copies_on_volume(volume, None, 10).await,
                Err(CatalogError::Incomplete)
            ));
            let in_flight = catalog.db.create_trx().unwrap();
            assert!(
                in_flight
                    .get(&scan_key(volume, scan.id), false)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                catalog
                    .cancel_volume_reconcile(&serial, 11, volume, scan)
                    .await
                    .unwrap(),
                VolumeOutcome::Cancelled
            );
            in_flight.set(&progress_key(volume, scan), &[0; 81]);
            assert!(in_flight.commit().await.is_err());
            assert_eq!(
                catalog
                    .verify_volume_scan_page(&serial, 11, volume, scan, 100)
                    .await
                    .unwrap(),
                VerificationOutcome::NotVerifying
            );
        }
    }
}
