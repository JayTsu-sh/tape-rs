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
    pub raw_body: Option<RawBody>,
    pub source_index: Vec<u8>,
    pub input: [u8; 32],
    pub volume: String,
    pub previous: Vec<u8>,
    pub previous_dp_eod: u64,
    pub previous_source: Vec<u8>,
    pub donor: Object,
    pub expected: Object,
    pub ancestors: Vec<Scaffold>,
    pub displaced: Option<Scaffold>,
    pub physical_files: u64,
    pub target_objects: u64,
    pub target_tree: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scaffold {
    pub(super) original: Object,
    pub(super) expected: Object,
}

pub(super) fn moved_file(entry: IndexEntry, path: &str) -> Result<IndexEntry> {
    import_spool::validate_path(path).map_err(cache)?;
    let IndexEntry::File { mut node, .. } = entry else {
        return Err(CatalogError::Invalid("回收同名节点保留只接受文件或链接"));
    };
    node.name = path
        .rsplit('/')
        .next()
        .ok_or(CatalogError::Incomplete)?
        .into();
    Ok(IndexEntry::File {
        path: path.into(),
        node,
    })
}

/// Renaming a plain file must not redirect an existing relative link. The
/// replacement at `path` itself is new; it was a plain file in the original tree.
pub(super) fn require_displacement_keeps_links(
    target: &CapturedIndexSource,
    path: &str,
) -> Result<()> {
    target
        .for_each_entry(|entry| {
            if let IndexEntry::File {
                path: link_path,
                node,
            } = entry
                && link_path != path
                && let Some(link_target) = &node.symlink
                && crate::ltfs::volume::resolve_symlink(&link_path, link_target).as_deref()
                    == Some(path)
            {
                return Err(callback(CatalogError::Invalid(
                    "同名目的文件仍被已有相对链接引用，不能改名归档",
                )));
            }
            Ok(())
        })
        .map_err(cache)
}

/// A physical append has different index geometry from a shared-body reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawBody {
    pub body: Object,
    pub block_size: u32,
    pub sha256: [u8; 32],
    pub md5: Option<[u8; 16]>,
}

/// Constructed only from captured source/target indexes and retained byte proof.
/// Contains fingerprints, never foreign source block addresses or user attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataPlan(pub(super) Plan);

pub(super) fn node_input(parent: &str, inventory: [u8; 32], source: &Object) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/reclaim-node-input/1");
    hash.update(parent.as_bytes());
    hash.update(inventory);
    hash.update(source.encode()?);
    Ok(hash.finalize().into())
}

impl ReclaimMetadataPlan {
    pub(crate) fn is_node_metadata(&self) -> bool {
        self.0.raw_body.is_none() && self.0.source.kind != ReclaimSourceNodeKind::File
    }
    pub(crate) fn is_raw_body(&self) -> bool {
        self.0.raw_body.is_some()
    }
    pub(crate) fn raw_body_length(&self) -> Result<u64> {
        self.0
            .raw_body
            .as_ref()
            .and_then(|raw| raw.body.length)
            .ok_or(CatalogError::Invalid("不是具有完整长度的原始正文计划"))
    }
    pub(crate) fn is_source_based(&self) -> bool {
        self.is_node_metadata() || self.is_raw_body()
    }
    pub(super) fn index_start(&self) -> Result<u64> {
        let blocks = match &self.0.raw_body {
            Some(raw) if raw.block_size > 0 => raw
                .body
                .length
                .ok_or(CatalogError::Corrupt("原始正文长度缺失"))?
                .div_ceil(u64::from(raw.block_size)),
            Some(_) => return Err(CatalogError::Corrupt("原始正文块大小为零")),
            None => 0,
        };
        self.previous_dp_eod()
            .checked_add(blocks)
            .and_then(|n| n.checked_add(1))
            .ok_or(CatalogError::Invalid("原始正文索引追加位置溢出"))
    }
    pub(crate) fn raw_body_node(&self, source: &CapturedIndexSource) -> Result<FileNode> {
        self.validate_source(source)?;
        let raw = self
            .0
            .raw_body
            .as_ref()
            .ok_or(CatalogError::Invalid("不是原始正文追加计划"))?;
        let (entry, body) = raw_read::source_body(source, self.source_uid())?;
        if body != raw.body {
            return Err(CatalogError::Invalid("原始正文执行改变封存数据对象"));
        }
        let old = active_volume::decode_index(self.volume(), &self.0.previous)?;
        let node = FileNode::reclaimed_body(
            self.path(),
            self.uid(),
            &entry,
            crate::ltfs::index::IndexLocation {
                partition: old.location.partition,
                start_block: self.previous_dp_eod(),
            },
            raw.sha256,
            raw.md5,
        )
        .map_err(cache)?;
        if Object::capture(IndexEntry::File {
            path: self.path().into(),
            node: node.clone(),
        })? != self.0.expected
        {
            return Err(CatalogError::Corrupt("原始正文追加节点偏离固定计划"));
        }
        Ok(node)
    }
    pub(crate) fn require_raw_readback(
        &self,
        target: &CapturedIndexSource,
        bytes: Option<(u64, [u8; 32], Option<[u8; 16]>)>,
    ) -> Result<()> {
        match (&self.0.raw_body, bytes) {
            (None, None) => Ok(()),
            (Some(raw), Some((length, sha256, md5))) => {
                let node = target
                    .entry_by_uid(self.uid())
                    .map_err(cache)?
                    .ok_or(CatalogError::Incomplete)?;
                if raw.body.length != Some(length)
                    || raw.sha256 != sha256
                    || raw.md5 != md5
                    || Object::capture(node)? != self.0.expected
                {
                    return Err(CatalogError::Invalid("原始正文目的实读校验与固定计划不同"));
                }
                Ok(())
            }
            _ => Err(CatalogError::Invalid("原始正文发布缺少本轮目的实读校验")),
        }
    }
    pub(crate) fn stage_raw_body<R: std::io::Read>(
        &self,
        volume: &mut crate::ltfs::volume::LtfsVolume<'_>,
        source: &CapturedIndexSource,
        target: &CapturedIndexSource,
        reader: &mut R,
    ) -> Result<()> {
        let raw = self
            .0
            .raw_body
            .as_ref()
            .ok_or(CatalogError::Invalid("不是原始正文追加计划"))?;
        let expected = self.raw_body_node(source)?;
        if volume.block_size() != raw.block_size
            || self.observe_current(target, None)?.kind()
                != ReclaimMetadataObservationKind::Unchanged
            || volume.verified_committed_index_construct().map_err(cache)? != target.construct()
            || volume.recovery().dp.eod != self.previous_dp_eod()
        {
            return Err(CatalogError::Invalid("原始正文追加的标签、卷头或位置改变"));
        }
        volume
            .append_reclaimed_body(self.path(), &expected, reader)
            .map_err(cache)
    }
    pub(super) fn added_files(&self) -> u64 {
        u64::from(self.0.source.kind != ReclaimSourceNodeKind::Directory)
    }
    pub fn path(&self) -> &str {
        &self.0.expected.path
    }
    pub fn uid(&self) -> u64 {
        self.0.expected.uid
    }
    pub(super) fn first_uid(&self) -> Result<u64> {
        self.uid()
            .checked_sub(self.0.ancestors.len() as u64)
            .filter(|uid| *uid > 1)
            .ok_or(CatalogError::Corrupt("原始节点父目录 UID 范围无效"))
    }
    pub fn source_uid(&self) -> u64 {
        self.0.source.uid
    }
    pub fn volume(&self) -> Uuid {
        Uuid::parse_str(&self.0.volume).expect("validated metadata volume")
    }
    /// Original audited DP append boundary; the XML SHA alone cannot bind it.
    pub fn previous_dp_eod(&self) -> u64 {
        self.0.previous_dp_eod
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
        for ancestor in &self.0.ancestors {
            let entry = source
                .entry_by_uid(ancestor.original.uid)
                .map_err(cache)?
                .ok_or(CatalogError::Incomplete)?;
            if Object::capture(entry.clone())? != ancestor.original {
                return Err(CatalogError::Invalid("原始节点父目录改变封存来源"));
            }
            let relocated = node_metadata::relocation_source(&entry, &ancestor.expected.path)?;
            if Object::capture(
                relocated
                    .reclaimed_metadata_node(&ancestor.expected.path, ancestor.expected.uid)
                    .map_err(cache)?,
            )? != ancestor.expected
            {
                return Err(CatalogError::Invalid("原始节点父目录偏离固定配方"));
            }
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
        if self.is_raw_body() {
            return Err(CatalogError::Invalid("原始正文需要专用流式追加路径"));
        }
        if let Some(displaced) = &self.0.displaced {
            // Reject sealed unsafe plans before position I/O or tree changes.
            require_displacement_keeps_links(target, &displaced.original.path)?;
        }
        if self.observe_current(target, None)?.kind() != ReclaimMetadataObservationKind::Unchanged
            || volume.verified_committed_index_construct().map_err(cache)? != target.construct()
            || volume.recovery().dp.eod != self.previous_dp_eod()
        {
            return Err(CatalogError::Invalid("元数据执行卷头不同于原目标计划"));
        }
        let entry = source
            .entry_by_uid(self.source_uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if self.is_node_metadata() {
            for ancestor in &self.0.ancestors {
                let entry = source
                    .entry_by_uid(ancestor.original.uid)
                    .map_err(cache)?
                    .ok_or(CatalogError::Incomplete)?;
                let entry = node_metadata::relocation_source(&entry, &ancestor.expected.path)?;
                volume
                    .reference_reclaimed_node(
                        &ancestor.expected.path,
                        ancestor.expected.uid,
                        &entry,
                    )
                    .map_err(cache)?;
            }
            let entry = node_metadata::relocation_source(&entry, self.path())?;
            let expected = entry
                .reclaimed_metadata_node(self.path(), self.uid())
                .map_err(cache)?;
            if Object::capture(expected)? != self.0.expected {
                return Err(CatalogError::Invalid("原始节点执行偏离固定计划"));
            }
            if let Some(displaced) = &self.0.displaced {
                let old = target
                    .entry_by_uid(displaced.original.uid)
                    .map_err(cache)?
                    .ok_or(CatalogError::Incomplete)?;
                if Object::capture(old.clone())? != displaced.original
                    || Object::capture(moved_file(old, &displaced.expected.path)?)?
                        != displaced.expected
                {
                    return Err(CatalogError::Invalid("同名目的节点改变原保留配方"));
                }
                volume
                    .displace_reclaimed_file(
                        &displaced.original.path,
                        displaced.original.uid,
                        &displaced.expected.path,
                    )
                    .map_err(cache)?;
            }
            return volume
                .reference_reclaimed_node(self.path(), self.uid(), &entry)
                .map_err(cache);
        }
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
        let mut parent_path = p.donor.path.as_str();
        let mut uid = self.first_uid()?;
        if !self.is_node_metadata() && !p.ancestors.is_empty()
            || p.ancestors.len() >= usize::from(MAX_DEPTH)
        {
            return Err(CatalogError::Corrupt("仅原始目录或链接允许有界父目录配方"));
        }
        for ancestor in &p.ancestors {
            ancestor.original.encode()?;
            ancestor.expected.encode()?;
            if ancestor.original.kind != ReclaimSourceNodeKind::Directory
                || ancestor.expected.kind != ReclaimSourceNodeKind::Directory
                || ancestor.expected.uid != uid
                || ancestor
                    .expected
                    .path
                    .rsplit_once('/')
                    .map_or("", |(p, _)| p)
                    != parent_path
            {
                return Err(CatalogError::Corrupt(
                    "原始节点父目录配方顺序、类型或 UID 无效",
                ));
            }
            uid = uid.checked_add(1).ok_or(CatalogError::Incomplete)?;
            parent_path = &ancestor.expected.path;
        }
        if let Some(raw) = &p.raw_body {
            raw.body.encode()?;
        }
        let volume = Uuid::parse_str(&p.volume).map_err(cache)?;
        let head = active_volume::decode_index(volume, &p.previous)?;
        if let Some(displaced) = &p.displaced {
            displaced.original.encode()?;
            displaced.expected.encode()?;
            if !self.is_node_metadata()
                || p.source.kind != ReclaimSourceNodeKind::Symlink
                || displaced.original.kind != ReclaimSourceNodeKind::File
                || displaced.original.uid >= self.first_uid()?
                || displaced.original.path != p.expected.path
                || displaced.expected.uid != displaced.original.uid
                || displaced.expected.kind != displaced.original.kind
                || displaced.expected.length != displaced.original.length
                || displaced.expected.path
                    != format!("_reclaim_displaced_{}_{}", p.parent, displaced.original.uid)
            {
                return Err(CatalogError::Corrupt("同名目的节点保留配方无效"));
            }
        }
        let source_kind_matches = if self.is_raw_body() {
            matches!(
                p.source.kind,
                ReclaimSourceNodeKind::File | ReclaimSourceNodeKind::Symlink
            ) && p.expected.kind == ReclaimSourceNodeKind::File
                && p.raw_body.as_ref().is_some_and(|raw| {
                    raw.body.kind == ReclaimSourceNodeKind::File
                        && raw.body.length == p.expected.length
                })
                && p.donor.kind == ReclaimSourceNodeKind::Directory
                && p.expected
                    .path
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    == parent_path
        } else if self.is_node_metadata() {
            p.source.kind == p.expected.kind
                && p.donor.kind == ReclaimSourceNodeKind::Directory
                && p.expected
                    .path
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    == parent_path
                && p.input == node_input(&p.parent, p.inventory, &p.source)?
        } else {
            p.donor.kind == ReclaimSourceNodeKind::File
                && p.expected.kind == ReclaimSourceNodeKind::File
                && p.source.length == p.donor.length
        };
        if p.version != 7
            || volume.is_nil()
            || volume.to_string() != p.volume
            || p.parent.len() != 64
            || !p
                .parent
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !source_kind_matches
            || (!self.is_raw_body() && p.source.length != p.expected.length)
            || p.donor.uid == p.expected.uid
            || p.expected.uid <= 1
            || head.generation.checked_add(1).is_none()
            || head
                .location
                .start_block
                .checked_add(2)
                .is_none_or(|minimum| p.previous_dp_eod < minimum)
            || self.index_start().is_err()
            || p.raw_body
                .as_ref()
                .is_some_and(|raw| raw.block_size < 4096 || raw.block_size > 16 << 20)
            || p.physical_files.checked_add(self.added_files()).is_none()
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
    /// Fix a physical body append from live source-byte evidence. The label's
    /// actual block size binds its DP index location and must match at execution.
    pub fn plan_reclaimed_raw_body(
        &self,
        reservation: &ReclaimReservation,
        proof: &ReclaimRawSourceRead,
        target: &CapturedIndexSource,
        label: &crate::ltfs::label::LtfsLabel,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        self.plan_raw_body_at(reservation, &proof.fact, target, label, path)
    }
    pub(super) fn plan_raw_body_at(
        &self,
        reservation: &ReclaimReservation,
        fact: &raw_read::ReadFact,
        target: &CapturedIndexSource,
        label: &crate::ltfs::label::LtfsLabel,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        import_spool::validate_path(path).map_err(cache)?;
        fact.require_inventory(reservation, self)?;
        let (entry, _) = raw_read::source_body(self.source, fact.object.uid)?;
        let (previous_dp_eod, _, _) = current_proof(target.scope(), target.header())?;
        if label.volume_uuid != target.header().volume_uuid
            || label.data_partition != target.construct().location.partition
            || label.volume_uuid.to_string() == self.summary.volume
        {
            return Err(CatalogError::Invalid("原始正文目的标签或介质身份不一致"));
        }
        let uid = target
            .header()
            .highest_file_uid
            .checked_add(1)
            .ok_or(CatalogError::Invalid("原始正文目的 UID 耗尽"))?;
        let expected = FileNode::reclaimed_body(
            path,
            uid,
            &entry,
            crate::ltfs::index::IndexLocation {
                partition: label.data_partition,
                start_block: previous_dp_eod,
            },
            fact.sha256,
            fact.md5,
        )
        .map_err(cache)?;
        let parent_path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let mut anchor = None;
        let mut files = 0u64;
        let mut tree = TargetTree::new(label.volume_uuid);
        target
            .for_each_entry(|entry| {
                let object = Object::capture(entry).map_err(callback)?;
                if object.path == path {
                    return Err(callback(CatalogError::Invalid("原始正文目的路径已存在")));
                }
                if object.path == parent_path && object.kind == ReclaimSourceNodeKind::Directory {
                    anchor = Some(object.clone());
                }
                files = files
                    .checked_add(u64::from(object.kind != ReclaimSourceNodeKind::Directory))
                    .ok_or_else(|| callback(CatalogError::Invalid("原始正文目的计数溢出")))?;
                tree.add(&object).map_err(callback)
            })
            .map_err(cache)?;
        let mut previous = Vec::new();
        active_volume::encode_index(&mut previous, target.construct().into());
        let plan = ReclaimMetadataPlan(Plan {
            version: 7,
            parent: reservation.fingerprint(),
            inventory: self.summary.digest,
            source: fact.object.clone(),
            raw_body: Some(RawBody {
                body: fact.body.clone(),
                block_size: label.blocksize,
                sha256: fact.sha256,
                md5: fact.md5,
            }),
            source_index: self.summary.head.clone(),
            input: fact.fingerprint()?,
            volume: label.volume_uuid.to_string(),
            previous,
            previous_dp_eod,
            previous_source: target.identity().map_err(cache)?.to_bytes(),
            donor: anchor.ok_or(CatalogError::Invalid("原始正文目的父目录不存在"))?,
            ancestors: Vec::new(),
            displaced: None,
            expected: Object::capture(IndexEntry::File {
                path: path.into(),
                node: expected,
            })?,
            physical_files: files,
            target_objects: tree.objects,
            target_tree: tree.finish(),
        });
        plan.encode()?;
        Ok(plan)
    }
    /// Fix one new raw directory/native link and its existing parent anchor.
    /// The streaming recipe neither creates a logical inode nor authorizes I/O.
    pub fn plan_reclaimed_node(
        &self,
        reservation: &ReclaimReservation,
        original: &ReclaimSourceNode,
        target: &CapturedIndexSource,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        import_spool::validate_path(path).map_err(cache)?;
        let (previous_dp_eod, _, _) = current_proof(target.scope(), target.header())?;
        if self.summary.volume != reservation.spec().volume().to_string()
            || reservation.intent()["index"] != json!(self.summary.head)
            || self.summary.volume == target.header().volume_uuid.to_string()
        {
            return Err(CatalogError::Invalid(
                "原始节点计划改变原源或复用源卷作为目的",
            ));
        }
        let entry = self
            .source
            .entry_by_uid(original.uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        node_metadata::attributes_of(&entry)?;
        let source = Object::capture(entry.clone())?;
        if source != original.0 || source.kind == ReclaimSourceNodeKind::File {
            return Err(CatalogError::Invalid("原始节点计划不是原目录或原生链接"));
        }
        let parent_path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let displaced = if let Some(old) = target.entry_by_path(path).map_err(cache)? {
            let original = Object::capture(old.clone())?;
            if source.kind != ReclaimSourceNodeKind::Symlink
                || original.kind != ReclaimSourceNodeKind::File
            {
                return Err(CatalogError::Invalid(
                    "原生链接目的路径被目录或链接占用，不能改变已有链接的相对目标",
                ));
            }
            let archive = format!(
                "_reclaim_displaced_{}_{}",
                reservation.fingerprint(),
                original.uid
            );
            if target.entry_by_path(&archive).map_err(cache)?.is_some() {
                return Err(CatalogError::Invalid("同名目的节点的固定保留路径已存在"));
            }
            let expected = Object::capture(moved_file(old, &archive)?)?;
            Some(Scaffold { original, expected })
        } else {
            None
        };
        if let Some(displaced) = &displaced {
            require_displacement_keeps_links(target, &displaced.original.path)?;
        }
        let mut existing = std::collections::BTreeMap::new();
        let mut files = 0u64;
        let mut tree = TargetTree::new(target.header().volume_uuid);
        target
            .for_each_entry(|entry| {
                let object = Object::capture(entry).map_err(callback)?;
                if object.path.is_empty()
                    || parent_path == object.path
                    || parent_path
                        .strip_prefix(&object.path)
                        .is_some_and(|tail| tail.starts_with('/'))
                {
                    if object.kind != ReclaimSourceNodeKind::Directory {
                        return Err(callback(CatalogError::Invalid("原始节点父路径被文件占用")));
                    }
                    existing.insert(object.path.clone(), object.clone());
                }
                files = files
                    .checked_add(u64::from(object.kind != ReclaimSourceNodeKind::Directory))
                    .ok_or_else(|| callback(CatalogError::Invalid("原始节点目的计数溢出")))?;
                tree.add(&object).map_err(callback)
            })
            .map_err(cache)?;
        let mut anchor = existing.get("").cloned().ok_or(CatalogError::Incomplete)?;
        let mut ancestors = Vec::new();
        let mut prefix = String::new();
        let source_prefix = path.strip_suffix(&source.path).unwrap_or("");
        let root_uid = self.source.header().root.meta.file_uid;
        for component in parent_path.split('/').filter(|part| !part.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(directory) = existing.get(&prefix) {
                if !ancestors.is_empty() {
                    return Err(CatalogError::Corrupt("原始节点父目录树不连续"));
                }
                anchor = directory.clone();
                continue;
            }
            if ancestors.len() + 1 >= usize::from(MAX_DEPTH) {
                return Err(CatalogError::Invalid("原始节点缺失父目录超过深度预算"));
            }
            let original = match prefix.strip_prefix(source_prefix) {
                Some(relative) if !relative.is_empty() => {
                    self.source.entry_by_path(relative).map_err(cache)?
                }
                _ => None,
            }
            .filter(|entry| matches!(entry, IndexEntry::Directory { .. }))
            .or(self.source.entry_by_uid(root_uid).map_err(cache)?)
            .ok_or(CatalogError::Incomplete)?;
            let original_object = Object::capture(original.clone())?;
            let uid = target
                .header()
                .highest_file_uid
                .checked_add(ancestors.len() as u64 + 1)
                .ok_or(CatalogError::Invalid("原始节点父目录 UID 耗尽"))?;
            let relocated = node_metadata::relocation_source(&original, &prefix)?;
            let expected = Object::capture(
                relocated
                    .reclaimed_metadata_node(&prefix, uid)
                    .map_err(cache)?,
            )?;
            ancestors.push(Scaffold {
                original: original_object,
                expected,
            });
        }
        let uid = target
            .header()
            .highest_file_uid
            .checked_add(ancestors.len() as u64 + 1)
            .ok_or(CatalogError::Invalid("原始节点目的 UID 耗尽"))?;
        let relocated = node_metadata::relocation_source(&entry, path)?;
        let expected = Object::capture(
            relocated
                .reclaimed_metadata_node(path, uid)
                .map_err(cache)?,
        )?;
        let parent = reservation.fingerprint();
        let mut previous = Vec::new();
        active_volume::encode_index(&mut previous, target.construct().into());
        let plan = ReclaimMetadataPlan(Plan {
            version: 7,
            input: node_input(&parent, self.summary.digest, &source)?,
            parent,
            inventory: self.summary.digest,
            source,
            raw_body: None,
            source_index: self.summary.head.clone(),
            volume: target.header().volume_uuid.to_string(),
            previous,
            previous_dp_eod,
            previous_source: target.identity().map_err(cache)?.to_bytes(),
            donor: anchor,
            expected,
            ancestors,
            displaced,
            physical_files: files,
            target_objects: tree.objects,
            target_tree: tree.finish(),
        });
        plan.encode()?;
        Ok(plan)
    }
    /// Stream target paths/counts and fix one new UID at its verified donor head.
    /// A later head requires a new actual donor proof, not a reused hash string.
    pub fn plan_reclaimed_body(
        &self,
        input: &ReclaimMetadataInput,
        target: &CapturedIndexSource,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        if ActiveIndexIdentity::from(target.construct()) != input.destination().index() {
            return Err(CatalogError::Invalid(
                "首次元数据计划必须使用原已核验 donor 卷头",
            ));
        }
        self.plan_body_at(input, target, path)
    }

    /// A later head requires a new uncached donor read; original retirement and
    /// byte-proof records remain unchanged and are still part of the new plan.
    pub fn plan_reclaimed_body_after_recovery(
        &self,
        input: &ReclaimMetadataInput,
        target: &CapturedIndexSource,
        path: &str,
        donor: &ReclaimMetadataDonorEvidence,
    ) -> Result<ReclaimMetadataPlan> {
        donor.require_capture(input, target)?;
        self.plan_body_at(input, target, path)
    }

    fn plan_body_at(
        &self,
        input: &ReclaimMetadataInput,
        target: &CapturedIndexSource,
        path: &str,
    ) -> Result<ReclaimMetadataPlan> {
        import_spool::validate_path(path).map_err(cache)?;
        if !crate::ltfs::xml_text_valid(path) {
            return Err(CatalogError::Invalid("回收元数据路径包含 XML 非法字符"));
        }
        let (previous_dp_eod, _, _) = current_proof(target.scope(), target.header())?;
        let proof = input.source().byte_proof();
        let destination = input.destination();
        if proof.inventory() != self.summary.digest
            || proof.index() != self.summary.head
            || self.summary.volume != input.source().source().copy().volume_id.to_string()
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
        active_volume::encode_index(&mut previous, target.construct().into());
        let plan = ReclaimMetadataPlan(Plan {
            version: 7,
            parent,
            inventory: self.summary.digest,
            source,
            raw_body: None,
            source_index: self.summary.head.clone(),
            input: input.fingerprint()?,
            volume: destination.copy().volume_id.to_string(),
            previous,
            previous_dp_eod,
            previous_source: target.identity().map_err(cache)?.to_bytes(),
            donor: Object::capture(donor_entry)?,
            ancestors: Vec::new(),
            displaced: None,
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

#[cfg(test)]
#[path = "reclaim_metadata_node_plan_tests.rs"]
mod node_tests;
