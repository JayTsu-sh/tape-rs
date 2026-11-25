//! Immutable, namespace-owned physical file layouts.
//!
//! These objects describe a committed body. They do not publish a copy, prove
//! the current medium, grant device access, or authorize source initialization.
//! The active batch publication transaction owns visibility of their bindings.
//! Objects remain immutable for the namespace lifetime; namespace cleanup must
//! register BODY_LAYOUT_KIND before this module is connected to a producer.

use super::*;
use crate::ltfs::index::Extent;
use crate::ltfs::recovery::IndexSourceIdentity;
use crate::ltfs::volume::LtfsVolume;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::sync::Arc;

#[cfg(test)]
mod attach_test_fault;
mod backfill;
mod import_source;
mod object_refs;
#[cfg(test)]
pub(in crate::managed_catalog) use attach_test_fault::{AttachTestPoint, arm_attach_test_fault};
pub(in crate::managed_catalog) use backfill::CopyReadSnapshot;
pub(crate) use backfill::VerifiedExistingCopyLayout;
mod retained;
pub(in crate::managed_catalog) use retained::{
    BindingLocation, clear_retained_child_at, retained_binding_at,
};
mod read_plan;
pub use read_plan::{PhysicalPlanProvenance, PlanExtents, VerifiedPhysicalPlan};

const CHUNK_BYTES: usize = 32_768;
const CHUNKS_PER_TRANSACTION: usize = 4;
const LAYOUT_HEADER: usize = 45;
const EXTENT_BYTES: usize = 33;
const MANIFEST_BYTES: usize = 47;
const LAYOUT_DOMAIN: &[u8] = b"tape-rs/body-layout/1";
// Subtypes of BODY_LAYOUT_KIND (0x80), independent of management/R/0x82.
// 0..4 retain their original object/binding/batch/scoped/LC1 encoding.
const IMPORT_STAGE_SUBTYPE: u8 = 5;
const OBJECT_REFS_SUBTYPE: u8 = 6;
// Reserved by the authority design: 7 = policy; 8 = migration/coverage.
// Neither policy activation nor object GC is enabled by this producer slice.
// ReadStorageBudget::index_bytes is a disk budget, not a heap admission. Keep
// this compatibility decoder bounded. Managed reads use the paged fd plan.
pub(super) const MAX_READ_LAYOUT_BYTES: u64 = 8 << 20;

fn projection_size(extents: usize) -> Option<u32> {
    extents
        .checked_mul(EXTENT_BYTES)
        .and_then(|len| len.checked_add(LAYOUT_HEADER))
        .and_then(|len| u32::try_from(len).ok())
}

fn validate_layout_extent(
    extent: &Extent,
    block_size: u32,
    partitions: [char; 2],
    offset: &mut u64,
) -> Result<()> {
    if !partitions.contains(&extent.partition) || extent.file_offset != *offset {
        return Err(CatalogError::Invalid(
            "正文布局 extent 分区、顺序或文件偏移无效",
        ));
    }
    *offset = offset
        .checked_add(extent.byte_count)
        .ok_or(CatalogError::Invalid("正文布局文件范围溢出"))?;
    let physical_bytes = extent
        .byte_offset
        .checked_add(extent.byte_count)
        .ok_or(CatalogError::Invalid("正文布局 extent 字节范围溢出"))?;
    if extent.byte_count > 0 && extent.byte_offset >= u64::from(block_size) {
        return Err(CatalogError::Invalid("正文布局块内偏移超出块大小"));
    }
    if extent.byte_count > 0 {
        extent
            .start_block
            .checked_add((physical_bytes - 1) / u64::from(block_size))
            .ok_or(CatalogError::Invalid("正文布局 extent 块范围溢出"))?;
    }
    Ok(())
}

/// The immutable physical description, independent of a file name or copy ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BodyLayout<'layout> {
    volume: Uuid,
    uid: u64,
    length: u64,
    block_size: u32,
    partitions: [char; 2],
    extents: Cow<'layout, [Extent]>,
}

impl<'layout> BodyLayout<'layout> {
    fn new(
        volume: Uuid,
        uid: u64,
        length: u64,
        block_size: u32,
        partitions: [char; 2],
        extents: Vec<Extent>,
    ) -> Result<Self> {
        let layout = Self {
            volume,
            uid,
            length,
            block_size,
            partitions,
            extents: extents.into(),
        };
        layout.validate()?;
        Ok(layout)
    }

    // This view borrows the PR-verified committed index across every await.
    // No additional whole extent array is allocated by a production producer.
    fn borrowed(
        volume: Uuid,
        uid: u64,
        length: u64,
        block_size: u32,
        partitions: [char; 2],
        extents: &'layout [Extent],
    ) -> Result<Self> {
        let layout = Self {
            volume,
            uid,
            length,
            block_size,
            partitions,
            extents: Cow::Borrowed(extents),
        };
        layout.validate()?;
        Ok(layout)
    }

    fn validate(&self) -> Result<()> {
        if self.volume.is_nil()
            || self.uid == 0
            || self.block_size == 0
            || self.partitions[0] == self.partitions[1]
            || !self.partitions.iter().all(|part| matches!(part, 'a' | 'b'))
        {
            return Err(CatalogError::Invalid("正文布局卷、UID、块大小或分区无效"));
        }
        let mut offset = 0u64;
        for extent in self.extents.iter() {
            validate_layout_extent(extent, self.block_size, self.partitions, &mut offset)?;
        }
        if offset != self.length {
            return Err(CatalogError::Invalid("正文布局 extent 总长度不符"));
        }
        self.encoded_len()?;
        Ok(())
    }

    fn encoded_len(&self) -> Result<usize> {
        let bytes = self
            .extents
            .len()
            .checked_mul(EXTENT_BYTES)
            .and_then(|len| len.checked_add(LAYOUT_HEADER))
            .ok_or(CatalogError::Invalid("正文布局编码长度溢出"))?;
        u32::try_from(bytes).map_err(|_| CatalogError::Invalid("正文布局超出编码长度范围"))?;
        Ok(bytes)
    }

    fn header(&self) -> [u8; LAYOUT_HEADER] {
        let mut out = [0; LAYOUT_HEADER];
        out[..3].copy_from_slice(b"BL1");
        out[3..19].copy_from_slice(self.volume.as_bytes());
        out[19..27].copy_from_slice(&self.uid.to_be_bytes());
        out[27..35].copy_from_slice(&self.length.to_be_bytes());
        out[35..39].copy_from_slice(&self.block_size.to_be_bytes());
        out[39] = self.partitions[0] as u8;
        out[40] = self.partitions[1] as u8;
        out[41..45].copy_from_slice(&(self.extents.len() as u32).to_be_bytes());
        out
    }

    fn extent_bytes(extent: &Extent) -> [u8; EXTENT_BYTES] {
        let mut out = [0; EXTENT_BYTES];
        out[0] = extent.partition as u8;
        for (index, value) in [
            extent.start_block,
            extent.byte_offset,
            extent.byte_count,
            extent.file_offset,
        ]
        .into_iter()
        .enumerate()
        {
            out[1 + index * 8..9 + index * 8].copy_from_slice(&value.to_be_bytes());
        }
        out
    }

    pub(crate) fn manifest(&self) -> Result<LayoutManifest> {
        self.validate()?;
        let len = self.encoded_len()?;
        let mut hash = Sha256::new();
        hash.update(LAYOUT_DOMAIN);
        hash.update(self.header());
        for extent in self.extents.iter() {
            hash.update(Self::extent_bytes(extent));
        }
        Ok(LayoutManifest {
            digest: hash.finalize().into(),
            len: len as u32,
            extents: self.extents.len() as u32,
            chunks: len.div_ceil(CHUNK_BYTES) as u32,
        })
    }

    /// Encode only one fixed-size chunk, without duplicating the whole layout.
    fn chunk(&self, manifest: LayoutManifest, index: u32) -> Result<Vec<u8>> {
        manifest.validate()?;
        if index >= manifest.chunks || self.encoded_len()? != manifest.len as usize {
            return Err(CatalogError::Invalid("正文布局分片位置无效"));
        }
        let mut position = index as usize * CHUNK_BYTES;
        let end = position
            .saturating_add(CHUNK_BYTES)
            .min(manifest.len as usize);
        let mut out = Vec::with_capacity(end - position);
        if position < LAYOUT_HEADER {
            let next = end.min(LAYOUT_HEADER);
            out.extend_from_slice(&self.header()[position..next]);
            position = next;
        }
        while position < end {
            let relative = position - LAYOUT_HEADER;
            let extent = relative / EXTENT_BYTES;
            let offset = relative % EXTENT_BYTES;
            let bytes = Self::extent_bytes(&self.extents[extent]);
            let count = (EXTENT_BYTES - offset).min(end - position);
            out.extend_from_slice(&bytes[offset..offset + count]);
            position += count;
        }
        Ok(out)
    }

    pub(crate) fn volume(&self) -> Uuid {
        self.volume
    }

    pub(crate) fn uid(&self) -> u64 {
        self.uid
    }

    pub(crate) fn length(&self) -> u64 {
        self.length
    }

    pub(crate) fn block_size(&self) -> u32 {
        self.block_size
    }

    pub(crate) fn partitions(&self) -> [char; 2] {
        self.partitions
    }

    pub(crate) fn extents(&self) -> &[Extent] {
        &self.extents
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayoutManifest {
    digest: [u8; 32],
    len: u32,
    extents: u32,
    chunks: u32,
}

impl LayoutManifest {
    fn validate(self) -> Result<()> {
        let expected = u64::from(self.extents)
            .checked_mul(EXTENT_BYTES as u64)
            .and_then(|len| len.checked_add(LAYOUT_HEADER as u64));
        if expected != Some(u64::from(self.len))
            || self.chunks != (self.len as usize).div_ceil(CHUNK_BYTES) as u32
        {
            return Err(CatalogError::Corrupt("正文布局分片清单长度或数量"));
        }
        Ok(())
    }

    fn encode(self) -> [u8; MANIFEST_BYTES] {
        let mut out = [0; MANIFEST_BYTES];
        out[..3].copy_from_slice(b"LM1");
        out[3..35].copy_from_slice(&self.digest);
        out[35..39].copy_from_slice(&self.len.to_be_bytes());
        out[39..43].copy_from_slice(&self.extents.to_be_bytes());
        out[43..47].copy_from_slice(&self.chunks.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != MANIFEST_BYTES || &bytes[..3] != b"LM1" {
            return Err(CatalogError::Corrupt("正文布局分片清单编码"));
        }
        let manifest = Self {
            digest: bytes[3..35].try_into().expect("checked manifest length"),
            len: u32::from_be_bytes(bytes[35..39].try_into().expect("checked manifest length")),
            extents: u32::from_be_bytes(bytes[39..43].try_into().expect("checked manifest length")),
            chunks: u32::from_be_bytes(bytes[43..47].try_into().expect("checked manifest length")),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Only a verified committed LTFS view can mint new physical layout evidence.
pub(crate) struct CommittedLayoutSource<'view, 'device> {
    volume: &'view LtfsVolume<'device>,
    source: ActiveIndexIdentity,
}

impl<'view, 'device> CommittedLayoutSource<'view, 'device> {
    pub(crate) fn new(volume: &'view mut LtfsVolume<'device>) -> Result<Self> {
        let source = volume
            .verified_committed_index_construct()
            .map_err(|error| CatalogError::Cache(error.to_string()))?
            .into();
        Ok(Self { volume, source })
    }

    /// Size preflight only; this never mints file evidence or publishes a copy.
    /// A new optional batch constraint is declared only if every file fits.
    pub(crate) fn projection_supported(&self, path: &str) -> Result<bool> {
        let file = self
            .volume
            .index()
            .find_file(path)
            .ok_or(CatalogError::Invalid("正文布局尺寸预检缺少已提交文件"))?;
        Ok(projection_size(file.extents.len()).is_some())
    }

    #[cfg(test)]
    fn file(
        &self,
        intent: &ActiveFileIntent,
        receipt: &CommitReceipt,
    ) -> Result<Option<VerifiedCommittedBodyLayout<'_>>> {
        validate_receipt(receipt)?;
        let path = intent.ltfs_path()?;
        let prepared = prepare_committed_active_file(self.volume, &path, intent)?;
        if receipt_fingerprint(&prepared.receipt) != receipt_fingerprint(receipt) {
            return Err(CatalogError::Invalid(
                "已提交正文布局与原收据或索引身份不符",
            ));
        }
        self.prepared(&path, &prepared)
    }

    pub(crate) fn prepared(
        &self,
        path: &str,
        prepared: &PreparedActiveFile,
    ) -> Result<Option<VerifiedCommittedBodyLayout<'_>>> {
        let receipt = &prepared.receipt;
        validate_receipt(receipt)?;
        if receipt.copy_id != derive_copy_id(receipt)
            || self.source.volume_uuid != receipt.volume_id
            || self.source.generation != receipt.volume_generation
            || path.rsplit('/').next().map(str::as_bytes) != Some(receipt.name.as_slice())
        {
            return Err(CatalogError::Invalid(
                "已提交正文布局与原收据或索引身份不符",
            ));
        }
        let file = self
            .volume
            .index()
            .find_file(path)
            .ok_or(CatalogError::Invalid("已提交正文布局缺少原文件"))?;
        let attributes = NodeAttributes::from_file(file)?;
        if file.symlink.is_some()
            || file.meta.file_uid != receipt.ltfs_file_uid
            || file.length != receipt.size
            || ContentDigest::from_ltfs_xattrs(&file.xattrs)? != receipt.content_digest
            || receipt.metadata_digest != Some(attributes.digest())
            || prepared.attributes.digest() != attributes.digest()
        {
            return Err(CatalogError::Invalid(
                "已提交正文布局的 UID、内容或属性不符",
            ));
        }
        if !self.projection_supported(path)? {
            return Ok(None);
        }
        let label = self.volume.label();
        let layout = BodyLayout::borrowed(
            label.volume_uuid,
            file.meta.file_uid,
            file.length,
            label.blocksize,
            [label.index_partition, label.data_partition],
            &file.extents,
        )?;
        Ok(Some(VerifiedCommittedBodyLayout {
            receipt: receipt.clone(),
            source: self.source,
            path: path.to_owned(),
            layout,
        }))
    }
}

pub(crate) struct VerifiedCommittedBodyLayout<'view> {
    receipt: CommitReceipt,
    source: ActiveIndexIdentity,
    path: String,
    layout: BodyLayout<'view>,
}

const MAX_LB2_BINDING_BYTES: usize = 90_000;
const MAX_SOURCE_IDENTITY_BYTES: usize = 68 + 66 * 64;
const LB2_FIXED_BYTES: usize = 306;
const COPY_RECORD_DOMAIN: &[u8] = b"tape-rs/body-layout-copy-record/1";

fn copy_record_digest(raw: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(COPY_RECORD_DOMAIN);
    hash.update(raw);
    hash.finalize().into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LayoutProvenance {
    CommittedReceipt {
        receipt: [u8; 32],
        request_epoch: u64,
        batch: Uuid,
    },
    VerifiedCopySource {
        copy_record_sha: [u8; 32],
        source_identity_sha: [u8; 32],
        source_identity: IndexSourceIdentity,
    },
}

/// A staged binding is private until the original catalog copy becomes visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CopyLayoutBinding {
    namespace: Uuid,
    file: Uuid,
    copy: Uuid,
    version: u64,
    scan: Uuid,
    body_uid: u64,
    length: u64,
    content_digest: ContentDigest,
    source: ActiveIndexIdentity,
    provenance: LayoutProvenance,
    path: String,
    manifest: LayoutManifest,
}

impl CopyLayoutBinding {
    pub(crate) fn fingerprint(&self) -> Result<[u8; 32]> {
        Ok(Sha256::digest(self.encode()?).into())
    }

    pub(crate) fn encoded_bytes(&self) -> u64 {
        u64::from(self.manifest.len)
    }

    fn from_verified(batch: Uuid, verified: &VerifiedCommittedBodyLayout<'_>) -> Result<Self> {
        let receipt = &verified.receipt;
        let binding = Self {
            namespace: receipt.namespace,
            file: receipt.file_id,
            copy: receipt.copy_id,
            version: receipt
                .expected_version
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(CatalogError::Invalid("正文布局副本版本溢出"))?,
            scan: receipt.volume_scan_id,
            body_uid: receipt.ltfs_file_uid,
            length: receipt.size,
            content_digest: receipt.content_digest,
            source: verified.source,
            provenance: LayoutProvenance::CommittedReceipt {
                receipt: receipt_fingerprint(receipt).try_into().expect("SHA-256"),
                request_epoch: receipt.request_epoch,
                batch,
            },
            path: verified.path.clone(),
            manifest: verified.layout.manifest()?,
        };
        binding.validate()?;
        Ok(binding)
    }

    fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        if self.namespace.is_nil()
            || self.file.is_nil()
            || self.copy.is_nil()
            || self.scan.is_nil()
            || self.version == 0
            || self.body_uid == 0
            || self.source.volume_uuid.is_nil()
            || self.source.generation == 0
            || !matches!(self.source.location.partition, 'a' | 'b')
            || self.source.location.start_block == 0
            || !self.path.starts_with('/')
            || self.path.as_bytes().contains(&0)
            || self.path.len() > u16::MAX as usize
        {
            return Err(CatalogError::Corrupt("正文布局绑定身份或路径"));
        }
        match &self.provenance {
            LayoutProvenance::CommittedReceipt {
                request_epoch,
                batch,
                ..
            } => {
                if *request_epoch == 0 || batch.is_nil() {
                    return Err(CatalogError::Corrupt("正文布局原收据身份"));
                }
            }
            LayoutProvenance::VerifiedCopySource {
                source_identity,
                source_identity_sha,
                ..
            } => {
                self.lb2_encoded_len(source_identity)?;
                if self.path == "/" || self.path.starts_with("//") {
                    return Err(CatalogError::Corrupt("LB2 正文物理路径不规范"));
                }
                if ActiveIndexIdentity::from(source_identity.head()) != self.source
                    || source_identity.fingerprint() != *source_identity_sha
                {
                    return Err(CatalogError::Corrupt("正文布局完整来源身份或摘要不同"));
                }
            }
        }
        Ok(())
    }

    fn lb2_encoded_len(&self, identity: &IndexSourceIdentity) -> Result<usize> {
        let identity_bytes = identity
            .predecessors()
            .len()
            .checked_mul(66)
            .and_then(|n| n.checked_add(68))
            .filter(|n| (68..=MAX_SOURCE_IDENTITY_BYTES).contains(n))
            .ok_or(CatalogError::Corrupt("正文布局完整来源编码长度"))?;
        LB2_FIXED_BYTES
            .checked_add(self.path.len())
            .and_then(|n| n.checked_add(identity_bytes))
            .filter(|n| *n <= MAX_LB2_BINDING_BYTES)
            .ok_or(CatalogError::Corrupt("LB2 绑定超过编码上限"))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = match &self.provenance {
            LayoutProvenance::CommittedReceipt { .. } => b"LB1".to_vec(),
            LayoutProvenance::VerifiedCopySource {
                source_identity, ..
            } => {
                let len = self.lb2_encoded_len(source_identity)?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(len)
                    .map_err(|_| CatalogError::Invalid("LB2 绑定内存不足"))?;
                bytes.extend_from_slice(b"LB2");
                bytes
            }
        };
        for id in [self.namespace, self.file, self.copy] {
            out.extend_from_slice(id.as_bytes());
        }
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(self.scan.as_bytes());
        out.extend_from_slice(self.source.volume_uuid.as_bytes());
        active_volume::encode_index(&mut out, self.source);
        out.extend_from_slice(&self.body_uid.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
        out.push(self.content_digest.tag());
        out.extend_from_slice(&self.content_digest.payload());
        match &self.provenance {
            LayoutProvenance::CommittedReceipt {
                receipt,
                request_epoch,
                batch,
            } => {
                out.extend_from_slice(receipt);
                out.extend_from_slice(&request_epoch.to_be_bytes());
                out.extend_from_slice(batch.as_bytes());
            }
            LayoutProvenance::VerifiedCopySource {
                copy_record_sha,
                source_identity_sha,
                source_identity,
            } => {
                out.push(1);
                out.extend_from_slice(copy_record_sha);
                out.extend_from_slice(source_identity_sha);
                let identity = source_identity.to_bytes();
                out.extend_from_slice(&(identity.len() as u16).to_be_bytes());
                out.extend_from_slice(&identity);
            }
        }
        out.extend_from_slice(&(self.path.len() as u16).to_be_bytes());
        out.extend_from_slice(self.path.as_bytes());
        out.extend_from_slice(&self.manifest.encode());
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = BindingReader { bytes, at: 0 };
        let version = reader.take(3)?;
        let lb2 = match version {
            b"LB1" => false,
            b"LB2" => true,
            _ => return Err(CatalogError::Corrupt("正文布局绑定版本")),
        };
        if lb2 && bytes.len() > MAX_LB2_BINDING_BYTES {
            return Err(CatalogError::Corrupt("LB2 绑定超过编码上限"));
        }
        let namespace = reader.uuid()?;
        let file = reader.uuid()?;
        let copy = reader.uuid()?;
        let version = reader.u64()?;
        let scan = reader.uuid()?;
        let volume = reader.uuid()?;
        let source = active_volume::decode_index(volume, reader.take(50)?)?;
        let body_uid = reader.u64()?;
        let length = reader.u64()?;
        let tag = reader.fixed::<1>()?[0];
        let content_digest = ContentDigest::decode(tag, reader.take(32)?)?;
        let provenance = if lb2 {
            if reader.fixed::<1>()? != [1] {
                return Err(CatalogError::Corrupt("LB2 来源种类"));
            }
            let copy_record_sha = reader.fixed()?;
            let source_identity_sha = reader.fixed()?;
            let len = usize::from(u16::from_be_bytes(reader.fixed()?));
            if !(68..=MAX_SOURCE_IDENTITY_BYTES).contains(&len) {
                return Err(CatalogError::Corrupt("LB2 完整来源长度"));
            }
            let raw = reader.take(len)?;
            let source_identity = IndexSourceIdentity::from_bytes(raw)
                .map_err(|_| CatalogError::Corrupt("LB2 完整来源格式"))?;
            if source_identity.to_bytes() != raw {
                return Err(CatalogError::Corrupt("LB2 完整来源编码非规范"));
            }
            LayoutProvenance::VerifiedCopySource {
                copy_record_sha,
                source_identity_sha,
                source_identity,
            }
        } else {
            LayoutProvenance::CommittedReceipt {
                receipt: reader.fixed()?,
                request_epoch: reader.u64()?,
                batch: reader.uuid()?,
            }
        };
        let path_len = u16::from_be_bytes(reader.fixed()?) as usize;
        let path = std::str::from_utf8(reader.take(path_len)?)
            .map_err(|_| CatalogError::Corrupt("正文布局路径不是 UTF-8"))?
            .to_owned();
        let manifest = LayoutManifest::decode(reader.take(MANIFEST_BYTES)?)?;
        if reader.at != bytes.len() {
            return Err(CatalogError::Corrupt("正文布局绑定含多余尾部"));
        }
        let binding = Self {
            namespace,
            file,
            copy,
            version,
            scan,
            body_uid,
            length,
            content_digest,
            source,
            provenance,
            path,
            manifest,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(crate) fn matches_copy(
        &self,
        namespace: Uuid,
        file: Uuid,
        copy_id: Uuid,
        copy: &PhysicalCopy,
        source: ActiveIndexIdentity,
    ) -> bool {
        let provenance = match &self.provenance {
            LayoutProvenance::CommittedReceipt { request_epoch, .. } => {
                copy.media.is_none()
                    && copy.origin
                        == (CopyOrigin::FileRequest {
                            request_epoch: *request_epoch,
                        })
            }
            LayoutProvenance::VerifiedCopySource { .. } => true,
        };
        provenance
            && self.namespace == namespace
            && self.file == file
            && self.copy == copy_id
            && self.version == copy.version
            && self.scan == copy.volume_scan_id
            && self.source == source
            && self.source.volume_uuid == copy.volume_id
            && self.source.generation == copy.volume_generation
            && self.body_uid == copy.ltfs_file_uid
            && self.length == copy.size
            && self.content_digest == copy.content_digest
    }

    fn matches_receipt(&self, receipt: &CommitReceipt, source: ActiveIndexIdentity) -> bool {
        let LayoutProvenance::CommittedReceipt {
            receipt: fingerprint,
            request_epoch,
            ..
        } = &self.provenance
        else {
            return false;
        };
        self.namespace == receipt.namespace
            && self.file == receipt.file_id
            && self.copy == receipt.copy_id
            && Some(self.version) == receipt.expected_version.unwrap_or(0).checked_add(1)
            && self.scan == receipt.volume_scan_id
            && self.body_uid == receipt.ltfs_file_uid
            && self.length == receipt.size
            && self.content_digest == receipt.content_digest
            && *request_epoch == receipt.request_epoch
            && self.source == source
            && self.source.volume_uuid == receipt.volume_id
            && self.source.generation == receipt.volume_generation
            && fingerprint.as_slice() == receipt_fingerprint(receipt).as_slice()
    }

    fn batch(&self) -> Option<Uuid> {
        match &self.provenance {
            LayoutProvenance::CommittedReceipt { batch, .. } => Some(*batch),
            _ => None,
        }
    }

    pub(crate) fn source_identity(&self) -> Option<&IndexSourceIdentity> {
        match &self.provenance {
            LayoutProvenance::VerifiedCopySource {
                source_identity, ..
            } => Some(source_identity),
            _ => None,
        }
    }

    pub(crate) fn matches_raw_copy(&self, raw: Option<&[u8]>) -> bool {
        match &self.provenance {
            LayoutProvenance::CommittedReceipt { .. } => true,
            LayoutProvenance::VerifiedCopySource {
                copy_record_sha, ..
            } => raw.is_some_and(|raw| copy_record_digest(raw) == *copy_record_sha),
        }
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn layout_digest(&self) -> [u8; 32] {
        self.manifest.digest
    }

    pub(crate) fn fits_read_budget(&self, maximum_bytes: u64) -> bool {
        self.encoded_bytes() <= maximum_bytes.min(MAX_READ_LAYOUT_BYTES)
    }
}

struct BindingReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> BindingReader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(CatalogError::Corrupt("正文布局绑定长度溢出"))?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or(CatalogError::Corrupt("正文布局绑定截断"))?;
        self.at = end;
        Ok(value)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("checked field length"))
    }

    fn uuid(&mut self) -> Result<Uuid> {
        Ok(Uuid::from_bytes(self.fixed()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.fixed()?))
    }
}

fn namespace_prefix(namespace: Uuid) -> Vec<u8> {
    let mut out = key(BODY_LAYOUT_KIND);
    out.extend_from_slice(namespace.as_bytes());
    out
}

fn object_prefix(namespace: Uuid, manifest: LayoutManifest) -> Vec<u8> {
    let mut out = namespace_prefix(namespace);
    out.push(0);
    out.extend_from_slice(&manifest.digest);
    out
}

fn progress_key(namespace: Uuid, manifest: LayoutManifest) -> Vec<u8> {
    let mut out = object_prefix(namespace, manifest);
    out.push(0);
    out
}

fn chunk_key(namespace: Uuid, manifest: LayoutManifest, chunk: u32) -> Vec<u8> {
    let mut out = object_prefix(namespace, manifest);
    out.push(1);
    out.extend_from_slice(&chunk.to_be_bytes());
    out
}

fn binding_key(namespace: Uuid, file: Uuid, copy: Uuid) -> Vec<u8> {
    let mut out = namespace_prefix(namespace);
    out.push(1);
    out.extend_from_slice(file.as_bytes());
    out.extend_from_slice(copy.as_bytes());
    out
}

fn progress(manifest: LayoutManifest, next: u32) -> Vec<u8> {
    let mut out = b"LP1".to_vec();
    out.extend_from_slice(&manifest.encode());
    out.extend_from_slice(&next.to_be_bytes());
    out
}

fn decode_progress(bytes: &[u8], manifest: LayoutManifest) -> Result<u32> {
    if bytes.len() != 3 + MANIFEST_BYTES + 4
        || &bytes[..3] != b"LP1"
        || LayoutManifest::decode(&bytes[3..3 + MANIFEST_BYTES])? != manifest
    {
        return Err(CatalogError::Corrupt("正文布局上传身份"));
    }
    let next = u32::from_be_bytes(
        bytes[3 + MANIFEST_BYTES..]
            .try_into()
            .expect("checked progress length"),
    );
    if next > manifest.chunks {
        return Err(CatalogError::Corrupt("正文布局上传进度"));
    }
    Ok(next)
}

async fn require_namespace(tr: &Transaction, namespace: Uuid) -> Result<()> {
    if namespace.is_nil() {
        return Err(CatalogError::Invalid("正文布局命名空间为 nil"));
    }
    let state = tr.get(&namespace_key(namespace), false).await?;
    if !matches!(
        state.as_deref().map(decode_namespace_state).transpose()?,
        Some(NamespaceState::Building | NamespaceState::Ready | NamespaceState::Active)
    ) {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

async fn require_complete(
    tr: &Transaction,
    namespace: Uuid,
    manifest: LayoutManifest,
) -> Result<()> {
    manifest.validate()?;
    let raw = tr
        .get(&progress_key(namespace, manifest), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if decode_progress(&raw, manifest)? != manifest.chunks {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

fn required_copy_key(namespace: Uuid, file: Uuid, copy: Uuid) -> Vec<u8> {
    let mut key = namespace_prefix(namespace);
    key.push(4);
    key.extend_from_slice(file.as_bytes());
    key.extend_from_slice(copy.as_bytes());
    key
}
fn required_copy_value(raw_binding: &[u8]) -> [u8; 35] {
    let digest = Sha256::new()
        .chain_update(b"tape-rs/copy-layout-required/1")
        .chain_update(raw_binding)
        .finalize();
    let mut value = [0; 35];
    value[..3].copy_from_slice(b"LC1");
    value[3..].copy_from_slice(&digest);
    value
}

#[cfg(test)]
pub(in crate::managed_catalog) fn test_copy_layout_keys(
    namespace: Uuid,
    file: Uuid,
    copy: Uuid,
) -> (Vec<u8>, Vec<u8>) {
    (
        binding_key(namespace, file, copy),
        required_copy_key(namespace, file, copy),
    )
}

#[cfg(test)]
pub(in crate::managed_catalog) fn test_required_layout_value(
    binding: &CopyLayoutBinding,
) -> [u8; 35] {
    required_copy_value(&binding.encode().expect("validated test binding"))
}

#[cfg(test)]
pub(in crate::managed_catalog) use retained::{
    test_inert_neighbor_scope_key, test_retained_layout_key,
};

#[cfg(test)]
pub(in crate::managed_catalog) fn test_first_extent_chunk(
    binding: &CopyLayoutBinding,
) -> (Vec<u8>, usize) {
    assert!(binding.manifest.extents > 0);
    // First extent's start-block least-significant byte in the real codec.
    (
        chunk_key(binding.namespace, binding.manifest, 0),
        LAYOUT_HEADER + 8,
    )
}

#[cfg(test)]
pub(in crate::managed_catalog) fn test_small_layout_object_keys(
    binding: &CopyLayoutBinding,
) -> Vec<Vec<u8>> {
    assert!(binding.manifest.chunks <= CHUNKS_PER_TRANSACTION as u32);
    let mut keys = vec![progress_key(binding.namespace, binding.manifest)];
    for index in 0..binding.manifest.chunks {
        keys.push(chunk_key(binding.namespace, binding.manifest, index));
    }
    keys
}
pub(in crate::managed_catalog) async fn has_required_copy_at(
    tr: &Transaction,
    namespace: Uuid,
    file: Uuid,
    copy: Uuid,
) -> Result<bool> {
    Ok(tr
        .get(&required_copy_key(namespace, file, copy), false)
        .await?
        .is_some())
}
pub(super) async fn binding_at(
    tr: &Transaction,
    namespace: Uuid,
    file: Uuid,
    copy: Uuid,
) -> Result<Option<CopyLayoutBinding>> {
    let marker = tr
        .get(&required_copy_key(namespace, file, copy), false)
        .await?;
    let raw = tr.get(&binding_key(namespace, file, copy), false).await?;
    if let Some(marker) = marker {
        let raw = raw
            .as_deref()
            .ok_or(CatalogError::Corrupt("必需正文布局缺少全局绑定"))?;
        if marker.as_ref() != required_copy_value(raw) {
            return Err(CatalogError::Corrupt("必需正文布局标记与全局绑定不同"));
        }
    }
    raw.map(|raw| {
        let binding = CopyLayoutBinding::decode(&raw)?;
        if binding.namespace != namespace || binding.file != file || binding.copy != copy {
            return Err(CatalogError::Corrupt("正文布局绑定与键身份不一致"));
        }
        Ok(binding)
    })
    .transpose()
}
async fn install_global_binding_at(
    tr: &Transaction,
    binding: &CopyLayoutBinding,
    raw: &[u8],
) -> Result<()> {
    // Validate an existing requirement before making either mutation.
    binding_at(tr, binding.namespace, binding.file, binding.copy).await?;
    let key = binding_key(binding.namespace, binding.file, binding.copy);
    if let Some(old) = tr.get(&key, false).await? {
        if old.as_ref() != raw {
            return Err(CatalogError::Corrupt("全局正文布局重试改变原绑定"));
        }
    } else {
        tr.set(&key, raw);
    }
    let key = required_copy_key(binding.namespace, binding.file, binding.copy);
    let expected = required_copy_value(raw);
    if let Some(old) = tr.get(&key, false).await? {
        if old.as_ref() != expected {
            return Err(CatalogError::Corrupt("全局正文布局 required 标记冲突"));
        }
    } else {
        tr.set(&key, &expected);
    }
    Ok(())
}

/// Called inside the original active batch preparation/publication transaction.
pub(super) async fn require_binding_at(
    tr: &Transaction,
    batch: Uuid,
    receipt: &CommitReceipt,
    source: ActiveIndexIdentity,
) -> Result<()> {
    let binding = binding_at(tr, receipt.namespace, receipt.file_id, receipt.copy_id)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if binding.batch() != Some(batch) || !binding.matches_receipt(receipt, source) {
        return Err(CatalogError::Corrupt(
            "正文布局与原批次、收据或提交构造不一致",
        ));
    }
    if !has_required_copy_at(tr, receipt.namespace, receipt.file_id, receipt.copy_id).await? {
        return Err(CatalogError::Incomplete);
    }
    require_complete(tr, receipt.namespace, binding.manifest).await
}

/// Optional constraint for a new daemon batch. Legacy batches have no record.
/// The original batch module supplies its sealed manifest and observed source;
/// the layout domain does not create a second batch or publication authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BatchLayoutRequirement {
    pub namespace: Uuid,
    pub batch: Uuid,
    pub source: ActiveIndexIdentity,
    pub manifest: [u8; 32],
    pub items: u32,
    pub pages: u16,
}

impl BatchLayoutRequirement {
    fn encode(self) -> Result<Vec<u8>> {
        if self.namespace.is_nil()
            || self.source.volume_uuid.is_nil()
            || self.source.generation == 0
            || !(1..=65_536).contains(&self.items)
            || usize::from(self.pages) != (self.items as usize).div_ceil(128)
        {
            return Err(CatalogError::Invalid("正文布局批次要求身份或分页无效"));
        }
        let mut out = b"LR1".to_vec();
        out.extend_from_slice(self.namespace.as_bytes());
        out.extend_from_slice(self.batch.as_bytes());
        out.extend_from_slice(self.source.volume_uuid.as_bytes());
        active_volume::encode_index(&mut out, self.source);
        out.extend_from_slice(&self.manifest);
        out.extend_from_slice(&self.items.to_be_bytes());
        out.extend_from_slice(&self.pages.to_be_bytes());
        Ok(out)
    }

    fn prefix(self) -> Vec<u8> {
        let mut out = namespace_prefix(self.namespace);
        out.push(2);
        out.extend_from_slice(self.batch.as_bytes());
        out
    }

    fn key(self) -> Vec<u8> {
        let mut out = self.prefix();
        out.push(0);
        out
    }

    fn page_prefix(self) -> Vec<u8> {
        let mut out = self.prefix();
        out.push(1);
        out
    }

    fn page_key(self, page: u16) -> Vec<u8> {
        let mut out = self.page_prefix();
        out.extend_from_slice(&page.to_be_bytes());
        out
    }

    fn marker_page_prefix(self) -> Vec<u8> {
        let mut key = self.prefix();
        key.push(2);
        key
    }
    fn marker_page_key(self, page: u16) -> Vec<u8> {
        let mut key = self.marker_page_prefix();
        key.extend_from_slice(&page.to_be_bytes());
        key
    }
    fn marker_page_value(self, page: u16, prepared: &[u8; 49]) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/copy-layout-marker-page/1")
            .chain_update(self.page_marker(page, prepared)?)
            .finalize()
            .into())
    }

    fn page_marker(self, page: u16, prepared: &[u8; 49]) -> Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/body-layout-page/1");
        hash.update(self.encode()?);
        hash.update(page.to_be_bytes());
        hash.update(prepared);
        Ok(hash.finalize().into())
    }

    /// Must be called before the original publication barrier can be written.
    /// The caller checks original owner, sealed manifest and unpublished state.
    pub(super) async fn declare_at(self, tr: &Transaction) -> Result<()> {
        require_namespace(tr, self.namespace).await?;
        let expected = self.encode()?;
        let key = self.key();
        if let Some(saved) = tr.get(&key, false).await? {
            if saved.as_ref() != expected {
                return Err(CatalogError::Corrupt("正文布局批次要求改变"));
            }
        } else {
            tr.set(&key, &expected);
        }
        Ok(())
    }

    pub(super) async fn exists_at(self, tr: &Transaction) -> Result<bool> {
        let Some(saved) = tr.get(&self.key(), false).await? else {
            return Ok(false);
        };
        if saved.as_ref() != self.encode()? {
            return Err(CatalogError::Corrupt("正文布局批次要求与原清单不一致"));
        }
        Ok(true)
    }

    /// Called even when the older attribute preparation page already exists.
    /// Thus legacy page markers cannot satisfy a newly declared requirement.
    pub(super) async fn prepare_page_at(
        self,
        tr: &Transaction,
        page: u16,
        raw: &[u8],
    ) -> Result<()> {
        if !self.exists_at(tr).await? {
            return Ok(());
        }
        let count = (self.items as usize)
            .saturating_sub(usize::from(page) * 128)
            .min(128);
        if page >= self.pages
            || raw.len() != 2 + count * 56
            || u16::from_be_bytes(raw[..2].try_into().expect("checked page length")) as usize
                != count
        {
            return Err(CatalogError::Corrupt("正文布局批次页长度或编号无效"));
        }
        for entry in raw[2..].chunks_exact(56) {
            let epoch = read_u64(&entry[..8]);
            let operation = uuid_from(&entry[8..24])?;
            let mut intent_key = requests::epoch_prefix(b'I', self.namespace, epoch);
            intent_key.extend_from_slice(operation.as_bytes());
            let intent = tr
                .get(&intent_key, false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let receipt = requests::decode_intent(&intent, self.namespace, epoch, operation)?;
            if receipt_fingerprint(&receipt) != entry[24..]
                || receipt.volume_id != self.source.volume_uuid
                || receipt.volume_generation != self.source.generation
            {
                return Err(CatalogError::Corrupt("正文布局批次页与原收据不一致"));
            }
            require_binding_at(tr, self.batch, &receipt, self.source).await?;
        }
        let mut original = [0; 49];
        original[0] = 1;
        original[1..17].copy_from_slice(self.batch.as_bytes());
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/active-prepared-page/1");
        hash.update(raw);
        original[17..].copy_from_slice(&hash.finalize());
        let marker = self.page_marker(page, &original)?;
        let key = self.page_key(page);
        if let Some(saved) = tr.get(&key, false).await? {
            if saved.as_ref() != marker {
                return Err(CatalogError::Corrupt("正文布局准备页改变"));
            }
        } else {
            tr.set(&key, &marker);
        }
        let key = self.marker_page_key(page);
        let expected = self.marker_page_value(page, &original)?;
        if let Some(saved) = tr.get(&key, false).await? {
            if saved.as_ref() != expected {
                return Err(CatalogError::Corrupt("正文布局 marker 页证明改变"));
            }
        } else {
            tr.set(&key, &expected);
        }
        Ok(())
    }

    /// Original publication reads at most 512 small page markers. Absence of a
    /// requirement is the explicit legacy fallback, not an incomplete layout.
    pub(super) async fn complete_at(self, tr: &Transaction, prepared: &[[u8; 49]]) -> Result<bool> {
        if !self.exists_at(tr).await? {
            return Ok(true);
        }
        if prepared.len() != usize::from(self.pages) {
            return Ok(false);
        }
        let rows = read_page(
            tr,
            &self.page_prefix(),
            None,
            usize::from(self.pages) + 1,
            false,
        )
        .await?;
        if rows.next.is_some() || rows.items.len() != usize::from(self.pages) {
            return Ok(false);
        }
        for (number, (key, value)) in rows.items.into_iter().enumerate() {
            if key != self.page_key(number as u16)
                || value.as_slice() != self.page_marker(number as u16, &prepared[number])?
            {
                return Err(CatalogError::Corrupt("正文布局准备页标记损坏"));
            }
        }
        let rows = read_page(
            tr,
            &self.marker_page_prefix(),
            None,
            usize::from(self.pages) + 1,
            false,
        )
        .await?;
        if rows.next.is_some() || rows.items.len() != usize::from(self.pages) {
            return Ok(false);
        }
        for (number, (key, value)) in rows.items.into_iter().enumerate() {
            if key != self.marker_page_key(number as u16)
                || value.as_slice() != self.marker_page_value(number as u16, &prepared[number])?
            {
                return Err(CatalogError::Corrupt("正文布局 marker 页证明损坏"));
            }
        }
        Ok(true)
    }
}

struct LayoutReader<'a> {
    catalog: &'a ManagedCatalog,
    namespace: Uuid,
    manifest: LayoutManifest,
    next: u32,
    chunk: Vec<u8>,
    offset: usize,
    consumed: usize,
    hash: Sha256,
    location: Option<BindingLocation>,
}

#[derive(Clone)]
enum LayoutStageAuthority<'a> {
    Active {
        serial: String,
        owner: u64,
    },
    OriginalCheckpoint(&'a crate::managed_catalog::OriginalCheckpointContext),
    #[cfg(test)]
    StorageFixture,
}

impl LayoutStageAuthority<'_> {
    fn finish<T>(&self, result: Result<T>) -> Result<T> {
        match self {
            Self::OriginalCheckpoint(_) => result.map_err(control::notifications::atomic_error),
            _ => result,
        }
    }
    async fn require_at(
        &self,
        tr: &Transaction,
        batch: Uuid,
        proof: &VerifiedCommittedBodyLayout<'_>,
    ) -> Result<()> {
        require_namespace(tr, proof.receipt.namespace).await?;
        match self {
            Self::Active { serial, owner } => {
                active_batch::require_layout_stage_at(
                    tr,
                    serial,
                    *owner,
                    batch,
                    &proof.receipt,
                    proof.source,
                )
                .await
            }
            Self::OriginalCheckpoint(context) => {
                active_batch::require_original_layout_stage_at(
                    tr,
                    context,
                    batch,
                    &proof.receipt,
                    proof.source,
                )
                .await
            }
            #[cfg(test)]
            Self::StorageFixture => Ok(()),
        }
    }
}

// Unlike the generic read_page, this exact page does not fetch a lookahead
// value. All returned keys are checked against the next expected chunk key.
async fn layout_chunks_at(
    tr: &Transaction,
    prefix: &[u8],
    after: Option<&[u8]>,
    count: usize,
) -> std::result::Result<Vec<(Vec<u8>, Vec<u8>)>, FdbBindingError> {
    let begin = after.map_or_else(
        || KeySelector::first_greater_or_equal(prefix.to_vec()),
        |key| KeySelector::first_greater_than(key.to_vec()),
    );
    let mut range = RangeOption {
        begin,
        end: KeySelector::first_greater_or_equal(prefix_end(prefix)),
        limit: Some(count),
        ..RangeOption::default()
    };
    let mut rows = Vec::with_capacity(count);
    let mut iteration = 1;
    loop {
        let values = tr.get_range(&range, iteration, true).await?;
        for value in values.iter() {
            rows.push((value.key().to_vec(), value.value().to_vec()));
        }
        match range.next_range(&values) {
            Some(next) if rows.len() < count => {
                range = next;
                iteration += 1;
            }
            _ => return Ok(rows),
        }
    }
}

impl LayoutReader<'_> {
    async fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0; N];
        let mut at = 0;
        while at < N {
            if self.offset == self.chunk.len() {
                self.available().await?;
            }
            let count = (N - at).min(self.chunk.len() - self.offset);
            out[at..at + count].copy_from_slice(&self.chunk[self.offset..self.offset + count]);
            self.offset += count;
            self.consumed += count;
            at += count;
        }
        Ok(out)
    }

    async fn available(&mut self) -> Result<()> {
        if self.next == self.manifest.chunks {
            return Err(CatalogError::Corrupt("正文布局数据截断"));
        }
        let first = self.next;
        let count = ((self.manifest.chunks - first) as usize).min(CHUNKS_PER_TRANSACTION);
        let mut prefix = object_prefix(self.namespace, self.manifest);
        prefix.push(1);
        let after = (first > 0).then(|| chunk_key(self.namespace, self.manifest, first - 1));
        let location = self.location.clone();
        let chunks = self
            .catalog
            .db
            .run(|tr, _| {
                let prefix = prefix.clone();
                let after = after.clone();
                let location = location.clone();
                async move {
                    requests::retry_result(
                        async {
                            if let Some(location) = location {
                                location.require_at(&tr).await?;
                            }
                            Ok(layout_chunks_at(&tr, &prefix, after.as_deref(), count).await?)
                        }
                        .await,
                    )
                }
            })
            .await??;
        self.chunk.clear();
        if chunks.len() != count {
            return Err(CatalogError::Corrupt("正文布局分片缺失"));
        }
        for (offset, (key, bytes)) in chunks.into_iter().enumerate() {
            let index = first + offset as u32;
            let expected =
                (self.manifest.len as usize - index as usize * CHUNK_BYTES).min(CHUNK_BYTES);
            if key != chunk_key(self.namespace, self.manifest, index) || bytes.len() != expected {
                return Err(CatalogError::Corrupt("正文布局分片顺序或长度"));
            }
            self.chunk.extend_from_slice(&bytes);
        }
        if first as usize + count == self.manifest.chunks as usize {
            let after = chunk_key(self.namespace, self.manifest, self.manifest.chunks - 1);
            let extra = self
                .catalog
                .db
                .run(|tr, _| {
                    let prefix = prefix.clone();
                    let after = after.clone();
                    let location = location.clone();
                    async move {
                        requests::retry_result(
                            async {
                                if let Some(location) = location {
                                    location.require_at(&tr).await?;
                                }
                                Ok(layout_chunks_at(&tr, &prefix, Some(&after), 1).await?)
                            }
                            .await,
                        )
                    }
                })
                .await??;
            if !extra.is_empty() {
                return Err(CatalogError::Corrupt("正文布局有清单之外的分片"));
            }
        }
        self.hash.update(&self.chunk);
        self.next += count as u32;
        self.offset = 0;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.consumed != self.manifest.len as usize
            || self.offset != self.chunk.len()
            || self.next != self.manifest.chunks
            || self.hash.finalize().as_slice() != self.manifest.digest
        {
            return Err(CatalogError::Corrupt("正文布局完整长度或摘要不符"));
        }
        Ok(())
    }
}

impl ManagedCatalog {
    async fn verify_body_layout_object(
        &self,
        namespace: Uuid,
        manifest: LayoutManifest,
    ) -> Result<()> {
        self.verify_body_layout_object_at(namespace, manifest, None)
            .await
    }
    async fn verify_body_layout_object_at(
        &self,
        namespace: Uuid,
        manifest: LayoutManifest,
        location: Option<BindingLocation>,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        if let Some(location) = &location {
            location.require_at(&tr).await?;
        }
        require_complete(&tr, namespace, manifest).await?;
        drop(tr);
        let mut hash = Sha256::new();
        hash.update(LAYOUT_DOMAIN);
        let mut reader = LayoutReader {
            catalog: self,
            namespace,
            manifest,
            next: 0,
            chunk: Vec::new(),
            offset: 0,
            consumed: 0,
            hash,
            location,
        };
        while reader.next < manifest.chunks {
            reader.available().await?;
            reader.consumed += reader.chunk.len();
            reader.offset = reader.chunk.len();
        }
        reader.finish()
    }

    /// Upload immutable chunks under the original batch/owner execution fence.
    /// This never publishes a copy or authorizes physical device operations.
    pub(crate) async fn stage_observed_body_layout(
        &self,
        serial: &str,
        owner: u64,
        batch: Uuid,
        verified: &VerifiedCommittedBodyLayout<'_>,
    ) -> Result<CopyLayoutBinding> {
        control::validate_physical_serial(serial)?;
        if owner == 0 || verified.receipt.owner_epoch != owner {
            return Err(CatalogError::OwnerFenced);
        }
        self.stage_body_layout_inner(
            batch,
            verified,
            LayoutStageAuthority::Active {
                serial: serial.to_owned(),
                owner,
            },
        )
        .await
    }

    pub(crate) async fn stage_original_checkpoint_body_layout(
        &self,
        context: &crate::managed_catalog::OriginalCheckpointContext,
        batch: Uuid,
        verified: &VerifiedCommittedBodyLayout<'_>,
    ) -> Result<CopyLayoutBinding> {
        if batch != context.batch_id() {
            return Err(CatalogError::Invalid("原checkpoint布局不得替换batch"));
        }
        self.stage_body_layout_inner(
            batch,
            verified,
            LayoutStageAuthority::OriginalCheckpoint(context),
        )
        .await
    }

    #[cfg(test)]
    async fn stage_test_body_layout(
        &self,
        batch: Uuid,
        verified: &VerifiedCommittedBodyLayout<'_>,
    ) -> Result<CopyLayoutBinding> {
        self.stage_body_layout_inner(batch, verified, LayoutStageAuthority::StorageFixture)
            .await
    }

    async fn stage_body_layout_inner(
        &self,
        batch: Uuid,
        verified: &VerifiedCommittedBodyLayout<'_>,
        authority: LayoutStageAuthority<'_>,
    ) -> Result<CopyLayoutBinding> {
        let binding = CopyLayoutBinding::from_verified(batch, verified)?;
        let namespace = binding.namespace;
        let manifest = binding.manifest;
        let marker = progress_key(namespace, manifest);
        let mut next = 0;
        while next < manifest.chunks {
            let expected = next;
            next = self
                .db
                .run(|tr, _| {
                    let marker = marker.clone();
                    let authority = authority.clone();
                    async move {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            512_000,
                        ))?;
                        requests::retry_result(
                            authority.finish(
                                async {
                                    authority.require_at(&tr, batch, verified).await?;
                                    let raw = tr.get(&marker, false).await?;
                                    let first = raw
                                        .as_deref()
                                        .map(|raw| decode_progress(raw, manifest))
                                        .transpose()?
                                        .unwrap_or(0);
                                    if first > expected {
                                        return Ok(first);
                                    }
                                    if first < expected {
                                        return Err(CatalogError::Corrupt("正文布局上传进度倒退"));
                                    }
                                    let end = (first + CHUNKS_PER_TRANSACTION as u32)
                                        .min(manifest.chunks);
                                    for index in first..end {
                                        let bytes = verified.layout.chunk(manifest, index)?;
                                        let key = chunk_key(namespace, manifest, index);
                                        if let Some(saved) = tr.get(&key, false).await? {
                                            if saved.as_ref() != bytes {
                                                return Err(CatalogError::Corrupt(
                                                    "正文布局分片重试冲突",
                                                ));
                                            }
                                        } else {
                                            tr.set(&key, &bytes);
                                        }
                                    }
                                    tr.set(&marker, &progress(manifest, end));
                                    #[cfg(test)]
                                    assert!(tr.get_approximate_size().await? < 512_000);
                                    Ok(end)
                                }
                                .await,
                            ),
                        )
                    }
                })
                .await??;
        }
        self.verify_body_layout_object(namespace, manifest).await?;
        let raw = binding.encode()?;
        let saved_binding = &binding;
        self.db
            .run(|tr, _| {
                let raw = raw.clone();
                let binding = saved_binding;
                let authority = authority.clone();
                async move {
                    requests::retry_result(
                        authority.finish(
                            async {
                                authority.require_at(&tr, batch, verified).await?;
                                require_complete(&tr, namespace, manifest).await?;
                                install_global_binding_at(&tr, &binding, &raw).await?;
                                Ok(())
                            }
                            .await,
                        ),
                    )
                }
            })
            .await??;
        Ok(binding)
    }

    /// Compatibility decoder for small explicit/test plans. Managed reads use
    /// prepare_body_read_plan and never treat a budget miss as a cache fallback.
    pub(crate) async fn load_body_layout(
        &self,
        binding: &CopyLayoutBinding,
        maximum_bytes: u64,
    ) -> Result<Option<BodyLayout<'static>>> {
        binding.validate()?;
        let manifest = binding.manifest;
        let tr = self.db.create_trx()?;
        require_namespace(&tr, binding.namespace).await?;
        require_complete(&tr, binding.namespace, manifest).await?;
        if binding_at(&tr, binding.namespace, binding.file, binding.copy)
            .await?
            .as_ref()
            != Some(binding)
        {
            return Err(CatalogError::Corrupt("正文布局读取前绑定不一致"));
        }
        drop(tr);
        if !binding.fits_read_budget(maximum_bytes) {
            // The binding/schema/complete marker have been checked. Budget
            // exhaustion is not evidence that unvisited chunks are corrupt.
            // Do not fetch those chunks merely to choose the old safe path.
            return Ok(None);
        }
        let mut hash = Sha256::new();
        hash.update(LAYOUT_DOMAIN);
        let mut reader = LayoutReader {
            catalog: self,
            namespace: binding.namespace,
            manifest,
            next: 0,
            chunk: Vec::new(),
            offset: 0,
            consumed: 0,
            hash,
            location: None,
        };
        if reader.fixed::<3>().await? != *b"BL1" {
            return Err(CatalogError::Corrupt("正文布局版本"));
        }
        let volume = Uuid::from_bytes(reader.fixed().await?);
        let uid = u64::from_be_bytes(reader.fixed().await?);
        let length = u64::from_be_bytes(reader.fixed().await?);
        let block_size = u32::from_be_bytes(reader.fixed().await?);
        let partitions = reader.fixed::<2>().await?.map(char::from);
        let count = u32::from_be_bytes(reader.fixed().await?);
        if count != manifest.extents {
            return Err(CatalogError::Corrupt("正文布局 extent 数量与清单不符"));
        }
        let mut extents = Vec::new();
        extents
            .try_reserve_exact(count as usize)
            .map_err(|_| CatalogError::Invalid("正文布局超过可用内存预算"))?;
        for _ in 0..count {
            extents.push(Extent {
                partition: reader.fixed::<1>().await?[0] as char,
                start_block: u64::from_be_bytes(reader.fixed().await?),
                byte_offset: u64::from_be_bytes(reader.fixed().await?),
                byte_count: u64::from_be_bytes(reader.fixed().await?),
                file_offset: u64::from_be_bytes(reader.fixed().await?),
            });
        }
        reader.finish()?;
        let layout = BodyLayout::new(volume, uid, length, block_size, partitions, extents)
            .map_err(|_| CatalogError::Corrupt("正文布局 extent 语义"))?;
        if layout.volume != binding.source.volume_uuid
            || layout.uid != binding.body_uid
            || layout.length != binding.length
        {
            return Err(CatalogError::Corrupt("正文布局与副本正文身份不一致"));
        }
        let tr = self.db.create_trx()?;
        require_namespace(&tr, binding.namespace).await?;
        require_complete(&tr, binding.namespace, manifest).await?;
        if binding_at(&tr, binding.namespace, binding.file, binding.copy)
            .await?
            .as_ref()
            != Some(binding)
        {
            return Err(CatalogError::Corrupt("正文布局读取期间绑定改变"));
        }
        Ok(Some(layout))
    }
}

#[cfg(test)]
#[path = "body_layouts/tests.rs"]
mod tests;
