//! Durable, invisible staging of an independently audited cross-volume view.
//! These keys are not consulted by normal namespace lookups. Materialization
//! into nodes, tombstones and physical copies requires a separate proof gate.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    CatalogError, ContentDigest, DirectoryAudit, ImportCandidate, ImportOrigin, ImportReconciler,
    ImportSourceBinding, ImportVolume, ManagedCatalog, NamespaceState, NodeAttributes, NodeKind,
    ReconciledPath, Result, SourceCandidate, control, import_spool, key, namespace_key,
};

const ROW_DOMAIN: &[u8] = b"tape-rs/staged-import-rows/1";
const VALUE_CHUNK: usize = 64_000;
pub(super) const MANIFEST_LEN: usize = 171;
const ROW_HEADER_LEN: usize = 45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedImportSummary {
    pub audit: DirectoryAudit,
    pub paths: u64,
    pub rows_sha256: [u8; 32],
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedImportEntry {
    pub path: ReconciledPath,
    pub current_attributes: NodeAttributes,
    /// LTFS fileuid of each physical source, aligned with `path.sources`.
    pub source_file_uids: Vec<u64>,
    /// Version 5 rows retain the actual data object and EE target path. Older
    /// rows may be inspected or discarded, but cannot prove file materialization.
    pub source_data: Option<Vec<StagedSourceData>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StagedSourceData {
    NoCopy,
    NativeSymlink,
    Data {
        file_uid: u64,
        length: u64,
        content_digest: ContentDigest,
        ee_alias: bool,
        /// Only EE aliases need a second path; direct files reuse the candidate path.
        data_path: Option<String>,
    },
}

pub(super) fn source_data_matches(
    candidate: &ImportCandidate,
    logical_uid: u64,
    data: &StagedSourceData,
    require_ee_path: bool,
) -> bool {
    match data {
        StagedSourceData::NoCopy => candidate.deleted || candidate.kind == NodeKind::Directory,
        StagedSourceData::NativeSymlink => {
            !candidate.deleted
                && candidate.kind == NodeKind::File
                && candidate.length == 0
                && candidate.sha256.is_empty()
        }
        StagedSourceData::Data {
            file_uid,
            length,
            content_digest,
            ee_alias,
            data_path,
        } => {
            if candidate.deleted || candidate.kind != NodeKind::File || *file_uid == 0 {
                return false;
            }
            if *ee_alias {
                if *file_uid == logical_uid || candidate.length != 0 || !candidate.sha256.is_empty()
                {
                    return false;
                }
                if let Some(path) = data_path {
                    if !path.starts_with(".LTFSEE_DATA/")
                        || import_spool::validate_path(path).is_err()
                    {
                        return false;
                    }
                } else if require_ee_path {
                    return false;
                }
            } else if *file_uid != logical_uid || *length != candidate.length || data_path.is_some()
            {
                return false;
            }
            if candidate.sha256.is_empty() {
                return true;
            }
            let ContentDigest::Sha256(expected) = content_digest else {
                return false;
            };
            candidate.sha256.len() == 64
                && candidate
                    .sha256
                    .as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(expected)
                    .all(|(hex, byte)| {
                        std::str::from_utf8(hex)
                            .ok()
                            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                            == Some(*byte)
                    })
        }
    }
}

fn winner_data_matches(
    candidate: &ImportCandidate,
    attributes: &NodeAttributes,
    data: &StagedSourceData,
    require_ee_path: bool,
) -> bool {
    match (attributes.symlink(), data) {
        (
            None,
            StagedSourceData::Data {
                ee_alias: false, ..
            },
        ) => !candidate.deleted && candidate.kind == NodeKind::File,
        (
            Some(target),
            StagedSourceData::Data {
                ee_alias: true,
                data_path,
                ..
            },
        ) if target.contains(".LTFSEE_DATA/") => {
            !require_ee_path
                || import_spool::resolve_ee_alias_target(&candidate.physical_path, target)
                    .ok()
                    .as_ref()
                    == data_path.as_ref()
        }
        (Some(target), StagedSourceData::NativeSymlink) => !target.contains(".LTFSEE_DATA/"),
        (_, StagedSourceData::NoCopy) => candidate.deleted || candidate.kind == NodeKind::Directory,
        _ => false,
    }
}

fn decode_source_data(data: &[u8], at: &mut usize, version: u8) -> Result<StagedSourceData> {
    let tag = take(data, at, 1)?[0];
    match tag {
        0 => Ok(StagedSourceData::NoCopy),
        3 => Ok(StagedSourceData::NativeSymlink),
        1 | 2 => {
            let file_uid = take_u64(data, at)?;
            let length = take_u64(data, at)?;
            let digest_tag = take(data, at, 1)?[0];
            let digest_len = match digest_tag {
                0 => 0,
                1 => 32,
                2 => 16,
                _ => return Err(CatalogError::Corrupt("暂存数据摘要算法")),
            };
            let content_digest =
                ContentDigest::decode_compact(digest_tag, take(data, at, digest_len)?)?;
            let data_path = if tag == 2 && version >= 5 {
                Some(take_text(data, at)?)
            } else {
                None
            };
            Ok(StagedSourceData::Data {
                file_uid,
                length,
                content_digest,
                ee_alias: tag == 2,
                data_path,
            })
        }
        _ => Err(CatalogError::Corrupt("暂存数据对象类型")),
    }
}

fn encode_source_data(data: &StagedSourceData) -> Result<Vec<u8>> {
    Ok(match data {
        StagedSourceData::NoCopy => vec![0],
        StagedSourceData::NativeSymlink => vec![3],
        StagedSourceData::Data {
            file_uid,
            length,
            content_digest,
            ee_alias,
            data_path,
        } => {
            let mut out = Vec::with_capacity(50);
            out.push(if *ee_alias { 2 } else { 1 });
            out.extend_from_slice(&file_uid.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
            out.push(content_digest.tag());
            out.extend_from_slice(&content_digest.payload()[..content_digest.compact_len()]);
            if *ee_alias {
                let path = data_path
                    .as_deref()
                    .ok_or(CatalogError::Invalid("EE 数据对象路径缺失"))?;
                let len = u32::try_from(path.len())
                    .map_err(|_| CatalogError::Invalid("EE 数据对象路径过长"))?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(path.as_bytes());
            }
            out
        }
    })
}

fn path_sources_match(path: &ReconciledPath, sources: &HashMap<String, (Uuid, Uuid, u64)>) -> bool {
    path.sources.iter().all(|source| {
        sources.get(&source.barcode) == Some(&(path.pool_id, source.volume_id, source.generation))
    })
}

impl StagedImportSummary {
    fn initial(audit: DirectoryAudit) -> Self {
        Self {
            audit,
            paths: 0,
            rows_sha256: [0; 32],
            complete: false,
        }
    }

    pub(super) fn encode(self) -> [u8; MANIFEST_LEN] {
        let mut out = [0; MANIFEST_LEN];
        out[0] = 3;
        out[1] = u8::from(self.complete);
        out[2..10].copy_from_slice(&self.audit.applied_index.to_be_bytes());
        out[10..18].copy_from_slice(&self.audit.frozen_index.to_be_bytes());
        out[18..26].copy_from_slice(&self.audit.paths.to_be_bytes());
        out[26..34].copy_from_slice(&self.audit.tombstones.to_be_bytes());
        out[34..66].copy_from_slice(&self.audit.rows_sha256);
        out[66..98].copy_from_slice(&self.audit.snapshot_sha256);
        out[98..106].copy_from_slice(&self.paths.to_be_bytes());
        out[106..138].copy_from_slice(&self.rows_sha256);
        match self.audit.origin {
            ImportOrigin::LegacySnapshot => {}
            ImportOrigin::ExternalMedia { library_sha256 } => {
                out[138] = 1;
                out[139..171].copy_from_slice(&library_sha256);
            }
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != MANIFEST_LEN || bytes[0] != 3 || bytes[1] > 1 || bytes[138] > 1 {
            return Err(CatalogError::Corrupt("暂存导入清单格式"));
        }
        if (bytes[138] == 0 && bytes[139..171] != [0; 32])
            || (bytes[138] == 1 && bytes[139..171] == [0; 32])
        {
            return Err(CatalogError::Corrupt("暂存导入带库来源无效"));
        }
        let mut audit_sha = [0; 32];
        audit_sha.copy_from_slice(&bytes[34..66]);
        let mut snapshot_sha = [0; 32];
        snapshot_sha.copy_from_slice(&bytes[66..98]);
        let mut rows_sha = [0; 32];
        rows_sha.copy_from_slice(&bytes[106..138]);
        let value = Self {
            audit: DirectoryAudit {
                origin: if bytes[138] == 0 {
                    ImportOrigin::LegacySnapshot
                } else {
                    ImportOrigin::ExternalMedia {
                        library_sha256: bytes[139..171].try_into().unwrap(),
                    }
                },
                applied_index: u64::from_be_bytes(bytes[2..10].try_into().unwrap()),
                frozen_index: u64::from_be_bytes(bytes[10..18].try_into().unwrap()),
                paths: u64::from_be_bytes(bytes[18..26].try_into().unwrap()),
                tombstones: u64::from_be_bytes(bytes[26..34].try_into().unwrap()),
                rows_sha256: audit_sha,
                snapshot_sha256: snapshot_sha,
            },
            paths: u64::from_be_bytes(bytes[98..106].try_into().unwrap()),
            rows_sha256: rows_sha,
            complete: bytes[1] == 1,
        };
        if value.audit.applied_index == 0
            || value.audit.frozen_index == 0
            || value.audit.frozen_index > value.audit.applied_index
            || value.audit.snapshot_sha256 == [0; 32]
            || value.audit.tombstones > value.audit.paths
            || (value.complete && value.paths != value.audit.paths)
            || (!value.complete && (value.paths != 0 || value.rows_sha256 != [0; 32]))
        {
            return Err(CatalogError::Corrupt("暂存导入清单内容"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy)]
struct RowHeader {
    len: u64,
    chunks: u32,
    sha256: [u8; 32],
}

impl RowHeader {
    fn encode(self) -> [u8; ROW_HEADER_LEN] {
        let mut out = [0; ROW_HEADER_LEN];
        out[0] = 1;
        out[1..9].copy_from_slice(&self.len.to_be_bytes());
        out[9..13].copy_from_slice(&self.chunks.to_be_bytes());
        out[13..45].copy_from_slice(&self.sha256);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < ROW_HEADER_LEN || bytes[0] != 1 {
            return Err(CatalogError::Corrupt("暂存候选行头格式"));
        }
        let mut sha256 = [0; 32];
        sha256.copy_from_slice(&bytes[13..45]);
        let value = Self {
            len: u64::from_be_bytes(bytes[1..9].try_into().unwrap()),
            chunks: u32::from_be_bytes(bytes[9..13].try_into().unwrap()),
            sha256,
        };
        if value.len == 0 {
            return Err(CatalogError::Corrupt("暂存候选分块数量"));
        }
        if value.chunks == 0 {
            if value.len > VALUE_CHUNK as u64
                || bytes.len() != ROW_HEADER_LEN + value.len as usize
                || <[u8; 32]>::from(Sha256::digest(&bytes[ROW_HEADER_LEN..])) != value.sha256
            {
                return Err(CatalogError::Corrupt("暂存候选内联内容"));
            }
        } else if bytes.len() != ROW_HEADER_LEN
            || value.chunks as u64 != value.len.div_ceil(VALUE_CHUNK as u64)
        {
            return Err(CatalogError::Corrupt("暂存候选分块数量"));
        }
        Ok(value)
    }
}

pub(super) fn manifest_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'f');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn confirmed_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'j');
    out.extend_from_slice(namespace.as_bytes());
    out
}

fn row_key(namespace: Uuid, ordinal: u64) -> Vec<u8> {
    let mut out = key(b'g');
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&ordinal.to_be_bytes());
    out
}

fn chunk_key(namespace: Uuid, ordinal: u64, chunk: u32) -> Vec<u8> {
    let mut out = key(b'h');
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&ordinal.to_be_bytes());
    out.extend_from_slice(&chunk.to_be_bytes());
    out
}

pub(super) fn take<'a>(data: &'a [u8], at: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(length)
        .ok_or(CatalogError::Corrupt("暂存候选长度溢出"))?;
    let bytes = data
        .get(*at..end)
        .ok_or(CatalogError::Corrupt("暂存候选截断"))?;
    *at = end;
    Ok(bytes)
}

fn take_u32(data: &[u8], at: &mut usize) -> Result<u32> {
    Ok(u32::from_be_bytes(take(data, at, 4)?.try_into().unwrap()))
}

pub(super) fn take_u64(data: &[u8], at: &mut usize) -> Result<u64> {
    Ok(u64::from_be_bytes(take(data, at, 8)?.try_into().unwrap()))
}

pub(super) fn take_text(data: &[u8], at: &mut usize) -> Result<String> {
    let length = take_u32(data, at)? as usize;
    std::str::from_utf8(take(data, at, length)?)
        .map(str::to_owned)
        .map_err(|_| CatalogError::Corrupt("暂存候选 UTF-8"))
}

type DecodedEntry = (
    ReconciledPath,
    Option<NodeAttributes>,
    Option<Vec<u64>>,
    Option<Vec<StagedSourceData>>,
);

fn decode_entry(data: &[u8]) -> Result<DecodedEntry> {
    let mut at = 0;
    let version = take(data, &mut at, 1)?[0];
    if !(1..=5).contains(&version) {
        return Err(CatalogError::Corrupt("暂存候选版本"));
    }
    let pool_id = Uuid::from_slice(take(data, &mut at, 16)?)
        .map_err(|_| CatalogError::Corrupt("暂存候选池 UUID"))?;
    let logical_path = take_text(data, &mut at)?;
    let winner = take_u32(data, &mut at)? as usize;
    let count = take_u32(data, &mut at)? as usize;
    if pool_id.is_nil()
        || !logical_path.starts_with('/')
        || logical_path.len() < 2
        || count == 0
        || winner >= count
        || count > (data.len() - at) / 62
    {
        return Err(CatalogError::Corrupt("暂存候选逻辑身份"));
    }
    let mut sources = Vec::with_capacity(count);
    let mut source_file_uids = if version >= 3 {
        Some(Vec::with_capacity(count))
    } else {
        None
    };
    let mut source_data = if version >= 4 {
        Some(Vec::with_capacity(count))
    } else {
        None
    };
    let mut barcodes = std::collections::HashSet::with_capacity(count);
    for _ in 0..count {
        let barcode = take_text(data, &mut at)?;
        let volume_id = Uuid::from_slice(take(data, &mut at, 16)?)
            .map_err(|_| CatalogError::Corrupt("暂存候选卷 UUID"))?;
        let generation = take_u64(data, &mut at)?;
        let physical_path = take_text(data, &mut at)?;
        let kind = match take(data, &mut at, 1)?[0] {
            0 => NodeKind::Directory,
            1 => NodeKind::File,
            _ => return Err(CatalogError::Corrupt("暂存候选节点类型")),
        };
        let deleted = match take(data, &mut at, 1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(CatalogError::Corrupt("暂存候选删除位")),
        };
        let candidate_version = (take_u64(data, &mut at)?, take_u64(data, &mut at)?);
        let length = take_u64(data, &mut at)?;
        let sha256 = take_text(data, &mut at)?;
        let file_uid = if version >= 3 {
            let uid = take_u64(data, &mut at)?;
            if uid == 0 {
                return Err(CatalogError::Corrupt("暂存候选来源 fileuid"));
            }
            Some(uid)
        } else {
            None
        };
        let candidate = ImportCandidate {
            logical_path: logical_path.clone(),
            physical_path,
            kind,
            deleted,
            version: candidate_version,
            length,
            sha256,
        };
        let data_source = if version >= 4 {
            let data_source = decode_source_data(data, &mut at, version)?;
            if !source_data_matches(
                &candidate,
                file_uid.expect("version 4+ has a logical UID"),
                &data_source,
                version >= 5,
            ) {
                return Err(CatalogError::Corrupt("暂存数据对象与候选不一致"));
            }
            Some(data_source)
        } else {
            None
        };
        if barcode.is_empty()
            || barcode.len() > 32
            || !barcode.is_ascii()
            || !barcodes.insert(barcode.clone())
            || volume_id.is_nil()
            || generation == 0
            || candidate.physical_path.is_empty()
            || (deleted && (length != 0 || !candidate.sha256.is_empty()))
            || (kind == NodeKind::Directory && (length != 0 || !candidate.sha256.is_empty()))
        {
            return Err(CatalogError::Corrupt("暂存候选来源字段"));
        }
        sources.push(SourceCandidate {
            barcode,
            volume_id,
            generation,
            candidate,
        });
        if let Some(file_uid) = file_uid {
            source_file_uids.as_mut().unwrap().push(file_uid);
        }
        if let Some(data_source) = data_source {
            source_data.as_mut().unwrap().push(data_source);
        }
    }
    let attributes = if version >= 2 {
        let length = take_u32(data, &mut at)? as usize;
        let attributes = NodeAttributes::decode(take(data, &mut at, length)?)?;
        let candidate = &sources[winner].candidate;
        let physical_kind = if candidate.deleted {
            NodeKind::File
        } else {
            candidate.kind
        };
        let physical_length = if candidate.deleted || physical_kind == NodeKind::File {
            Some(candidate.length)
        } else {
            None
        };
        let reconstructed = import_spool::import_candidate(
            &candidate.physical_path,
            physical_kind,
            physical_length,
            &attributes,
        )
        .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if reconstructed.as_ref() != Some(candidate) {
            return Err(CatalogError::Corrupt("暂存候选当前属性与介质来源不一致"));
        }
        if source_file_uids
            .as_ref()
            .is_some_and(|uids| attributes.meta().map(|meta| meta.file_uid) != Some(uids[winner]))
        {
            return Err(CatalogError::Corrupt("暂存赢家属性 fileuid 与来源不一致"));
        }
        if let Some(data_source) = &source_data
            && !winner_data_matches(candidate, &attributes, &data_source[winner], version >= 5)
        {
            return Err(CatalogError::Corrupt("暂存赢家数据类型与属性不一致"));
        }
        Some(attributes)
    } else {
        None
    };
    if at != data.len() {
        return Err(CatalogError::Corrupt("暂存候选尾部数据"));
    }
    Ok((
        ReconciledPath {
            pool_id,
            logical_path,
            sources,
            winner,
        },
        attributes,
        source_file_uids,
        source_data,
    ))
}

struct RowWriter<'a> {
    catalog: &'a ManagedCatalog,
    namespace: Uuid,
    audit: DirectoryAudit,
    ordinal: u64,
    pending: Vec<u8>,
    len: u64,
    chunks: u32,
    sha: Sha256,
}

struct RowEvidence<'a> {
    attributes: Option<&'a NodeAttributes>,
    source_file_uids: Option<&'a [u64]>,
    source_data: Option<&'a [StagedSourceData]>,
}

impl RowWriter<'_> {
    async fn push(&mut self, mut bytes: &[u8]) -> Result<()> {
        self.len = self
            .len
            .checked_add(bytes.len() as u64)
            .ok_or(CatalogError::Invalid("暂存候选长度溢出"))?;
        self.sha.update(bytes);
        while !bytes.is_empty() {
            if self.pending.len() == VALUE_CHUNK {
                self.flush().await?;
            }
            let take = (VALUE_CHUNK - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        Ok(())
    }

    async fn field(&mut self, bytes: &[u8]) -> Result<()> {
        let len =
            u32::try_from(bytes.len()).map_err(|_| CatalogError::Invalid("暂存候选字段过长"))?;
        self.push(&len.to_be_bytes()).await?;
        self.push(bytes).await
    }

    async fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = self.chunks;
        self.chunks = self
            .chunks
            .checked_add(1)
            .ok_or(CatalogError::Invalid("暂存候选分块数溢出"))?;
        let bytes = std::mem::take(&mut self.pending);
        self.catalog
            .write_stage_value(
                self.namespace,
                self.audit,
                chunk_key(self.namespace, self.ordinal, chunk),
                bytes,
            )
            .await
    }

    async fn finish(mut self) -> Result<RowHeader> {
        let inline = self.chunks == 0;
        let final_chunk = if inline || self.pending.is_empty() {
            None
        } else {
            let chunk = self.chunks;
            self.chunks = self
                .chunks
                .checked_add(1)
                .ok_or(CatalogError::Invalid("暂存候选分块数溢出"))?;
            Some((
                chunk_key(self.namespace, self.ordinal, chunk),
                std::mem::take(&mut self.pending),
            ))
        };
        let header = RowHeader {
            len: self.len,
            chunks: self.chunks,
            sha256: self.sha.finalize().into(),
        };
        let mut row_value = header.encode().to_vec();
        if inline {
            row_value.extend_from_slice(&self.pending);
        }
        RowHeader::decode(&row_value)?;
        self.catalog
            .write_stage_final(
                self.namespace,
                self.audit,
                final_chunk,
                row_key(self.namespace, self.ordinal),
                row_value,
            )
            .await?;
        Ok(header)
    }
}

impl ManagedCatalog {
    async fn verify_import_volume_set(
        &self,
        namespace: Uuid,
        volumes: &[ImportVolume<'_>],
        bindings: &[ImportSourceBinding],
    ) -> Result<()> {
        if volumes.len() != bindings.len() {
            return Err(CatalogError::Incomplete);
        }
        let mut by_barcode = std::collections::HashMap::with_capacity(bindings.len());
        let mut volume_ids = std::collections::HashSet::with_capacity(bindings.len());
        for binding in bindings {
            let placement = &binding.placement;
            if placement.namespace != namespace
                || !volume_ids.insert(placement.volume_id)
                || by_barcode
                    .insert(placement.barcode.as_str(), binding)
                    .is_some()
            {
                return Err(CatalogError::Invalid("导入来源卷身份重复或命名空间不符"));
            }
        }
        for volume in volumes {
            let binding = by_barcode
                .get(volume.barcode.as_str())
                .ok_or(CatalogError::Incomplete)?;
            if binding.placement.pool_id != volume.pool_id
                || binding.placement.volume_id != volume.volume_id
                || binding.scan.generation != volume.generation
                || binding.source != volume.spool.source()
            {
                return Err(CatalogError::Incomplete);
            }
        }
        for binding in bindings {
            self.verify_import_source_binding(binding).await?;
        }
        if self.namespace_state(namespace).await? != Some(NamespaceState::Building) {
            return Err(CatalogError::Incomplete);
        }
        Ok(())
    }

    /// Audit every assigned volume against the offline directory, then stage
    /// all logical candidates under private FDB keys. Replaying after an
    /// interrupted write checks every already-present chunk byte for byte.
    /// Every offline source must match one currently ready managed FDB volume.
    /// This does not materialize the namespace or make any path queryable.
    pub async fn stage_audited_reconcile(
        &self,
        namespace: Uuid,
        snapshot: &Path,
        projection_scratch_dir: &Path,
        volumes: Vec<ImportVolume<'_>>,
        bindings: &[ImportSourceBinding],
    ) -> Result<StagedImportSummary> {
        self.stage_bound_reconcile(
            namespace,
            snapshot,
            projection_scratch_dir,
            volumes,
            bindings,
            ImportOrigin::LegacySnapshot,
        )
        .await
    }

    /// Import independently verified external media into a private namespace.
    /// Requires an entirely empty frozen old catalog; this is not a claim that
    /// external paths existed in that catalog. Existing routes cannot be edited.
    /// Full source binding, projection verification and three-node certification
    /// are still required before any namespace becomes active.
    pub async fn stage_external_import(
        &self,
        namespace: Uuid,
        snapshot: &Path,
        projection_scratch_dir: &Path,
        volumes: Vec<ImportVolume<'_>>,
        bindings: &[ImportSourceBinding],
    ) -> Result<StagedImportSummary> {
        let serial = &bindings
            .first()
            .ok_or(CatalogError::Invalid("外部导入缺少来源绑定"))?
            .placement
            .library_serial;
        if bindings
            .iter()
            .any(|binding| binding.placement.library_serial != *serial)
        {
            return Err(CatalogError::Invalid("一次外部导入必须限定同一个物理带库"));
        }
        let origin = ImportOrigin::ExternalMedia {
            library_sha256: Sha256::digest(serial.as_bytes()).into(),
        };
        self.stage_bound_reconcile(
            namespace,
            snapshot,
            projection_scratch_dir,
            volumes,
            bindings,
            origin,
        )
        .await
    }

    async fn stage_bound_reconcile(
        &self,
        namespace: Uuid,
        snapshot: &Path,
        projection_scratch_dir: &Path,
        volumes: Vec<ImportVolume<'_>>,
        bindings: &[ImportSourceBinding],
        origin: ImportOrigin,
    ) -> Result<StagedImportSummary> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("导入命名空间不能为 nil"));
        }
        self.verify_import_volume_set(namespace, &volumes, bindings)
            .await?;
        for volume in &volumes {
            volume
                .spool
                .verify_all()
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
        }
        let audit_sources = || {
            let mut merged = ImportReconciler::new(volumes.clone())?;
            match origin {
                ImportOrigin::LegacySnapshot => merged.audit_directory_snapshot(snapshot),
                ImportOrigin::ExternalMedia { .. } => {
                    merged.audit_external_sources(snapshot, &bindings[0].placement.library_serial)
                }
            }
        };
        let audit = audit_sources()?;
        let source_by_barcode = volumes
            .iter()
            .map(|volume| (volume.barcode.as_str(), volume.spool))
            .collect::<std::collections::HashMap<_, _>>();
        let mut projection =
            super::import_materialize::ProjectionScratch::new(namespace, projection_scratch_dir)?;
        self.begin_stage(namespace, audit).await?;
        self.stage_source_bindings(namespace, audit, bindings)
            .await?;
        let mut merged = ImportReconciler::new(volumes.clone())?;
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        let mut paths = 0u64;
        let mut tombstones = 0u64;
        while let Some(path) = merged.next_path()? {
            let mut source_file_uids = Vec::with_capacity(path.sources.len());
            let mut source_data = Vec::with_capacity(path.sources.len());
            let mut winner_attributes = None;
            for (index, candidate) in path.sources.iter().enumerate() {
                let source = source_by_barcode
                    .get(candidate.barcode.as_str())
                    .ok_or(CatalogError::Corrupt("暂存候选缺少来源卷"))?;
                let node = source
                    .node_by_path(&candidate.candidate.physical_path)
                    .map_err(|error| CatalogError::Cache(error.to_string()))?
                    .ok_or(CatalogError::Corrupt("暂存候选缺少物理节点"))?;
                let reconstructed = import_spool::import_candidate(
                    &node.path,
                    node.kind,
                    node.length,
                    &node.attributes,
                )
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
                if reconstructed.as_ref() != Some(&candidate.candidate) {
                    return Err(CatalogError::Corrupt("暂存候选与完整 LTFS 属性不一致"));
                }
                let data_source = if candidate.candidate.deleted
                    || candidate.candidate.kind == NodeKind::Directory
                {
                    StagedSourceData::NoCopy
                } else if let Some(target) = node.attributes.symlink() {
                    if target.contains(".LTFSEE_DATA/") {
                        let data = source
                            .resolve_file_data(&node.path)
                            .map_err(|error| CatalogError::Cache(error.to_string()))?
                            .ok_or(CatalogError::Corrupt("暂存 EE 别名数据文件缺失"))?;
                        StagedSourceData::Data {
                            file_uid: data.data_file_uid,
                            length: data.length,
                            content_digest: data.content_digest,
                            ee_alias: true,
                            data_path: Some(data.data_path),
                        }
                    } else {
                        StagedSourceData::NativeSymlink
                    }
                } else {
                    StagedSourceData::Data {
                        file_uid: node
                            .attributes
                            .meta()
                            .ok_or(CatalogError::Corrupt("暂存来源缺少 LTFS fileuid"))?
                            .file_uid,
                        length: node
                            .length
                            .ok_or(CatalogError::Corrupt("暂存文件长度缺失"))?,
                        content_digest: node
                            .content_digest
                            .ok_or(CatalogError::Corrupt("暂存文件摘要状态缺失"))?,
                        ee_alias: false,
                        data_path: None,
                    }
                };
                let logical_uid = node
                    .attributes
                    .meta()
                    .ok_or(CatalogError::Corrupt("暂存来源缺少 LTFS fileuid"))?
                    .file_uid;
                if !source_data_matches(&candidate.candidate, logical_uid, &data_source, true) {
                    return Err(CatalogError::Corrupt("暂存来源数据对象与候选不一致"));
                }
                source_file_uids.push(logical_uid);
                source_data.push(data_source);
                if index == path.winner {
                    winner_attributes = Some(node.attributes);
                }
            }
            let winner_attributes =
                winner_attributes.ok_or(CatalogError::Corrupt("暂存赢家缺失"))?;
            projection.add_entry(
                &StagedImportEntry {
                    path: path.clone(),
                    current_attributes: winner_attributes.clone(),
                    source_file_uids: source_file_uids.clone(),
                    source_data: Some(source_data.clone()),
                },
                bindings,
            )?;
            let header = self
                .stage_row(
                    namespace,
                    audit,
                    paths,
                    &path,
                    RowEvidence {
                        attributes: Some(&winner_attributes),
                        source_file_uids: Some(&source_file_uids),
                        source_data: Some(&source_data),
                    },
                )
                .await?;
            rows.update(paths.to_be_bytes());
            rows.update(header.sha256);
            paths = paths
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("暂存路径数溢出"))?;
            tombstones += u64::from(path.current().candidate.deleted);
        }
        if paths != audit.paths || tombstones != audit.tombstones {
            return Err(CatalogError::Incomplete);
        }
        for volume in &volumes {
            volume
                .spool
                .verify_all()
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
        }
        if audit_sources()? != audit {
            return Err(CatalogError::Incomplete);
        }
        self.verify_import_volume_set(namespace, &volumes, bindings)
            .await?;
        rows.update(paths.to_be_bytes());
        let summary = StagedImportSummary {
            audit,
            paths,
            rows_sha256: rows.finalize().into(),
            complete: true,
        };
        self.finish_stage(namespace, summary).await?;
        self.finish_source_bindings(namespace, summary, bindings)
            .await?;
        if let Some(expected) = projection.summary()? {
            self.write_staged_projection_expectation(namespace, summary, expected)
                .await?;
        }
        Ok(summary)
    }

    pub async fn staged_import_summary(
        &self,
        namespace: Uuid,
    ) -> Result<Option<StagedImportSummary>> {
        let tr = self.db.create_trx()?;
        tr.get(&manifest_key(namespace), true)
            .await?
            .map(|bytes| StagedImportSummary::decode(&bytes))
            .transpose()
    }

    /// Re-read a completed private import from FDB before materialization.
    /// Pages bound transaction size; the running digest and previous path are
    /// the only state retained across pages. This does not publish any file.
    pub async fn verify_staged_import(&self, namespace: Uuid) -> Result<StagedImportSummary> {
        self.verify_staged_import_inner(namespace, false, None)
            .await
    }

    pub(super) async fn verify_staged_import_inner(
        &self,
        namespace: Uuid,
        require_attributes: bool,
        source_map: Option<&HashMap<String, (Uuid, Uuid, u64)>>,
    ) -> Result<StagedImportSummary> {
        let summary = self
            .staged_import_summary(namespace)
            .await?
            .filter(|summary| summary.complete)
            .ok_or(CatalogError::Incomplete)?;
        if self.namespace_state(namespace).await? != Some(NamespaceState::Building) {
            return Err(CatalogError::Incomplete);
        }
        let mut prefix = key(b'g');
        prefix.extend_from_slice(namespace.as_bytes());
        let mut after = None;
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        let mut paths = 0u64;
        let mut tombstones = 0u64;
        let mut previous: Option<(Uuid, String)> = None;
        loop {
            let tr = self.db.create_trx()?;
            let page = super::read_page(&tr, &prefix, after.as_deref(), 64, true).await?;
            for (entry, raw) in page.items {
                if entry.len() != prefix.len() + 8
                    || u64::from_be_bytes(entry[prefix.len()..].try_into().unwrap()) != paths
                    || paths >= summary.paths
                {
                    return Err(CatalogError::Corrupt("暂存候选行号或数量"));
                }
                let header = RowHeader::decode(&raw)?;
                let path = if header.chunks == 0 {
                    let (path, attributes, source_file_uids, source_data) =
                        decode_entry(&raw[ROW_HEADER_LEN..])?;
                    if require_attributes
                        && (attributes.is_none()
                            || source_file_uids.is_none()
                            || (source_map.is_some() && source_data.is_none()))
                    {
                        return Err(CatalogError::Incomplete);
                    }
                    path
                } else if require_attributes {
                    let entry = self
                        .staged_entry(namespace, paths)
                        .await?
                        .ok_or(CatalogError::Corrupt("暂存候选分块行缺失"))?;
                    if source_map.is_some() && entry.source_data.is_none() {
                        return Err(CatalogError::Incomplete);
                    }
                    entry.path
                } else {
                    self.staged_path(namespace, paths)
                        .await?
                        .ok_or(CatalogError::Corrupt("暂存候选分块行缺失"))?
                };
                if let Some(source_map) = source_map
                    && !path_sources_match(&path, source_map)
                {
                    return Err(CatalogError::Incomplete);
                }
                let order = (path.pool_id, path.logical_path);
                if previous.as_ref().is_some_and(|prior| prior >= &order) {
                    return Err(CatalogError::Corrupt("暂存候选路径乱序或重复"));
                }
                tombstones += u64::from(path.sources[path.winner].candidate.deleted);
                previous = Some(order);
                rows.update(paths.to_be_bytes());
                rows.update(header.sha256);
                paths += 1;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        rows.update(paths.to_be_bytes());
        if paths != summary.paths
            || tombstones != summary.audit.tombstones
            || <[u8; 32]>::from(rows.finalize()) != summary.rows_sha256
            || self.staged_import_summary(namespace).await? != Some(summary)
            || self.namespace_state(namespace).await? != Some(NamespaceState::Building)
        {
            return Err(CatalogError::Incomplete);
        }
        Ok(summary)
    }

    /// Confirm an un-managed import. Managed imports must use
    /// `confirm_bound_staged_import` so every source has durable scan evidence.
    pub async fn confirm_staged_import(&self, namespace: Uuid) -> Result<StagedImportSummary> {
        let summary = self
            .verify_staged_import_inner(namespace, true, None)
            .await?;
        let proof = confirmed_key(namespace);
        let manifest = manifest_key(namespace);
        let state = namespace_key(namespace);
        let managed = control::managed_key(namespace);
        let expected = summary.encode();
        self.db
            .run(|tr, _| {
                let (proof, manifest, state, managed) = (
                    proof.clone(),
                    manifest.clone(),
                    state.clone(),
                    managed.clone(),
                );
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr.get(&managed, false).await?.is_some()
                        || tr.get(&manifest, false).await?.as_deref() != Some(expected.as_slice())
                        || tr
                            .get(
                                &super::staged_sources::binding_required_key(namespace),
                                false,
                            )
                            .await?
                            .is_some()
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    match tr.get(&proof, false).await? {
                        Some(saved) if saved.as_ref() == expected => Ok(Ok(())),
                        Some(_) => Ok(Err(CatalogError::Corrupt("导入物化证明与已暂存证据冲突"))),
                        None => {
                            tr.set(&proof, &expected);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await??;
        Ok(summary)
    }

    pub(super) async fn confirmed_staged_import(
        &self,
        namespace: Uuid,
    ) -> Result<StagedImportSummary> {
        let tr = self.db.create_trx()?;
        if tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Building as u8][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let manifest = tr
            .get(&manifest_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let proof = tr
            .get(&confirmed_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if manifest.as_ref() != proof.as_ref() {
            return Err(CatalogError::Incomplete);
        }
        let summary = StagedImportSummary::decode(&manifest)?;
        if !summary.complete {
            return Err(CatalogError::Incomplete);
        }
        let managed = tr.get(&control::managed_key(namespace), false).await?;
        if let Some(managed) = managed {
            if !managed.is_empty()
                || tr
                    .get(
                        &super::staged_sources::binding_required_key(namespace),
                        false,
                    )
                    .await?
                    .as_deref()
                    != Some(super::staged_sources::REQUIRED_VERSION)
            {
                return Err(CatalogError::Incomplete);
            }
            let sources = tr
                .get(&super::staged_sources::summary_key(namespace), false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            if tr
                .get(&super::staged_sources::confirmed_key(namespace), false)
                .await?
                .as_deref()
                != Some(sources.as_ref())
                || super::staged_sources::StagedSourceSummary::decode(&sources)?.staged != summary
            {
                return Err(CatalogError::Incomplete);
            }
        } else if tr
            .get(
                &super::staged_sources::binding_required_key(namespace),
                false,
            )
            .await?
            .is_some()
        {
            return Err(CatalogError::Corrupt("非受管导入存在来源绑定标记"));
        }
        Ok(summary)
    }

    /// Independently re-read one completed staged row without materializing
    /// its possibly large payload in memory. This checks every FDB value chunk.
    pub async fn staged_row_sha256(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<[u8; 32]>> {
        let tr = self.db.create_trx()?;
        let Some(raw) = tr.get(&row_key(namespace, ordinal), true).await? else {
            return Ok(None);
        };
        let header = RowHeader::decode(&raw)?;
        if header.chunks == 0 {
            return Ok(Some(header.sha256));
        }
        let mut hashed = Sha256::new();
        let mut length = 0u64;
        for chunk in 0..header.chunks {
            let tr = self.db.create_trx()?;
            let bytes = tr
                .get(&chunk_key(namespace, ordinal, chunk), true)
                .await?
                .ok_or(CatalogError::Corrupt("暂存候选分块缺失"))?;
            let expected = if chunk + 1 == header.chunks {
                (header.len - length) as usize
            } else {
                VALUE_CHUNK
            };
            if bytes.len() != expected || bytes.is_empty() {
                return Err(CatalogError::Corrupt("暂存候选分块长度"));
            }
            length += bytes.len() as u64;
            hashed.update(&bytes);
        }
        if length != header.len || <[u8; 32]>::from(hashed.finalize()) != header.sha256 {
            return Err(CatalogError::Corrupt("暂存候选分块摘要"));
        }
        Ok(Some(header.sha256))
    }

    /// Reconstruct one staged logical path for the later materialization pass.
    /// Memory use is proportional to this path's physical-copy descriptions,
    /// never to the number of paths in the library.
    pub async fn staged_path(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<ReconciledPath>> {
        Ok(self
            .read_staged_entry(namespace, ordinal)
            .await?
            .map(|(path, _, _, _)| path))
    }

    /// Return complete current LTFS attributes and every source's fileuid.
    /// Older rows lacking either cannot enter materialization.
    pub async fn staged_entry(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<StagedImportEntry>> {
        self.read_staged_entry(namespace, ordinal)
            .await?
            .map(|(path, attributes, source_file_uids, source_data)| {
                Ok(StagedImportEntry {
                    path,
                    current_attributes: attributes.ok_or(CatalogError::Incomplete)?,
                    source_file_uids: source_file_uids.ok_or(CatalogError::Incomplete)?,
                    source_data,
                })
            })
            .transpose()
    }

    async fn read_staged_entry(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<DecodedEntry>> {
        let tr = self.db.create_trx()?;
        let Some(raw) = tr.get(&row_key(namespace, ordinal), true).await? else {
            return Ok(None);
        };
        let header = RowHeader::decode(&raw)?;
        let data = if header.chunks == 0 {
            raw[ROW_HEADER_LEN..].to_vec()
        } else {
            let mut data = Vec::new();
            let mut hashed = Sha256::new();
            for chunk in 0..header.chunks {
                let tr = self.db.create_trx()?;
                let bytes = tr
                    .get(&chunk_key(namespace, ordinal, chunk), true)
                    .await?
                    .ok_or(CatalogError::Corrupt("暂存候选分块缺失"))?;
                if bytes.is_empty()
                    || bytes.len() > VALUE_CHUNK
                    || (chunk + 1 < header.chunks && bytes.len() != VALUE_CHUNK)
                {
                    return Err(CatalogError::Corrupt("暂存候选分块长度"));
                }
                hashed.update(&bytes);
                data.extend_from_slice(&bytes);
                if data.len() as u64 > header.len {
                    return Err(CatalogError::Corrupt("暂存候选分块总长度"));
                }
            }
            if data.len() as u64 != header.len
                || <[u8; 32]>::from(hashed.finalize()) != header.sha256
            {
                return Err(CatalogError::Corrupt("暂存候选分块摘要"));
            }
            data
        };
        decode_entry(&data).map(Some)
    }

    async fn begin_stage(&self, namespace: Uuid, audit: DirectoryAudit) -> Result<()> {
        let manifest = manifest_key(namespace);
        let state = namespace_key(namespace);
        self.db
            .run(|tr, _| {
                let (manifest, state) = (manifest.clone(), state.clone());
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    if let Some(saved) = tr.get(&manifest, false).await? {
                        let saved = match StagedImportSummary::decode(&saved) {
                            Ok(saved) => saved,
                            Err(error) => return Ok(Err(error)),
                        };
                        if saved.audit != audit {
                            return Ok(Err(CatalogError::Invalid(
                                "暂存导入审计证据与已有任务冲突",
                            )));
                        }
                    } else {
                        tr.set(&manifest, &StagedImportSummary::initial(audit).encode());
                    }
                    Ok(Ok(()))
                }
            })
            .await?
    }

    async fn write_stage_value(
        &self,
        namespace: Uuid,
        audit: DirectoryAudit,
        entry: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<()> {
        let manifest = manifest_key(namespace);
        let state = namespace_key(namespace);
        self.db
            .run(|tr, _| {
                let (manifest, state, entry, value) = (
                    manifest.clone(),
                    state.clone(),
                    entry.clone(),
                    value.clone(),
                );
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let saved = tr.get(&manifest, false).await?;
                    let saved = match saved
                        .as_deref()
                        .map(StagedImportSummary::decode)
                        .transpose()
                    {
                        Ok(Some(saved)) if saved.audit == audit => saved,
                        Ok(_) => return Ok(Err(CatalogError::Incomplete)),
                        Err(error) => return Ok(Err(error)),
                    };
                    match tr.get(&entry, false).await? {
                        Some(existing) if existing.as_ref() != value => {
                            Ok(Err(CatalogError::Corrupt("暂存候选重试内容冲突")))
                        }
                        Some(_) => Ok(Ok(())),
                        None if saved.complete => Ok(Err(CatalogError::Incomplete)),
                        None => {
                            tr.set(&entry, &value);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await?
    }

    async fn write_stage_final(
        &self,
        namespace: Uuid,
        audit: DirectoryAudit,
        chunk: Option<(Vec<u8>, Vec<u8>)>,
        row: Vec<u8>,
        row_value: Vec<u8>,
    ) -> Result<()> {
        let manifest = manifest_key(namespace);
        let state = namespace_key(namespace);
        self.db
            .run(|tr, _| {
                let (manifest, state, chunk, row) =
                    (manifest.clone(), state.clone(), chunk.clone(), row.clone());
                let row_value = row_value.clone();
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let saved = tr.get(&manifest, false).await?;
                    let saved = match saved
                        .as_deref()
                        .map(StagedImportSummary::decode)
                        .transpose()
                    {
                        Ok(Some(saved)) if saved.audit == audit => saved,
                        Ok(_) => return Ok(Err(CatalogError::Incomplete)),
                        Err(error) => return Ok(Err(error)),
                    };
                    let old_chunk = if let Some((key, _)) = &chunk {
                        tr.get(key, false).await?
                    } else {
                        None
                    };
                    let old_row = tr.get(&row, false).await?;
                    if chunk
                        .as_ref()
                        .zip(old_chunk.as_ref())
                        .is_some_and(|((_, value), old)| old.as_ref() != value)
                        || old_row
                            .as_ref()
                            .is_some_and(|old| old.as_ref() != row_value)
                    {
                        return Ok(Err(CatalogError::Corrupt("暂存候选重试内容冲突")));
                    }
                    if saved.complete
                        && (chunk.is_some() && old_chunk.is_none() || old_row.is_none())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    if let Some((key, value)) = chunk
                        && old_chunk.is_none()
                    {
                        tr.set(&key, &value);
                    }
                    if old_row.is_none() {
                        tr.set(&row, &row_value);
                    }
                    Ok(Ok(()))
                }
            })
            .await?
    }

    async fn stage_row(
        &self,
        namespace: Uuid,
        audit: DirectoryAudit,
        ordinal: u64,
        path: &ReconciledPath,
        evidence: RowEvidence<'_>,
    ) -> Result<RowHeader> {
        let RowEvidence {
            attributes,
            source_file_uids,
            source_data,
        } = evidence;
        if path.pool_id.is_nil() || path.winner >= path.sources.len() {
            return Err(CatalogError::Invalid("暂存候选身份无效"));
        }
        if source_file_uids.is_some_and(|uids| {
            attributes.is_none() || uids.len() != path.sources.len() || uids.contains(&0)
        }) {
            return Err(CatalogError::Invalid("暂存来源 fileuid 无效"));
        }
        if source_file_uids.is_some_and(|uids| {
            attributes
                .and_then(NodeAttributes::meta)
                .map(|meta| meta.file_uid)
                != Some(uids[path.winner])
        }) {
            return Err(CatalogError::Invalid("暂存赢家 fileuid 与属性不一致"));
        }
        if let Some(data) = source_data {
            let uids = source_file_uids.ok_or(CatalogError::Invalid("暂存数据缺少逻辑 fileuid"))?;
            if data.len() != path.sources.len()
                || attributes.is_none()
                || data.iter().enumerate().any(|(index, source)| {
                    !source_data_matches(&path.sources[index].candidate, uids[index], source, true)
                })
                || !winner_data_matches(
                    &path.sources[path.winner].candidate,
                    attributes.expect("checked above"),
                    &data[path.winner],
                    true,
                )
            {
                return Err(CatalogError::Invalid("暂存数据对象与候选不一致"));
            }
        }
        let winner = u32::try_from(path.winner)
            .map_err(|_| CatalogError::Invalid("暂存候选副本数量过大"))?;
        let count = u32::try_from(path.sources.len())
            .map_err(|_| CatalogError::Invalid("暂存候选副本数量过大"))?;
        let mut writer = RowWriter {
            catalog: self,
            namespace,
            audit,
            ordinal,
            pending: Vec::with_capacity(VALUE_CHUNK),
            len: 0,
            chunks: 0,
            sha: Sha256::new(),
        };
        writer
            .push(&[if source_data.is_some() {
                5
            } else if source_file_uids.is_some() {
                3
            } else if attributes.is_some() {
                2
            } else {
                1
            }])
            .await?;
        writer.push(path.pool_id.as_bytes()).await?;
        writer.field(path.logical_path.as_bytes()).await?;
        writer.push(&winner.to_be_bytes()).await?;
        writer.push(&count.to_be_bytes()).await?;
        for (index, source) in path.sources.iter().enumerate() {
            let candidate = &source.candidate;
            if candidate.logical_path != path.logical_path
                || source.barcode.is_empty()
                || source.barcode.len() > 32
                || !source.barcode.is_ascii()
                || source.volume_id.is_nil()
                || source.generation == 0
            {
                return Err(CatalogError::Invalid("暂存候选来源无效"));
            }
            writer.field(source.barcode.as_bytes()).await?;
            writer.push(source.volume_id.as_bytes()).await?;
            writer.push(&source.generation.to_be_bytes()).await?;
            writer.field(candidate.physical_path.as_bytes()).await?;
            writer
                .push(&[u8::from(candidate.kind == super::NodeKind::File)])
                .await?;
            writer.push(&[u8::from(candidate.deleted)]).await?;
            writer.push(&candidate.version.0.to_be_bytes()).await?;
            writer.push(&candidate.version.1.to_be_bytes()).await?;
            writer.push(&candidate.length.to_be_bytes()).await?;
            writer.field(candidate.sha256.as_bytes()).await?;
            if let Some(uids) = source_file_uids {
                writer.push(&uids[index].to_be_bytes()).await?;
            }
            if let Some(data) = source_data {
                writer.push(&encode_source_data(&data[index])?).await?;
            }
        }
        if let Some(attributes) = attributes {
            writer
                .field(
                    attributes
                        .bytes()
                        .ok_or(CatalogError::Invalid("不能暂存未读取的 LTFS 属性"))?,
                )
                .await?;
        }
        writer.finish().await
    }

    async fn finish_stage(&self, namespace: Uuid, summary: StagedImportSummary) -> Result<()> {
        let manifest = manifest_key(namespace);
        let state = namespace_key(namespace);
        self.db
            .run(|tr, _| {
                let (manifest, state) = (manifest.clone(), state.clone());
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let saved = tr.get(&manifest, false).await?;
                    let saved = match saved
                        .as_deref()
                        .map(StagedImportSummary::decode)
                        .transpose()
                    {
                        Ok(Some(saved)) if saved.audit == summary.audit => saved,
                        Ok(_) => return Ok(Err(CatalogError::Incomplete)),
                        Err(error) => return Ok(Err(error)),
                    };
                    if saved.complete {
                        if saved != summary {
                            return Ok(Err(CatalogError::Corrupt("暂存完成摘要冲突")));
                        }
                    } else {
                        tr.set(&manifest, &summary.encode());
                    }
                    Ok(Ok(()))
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{NodeMeta, Xattr};
    use crate::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION, tombstone_path};
    use crate::managed_catalog::{ImportCandidate, NodeKind, SourceCandidate};

    #[test]
    fn manifest_seals_import_origin_and_rejects_old_or_unknown_format() {
        let mut audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 2,
            frozen_index: 1,
            paths: 0,
            tombstones: 0,
            rows_sha256: [1; 32],
            snapshot_sha256: [2; 32],
        };
        let legacy = StagedImportSummary::initial(audit);
        audit.origin = ImportOrigin::ExternalMedia {
            library_sha256: [3; 32],
        };
        let external = StagedImportSummary::initial(audit);
        for summary in [legacy, external] {
            let encoded = summary.encode();
            assert_eq!(StagedImportSummary::decode(&encoded).unwrap(), summary);
            for n in 0..encoded.len() {
                assert!(StagedImportSummary::decode(&encoded[..n]).is_err());
            }
        }
        assert_ne!(
            Sha256::digest(legacy.encode()),
            Sha256::digest(external.encode())
        );
        let mut old = legacy.encode().to_vec();
        old[0] = 2;
        old.truncate(138);
        assert!(StagedImportSummary::decode(&old).is_err());
        let mut unknown = external.encode();
        unknown[138] = 2;
        assert!(StagedImportSummary::decode(&unknown).is_err());
    }

    #[test]
    fn ee_data_source_roundtrips_without_using_alias_uid_as_extent_uid() {
        let candidate = ImportCandidate {
            logical_path: "/user/file".into(),
            physical_path: "user/file".into(),
            kind: NodeKind::File,
            deleted: false,
            version: (9, 2),
            length: 0,
            sha256: String::new(),
        };
        let source = StagedSourceData::Data {
            file_uid: 123,
            length: 1_048_576,
            content_digest: ContentDigest::Sha256([7; 32]),
            ee_alias: true,
            data_path: Some(".LTFSEE_DATA/object".into()),
        };
        assert!(source_data_matches(&candidate, 456, &source, true));
        assert!(!source_data_matches(&candidate, 123, &source, true));
        let mut invalid_target = source.clone();
        if let StagedSourceData::Data { data_path, .. } = &mut invalid_target {
            *data_path = Some(".LTFSEE_DATA/../other".into());
        }
        assert!(!source_data_matches(&candidate, 456, &invalid_target, true));
        let mut missing_target = source.clone();
        if let StagedSourceData::Data { data_path, .. } = &mut missing_target {
            *data_path = None;
        }
        assert!(!source_data_matches(&candidate, 456, &missing_target, true));
        assert!(!source_data_matches(
            &ImportCandidate {
                length: 1,
                ..candidate.clone()
            },
            456,
            &source,
            true,
        ));
        let encoded = encode_source_data(&source).unwrap();
        let mut at = 0;
        assert_eq!(decode_source_data(&encoded, &mut at, 5).unwrap(), source);
        assert_eq!(at, encoded.len());
        for truncated in 0..encoded.len() {
            let mut at = 0;
            assert!(decode_source_data(&encoded[..truncated], &mut at, 5).is_err());
        }
        let mut invalid = encoded.clone();
        invalid[17] = 3;
        let mut at = 0;
        assert!(decode_source_data(&invalid, &mut at, 5).is_err());
        let mut invalid_path = encoded;
        invalid_path.truncate(invalid_path.len() - 1);
        let mut at = 0;
        assert!(decode_source_data(&invalid_path, &mut at, 5).is_err());
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅暂存合成 EE 别名元数据"]
    async fn ee_alias_data_identity_survives_staged_fdb_row() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        assert!(catalog.begin_import(namespace).await.unwrap());
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 1,
            tombstones: 0,
            rows_sha256: [9; 32],
            snapshot_sha256: [8; 32],
        };
        catalog.begin_stage(namespace, audit).await.unwrap();
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 33,
                ..Default::default()
            },
            vec![Xattr {
                key: XATTR_VERSION.into(),
                value: "2.2".into(),
                base64: false,
            }],
            Some("../.LTFSEE_DATA/object".into()),
        )
        .unwrap();
        let candidate =
            import_spool::import_candidate("dir/alias", NodeKind::File, Some(0), &attributes)
                .unwrap()
                .unwrap();
        let path = ReconciledPath {
            pool_id: Uuid::new_v4(),
            logical_path: candidate.logical_path.clone(),
            sources: vec![SourceCandidate {
                barcode: "EEALIASL8".into(),
                volume_id: Uuid::new_v4(),
                generation: 2,
                candidate,
            }],
            winner: 0,
        };
        let data = StagedSourceData::Data {
            file_uid: 3,
            length: 1_048_576,
            content_digest: ContentDigest::Sha256([5; 32]),
            ee_alias: true,
            data_path: Some(".LTFSEE_DATA/object".into()),
        };
        let mut wrong_data = data.clone();
        if let StagedSourceData::Data { data_path, .. } = &mut wrong_data {
            *data_path = Some(".LTFSEE_DATA/other".into());
        }
        assert!(
            catalog
                .stage_row(
                    namespace,
                    audit,
                    0,
                    &path,
                    RowEvidence {
                        attributes: Some(&attributes),
                        source_file_uids: Some(&[33]),
                        source_data: Some(&[wrong_data]),
                    },
                )
                .await
                .is_err()
        );
        let header = catalog
            .stage_row(
                namespace,
                audit,
                0,
                &path,
                RowEvidence {
                    attributes: Some(&attributes),
                    source_file_uids: Some(&[33]),
                    source_data: Some(std::slice::from_ref(&data)),
                },
            )
            .await
            .unwrap();
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        rows.update(0u64.to_be_bytes());
        rows.update(header.sha256);
        rows.update(1u64.to_be_bytes());
        let summary = StagedImportSummary {
            audit,
            paths: 1,
            rows_sha256: rows.finalize().into(),
            complete: true,
        };
        catalog.finish_stage(namespace, summary).await.unwrap();
        assert_eq!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .staged_entry(namespace, 0)
                .await
                .unwrap(),
            Some(StagedImportEntry {
                path,
                current_attributes: attributes,
                source_file_uids: vec![33],
                source_data: Some(vec![data]),
            })
        );
        assert_eq!(
            catalog.verify_staged_import(namespace).await.unwrap(),
            summary
        );
        assert!(catalog.discard_import(namespace).await.unwrap());
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；构造旧版受管暂存证明验证拒绝"]
    async fn old_markers_cannot_authorize_managed_import() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        assert!(catalog.begin_managed_import(namespace).await.unwrap());
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 0,
            tombstones: 0,
            rows_sha256: [1; 32],
            snapshot_sha256: [8; 32],
        };
        catalog.begin_stage(namespace, audit).await.unwrap();
        let summary = StagedImportSummary {
            audit,
            paths: 0,
            rows_sha256: [2; 32],
            complete: true,
        };
        catalog.finish_stage(namespace, summary).await.unwrap();
        let mut source_summary = vec![1];
        source_summary.extend_from_slice(&summary.encode());
        source_summary.extend_from_slice(&0u64.to_be_bytes());
        source_summary.extend_from_slice(&[0; 32]);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&confirmed_key(namespace), &summary.encode());
        tr.set(
            &super::super::staged_sources::binding_required_key(namespace),
            &[],
        );
        tr.set(
            &super::super::staged_sources::summary_key(namespace),
            &source_summary,
        );
        tr.set(
            &super::super::staged_sources::confirmed_key(namespace),
            &source_summary,
        );
        tr.commit().await.unwrap();
        assert!(catalog.confirmed_staged_import(namespace).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &super::super::staged_sources::binding_required_key(namespace),
            &[4],
        );
        tr.commit().await.unwrap();
        assert!(catalog.confirmed_staged_import(namespace).await.is_err());
        assert!(catalog.discard_import(namespace).await.unwrap());
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
    }
    #[test]
    fn bound_candidate_requires_exact_pool_volume_and_generation() {
        let pool = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let path = ReconciledPath {
            pool_id: pool,
            logical_path: "/file".into(),
            sources: vec![SourceCandidate {
                barcode: "EXACTL8".into(),
                volume_id: volume,
                generation: 3,
                candidate: ImportCandidate {
                    logical_path: "/file".into(),
                    physical_path: "file".into(),
                    kind: NodeKind::File,
                    deleted: false,
                    version: (1, 1),
                    length: 1,
                    sha256: String::new(),
                },
            }],
            winner: 0,
        };
        let mut sources = HashMap::from([("EXACTL8".into(), (pool, volume, 3))]);
        assert!(path_sources_match(&path, &sources));
        sources.insert("EXACTL8".into(), (pool, volume, 2));
        assert!(!path_sources_match(&path, &sources));
        sources.insert("EXACTL8".into(), (pool, Uuid::new_v4(), 3));
        assert!(!path_sources_match(&path, &sources));
        sources.insert("EXACTL8".into(), (Uuid::new_v4(), volume, 3));
        assert!(!path_sources_match(&path, &sources));
        sources.clear();
        assert!(!path_sources_match(&path, &sources));
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；只使用合成目录"]
    async fn confirmed_directories_materialize_in_order_and_replay() {
        use crate::managed_catalog::{ControlOutcome, PoolPolicy, PoolRecord};

        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        let pool = Uuid::new_v4();
        catalog.begin_import(namespace).await.unwrap();
        catalog
            .register_physical_library(namespace, "STAGED_TEST")
            .await
            .unwrap();
        assert_eq!(
            catalog
                .create_pool(
                    namespace,
                    &PoolRecord {
                        id: pool,
                        library_serial: "STAGED_TEST".into(),
                        name: "import-test".into(),
                        file_limit: 100,
                        policy: PoolPolicy::default(),
                        revision: 1,
                    },
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 2,
            tombstones: 0,
            rows_sha256: [9; 32],
            snapshot_sha256: [8; 32],
        };
        catalog.begin_stage(namespace, audit).await.unwrap();
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        for (ordinal, (path, uid)) in [("/a", 2), ("/a/b", 3)].into_iter().enumerate() {
            let physical = path.trim_start_matches('/');
            let attributes = NodeAttributes::new(
                NodeMeta {
                    file_uid: uid,
                    ..Default::default()
                },
                vec![Xattr {
                    key: XATTR_VERSION.into(),
                    value: "1.1".into(),
                    base64: false,
                }],
                None,
            )
            .unwrap();
            let candidate =
                import_spool::import_candidate(physical, NodeKind::Directory, None, &attributes)
                    .unwrap()
                    .unwrap();
            let entry = ReconciledPath {
                pool_id: pool,
                logical_path: path.into(),
                sources: vec![SourceCandidate {
                    barcode: "SYNTHL8".into(),
                    volume_id: Uuid::from_u128(7),
                    generation: 1,
                    candidate,
                }],
                winner: 0,
            };
            let header = catalog
                .stage_row(
                    namespace,
                    audit,
                    ordinal as u64,
                    &entry,
                    RowEvidence {
                        attributes: Some(&attributes),
                        source_file_uids: Some(&[uid]),
                        source_data: None,
                    },
                )
                .await
                .unwrap();
            rows.update((ordinal as u64).to_be_bytes());
            rows.update(header.sha256);
        }
        rows.update(2u64.to_be_bytes());
        let summary = StagedImportSummary {
            audit,
            paths: 2,
            rows_sha256: rows.finalize().into(),
            complete: true,
        };
        catalog.finish_stage(namespace, summary).await.unwrap();
        assert!(
            catalog
                .materialize_staged_directory(namespace, 0)
                .await
                .is_err()
        );
        assert_eq!(
            catalog.confirm_staged_import(namespace).await.unwrap(),
            summary
        );
        assert_eq!(
            catalog.confirm_staged_import(namespace).await.unwrap(),
            summary
        );
        assert!(
            catalog
                .materialize_staged_directory(namespace, 1)
                .await
                .is_err()
        );
        let parent = catalog
            .materialize_staged_directory(namespace, 0)
            .await
            .unwrap()
            .unwrap();
        let child = catalog
            .materialize_staged_directory(namespace, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            catalog
                .materialize_staged_directory(namespace, 0)
                .await
                .unwrap(),
            Some(parent)
        );
        assert_eq!(
            catalog
                .materialize_staged_directory(namespace, 1)
                .await
                .unwrap(),
            Some(child)
        );
        assert!(catalog.resolve_path(namespace, b"/a/b").await.is_err());
        assert!(catalog.seal_import(namespace).await.is_err());
        assert!(catalog.discard_import(namespace).await.unwrap());
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
        assert!(
            catalog
                .staged_import_summary(namespace)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；只使用合成旧导入证明"]
    async fn old_managed_directory_proof_cannot_skip_source_binding() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        catalog.begin_managed_import(namespace).await.unwrap();
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 0,
            tombstones: 0,
            rows_sha256: [9; 32],
            snapshot_sha256: [8; 32],
        };
        catalog.begin_stage(namespace, audit).await.unwrap();
        let summary = StagedImportSummary {
            audit,
            paths: 0,
            rows_sha256: [7; 32],
            complete: true,
        };
        catalog.finish_stage(namespace, summary).await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&confirmed_key(namespace), &summary.encode());
        tr.commit().await.unwrap();
        assert!(catalog.confirm_staged_import(namespace).await.is_err());
        assert!(catalog.confirmed_staged_import(namespace).await.is_err());
        assert!(
            catalog
                .materialize_staged_directory(namespace, 0)
                .await
                .is_err()
        );
        assert!(catalog.discard_import(namespace).await.unwrap());
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
    }

    #[test]
    fn complete_tombstone_attributes_roundtrip_and_reject_mismatch() {
        let physical = tombstone_path("/old");
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 5,
                ..Default::default()
            },
            vec![
                Xattr {
                    key: XATTR_VERSION.into(),
                    value: "2.2".into(),
                    base64: false,
                },
                Xattr {
                    key: XATTR_DELETED_PATH.into(),
                    value: "/old".into(),
                    base64: false,
                },
                Xattr {
                    key: "tapers.deletedKind".into(),
                    value: "directory".into(),
                    base64: false,
                },
            ],
            None,
        )
        .unwrap();
        let candidate =
            import_spool::import_candidate(&physical, NodeKind::File, Some(0), &attributes)
                .unwrap()
                .unwrap();
        assert!(candidate.deleted);
        assert_eq!(candidate.kind, NodeKind::Directory);
        let pool = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let mut encoded = Vec::new();
        let field = |out: &mut Vec<u8>, value: &[u8]| {
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        };
        encoded.push(3);
        encoded.extend_from_slice(pool.as_bytes());
        field(&mut encoded, candidate.logical_path.as_bytes());
        encoded.extend_from_slice(&0u32.to_be_bytes());
        encoded.extend_from_slice(&1u32.to_be_bytes());
        field(&mut encoded, b"TOMBL8");
        encoded.extend_from_slice(volume.as_bytes());
        encoded.extend_from_slice(&1u64.to_be_bytes());
        field(&mut encoded, physical.as_bytes());
        encoded.extend_from_slice(&[0, 1]);
        encoded.extend_from_slice(&candidate.version.0.to_be_bytes());
        encoded.extend_from_slice(&candidate.version.1.to_be_bytes());
        encoded.extend_from_slice(&0u64.to_be_bytes());
        field(&mut encoded, b"");
        let uid_at = encoded.len();
        encoded.extend_from_slice(&5u64.to_be_bytes());
        field(&mut encoded, attributes.bytes().unwrap());
        let (decoded, decoded_attributes, decoded_uids, decoded_data) =
            decode_entry(&encoded).unwrap();
        assert_eq!(decoded.pool_id, pool);
        assert_eq!(decoded.current().candidate, candidate);
        assert_eq!(decoded_attributes, Some(attributes.clone()));
        assert_eq!(decoded_uids, Some(vec![5]));
        assert_eq!(decoded_data, None);
        let mut current = encoded.clone();
        current[0] = 4;
        current.insert(uid_at + 8, 0);
        let (_, _, _, current_data) = decode_entry(&current).unwrap();
        assert_eq!(current_data, Some(vec![StagedSourceData::NoCopy]));
        current[uid_at + 8] = 3;
        assert!(decode_entry(&current).is_err());
        let mut wrong_uid = encoded.clone();
        wrong_uid[uid_at + 7] ^= 1;
        assert!(decode_entry(&wrong_uid).is_err());
        let mut old = encoded.clone();
        old[0] = 2;
        old.drain(uid_at..uid_at + 8);
        let (_, old_attributes, old_uids, old_data) = decode_entry(&old).unwrap();
        assert_eq!(old_attributes, Some(attributes));
        assert_eq!(old_uids, None);
        assert_eq!(old_data, None);
        let at = encoded
            .windows(4)
            .rposition(|window| window == b"/old")
            .unwrap();
        encoded[at..at + 4].copy_from_slice(b"/new");
        assert!(decode_entry(&encoded).is_err());
    }

    #[test]
    fn staged_headers_reject_truncation_and_impossible_chunk_counts() {
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 2,
            tombstones: 1,
            rows_sha256: [3; 32],
            snapshot_sha256: [8; 32],
        };
        let initial = StagedImportSummary::initial(audit);
        assert_eq!(
            StagedImportSummary::decode(&initial.encode()).unwrap(),
            initial
        );
        let mut old = initial.encode();
        old[0] = 1;
        assert!(StagedImportSummary::decode(&old).is_err());
        let mut bad_freeze = initial.encode();
        bad_freeze[10..18].copy_from_slice(&2u64.to_be_bytes());
        assert!(StagedImportSummary::decode(&bad_freeze).is_err());
        let mut bad_digest = initial.encode();
        bad_digest[66..98].fill(0);
        assert!(StagedImportSummary::decode(&bad_digest).is_err());
        assert!(StagedImportSummary::decode(&initial.encode()[..MANIFEST_LEN - 1]).is_err());
        let row = RowHeader {
            len: 64_001,
            chunks: 2,
            sha256: [4; 32],
        };
        assert_eq!(RowHeader::decode(&row.encode()).unwrap().chunks, 2);
        let mut bad = row.encode();
        bad[12] = 1;
        assert!(RowHeader::decode(&bad).is_err());
        let payload = b"inline";
        let inline = RowHeader {
            len: payload.len() as u64,
            chunks: 0,
            sha256: Sha256::digest(payload).into(),
        };
        let mut stored = inline.encode().to_vec();
        stored.extend_from_slice(payload);
        assert_eq!(RowHeader::decode(&stored).unwrap().chunks, 0);
        stored[ROW_HEADER_LEN] ^= 1;
        assert!(RowHeader::decode(&stored).is_err());
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；验证大候选分块与重读"]
    async fn oversized_candidate_is_chunked_without_a_large_fdb_value() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        catalog.begin_managed_import(namespace).await.unwrap();
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 1,
            frozen_index: 1,
            paths: 1,
            tombstones: 0,
            rows_sha256: [7; 32],
            snapshot_sha256: [8; 32],
        };
        catalog.begin_stage(namespace, audit).await.unwrap();
        let long_path = format!("/{}", vec!["x".repeat(255); 400].join("/"));
        let candidate = ImportCandidate {
            logical_path: long_path.clone(),
            physical_path: long_path[1..].to_string(),
            kind: NodeKind::File,
            deleted: false,
            version: (1, 1),
            length: 0,
            sha256: String::new(),
        };
        let path = ReconciledPath {
            pool_id: Uuid::new_v4(),
            logical_path: long_path,
            sources: vec![SourceCandidate {
                barcode: "LARGEL8".into(),
                volume_id: Uuid::new_v4(),
                generation: 1,
                candidate,
            }],
            winner: 0,
        };
        let header = catalog
            .stage_row(
                namespace,
                audit,
                0,
                &path,
                RowEvidence {
                    attributes: None,
                    source_file_uids: None,
                    source_data: None,
                },
            )
            .await
            .unwrap();
        assert!(header.chunks >= 4);
        assert_eq!(
            catalog.staged_row_sha256(namespace, 0).await.unwrap(),
            Some(header.sha256)
        );
        assert_eq!(
            catalog.staged_path(namespace, 0).await.unwrap(),
            Some(path.clone())
        );
        assert!(catalog.staged_entry(namespace, 0).await.is_err());
        assert_eq!(
            catalog
                .stage_row(
                    namespace,
                    audit,
                    0,
                    &path,
                    RowEvidence {
                        attributes: None,
                        source_file_uids: None,
                        source_data: None,
                    },
                )
                .await
                .unwrap()
                .sha256,
            header.sha256
        );
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        rows.update(0u64.to_be_bytes());
        rows.update(header.sha256);
        rows.update(1u64.to_be_bytes());
        let summary = StagedImportSummary {
            audit,
            paths: 1,
            rows_sha256: rows.finalize().into(),
            complete: true,
        };
        catalog.finish_stage(namespace, summary).await.unwrap();
        assert_eq!(
            catalog.verify_staged_import(namespace).await.unwrap(),
            summary
        );
        assert!(catalog.confirm_staged_import(namespace).await.is_err());
        let first_chunk = chunk_key(namespace, 0, 0);
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&first_chunk, false).await.unwrap().unwrap().to_vec();
        let mut corrupted = original.clone();
        corrupted[0] ^= 1;
        tr.set(&first_chunk, &corrupted);
        tr.commit().await.unwrap();
        assert!(catalog.verify_staged_import(namespace).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&first_chunk, &original);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog.verify_staged_import(namespace).await.unwrap(),
            summary
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&row_key(namespace, 1), &header.encode());
        tr.commit().await.unwrap();
        assert!(catalog.verify_staged_import(namespace).await.is_err());
        assert!(catalog.discard_import(namespace).await.unwrap());
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER、FDB_STAGE_STATE、FDB_STAGE_PHASE；分进程验证真实停服恢复"]
    async fn staged_row_survives_fdb_server_restart() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let state = std::env::var("FDB_STAGE_STATE").expect("FDB_STAGE_STATE");
        let phase = std::env::var("FDB_STAGE_PHASE").expect("FDB_STAGE_PHASE");
        let namespace = if phase == "seed" {
            Uuid::new_v4()
        } else {
            Uuid::parse_str(std::fs::read_to_string(&state).unwrap().trim()).unwrap()
        };
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let audit = DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index: 77,
            frozen_index: 77,
            paths: 1,
            tombstones: 0,
            rows_sha256: [9; 32],
            snapshot_sha256: [8; 32],
        };
        let path = ReconciledPath {
            pool_id: Uuid::from_u128(namespace.as_u128().wrapping_add(1)),
            logical_path: "/restart".into(),
            sources: vec![SourceCandidate {
                barcode: "RESTARTL8".into(),
                volume_id: Uuid::from_u128(namespace.as_u128().wrapping_add(2)),
                generation: 1,
                candidate: ImportCandidate {
                    logical_path: "/restart".into(),
                    physical_path: "restart".into(),
                    kind: NodeKind::File,
                    deleted: false,
                    version: (1, 1),
                    length: 3,
                    sha256: String::new(),
                },
            }],
            winner: 0,
        };
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 7,
                ..Default::default()
            },
            vec![
                Xattr {
                    key: XATTR_VERSION.into(),
                    value: "1.1".into(),
                    base64: false,
                },
                Xattr {
                    key: "user.large".into(),
                    value: "R".repeat(80_000),
                    base64: false,
                },
            ],
            None,
        )
        .unwrap();
        if phase == "seed" {
            assert!(catalog.begin_import(namespace).await.unwrap());
            catalog.begin_stage(namespace, audit).await.unwrap();
        }
        let data = StagedSourceData::Data {
            file_uid: 7,
            length: 3,
            content_digest: ContentDigest::Unknown,
            ee_alias: false,
            data_path: None,
        };
        let header = catalog
            .stage_row(
                namespace,
                audit,
                0,
                &path,
                RowEvidence {
                    attributes: Some(&attributes),
                    source_file_uids: Some(&[7]),
                    source_data: Some(std::slice::from_ref(&data)),
                },
            )
            .await
            .unwrap();
        assert!(header.chunks >= 2);
        let mut rows = Sha256::new();
        rows.update(ROW_DOMAIN);
        rows.update(0u64.to_be_bytes());
        rows.update(header.sha256);
        rows.update(1u64.to_be_bytes());
        let summary = StagedImportSummary {
            audit,
            paths: 1,
            rows_sha256: rows.finalize().into(),
            complete: true,
        };
        if phase == "seed" {
            catalog.finish_stage(namespace, summary).await.unwrap();
            std::fs::write(&state, namespace.to_string()).unwrap();
        } else if phase == "verify" {
            assert_eq!(
                catalog.staged_import_summary(namespace).await.unwrap(),
                Some(summary)
            );
            assert_eq!(
                catalog.staged_row_sha256(namespace, 0).await.unwrap(),
                Some(header.sha256)
            );
            assert_eq!(
                catalog.staged_path(namespace, 0).await.unwrap(),
                Some(path.clone())
            );
            assert_eq!(
                catalog.staged_entry(namespace, 0).await.unwrap(),
                Some(StagedImportEntry {
                    path,
                    current_attributes: attributes,
                    source_file_uids: vec![7],
                    source_data: Some(vec![data]),
                })
            );
            assert_eq!(
                catalog.verify_staged_import(namespace).await.unwrap(),
                summary
            );
            assert_eq!(
                catalog.confirm_staged_import(namespace).await.unwrap(),
                summary
            );
            assert!(catalog.resolve_path(namespace, b"/restart").await.is_err());
        } else if phase == "cleanup" {
            assert_eq!(
                catalog.staged_import_summary(namespace).await.unwrap(),
                Some(summary)
            );
            assert!(catalog.discard_import(namespace).await.unwrap());
            loop {
                if catalog
                    .purge_discarded_namespace(namespace, 100)
                    .await
                    .unwrap()
                    .complete
                {
                    break;
                }
            }
            assert!(
                catalog
                    .staged_import_summary(namespace)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(catalog.staged_path(namespace, 0).await.unwrap().is_none());
            assert!(catalog.staged_entry(namespace, 0).await.unwrap().is_none());
        } else {
            panic!("FDB_STAGE_PHASE 必须是 seed、verify 或 cleanup");
        }
    }
}
