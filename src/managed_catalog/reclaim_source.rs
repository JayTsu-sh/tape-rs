//! Inventory every physical source node, before deciding how it is covered.
//! A sealed inventory is not evidence of migration and never permits erasure.
use super::*;
use crate::ltfs::index::IndexEntry;
use crate::ltfs::recovery::{CapturedIndexSource, IndexSourceScope};
use serde::{Deserialize, Serialize};
use std::io::Write;

const PAGE: usize = 32;
const MAX_ROW: usize = 48_000;
#[path = "reclaim_source_ledger.rs"]
mod ledger;
pub(super) use ledger::clear_reserved_at;
#[path = "reclaim_source_read.rs"]
mod read;
pub use read::{ReclaimReadSource, ReclaimSourceRead};
#[path = "reclaim_data_relocation.rs"]
mod relocation;
pub(super) use relocation::require_no_data_at;
pub use relocation::{ReclaimDataProgress, ReclaimMetadataInput};
#[path = "reclaim_metadata_plan.rs"]
mod metadata_plan;
pub use metadata_plan::ReclaimMetadataPlan;
#[path = "reclaim_metadata_observation.rs"]
mod metadata_observation;
pub use metadata_observation::{ReclaimMetadataObservation, ReclaimMetadataObservationKind};
#[path = "reclaim_metadata_write.rs"]
mod metadata_write;
pub(super) use metadata_write::require_idle_at as require_metadata_idle_at;
pub use metadata_write::{ReclaimMetadataAdmission, ReclaimMetadataWrite};
#[path = "reclaim_file_metadata.rs"]
mod metadata;
pub(super) use metadata::require_no_metadata_at;
pub use metadata::{ReclaimFileMetadata, ReclaimMetadataProgress};
#[path = "reclaim_node_metadata.rs"]
mod node_metadata;
pub use node_metadata::{ReclaimNodeDestination, ReclaimNodeMetadata, ReclaimNodeReadRequest};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimSourceNodeKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    path: String,
    uid: u64,
    kind: ReclaimSourceNodeKind,
    length: Option<u64>,
    sha256: [u8; 32],
}

/// One node from the original sealed physical inventory, not a destination
/// receipt. The private payload cannot be supplied as caller-created evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimSourceNode(Object);

impl ReclaimSourceNode {
    /// LTFS path without a leading slash; the empty path identifies the root.
    pub fn path(&self) -> &str {
        &self.0.path
    }
    pub fn uid(&self) -> u64 {
        self.0.uid
    }
    pub fn kind(&self) -> ReclaimSourceNodeKind {
        self.0.kind
    }
    /// None for directories; Some(0) remains a real zero-length file or link.
    pub fn length(&self) -> Option<u64> {
        self.0.length
    }
    /// Digest of the complete original node, including attributes and extents.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.0.sha256
    }
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn cache(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Cache(format!("回收源对象清单: {error}"))
}
fn callback(error: CatalogError) -> crate::error::TapeError {
    crate::error::TapeError::Catalog(error.to_string())
}

impl Object {
    fn capture(entry: IndexEntry) -> Result<Self> {
        let (path, uid, kind, length) = match &entry {
            IndexEntry::Directory { path, node } => {
                if !node.files.is_empty() || !node.subdirs.is_empty() || node.delta_fields.is_some()
                {
                    return Err(CatalogError::Invalid("回收源目录必须是完整的无子树节点"));
                }
                (
                    path,
                    node.meta.file_uid,
                    ReclaimSourceNodeKind::Directory,
                    None,
                )
            }
            IndexEntry::File { path, node } => {
                if node.delta_fields.is_some() || node.open_for_write == Some(true) {
                    return Err(CatalogError::Invalid("回收源文件未完整关闭"));
                }
                if node.symlink.is_some() && !node.extents.is_empty() {
                    return Err(CatalogError::Invalid(
                        "回收源符号链接不能同时携带数据 extent",
                    ));
                }
                let kind = if node.symlink.is_some() {
                    ReclaimSourceNodeKind::Symlink
                } else {
                    ReclaimSourceNodeKind::File
                };
                (path, node.meta.file_uid, kind, Some(node.length))
            }
        };
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-source-object/1");
        // Hash complete metadata, xattrs, extents and link text without making
        // a second in-memory copy of a potentially large node payload.
        serde_json::to_writer(HashWriter(&mut hash), &entry).map_err(cache)?;
        let object = Self {
            path: path.clone(),
            uid,
            kind,
            length,
            sha256: hash.finalize().into(),
        };
        object.encode()?;
        Ok(object)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        if self.uid == 0
            || (self.kind == ReclaimSourceNodeKind::Directory) != self.length.is_none()
            || self.path.is_empty() && self.kind != ReclaimSourceNodeKind::Directory
        {
            return Err(CatalogError::Corrupt("回收源对象 UID、类型或长度"));
        }
        if !self.path.is_empty() {
            import_spool::validate_path(&self.path).map_err(cache)?;
        }
        let raw = serde_json::to_vec(self).map_err(cache)?;
        if raw.len() > MAX_ROW {
            return Err(CatalogError::Invalid("回收源对象路径超出持久化证明预算"));
        }
        Ok(raw)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    objects: u64,
    directories: u64,
    files: u64,
    symlinks: u64,
    data_bytes: u64,
}
impl Counts {
    fn add(&mut self, object: &Object) -> Result<()> {
        let increment = |value: &mut u64, n| -> Result<()> {
            *value = value
                .checked_add(n)
                .ok_or(CatalogError::Invalid("回收源对象计数溢出"))?;
            Ok(())
        };
        increment(&mut self.objects, 1)?;
        match object.kind {
            ReclaimSourceNodeKind::Directory => increment(&mut self.directories, 1)?,
            ReclaimSourceNodeKind::Symlink => increment(&mut self.symlinks, 1)?,
            ReclaimSourceNodeKind::File => {
                increment(&mut self.files, 1)?;
                increment(
                    &mut self.data_bytes,
                    object.length.expect("checked file length"),
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    volume: String,
    head: Vec<u8>,
    identity: [u8; 32],
    dp_eod: u64,
    ip_generation: Option<u64>,
    history_depth: u32,
    counts: Counts,
    digest: [u8; 32],
}
impl Summary {
    fn seed(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-source-inventory/1");
        hash.update(self.identity);
        hash.update(self.dp_eod.to_be_bytes());
        hash.update([u8::from(self.ip_generation.is_some())]);
        hash.update(self.ip_generation.unwrap_or_default().to_be_bytes());
        hash.update(self.history_depth.to_be_bytes());
        hash.finalize().into()
    }
}

fn advance_digest(previous: [u8; 32], count: u64, row: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/reclaim-source-row/1");
    hash.update(previous);
    hash.update(count.to_be_bytes());
    hash.update((row.len() as u64).to_be_bytes());
    hash.update(row);
    hash.finalize().into()
}

/// Borrows the immutable, budgeted source database. Only a verified current
/// capture can construct this type; callers cannot supply a summary or rows.
pub struct ReclaimSourceInventory<'a> {
    source: &'a CapturedIndexSource,
    summary: Summary,
}
fn current_proof(
    scope: IndexSourceScope,
    header: &crate::ltfs::index::LtfsIndex,
) -> Result<(u64, Option<u64>, u32)> {
    let IndexSourceScope::Current {
        dp_eod,
        ip_generation,
        history_depth,
        history_truncated: false,
    } = scope
    else {
        return Err(CatalogError::Invalid(
            "回收源清单需要当前完整索引及未截断的历史证明",
        ));
    };
    if !header.unknown_elements.is_empty() {
        return Err(CatalogError::Invalid(
            "回收源包含未知索引字段，不能封存擦除依据",
        ));
    }
    Ok((dp_eod, ip_generation, history_depth))
}
impl<'a> ReclaimSourceInventory<'a> {
    pub fn from_captured(source: &'a CapturedIndexSource) -> Result<Self> {
        let (dp_eod, ip_generation, history_depth) =
            current_proof(source.scope(), source.header())?;
        let identity = source.identity().map_err(cache)?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, identity.head().into());
        let mut summary = Summary {
            volume: identity.head().volume_uuid.to_string(),
            head,
            identity: identity.fingerprint(),
            dp_eod,
            ip_generation,
            history_depth,
            counts: Counts::default(),
            digest: [0; 32],
        };
        summary.digest = summary.seed();
        let mut previous: Option<String> = None;
        source
            .for_each_entry(|entry| {
                let object = Object::capture(entry).map_err(callback)?;
                if previous
                    .as_ref()
                    .is_some_and(|p| object.path.as_bytes() <= p.as_bytes())
                    || previous.is_none()
                        && (!object.path.is_empty()
                            || object.kind != ReclaimSourceNodeKind::Directory)
                {
                    return Err(callback(CatalogError::Corrupt(
                        "回收源对象顺序或根目录不完整",
                    )));
                }
                let raw = object.encode().map_err(callback)?;
                summary.counts.add(&object).map_err(callback)?;
                summary.digest = advance_digest(summary.digest, summary.counts.objects, &raw);
                previous = Some(object.path);
                Ok(())
            })
            .map_err(cache)?;
        if summary.counts.directories == 0 {
            return Err(CatalogError::Invalid("回收源对象清单缺少根目录"));
        }
        Ok(Self { source, summary })
    }
    pub fn objects(&self) -> u64 {
        self.summary.counts.objects
    }
    pub fn directories(&self) -> u64 {
        self.summary.counts.directories
    }
    pub fn files(&self) -> u64 {
        self.summary.counts.files
    }
    pub fn symlinks(&self) -> u64 {
        self.summary.counts.symlinks
    }
    pub fn data_bytes(&self) -> u64 {
        self.summary.counts.data_bytes
    }

    fn page(&self, after: Option<&str>, limit: usize) -> Result<Vec<Object>> {
        let mut rows = Vec::with_capacity(limit);
        self.source
            .for_each_entry_page(after, limit, |entry| {
                rows.push(Object::capture(entry).map_err(callback)?);
                Ok(())
            })
            .map_err(cache)?;
        Ok(rows)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimSourceProgress {
    version: u8,
    parent: String,
    summary: Summary,
    counts: Counts,
    after: Option<String>,
    digest: [u8; 32],
    complete: bool,
}
impl ReclaimSourceProgress {
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn objects(&self) -> u64 {
        self.counts.objects
    }
    pub fn total_objects(&self) -> u64 {
        self.summary.counts.objects
    }
}

#[cfg(test)]
#[path = "reclaim_source_tests.rs"]
mod tests;
