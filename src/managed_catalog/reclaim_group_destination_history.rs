//! Optional destination history addresses across reclaim tasks.
use super::*;
use crate::error::TapeError;
use crate::ltfs::recovery::{
    CapturedIndexSource, FULL_HISTORY_ENTRY_BYTES, FULL_HISTORY_MANIFEST_BYTES,
    FullHistoryDirectory, FullHistoryManifest, IndexScratchBudget,
};

// One replacement slot per physical cartridge. The namespace cleanup already
// owns FULL_HISTORY_HINT_KIND. Reformatting changes the matching scan and UUID.
fn key_for(parent: &ReclaimReservation, barcode: &str) -> Vec<u8> {
    let mut key = crate::managed_catalog::key(crate::managed_catalog::FULL_HISTORY_HINT_KIND);
    key.extend_from_slice(parent.namespace().as_bytes());
    key.push(b'g');
    crate::managed_catalog::append_component(&mut key, parent.serial().as_bytes());
    crate::managed_catalog::append_component(&mut key, barcode.as_bytes());
    key
}

const VERSION: &[u8; 3] = b"GH1";
const HEADER_BYTES: usize = VERSION.len() + 16 + 8 + 8;
const MAX_ENTRIES: u64 = 1024;

fn binding(scan: VolumeScan, revision: u64) -> [u8; HEADER_BYTES] {
    let mut bytes = [0; HEADER_BYTES];
    bytes[..3].copy_from_slice(VERSION);
    bytes[3..19].copy_from_slice(scan.id.as_bytes());
    bytes[19..27].copy_from_slice(&scan.generation.to_be_bytes());
    bytes[27..].copy_from_slice(&revision.to_be_bytes());
    bytes
}

fn directory_bytes(
    raw: &[u8],
    scan: VolumeScan,
    revision: u64,
    head: ActiveIndexIdentity,
) -> Option<&[u8]> {
    if raw.get(..HEADER_BYTES)? != binding(scan, revision) {
        return None;
    }
    let body = &raw[HEADER_BYTES..];
    let manifest =
        FullHistoryManifest::from_bytes(body.get(..FULL_HISTORY_MANIFEST_BYTES)?).ok()?;
    if manifest.entries > MAX_ENTRIES
        || body.len()
            != FULL_HISTORY_MANIFEST_BYTES + manifest.entries as usize * FULL_HISTORY_ENTRY_BYTES
        || ActiveIndexIdentity::from(manifest.head) != head
    {
        return None;
    }
    Some(body)
}

fn decode_hint(
    raw: &[u8],
    scan: VolumeScan,
    revision: u64,
    head: ActiveIndexIdentity,
    scratch: &IndexScratchBudget,
) -> Result<Option<FullHistoryDirectory>> {
    let Some(mut directory) = directory_bytes(raw, scan, revision, head) else {
        return Ok(None);
    };
    match FullHistoryDirectory::read_from(&mut directory, scratch) {
        Ok(hint) => Ok(Some(hint)),
        Err(
            TapeError::LocalCapacity { .. }
            | TapeError::LocalSpace { .. }
            | TapeError::RecoveryRestricted { .. }
            | TapeError::Ltfs(_),
        ) => Ok(None),
        Err(error) => Err(cache(error)),
    }
}

impl ManagedCatalog {
    /// Read the current destination identity and the advisory slot at one
    /// read version. The caller's per-CDB read/PR guard grants media access.
    pub(crate) async fn load_reclaim_group_destination_history_hint(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        expected_layout: &ReclaimLayout,
        scratch: &IndexScratchBudget,
    ) -> Result<Option<FullHistoryDirectory>> {
        if !parent.spec().group_only()
            || scratch.max_bytes
                < CapturedIndexSource::RECLAIM_PREFLIGHT_WITH_HINT_SCRATCH_MIN_BYTES
        {
            return Ok(None);
        }
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, parent.serial(), parent.route, owner).await?;
        parent_at(&tr, parent, owner).await?;
        if layout::at(&tr, parent).await?.as_ref() != Some(expected_layout) {
            return Err(CatalogError::Invalid("回收预检位置提示的原固定布局改变"));
        }
        let target = &expected_layout.destination;
        let assignment = Self::verified_assignment(&tr, parent.serial(), &target.barcode)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if assignment.namespace != parent.namespace()
            || assignment.pool_id != parent.spec().pool()
            || assignment.volume_id != Some(target.volume)
            || assignment.revision != target.revision
            || !assignment.allows_io()
        {
            return Err(CatalogError::MediaUnavailable);
        }
        let (scan, head) = active_volume::observed_at(&tr, parent.serial(), target.volume).await?;
        let key = key_for(parent, &target.barcode);
        let Some(raw) = tr.get(&key, false).await? else {
            return Ok(None);
        };
        drop(tr);
        decode_hint(&raw, scan, target.revision, head, scratch)
    }

    /// Save one complete actual schedule after the original active batch was
    /// published. A conflict or unknown result affects only this hint.
    pub(crate) async fn save_reclaim_group_destination_history_hint(
        &self,
        scope: &ReclaimGroupObservation,
        capture: &CapturedIndexSource,
    ) -> Result<()> {
        let parent = scope.parent();
        let Some(directory) = capture.full_history_hint() else {
            return Ok(());
        };
        let manifest = directory.manifest();
        if !parent.spec().group_only()
            || manifest.entries > MAX_ENTRIES
            || manifest.head != capture.construct()
        {
            return Ok(());
        }
        let (target, original_scan) = (
            &scope.group().header.destination,
            &scope.group().header.scan,
        );
        let tr = self.db.create_trx()?;
        scope.require_at(&tr).await?;
        let (scan, head) = active_volume::observed_at(&tr, parent.serial(), target.volume).await?;
        if scan.id.to_string() != *original_scan || ActiveIndexIdentity::from(manifest.head) != head
        {
            return Err(CatalogError::Invalid(
                "回收位置提示的实读卷头不再是目标活动卷头",
            ));
        }
        let mut value = binding(scan, target.revision).to_vec();
        directory.write_to(&mut value).map_err(cache)?;
        tr.set(&key_for(parent, &target.barcode), &value);
        tr.commit().await.map_err(|error| {
            CatalogError::Cache(format!("回收目标位置提示提交未确认: {error:?}"))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::IndexLocation;
    use crate::ltfs::recovery::FullHistoryEntry;
    use crate::ltfs::volume::CommittedIndexConstruct;

    #[test]
    fn group_destination_history_decodes_only_current_exact_bounded_schedule() {
        let scan = VolumeScan {
            id: Uuid::new_v4(),
            generation: 1,
        };
        let revision = 7;
        let entry = FullHistoryEntry {
            generation: 1,
            start_block: 5,
            end_block: 7,
            sha256: [3; 32],
            previous: None,
        };
        let manifest = FullHistoryManifest {
            head: CommittedIndexConstruct {
                volume_uuid: Uuid::new_v4(),
                generation: entry.generation,
                location: IndexLocation {
                    start_block: entry.start_block,
                    partition: 'b',
                },
                sha256: entry.sha256,
                incremental: false,
            },
            dp_eod: entry.end_block,
            entries: 1,
            block_size: 64 << 10,
            index_partition: 'a',
            data_partition: 'b',
            entries_sha256: Sha256::digest(entry.to_bytes()).into(),
        };
        let mut value = binding(scan, revision).to_vec();
        value.extend_from_slice(&manifest.to_bytes());
        value.extend_from_slice(&entry.to_bytes());
        let mut scratch = IndexScratchBudget {
            headroom_bytes: 0,
            ..Default::default()
        };
        let head = ActiveIndexIdentity::from(manifest.head);
        assert_eq!(
            decode_hint(&value, scan, revision, head, &scratch)
                .unwrap()
                .unwrap()
                .manifest(),
            manifest
        );
        assert!(directory_bytes(&[], scan, revision, head).is_none());
        let new_scan = VolumeScan {
            id: Uuid::new_v4(),
            ..scan
        };
        assert!(directory_bytes(&value, new_scan, revision, head).is_none());
        assert!(directory_bytes(&value, scan, revision + 1, head).is_none());
        let new_head = ActiveIndexIdentity {
            sha256: [7; 32],
            ..head
        };
        assert!(directory_bytes(&value, scan, revision, new_head).is_none());
        scratch.max_bytes = 0;
        assert!(
            decode_hint(&value, scan, revision, head, &scratch)
                .unwrap()
                .is_none()
        );
        scratch.max_bytes = 128 << 20;
        value[HEADER_BYTES + FULL_HISTORY_MANIFEST_BYTES + 16] ^= 1;
        assert!(
            decode_hint(&value, scan, revision, head, &scratch)
                .unwrap()
                .is_none()
        );
    }
}
