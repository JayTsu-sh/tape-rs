//! Fixed raw-file reference recipe; neither execution nor source erase authority.
use super::*;
use crate::ltfs::index::FileNode;
use crate::ltfs::recovery::IndexSourceIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub version: u8,
    pub parent: String,
    pub inventory: [u8; 32],
    pub source: Object,
    pub source_index: Vec<u8>,
    pub input: [u8; 32],
    pub volume: String,
    pub previous: Vec<u8>,
    pub previous_source: Vec<u8>,
    pub donor: Object,
    pub expected: Object,
    pub physical_files: u64,
    pub target_objects: u64,
    pub target_tree: [u8; 32],
}

/// Constructed only from captured source/target indexes and retained byte proof.
/// Contains fingerprints, never foreign source block addresses or user attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataPlan(pub(super) Plan);

impl ReclaimMetadataPlan {
    pub fn path(&self) -> &str {
        &self.0.expected.path
    }
    pub fn uid(&self) -> u64 {
        self.0.expected.uid
    }
    pub fn source_uid(&self) -> u64 {
        self.0.source.uid
    }
    pub fn volume(&self) -> Uuid {
        Uuid::parse_str(&self.0.volume).expect("validated metadata volume")
    }
    /// Descriptor for an explicit verified historical read, never media proof.
    pub fn previous_source(&self) -> Result<IndexSourceIdentity> {
        IndexSourceIdentity::from_bytes(&self.0.previous_source).map_err(cache)
    }
    pub fn fingerprint(&self) -> Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-plan/1");
        hash.update(self.encode()?);
        Ok(hash.finalize().into())
    }
    pub(crate) fn validate_source(&self, source: &CapturedIndexSource) -> Result<()> {
        self.encode()?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, source.construct().into());
        let entry = source
            .entry_by_uid(self.source_uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if head != self.0.source_index || Object::capture(entry)? != self.0.source {
            return Err(CatalogError::Invalid("元数据执行来源改变原封存节点或索引"));
        }
        Ok(())
    }

    /// Stage exactly one raw reference on the already selected target. This
    /// changes only the in-memory working tree and does not grant I/O authority.
    pub(crate) fn stage_reference(
        &self,
        volume: &mut crate::ltfs::volume::LtfsVolume<'_>,
        source: &CapturedIndexSource,
        target: &CapturedIndexSource,
    ) -> Result<()> {
        self.validate_source(source)?;
        if self.observe_current(target, None)?.kind() != ReclaimMetadataObservationKind::Unchanged
            || volume.verified_committed_index_construct().map_err(cache)? != target.construct()
        {
            return Err(CatalogError::Invalid("元数据执行卷头不同于原目标计划"));
        }
        let entry = source
            .entry_by_uid(self.source_uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let donor_entry = target
            .entry_by_uid(self.0.donor.uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File { node: donor, .. } = donor_entry else {
            return Err(CatalogError::Incomplete);
        };
        let node = FileNode::reclaimed_reference(self.path(), self.uid(), &donor, &entry)
            .map_err(cache)?;
        if Object::capture(IndexEntry::File {
            path: self.path().into(),
            node,
        })? != self.0.expected
        {
            return Err(CatalogError::Invalid("元数据执行节点偏离原固定计划"));
        }
        volume
            .reference_reclaimed_body(
                self.path(),
                self.uid(),
                &self.0.donor.path,
                self.0.donor.uid,
                &entry,
            )
            .map_err(cache)
    }
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let p = &self.0;
        p.source.encode()?;
        p.donor.encode()?;
        p.expected.encode()?;
        let volume = Uuid::parse_str(&p.volume).map_err(cache)?;
        let head = active_volume::decode_index(volume, &p.previous)?;
        if p.version != 2
            || volume.is_nil()
            || volume.to_string() != p.volume
            || p.parent.len() != 64
            || !p
                .parent
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || p.source.kind != ReclaimSourceNodeKind::File
            || p.donor.kind != ReclaimSourceNodeKind::File
            || p.expected.kind != ReclaimSourceNodeKind::File
            || p.source.length != p.donor.length
            || p.source.length != p.expected.length
            || p.donor.uid == p.expected.uid
            || p.expected.uid <= 1
            || head.generation.checked_add(1).is_none()
            || p.physical_files.checked_add(1).is_none()
            || p.target_objects <= p.physical_files
            || ActiveIndexIdentity::from(self.previous_source()?.head()) != head
        {
            return Err(CatalogError::Corrupt(
                "回收元数据固定计划身份、类型或名额无效",
            ));
        }
        let raw = serde_json::to_vec(p).map_err(cache)?;
        if raw.len() > 48_000 {
            return Err(CatalogError::Invalid("回收元数据固定计划超出持久化预算"));
        }
        Ok(raw)
    }
}

impl ReclaimSourceInventory<'_> {
    /// Stream target paths/counts and fix one new UID at its verified donor head.
    /// A later head requires a new actual donor proof, not a reused hash string.
    pub fn plan_reclaimed_body(
        &self,
        input: &ReclaimMetadataInput,
        target: &CapturedIndexSource,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        import_spool::validate_path(path).map_err(cache)?;
        if !crate::ltfs::xml_text_valid(path) {
            return Err(CatalogError::Invalid("回收元数据路径包含 XML 非法字符"));
        }
        current_proof(target.scope(), target.header())?;
        let proof = input.source().byte_proof();
        let destination = input.destination();
        if proof.inventory() != self.summary.digest
            || proof.index() != self.summary.head
            || self.summary.volume != input.source().source().copy().volume_id.to_string()
            || ActiveIndexIdentity::from(target.construct()) != destination.index()
            || target.header().volume_uuid != destination.copy().volume_id
            || target.header().volume_uuid.to_string() == self.summary.volume
        {
            return Err(CatalogError::Invalid(
                "回收元数据计划与原源或实读目标卷头不同",
            ));
        }
        let source_entry = self
            .source
            .entry_by_uid(proof.source_node().uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let source = Object::capture(source_entry.clone())?;
        if source != *proof.object() {
            return Err(CatalogError::Invalid("回收元数据原始节点与实读正文不同"));
        }
        let donor_entry = target
            .entry_by_uid(destination.copy().ltfs_file_uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File { node: donor, .. } = &donor_entry else {
            return Err(CatalogError::Invalid("回收元数据正文目标不是普通文件"));
        };
        if destination.copy().content_digest != ContentDigest::Sha256(proof.sha256())
            || ContentDigest::from_ltfs_xattrs(&donor.xattrs)?
                != ContentDigest::Sha256(proof.sha256())
            || donor.length != destination.copy().size
        {
            return Err(CatalogError::Invalid("回收元数据目标节点与已核验正文不同"));
        }
        let uid = target
            .header()
            .highest_file_uid
            .checked_add(1)
            .ok_or(CatalogError::Invalid("回收元数据 UID 空间耗尽"))?;
        let node = FileNode::reclaimed_reference(path, uid, donor, &source_entry).map_err(cache)?;
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        let mut parent_found = false;
        let mut files = 0u64;
        let mut tree = TargetTree::new(target.header().volume_uuid);
        target
            .for_each_entry(|entry| {
                let entry_path = match &entry {
                    IndexEntry::Directory { path, .. } => {
                        if path == parent {
                            parent_found = true;
                        }
                        path
                    }
                    IndexEntry::File { path, .. } => {
                        files = files
                            .checked_add(1)
                            .ok_or_else(|| callback(CatalogError::Invalid("物理文件计数溢出")))?;
                        path
                    }
                };
                if entry_path == path {
                    return Err(callback(CatalogError::Invalid("回收元数据目的路径已存在")));
                }
                tree.add(&Object::capture(entry).map_err(callback)?)
                    .map_err(callback)?;
                Ok(())
            })
            .map_err(cache)?;
        if !parent_found {
            return Err(CatalogError::Invalid("回收元数据目的父目录不存在"));
        }
        let parent = proof.value()?["parent"]
            .as_str()
            .ok_or(CatalogError::Incomplete)?
            .to_owned();
        let mut previous = Vec::new();
        active_volume::encode_index(&mut previous, destination.index());
        let plan = ReclaimMetadataPlan(Plan {
            version: 2,
            parent,
            inventory: self.summary.digest,
            source,
            source_index: self.summary.head.clone(),
            input: input.fingerprint()?,
            volume: destination.copy().volume_id.to_string(),
            previous,
            previous_source: target.identity().map_err(cache)?.to_bytes(),
            donor: Object::capture(donor_entry)?,
            expected: Object::capture(IndexEntry::File {
                path: path.into(),
                node,
            })?,
            physical_files: files,
            target_objects: tree.objects,
            target_tree: tree.finish(),
        });
        plan.encode()?;
        Ok(plan)
    }
}

/// Canonical path-ordered stream; no tree or path set is retained in memory.
pub(super) struct TargetTree {
    hash: Sha256,
    previous: Option<String>,
    pub(super) objects: u64,
}
impl TargetTree {
    pub(super) fn new(volume: Uuid) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-target-tree/1");
        hash.update(volume.as_bytes());
        Self {
            hash,
            previous: None,
            objects: 0,
        }
    }
    pub(super) fn add(&mut self, object: &Object) -> Result<()> {
        if self
            .previous
            .as_ref()
            .is_some_and(|p| object.path.as_bytes() <= p.as_bytes())
            || self.previous.is_none()
                && (!object.path.is_empty() || object.kind != ReclaimSourceNodeKind::Directory)
        {
            return Err(CatalogError::Corrupt(
                "回收元数据目标对象顺序或根目录不完整",
            ));
        }
        let raw = object.encode()?;
        self.objects = self
            .objects
            .checked_add(1)
            .ok_or(CatalogError::Invalid("回收元数据目标对象计数溢出"))?;
        self.hash.update((raw.len() as u64).to_be_bytes());
        self.hash.update(raw);
        self.previous = Some(object.path.clone());
        Ok(())
    }
    pub(super) fn finish(self) -> [u8; 32] {
        self.hash.finalize().into()
    }
}
