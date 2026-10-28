//! Durable, bounded Active batch admission before any media I/O.
//! A sealed batch owns its request intents, but does not publish files or
//! assert that a tape commit happened.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

use super::*;

const PAGE_ITEMS: usize = 128;
const MAX_BATCH_ITEMS: usize = 65_536;
const HEADER_LEN: usize = 96;
const PREPARED_LEN: usize = 87;
const PREPARED_PAGE_LEN: usize = 49;
const PREPARED_BATCH_LEN: usize = 113;
const TARGET_LOCATOR_LEN: usize = 81;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveBatchOutcome {
    Staged,
    AlreadyStaged,
    Aborted,
    AlreadyAborted,
    StaleOwner,
    GenerationConflict,
    BatchConflict,
    RequestConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveBatchPhase {
    Open = 0,
    Sealed = 1,
    Aborting = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePreparationOutcome {
    Prepared,
    AlreadyPrepared,
    StaleOwner,
    BatchConflict,
    RequestConflict,
    IndexNotObserved,
    PageIncomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePublicationOutcome {
    Published,
    AlreadyPublished,
    StaleOwner,
    BatchConflict,
    IndexNotObserved,
    PageIncomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveFinalizationOutcome {
    Finalized,
    AlreadyFinalized,
    StaleOwner,
    BatchConflict,
    Unpublished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveMaterializationOutcome {
    Materialized,
    AlreadyMaterialized,
    StaleOwner,
    NotFinalized,
    BatchConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveCompactionOutcome {
    Completed,
    AlreadyCompleted,
    StaleOwner,
    BatchConflict,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveBatchStatus {
    pub batch_id: Uuid,
    pub items: u32,
    pub pages: u16,
    pub phase: ActiveBatchPhase,
    /// All files have been materialized; the compact completion receipt is durable.
    pub completed: bool,
    /// The atomic visibility barrier exists for this batch.
    pub published: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveBatchRecoveryPage {
    /// Exact observed on-tape index for this batch. It is not a read plan.
    pub index: active_volume::ActiveIndexIdentity,
    pub receipts: Vec<CommitReceipt>,
    pub next_page: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSealedBatchPage {
    pub files: Vec<(CommitReceipt, physical::ActiveFileIntent)>,
    pub next_page: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    batch_id: Uuid,
    namespace: Uuid,
    owner_epoch: u64,
    scan_id: Uuid,
    items: u32,
    pages: u16,
    digest: [u8; 32],
    phase: ActiveBatchPhase,
}

impl Header {
    fn encode(self) -> [u8; HEADER_LEN] {
        let mut value = [0; HEADER_LEN];
        value[0] = 2;
        value[1] = self.phase as u8;
        value[2..18].copy_from_slice(self.batch_id.as_bytes());
        value[18..34].copy_from_slice(self.namespace.as_bytes());
        value[34..42].copy_from_slice(&self.owner_epoch.to_be_bytes());
        value[42..58].copy_from_slice(self.scan_id.as_bytes());
        value[58..62].copy_from_slice(&self.items.to_be_bytes());
        value[62..64].copy_from_slice(&self.pages.to_be_bytes());
        value[64..96].copy_from_slice(&self.digest);
        value
    }

    fn decode(value: &[u8]) -> Result<Self> {
        if value.len() != HEADER_LEN || value[0] != 2 || value[1] > 2 {
            return Err(CatalogError::Corrupt("活动批次头版本或长度"));
        }
        let header = Self {
            batch_id: uuid_from(&value[2..18])?,
            namespace: uuid_from(&value[18..34])?,
            owner_epoch: read_u64(&value[34..42]),
            scan_id: uuid_from(&value[42..58])?,
            items: u32::from_be_bytes(value[58..62].try_into().expect("validated length")),
            pages: u16::from_be_bytes(value[62..64].try_into().expect("validated length")),
            digest: value[64..96].try_into().expect("validated length"),
            phase: match value[1] {
                0 => ActiveBatchPhase::Open,
                1 => ActiveBatchPhase::Sealed,
                2 => ActiveBatchPhase::Aborting,
                _ => unreachable!("validated phase"),
            },
        };
        if header.batch_id.is_nil()
            || header.namespace.is_nil()
            || header.scan_id.is_nil()
            || header.owner_epoch == 0
            || !(1..=MAX_BATCH_ITEMS as u32).contains(&header.items)
            || usize::from(header.pages) != (header.items as usize).div_ceil(PAGE_ITEMS)
        {
            return Err(CatalogError::Corrupt("活动批次头身份或分页"));
        }
        Ok(header)
    }

    fn same_manifest(self, other: Self) -> bool {
        Self {
            phase: ActiveBatchPhase::Open,
            ..self
        } == Self {
            phase: ActiveBatchPhase::Open,
            ..other
        }
    }
}

pub(super) fn batch_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"b/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

pub(super) async fn unsealed_or_absent_for_recovery(
    tr: &Transaction,
    volume: Uuid,
    generation: u64,
) -> Result<bool> {
    let Some(value) = tr.get(&batch_key(volume, generation), false).await? else {
        return Ok(true);
    };
    Ok(Header::decode(&value)?.phase != ActiveBatchPhase::Sealed)
}

fn page_key(volume: Uuid, generation: u64, page: u16) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"g/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&page.to_be_bytes());
    out
}

fn page_digest_key(volume: Uuid, generation: u64, page: u16) -> Vec<u8> {
    let mut out = page_digest_prefix(volume, generation);
    out.extend_from_slice(&page.to_be_bytes());
    out
}

fn page_digest_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"h/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn prepared_key(volume: Uuid, generation: u64, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"v/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}

fn prepared_chunk_key(
    volume: Uuid,
    generation: u64,
    epoch: u64,
    operation: Uuid,
    chunk: u16,
) -> Vec<u8> {
    let mut out = prepared_chunk_prefix(volume, generation, epoch, operation);
    out.extend_from_slice(&chunk.to_be_bytes());
    out
}

fn prepared_chunk_prefix(volume: Uuid, generation: u64, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"a/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}

fn prepared_page_key(volume: Uuid, generation: u64, page: u16) -> Vec<u8> {
    let mut out = prepared_page_prefix(volume, generation);
    out.extend_from_slice(&page.to_be_bytes());
    out
}

fn prepared_page_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"y/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

pub(super) fn prepared_batch_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"z/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn publication_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"c/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn finalized_page_key(volume: Uuid, generation: u64, page: u16) -> Vec<u8> {
    let mut out = finalized_page_prefix(volume, generation);
    out.extend_from_slice(&page.to_be_bytes());
    out
}

fn finalized_page_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"t/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn compacted_page_prefix(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"j/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn compacted_page_key(volume: Uuid, generation: u64, page: u16) -> Vec<u8> {
    let mut out = compacted_page_prefix(volume, generation);
    out.extend_from_slice(&page.to_be_bytes());
    out
}

fn compacted_batch_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"d/");
    out.extend_from_slice(volume.as_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn compacted_batch_value(header: Header, publication: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + PREPARED_BATCH_LEN);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(publication);
    out
}

fn decode_compacted_batch(value: &[u8]) -> Result<(Header, &[u8])> {
    if value.len() != HEADER_LEN + PREPARED_BATCH_LEN {
        return Err(CatalogError::Corrupt("活动批次完成记录长度"));
    }
    let header = Header::decode(&value[..HEADER_LEN])?;
    let publication = &value[HEADER_LEN..];
    if header.phase != ActiveBatchPhase::Sealed
        || publication[0] != 1
        || publication[1..17] != header.batch_id.as_bytes()[..]
    {
        return Err(CatalogError::Corrupt("活动批次完成记录内容"));
    }
    Ok((header, publication))
}

fn materialized_key(namespace: Uuid, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"m/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}

fn materialized_value(batch_id: Uuid, fingerprint: &[u8], version: u64) -> [u8; 57] {
    let mut out = [0; 57];
    out[0] = 1;
    out[1..17].copy_from_slice(batch_id.as_bytes());
    out[17..49].copy_from_slice(fingerprint);
    out[49..57].copy_from_slice(&version.to_be_bytes());
    out
}

/// Readers use the marker as the single visibility barrier for an entire
/// prepared batch. Terminal request rows and permits are projected later.
pub(super) async fn published_at(tr: &Transaction, locator: TargetLocator) -> Result<bool> {
    let Some(marker) = tr
        .get(&publication_key(locator.volume, locator.generation), true)
        .await?
    else {
        return Ok(false);
    };
    let prepared = tr
        .get(
            &prepared_batch_key(locator.volume, locator.generation),
            true,
        )
        .await?
        .ok_or(CatalogError::Corrupt("活动发布标记缺少整批准备证明"))?;
    if marker.len() != PREPARED_BATCH_LEN
        || marker[0] != 1
        || marker[1..17] != locator.batch_id.as_bytes()[..]
        || marker.as_ref() != prepared.as_ref()
    {
        return Err(CatalogError::Corrupt("活动发布标记与整批准备证明不一致"));
    }
    Ok(true)
}

fn prepared_value(batch_id: Uuid, fingerprint: &[u8], attributes: &NodeAttributes) -> Vec<u8> {
    let bytes = attributes.bytes().expect("准备记录要求完整 LTFS 属性");
    let mut out = Vec::with_capacity(PREPARED_LEN);
    out.push(2);
    out.extend_from_slice(batch_id.as_bytes());
    out.extend_from_slice(fingerprint);
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&attributes.digest());
    out.extend_from_slice(&(bytes.len().div_ceil(CHUNK_SIZE) as u16).to_be_bytes());
    out
}

fn decode_prepared(value: &[u8]) -> Result<(Uuid, [u8; 32], AttributeManifest)> {
    if value.len() != PREPARED_LEN || value[0] != 2 {
        return Err(CatalogError::Corrupt("活动文件准备记录版本或长度"));
    }
    let batch_id = uuid_from(&value[1..17])?;
    let manifest = AttributeManifest {
        digest: value[53..85].try_into().expect("validated length"),
        len: u32::from_be_bytes(value[49..53].try_into().expect("validated length")),
        chunks: u16::from_be_bytes(value[85..87].try_into().expect("validated length")),
    };
    if batch_id.is_nil()
        || manifest.len == 0
        || manifest.len as usize > MAX_REPRESENTABLE
        || manifest.chunks as usize != (manifest.len as usize).div_ceil(CHUNK_SIZE)
    {
        return Err(CatalogError::Corrupt("活动文件准备记录身份或属性长度"));
    }
    Ok((
        batch_id,
        value[17..49].try_into().expect("validated length"),
        manifest,
    ))
}

fn prepared_page_value(batch_id: Uuid, page: &[u8]) -> [u8; PREPARED_PAGE_LEN] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/active-prepared-page/1");
    hash.update(page);
    let mut value = [0; PREPARED_PAGE_LEN];
    value[0] = 1;
    value[1..17].copy_from_slice(batch_id.as_bytes());
    value[17..].copy_from_slice(&hash.finalize());
    value
}

fn finalized_page_value(
    batch_id: Uuid,
    publication: &[u8],
    prepared_page: &[u8],
) -> [u8; PREPARED_PAGE_LEN] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/active-finalized-page/1");
    hash.update(publication);
    hash.update(prepared_page);
    let mut value = [0; PREPARED_PAGE_LEN];
    value[0] = 1;
    value[1..17].copy_from_slice(batch_id.as_bytes());
    value[17..].copy_from_slice(&hash.finalize());
    value
}

fn prepared_batch_value(
    header: Header,
    page_markers: &[[u8; PREPARED_PAGE_LEN]],
    index: active_volume::ActiveIndexIdentity,
) -> [u8; PREPARED_BATCH_LEN] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/active-prepared-batch/1");
    hash.update(header.digest);
    for marker in page_markers {
        hash.update(marker);
    }
    let mut value = [0; PREPARED_BATCH_LEN];
    value[0] = 1;
    value[1..17].copy_from_slice(header.batch_id.as_bytes());
    value[17..49].copy_from_slice(&header.digest);
    value[49..81].copy_from_slice(&hash.finalize());
    value[81..].copy_from_slice(&index.sha256);
    value
}

async fn complete_preparation_at(
    tr: &Transaction,
    volume: Uuid,
    generation: u64,
    header: Header,
    index: active_volume::ActiveIndexIdentity,
) -> Result<Option<[u8; PREPARED_BATCH_LEN]>> {
    let prefix = prepared_page_prefix(volume, generation);
    let rows = read_page(tr, &prefix, None, usize::from(header.pages) + 1, false).await?;
    if rows.next.is_some() || rows.items.len() != usize::from(header.pages) {
        return Ok(None);
    }
    let mut markers = Vec::with_capacity(usize::from(header.pages));
    for (number, (key, value)) in rows.items.into_iter().enumerate() {
        if key != prepared_page_key(volume, generation, number as u16)
            || value.len() != PREPARED_PAGE_LEN
            || value[0] != 1
            || value[1..17] != header.batch_id.as_bytes()[..]
        {
            return Err(CatalogError::Corrupt("活动准备页标记损坏"));
        }
        markers.push(value.try_into().expect("validated length"));
    }
    Ok(Some(prepared_batch_value(header, &markers, index)))
}

pub(super) async fn prepared_attributes_at(
    tr: &Transaction,
    batch_id: Uuid,
    receipt: &CommitReceipt,
) -> Result<Option<NodeAttributes>> {
    let marker = tr
        .get(
            &prepared_key(
                receipt.volume_id,
                receipt.volume_generation,
                receipt.request_epoch,
                receipt.operation_id,
            ),
            false,
        )
        .await?;
    let Some(marker) = marker else {
        return Ok(None);
    };
    let (saved_batch, fingerprint, manifest) = decode_prepared(&marker)?;
    if saved_batch != batch_id || fingerprint.as_slice() != receipt_fingerprint(receipt).as_slice()
    {
        return Err(CatalogError::Corrupt("活动文件准备记录与请求不一致"));
    }
    let attributes = if manifest.len as usize > INLINE_LIMIT {
        attribute_objects::require_complete(tr, receipt.namespace, manifest).await?;
        NodeAttributes::from_manifest(manifest)?
    } else {
        let bytes = tr
            .get(
                &prepared_chunk_key(
                    receipt.volume_id,
                    receipt.volume_generation,
                    receipt.request_epoch,
                    receipt.operation_id,
                    0,
                ),
                false,
            )
            .await?
            .ok_or(CatalogError::Corrupt("活动文件准备属性内联值缺失"))?;
        if bytes.len() != manifest.len as usize {
            return Err(CatalogError::Corrupt("活动文件准备属性长度"));
        }
        NodeAttributes::decode(&bytes)?
    };
    if attributes.digest() != manifest.digest
        || Some(attributes.digest()) != receipt.metadata_digest
    {
        return Err(CatalogError::Corrupt("活动文件准备属性摘要"));
    }
    Ok(Some(attributes))
}

pub(super) fn claim_key(receipt: &CommitReceipt) -> Vec<u8> {
    claim_key_for(
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    )
}

fn claim_key_for(namespace: Uuid, request_epoch: u64, operation_id: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"i/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&request_epoch.to_be_bytes());
    out.extend_from_slice(operation_id.as_bytes());
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TargetLocator {
    pub batch_id: Uuid,
    pub volume: Uuid,
    pub generation: u64,
    pub request_epoch: u64,
    pub operation: Uuid,
    pub file_id: Uuid,
}

impl TargetLocator {
    pub(super) fn from_receipt(batch_id: Uuid, receipt: &CommitReceipt) -> Self {
        Self {
            batch_id,
            volume: receipt.volume_id,
            generation: receipt.volume_generation,
            request_epoch: receipt.request_epoch,
            operation: receipt.operation_id,
            file_id: receipt.file_id,
        }
    }

    fn encode(self) -> [u8; TARGET_LOCATOR_LEN] {
        let mut value = [0; TARGET_LOCATOR_LEN];
        value[0] = 1;
        value[1..17].copy_from_slice(self.batch_id.as_bytes());
        value[17..33].copy_from_slice(self.volume.as_bytes());
        value[33..41].copy_from_slice(&self.generation.to_be_bytes());
        value[41..49].copy_from_slice(&self.request_epoch.to_be_bytes());
        value[49..65].copy_from_slice(self.operation.as_bytes());
        value[65..81].copy_from_slice(self.file_id.as_bytes());
        value
    }

    pub(super) fn decode(value: &[u8]) -> Result<Self> {
        if value.len() != TARGET_LOCATOR_LEN || value[0] != 1 {
            return Err(CatalogError::Corrupt("活动目标定位记录版本或长度"));
        }
        let locator = Self {
            batch_id: uuid_from(&value[1..17])?,
            volume: uuid_from(&value[17..33])?,
            generation: read_u64(&value[33..41]),
            request_epoch: read_u64(&value[41..49]),
            operation: uuid_from(&value[49..65])?,
            file_id: uuid_from(&value[65..81])?,
        };
        if locator.batch_id.is_nil()
            || locator.volume.is_nil()
            || locator.generation == 0
            || locator.request_epoch == 0
            || locator.operation.is_nil()
            || locator.file_id.is_nil()
        {
            return Err(CatalogError::Corrupt("活动目标定位记录身份无效"));
        }
        Ok(locator)
    }

    fn encode_reverse(self) -> [u8; 49] {
        let mut value = [0; 49];
        value[0] = 1;
        value[1..17].copy_from_slice(self.batch_id.as_bytes());
        value[17..25].copy_from_slice(&self.generation.to_be_bytes());
        value[25..33].copy_from_slice(&self.request_epoch.to_be_bytes());
        value[33..49].copy_from_slice(self.operation.as_bytes());
        value
    }

    pub(super) fn decode_reverse(value: &[u8], volume: Uuid, file_id: Uuid) -> Result<Self> {
        if value.len() != 49 || value[0] != 1 {
            return Err(CatalogError::Corrupt("活动卷反向定位记录版本或长度"));
        }
        let locator = Self {
            batch_id: uuid_from(&value[1..17])?,
            volume,
            generation: read_u64(&value[17..25]),
            request_epoch: read_u64(&value[25..33]),
            operation: uuid_from(&value[33..49])?,
            file_id,
        };
        if locator.batch_id.is_nil()
            || locator.volume.is_nil()
            || locator.generation == 0
            || locator.request_epoch == 0
            || locator.operation.is_nil()
            || locator.file_id.is_nil()
        {
            return Err(CatalogError::Corrupt("活动卷反向定位记录身份无效"));
        }
        Ok(locator)
    }
}

pub(super) fn reverse_claim_prefix_for(volume: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"o/");
    out.extend_from_slice(volume.as_bytes());
    out
}

fn reverse_claim_key(receipt: &CommitReceipt) -> Vec<u8> {
    let mut out = reverse_claim_prefix_for(receipt.volume_id);
    out.extend_from_slice(receipt.namespace.as_bytes());
    out.extend_from_slice(receipt.file_id.as_bytes());
    out.extend_from_slice(receipt.copy_id.as_bytes());
    out
}

pub(super) fn path_claim_key_for(namespace: Uuid, parent: Uuid, name: &[u8]) -> Vec<u8> {
    let mut out = path_claim_prefix_for(namespace, parent);
    append_component(&mut out, name);
    out
}

pub(super) fn path_claim_prefix_for(namespace: Uuid, parent: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"l/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(parent.as_bytes());
    out
}

fn path_claim_key(receipt: &CommitReceipt) -> Vec<u8> {
    path_claim_key_for(receipt.namespace, receipt.parent, &receipt.name)
}

pub(super) fn file_claim_key_for(namespace: Uuid, file_id: Uuid) -> Vec<u8> {
    let mut out = key(b'p');
    out.extend_from_slice(b"n/");
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(file_id.as_bytes());
    out
}

fn file_claim_key(receipt: &CommitReceipt) -> Vec<u8> {
    file_claim_key_for(receipt.namespace, receipt.file_id)
}

pub(super) async fn target_claims_match(
    tr: &Transaction,
    receipt: &CommitReceipt,
    batch_id: Uuid,
) -> Result<bool> {
    if tr.get(&claim_key(receipt), false).await?.as_deref() != Some(batch_id.as_bytes()) {
        return Ok(false);
    }
    let expected = TargetLocator::from_receipt(batch_id, receipt);
    for claim in [path_claim_key(receipt), file_claim_key(receipt)] {
        let Some(value) = tr.get(&claim, false).await? else {
            return Ok(false);
        };
        if TargetLocator::decode(&value)? != expected {
            return Ok(false);
        }
    }
    let Some(reverse) = tr.get(&reverse_claim_key(receipt), false).await? else {
        return Ok(false);
    };
    if TargetLocator::decode_reverse(&reverse, receipt.volume_id, receipt.file_id)? != expected {
        return Ok(false);
    }
    Ok(true)
}

pub(super) async fn target_matches_baseline(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<bool> {
    let Some(parent) = tr
        .get(&node_key(receipt.namespace, receipt.parent), false)
        .await?
    else {
        return Ok(false);
    };
    if decode_node(&parent)?.kind != NodeKind::Directory {
        return Ok(false);
    }
    let child = tr
        .get(
            &child_key(receipt.namespace, receipt.parent, &receipt.name),
            false,
        )
        .await?;
    let node = tr
        .get(&node_key(receipt.namespace, receipt.file_id), false)
        .await?;
    let head = tr
        .get(&head_key(receipt.namespace, receipt.file_id), false)
        .await?;
    if tr
        .get(
            &copy_key(receipt.namespace, receipt.file_id, receipt.copy_id),
            false,
        )
        .await?
        .is_some()
    {
        return Ok(false);
    }
    match receipt.expected_version {
        None => Ok(child.is_none() && node.is_none() && head.is_none()),
        Some(expected) => {
            if child.as_deref() != Some(receipt.file_id.as_bytes()) {
                return Ok(false);
            }
            let (Some(node), Some(head)) = (node, head) else {
                return Ok(false);
            };
            let node = decode_node(&node)?;
            let head = decode_head(&head)?;
            Ok(node.kind == NodeKind::File
                && node.parent == receipt.parent
                && node.name == receipt.name
                && head.version == expected)
        }
    }
}

fn page_claim_keys(namespace: Uuid, value: &[u8], expected_items: usize) -> Result<Vec<Vec<u8>>> {
    if value.len() < 2 || value.len() != 2 + 56 * expected_items {
        return Err(CatalogError::Corrupt("活动批次页长度"));
    }
    let count = u16::from_be_bytes([value[0], value[1]]) as usize;
    if count != expected_items {
        return Err(CatalogError::Corrupt("活动批次页项目数"));
    }
    let mut keys = Vec::with_capacity(count);
    let (entries, remainder) = value[2..].as_chunks::<56>();
    debug_assert!(remainder.is_empty());
    for entry in entries {
        let epoch = read_u64(&entry[..8]);
        let operation = uuid_from(&entry[8..24])?;
        if epoch == 0 || operation.is_nil() {
            return Err(CatalogError::Corrupt("活动批次页请求身份"));
        }
        keys.push(claim_key_for(namespace, epoch, operation));
    }
    Ok(keys)
}

fn page_items(header: Header, page: usize) -> usize {
    (header.items as usize - page * PAGE_ITEMS).min(PAGE_ITEMS)
}

fn page_value(receipts: &[CommitReceipt]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + 56 * receipts.len());
    out.extend_from_slice(
        &u16::try_from(receipts.len())
            .map_err(|_| CatalogError::Invalid("活动批次页文件过多"))?
            .to_be_bytes(),
    );
    for receipt in receipts {
        out.extend_from_slice(&receipt.request_epoch.to_be_bytes());
        out.extend_from_slice(receipt.operation_id.as_bytes());
        out.extend_from_slice(&receipt_fingerprint(receipt));
    }
    Ok(out)
}

fn page_digest(page: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/active-batch-page/2");
    hash.update((page.len() as u32).to_be_bytes());
    hash.update(page);
    hash.finalize().into()
}

fn manifest_digest(pages: &[[u8; 32]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/active-batch/2");
    for page in pages {
        hash.update(page);
    }
    hash.finalize().into()
}

fn next_generation(current: ActiveIndexIdentity, target: u64) -> bool {
    current.generation.checked_add(1) == Some(target)
}

/// Called inside the volume-index CAS transaction. A sealed header was only
/// created after every page and request claim passed the manifest check.
pub(super) async fn sealed_for_index(
    tr: &Transaction,
    namespace: Uuid,
    volume: Uuid,
    generation: u64,
    batch_id: Uuid,
    owner_epoch: u64,
    scan_id: Uuid,
) -> Result<bool> {
    let Some(value) = tr.get(&batch_key(volume, generation), false).await? else {
        return Ok(false);
    };
    let header = Header::decode(&value)?;
    Ok(header.phase == ActiveBatchPhase::Sealed
        && header.namespace == namespace
        && header.batch_id == batch_id
        && header.owner_epoch <= owner_epoch
        && header.scan_id == scan_id)
}

async fn abort_header(
    tr: &Transaction,
    serial: &str,
    volume: Uuid,
    generation: u64,
    batch_id: Uuid,
    owner_epoch: u64,
) -> Result<std::result::Result<Header, ActiveBatchOutcome>> {
    let namespace = routing::require_active_namespace(tr, serial).await?;
    let placement = control::verified_placement(tr, volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.namespace != namespace || placement.library_serial != serial {
        return Err(CatalogError::Invalid("活动批次与带库放置不一致"));
    }
    if !routing::require_active_owner(tr, namespace, serial, owner_epoch).await? {
        return Ok(Err(ActiveBatchOutcome::StaleOwner));
    }
    let Some(value) = tr.get(&batch_key(volume, generation), false).await? else {
        return Ok(Err(ActiveBatchOutcome::AlreadyAborted));
    };
    let header = Header::decode(&value)?;
    if header.namespace != namespace || header.batch_id != batch_id {
        return Ok(Err(ActiveBatchOutcome::BatchConflict));
    }
    let (scan, current) = active_volume::observed_at(tr, serial, volume).await?;
    if scan.id != header.scan_id || current.generation >= generation {
        return Ok(Err(ActiveBatchOutcome::GenerationConflict));
    }
    Ok(Ok(header))
}

impl ManagedCatalog {
    /// Finish the invisible preparation only when every bounded page has
    /// already been closed. The small marker is a future publication barrier
    /// prerequisite; writing it does not expose any file or finish any task.
    pub async fn seal_observed_active_preparation(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
    ) -> Result<ActivePreparationOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动准备批次身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActivePreparationOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let Some((observed_batch, index)) =
                                active_volume::observed_batch_at(&tr, &serial, volume, generation)
                                    .await?
                            else {
                                return Ok(ActivePreparationOutcome::IndexNotObserved);
                            };
                            if observed_batch != batch_id {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let Some(marker) =
                                complete_preparation_at(&tr, volume, generation, header, index)
                                    .await?
                            else {
                                return Ok(ActivePreparationOutcome::PageIncomplete);
                            };
                            let marker_key = prepared_batch_key(volume, generation);
                            if let Some(existing) = tr.get(&marker_key, false).await? {
                                return Ok(if existing.as_ref() == marker {
                                    ActivePreparationOutcome::AlreadyPrepared
                                } else {
                                    ActivePreparationOutcome::BatchConflict
                                });
                            }
                            tr.set(&marker_key, &marker);
                            Ok(ActivePreparationOutcome::Prepared)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Atomically expose every prepared file in a physically observed batch.
    /// The caller must reverify media identity and hold the device PR before
    /// calling this; a recorded index alone is not fresh device evidence.
    /// Request terminal rows and permits still require bounded projection.
    pub async fn publish_observed_active_batch(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
    ) -> Result<ActivePublicationOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动发布批次身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActivePublicationOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActivePublicationOutcome::BatchConflict);
                            }
                            if let Some(done) = tr
                                .get(&compacted_batch_key(volume, generation), false)
                                .await?
                            {
                                let (header, _) = decode_compacted_batch(&done)?;
                                return Ok(
                                    if header.batch_id == batch_id
                                        && header.namespace == namespace
                                        && header.owner_epoch <= owner_epoch
                                    {
                                        ActivePublicationOutcome::AlreadyPublished
                                    } else {
                                        ActivePublicationOutcome::BatchConflict
                                    },
                                );
                            }
                            let raw = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&raw)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                            {
                                return Ok(ActivePublicationOutcome::BatchConflict);
                            }
                            let (scan, _) =
                                active_volume::observed_at(&tr, &serial, volume).await?;
                            if scan.id != header.scan_id {
                                return Ok(ActivePublicationOutcome::BatchConflict);
                            }
                            let Some((observed_batch, index)) =
                                active_volume::observed_batch_at(&tr, &serial, volume, generation)
                                    .await?
                            else {
                                return Ok(ActivePublicationOutcome::IndexNotObserved);
                            };
                            if observed_batch != batch_id {
                                return Ok(ActivePublicationOutcome::BatchConflict);
                            }
                            let Some(expected) =
                                complete_preparation_at(&tr, volume, generation, header, index)
                                    .await?
                            else {
                                return Ok(ActivePublicationOutcome::PageIncomplete);
                            };
                            let prepared = tr
                                .get(&prepared_batch_key(volume, generation), false)
                                .await?;
                            if prepared.as_deref() != Some(expected.as_slice()) {
                                return Ok(ActivePublicationOutcome::PageIncomplete);
                            }
                            let key = publication_key(volume, generation);
                            if let Some(existing) = tr.get(&key, false).await? {
                                return Ok(if existing.as_ref() == expected {
                                    ActivePublicationOutcome::AlreadyPublished
                                } else {
                                    ActivePublicationOutcome::BatchConflict
                                });
                            }
                            tr.set(&key, &expected);
                            Ok(ActivePublicationOutcome::Published)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Project one published page to durable request outcomes and release its
    /// admission permits. The page marker, terminal rows and quota changes are
    /// one FDB transaction, so an unknown commit result can be retried safely.
    /// Admission intents remain pinned for published-file proof until a later
    /// materialization protocol replaces those references.
    pub async fn finalize_published_active_batch_page(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<ActiveFinalizationOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动终结批次身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActiveFinalizationOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActiveFinalizationOutcome::BatchConflict);
                            }
                            if let Some(done) = tr
                                .get(&compacted_batch_key(volume, generation), false)
                                .await?
                            {
                                let (header, _) = decode_compacted_batch(&done)?;
                                return Ok(
                                    if header.batch_id == batch_id
                                        && header.namespace == namespace
                                        && header.owner_epoch <= owner_epoch
                                        && usize::from(page) < usize::from(header.pages)
                                    {
                                        ActiveFinalizationOutcome::AlreadyFinalized
                                    } else {
                                        ActiveFinalizationOutcome::BatchConflict
                                    },
                                );
                            }
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                                || usize::from(page) >= usize::from(header.pages)
                            {
                                return Ok(ActiveFinalizationOutcome::BatchConflict);
                            }
                            let (scan, _) =
                                active_volume::observed_at(&tr, &serial, volume).await?;
                            if scan.id != header.scan_id {
                                return Ok(ActiveFinalizationOutcome::BatchConflict);
                            }
                            let Some((observed_batch, index)) =
                                active_volume::observed_batch_at(&tr, &serial, volume, generation)
                                    .await?
                            else {
                                return Ok(ActiveFinalizationOutcome::Unpublished);
                            };
                            if observed_batch != batch_id {
                                return Ok(ActiveFinalizationOutcome::BatchConflict);
                            }
                            let Some(publication) =
                                tr.get(&publication_key(volume, generation), false).await?
                            else {
                                return Ok(ActiveFinalizationOutcome::Unpublished);
                            };
                            let prepared_batch = tr
                                .get(&prepared_batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动发布缺少整批准备证明"))?;
                            if publication.len() != PREPARED_BATCH_LEN
                                || publication[0] != 1
                                || publication[1..17] != batch_id.as_bytes()[..]
                                || publication[81..] != index.sha256
                                || publication.as_ref() != prepared_batch.as_ref()
                            {
                                return Err(CatalogError::Corrupt("活动发布证明与物理索引不一致"));
                            }
                            let raw = tr
                                .get(&page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动终结批次页缺失"))?;
                            if tr
                                .get(&page_digest_key(volume, generation, page), false)
                                .await?
                                .as_deref()
                                != Some(page_digest(&raw).as_slice())
                            {
                                return Err(CatalogError::Corrupt("活动终结批次页摘要不匹配"));
                            }
                            let page_proof = tr
                                .get(&prepared_page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动终结缺少准备页证明"))?;
                            if page_proof.as_ref() != prepared_page_value(batch_id, &raw) {
                                return Err(CatalogError::Corrupt("活动终结准备页证明不一致"));
                            }
                            let finalized =
                                finalized_page_value(batch_id, &publication, &page_proof);
                            let finalized_key = finalized_page_key(volume, generation, page);
                            if let Some(existing) = tr.get(&finalized_key, false).await? {
                                return Ok(if existing.as_ref() == finalized {
                                    ActiveFinalizationOutcome::AlreadyFinalized
                                } else {
                                    ActiveFinalizationOutcome::BatchConflict
                                });
                            }
                            let claims = page_claim_keys(
                                namespace,
                                &raw,
                                page_items(header, usize::from(page)),
                            )?;
                            let (entries, remainder) = raw[2..].as_chunks::<56>();
                            debug_assert!(remainder.is_empty());
                            let mut terminals = Vec::with_capacity(entries.len());
                            let mut permits = BTreeMap::<u16, u64>::new();
                            for (entry, claim) in entries.iter().zip(claims) {
                                let epoch = read_u64(&entry[..8]);
                                let operation = uuid_from(&entry[8..24])?;
                                if tr.get(&claim, false).await?.as_deref()
                                    != Some(batch_id.as_bytes())
                                {
                                    return Err(CatalogError::Corrupt("活动终结请求占用不一致"));
                                }
                                let mut intent_key = requests::epoch_prefix(b'I', namespace, epoch);
                                intent_key.extend_from_slice(operation.as_bytes());
                                let intent = tr
                                    .get(&intent_key, false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("活动终结准入意图缺失"))?;
                                let receipt =
                                    requests::decode_intent(&intent, namespace, epoch, operation)?;
                                if receipt_fingerprint(&receipt) != entry[24..]
                                    || receipt.library_serial != serial
                                    || receipt.volume_id != volume
                                    || receipt.volume_generation != generation
                                    || receipt.volume_scan_id != header.scan_id
                                    || receipt.owner_epoch != header.owner_epoch
                                    || !target_claims_match(&tr, &receipt, batch_id).await?
                                {
                                    return Err(CatalogError::Corrupt("活动终结收据与批次不一致"));
                                }
                                let prepared = tr
                                    .get(&prepared_key(volume, generation, epoch, operation), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("活动终结文件准备缺失"))?;
                                let (saved_batch, fingerprint, _) = decode_prepared(&prepared)?;
                                if saved_batch != batch_id || fingerprint != entry[24..] {
                                    return Err(CatalogError::Corrupt("活动终结文件准备不一致"));
                                }
                                let permit = requests::matching_permit(&intent, &receipt)?
                                    .ok_or(CatalogError::Corrupt("活动终结准入许可不一致"))?;
                                *permits.entry(permit).or_default() += 1;
                                let op = op_key(namespace, epoch, operation);
                                if tr.get(&op, false).await?.is_some() {
                                    return Err(CatalogError::Corrupt("活动终结前已有请求结果"));
                                }
                                terminals.push((
                                    op,
                                    requests::terminal_value(
                                        &receipt_fingerprint(&receipt),
                                        Some(receipt.expected_version.unwrap_or(0) + 1),
                                    ),
                                ));
                            }
                            let mut releases = Vec::with_capacity(permits.len());
                            for (permit, count) in permits {
                                releases.push(
                                    maintenance::release_many(&tr, namespace, permit, count)
                                        .await?,
                                );
                            }
                            for (key, value) in terminals {
                                tr.set(&key, &value);
                            }
                            for release in releases {
                                release.apply(&tr);
                            }
                            tr.set(&finalized_key, &finalized);
                            Ok(ActiveFinalizationOutcome::Finalized)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Move one finalized candidate into the ordinary inode/copy index. The
    /// candidate remains visible until its target claims are removed in the
    /// same transaction as the baseline rows, so readers see one complete
    /// version on either side of the commit. The small p/m receipt handles an
    /// unknown FDB commit result without retaining the large prepared attrs.
    pub async fn materialize_published_active_file(
        &self,
        serial: &str,
        owner_epoch: u64,
        batch_id: Uuid,
        page: u16,
        receipt: &CommitReceipt,
    ) -> Result<ActiveMaterializationOutcome> {
        control::validate_physical_serial(serial)?;
        validate_receipt(receipt)?;
        if owner_epoch == 0 || batch_id.is_nil() || serial != receipt.library_serial {
            return Err(CatalogError::Invalid("活动归并身份无效"));
        }
        let version = receipt
            .expected_version
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(CatalogError::Invalid("文件版本溢出"))?;
        let fingerprint = receipt_fingerprint(receipt);
        let marker = materialized_value(batch_id, &fingerprint, version);
        let receipt = receipt.clone();
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let receipt = receipt.clone();
                let serial = serial.clone();
                let fingerprint = fingerprint.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if namespace != receipt.namespace {
                                return Ok(ActiveMaterializationOutcome::BatchConflict);
                            }
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActiveMaterializationOutcome::StaleOwner);
                            }
                            let result = tr
                                .get(
                                    &op_key(namespace, receipt.request_epoch, receipt.operation_id),
                                    false,
                                )
                                .await?;
                            let Some(result) = result else {
                                return Ok(ActiveMaterializationOutcome::NotFinalized);
                            };
                            if requests::terminal_result(&result, &fingerprint)?
                                != PublishOutcome::AlreadyApplied(version)
                            {
                                return Ok(ActiveMaterializationOutcome::BatchConflict);
                            }
                            if let Some(done) = tr
                                .get(
                                    &compacted_batch_key(
                                        receipt.volume_id,
                                        receipt.volume_generation,
                                    ),
                                    false,
                                )
                                .await?
                            {
                                let (header, _) = decode_compacted_batch(&done)?;
                                return Ok(
                                    if header.batch_id == batch_id && header.namespace == namespace
                                    {
                                        ActiveMaterializationOutcome::AlreadyMaterialized
                                    } else {
                                        ActiveMaterializationOutcome::BatchConflict
                                    },
                                );
                            }
                            let done_key = materialized_key(
                                namespace,
                                receipt.request_epoch,
                                receipt.operation_id,
                            );
                            if let Some(saved) = tr.get(&done_key, false).await? {
                                return Ok(if saved.as_ref() == marker {
                                    ActiveMaterializationOutcome::AlreadyMaterialized
                                } else {
                                    ActiveMaterializationOutcome::BatchConflict
                                });
                            }
                            let volume = receipt.volume_id;
                            let generation = receipt.volume_generation;
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                                || header.scan_id != receipt.volume_scan_id
                                || usize::from(page) >= usize::from(header.pages)
                            {
                                return Ok(ActiveMaterializationOutcome::BatchConflict);
                            }
                            let publication = tr
                                .get(&publication_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let prepared_batch = tr
                                .get(&prepared_batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并缺少整批准备证明"))?;
                            if publication.as_ref() != prepared_batch.as_ref() {
                                return Err(CatalogError::Corrupt("活动归并发布证明不一致"));
                            }
                            let raw = tr
                                .get(&page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并批次页缺失"))?;
                            if tr
                                .get(&page_digest_key(volume, generation, page), false)
                                .await?
                                .as_deref()
                                != Some(page_digest(&raw).as_slice())
                            {
                                return Err(CatalogError::Corrupt("活动归并批次页摘要不匹配"));
                            }
                            let page_proof = tr
                                .get(&prepared_page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并缺少准备页证明"))?;
                            if page_proof.as_ref() != prepared_page_value(batch_id, &raw) {
                                return Err(CatalogError::Corrupt("活动归并准备页证明不一致"));
                            }
                            let finalized = tr
                                .get(&finalized_page_key(volume, generation, page), false)
                                .await?;
                            if finalized.as_deref()
                                != Some(
                                    finalized_page_value(batch_id, &publication, &page_proof)
                                        .as_slice(),
                                )
                            {
                                return Ok(ActiveMaterializationOutcome::NotFinalized);
                            }
                            page_claim_keys(
                                namespace,
                                &raw,
                                page_items(header, usize::from(page)),
                            )?;
                            let (entries, remainder) = raw[2..].as_chunks::<56>();
                            if !remainder.is_empty()
                                || entries.len() != page_items(header, usize::from(page))
                                || !entries.iter().any(|entry| {
                                    entry[..8] == receipt.request_epoch.to_be_bytes()
                                        && entry[8..24] == receipt.operation_id.as_bytes()[..]
                                        && entry[24..] == fingerprint
                                })
                            {
                                return Err(CatalogError::Corrupt("活动归并请求不在已终结页"));
                            }
                            if let Some(compacted) = tr
                                .get(&compacted_page_key(volume, generation, page), false)
                                .await?
                            {
                                return Ok(
                                    if compacted.as_ref()
                                        == finalized_page_value(batch_id, &publication, &page_proof)
                                    {
                                        ActiveMaterializationOutcome::AlreadyMaterialized
                                    } else {
                                        ActiveMaterializationOutcome::BatchConflict
                                    },
                                );
                            }
                            let locator = TargetLocator::from_receipt(batch_id, &receipt);
                            let Some((published, placement)) =
                                lookup::published_receipt_at(&tr, namespace, locator).await?
                            else {
                                return Err(CatalogError::Corrupt("活动归并缺少已发布目标"));
                            };
                            let mut published = published;
                            published.owner_epoch = receipt.owner_epoch;
                            if published != receipt
                                || placement.volume_id != volume
                                || !target_matches_baseline(&tr, &receipt).await?
                            {
                                return Ok(ActiveMaterializationOutcome::BatchConflict);
                            }
                            let attributes = prepared_attributes_at(&tr, batch_id, &receipt)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并缺少文件属性"))?;
                            if Some(attributes.digest()) != receipt.metadata_digest {
                                return Err(CatalogError::Corrupt("活动归并属性摘要不一致"));
                            }
                            let inode_key = node_key(namespace, receipt.file_id);
                            let old_external = tr
                                .get(&inode_key, false)
                                .await?
                                .as_deref()
                                .map(decode_node)
                                .transpose()?
                                .and_then(|node| node.attributes)
                                .is_some_and(|attrs| attrs.is_external());
                            write_attribute_chunks(
                                &tr,
                                namespace,
                                receipt.file_id,
                                old_external,
                                Some(&attributes),
                            )
                            .await?;
                            tr.set(
                                &child_key(namespace, receipt.parent, &receipt.name),
                                receipt.file_id.as_bytes(),
                            );
                            tr.set(
                                &inode_key,
                                &encode_node(&Node {
                                    kind: NodeKind::File,
                                    parent: receipt.parent,
                                    name: receipt.name.clone(),
                                    attributes: Some(attributes),
                                }),
                            );
                            tr.set(
                                &head_key(namespace, receipt.file_id),
                                &encode_head(&FileHead {
                                    version,
                                    size: receipt.size,
                                    copy_id: receipt.copy_id,
                                }),
                            );
                            tr.set(
                                &copy_key(namespace, receipt.file_id, receipt.copy_id),
                                &encode_copy(&receipt, version),
                            );
                            let mut volume_value = version.to_be_bytes().to_vec();
                            volume_value.extend_from_slice(&fingerprint);
                            tr.set(
                                &volume_key(
                                    volume,
                                    receipt.volume_scan_id,
                                    namespace,
                                    receipt.file_id,
                                    receipt.copy_id,
                                ),
                                &volume_value,
                            );
                            tr.clear(&claim_key(&receipt));
                            tr.clear(&path_claim_key(&receipt));
                            tr.clear(&file_claim_key(&receipt));
                            tr.clear(&reverse_claim_key(&receipt));
                            tr.clear(&requests::intent_key(&receipt));
                            tr.clear(&reservation::upgraded_key_for(&receipt));
                            tr.clear(&prepared_key(
                                volume,
                                generation,
                                receipt.request_epoch,
                                receipt.operation_id,
                            ));
                            let chunks = prepared_chunk_prefix(
                                volume,
                                generation,
                                receipt.request_epoch,
                                receipt.operation_id,
                            );
                            tr.clear_range(&chunks, &prefix_end(&chunks));
                            tr.set(&done_key, &marker);
                            Ok(ActiveMaterializationOutcome::Materialized)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Replace a finalized page's per-file completion receipts with one page
    /// marker. The page marker is written only after every file's atomic
    /// baseline switch has committed; an incomplete page leaves all receipts.
    pub async fn finish_materialized_active_batch_page(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<ActiveCompactionOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动归并页身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActiveCompactionOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                                || usize::from(page) >= usize::from(header.pages)
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let publication = tr
                                .get(&publication_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if tr
                                .get(&prepared_batch_key(volume, generation), false)
                                .await?
                                .as_deref()
                                != Some(publication.as_ref())
                            {
                                return Err(CatalogError::Corrupt("活动归并页发布证明不一致"));
                            }
                            let raw = tr
                                .get(&page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并页原清单缺失"))?;
                            if tr
                                .get(&page_digest_key(volume, generation, page), false)
                                .await?
                                .as_deref()
                                != Some(page_digest(&raw).as_slice())
                            {
                                return Err(CatalogError::Corrupt("活动归并页摘要不一致"));
                            }
                            let page_proof = tr
                                .get(&prepared_page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动归并页准备证明缺失"))?;
                            if page_proof.as_ref() != prepared_page_value(batch_id, &raw) {
                                return Err(CatalogError::Corrupt("活动归并页准备证明不一致"));
                            }
                            let finalized =
                                finalized_page_value(batch_id, &publication, &page_proof);
                            if tr
                                .get(&finalized_page_key(volume, generation, page), false)
                                .await?
                                .as_deref()
                                != Some(finalized.as_slice())
                            {
                                return Ok(ActiveCompactionOutcome::Incomplete);
                            }
                            let done_key = compacted_page_key(volume, generation, page);
                            if let Some(saved) = tr.get(&done_key, false).await? {
                                return Ok(if saved.as_ref() == finalized {
                                    ActiveCompactionOutcome::AlreadyCompleted
                                } else {
                                    ActiveCompactionOutcome::BatchConflict
                                });
                            }
                            page_claim_keys(
                                namespace,
                                &raw,
                                page_items(header, usize::from(page)),
                            )?;
                            let (entries, remainder) = raw[2..].as_chunks::<56>();
                            debug_assert!(remainder.is_empty());
                            let mut completed = Vec::with_capacity(entries.len());
                            for entry in entries {
                                let epoch = read_u64(&entry[..8]);
                                let operation = uuid_from(&entry[8..24])?;
                                let key = materialized_key(namespace, epoch, operation);
                                let Some(marker) = tr.get(&key, false).await? else {
                                    return Ok(ActiveCompactionOutcome::Incomplete);
                                };
                                if marker.len() != 57
                                    || marker[0] != 1
                                    || marker[1..17] != batch_id.as_bytes()[..]
                                    || marker[17..49] != entry[24..]
                                {
                                    return Err(CatalogError::Corrupt("活动归并页文件标记不一致"));
                                }
                                let version = read_u64(&marker[49..57]);
                                if version == 0 {
                                    return Err(CatalogError::Corrupt("活动归并页文件版本无效"));
                                }
                                let terminal = tr
                                    .get(&op_key(namespace, epoch, operation), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("归并页文件终态缺失"))?;
                                if requests::terminal_result(&terminal, &entry[24..])?
                                    != PublishOutcome::AlreadyApplied(version)
                                {
                                    return Err(CatalogError::Corrupt("归并页文件终态不一致"));
                                }
                                completed.push(key);
                            }
                            for key in completed {
                                tr.clear(&key);
                            }
                            tr.set(&done_key, &finalized);
                            Ok(ActiveCompactionOutcome::Completed)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Once every page has a completion marker, retire the candidate barrier
    /// atomically. All files are already in the ordinary catalog. The compact
    /// batch receipt survives later bounded page cleanup and unknown commits.
    pub async fn seal_materialized_active_batch(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
    ) -> Result<ActiveCompactionOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动归并整批身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActiveCompactionOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let done_key = compacted_batch_key(volume, generation);
                            if let Some(saved) = tr.get(&done_key, false).await? {
                                let (header, _) = decode_compacted_batch(&saved)?;
                                return Ok(
                                    if header.batch_id == batch_id && header.namespace == namespace
                                    {
                                        ActiveCompactionOutcome::AlreadyCompleted
                                    } else {
                                        ActiveCompactionOutcome::BatchConflict
                                    },
                                );
                            }
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let publication = tr
                                .get(&publication_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if publication.len() != PREPARED_BATCH_LEN
                                || publication[0] != 1
                                || publication[1..17] != batch_id.as_bytes()[..]
                                || tr
                                    .get(&prepared_batch_key(volume, generation), false)
                                    .await?
                                    .as_deref()
                                    != Some(publication.as_ref())
                            {
                                return Err(CatalogError::Corrupt("活动整批归并发布证明不一致"));
                            }
                            let pages = usize::from(header.pages);
                            let compacted = read_page(
                                &tr,
                                &compacted_page_prefix(volume, generation),
                                None,
                                pages + 1,
                                false,
                            )
                            .await?;
                            let finalized = read_page(
                                &tr,
                                &finalized_page_prefix(volume, generation),
                                None,
                                pages + 1,
                                false,
                            )
                            .await?;
                            if compacted.next.is_some()
                                || finalized.next.is_some()
                                || compacted.items.len() != pages
                                || finalized.items.len() != pages
                            {
                                return Ok(ActiveCompactionOutcome::Incomplete);
                            }
                            for (
                                number,
                                (
                                    (compacted_key, compacted_value),
                                    (finalized_key, finalized_value),
                                ),
                            ) in compacted
                                .items
                                .iter()
                                .zip(finalized.items.iter())
                                .enumerate()
                            {
                                if compacted_key
                                    != &compacted_page_key(volume, generation, number as u16)
                                    || finalized_key
                                        != &finalized_page_key(volume, generation, number as u16)
                                    || compacted_value != finalized_value
                                    || compacted_value.len() != PREPARED_PAGE_LEN
                                    || compacted_value[0] != 1
                                    || compacted_value[1..17] != batch_id.as_bytes()[..]
                                {
                                    return Err(CatalogError::Corrupt("活动归并页完成证明不一致"));
                                }
                            }
                            tr.set(&done_key, &compacted_batch_value(header, &publication));
                            tr.clear(&publication_key(volume, generation));
                            tr.clear(&prepared_batch_key(volume, generation));
                            tr.clear(&batch_key(volume, generation));
                            Ok(ActiveCompactionOutcome::Completed)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Reclaim one retired batch page after the batch completion receipt has
    /// replaced the publication barrier. Each page deletion is independently
    /// retryable and never touches the ordinary file rows.
    pub async fn cleanup_materialized_active_batch_page(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<ActiveCompactionOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动归并清理页身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActiveCompactionOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let done = tr
                                .get(&compacted_batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let (header, publication) = decode_compacted_batch(&done)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || usize::from(page) >= usize::from(header.pages)
                            {
                                return Ok(ActiveCompactionOutcome::BatchConflict);
                            }
                            let page_key = page_key(volume, generation, page);
                            let digest_key = page_digest_key(volume, generation, page);
                            let prepared_key = prepared_page_key(volume, generation, page);
                            let finalized_key = finalized_page_key(volume, generation, page);
                            let compacted_key = compacted_page_key(volume, generation, page);
                            let raw = tr.get(&page_key, false).await?;
                            let digest = tr.get(&digest_key, false).await?;
                            let prepared = tr.get(&prepared_key, false).await?;
                            let finalized = tr.get(&finalized_key, false).await?;
                            let compacted = tr.get(&compacted_key, false).await?;
                            let Some(compacted) = compacted else {
                                return if raw.is_none()
                                    && digest.is_none()
                                    && prepared.is_none()
                                    && finalized.is_none()
                                {
                                    Ok(ActiveCompactionOutcome::AlreadyCompleted)
                                } else {
                                    Err(CatalogError::Corrupt("活动归并清理页部分缺失"))
                                };
                            };
                            let (Some(raw), Some(digest), Some(prepared), Some(finalized)) =
                                (raw, digest, prepared, finalized)
                            else {
                                return Err(CatalogError::Corrupt("活动归并清理页证明缺失"));
                            };
                            page_claim_keys(
                                namespace,
                                &raw,
                                page_items(header, usize::from(page)),
                            )?;
                            if digest.as_ref() != page_digest(&raw)
                                || prepared.as_ref() != prepared_page_value(batch_id, &raw)
                                || finalized.as_ref()
                                    != finalized_page_value(batch_id, publication, &prepared)
                                || compacted.as_ref() != finalized.as_ref()
                            {
                                return Err(CatalogError::Corrupt("活动归并清理页证明不一致"));
                            }
                            for key in [
                                page_key,
                                digest_key,
                                prepared_key,
                                finalized_key,
                                compacted_key,
                            ] {
                                tr.clear(&key);
                            }
                            Ok(ActiveCompactionOutcome::Completed)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Rebuild the exact prepared LTFS attributes after takeover. A missing
    /// item is still pending preparation; damaged chunks fail closed.
    pub async fn recover_observed_active_prepared_file(
        &self,
        serial: &str,
        owner_epoch: u64,
        batch_id: Uuid,
        receipt: &CommitReceipt,
    ) -> Result<Option<NodeAttributes>> {
        control::validate_physical_serial(serial)?;
        validate_receipt(receipt)?;
        if owner_epoch == 0 || batch_id.is_nil() || serial != receipt.library_serial {
            return Err(CatalogError::Invalid("活动文件恢复身份无效"));
        }
        let tr = self.db.create_trx()?;
        if !routing::require_active_owner(&tr, receipt.namespace, serial, owner_epoch).await? {
            return Err(CatalogError::MaintenanceFenced);
        }
        let placement = control::verified_placement(&tr, receipt.volume_id)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != receipt.namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("活动文件与卷放置不一致"));
        }
        if !sealed_for_index(
            &tr,
            receipt.namespace,
            receipt.volume_id,
            receipt.volume_generation,
            batch_id,
            owner_epoch,
            receipt.volume_scan_id,
        )
        .await?
        {
            return Err(CatalogError::Invalid("活动文件与封存批次不一致"));
        }
        let Some((observed_batch, _)) = active_volume::observed_batch_at(
            &tr,
            serial,
            receipt.volume_id,
            receipt.volume_generation,
        )
        .await?
        else {
            return Err(CatalogError::Incomplete);
        };
        if observed_batch != batch_id || !target_claims_match(&tr, receipt, batch_id).await? {
            return Err(CatalogError::Corrupt("活动文件批次索引或占用不一致"));
        }
        let intent = tr
            .get(&requests::intent_key(receipt), false)
            .await?
            .ok_or(CatalogError::Corrupt("活动文件准入意图缺失"))?;
        if requests::matching_permit(&intent, receipt)?.is_none() {
            return Err(CatalogError::Corrupt("活动文件准入意图不一致"));
        }
        let attributes = prepared_attributes_at(&tr, batch_id, receipt).await?;
        drop(tr);
        match attributes {
            Some(attributes) => self
                .hydrate_attributes(receipt.namespace, attributes)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Persist one file's exact LTFS attributes after the sealed batch and
    /// committed index have been observed. This writes only private prepare
    /// keys; it does not change directory visibility or finish the request.
    pub async fn prepare_observed_active_file(
        &self,
        serial: &str,
        owner_epoch: u64,
        batch_id: Uuid,
        receipt: &CommitReceipt,
        attributes: &NodeAttributes,
    ) -> Result<ActivePreparationOutcome> {
        control::validate_physical_serial(serial)?;
        validate_receipt(receipt)?;
        if owner_epoch == 0 || batch_id.is_nil() || serial != receipt.library_serial {
            return Err(CatalogError::Invalid("活动文件准备身份无效"));
        }
        if receipt.metadata_digest != Some(attributes.digest()) {
            return Err(CatalogError::Invalid("收据与 LTFS 节点属性摘要不匹配"));
        }
        let bytes = attributes
            .bytes()
            .ok_or(CatalogError::Invalid("不能准备未读取的 LTFS 属性"))?;
        if attributes.symlink().is_none()
            && (attributes
                .meta()
                .ok_or(CatalogError::Invalid("不能准备未读取的 LTFS 属性"))?
                .file_uid
                != receipt.ltfs_file_uid
                || ContentDigest::from_ltfs_xattrs(
                    attributes
                        .xattrs()
                        .ok_or(CatalogError::Invalid("不能准备未读取的 LTFS 属性"))?,
                )? != receipt.content_digest)
        {
            return Err(CatalogError::Invalid("LTFS 文件属性与副本身份不一致"));
        }
        let fingerprint = receipt_fingerprint(receipt);
        let prepared = prepared_value(batch_id, &fingerprint, attributes);
        self.upload_attributes(receipt.namespace, attributes)
            .await?;
        let inline = (bytes.len() <= INLINE_LIMIT).then(|| bytes.to_vec());
        let manifest = attributes.manifest();
        let item_key = prepared_key(
            receipt.volume_id,
            receipt.volume_generation,
            receipt.request_epoch,
            receipt.operation_id,
        );
        let serial = serial.to_owned();
        let receipt = receipt.clone();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                let receipt = receipt.clone();
                let item_key = item_key.clone();
                let prepared = prepared.clone();
                let inline = inline.clone();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                receipt.namespace,
                                &serial,
                                owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActivePreparationOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, receipt.volume_id)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != receipt.namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            if !sealed_for_index(
                                &tr,
                                receipt.namespace,
                                receipt.volume_id,
                                receipt.volume_generation,
                                batch_id,
                                owner_epoch,
                                receipt.volume_scan_id,
                            )
                            .await?
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let Some((observed_batch, _)) = active_volume::observed_batch_at(
                                &tr,
                                &serial,
                                receipt.volume_id,
                                receipt.volume_generation,
                            )
                            .await?
                            else {
                                return Ok(ActivePreparationOutcome::IndexNotObserved);
                            };
                            if observed_batch != batch_id {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            if !target_claims_match(&tr, &receipt, batch_id).await? {
                                return Ok(ActivePreparationOutcome::RequestConflict);
                            }
                            let intent = tr.get(&requests::intent_key(&receipt), false).await?;
                            let Some(intent) = intent else {
                                return Ok(ActivePreparationOutcome::RequestConflict);
                            };
                            if requests::matching_permit(&intent, &receipt)?.is_none()
                                || tr
                                    .get(
                                        &op_key(
                                            receipt.namespace,
                                            receipt.request_epoch,
                                            receipt.operation_id,
                                        ),
                                        false,
                                    )
                                    .await?
                                    .is_some()
                            {
                                return Ok(ActivePreparationOutcome::RequestConflict);
                            }
                            if let Some(existing) = tr.get(&item_key, false).await? {
                                decode_prepared(&existing)?;
                                return Ok(if existing.as_ref() == prepared.as_slice() {
                                    ActivePreparationOutcome::AlreadyPrepared
                                } else {
                                    ActivePreparationOutcome::BatchConflict
                                });
                            }
                            if let Some(manifest) = manifest {
                                attribute_objects::require_complete(
                                    &tr,
                                    receipt.namespace,
                                    manifest,
                                )
                                .await?;
                            } else if let Some(inline) = inline {
                                let key = prepared_chunk_key(
                                    receipt.volume_id,
                                    receipt.volume_generation,
                                    receipt.request_epoch,
                                    receipt.operation_id,
                                    0,
                                );
                                if tr.get(&key, false).await?.is_some() {
                                    return Err(CatalogError::Corrupt("活动文件属性分块孤儿"));
                                }
                                tr.set(&key, &inline);
                            }
                            tr.set(&item_key, &prepared);
                            Ok(ActivePreparationOutcome::Prepared)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Close a bounded preparation page only after every original request has
    /// a matching durable file-attribute record. A future batch publication
    /// barrier can verify these small page markers in one transaction.
    pub async fn mark_observed_active_batch_page_prepared(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<ActivePreparationOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动准备页身份无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let namespace = routing::require_active_namespace(&tr, &serial).await?;
                            if !routing::require_active_owner(&tr, namespace, &serial, owner_epoch)
                                .await?
                            {
                                return Ok(ActivePreparationOutcome::StaleOwner);
                            }
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let header = tr
                                .get(&batch_key(volume, generation), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let header = Header::decode(&header)?;
                            if header.batch_id != batch_id
                                || header.namespace != namespace
                                || header.phase != ActiveBatchPhase::Sealed
                                || header.owner_epoch > owner_epoch
                                || usize::from(page) >= usize::from(header.pages)
                            {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let Some((observed_batch, _)) =
                                active_volume::observed_batch_at(&tr, &serial, volume, generation)
                                    .await?
                            else {
                                return Ok(ActivePreparationOutcome::IndexNotObserved);
                            };
                            if observed_batch != batch_id {
                                return Ok(ActivePreparationOutcome::BatchConflict);
                            }
                            let raw = tr
                                .get(&page_key(volume, generation, page), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动批次页缺失"))?;
                            if tr
                                .get(&page_digest_key(volume, generation, page), false)
                                .await?
                                .as_deref()
                                != Some(page_digest(&raw).as_slice())
                            {
                                return Err(CatalogError::Corrupt("活动批次页摘要不匹配"));
                            }
                            let claims = page_claim_keys(
                                namespace,
                                &raw,
                                page_items(header, usize::from(page)),
                            )?;
                            let (entries, remainder) = raw[2..].as_chunks::<56>();
                            debug_assert!(remainder.is_empty());
                            for (entry, claim) in entries.iter().zip(claims) {
                                if tr.get(&claim, false).await?.as_deref()
                                    != Some(batch_id.as_bytes())
                                {
                                    return Ok(ActivePreparationOutcome::RequestConflict);
                                }
                                let epoch = read_u64(&entry[..8]);
                                let operation = uuid_from(&entry[8..24])?;
                                let prepared = tr
                                    .get(&prepared_key(volume, generation, epoch, operation), false)
                                    .await?;
                                let Some(prepared) = prepared else {
                                    return Ok(ActivePreparationOutcome::PageIncomplete);
                                };
                                let (saved_batch, fingerprint, _) = decode_prepared(&prepared)?;
                                if saved_batch != batch_id || fingerprint != entry[24..] {
                                    return Err(CatalogError::Corrupt(
                                        "活动准备记录与批次清单不一致",
                                    ));
                                }
                            }
                            let marker = prepared_page_value(batch_id, &raw);
                            let marker_key = prepared_page_key(volume, generation, page);
                            if let Some(existing) = tr.get(&marker_key, false).await? {
                                return Ok(if existing.as_ref() == marker {
                                    ActivePreparationOutcome::AlreadyPrepared
                                } else {
                                    ActivePreparationOutcome::BatchConflict
                                });
                            }
                            tr.set(&marker_key, &marker);
                            Ok(ActivePreparationOutcome::Prepared)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Read one bounded page of outstanding work for a sealed, physically
    /// observed batch. Already materialized files and completed pages are
    /// omitted. `None` means the index has not been observed in FDB; callers
    /// must verify the tape before recording it or deciding what happened.
    pub async fn recover_observed_active_batch_page(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<Option<ActiveBatchRecoveryPage>> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("活动批次恢复身份无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        if !routing::require_active_owner(&tr, namespace, serial, owner_epoch).await? {
            return Err(CatalogError::Invalid("活动批次恢复 owner 已过期"));
        }
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("活动批次与带库放置不一致"));
        }
        let batch = tr.get(&batch_key(volume, generation), false).await?;
        let compacted = tr
            .get(&compacted_batch_key(volume, generation), false)
            .await?;
        if batch.is_some() && compacted.is_some() {
            return Err(CatalogError::Corrupt("活动批次头与完成记录同时存在"));
        }
        let completed = batch.is_none();
        let header = if let Some(raw) = batch {
            Header::decode(&raw)?
        } else {
            let raw = compacted.ok_or(CatalogError::Incomplete)?;
            decode_compacted_batch(&raw)?.0
        };
        if header.namespace != namespace
            || header.batch_id != batch_id
            || header.phase != ActiveBatchPhase::Sealed
            || header.owner_epoch > owner_epoch
            || usize::from(page) >= usize::from(header.pages)
        {
            return Err(CatalogError::Invalid("活动批次恢复头或页不匹配"));
        }
        let Some((observed_batch, index)) =
            active_volume::observed_batch_at(&tr, serial, volume, generation).await?
        else {
            return Ok(None);
        };
        if observed_batch != batch_id {
            return Err(CatalogError::Corrupt("活动卷索引与封存批次不一致"));
        }
        let next_page = (page + 1 < header.pages).then_some(page + 1);
        if completed {
            if tr
                .get(&publication_key(volume, generation), false)
                .await?
                .is_some()
                || tr
                    .get(&prepared_batch_key(volume, generation), false)
                    .await?
                    .is_some()
            {
                return Err(CatalogError::Corrupt("已完成批次仍有发布屏障"));
            }
            return Ok(Some(ActiveBatchRecoveryPage {
                index,
                receipts: Vec::new(),
                next_page,
            }));
        }
        let raw = tr
            .get(&page_key(volume, generation, page), false)
            .await?
            .ok_or(CatalogError::Corrupt("活动批次页缺失"))?;
        if tr
            .get(&page_digest_key(volume, generation, page), false)
            .await?
            .as_deref()
            != Some(page_digest(&raw).as_slice())
        {
            return Err(CatalogError::Corrupt("活动批次页摘要不匹配"));
        }
        let count = page_items(header, usize::from(page));
        let keys = page_claim_keys(namespace, &raw, count)?;
        if let Some(compacted) = tr
            .get(&compacted_page_key(volume, generation, page), false)
            .await?
        {
            let publication = tr
                .get(&publication_key(volume, generation), false)
                .await?
                .ok_or(CatalogError::Corrupt("归并页发布证明缺失"))?;
            if tr
                .get(&prepared_batch_key(volume, generation), false)
                .await?
                .as_deref()
                != Some(publication.as_ref())
            {
                return Err(CatalogError::Corrupt("归并页发布证明不一致"));
            }
            let prepared = tr
                .get(&prepared_page_key(volume, generation, page), false)
                .await?
                .ok_or(CatalogError::Corrupt("归并页准备证明缺失"))?;
            let finalized = tr
                .get(&finalized_page_key(volume, generation, page), false)
                .await?
                .ok_or(CatalogError::Corrupt("归并页终结证明缺失"))?;
            if prepared.as_ref() != prepared_page_value(batch_id, &raw)
                || finalized.as_ref() != finalized_page_value(batch_id, &publication, &prepared)
                || compacted.as_ref() != finalized.as_ref()
            {
                return Err(CatalogError::Corrupt("归并页完成证明不一致"));
            }
            return Ok(Some(ActiveBatchRecoveryPage {
                index,
                receipts: Vec::new(),
                next_page,
            }));
        }
        let mut receipts = Vec::with_capacity(count);
        let (entries, remainder) = raw[2..].as_chunks::<56>();
        debug_assert!(remainder.is_empty());
        for (entry, claim_key) in entries.iter().zip(keys) {
            let epoch = read_u64(&entry[..8]);
            let operation = uuid_from(&entry[8..24])?;
            if let Some(marker) = tr
                .get(&materialized_key(namespace, epoch, operation), false)
                .await?
            {
                if marker.len() != 57
                    || marker[0] != 1
                    || marker[1..17] != batch_id.as_bytes()[..]
                    || marker[17..49] != entry[24..]
                {
                    return Err(CatalogError::Corrupt("恢复页已归并文件标记不一致"));
                }
                let version = read_u64(&marker[49..57]);
                if version == 0 {
                    return Err(CatalogError::Corrupt("恢复页已归并文件版本无效"));
                }
                let terminal = tr
                    .get(&op_key(namespace, epoch, operation), false)
                    .await?
                    .ok_or(CatalogError::Corrupt("恢复页已归并文件终态缺失"))?;
                if requests::terminal_result(&terminal, &entry[24..])?
                    != PublishOutcome::AlreadyApplied(version)
                {
                    return Err(CatalogError::Corrupt("恢复页已归并文件终态不一致"));
                }
                continue;
            }
            if tr.get(&claim_key, false).await?.as_deref() != Some(batch_id.as_bytes()) {
                return Err(CatalogError::Corrupt("活动批次请求占用缺失或不一致"));
            }
            let mut intent_key = requests::epoch_prefix(b'I', namespace, epoch);
            intent_key.extend_from_slice(operation.as_bytes());
            let intent = tr
                .get(&intent_key, false)
                .await?
                .ok_or(CatalogError::Corrupt("活动批次准入意图缺失"))?;
            let receipt = requests::decode_intent(&intent, namespace, epoch, operation)?;
            if receipt_fingerprint(&receipt) != entry[24..]
                || receipt.library_serial != serial
                || receipt.volume_id != volume
                || receipt.volume_generation != generation
                || receipt.volume_scan_id != header.scan_id
                || receipt.owner_epoch != header.owner_epoch
            {
                return Err(CatalogError::Corrupt("活动批次页与准入意图不一致"));
            }
            if !target_claims_match(&tr, &receipt, batch_id).await? {
                return Err(CatalogError::Corrupt("活动批次页目标占用缺失或不一致"));
            }
            receipts.push(receipt);
        }
        Ok(Some(ActiveBatchRecoveryPage {
            index,
            receipts,
            next_page,
        }))
    }

    /// Read one bounded page of a sealed batch before FDB has observed its
    /// on-tape index. The full I receipt and original logical intent are both
    /// checked against the immutable manifest and target claims.
    pub async fn sealed_active_batch_page(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        page: u16,
    ) -> Result<ActiveSealedBatchPage> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || volume.is_nil() || generation == 0 || batch_id.is_nil() {
            return Err(CatalogError::Invalid("封存批次页身份无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        if !routing::require_active_owner(&tr, namespace, serial, owner_epoch).await? {
            return Err(CatalogError::Invalid("封存批次页 owner 已过期"));
        }
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("封存批次与带库放置不一致"));
        }
        let header = tr
            .get(&batch_key(volume, generation), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let header = Header::decode(&header)?;
        if header.namespace != namespace
            || header.batch_id != batch_id
            || header.phase != ActiveBatchPhase::Sealed
            || header.owner_epoch > owner_epoch
            || usize::from(page) >= usize::from(header.pages)
        {
            return Err(CatalogError::Invalid("封存批次页与头不一致"));
        }
        let raw = tr
            .get(&page_key(volume, generation, page), false)
            .await?
            .ok_or(CatalogError::Corrupt("封存批次页缺失"))?;
        if tr
            .get(&page_digest_key(volume, generation, page), false)
            .await?
            .as_deref()
            != Some(page_digest(&raw).as_slice())
        {
            return Err(CatalogError::Corrupt("封存批次页摘要不匹配"));
        }
        let count = page_items(header, usize::from(page));
        let claims = page_claim_keys(namespace, &raw, count)?;
        let (entries, remainder) = raw[2..].as_chunks::<56>();
        if !remainder.is_empty() || entries.len() != count {
            return Err(CatalogError::Corrupt("封存批次页条目数量不符"));
        }
        let mut files = Vec::with_capacity(count);
        for (entry, claim) in entries.iter().zip(claims) {
            if tr.get(&claim, false).await?.as_deref() != Some(batch_id.as_bytes()) {
                return Err(CatalogError::Corrupt("封存批次请求占用缺失"));
            }
            let epoch = read_u64(&entry[..8]);
            let operation = uuid_from(&entry[8..24])?;
            let mut intent_key = requests::epoch_prefix(b'I', namespace, epoch);
            intent_key.extend_from_slice(operation.as_bytes());
            let intent_raw = tr
                .get(&intent_key, false)
                .await?
                .ok_or(CatalogError::Corrupt("封存批次 I 收据缺失"))?;
            let receipt = requests::decode_intent(&intent_raw, namespace, epoch, operation)?;
            if receipt_fingerprint(&receipt) != entry[24..]
                || receipt.library_serial != serial
                || receipt.volume_id != volume
                || receipt.volume_generation != generation
                || receipt.volume_scan_id != header.scan_id
                || receipt.owner_epoch != header.owner_epoch
                || !target_claims_match(&tr, &receipt, batch_id).await?
            {
                return Err(CatalogError::Corrupt("封存批次页收据与声明不一致"));
            }
            let permit = requests::matching_permit(&intent_raw, &receipt)?
                .ok_or(CatalogError::Corrupt("封存批次 I 配额许可不一致"))?;
            let intent = reservation::intent_for_upgraded_at(&tr, &receipt, permit).await?;
            files.push((receipt, intent));
        }
        Ok(ActiveSealedBatchPage {
            files,
            next_page: (page + 1 < header.pages).then_some(page + 1),
        })
    }

    /// Claim every already-admitted request after appending the tape data.
    /// Each page commits atomically and is retryable before S4 is written.
    pub async fn stage_active_batch(
        &self,
        batch_id: Uuid,
        receipts: &[CommitReceipt],
    ) -> Result<ActiveBatchOutcome> {
        if batch_id.is_nil() || !(1..=MAX_BATCH_ITEMS).contains(&receipts.len()) {
            return Err(CatalogError::Invalid("活动批次 ID 或文件数量无效"));
        }
        let mut sorted = receipts.to_vec();
        sorted.sort_by_key(|r| (r.request_epoch, r.operation_id));
        let first = &sorted[0];
        control::validate_physical_serial(&first.library_serial)?;
        let mut paths = HashSet::with_capacity(sorted.len());
        let mut files = HashSet::with_capacity(sorted.len());
        for (position, receipt) in sorted.iter().enumerate() {
            validate_receipt(receipt)?;
            if receipt.namespace != first.namespace
                || receipt.library_serial != first.library_serial
                || receipt.volume_id != first.volume_id
                || receipt.volume_generation != first.volume_generation
                || receipt.volume_scan_id != first.volume_scan_id
                || receipt.owner_epoch != first.owner_epoch
                || receipt.copy_id != derive_copy_id(receipt)
                || (position > 0
                    && (receipt.request_epoch, receipt.operation_id)
                        == (
                            sorted[position - 1].request_epoch,
                            sorted[position - 1].operation_id,
                        ))
                || !paths.insert((receipt.parent, receipt.name.clone()))
                || !files.insert(receipt.file_id)
            {
                return Err(CatalogError::Invalid("活动批次收据身份、文件或路径重复"));
            }
        }
        let pages = sorted
            .chunks(PAGE_ITEMS)
            .map(page_value)
            .collect::<Result<Vec<_>>>()?;
        let page_digests = pages
            .iter()
            .map(|page| page_digest(page))
            .collect::<Vec<_>>();
        let header = Header {
            batch_id,
            namespace: first.namespace,
            owner_epoch: first.owner_epoch,
            scan_id: first.volume_scan_id,
            items: sorted.len() as u32,
            pages: pages.len() as u16,
            digest: manifest_digest(&page_digests),
            phase: ActiveBatchPhase::Open,
        };
        let volume = first.volume_id;
        let generation = first.volume_generation;
        let serial = first.library_serial.clone();
        let header_key = batch_key(volume, generation);
        let representative = first.clone();
        let beginning = self
            .db
            .run(|tr, _| {
                let header_key = header_key.clone();
                let representative = representative.clone();
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                header.namespace,
                                &serial,
                                header.owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActiveBatchOutcome::StaleOwner);
                            }
                            let Some(current) =
                                active_volume::request_current_index(&tr, &representative).await?
                            else {
                                return Ok(ActiveBatchOutcome::RequestConflict);
                            };
                            if !next_generation(current, generation) {
                                return Ok(ActiveBatchOutcome::GenerationConflict);
                            }
                            if active_volume::recovery_active(&tr, volume).await? {
                                return Ok(ActiveBatchOutcome::GenerationConflict);
                            }
                            if let Some(value) = tr.get(&header_key, false).await? {
                                let saved = Header::decode(&value)?;
                                return Ok(if !saved.same_manifest(header) {
                                    ActiveBatchOutcome::BatchConflict
                                } else {
                                    match saved.phase {
                                        ActiveBatchPhase::Open => ActiveBatchOutcome::Staged,
                                        ActiveBatchPhase::Sealed => {
                                            ActiveBatchOutcome::AlreadyStaged
                                        }
                                        ActiveBatchPhase::Aborting => {
                                            ActiveBatchOutcome::BatchConflict
                                        }
                                    }
                                });
                            }
                            tr.set(&header_key, &header.encode());
                            Ok(ActiveBatchOutcome::Staged)
                        }
                        .await,
                    )
                }
            })
            .await??;
        if beginning != ActiveBatchOutcome::Staged {
            return Ok(beginning);
        }
        for (number, (items, value)) in sorted.chunks(PAGE_ITEMS).zip(&pages).enumerate() {
            let items = items.to_vec();
            let value = value.clone();
            let representative = representative.clone();
            let serial = serial.clone();
            let header_key = header_key.clone();
            let page_key = page_key(volume, generation, number as u16);
            let digest_key = page_digest_key(volume, generation, number as u16);
            let digest = page_digests[number];
            let outcome = self
                .db
                .run(|tr, _| {
                    let items = items.clone();
                    let value = value.clone();
                    let representative = representative.clone();
                    let serial = serial.clone();
                    let header_key = header_key.clone();
                    let page_key = page_key.clone();
                    let digest_key = digest_key.clone();
                    async move {
                        requests::retry_result(
                            async {
                                if !routing::require_active_owner(
                                    &tr,
                                    header.namespace,
                                    &serial,
                                    header.owner_epoch,
                                )
                                .await?
                                {
                                    return Ok(ActiveBatchOutcome::StaleOwner);
                                }
                                if active_volume::recovery_active(&tr, volume).await? {
                                    return Ok(ActiveBatchOutcome::GenerationConflict);
                                }
                                let saved = tr
                                    .get(&header_key, false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("活动批次头缺失"))?;
                                let saved = Header::decode(&saved)?;
                                if !saved.same_manifest(header)
                                    || saved.phase != ActiveBatchPhase::Open
                                {
                                    return Ok(ActiveBatchOutcome::BatchConflict);
                                }
                                let Some(current) =
                                    active_volume::request_current_index(&tr, &representative)
                                        .await?
                                else {
                                    return Ok(ActiveBatchOutcome::RequestConflict);
                                };
                                if !next_generation(current, generation) {
                                    return Ok(ActiveBatchOutcome::GenerationConflict);
                                }
                                let existing_page = tr.get(&page_key, false).await?;
                                let existing_digest = tr.get(&digest_key, false).await?;
                                match (&existing_page, &existing_digest) {
                                    (Some(page), Some(saved))
                                        if page.as_ref() == value.as_slice()
                                            && saved.as_ref() == digest => {}
                                    (None, None) => {}
                                    (Some(_), Some(_)) => {
                                        return Ok(ActiveBatchOutcome::BatchConflict);
                                    }
                                    _ => {
                                        return Err(CatalogError::Corrupt(
                                            "活动批次页与摘要不成对",
                                        ));
                                    }
                                }
                                let mut pending_claims = Vec::with_capacity(items.len() * 4);
                                let mut completed_reservations = Vec::new();
                                for receipt in &items {
                                    let intent =
                                        tr.get(&requests::intent_key(receipt), false).await?;
                                    let Some(intent) = intent else {
                                        return Ok(ActiveBatchOutcome::RequestConflict);
                                    };
                                    let Some(permit) = requests::matching_permit(&intent, receipt)?
                                    else {
                                        return Ok(ActiveBatchOutcome::RequestConflict);
                                    };
                                    if let Some((reserved, original_permit)) =
                                        reservation::matching_at(&tr, receipt).await?
                                    {
                                        if original_permit != permit
                                            || !reservation::claims_match(&tr, &reserved).await?
                                        {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次写前预约与收据不一致",
                                            ));
                                        }
                                        if existing_page.is_some() {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次页仍保留写前预约",
                                            ));
                                        }
                                        if !reservation::parent_matches_at(&tr, &reserved).await? {
                                            return Ok(ActiveBatchOutcome::RequestConflict);
                                        }
                                        completed_reservations.push(reserved);
                                    } else if reservation::claim_conflicts(&tr, receipt).await? {
                                        return Ok(ActiveBatchOutcome::BatchConflict);
                                    }
                                    if !target_matches_baseline(&tr, receipt).await? {
                                        return Ok(ActiveBatchOutcome::RequestConflict);
                                    }
                                    let locator =
                                        TargetLocator::from_receipt(batch_id, receipt).encode();
                                    let reverse = TargetLocator::from_receipt(batch_id, receipt)
                                        .encode_reverse();
                                    for (claim, expected, target) in [
                                        (claim_key(receipt), batch_id.as_bytes().to_vec(), 0),
                                        (path_claim_key(receipt), locator.to_vec(), 1),
                                        (file_claim_key(receipt), locator.to_vec(), 1),
                                        (reverse_claim_key(receipt), reverse.to_vec(), 2),
                                    ] {
                                        if let Some(existing) = tr.get(&claim, false).await? {
                                            match target {
                                                1 => {
                                                    TargetLocator::decode(&existing)?;
                                                }
                                                2 => {
                                                    TargetLocator::decode_reverse(
                                                        &existing,
                                                        receipt.volume_id,
                                                        receipt.file_id,
                                                    )?;
                                                }
                                                _ => {}
                                            }
                                            if existing.as_ref() != expected.as_slice() {
                                                return Ok(ActiveBatchOutcome::BatchConflict);
                                            }
                                            if existing_page.is_none() {
                                                return Err(CatalogError::Corrupt(
                                                    "活动批次占用没有对应的页",
                                                ));
                                            }
                                        } else if existing_page.is_some() {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次页目标占用缺失",
                                            ));
                                        }
                                        pending_claims.push((claim, expected));
                                    }
                                }
                                for (claim, expected) in pending_claims {
                                    tr.set(&claim, &expected);
                                }
                                for reserved in &completed_reservations {
                                    reservation::clear(&tr, reserved);
                                }
                                tr.set(&page_key, &value);
                                tr.set(&digest_key, &digest);
                                Ok(ActiveBatchOutcome::Staged)
                            }
                            .await,
                        )
                    }
                })
                .await??;
            if outcome != ActiveBatchOutcome::Staged {
                return Ok(outcome);
            }
        }
        self.db
            .run(|tr, _| {
                let header_key = header_key.clone();
                let representative = representative.clone();
                let serial = serial.clone();
                let page_count = pages.len();
                async move {
                    requests::retry_result(
                        async {
                            if !routing::require_active_owner(
                                &tr,
                                header.namespace,
                                &serial,
                                header.owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActiveBatchOutcome::StaleOwner);
                            }
                            if active_volume::recovery_active(&tr, volume).await? {
                                return Ok(ActiveBatchOutcome::GenerationConflict);
                            }
                            let saved = tr
                                .get(&header_key, false)
                                .await?
                                .ok_or(CatalogError::Corrupt("活动批次头缺失"))?;
                            let saved = Header::decode(&saved)?;
                            if !saved.same_manifest(header) {
                                return Ok(ActiveBatchOutcome::BatchConflict);
                            }
                            match saved.phase {
                                ActiveBatchPhase::Open => {}
                                ActiveBatchPhase::Sealed => {
                                    return Ok(ActiveBatchOutcome::AlreadyStaged);
                                }
                                ActiveBatchPhase::Aborting => {
                                    return Ok(ActiveBatchOutcome::BatchConflict);
                                }
                            }
                            let Some(current) =
                                active_volume::request_current_index(&tr, &representative).await?
                            else {
                                return Ok(ActiveBatchOutcome::RequestConflict);
                            };
                            if !next_generation(current, generation) {
                                return Ok(ActiveBatchOutcome::GenerationConflict);
                            }
                            let prefix = page_digest_prefix(volume, generation);
                            let rows = read_page(&tr, &prefix, None, page_count + 1, false).await?;
                            if rows.next.is_some() || rows.items.len() != page_count {
                                return Err(CatalogError::Corrupt("活动批次页摘要数量不符"));
                            }
                            let mut actual = Vec::with_capacity(page_count);
                            for (number, (key, value)) in rows.items.into_iter().enumerate() {
                                if key != page_digest_key(volume, generation, number as u16)
                                    || value.len() != 32
                                {
                                    return Err(CatalogError::Corrupt("活动批次页摘要顺序或长度"));
                                }
                                actual.push(value[..].try_into().expect("validated length"));
                            }
                            if manifest_digest(&actual) != header.digest {
                                return Err(CatalogError::Corrupt("活动批次页摘要不匹配"));
                            }
                            tr.set(
                                &header_key,
                                &Header {
                                    phase: ActiveBatchPhase::Sealed,
                                    ..header
                                }
                                .encode(),
                            );
                            Ok(ActiveBatchOutcome::Staged)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Clear an interrupted, unsealed admission after owner takeover. The
    /// caller must first prove the fenced medium has no file S4 generation;
    /// an Open batch may have been staged after data records were appended.
    /// An Aborting header fences concurrent staging.
    pub async fn abort_open_active_batch(
        &self,
        serial: &str,
        volume: Uuid,
        generation: u64,
        batch_id: Uuid,
        owner_epoch: u64,
    ) -> Result<ActiveBatchOutcome> {
        control::validate_physical_serial(serial)?;
        if volume.is_nil() || generation == 0 || batch_id.is_nil() || owner_epoch == 0 {
            return Err(CatalogError::Invalid("活动批次中止身份无效"));
        }
        let serial = serial.to_owned();
        let header_key = batch_key(volume, generation);
        let start = self
            .db
            .run(|tr, _| {
                let serial = serial.clone();
                let header_key = header_key.clone();
                async move {
                    requests::retry_result(
                        async {
                            let header = match abort_header(
                                &tr,
                                &serial,
                                volume,
                                generation,
                                batch_id,
                                owner_epoch,
                            )
                            .await?
                            {
                                Ok(header) => header,
                                Err(outcome) => return Ok(outcome),
                            };
                            match header.phase {
                                ActiveBatchPhase::Sealed => Ok(ActiveBatchOutcome::BatchConflict),
                                ActiveBatchPhase::Aborting => Ok(ActiveBatchOutcome::Aborted),
                                ActiveBatchPhase::Open => {
                                    tr.set(
                                        &header_key,
                                        &Header {
                                            phase: ActiveBatchPhase::Aborting,
                                            ..header
                                        }
                                        .encode(),
                                    );
                                    Ok(ActiveBatchOutcome::Aborted)
                                }
                            }
                        }
                        .await,
                    )
                }
            })
            .await??;
        if start != ActiveBatchOutcome::Aborted {
            return Ok(start);
        }
        let Some(status) = self
            .active_batch_status(&serial, volume, generation)
            .await?
        else {
            return Ok(ActiveBatchOutcome::AlreadyAborted);
        };
        if status.batch_id != batch_id || status.phase != ActiveBatchPhase::Aborting {
            return Ok(ActiveBatchOutcome::BatchConflict);
        }
        for page in 0..(status.items as usize).div_ceil(PAGE_ITEMS) {
            let serial = serial.clone();
            let page_key = page_key(volume, generation, page as u16);
            let digest_key = page_digest_key(volume, generation, page as u16);
            let outcome = self
                .db
                .run(|tr, _| {
                    let serial = serial.clone();
                    let page_key = page_key.clone();
                    let digest_key = digest_key.clone();
                    async move {
                        requests::retry_result(
                            async {
                                let header = match abort_header(
                                    &tr,
                                    &serial,
                                    volume,
                                    generation,
                                    batch_id,
                                    owner_epoch,
                                )
                                .await?
                                {
                                    Ok(header) => header,
                                    Err(outcome) => return Ok(outcome),
                                };
                                if header.phase != ActiveBatchPhase::Aborting {
                                    return Ok(ActiveBatchOutcome::BatchConflict);
                                }
                                let page_value = tr.get(&page_key, false).await?;
                                let digest = tr.get(&digest_key, false).await?;
                                if page_value.is_some() != digest.is_some() {
                                    return Err(CatalogError::Corrupt(
                                        "活动批次中止页与摘要不成对",
                                    ));
                                }
                                if let Some(value) = page_value {
                                    if digest.as_deref() != Some(page_digest(&value).as_slice()) {
                                        return Err(CatalogError::Corrupt(
                                            "活动批次中止页摘要不匹配",
                                        ));
                                    }
                                    let request_claims = page_claim_keys(
                                        header.namespace,
                                        &value,
                                        page_items(header, page),
                                    )?;
                                    let mut claims = Vec::with_capacity(request_claims.len() * 4);
                                    let (entries, remainder) = value[2..].as_chunks::<56>();
                                    debug_assert!(remainder.is_empty());
                                    for (entry, request_claim) in entries.iter().zip(request_claims)
                                    {
                                        let epoch = read_u64(&entry[..8]);
                                        let operation = uuid_from(&entry[8..24])?;
                                        let mut intent_key =
                                            requests::epoch_prefix(b'I', header.namespace, epoch);
                                        intent_key.extend_from_slice(operation.as_bytes());
                                        let intent = tr.get(&intent_key, false).await?.ok_or(
                                            CatalogError::Corrupt("活动批次中止准入意图缺失"),
                                        )?;
                                        let receipt = requests::decode_intent(
                                            &intent,
                                            header.namespace,
                                            epoch,
                                            operation,
                                        )?;
                                        if receipt_fingerprint(&receipt) != entry[24..]
                                            || receipt.library_serial != serial
                                            || receipt.volume_id != volume
                                            || receipt.volume_generation != generation
                                            || receipt.volume_scan_id != header.scan_id
                                            || receipt.owner_epoch != header.owner_epoch
                                        {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次中止页与准入意图不一致",
                                            ));
                                        }
                                        let locator =
                                            TargetLocator::from_receipt(batch_id, &receipt)
                                                .encode();
                                        let reverse =
                                            TargetLocator::from_receipt(batch_id, &receipt)
                                                .encode_reverse();
                                        claims.extend([
                                            (request_claim, batch_id.as_bytes().to_vec(), 0),
                                            (path_claim_key(&receipt), locator.to_vec(), 1),
                                            (file_claim_key(&receipt), locator.to_vec(), 1),
                                            (reverse_claim_key(&receipt), reverse.to_vec(), 2),
                                        ]);
                                    }
                                    for (claim, expected, target) in &claims {
                                        let marker = tr.get(claim, false).await?.ok_or(
                                            CatalogError::Corrupt("活动批次页目标占用缺失"),
                                        )?;
                                        match target {
                                            1 => {
                                                TargetLocator::decode(&marker)?;
                                            }
                                            2 => {
                                                let file = uuid_from(
                                                    &claim[claim.len() - 32..claim.len() - 16],
                                                )?;
                                                TargetLocator::decode_reverse(
                                                    &marker, volume, file,
                                                )?;
                                            }
                                            _ => {}
                                        }
                                        if marker.as_ref() != expected.as_slice() {
                                            return Err(CatalogError::Corrupt(
                                                "活动批次页目标占用不一致",
                                            ));
                                        }
                                    }
                                    for (claim, _, _) in claims {
                                        tr.clear(&claim);
                                    }
                                    tr.clear(&page_key);
                                    tr.clear(&digest_key);
                                }
                                Ok(ActiveBatchOutcome::Aborted)
                            }
                            .await,
                        )
                    }
                })
                .await??;
            if outcome != ActiveBatchOutcome::Aborted {
                return Ok(outcome);
            }
        }
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                let header_key = header_key.clone();
                async move {
                    requests::retry_result(
                        async {
                            let header = match abort_header(
                                &tr,
                                &serial,
                                volume,
                                generation,
                                batch_id,
                                owner_epoch,
                            )
                            .await?
                            {
                                Ok(header) => header,
                                Err(outcome) => return Ok(outcome),
                            };
                            if header.phase != ActiveBatchPhase::Aborting {
                                return Ok(ActiveBatchOutcome::BatchConflict);
                            }
                            for page in 0..usize::from(header.pages) {
                                if tr
                                    .get(&page_key(volume, generation, page as u16), false)
                                    .await?
                                    .is_some()
                                    || tr
                                        .get(
                                            &page_digest_key(volume, generation, page as u16),
                                            false,
                                        )
                                        .await?
                                        .is_some()
                                {
                                    return Err(CatalogError::Corrupt(
                                        "活动批次中止后仍有页或摘要",
                                    ));
                                }
                            }
                            tr.clear(&header_key);
                            Ok(ActiveBatchOutcome::Aborted)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Discover a staged batch for takeover. Sealed is a metadata barrier,
    /// never evidence of a successful physical tape commit.
    pub async fn active_batch_status(
        &self,
        serial: &str,
        volume: Uuid,
        generation: u64,
    ) -> Result<Option<ActiveBatchStatus>> {
        control::validate_physical_serial(serial)?;
        if generation == 0 {
            return Err(CatalogError::Invalid("活动批次代数无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("活动批次与带库放置不一致"));
        }
        let batch = tr.get(&batch_key(volume, generation), false).await?;
        let compacted = tr
            .get(&compacted_batch_key(volume, generation), false)
            .await?;
        if batch.is_some() && compacted.is_some() {
            return Err(CatalogError::Corrupt("活动批次头与完成记录同时存在"));
        }
        let completed = batch.is_none();
        let header = if let Some(value) = batch {
            Header::decode(&value)?
        } else {
            let Some(value) = compacted else {
                return Ok(None);
            };
            decode_compacted_batch(&value)?.0
        };
        if header.namespace != namespace {
            return Err(CatalogError::Corrupt("活动批次与命名空间不一致"));
        }
        let publication = tr.get(&publication_key(volume, generation), false).await?;
        if completed {
            let Some((observed_batch, _)) =
                active_volume::observed_batch_at(&tr, serial, volume, generation).await?
            else {
                return Err(CatalogError::Corrupt("已完成批次缺少物理索引观察"));
            };
            if observed_batch != header.batch_id
                || publication.is_some()
                || tr
                    .get(&prepared_batch_key(volume, generation), false)
                    .await?
                    .is_some()
            {
                return Err(CatalogError::Corrupt(
                    "已完成批次与物理索引或发布屏障不一致",
                ));
            }
        }
        Ok(Some(ActiveBatchStatus {
            batch_id: header.batch_id,
            items: header.items,
            pages: header.pages,
            phase: header.phase,
            completed,
            published: publication.is_some(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::NodeMeta;

    #[test]
    fn prepared_attribute_marker_rejects_truncated_and_unbounded_values() {
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 42,
                ..Default::default()
            },
            Vec::new(),
            None,
        )
        .unwrap();
        let batch = Uuid::new_v4();
        let marker = prepared_value(batch, &[7; 32], &attributes);
        assert_eq!(decode_prepared(&marker).unwrap().0, batch);
        assert!(decode_prepared(&marker[..marker.len() - 1]).is_err());
        let mut corrupt = marker.clone();
        corrupt[0] = 1;
        assert!(decode_prepared(&corrupt).is_err());
        corrupt = marker.clone();
        corrupt[49..53].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_prepared(&corrupt).is_err());
        corrupt = marker;
        corrupt[85..87].copy_from_slice(&0_u16.to_be_bytes());
        assert!(decode_prepared(&corrupt).is_err());
    }

    #[test]
    fn prepared_page_marker_binds_exact_page_and_batch() {
        let batch = Uuid::new_v4();
        let page = prepared_page_value(batch, b"two files");
        assert_eq!(page[1..17], batch.as_bytes()[..]);
        assert_ne!(page, prepared_page_value(batch, b"one file"));
        assert_ne!(page, prepared_page_value(Uuid::new_v4(), b"two files"));
    }
}
