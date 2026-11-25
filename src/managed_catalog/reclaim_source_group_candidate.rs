//! Metadata capability from the actual current capture, not a byte-read proof.
use super::*;
use crate::managed_catalog::body_layouts::{PhysicalPlanProvenance, VerifiedPhysicalPlan};

#[derive(Clone, Debug)]
pub(crate) struct ReclaimGroupCandidate {
    parent: String,
    file: ReclaimFile,
    summary: Summary,
    object: Object,
    head: Vec<u8>,
    copy: Vec<u8>,
    known_sha256: [u8; 32],
    physical: [u8; 32],
    block_size: u32,
    first: Option<(char, u64, u32)>,
    end: Option<(char, u64, u32)>,
}
impl ReclaimGroupCandidate {
    pub(in crate::managed_catalog::control::reclaim) fn file(&self) -> &ReclaimFile {
        &self.file
    }
    pub(in crate::managed_catalog::control::reclaim) fn index(&self) -> &[u8] {
        &self.summary.head
    }
    pub(in crate::managed_catalog::control::reclaim) fn head(&self) -> &[u8] {
        &self.head
    }
    pub(in crate::managed_catalog::control::reclaim) fn copy_digest(&self) -> [u8; 32] {
        Sha256::digest(&self.copy).into()
    }
    pub(in crate::managed_catalog::control::reclaim) fn known_sha256(&self) -> [u8; 32] {
        self.known_sha256
    }
    pub(in crate::managed_catalog::control::reclaim) fn physical_digest(&self) -> [u8; 32] {
        self.physical
    }
    pub(in crate::managed_catalog::control::reclaim) fn inventory(&self) -> [u8; 32] {
        self.summary.digest
    }
    pub(in crate::managed_catalog::control::reclaim) fn node_digest(&self) -> [u8; 32] {
        self.object.sha256
    }
    pub(in crate::managed_catalog::control::reclaim) fn body_uid(&self) -> u64 {
        self.object.uid
    }
    pub(in crate::managed_catalog::control::reclaim) fn block_size(&self) -> u32 {
        self.block_size
    }
    pub(in crate::managed_catalog::control::reclaim) fn range(
        &self,
    ) -> (Option<(char, u64, u32)>, Option<(char, u64, u32)>) {
        (self.first, self.end)
    }
    pub(in crate::managed_catalog::control::reclaim) async fn require_at(
        &self,
        tr: &Transaction,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<()> {
        if self.parent != parent.fingerprint() {
            return Err(CatalogError::Invalid("当前正文候选属于其他原父任务"));
        }
        let record = record_at(tr, parent, owner).await?;
        if record["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        source_state_at(tr, parent).await?;
        let seal = ledger::require_complete_at(tr, parent).await?;
        if seal.summary != self.summary
            || tr
                .get(&ledger::object_key(parent, self.object.uid), false)
                .await?
                .as_deref()
                != Some(self.object.encode()?.as_slice())
        {
            return Err(CatalogError::Invalid("当前正文候选与完整源清单不同"));
        }
        let path = pool_catalog_path(parent.spec().pool(), self.file.path().as_bytes())?;
        let Some(ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            ..
        }) = lookup::resolve_active_path_at_mode(
            tr,
            parent.namespace(),
            lookup::path_components(&path)?,
            false,
        )
        .await?
        else {
            return Err(CatalogError::Invalid("原候选文件已不存在"));
        };
        if id != self.file.file_id()
            || encode_head(&head) != self.head
            || copy_records::encode_physical_copy(&copy)? != self.copy
            || placement.namespace != parent.namespace()
            || placement.library_serial != parent.serial()
            || placement.pool_id != parent.spec().pool()
            || placement.barcode != parent.spec().barcode()
            || copy.volume_id != parent.spec().volume()
        {
            return Err(CatalogError::Invalid("原候选文件、R、卷头或归属已改变"));
        }
        Ok(())
    }
}
impl ReclaimSourceInventory<'_> {
    /// No body bytes are read or claimed here. Unsupported current geometry is
    /// a single-file fallback before moving the original walk cursor.
    pub(crate) fn reclaim_group_candidate(
        &self,
        parent: &ReclaimReservation,
        file: &ReclaimFile,
        read: &ReadMetadata,
        physical: &VerifiedPhysicalPlan,
    ) -> Result<Option<ReclaimGroupCandidate>> {
        file.validate()?;
        let Some(directory) = self.source.full_history_hint() else {
            return Ok(None);
        };
        if directory.manifest().head != self.source.construct()
            || directory.manifest().dp_eod != self.source.construct_end()
            || directory.manifest().entries == 0
        {
            return Err(CatalogError::Corrupt("当前正文候选缺少完整可比较历史目录"));
        }
        let ContentDigest::Sha256(known_sha256) = read.copy.content_digest else {
            return Ok(None);
        };
        if read.copy.media.is_some() || physical.provenance() != PhysicalPlanProvenance::FdbLayout {
            return Ok(None);
        }
        if self.summary.volume != parent.spec().volume().to_string()
            || self.summary.head
                != serde_json::from_value::<Vec<u8>>(parent.intent()["index"].clone())
                    .map_err(cache)?
            || read.id != file.file_id()
            || read.head.version != file.version()
            || read.copy.version != read.head.version
            || read.head.size != file.size()
            || read.copy.size != file.size()
            || physical.length() != file.size()
            || read.copy.volume_id != parent.spec().volume()
            || read.placement.namespace != parent.namespace()
            || read.placement.library_serial != parent.serial()
            || read.placement.pool_id != parent.spec().pool()
            || read.placement.barcode != parent.spec().barcode()
        {
            return Err(CatalogError::Invalid("当前正文候选不属于原文件/源卷"));
        }
        let Some(entry) = self
            .source
            .entry_by_uid(read.copy.ltfs_file_uid)
            .map_err(cache)?
        else {
            return Ok(None);
        };
        let IndexEntry::File { path, node } = &entry else {
            return Ok(None);
        };
        if node.symlink.is_some()
            || node.length != file.size()
            || physical.path() != path
            || !physical.matches_extents(&node.extents).map_err(cache)?
        {
            return Ok(None);
        }
        if matches!(ContentDigest::from_ltfs_xattrs(&node.xattrs)?,ContentDigest::Sha256(value) if value!=known_sha256)
        {
            return Err(CatalogError::Invalid("已知SHA与当前正文节点冲突"));
        }
        read::expected_md5(node)?;
        let block_size = physical.block_size();
        if block_size == 0 {
            return Err(CatalogError::Corrupt("当前正文块大小无效"));
        }
        let partition = physical.partitions()[1];
        let mut logical = 0u64;
        let mut first = None;
        let mut end = None;
        let mut digest = Sha256::new().chain_update(b"tape-rs/reclaim-group-extents/1");
        for extent in &node.extents {
            if extent.partition != partition
                || extent.byte_count == 0
                || extent.byte_offset >= u64::from(block_size)
                || extent.file_offset != logical
            {
                return Ok(None);
            }
            let start = (partition, extent.start_block, extent.byte_offset as u32);
            if end.is_some_and(|previous| previous > start) {
                return Ok(None);
            }
            let span = extent
                .byte_offset
                .checked_add(extent.byte_count)
                .ok_or(CatalogError::Invalid("正文物理范围溢出"))?;
            let block = extent
                .start_block
                .checked_add(span / u64::from(block_size))
                .ok_or(CatalogError::Invalid("正文块范围溢出"))?;
            end = Some((partition, block, (span % u64::from(block_size)) as u32));
            first.get_or_insert(start);
            logical = logical
                .checked_add(extent.byte_count)
                .ok_or(CatalogError::Invalid("正文长度溢出"))?;
            digest.update(serde_json::to_vec(extent).map_err(cache)?);
        }
        if logical != file.size() {
            return Ok(None);
        }
        Ok(Some(ReclaimGroupCandidate {
            parent: parent.fingerprint(),
            file: file.clone(),
            summary: self.summary.clone(),
            object: Object::capture(entry)?,
            head: encode_head(&read.head),
            copy: copy_records::encode_physical_copy(&read.copy)?,
            known_sha256,
            physical: digest.finalize().into(),
            block_size,
            first,
            end,
        }))
    }
}
