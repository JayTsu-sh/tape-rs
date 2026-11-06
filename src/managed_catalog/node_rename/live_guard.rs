//! Current primary metadata/data references for one original pool volume.
//! A complete guard is inert; physical plans still need source/index/PR proofs.
use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRenameReferenceRole {
    Source,
    Target,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameLiveFile {
    pub node: NodeRenameRevision,
    pub copy_id: Uuid,
    pub metadata_uid: u64,
    pub data_uid: Option<u64>,
    pub role: NodeRenameReferenceRole,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeRenameDataReferences {
    pub source: u64,
    pub target: u64,
    pub other: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameVolumeGuardStats {
    pub files: u64,
    pub source_files: u64,
    pub target_files: u64,
    pub other_files: u64,
    pub data_uids: u64,
    pub sha256: [u8; 32],
    pub database_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Binding {
    pub source: ActiveIndexIdentity,
    pub scan: VolumeScan,
    pub placement: VolumePlacement,
    pub pool_revision: u64,
    pub assignment_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    namespace: String,
    file: String,
    file_version: u64,
    copy: String,
    metadata_uid: u64,
    data_uid: Option<u64>,
    role: NodeRenameReferenceRole,
    copy_sha256: [u8; 32],
}

pub struct NodeRenameVolumeGuard {
    conn: Connection,
    request: NodeRenameRequest,
    binding: Binding,
    manifest: [u8; 32],
    initial: [u8; 32],
    stats: NodeRenameVolumeGuardStats,
    max_bytes: u64,
    sealed: bool,
}

fn corrupt() -> CatalogError {
    CatalogError::Corrupt("rename 卷有效 UID 引用证明不一致")
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-live-row/1")
        .chain_update(bytes)
        .finalize()
        .into()
}
fn live(wire: &Wire) -> Result<NodeRenameLiveFile> {
    if wire.version != 1
        || wire.file_version == 0
        || wire.metadata_uid == 0
        || wire.data_uid == Some(0)
    {
        return Err(corrupt());
    }
    Ok(NodeRenameLiveFile {
        node: NodeRenameRevision {
            namespace: node_mutation::id(&wire.namespace)?,
            inode: node_mutation::id(&wire.file)?,
            version: wire.file_version,
        },
        copy_id: node_mutation::id(&wire.copy)?,
        metadata_uid: wire.metadata_uid,
        data_uid: wire.data_uid,
        role: wire.role,
    })
}

impl NodeRenameVolumeGuard {
    pub(super) fn new(
        request: &NodeRenameRequest,
        manifest: [u8; 32],
        binding: Binding,
        max_bytes: u64,
    ) -> Result<Self> {
        let conn = projection::temporary_database(max_bytes)?;
        conn.execute_batch("CREATE TABLE metadata(uid BLOB PRIMARY KEY NOT NULL CHECK(length(uid)=8),
            file BLOB NOT NULL UNIQUE CHECK(length(file)=16),data_uid BLOB CHECK(data_uid IS NULL OR length(data_uid)=8),sha BLOB NOT NULL CHECK(length(sha)=32),body BLOB NOT NULL CHECK(length(body)<=4096)) WITHOUT ROWID;
            CREATE INDEX data_users ON metadata(data_uid) WHERE data_uid IS NOT NULL;
            CREATE TABLE data(uid BLOB PRIMARY KEY NOT NULL CHECK(length(uid)=8),source INTEGER NOT NULL CHECK(source>=0),
                target INTEGER NOT NULL CHECK(target>=0),other INTEGER NOT NULL CHECK(other>=0),CHECK(source+target+other>0)) WITHOUT ROWID;").map_err(projection::sql)?;
        let source = binding.source;
        let bytes = node_mutation::canonical(
            &serde_json::json!({"request":request.fingerprint(),"manifest":manifest,
            "namespace":binding.placement.namespace.to_string(),"pool":binding.placement.pool_id.to_string(),
            "library":binding.placement.library_serial,"barcode":binding.placement.barcode,
            "volume":source.volume_uuid.to_string(),"generation":source.generation,"partition":source.location.partition,
            "block":source.location.start_block,"sha256":source.sha256,"incremental":source.incremental,
            "scan":binding.scan.id.to_string(),"scan_generation":binding.scan.generation,
            "pool_revision":binding.pool_revision,"assignment_revision":binding.assignment_revision}),
        )?;
        let initial = Sha256::new()
            .chain_update(b"tape-rs/node-rename-live-uids/1")
            .chain_update(bytes)
            .finalize()
            .into();
        let size = projection::database_bytes(&conn)?;
        if size > max_bytes {
            return Err(invalid("rename 卷 UID 防护超过数据库页预算"));
        }
        Ok(Self {
            conn,
            request: request.clone(),
            binding,
            manifest,
            initial,
            stats: NodeRenameVolumeGuardStats {
                files: 0,
                source_files: 0,
                target_files: 0,
                other_files: 0,
                data_uids: 0,
                sha256: initial,
                database_bytes: size,
            },
            max_bytes,
            sealed: false,
        })
    }

    pub(super) fn append(&mut self, files: &[(NodeRenameLiveFile, PhysicalCopy)]) -> Result<()> {
        if self.sealed {
            return Err(invalid("rename 卷 UID 防护已封口"));
        }
        let mut stats = self.stats.clone();
        let tr = self.conn.transaction().map_err(projection::sql)?;
        for (file, copy) in files {
            let expected_data = (!copy.media.as_ref().is_some_and(|m| m.native_symlink))
                .then_some(copy.ltfs_file_uid);
            let expected_uid = copy
                .media
                .as_ref()
                .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
            if file.node.namespace != self.request.spec().source.namespace
                || file.node.version != copy.version
                || file.metadata_uid != expected_uid
                || file.data_uid != expected_data
                || copy.volume_id != self.binding.source.volume_uuid
                || copy.volume_scan_id != self.binding.scan.id
                || copy.volume_generation > self.binding.source.generation
            {
                return Err(corrupt());
            }
            let encoded = copy_records::encode_physical_copy(copy)?;
            let copy_sha256 = Sha256::new()
                .chain_update(b"tape-rs/node-rename-live-copy/1")
                .chain_update(encoded)
                .finalize()
                .into();
            let wire = Wire {
                version: 1,
                namespace: file.node.namespace.to_string(),
                file: file.node.inode.to_string(),
                file_version: file.node.version,
                copy: file.copy_id.to_string(),
                metadata_uid: file.metadata_uid,
                data_uid: file.data_uid,
                role: file.role,
                copy_sha256,
            };
            live(&wire)?;
            let body = node_mutation::canonical(&wire)?;
            if let Some(saved) = tr
                .query_row(
                    "SELECT body FROM metadata WHERE uid=?",
                    [file.metadata_uid.to_be_bytes().as_slice()],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(projection::sql)?
            {
                if saved != body {
                    return Err(corrupt());
                }
                continue;
            }
            tr.execute(
                "INSERT INTO metadata(uid,file,data_uid,sha,body) VALUES(?,?,?,?,?)",
                params![
                    file.metadata_uid.to_be_bytes().as_slice(),
                    file.node.inode.as_bytes().as_slice(),
                    file.data_uid.map(|uid| uid.to_be_bytes().to_vec()),
                    hash(&body).as_slice(),
                    body
                ],
            )
            .map_err(projection::sql)?;
            let refs = match file.role {
                NodeRenameReferenceRole::Source => NodeRenameDataReferences {
                    source: 1,
                    ..Default::default()
                },
                NodeRenameReferenceRole::Target => NodeRenameDataReferences {
                    target: 1,
                    ..Default::default()
                },
                NodeRenameReferenceRole::Other => NodeRenameDataReferences {
                    other: 1,
                    ..Default::default()
                },
            };
            if let Some(uid) = file.data_uid {
                tr.execute("INSERT INTO data(uid,source,target,other) VALUES(?,?,?,?) ON CONFLICT(uid) DO UPDATE SET source=source+excluded.source,target=target+excluded.target,other=other+excluded.other",
                    params![uid.to_be_bytes().as_slice(),refs.source as i64,refs.target as i64,refs.other as i64]).map_err(projection::sql)?;
            }
            stats.files = stats.files.checked_add(1).ok_or_else(corrupt)?;
            match file.role {
                NodeRenameReferenceRole::Source => stats.source_files += 1,
                NodeRenameReferenceRole::Target => stats.target_files += 1,
                NodeRenameReferenceRole::Other => stats.other_files += 1,
            }
        }
        stats.database_bytes = projection::database_bytes(&tr)?;
        if stats.database_bytes > self.max_bytes {
            return Err(invalid("rename 卷 UID 防护超过数据库页预算"));
        }
        tr.commit().map_err(projection::sql)?;
        self.stats = stats;
        Ok(())
    }

    pub(super) fn seal(&mut self, source_files: u64, target_files: u64) -> Result<()> {
        if self.stats.source_files != source_files || self.stats.target_files != target_files {
            return Err(corrupt());
        }
        let mut hash = Sha256::new()
            .chain_update(b"tape-rs/node-rename-live-uids/1")
            .chain_update(self.initial);
        let mut statement = self
            .conn
            .prepare("SELECT uid,sha,body FROM metadata ORDER BY uid")
            .map_err(projection::sql)?;
        let mut rows = statement.query([]).map_err(projection::sql)?;
        let mut count = 0;
        let mut sources = 0;
        let mut targets = 0;
        let mut others = 0;
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let uid: Vec<u8> = row.get(0).map_err(projection::sql)?;
            let sha: Vec<u8> = row.get(1).map_err(projection::sql)?;
            let body: Vec<u8> = row.get(2).map_err(projection::sql)?;
            let file = self.decode(&uid, &sha, &body)?;
            hash.update(file.metadata_uid.to_be_bytes());
            hash.update((body.len() as u64).to_be_bytes());
            hash.update(body);
            count += 1;
            match file.role {
                NodeRenameReferenceRole::Source => sources += 1,
                NodeRenameReferenceRole::Target => targets += 1,
                NodeRenameReferenceRole::Other => others += 1,
            }
        }
        drop(rows);
        drop(statement);
        if count != self.stats.files
            || (sources, targets, others)
                != (
                    self.stats.source_files,
                    self.stats.target_files,
                    self.stats.other_files,
                )
        {
            return Err(corrupt());
        }
        self.stats.data_uids = self
            .conn
            .query_row("SELECT count(*) FROM data", [], |r| r.get(0))
            .map_err(projection::sql)?;
        {
            let mut statement=self.conn.prepare("SELECT DISTINCT data_uid FROM metadata WHERE data_uid IS NOT NULL ORDER BY data_uid").map_err(projection::sql)?;
            let rows = statement
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .map_err(projection::sql)?;
            let mut groups = 0;
            for row in rows {
                let raw = row.map_err(projection::sql)?;
                let uid = u64::from_be_bytes(raw.as_slice().try_into().map_err(|_| corrupt())?);
                if self.data_at(uid)?.is_none() {
                    return Err(corrupt());
                }
                groups += 1;
            }
            if groups != self.stats.data_uids {
                return Err(corrupt());
            }
        }
        self.stats.sha256 = hash.finalize().into();
        self.conn
            .pragma_update(None, "query_only", true)
            .map_err(projection::sql)?;
        self.sealed = true;
        Ok(())
    }
    fn decode(&self, uid: &[u8], sha: &[u8], body: &[u8]) -> Result<NodeRenameLiveFile> {
        if body.len() > 4096 || sha != hash(body) {
            return Err(corrupt());
        }
        let wire: Wire = serde_json::from_slice(body).map_err(|_| corrupt())?;
        let file = live(&wire)?;
        if node_mutation::canonical(&wire)? != body
            || uid != file.metadata_uid.to_be_bytes()
            || file.node.namespace != self.request.spec().source.namespace
        {
            return Err(corrupt());
        }
        Ok(file)
    }
    fn readable(&self) -> Result<()> {
        if self.sealed {
            Ok(())
        } else {
            Err(CatalogError::Incomplete)
        }
    }
    pub(super) fn require_physical_inputs(
        &self,
        baseline: &NodeRenameBaseline,
        projection: &NodeRenameProjectionSpool,
    ) -> Result<()> {
        self.readable()?;
        if !self.request.matches_baseline(baseline)? {
            return Err(invalid("rename 物理来源必须绑定原规范输入"));
        }
        projection.require_guard(&self.request, self.manifest)
    }
    pub(super) fn original_projection_binding(&self) -> ([u8; 32], [u8; 32], Binding) {
        (
            self.request.fingerprint(),
            self.manifest,
            self.binding.clone(),
        )
    }
    pub fn source_index(&self) -> ActiveIndexIdentity {
        self.binding.source
    }
    pub fn stats(&self) -> &NodeRenameVolumeGuardStats {
        &self.stats
    }
    pub fn metadata(&self, uid: u64) -> Result<Option<NodeRenameLiveFile>> {
        self.readable()?;
        if uid == 0 {
            return Err(invalid("metadata UID 为零"));
        }
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT sha,body FROM metadata WHERE uid=?",
                [uid.to_be_bytes().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(projection::sql)?;
        row.map(|(sha, body)| self.decode(&uid.to_be_bytes(), &sha, &body))
            .transpose()
    }
    fn data_at(&self, uid: u64) -> Result<Option<NodeRenameDataReferences>> {
        let saved = self
            .conn
            .query_row(
                "SELECT source,target,other FROM data WHERE uid=?",
                [uid.to_be_bytes().as_slice()],
                |r| {
                    Ok(NodeRenameDataReferences {
                        source: r.get(0)?,
                        target: r.get(1)?,
                        other: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(projection::sql)?;
        let mut statement = self
            .conn
            .prepare("SELECT uid,sha,body FROM metadata WHERE data_uid=?")
            .map_err(projection::sql)?;
        let mut rows = statement
            .query([uid.to_be_bytes().as_slice()])
            .map_err(projection::sql)?;
        let mut expected = NodeRenameDataReferences::default();
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let key: Vec<u8> = row.get(0).map_err(projection::sql)?;
            let sha: Vec<u8> = row.get(1).map_err(projection::sql)?;
            let body: Vec<u8> = row.get(2).map_err(projection::sql)?;
            let file = self.decode(&key, &sha, &body)?;
            if file.data_uid != Some(uid) {
                return Err(corrupt());
            }
            match file.role {
                NodeRenameReferenceRole::Source => expected.source += 1,
                NodeRenameReferenceRole::Target => expected.target += 1,
                NodeRenameReferenceRole::Other => expected.other += 1,
            }
        }
        let actual = if expected == NodeRenameDataReferences::default() {
            None
        } else {
            Some(expected)
        };
        if saved != actual {
            return Err(corrupt());
        }
        Ok(actual)
    }
    pub fn data(&self, uid: u64) -> Result<Option<NodeRenameDataReferences>> {
        self.readable()?;
        if uid == 0 {
            return Err(invalid("data UID 为零"));
        }
        self.data_at(uid)
    }
}

pub(super) async fn binding_at(
    tr: &Transaction,
    request: &NodeRenameRequest,
    volume: Uuid,
) -> Result<Binding> {
    let ns = request.spec().source.namespace;
    let pool_id = request.spec().pool;
    let placement = verified_placement(tr, volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.namespace != ns
        || placement.pool_id != pool_id
        || placement.library_serial != request.library()
    {
        return Err(invalid("卷不属于原 rename 带库和池"));
    }
    let assignment = ManagedCatalog::verified_assignment(tr, request.library(), &placement.barcode)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if !assignment.allows_io() {
        return Err(CatalogError::MediaUnavailable);
    }
    let pool = decode_pool(
        pool_id,
        &tr.get(&pool_key(ns, pool_id), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    active_pools::require_indexes(tr, ns, &pool).await?;
    let (scan, source) = active_volume::observed_at(tr, request.library(), volume).await?;
    if require_volume_ready(tr, volume, None).await? != scan || source.volume_uuid != volume {
        return Err(corrupt());
    }
    Ok(Binding {
        source,
        scan,
        placement,
        pool_revision: pool.revision,
        assignment_revision: assignment.revision,
    })
}

pub(super) async fn current_at(
    tr: &Transaction,
    namespace: Uuid,
    row: &VolumeCopy,
    binding: &Binding,
) -> Result<Option<(FileHead, PhysicalCopy)>> {
    if row.namespace != namespace {
        return Ok(None);
    }
    let published = lookup::published_file_receipt_at(tr, namespace, row.file_id).await?;
    let (head, copy) = if let Some((_, receipt, placement)) = published {
        let head = lookup::candidate_head(&receipt)?;
        if receipt.volume_id != binding.source.volume_uuid {
            return Ok(None);
        }
        if receipt.pool_id != Some(binding.placement.pool_id) || placement != binding.placement {
            return Err(corrupt());
        }
        (head.clone(), lookup::candidate_copy(&receipt, head.version))
    } else {
        let node = tr.get(&node_key(namespace, row.file_id), false).await?;
        let head = tr.get(&head_key(namespace, row.file_id), false).await?;
        let node = node.ok_or_else(corrupt)?;
        if let Some(retired) = retired_nodes::from_node_record(namespace, row.file_id, &node)? {
            // Unlink retains the original historical H record; rename replacement
            // clears it. Neither is a live reference, but a different H is corrupt.
            let historical = head.as_deref().map(decode_head).transpose()?;
            if historical.is_some() && historical != retired.previous_head {
                return Err(corrupt());
            }
            return Ok(None);
        }
        if decode_node(&node)?.kind != NodeKind::File {
            return Err(corrupt());
        }
        let head = decode_head(&head.ok_or_else(corrupt)?)?;
        let raw = tr
            .get(&copy_key(namespace, row.file_id, head.copy_id), false)
            .await?
            .ok_or_else(corrupt)?;
        let StoredCopy::Present(copy) = decode_stored_copy(&raw)? else {
            return Err(corrupt());
        };
        if copy.volume_id != binding.source.volume_uuid {
            return Ok(None);
        }
        let reverse = tr
            .get(
                &volume_key(
                    copy.volume_id,
                    copy.volume_scan_id,
                    namespace,
                    row.file_id,
                    head.copy_id,
                ),
                false,
            )
            .await?
            .ok_or_else(corrupt)?;
        // The trailing 32 bytes bind the publishing origin (receipt or node
        // projection), not the file's content checksum. Current H/R and the
        // exact V identity/version are the uniform read contract.
        if reverse.len() != 40 || read_u64(&reverse) != head.version {
            return Err(corrupt());
        }
        (head, copy)
    };
    if (head.copy_id == row.copy_id && head.version != row.version)
        || copy.version != head.version
        || copy.size != head.size
        || copy.volume_id != binding.source.volume_uuid
        || copy.volume_scan_id != binding.scan.id
        || copy.volume_generation > binding.source.generation
        || lookup::require_serving_copy_at(tr, namespace, &copy).await? != binding.placement
    {
        return Err(corrupt());
    }
    copy_records::encode_physical_copy(&copy)?;
    Ok(Some((head, copy)))
}

#[cfg(test)]
#[path = "live_guard_tests.rs"]
pub(super) mod tests;
