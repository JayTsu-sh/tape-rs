//! Deterministic, restartable projection of a confirmed import.
//! The namespace stays Building; staged tombstones remain private.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::{
    CatalogError, DirectoryOutcome, FileHead, ImportSourceBinding, MAX_DEPTH, ManagedCatalog,
    MediaProvenance, NodeAttributes, NodeKind, PhysicalCopy, Result, StagedImportEntry,
    StagedSourceData, staged_import, validate_media, validate_name,
};
use super::{
    NamespaceDigest, NamespaceState, NamespaceSummary, Node, ScanState, StagedImportSummary,
    StoredCopy, Transaction, child_key, control, copy_key, copy_prefix, decode_head, decode_node,
    decode_stored_copy, encode_head, encode_node, encode_physical_copy, encode_ready_volume,
    encode_scan_manifest, head_key, index_evidence, namespace_key, namespace_verification,
    node_key, read_page, ready_volume_key, reconcile_volume_key, requests, scan_key,
    source_revision, staged_sources, uuid_from, volume_key, volume_prefix, write_attribute_chunks,
};

const INODE_DOMAIN: &[u8] = b"tape-rs/import-inode/1";
const COPY_DOMAIN: &[u8] = b"tape-rs/import-copy/1";
const COPY_SUMMARY_DOMAIN: &[u8] = b"tape-rs/import-copy-summary/1";
const TOMBSTONE_SUMMARY_DOMAIN: &[u8] = b"tape-rs/import-tombstone-summary/1";
const PROJECTION_PROOF_VERSION: u8 = 3;
const PROJECTION_CURSOR_VERSION: u8 = 1;
const COPY_VERIFIED_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportProjectionProgress {
    pub next: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedStagingPurgeProgress {
    pub removed: usize,
    pub complete: bool,
}

impl ImportProjectionProgress {
    pub fn complete(self) -> bool {
        self.next == self.total
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CopySummary {
    copies: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TombstoneSummary {
    tombstones: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProjectionExpected {
    namespace: NamespaceSummary,
    copies: CopySummary,
    tombstones: TombstoneSummary,
}

fn copy_summary_start(namespace: Uuid) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(COPY_SUMMARY_DOMAIN);
    hash.update(namespace.as_bytes());
    hash
}

fn tombstone_summary_start(namespace: Uuid) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(TOMBSTONE_SUMMARY_DOMAIN);
    hash.update(namespace.as_bytes());
    hash
}

fn tombstone_prefix(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b't');
    key.extend_from_slice(namespace.as_bytes());
    key
}

fn tombstone_key(namespace: Uuid, pool: Uuid, path: &str) -> Vec<u8> {
    let mut key = tombstone_prefix(namespace);
    key.extend_from_slice(pool.as_bytes());
    key.extend_from_slice(&Sha256::digest(path.as_bytes()));
    key
}

fn fold_copy_summary(hash: &mut Sha256, key: &[u8], value: &[u8]) -> Result<()> {
    let key_len = u32::try_from(key.len()).map_err(|_| CatalogError::Corrupt("副本键过长"))?;
    let value_len = u32::try_from(value.len()).map_err(|_| CatalogError::Corrupt("副本值过长"))?;
    hash.update(key_len.to_be_bytes());
    hash.update(key);
    hash.update(value_len.to_be_bytes());
    hash.update(value);
    Ok(())
}

fn projection_key(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b'q');
    key.extend_from_slice(namespace.as_bytes());
    key
}

pub(super) fn copy_verified_key(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b's');
    key.extend_from_slice(namespace.as_bytes());
    key
}

fn sealed_import_key(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b'w');
    key.extend_from_slice(namespace.as_bytes());
    key
}

pub(super) async fn require_sealed_ready(tr: &Transaction, namespace: Uuid) -> Result<()> {
    if tr.get(&namespace_key(namespace), false).await?.as_deref()
        != Some(&[NamespaceState::Sealed as u8][..])
        || tr
            .get(&control::managed_key(namespace), false)
            .await?
            .as_deref()
            != Some(&[][..])
    {
        return Err(CatalogError::Incomplete);
    }
    let proof = tr
        .get(&projection_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let verified = tr
        .get(&copy_verified_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if verified.len() != 41
        || verified[0] != COPY_VERIFIED_VERSION
        || verified[1..9] != source_revision(tr, namespace).await?.to_be_bytes()
        || verified[9..] != Sha256::digest(&proof)[..]
        || tr
            .get(&sealed_import_key(namespace), false)
            .await?
            .as_deref()
            != Some(verified.as_ref())
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

fn encode_copy_verified(proof: &[u8], source_revision: u64) -> [u8; 41] {
    let mut value = [0; 41];
    value[0] = COPY_VERIFIED_VERSION;
    value[1..9].copy_from_slice(&source_revision.to_be_bytes());
    value[9..].copy_from_slice(&Sha256::digest(proof));
    value
}

// A future seal must call this inside the transaction that publishes the
// namespace. A standalone read is only a diagnostic because its result can
// become stale as soon as the transaction ends.
async fn require_verified_copy_fence(tr: &Transaction, namespace: Uuid) -> Result<()> {
    if tr.get(&namespace_key(namespace), false).await?.as_deref()
        != Some(&[NamespaceState::Verified as u8][..])
        || tr
            .get(&control::managed_key(namespace), false)
            .await?
            .as_deref()
            != Some(&[][..])
    {
        return Err(CatalogError::Incomplete);
    }
    let manifest = tr
        .get(&staged_import::manifest_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let staged = StagedImportSummary::decode(&manifest)?;
    if !staged.complete
        || tr
            .get(&staged_import::confirmed_key(namespace), false)
            .await?
            .as_deref()
            != Some(manifest.as_ref())
        || tr
            .get(&staged_sources::binding_required_key(namespace), false)
            .await?
            .as_deref()
            != Some(staged_sources::REQUIRED_VERSION)
    {
        return Err(CatalogError::Incomplete);
    }
    let source = tr
        .get(&staged_sources::summary_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if staged_sources::StagedSourceSummary::decode(&source)?.staged != staged
        || tr
            .get(&staged_sources::confirmed_key(namespace), false)
            .await?
            .as_deref()
            != Some(source.as_ref())
    {
        return Err(CatalogError::Incomplete);
    }
    let proof = tr
        .get(&projection_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let expected = decode_projection_expectation(&proof, staged)?;
    namespace_verification::require_verified_summary(tr, namespace, expected.namespace).await?;
    if requests::has_namespace_holds(tr, namespace).await? {
        return Err(CatalogError::Incomplete);
    }
    let cursor = tr
        .get(&projection_cursor_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if decode_projection_cursor(&cursor, &proof, staged.paths)? != staged.paths {
        return Err(CatalogError::Incomplete);
    }
    let revision = source_revision(tr, namespace).await?;
    if tr
        .get(&copy_verified_key(namespace), false)
        .await?
        .as_deref()
        != Some(encode_copy_verified(&proof, revision).as_slice())
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

fn projection_cursor_key(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b'u');
    key.extend_from_slice(namespace.as_bytes());
    key
}

fn encode_projection_cursor(next: u64, proof: &[u8]) -> [u8; 41] {
    let mut value = [0; 41];
    value[0] = PROJECTION_CURSOR_VERSION;
    value[1..9].copy_from_slice(&next.to_be_bytes());
    value[9..].copy_from_slice(&Sha256::digest(proof));
    value
}

fn decode_projection_cursor(value: &[u8], proof: &[u8], total: u64) -> Result<u64> {
    if value.len() != 41
        || value[0] != PROJECTION_CURSOR_VERSION
        || value[9..] != Sha256::digest(proof)[..]
    {
        return Err(CatalogError::Corrupt("导入物化游标与投影证明不符"));
    }
    let next = u64::from_be_bytes(value[1..9].try_into().unwrap());
    if next > total {
        return Err(CatalogError::Corrupt("导入物化游标超出暂存行数"));
    }
    Ok(next)
}

fn encode_projection_expectation(
    staged: StagedImportSummary,
    expected: ProjectionExpected,
) -> Vec<u8> {
    let mut value = Vec::with_capacity(1 + staged.encode().len() + 128);
    value.push(PROJECTION_PROOF_VERSION);
    value.extend_from_slice(&staged.encode());
    value.extend_from_slice(&expected.namespace.nodes.to_be_bytes());
    value.extend_from_slice(&expected.namespace.files.to_be_bytes());
    value.extend_from_slice(&expected.namespace.digest);
    value.extend_from_slice(&expected.copies.copies.to_be_bytes());
    value.extend_from_slice(&expected.copies.digest);
    value.extend_from_slice(&expected.tombstones.tombstones.to_be_bytes());
    value.extend_from_slice(&expected.tombstones.digest);
    value
}

fn decode_projection_expectation(
    bytes: &[u8],
    staged: StagedImportSummary,
) -> Result<ProjectionExpected> {
    let manifest = staged.encode();
    if bytes.len() != 1 + manifest.len() + 128
        || bytes[0] != PROJECTION_PROOF_VERSION
        || bytes[1..1 + manifest.len()] != manifest
    {
        return Err(CatalogError::Corrupt("导入投影期望与暂存清单不符"));
    }
    let data = &bytes[1 + manifest.len()..];
    let namespace = NamespaceSummary {
        nodes: u64::from_be_bytes(data[..8].try_into().unwrap()),
        files: u64::from_be_bytes(data[8..16].try_into().unwrap()),
        digest: data[16..48].try_into().unwrap(),
    };
    if namespace.nodes == 0 || namespace.files >= namespace.nodes {
        return Err(CatalogError::Corrupt("导入投影期望节点数量无效"));
    }
    let tombstones = TombstoneSummary {
        tombstones: u64::from_be_bytes(data[88..96].try_into().unwrap()),
        digest: data[96..128].try_into().unwrap(),
    };
    if tombstones.tombstones != staged.audit.tombstones {
        return Err(CatalogError::Corrupt("导入投影墓碑数量与审计不符"));
    }
    Ok(ProjectionExpected {
        namespace,
        copies: CopySummary {
            copies: u64::from_be_bytes(data[48..56].try_into().unwrap()),
            digest: data[56..88].try_into().unwrap(),
        },
        tombstones,
    })
}

/// Disk-backed inode sort for the independent expected namespace digest.
/// A 100-million-path import must not collect its nodes in process memory.
pub(super) struct ProjectionScratch {
    path: PathBuf,
    conn: Option<rusqlite::Connection>,
    namespace: Uuid,
    pools: HashSet<Uuid>,
    supported: bool,
}

impl ProjectionScratch {
    pub(super) fn new(namespace: Uuid, directory: &Path) -> Result<Self> {
        let path = directory.join(format!("tape-rs-import-projection-{}.db", Uuid::new_v4()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let conn = match rusqlite::Connection::open(&path) {
            Ok(conn) => conn,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err(CatalogError::Cache(error.to_string()));
            }
        };
        if let Err(error) = conn.execute_batch(
            "PRAGMA journal_mode=OFF;
             PRAGMA synchronous=OFF;
             PRAGMA temp_store=FILE;
             CREATE TABLE expected_nodes(
               inode BLOB PRIMARY KEY, node BLOB NOT NULL, head BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE expected_copies(
               key BLOB PRIMARY KEY, value BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE expected_tombstones(
               key BLOB PRIMARY KEY, value BLOB NOT NULL
             ) WITHOUT ROWID;",
        ) {
            drop(conn);
            let _ = std::fs::remove_file(&path);
            return Err(CatalogError::Cache(error.to_string()));
        }
        let mut scratch = Self {
            path,
            conn: Some(conn),
            namespace,
            pools: HashSet::new(),
            supported: true,
        };
        scratch.insert(
            Uuid::nil(),
            &Node {
                kind: NodeKind::Directory,
                parent: Uuid::nil(),
                name: Vec::new(),
                attributes: None,
            },
            None,
        )?;
        Ok(scratch)
    }

    fn insert(&mut self, id: Uuid, node: &Node, head: Option<&FileHead>) -> Result<()> {
        let encoded = encode_node(node);
        let head = head.map(encode_head).unwrap_or_default();
        let conn = self.conn.as_ref().expect("scratch open");
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO expected_nodes(inode,node,head) VALUES(?1,?2,?3)",
                rusqlite::params![id.as_bytes().as_slice(), &encoded, &head],
            )
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if inserted == 0 {
            let (old_node, old_head): (Vec<u8>, Vec<u8>) = conn
                .query_row(
                    "SELECT node,head FROM expected_nodes WHERE inode=?1",
                    [id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            if old_node != encoded || old_head != head {
                return Err(CatalogError::Corrupt("导入期望 inode 身份冲突"));
            }
        }
        Ok(())
    }

    fn insert_copy(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let conn = self.conn.as_ref().expect("scratch open");
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO expected_copies(key,value) VALUES(?1,?2)",
                rusqlite::params![key, value],
            )
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if inserted == 0 {
            let old: Vec<u8> = conn
                .query_row(
                    "SELECT value FROM expected_copies WHERE key=?1",
                    [key],
                    |row| row.get(0),
                )
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            if old != value {
                return Err(CatalogError::Corrupt("导入期望副本身份冲突"));
            }
        }
        Ok(())
    }

    fn insert_tombstone(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let conn = self.conn.as_ref().expect("scratch open");
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO expected_tombstones(key,value) VALUES(?1,?2)",
                rusqlite::params![key, value],
            )
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if inserted == 0 {
            let old: Vec<u8> = conn
                .query_row(
                    "SELECT value FROM expected_tombstones WHERE key=?1",
                    [key],
                    |row| row.get(0),
                )
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            if old != value {
                return Err(CatalogError::Corrupt("导入期望墓碑身份冲突"));
            }
        }
        Ok(())
    }

    pub(super) fn add_entry(
        &mut self,
        entry: &StagedImportEntry,
        bindings: &[ImportSourceBinding],
    ) -> Result<()> {
        if !self.supported {
            return Ok(());
        }
        let pool = entry.path.pool_id;
        if pool.is_nil() {
            return Err(CatalogError::Corrupt("导入池身份无效"));
        }
        if self.pools.insert(pool) {
            self.insert(
                import_inode(self.namespace, pool, "/"),
                &Node {
                    kind: NodeKind::Directory,
                    parent: Uuid::nil(),
                    name: pool.as_hyphenated().to_string().into_bytes(),
                    attributes: None,
                },
                None,
            )?;
        }
        let current = entry
            .path
            .sources
            .get(entry.path.winner)
            .ok_or(CatalogError::Corrupt("导入文件赢家索引无效"))?;
        if current.candidate.deleted {
            let plan = plan_tombstone(self.namespace, entry, bindings)?;
            return self.insert_tombstone(
                &tombstone_key(self.namespace, pool, &plan.path),
                &encode_tombstone(&plan)?,
            );
        }
        if current.candidate.kind == NodeKind::Directory {
            if entry.path.sources.iter().any(|source| {
                source.candidate.deleted || source.candidate.kind != NodeKind::Directory
            }) {
                self.supported = false;
                return Ok(());
            }
            let (parent, name, id) = path_target(self.namespace, entry)?;
            return self.insert(
                id,
                &Node {
                    kind: NodeKind::Directory,
                    parent,
                    name,
                    attributes: Some(entry.current_attributes.clone()),
                },
                None,
            );
        }
        let plan = match plan_data_file(self.namespace, entry, bindings) {
            Ok(Some(plan)) if require_bounded_data_plan(&plan).is_ok() => plan,
            Ok(None) | Err(CatalogError::Incomplete) | Ok(Some(_)) => {
                self.supported = false;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.insert(
            plan.id,
            &Node {
                kind: NodeKind::File,
                parent: plan.parent,
                name: plan.name,
                attributes: Some(plan.attributes),
            },
            Some(&plan.head),
        )?;
        for source in plan.copies {
            self.insert_copy(
                &copy_key(self.namespace, plan.id, source.id),
                &encode_physical_copy(&source.copy)?,
            )?;
        }
        Ok(())
    }

    pub(super) fn summary(&self) -> Result<Option<ProjectionExpected>> {
        if !self.supported {
            return Ok(None);
        }
        let mut digest = NamespaceDigest::new(self.namespace)?;
        let mut statement = self
            .conn
            .as_ref()
            .expect("scratch open")
            .prepare("SELECT inode,node,head FROM expected_nodes ORDER BY inode")
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let mut rows = statement
            .query([])
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| CatalogError::Cache(error.to_string()))?
        {
            let id: Vec<u8> = row
                .get(0)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            let id =
                Uuid::from_slice(&id).map_err(|_| CatalogError::Corrupt("导入期望 inode 长度"))?;
            let node: Vec<u8> = row
                .get(1)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            let head: Vec<u8> = row
                .get(2)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            let node = decode_node(&node)?;
            let head = if head.is_empty() {
                None
            } else {
                Some(decode_head(&head)?)
            };
            digest.push(id, &node, head.as_ref())?;
        }
        drop(rows);
        drop(statement);
        let mut hash = copy_summary_start(self.namespace);
        let mut count = 0u64;
        let mut statement = self
            .conn
            .as_ref()
            .expect("scratch open")
            .prepare("SELECT key,value FROM expected_copies ORDER BY key")
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let mut rows = statement
            .query([])
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| CatalogError::Cache(error.to_string()))?
        {
            let key: Vec<u8> = row
                .get(0)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            let value: Vec<u8> = row
                .get(1)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            fold_copy_summary(&mut hash, &key, &value)?;
            count = count
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("导入期望副本数量溢出"))?;
        }
        drop(rows);
        drop(statement);
        let mut tombstone_hash = tombstone_summary_start(self.namespace);
        let mut tombstone_count = 0u64;
        let mut statement = self
            .conn
            .as_ref()
            .expect("scratch open")
            .prepare("SELECT key,value FROM expected_tombstones ORDER BY key")
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        let mut rows = statement
            .query([])
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| CatalogError::Cache(error.to_string()))?
        {
            let key: Vec<u8> = row
                .get(0)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            let value: Vec<u8> = row
                .get(1)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            fold_copy_summary(&mut tombstone_hash, &key, &value)?;
            tombstone_count = tombstone_count
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("导入期望墓碑数量溢出"))?;
        }
        Ok(Some(ProjectionExpected {
            namespace: digest.summary(),
            copies: CopySummary {
                copies: count,
                digest: hash.finalize().into(),
            },
            tombstones: TombstoneSummary {
                tombstones: tombstone_count,
                digest: tombstone_hash.finalize().into(),
            },
        }))
    }
}

impl Drop for ProjectionScratch {
    fn drop(&mut self) {
        self.conn.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportDataCopy {
    pub id: Uuid,
    pub binding: ImportSourceBinding,
    pub copy: PhysicalCopy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportDataFile {
    pub pool_id: Uuid,
    pub id: Uuid,
    pub parent: Uuid,
    pub name: Vec<u8>,
    pub attributes: NodeAttributes,
    pub head: FileHead,
    pub copies: Vec<ImportDataCopy>,
}

/// A deleted logical path has no namespace inode. The source tape and
/// monotonic tape version must survive import so a later scan cannot revive
/// an older file at the same pool/path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportTombstone {
    pub pool_id: Uuid,
    pub path: String,
    pub version: (u64, u64),
    pub binding: ImportSourceBinding,
    pub logical_file_uid: u64,
}

fn encode_tombstone(plan: &ImportTombstone) -> Result<Vec<u8>> {
    let path = plan.path.as_bytes();
    let path_len =
        u32::try_from(path.len()).map_err(|_| CatalogError::Invalid("导入墓碑路径过长"))?;
    let mut value = Vec::with_capacity(69 + path.len());
    value.push(1);
    value.extend_from_slice(&path_len.to_be_bytes());
    value.extend_from_slice(path);
    value.extend_from_slice(&plan.version.0.to_be_bytes());
    value.extend_from_slice(&plan.version.1.to_be_bytes());
    value.extend_from_slice(plan.binding.placement.volume_id.as_bytes());
    value.extend_from_slice(plan.binding.scan.id.as_bytes());
    value.extend_from_slice(&plan.binding.scan.generation.to_be_bytes());
    value.extend_from_slice(&plan.logical_file_uid.to_be_bytes());
    Ok(value)
}

fn plan_tombstone(
    namespace: Uuid,
    entry: &StagedImportEntry,
    bindings: &[ImportSourceBinding],
) -> Result<ImportTombstone> {
    let source = entry
        .path
        .sources
        .get(entry.path.winner)
        .ok_or(CatalogError::Corrupt("导入墓碑赢家索引无效"))?;
    let data = entry.source_data.as_ref().ok_or(CatalogError::Incomplete)?;
    let uid = *entry
        .source_file_uids
        .get(entry.path.winner)
        .ok_or(CatalogError::Corrupt("导入墓碑 fileuid 缺失"))?;
    let binding = bindings
        .iter()
        .find(|binding| binding.placement.volume_id == source.volume_id)
        .ok_or(CatalogError::Incomplete)?;
    path_target(namespace, entry)?;
    if !source.candidate.deleted
        || source.candidate.kind != NodeKind::File
        || source.candidate.physical_path
            != crate::ltfs::volume::tombstone_path(&entry.path.logical_path)
        || source.candidate.length != 0
        || entry.current_attributes.symlink().is_some()
        || entry.current_attributes.meta().map(|meta| meta.file_uid) != Some(uid)
        || uid == 0
        || data.get(entry.path.winner) != Some(&StagedSourceData::NoCopy)
        || binding.placement.namespace != namespace
        || binding.placement.pool_id != entry.path.pool_id
        || binding.placement.barcode != source.barcode
        || binding.scan.generation != source.generation
    {
        return Err(CatalogError::Corrupt("导入墓碑与已绑定来源不符"));
    }
    Ok(ImportTombstone {
        pool_id: entry.path.pool_id,
        path: entry.path.logical_path.clone(),
        version: source.candidate.version,
        binding: binding.clone(),
        logical_file_uid: uid,
    })
}

fn import_inode(namespace: Uuid, pool: Uuid, path: &str) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(INODE_DOMAIN);
    hash.update(namespace.as_bytes());
    hash.update(pool.as_bytes());
    hash.update(path.as_bytes());
    let digest: [u8; 32] = hash.finalize().into();
    let mut id: [u8; 16] = digest[..16].try_into().unwrap();
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

fn path_target(namespace: Uuid, entry: &StagedImportEntry) -> Result<(Uuid, Vec<u8>, Uuid)> {
    let pool = entry.path.pool_id;
    let path = entry.path.logical_path.as_str();
    if namespace.is_nil() || pool.is_nil() || !path.starts_with('/') || path == "/" {
        return Err(CatalogError::Corrupt("暂存目录逻辑路径无效"));
    }
    let components = path[1..].split('/').collect::<Vec<_>>();
    if components.len() + 1 > usize::from(MAX_DEPTH) {
        return Err(CatalogError::Invalid("导入目录深度超过上限"));
    }
    for component in &components {
        validate_name(component.as_bytes())?;
    }
    let name = components.last().unwrap().as_bytes().to_vec();
    let parent = match path.rfind('/').unwrap() {
        0 => import_inode(namespace, pool, "/"),
        at => import_inode(namespace, pool, &path[..at]),
    };
    Ok((parent, name, import_inode(namespace, pool, path)))
}

fn import_copy_id(
    namespace: Uuid,
    pool: Uuid,
    path: &str,
    binding: &ImportSourceBinding,
    version: (u64, u64),
    data_uid: u64,
) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(COPY_DOMAIN);
    hash.update(namespace.as_bytes());
    hash.update(pool.as_bytes());
    hash.update(path.as_bytes());
    hash.update(binding.placement.volume_id.as_bytes());
    hash.update(binding.scan.id.as_bytes());
    hash.update(version.0.to_be_bytes());
    hash.update(version.1.to_be_bytes());
    hash.update(data_uid.to_be_bytes());
    let digest: [u8; 32] = hash.finalize().into();
    let mut id: [u8; 16] = digest[..16].try_into().unwrap();
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

/// A bounded, deterministic plan for a staged path whose every source holds
/// data or a native LTFS link. Link copies carry only their media identity;
/// readers must reject payload requests rather than follow their target.
pub(super) fn plan_data_file(
    namespace: Uuid,
    entry: &StagedImportEntry,
    bindings: &[ImportSourceBinding],
) -> Result<Option<ImportDataFile>> {
    let current = entry
        .path
        .sources
        .get(entry.path.winner)
        .ok_or(CatalogError::Corrupt("导入文件赢家索引无效"))?;
    if current.candidate.deleted || current.candidate.kind == NodeKind::Directory {
        return Ok(None);
    }
    let sources = &entry.path.sources;
    let data = entry.source_data.as_ref().ok_or(CatalogError::Incomplete)?;
    if sources.is_empty()
        || sources.len() != data.len()
        || sources.len() != entry.source_file_uids.len()
    {
        return Err(CatalogError::Corrupt("导入文件来源数量不符"));
    }
    if matches!(data[entry.path.winner], StagedSourceData::NativeSymlink)
        && entry
            .current_attributes
            .symlink()
            .is_none_or(|target| target.contains(".LTFSEE_DATA/"))
    {
        return Err(CatalogError::Corrupt("原生符号链接属性与来源类型不符"));
    }
    if entry.current_attributes.meta().map(|meta| meta.file_uid)
        != Some(entry.source_file_uids[entry.path.winner])
    {
        return Err(CatalogError::Corrupt("导入赢家属性 fileuid 不符"));
    }
    let (parent, name, id) = path_target(namespace, entry)?;
    let by_volume = bindings
        .iter()
        .map(|binding| (binding.placement.volume_id, binding))
        .collect::<HashMap<_, _>>();
    if by_volume.len() != bindings.len() {
        return Err(CatalogError::Corrupt("导入来源卷重复"));
    }
    let mut versions = sources
        .iter()
        .map(|source| source.candidate.version)
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions.dedup();
    if versions.last().copied() != Some(current.candidate.version)
        || sources
            .iter()
            .filter(|source| source.candidate.version == current.candidate.version)
            .count()
            != 1
    {
        return Err(CatalogError::Corrupt("导入文件当前版本不唯一"));
    }
    let mut copies = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let binding = by_volume
            .get(&source.volume_id)
            .ok_or(CatalogError::Incomplete)?;
        if binding.placement.namespace != namespace
            || binding.placement.pool_id != entry.path.pool_id
            || binding.placement.barcode != source.barcode
            || binding.scan.generation != source.generation
            || source.candidate.logical_path != entry.path.logical_path
            || !staged_import::source_data_matches(
                &source.candidate,
                entry.source_file_uids[index],
                &data[index],
                true,
            )
        {
            return Err(CatalogError::Corrupt("导入来源与已绑定卷不符"));
        }
        let native_symlink = matches!(data[index], StagedSourceData::NativeSymlink);
        let (file_uid, length, content_digest, ee_alias, data_path) = match &data[index] {
            StagedSourceData::Data {
                file_uid,
                length,
                content_digest,
                ee_alias,
                data_path,
            } => (file_uid, length, content_digest, ee_alias, data_path),
            StagedSourceData::NativeSymlink => (
                &entry.source_file_uids[index],
                &0,
                &super::ContentDigest::Unknown,
                &false,
                &None,
            ),
            StagedSourceData::NoCopy => return Err(CatalogError::Incomplete),
        };
        let rank = versions
            .binary_search(&source.candidate.version)
            .expect("version collected") as u64
            + 1;
        let media = MediaProvenance {
            version: source.candidate.version,
            logical_file_uid: entry.source_file_uids[index],
            physical_path: data_path
                .clone()
                .unwrap_or_else(|| source.candidate.physical_path.clone()),
            ee_alias: *ee_alias,
            native_symlink,
        };
        validate_media(&media, *file_uid)?;
        copies.push(ImportDataCopy {
            id: import_copy_id(
                namespace,
                entry.path.pool_id,
                &entry.path.logical_path,
                binding,
                source.candidate.version,
                *file_uid,
            ),
            binding: (*binding).clone(),
            copy: PhysicalCopy {
                version: rank,
                request_epoch: 1,
                volume_id: source.volume_id,
                volume_generation: binding.scan.generation,
                volume_scan_id: binding.scan.id,
                ltfs_file_uid: *file_uid,
                size: *length,
                content_digest: *content_digest,
                media: Some(media),
            },
        });
    }
    let winner = &copies[entry.path.winner];
    if copies
        .iter()
        .map(|copy| copy.id)
        .collect::<HashSet<_>>()
        .len()
        != copies.len()
    {
        return Err(CatalogError::Corrupt("导入物理副本身份重复"));
    }
    Ok(Some(ImportDataFile {
        pool_id: entry.path.pool_id,
        id,
        parent,
        name,
        attributes: entry.current_attributes.clone(),
        head: FileHead {
            version: winner.copy.version,
            size: winner.copy.size,
            copy_id: winner.id,
        },
        copies,
    }))
}

fn require_directory_outcome(outcome: DirectoryOutcome) -> Result<()> {
    match outcome {
        DirectoryOutcome::Created | DirectoryOutcome::AlreadyExists => Ok(()),
        DirectoryOutcome::MissingParent => Err(CatalogError::Incomplete),
        DirectoryOutcome::NameConflict | DirectoryOutcome::IdConflict => {
            Err(CatalogError::Corrupt("导入目录 inode 或名称冲突"))
        }
        DirectoryOutcome::NamespaceNotReady | DirectoryOutcome::NamespaceNotBuilding => {
            Err(CatalogError::Incomplete)
        }
    }
}

async fn require_import_projection_proof(
    tr: &Transaction,
    namespace: Uuid,
    summary: StagedImportSummary,
) -> Result<()> {
    let expected_summary = summary.encode();
    if tr.get(&namespace_key(namespace), false).await?.as_deref()
        != Some(&[NamespaceState::Building as u8][..])
        || tr
            .get(&control::managed_key(namespace), false)
            .await?
            .as_deref()
            != Some(&[][..])
        || tr
            .get(&staged_sources::binding_required_key(namespace), false)
            .await?
            .as_deref()
            != Some(staged_sources::REQUIRED_VERSION)
        || tr
            .get(&staged_import::manifest_key(namespace), false)
            .await?
            .as_deref()
            != Some(expected_summary.as_slice())
        || tr
            .get(&staged_import::confirmed_key(namespace), false)
            .await?
            .as_deref()
            != Some(expected_summary.as_slice())
    {
        return Err(CatalogError::Incomplete);
    }
    let sources = tr
        .get(&staged_sources::summary_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if tr
        .get(&staged_sources::confirmed_key(namespace), false)
        .await?
        .as_deref()
        != Some(sources.as_ref())
        || staged_sources::StagedSourceSummary::decode(&sources)?.staged != summary
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

async fn require_import_binding(
    tr: &Transaction,
    namespace: Uuid,
    binding: &ImportSourceBinding,
) -> Result<()> {
    let placement = &binding.placement;
    let saved = tr
        .get(
            &staged_sources::source_key(namespace, placement.volume_id),
            false,
        )
        .await?;
    if saved.as_deref() != Some(staged_sources::encode_binding(binding)?.as_slice())
        || control::verified_import_placement(tr, placement.volume_id).await?
            != Some(placement.clone())
        || tr
            .get(&reconcile_volume_key(placement.volume_id), false)
            .await?
            .is_some()
        || tr
            .get(&ready_volume_key(placement.volume_id), false)
            .await?
            .as_deref()
            != Some(encode_ready_volume(binding.scan, &placement.library_serial).as_slice())
        || tr
            .get(&scan_key(placement.volume_id, binding.scan.id), false)
            .await?
            .as_deref()
            != Some(
                encode_scan_manifest(binding.scan, &placement.library_serial, ScanState::Ready)
                    .as_slice(),
            )
        || index_evidence::require_source(tr, placement.volume_id, binding.scan).await?
            != binding.source
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

async fn write_projected_file(
    tr: &Transaction,
    namespace: Uuid,
    summary: StagedImportSummary,
    plan: &ImportDataFile,
    verify_only: bool,
) -> Result<bool> {
    require_import_projection_proof(tr, namespace, summary).await?;
    for source in &plan.copies {
        require_import_binding(tr, namespace, &source.binding).await?;
    }
    let parent = tr
        .get(&node_key(namespace, plan.parent), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if decode_node(&parent)?.kind != NodeKind::Directory {
        return Err(CatalogError::Corrupt("导入文件父节点不是目录"));
    }
    let child_key = child_key(namespace, plan.parent, &plan.name);
    let node_key = node_key(namespace, plan.id);
    let head_key = head_key(namespace, plan.id);
    let node_value = encode_node(&Node {
        kind: NodeKind::File,
        parent: plan.parent,
        name: plan.name.clone(),
        attributes: Some(plan.attributes.clone()),
    });
    let head_value = encode_head(&plan.head);
    let child = tr.get(&child_key, false).await?;
    let node = tr.get(&node_key, false).await?;
    let head = tr.get(&head_key, false).await?;
    let mut existing = 0usize;
    for (actual, expected) in [
        (child.as_deref(), plan.id.as_bytes().as_slice()),
        (node.as_deref(), node_value.as_slice()),
        (head.as_deref(), head_value.as_slice()),
    ] {
        if let Some(actual) = actual {
            existing += 1;
            if actual != expected {
                return Err(CatalogError::Corrupt("导入文件 inode 或 head 与重试不一致"));
            }
        }
    }
    let mut copies = Vec::with_capacity(plan.copies.len());
    for source in &plan.copies {
        let copy_key = copy_key(namespace, plan.id, source.id);
        let volume_key = volume_key(
            source.copy.volume_id,
            source.copy.volume_scan_id,
            namespace,
            plan.id,
            source.id,
        );
        let copy_value = encode_physical_copy(&source.copy)?;
        let mut volume_value = source.copy.version.to_be_bytes().to_vec();
        volume_value.extend_from_slice(&Sha256::digest(&copy_value));
        for (key, expected) in [(&copy_key, &copy_value), (&volume_key, &volume_value)] {
            if let Some(actual) = tr.get(key, false).await? {
                existing += 1;
                if actual.as_ref() != expected {
                    return Err(CatalogError::Corrupt("导入副本与重试不一致"));
                }
            }
        }
        copies.push((copy_key, copy_value, volume_key, volume_value));
    }
    let mut prefix = copy_prefix(namespace);
    prefix.extend_from_slice(plan.id.as_bytes());
    let page = read_page(tr, &prefix, None, plan.copies.len(), false).await?;
    let mut actual = page
        .items
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    let mut expected = copies
        .iter()
        .map(|(copy_key, _, _, _)| copy_key.clone())
        .collect::<Vec<_>>();
    actual.sort_unstable();
    expected.sort_unstable();
    if page.next.is_some()
        || (existing != 0 && actual != expected)
        || (existing == 0 && !actual.is_empty())
    {
        return Err(CatalogError::Corrupt("导入文件副本集合与暂存来源不符"));
    }
    if existing == 3 + 2 * copies.len() {
        // The node may keep large attributes in separate chunks; verify them
        // before accepting an apparently identical replay.
        let decoded = decode_node(node.as_deref().expect("count checked"))?;
        if let Some(manifest) = decoded
            .attributes
            .as_ref()
            .and_then(NodeAttributes::manifest)
        {
            super::attribute_objects::require_complete(tr, namespace, manifest).await?;
        }
        return Ok(false);
    }
    if existing != 0 {
        return Err(CatalogError::Corrupt("导入文件记录只写入了一部分"));
    }
    if verify_only {
        return Err(CatalogError::Incomplete);
    }
    write_attribute_chunks(tr, namespace, plan.id, false, Some(&plan.attributes)).await?;
    tr.set(&child_key, plan.id.as_bytes());
    tr.set(&node_key, &node_value);
    tr.set(&head_key, &head_value);
    for (copy_key, copy_value, volume_key, volume_value) in copies {
        tr.set(&copy_key, &copy_value);
        tr.set(&volume_key, &volume_value);
    }
    Ok(true)
}

async fn write_projected_tombstone(
    tr: &Transaction,
    namespace: Uuid,
    summary: StagedImportSummary,
    plan: &ImportTombstone,
    verify_only: bool,
) -> Result<bool> {
    require_import_projection_proof(tr, namespace, summary).await?;
    require_import_binding(tr, namespace, &plan.binding).await?;
    let id = import_inode(namespace, plan.pool_id, &plan.path);
    if tr.get(&node_key(namespace, id), false).await?.is_some()
        || tr.get(&head_key(namespace, id), false).await?.is_some()
    {
        return Err(CatalogError::Corrupt("导入墓碑路径已有文件或目录"));
    }
    let key = tombstone_key(namespace, plan.pool_id, &plan.path);
    let value = encode_tombstone(plan)?;
    match tr.get(&key, false).await? {
        Some(saved) if saved.as_ref() == value => Ok(false),
        Some(_) => Err(CatalogError::Corrupt("导入墓碑与重试来源冲突")),
        None if verify_only => Err(CatalogError::Incomplete),
        None => {
            tr.set(&key, &value);
            Ok(true)
        }
    }
}

fn require_bounded_data_plan(plan: &ImportDataFile) -> Result<()> {
    if plan.copies.is_empty() || plan.copies.len() > 32 || plan.attributes.bytes().is_none() {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

impl ManagedCatalog {
    pub async fn staged_tombstone_plan(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<ImportTombstone>> {
        let summary = self.confirmed_staged_import(namespace).await?;
        if ordinal >= summary.paths {
            return Err(CatalogError::Invalid("暂存导入行号超出范围"));
        }
        let entry = self
            .staged_entry(namespace, ordinal)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let current = entry.path.current();
        if !current.candidate.deleted {
            return Ok(None);
        }
        if !self
            .import_pool_exists(namespace, entry.path.pool_id)
            .await?
        {
            return Err(CatalogError::Incomplete);
        }
        let binding = self
            .verified_staged_binding(namespace, current.volume_id)
            .await?;
        plan_tombstone(namespace, &entry, &[binding]).map(Some)
    }

    /// A tombstone is private until the whole import has an independent seal.
    /// Retries compare the exact source and version and never repair a
    /// conflicting row implicitly.
    pub async fn materialize_staged_tombstone(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<ImportTombstone>> {
        let Some(plan) = self.staged_tombstone_plan(namespace, ordinal).await? else {
            return Ok(None);
        };
        let summary = self.confirmed_staged_import(namespace).await?;
        let root = import_inode(namespace, plan.pool_id, "/");
        require_directory_outcome(
            self.create_import_directory(
                namespace,
                Uuid::nil(),
                plan.pool_id.as_hyphenated().to_string().as_bytes(),
                root,
            )
            .await?,
        )?;
        self.db
            .run(|tr, _| {
                let plan = plan.clone();
                async move {
                    match write_projected_tombstone(&tr, namespace, summary, &plan, false).await {
                        Err(CatalogError::Fdb(error)) => Err(error),
                        Err(CatalogError::Client(error)) => Err(error.into()),
                        result => Ok(result),
                    }
                }
            })
            .await??;
        Ok(Some(plan))
    }

    pub async fn verify_materialized_tombstone(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<ImportTombstone>> {
        let Some(plan) = self.staged_tombstone_plan(namespace, ordinal).await? else {
            return Ok(None);
        };
        let summary = self.confirmed_staged_import(namespace).await?;
        self.db
            .run(|tr, _| {
                let plan = plan.clone();
                async move {
                    match write_projected_tombstone(&tr, namespace, summary, &plan, true).await {
                        Err(CatalogError::Fdb(error)) => Err(error),
                        Err(CatalogError::Client(error)) => Err(error.into()),
                        result => Ok(result),
                    }
                }
            })
            .await??;
        Ok(Some(plan))
    }

    /// Compare every frozen R row with the source digest and every bound
    /// scan's V row with its R value. The verified marker is bound to the
    /// expectation; `seal_import` still requires a complete import protocol.
    pub async fn verify_bound_import_copies(&self, namespace: Uuid) -> Result<u64> {
        let tr = self.db.create_trx()?;
        if tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Verified as u8][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let revision = source_revision(&tr, namespace).await?;
        let manifest = tr
            .get(&staged_import::manifest_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let staged = StagedImportSummary::decode(&manifest)?;
        let proof = tr
            .get(&projection_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?
            .to_vec();
        let expected = decode_projection_expectation(&proof, staged)?;
        let sources = tr
            .get(&staged_sources::summary_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let sources = staged_sources::StagedSourceSummary::decode(&sources)?;
        if sources.staged != staged {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);

        let prefix = copy_prefix(namespace);
        let mut after = None;
        let mut hash = copy_summary_start(namespace);
        let mut count = 0u64;
        loop {
            let tr = self.db.create_trx()?;
            let page = read_page(&tr, &prefix, after.as_deref(), 1_000, false).await?;
            for (key, value) in page.items {
                if key.len() != prefix.len() + 32 {
                    return Err(CatalogError::Corrupt("导入副本键长度"));
                }
                fold_copy_summary(&mut hash, &key, &value)?;
                count = count
                    .checked_add(1)
                    .ok_or(CatalogError::Corrupt("导入副本数量溢出"))?;
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        let actual = CopySummary {
            copies: count,
            digest: hash.finalize().into(),
        };
        if actual != expected.copies {
            return Err(CatalogError::Incomplete);
        }
        // V is ordered by volume and scan, not namespace. Visit only the
        // volumes bound to this import and prove that every reverse row names
        // precisely one matching R row. A missing or orphan V blocks publish.
        let source_prefix = staged_sources::source_prefix(namespace);
        let mut source_after = None;
        let mut source_hash = Sha256::new();
        source_hash.update(staged_sources::SOURCES_DOMAIN);
        let mut source_count = 0u64;
        let mut reverse_count = 0u64;
        loop {
            let tr = self.db.create_trx()?;
            let sources =
                read_page(&tr, &source_prefix, source_after.as_deref(), 64, false).await?;
            for (source_key, source_value) in sources.items {
                if source_key.len() != source_prefix.len() + 16 {
                    return Err(CatalogError::Corrupt("导入来源键长度"));
                }
                let volume = uuid_from(&source_key[source_prefix.len()..])?;
                let binding = staged_sources::decode_binding(namespace, volume, &source_value)?;
                self.verify_import_source_binding(&binding).await?;
                source_hash.update(&source_key);
                source_hash.update((source_value.len() as u32).to_be_bytes());
                source_hash.update(&source_value);
                source_count = source_count
                    .checked_add(1)
                    .ok_or(CatalogError::Corrupt("导入来源数量溢出"))?;
                let prefix = volume_prefix(volume, binding.scan.id);
                let mut after = None;
                loop {
                    let tr = self.db.create_trx()?;
                    let page = read_page(&tr, &prefix, after.as_deref(), 1_000, false).await?;
                    let mut checks = Vec::with_capacity(page.items.len());
                    for (key, value) in page.items {
                        if key.len() != prefix.len() + 48
                            || key[prefix.len()..prefix.len() + 16] != namespace.as_bytes()[..]
                            || value.len() != 40
                        {
                            return Err(CatalogError::Corrupt("导入卷反向索引形状"));
                        }
                        let file = uuid_from(&key[prefix.len() + 16..prefix.len() + 32])?;
                        let copy = uuid_from(&key[prefix.len() + 32..])?;
                        checks.push((value, tr.get(&copy_key(namespace, file, copy), false)));
                    }
                    for (value, raw) in checks {
                        let raw = raw.await?.ok_or(CatalogError::Incomplete)?;
                        let StoredCopy::Present(stored) = decode_stored_copy(&raw)? else {
                            return Err(CatalogError::Corrupt("导入副本已回收"));
                        };
                        if stored.volume_id != volume
                            || stored.volume_scan_id != binding.scan.id
                            || value[..8] != stored.version.to_be_bytes()
                            || value[8..] != Sha256::digest(&raw)[..]
                        {
                            return Err(CatalogError::Incomplete);
                        }
                        reverse_count = reverse_count
                            .checked_add(1)
                            .ok_or(CatalogError::Corrupt("导入反向副本数量溢出"))?;
                    }
                    after = page.next;
                    if after.is_none() {
                        break;
                    }
                }
            }
            source_after = sources.next;
            if source_after.is_none() {
                break;
            }
        }
        source_hash.update(source_count.to_be_bytes());
        if reverse_count != count
            || source_count != sources.volumes
            || <[u8; 32]>::from(source_hash.finalize()) != sources.sources_sha256
        {
            return Err(CatalogError::Incomplete);
        }
        let prefix = tombstone_prefix(namespace);
        let mut after = None;
        let mut tombstone_hash = tombstone_summary_start(namespace);
        let mut tombstone_count = 0u64;
        loop {
            let tr = self.db.create_trx()?;
            let page = read_page(&tr, &prefix, after.as_deref(), 1_000, false).await?;
            for (key, value) in page.items {
                if key.len() != prefix.len() + 48 {
                    return Err(CatalogError::Corrupt("导入墓碑键长度"));
                }
                fold_copy_summary(&mut tombstone_hash, &key, &value)?;
                tombstone_count = tombstone_count
                    .checked_add(1)
                    .ok_or(CatalogError::Corrupt("导入墓碑数量溢出"))?;
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        if tombstone_count != expected.tombstones.tombstones
            || <[u8; 32]>::from(tombstone_hash.finalize()) != expected.tombstones.digest
        {
            return Err(CatalogError::Incomplete);
        }
        self.db
            .run(|tr, _| {
                let proof = proof.clone();
                async move {
                    if tr.get(&namespace_key(namespace), false).await?.as_deref()
                        != Some(&[NamespaceState::Verified as u8][..])
                        || tr.get(&projection_key(namespace), false).await?.as_deref()
                            != Some(proof.as_slice())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let current_revision = match source_revision(&tr, namespace).await {
                        Ok(revision) => revision,
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    };
                    if current_revision != revision {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let key = copy_verified_key(namespace);
                    let value = encode_copy_verified(&proof, revision);
                    match tr.get(&key, false).await? {
                        Some(saved) if saved.as_ref() == value => Ok(Ok(())),
                        Some(_) => Ok(Err(CatalogError::Corrupt("导入副本证明与期望冲突"))),
                        None => {
                            tr.set(&key, &value);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await??;
        Ok(count)
    }

    /// Check the current private copy proof and scan revision together. This
    /// read-only check does not authorize publication; a seal must repeat it
    /// inside its own write transaction.
    pub async fn inspect_bound_import_copy_fence(&self, namespace: Uuid) -> Result<()> {
        let tr = self.db.create_trx()?;
        require_verified_copy_fence(&tr, namespace).await
    }

    /// Seal a fully verified managed import with no tombstones. The source
    /// and copy proofs are checked in the same transaction as the read-only
    /// Sealed state; ordinary writes wait for a separate activation barrier.
    pub async fn seal_bound_staged_import(&self, namespace: Uuid) -> Result<bool> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace ID 不能为 nil"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let state = tr.get(&namespace_key(namespace), false).await?;
                        let marker = tr.get(&sealed_import_key(namespace), false).await?;
                        if state.as_deref() == Some(&[NamespaceState::Sealed as u8]) {
                            let saved = tr
                                .get(&copy_verified_key(namespace), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if marker.as_deref() != Some(saved.as_ref())
                                || saved.len() != 41
                                || saved[0] != COPY_VERIFIED_VERSION
                            {
                                return Err(CatalogError::Corrupt("受管导入封口标记"));
                            }
                            return Ok(false);
                        }
                        if marker.is_some() {
                            return Err(CatalogError::Corrupt("未发布候选已有封口标记"));
                        }
                        require_verified_copy_fence(&tr, namespace).await?;
                        let manifest = tr
                            .get(&staged_import::manifest_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        let staged = StagedImportSummary::decode(&manifest)?;
                        let proof = tr
                            .get(&projection_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        let expected = decode_projection_expectation(&proof, staged)?;
                        if expected.tombstones.tombstones != 0 {
                            return Err(CatalogError::Invalid(
                                "含墓碑的候选须先实现运行时删除顺序，不能发布",
                            ));
                        }
                        let verified = tr
                            .get(&copy_verified_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        tr.set(&sealed_import_key(namespace), &verified);
                        tr.set(&namespace_key(namespace), &[NamespaceState::Sealed as u8]);
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Reclaim the large per-path import spool after publication. Summaries,
    /// bound source records, copy proof and volume indexes remain intact.
    pub async fn purge_sealed_import_staging(
        &self,
        namespace: Uuid,
        limit: usize,
    ) -> Result<SealedStagingPurgeProgress> {
        if namespace.is_nil() || !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid(
                "namespace 必须有效，暂存清理批次必须在 1..=100",
            ));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        if tr.get(&namespace_key(namespace), false).await?.as_deref()
                            != Some(&[NamespaceState::Sealed as u8][..])
                        {
                            return Err(CatalogError::Incomplete);
                        }
                        let marker = tr
                            .get(&sealed_import_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        let verified = tr
                            .get(&copy_verified_key(namespace), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                        if marker.as_ref() != verified.as_ref()
                            || marker.len() != 41
                            || marker[0] != COPY_VERIFIED_VERSION
                        {
                            return Err(CatalogError::Corrupt("受管导入封口标记"));
                        }
                        let mut removed = 0;
                        for kind in *b"gh" {
                            let mut prefix = super::key(kind);
                            prefix.extend_from_slice(namespace.as_bytes());
                            let page =
                                read_page(&tr, &prefix, None, limit - removed, false).await?;
                            for (key, _) in &page.items {
                                tr.clear(key);
                            }
                            removed += page.items.len();
                            if removed == limit {
                                return Ok(SealedStagingPurgeProgress {
                                    removed,
                                    complete: false,
                                });
                            }
                        }
                        Ok(SealedStagingPurgeProgress {
                            removed,
                            complete: true,
                        })
                    }
                    .await,
                )
            })
            .await?
    }

    pub(super) async fn write_staged_projection_expectation(
        &self,
        namespace: Uuid,
        staged: StagedImportSummary,
        expected: ProjectionExpected,
    ) -> Result<()> {
        if expected.tombstones.tombstones != staged.audit.tombstones {
            return Err(CatalogError::Corrupt("导入投影墓碑数量与审计不符"));
        }
        let value = encode_projection_expectation(staged, expected);
        let key = projection_key(namespace);
        self.db
            .run(|tr, _| {
                let key = key.clone();
                let value = value.clone();
                async move {
                    let source = tr
                        .get(&staged_sources::summary_key(namespace), false)
                        .await?;
                    let source = match source
                        .as_deref()
                        .map(staged_sources::StagedSourceSummary::decode)
                        .transpose()
                    {
                        Ok(Some(source)) => source,
                        Ok(None) => return Ok(Err(CatalogError::Incomplete)),
                        Err(error) => return Ok(Err(error)),
                    };
                    if tr.get(&namespace_key(namespace), false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr
                            .get(&control::managed_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(&[][..])
                        || tr
                            .get(&staged_sources::binding_required_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(staged_sources::REQUIRED_VERSION)
                        || tr
                            .get(&staged_import::manifest_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(staged.encode().as_slice())
                        || source.staged != staged
                        || !staged.complete
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    match tr.get(&key, false).await? {
                        Some(saved) if saved.as_ref() == value => Ok(Ok(())),
                        Some(_) => Ok(Err(CatalogError::Corrupt("导入投影期望与已有记录冲突"))),
                        None => {
                            tr.set(&key, &value);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await??;
        Ok(())
    }

    /// Materialize at most one bounded page of confirmed paths. The durable
    /// cursor advances only after every path in the page has been projected;
    /// a crash before that transaction replays the same idempotent writes.
    pub async fn materialize_bound_import_page(
        &self,
        namespace: Uuid,
        limit: usize,
    ) -> Result<ImportProjectionProgress> {
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::Invalid("导入物化分页大小必须在 1..=1000"));
        }
        let staged = self.confirmed_staged_import(namespace).await?;
        let tr = self.db.create_trx()?;
        if tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Building as u8][..])
            || tr
                .get(&control::managed_key(namespace), false)
                .await?
                .as_deref()
                != Some(&[][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let proof = tr
            .get(&projection_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?
            .to_vec();
        decode_projection_expectation(&proof, staged)?;
        let cursor_key = projection_cursor_key(namespace);
        let cursor = tr.get(&cursor_key, false).await?;
        let start = match cursor.as_deref() {
            Some(value) => decode_projection_cursor(value, &proof, staged.paths)?,
            None => 0,
        };
        drop(tr);
        if start == staged.paths && cursor.is_some() {
            return Ok(ImportProjectionProgress {
                next: start,
                total: staged.paths,
            });
        }
        let end = start.saturating_add(limit as u64).min(staged.paths);
        for ordinal in start..end {
            let entry = self
                .staged_entry(namespace, ordinal)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let current = entry.path.current();
            if current.candidate.deleted {
                self.materialize_staged_tombstone(namespace, ordinal)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
            } else if current.candidate.kind == NodeKind::Directory {
                self.materialize_staged_directory(namespace, ordinal)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
            } else {
                self.materialize_staged_data_file(namespace, ordinal)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
            }
        }
        let old = encode_projection_cursor(start, &proof);
        let new = encode_projection_cursor(end, &proof);
        self.db
            .run(|tr, _| {
                let proof = proof.clone();
                let cursor_key = cursor_key.clone();
                async move {
                    if tr.get(&namespace_key(namespace), false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr
                            .get(&control::managed_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(&[][..])
                        || tr.get(&projection_key(namespace), false).await?.as_deref()
                            != Some(proof.as_slice())
                        || tr
                            .get(&staged_import::manifest_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(staged.encode().as_slice())
                        || tr
                            .get(&staged_import::confirmed_key(namespace), false)
                            .await?
                            .as_deref()
                            != Some(staged.encode().as_slice())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let cursor = tr.get(&cursor_key, false).await?;
                    if cursor.as_deref() == Some(new.as_slice()) {
                        return Ok(Ok(()));
                    }
                    if cursor.as_deref() != Some(old.as_slice())
                        && !(start == 0 && cursor.is_none())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    tr.set(&cursor_key, &new);
                    Ok(Ok(()))
                }
            })
            .await??;
        Ok(ImportProjectionProgress {
            next: end,
            total: staged.paths,
        })
    }

    /// Freeze a fully bound import against a disk-sorted digest derived while
    /// staging audited source rows. This does not clear the staged manifest or
    /// permit `seal_import`; unsupported source kinds have no digest to freeze.
    pub async fn freeze_bound_staged_import(
        &self,
        namespace: Uuid,
    ) -> Result<super::NamespaceVerificationOutcome> {
        let state = self
            .namespace_state(namespace)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if state == NamespaceState::Building {
            let staged = self.confirmed_staged_import(namespace).await?;
            if self.verify_staged_source_bindings(namespace).await?.staged != staged {
                return Err(CatalogError::Incomplete);
            }
        } else if !matches!(
            state,
            NamespaceState::Verifying | NamespaceState::Verified | NamespaceState::Rejected
        ) {
            return Err(CatalogError::Incomplete);
        }
        let tr = self.db.create_trx()?;
        let manifest = tr
            .get(&staged_import::manifest_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let staged = StagedImportSummary::decode(&manifest)?;
        let source = tr
            .get(&staged_sources::summary_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if !staged.complete
            || tr
                .get(&staged_import::confirmed_key(namespace), false)
                .await?
                .as_deref()
                != Some(manifest.as_ref())
            || tr
                .get(&staged_sources::confirmed_key(namespace), false)
                .await?
                .as_deref()
                != Some(source.as_ref())
            || staged_sources::StagedSourceSummary::decode(&source)?.staged != staged
            || tr
                .get(&staged_sources::binding_required_key(namespace), false)
                .await?
                .as_deref()
                != Some(staged_sources::REQUIRED_VERSION)
        {
            return Err(CatalogError::Incomplete);
        }
        let proof = tr
            .get(&projection_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let expected = decode_projection_expectation(&proof, staged)?;
        let cursor = tr
            .get(&projection_cursor_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if decode_projection_cursor(&cursor, &proof, staged.paths)? != staged.paths {
            return Err(CatalogError::Incomplete);
        }
        self.freeze_namespace(namespace, expected.namespace).await
    }

    /// Re-read one bounded staged data file and its exact projected copy set
    /// without writing FDB. This is a per-path check, not a seal proof for the
    /// whole namespace; other paths and orphan reverse indexes need their own
    /// paged verification before publication.
    pub async fn verify_materialized_data_file(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<Uuid>> {
        let Some(plan) = self.staged_data_file_plan(namespace, ordinal).await? else {
            return Ok(None);
        };
        require_bounded_data_plan(&plan)?;
        if let Some(manifest) = plan.attributes.manifest() {
            self.verify_attribute_object(namespace, manifest).await?;
        }
        let summary = self.confirmed_staged_import(namespace).await?;
        self.db
            .run(|tr, _| {
                let plan = plan.clone();
                async move {
                    match write_projected_file(&tr, namespace, summary, &plan, true).await {
                        Err(CatalogError::Fdb(error)) => Err(error),
                        Err(CatalogError::Client(error)) => Err(error.into()),
                        result => Ok(result),
                    }
                }
            })
            .await??;
        Ok(Some(plan.id))
    }

    /// Atomically project one bounded all-data file into a private Building
    /// namespace. An unchanged retry checks every R/V pair and attribute
    /// chunk; a partial/conflicting projection is rejected, never repaired by
    /// guessing. Tombstones use a separate private projection; unusually many
    /// copies remain blocked. Native links retain metadata without payload reads.
    pub async fn materialize_staged_data_file(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<Uuid>> {
        let Some(plan) = self.staged_data_file_plan(namespace, ordinal).await? else {
            return Ok(None);
        };
        require_bounded_data_plan(&plan)?;
        self.upload_attributes(namespace, &plan.attributes).await?;
        if let Some(manifest) = plan.attributes.manifest() {
            self.verify_attribute_object(namespace, manifest).await?;
        }
        let summary = self.confirmed_staged_import(namespace).await?;
        let root = import_inode(namespace, plan.pool_id, "/");
        require_directory_outcome(
            self.create_import_directory(
                namespace,
                Uuid::nil(),
                plan.pool_id.as_hyphenated().to_string().as_bytes(),
                root,
            )
            .await?,
        )?;
        self.db
            .run(|tr, _| {
                let plan = plan.clone();
                async move {
                    match write_projected_file(&tr, namespace, summary, &plan, false).await {
                        Err(CatalogError::Fdb(error)) => Err(error),
                        Err(CatalogError::Client(error)) => Err(error.into()),
                        result => Ok(result),
                    }
                }
            })
            .await??;
        Ok(Some(plan.id))
    }

    /// Build one file's bounded projection from confirmed rows and live,
    /// verified source bindings. No FDB keys are published by this call;
    /// a later write transaction must recheck the proof and scan identities.
    pub async fn staged_data_file_plan(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<ImportDataFile>> {
        let summary = self.confirmed_staged_import(namespace).await?;
        if ordinal >= summary.paths {
            return Err(CatalogError::Invalid("暂存导入行号超出范围"));
        }
        let entry = self
            .staged_entry(namespace, ordinal)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let current = entry
            .path
            .sources
            .get(entry.path.winner)
            .ok_or(CatalogError::Corrupt("导入文件赢家索引无效"))?;
        if current.candidate.deleted || current.candidate.kind == NodeKind::Directory {
            return Ok(None);
        }
        if !self
            .import_pool_exists(namespace, entry.path.pool_id)
            .await?
        {
            return Err(CatalogError::Incomplete);
        }
        let mut bindings = Vec::with_capacity(entry.path.sources.len());
        for source in &entry.path.sources {
            bindings.push(
                self.verified_staged_binding(namespace, source.volume_id)
                    .await?,
            );
        }
        plan_data_file(namespace, &entry, &bindings)
    }

    /// Project one staged live directory. Ordinals can be replayed after a
    /// crash; parents must be processed before children. A managed import
    /// requires the bound source proof. This never publishes the namespace
    /// and does not project files or deletion markers.
    pub async fn materialize_staged_directory(
        &self,
        namespace: Uuid,
        ordinal: u64,
    ) -> Result<Option<Uuid>> {
        let summary = self.confirmed_staged_import(namespace).await?;
        if ordinal >= summary.paths {
            return Err(CatalogError::Invalid("暂存导入行号超出范围"));
        }
        let entry = self
            .staged_entry(namespace, ordinal)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let current = entry.path.current();
        if current.candidate.deleted || current.candidate.kind != NodeKind::Directory {
            return Ok(None);
        }
        let tr = self.db.create_trx()?;
        match tr
            .get(&control::managed_key(namespace), true)
            .await?
            .as_deref()
        {
            Some([]) => {
                let binding = self
                    .verified_staged_binding(namespace, current.volume_id)
                    .await?;
                if binding.placement.pool_id != entry.path.pool_id
                    || binding.placement.barcode != current.barcode
                    || binding.scan.generation != current.generation
                {
                    return Err(CatalogError::Corrupt("导入目录与已绑定来源不符"));
                }
            }
            Some(_) => return Err(CatalogError::Corrupt("受管命名空间标记损坏")),
            None => {}
        }
        let (parent, name, id) = path_target(namespace, &entry)?;
        let pool = entry.path.pool_id;
        if !self.import_pool_exists(namespace, pool).await? {
            return Err(CatalogError::Incomplete);
        }
        let root = import_inode(namespace, pool, "/");
        require_directory_outcome(
            self.create_import_directory(
                namespace,
                Uuid::nil(),
                pool.as_hyphenated().to_string().as_bytes(),
                root,
            )
            .await?,
        )?;
        require_directory_outcome(
            self.create_import_directory_with_attributes(
                namespace,
                parent,
                &name,
                id,
                &entry.current_attributes,
            )
            .await?,
        )?;
        Ok(Some(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::volume_extents::ExpectedIndex;
    use crate::ltfs::index::IndexLocation;
    use crate::ltfs::index::NodeMeta;
    use crate::managed_catalog::{
        ContentDigest, ImportCandidate, NodeAttributes, ReconciledPath, SourceCandidate,
        VolumePlacement, VolumeScan,
    };

    #[test]
    fn projection_cursor_is_bound_to_proof_and_path_count() {
        let value = encode_projection_cursor(7, b"proof");
        assert_eq!(decode_projection_cursor(&value, b"proof", 7).unwrap(), 7);
        assert!(decode_projection_cursor(&value, b"other", 7).is_err());
        assert!(decode_projection_cursor(&value, b"proof", 6).is_err());
        let mut corrupt = value;
        corrupt[0] = 2;
        assert!(decode_projection_cursor(&corrupt, b"proof", 7).is_err());
    }

    fn entry(path: &str) -> StagedImportEntry {
        StagedImportEntry {
            path: ReconciledPath {
                pool_id: Uuid::from_u128(2),
                logical_path: path.into(),
                sources: Vec::new(),
                winner: 0,
            },
            current_attributes: NodeAttributes::new(
                NodeMeta {
                    file_uid: 1,
                    ..Default::default()
                },
                Vec::new(),
                None,
            )
            .unwrap(),
            source_file_uids: Vec::new(),
            source_data: None,
        }
    }

    #[test]
    fn inode_is_stable_and_scoped() {
        let namespace = Uuid::from_u128(1);
        let pool = Uuid::from_u128(2);
        let inode = import_inode(namespace, pool, "/dir");
        assert_eq!(inode, import_inode(namespace, pool, "/dir"));
        assert_ne!(inode, import_inode(namespace, pool, "/"));
        assert_ne!(inode, import_inode(namespace, Uuid::from_u128(3), "/dir"));
        assert_ne!(inode, import_inode(Uuid::from_u128(3), pool, "/dir"));
        assert!(!inode.is_nil());
    }

    #[test]
    fn directory_target_rejects_invalid_components() {
        let namespace = Uuid::from_u128(1);
        let (parent, name, inode) = path_target(namespace, &entry("/a/b")).unwrap();
        assert_eq!(parent, import_inode(namespace, Uuid::from_u128(2), "/a"));
        assert_eq!(name, b"b");
        assert_eq!(inode, import_inode(namespace, Uuid::from_u128(2), "/a/b"));
        for invalid in ["/", "/a//b", "/a/", "/..", "/a/./b", "/a\0b"] {
            assert!(path_target(namespace, &entry(invalid)).is_err());
        }
    }

    fn binding(namespace: Uuid, pool: Uuid, source: &SourceCandidate) -> ImportSourceBinding {
        ImportSourceBinding {
            placement: VolumePlacement {
                volume_id: source.volume_id,
                namespace,
                pool_id: pool,
                library_serial: "SIMLIB".into(),
                barcode: source.barcode.clone(),
            },
            scan: VolumeScan {
                generation: source.generation,
                id: Uuid::from_u128(source.volume_id.as_u128() + 20),
            },
            source: ExpectedIndex {
                volume_uuid: source.volume_id,
                generation: source.generation,
                location: IndexLocation {
                    partition: 'b',
                    start_block: 5,
                },
                sha256: [1; 32],
            },
        }
    }

    fn data_entry() -> (Uuid, StagedImportEntry, Vec<ImportSourceBinding>) {
        let namespace = Uuid::from_u128(1);
        let pool = Uuid::from_u128(2);
        let sources = [(7, (1, 1), 10), (8, (2, 1), 20)]
            .into_iter()
            .map(|(volume, version, _uid)| SourceCandidate {
                barcode: format!("SIM{volume}L8"),
                volume_id: Uuid::from_u128(volume),
                generation: 3,
                candidate: ImportCandidate {
                    logical_path: "/dir/file".into(),
                    physical_path: "dir/file".into(),
                    kind: NodeKind::File,
                    deleted: false,
                    version,
                    length: 42,
                    sha256: String::new(),
                },
            })
            .collect::<Vec<_>>();
        let bindings = sources
            .iter()
            .map(|source| binding(namespace, pool, source))
            .collect();
        let entry = StagedImportEntry {
            path: ReconciledPath {
                pool_id: pool,
                logical_path: "/dir/file".into(),
                sources,
                winner: 1,
            },
            current_attributes: NodeAttributes::new(
                NodeMeta {
                    file_uid: 20,
                    ..Default::default()
                },
                Vec::new(),
                None,
            )
            .unwrap(),
            source_file_uids: vec![10, 20],
            source_data: Some(
                [10, 20]
                    .into_iter()
                    .map(|file_uid| StagedSourceData::Data {
                        file_uid,
                        length: 42,
                        content_digest: ContentDigest::Unknown,
                        ee_alias: false,
                        data_path: None,
                    })
                    .collect(),
            ),
        };
        (namespace, entry, bindings)
    }

    #[test]
    fn data_plan_preserves_versions_and_is_stable_across_source_order() {
        let (namespace, mut entry, mut bindings) = data_entry();
        let first = plan_data_file(namespace, &entry, &bindings)
            .unwrap()
            .unwrap();
        assert_eq!(first.head.version, 2);
        assert_eq!(first.head.copy_id, first.copies[1].id);
        assert_eq!(first.copies[0].copy.version, 1);
        assert_eq!(first.copies[1].copy.media.as_ref().unwrap().version, (2, 1));
        assert_eq!(
            first.copies[1].copy.media.as_ref().unwrap().physical_path,
            "dir/file"
        );
        entry.path.sources.reverse();
        entry.source_file_uids.reverse();
        entry.source_data.as_mut().unwrap().reverse();
        entry.path.winner = 0;
        bindings.reverse();
        let replay = plan_data_file(namespace, &entry, &bindings)
            .unwrap()
            .unwrap();
        assert_eq!(first.id, replay.id);
        assert_eq!(first.head, replay.head);
        assert_eq!(first.copies[0], replay.copies[1]);
        assert_eq!(first.copies[1], replay.copies[0]);
        bindings[0].placement.barcode = "wrong".into();
        assert!(plan_data_file(namespace, &entry, &bindings).is_err());
        entry.path.winner = 99;
        assert!(plan_data_file(namespace, &entry, &bindings).is_err());
    }

    #[test]
    fn native_symlink_plan_retains_metadata_and_media_identity() {
        let (namespace, mut entry, bindings) = data_entry();
        entry.path.sources.truncate(1);
        entry.source_file_uids.truncate(1);
        entry.path.winner = 0;
        entry.path.sources[0].candidate.length = 0;
        entry.current_attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 10,
                ..Default::default()
            },
            Vec::new(),
            Some("../target".into()),
        )
        .unwrap();
        entry.source_data = Some(vec![StagedSourceData::NativeSymlink]);
        let plan = plan_data_file(namespace, &entry, &bindings)
            .unwrap()
            .unwrap();
        assert_eq!(plan.attributes.symlink(), Some("../target"));
        assert_eq!(plan.head.size, 0);
        assert_eq!(plan.copies.len(), 1);
        assert_eq!(plan.copies[0].copy.ltfs_file_uid, 10);
        assert!(plan.copies[0].copy.media.as_ref().unwrap().native_symlink);
        entry.path.sources[0].candidate.length = 1;
        assert!(plan_data_file(namespace, &entry, &bindings).is_err());
    }

    #[test]
    fn data_plan_keeps_ee_extent_and_rejects_unrepresented_sources() {
        let (namespace, mut entry, bindings) = data_entry();
        entry.path.sources[1].candidate.length = 0;
        entry.path.sources[1].candidate.physical_path = "dir/alias".into();
        entry.path.sources[1].candidate.logical_path = "/dir/file".into();
        entry.current_attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 20,
                ..Default::default()
            },
            Vec::new(),
            Some("../.LTFSEE_DATA/object".into()),
        )
        .unwrap();
        entry.source_data.as_mut().unwrap()[1] = StagedSourceData::Data {
            file_uid: 99,
            length: 42,
            content_digest: ContentDigest::Sha256([3; 32]),
            ee_alias: true,
            data_path: Some(".LTFSEE_DATA/object".into()),
        };
        let plan = plan_data_file(namespace, &entry, &bindings)
            .unwrap()
            .unwrap();
        assert_eq!(plan.head.size, 42);
        assert_eq!(plan.copies[1].copy.ltfs_file_uid, 99);
        assert_eq!(
            plan.copies[1].copy.media.as_ref().unwrap().logical_file_uid,
            20
        );
        assert_eq!(
            plan.copies[1].copy.media.as_ref().unwrap().physical_path,
            ".LTFSEE_DATA/object"
        );
        entry.source_data.as_mut().unwrap()[1] = StagedSourceData::NativeSymlink;
        assert!(matches!(
            plan_data_file(namespace, &entry, &bindings),
            Err(CatalogError::Corrupt("原生符号链接属性与来源类型不符"))
        ));
        entry.path.sources[1].candidate.deleted = true;
        assert!(
            plan_data_file(namespace, &entry, &bindings)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn data_plan_rejects_duplicate_physical_copy_identity() {
        let (namespace, mut entry, bindings) = data_entry();
        entry.path.sources.push(entry.path.sources[0].clone());
        entry.source_file_uids.push(entry.source_file_uids[0]);
        let first = entry.source_data.as_ref().unwrap()[0].clone();
        entry.source_data.as_mut().unwrap().push(first);
        assert!(matches!(
            plan_data_file(namespace, &entry, &bindings),
            Err(CatalogError::Corrupt("导入物理副本身份重复"))
        ));
    }

    #[test]
    fn projection_scratch_sorts_inodes_without_retaining_nodes_in_memory() {
        let (namespace, entry, bindings) = data_entry();
        let directory = std::env::temp_dir();
        let mut ascending = ProjectionScratch::new(namespace, &directory).unwrap();
        let mut descending = ProjectionScratch::new(namespace, &directory).unwrap();
        let mut paths = Vec::new();
        for n in 0..1_000 {
            let mut entry = entry.clone();
            let logical = format!("/file-{n:04}");
            entry.path.logical_path = logical.clone();
            for source in &mut entry.path.sources {
                source.candidate.logical_path = logical.clone();
                source.candidate.physical_path = logical[1..].into();
            }
            paths.push(entry);
        }
        for entry in &paths {
            ascending.add_entry(entry, &bindings).unwrap();
        }
        for entry in paths.iter().rev() {
            descending.add_entry(entry, &bindings).unwrap();
        }
        let first = ascending.summary().unwrap().unwrap();
        assert_eq!(
            (first.namespace.nodes, first.namespace.files),
            (1_002, 1_000)
        );
        assert_eq!(first.copies.copies, 2_000);
        assert_eq!(ascending.summary().unwrap(), descending.summary().unwrap());
        let path = ascending.path.clone();
        drop(ascending);
        assert!(!path.exists());
    }

    #[test]
    fn projection_scratch_proves_tombstone_without_resurrecting_file() {
        let (namespace, mut entry, bindings) = data_entry();
        entry.path.sources[entry.path.winner].candidate.deleted = true;
        entry.path.sources[entry.path.winner]
            .candidate
            .physical_path = crate::ltfs::volume::tombstone_path(&entry.path.logical_path);
        entry.path.sources[entry.path.winner].candidate.length = 0;
        entry.source_data.as_mut().unwrap()[entry.path.winner] = StagedSourceData::NoCopy;
        let plan = plan_tombstone(namespace, &entry, &bindings).unwrap();
        assert_eq!(plan.version, (2, 1));
        assert_eq!(plan.logical_file_uid, 20);
        assert_ne!(
            tombstone_key(namespace, plan.pool_id, &plan.path),
            tombstone_key(namespace, Uuid::from_u128(3), &plan.path)
        );
        let mut scratch = ProjectionScratch::new(namespace, &std::env::temp_dir()).unwrap();
        scratch.add_entry(&entry, &bindings).unwrap();
        let expected = scratch.summary().unwrap().unwrap();
        assert_eq!((expected.namespace.files, expected.copies.copies), (0, 0));
        assert_eq!(expected.tombstones.tombstones, 1);
        entry.path.sources[entry.path.winner]
            .candidate
            .physical_path = "wrong".into();
        assert!(plan_tombstone(namespace, &entry, &bindings).is_err());
    }

    #[test]
    fn projection_expectation_binds_the_complete_staged_manifest() {
        let staged = StagedImportSummary {
            audit: super::super::DirectoryAudit {
                origin: super::super::ImportOrigin::LegacySnapshot,
                applied_index: 1,
                frozen_index: 1,
                paths: 1,
                tombstones: 0,
                rows_sha256: [7; 32],
                snapshot_sha256: [8; 32],
            },
            paths: 1,
            rows_sha256: [9; 32],
            complete: true,
        };
        let expected = ProjectionExpected {
            namespace: NamespaceSummary {
                nodes: 3,
                files: 1,
                digest: [3; 32],
            },
            copies: CopySummary {
                copies: 2,
                digest: [4; 32],
            },
            tombstones: TombstoneSummary {
                tombstones: 0,
                digest: [5; 32],
            },
        };
        let bytes = encode_projection_expectation(staged, expected);
        assert_eq!(
            decode_projection_expectation(&bytes, staged).unwrap(),
            expected
        );
        let mut changed = staged;
        changed.rows_sha256[0] ^= 1;
        assert!(decode_projection_expectation(&bytes, changed).is_err());
        let mut changed = staged;
        changed.audit.snapshot_sha256[0] ^= 1;
        assert!(decode_projection_expectation(&bytes, changed).is_err());
        assert!(decode_projection_expectation(&bytes[..bytes.len() - 1], staged).is_err());
    }
}
