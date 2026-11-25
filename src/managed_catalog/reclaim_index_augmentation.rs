//! Immutable native metadata for the original reclaim body batch.
//! These records are intent only. Neither their presence nor a pending donor
//! proves a flush, an index checkpoint, or permission to erase the source.
use super::*;
#[path = "reclaim_group_isolation.rs"]
pub(in crate::managed_catalog) mod isolation;
pub(crate) use isolation::{OriginalObservationPermit, ReclaimExecutionGate};

use crate::ltfs::index::{DirectoryNode, FileNode, IndexEntry, LtfsIndex};
use crate::ltfs::recovery::IndexSourceIdentity;
use crate::ltfs::recovery::{CapturedIndexSource, IndexSourceScope};

const MAX_PAYLOAD_BYTES: usize = 80_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActualMember {
    ordinal: u8,
    receipt: Vec<u8>,
    donor_path: String,
    donor: FileNode,
    archive_uid: u64,
    expected: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Actual {
    version: u8,
    intent: [u8; 32],
    receipts: [u8; 32],
    members: Vec<[u8; 32]>,
    generation: u64,
    index_start: u64,
    highest_uid: u64,
    target_tree: [u8; 32],
    target_objects: u64,
    target_files: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<ActualRoot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActualRoot {
    uid: u64,
    expected: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog::control::reclaim) struct GroupRootReference {
    pub(super) group: String,
    physical_attempt: String,
    batch: String,
    plan: [u8; 32],
    observation: [u8; 32],
    target_uid: u64,
}

/// A deterministic recipe, not a media receipt. Its full native input stays
/// in the sealed intent; actual offsets come only from pending LTFS donors.
#[derive(Clone, Debug)]
pub(crate) struct ReclaimIndexAugmentation {
    intent: ReclaimIndexIntent,
    actual: Actual,
    members: Vec<ActualMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observed {
    version: u8,
    plan: [u8; 32],
    index: Vec<u8>,
    source: Vec<u8>,
    dp_eod: u64,
    construct_end: u64,
}

impl Observed {
    fn require_raw_full_dp(&self) -> Result<()> {
        let identity = IndexSourceIdentity::from_bytes(&self.source).map_err(cache)?;
        let index = active_volume::decode_index(identity.head().volume_uuid, &self.index)?;
        if identity.head().incremental
            || index.incremental
            || index != ActiveIndexIdentity::from(identity.head())
        {
            return Err(CatalogError::Corrupt(
                "回收持久物理证明必须为同一Full DP，禁止Delta",
            ));
        }
        Ok(())
    }
    fn require_full_dp(&self, plan: &ReclaimIndexAugmentation) -> Result<()> {
        self.require_raw_full_dp()?;
        let identity = IndexSourceIdentity::from_bytes(&self.source).map_err(cache)?;
        plan.intent.require_full_dp_construct(identity.head())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBinding {
    version: u8,
    batch: String,
    plan: [u8; 32],
    receipts: [u8; 32],
    manifest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchRoute {
    version: u8,
    parent: serde_json::Value,
    group: String,
    physical_attempt: String,
    batch: String,
    manifest: [u8; 32],
    plan: [u8; 32],
}

fn batch_route_key(namespace: Uuid, batch: Uuid) -> Vec<u8> {
    let mut key = domain(namespace, crate::managed_catalog::RECLAIM_COPY_GROUP_DOMAIN);
    key.push(b'B');
    key.extend_from_slice(batch.as_bytes());
    key
}

/// Actual complete physical observation. A valid body receipt by itself can
/// never construct this final-index/archive proof.
pub(crate) struct ReclaimIndexObservation(Observed);

pub(crate) struct PendingReclaimReference<'a> {
    pub(crate) donor_path: &'a str,
    pub(crate) donor: &'a FileNode,
    pub(crate) archive: &'a str,
    pub(crate) archive_uid: u64,
    pub(crate) source: &'a IndexEntry,
}

fn entry_path(entry: &IndexEntry) -> &str {
    match entry {
        IndexEntry::File { path, .. } | IndexEntry::Directory { path, .. } => path,
    }
}

fn expected_reference(native: &NativeMember, actual: &ActualMember) -> Result<IndexEntry> {
    Ok(IndexEntry::File {
        path: native.archive.clone(),
        node: FileNode::reclaimed_reference(
            &native.archive,
            actual.archive_uid,
            &actual.donor,
            &native.source,
        )
        .map_err(cache)?,
    })
}

fn child_free_directory(path: &str, index: &LtfsIndex) -> Result<IndexEntry> {
    let mut directory = &index.root;
    for component in path.split('/').filter(|p| !p.is_empty()) {
        directory = directory
            .subdirs
            .iter()
            .find(|d| d.name == component)
            .ok_or(CatalogError::Incomplete)?;
    }
    Ok(IndexEntry::Directory {
        path: path.into(),
        node: DirectoryNode {
            name: directory.name.clone(),
            meta: directory.meta.clone(),
            xattrs: directory.xattrs.clone(),
            ..Default::default()
        },
    })
}

/// Merge only the bounded changed paths with the complete original target.
/// Keep every unchanged native node in the canonical TargetTree comparison.
fn final_tree(
    volume: Uuid,
    target: &CapturedIndexSource,
    mut changes: std::collections::BTreeMap<String, IndexEntry>,
) -> Result<([u8; 32], u64, u64)> {
    let mut tree = source::TargetTree::new(volume);
    let mut files = 0u64;
    let mut add = |entry: IndexEntry| -> Result<()> {
        files = files
            .checked_add(u64::from(matches!(&entry, IndexEntry::File { .. })))
            .ok_or(CatalogError::Invalid("回收组目标文件计数溢出"))?;
        tree.add_entry(entry)
    };
    target
        .for_each_entry(|entry| {
            let path = entry_path(&entry);
            while changes
                .first_key_value()
                .is_some_and(|(p, _)| p.as_bytes() < path.as_bytes())
            {
                let (_, added) = changes.pop_first().ok_or_else(|| {
                    crate::error::TapeError::Catalog("回收组目标合并记录缺失".into())
                })?;
                add(added).map_err(|e| crate::error::TapeError::Catalog(e.to_string()))?;
            }
            let replacement = changes.remove(path).unwrap_or(entry);
            add(replacement).map_err(|e| crate::error::TapeError::Catalog(e.to_string()))
        })
        .map_err(cache)?;
    for (_, entry) in changes {
        add(entry)?;
    }
    let objects = tree.objects();
    Ok((tree.finish(), objects, files))
}

impl ReclaimIndexAugmentation {
    pub(crate) fn require_checkpoint_binding(
        &self,
        scope: &ReclaimGroupObservation,
        batch: Uuid,
        previous: ActiveIndexIdentity,
        current: ActiveIndexIdentity,
    ) -> Result<()> {
        self.validate(scope.parent(), scope.group())?;
        if current.incremental
            || batch != scope.batch_id()
            || previous != self.previous()?
            || current.volume_uuid != previous.volume_uuid
            || current.generation != self.actual.generation
            || current.location.partition != previous.location.partition
            || current.location.start_block != self.actual.index_start
        {
            return Err(CatalogError::Invalid("Observe设备必须绑定原实际checkpoint"));
        }
        Ok(())
    }

    pub(crate) fn target_file_count(&self) -> u64 {
        self.actual.target_files
    }
    pub(crate) fn fingerprint(&self) -> Result<[u8; 32]> {
        digest(b"tape-rs/reclaim-index-actual/1", &self.actual)
    }

    pub(crate) fn previous(&self) -> Result<ActiveIndexIdentity> {
        active_volume::decode_index(
            self.intent.intent.destination.volume,
            &self.intent.intent.previous,
        )
    }

    pub(crate) fn index_start(&self) -> u64 {
        self.actual.index_start
    }

    pub(crate) fn first_archive_uid(&self) -> Result<u64> {
        if let Some(root) = &self.actual.root {
            return Ok(root.uid);
        }
        self.members
            .first()
            .map(|m| m.archive_uid)
            .ok_or(CatalogError::Incomplete)
    }

    pub(crate) fn archive_root(&self) -> Result<Option<(String, u64, IndexEntry)>> {
        self.intent
            .intent
            .root
            .as_ref()
            .map(|root| Ok((root.archive.clone(), root.uid, root.relocated()?)))
            .transpose()
    }

    fn root_reference<'a>(
        &'a self,
        group: &ReclaimCopyGroup,
        observed: &Observed,
    ) -> Result<Option<(&'a IndexEntry, GroupRootReference)>> {
        let Some(root) = &self.intent.intent.root else {
            return Ok(None);
        };
        observed.require_full_dp(self)?;
        let index = IndexSourceIdentity::from_bytes(&observed.source)
            .map_err(cache)?
            .head();
        let plan = self.fingerprint()?;
        if observed.version != 1
            || observed.plan != plan
            || observed.dp_eod != observed.construct_end
            || index.generation != self.actual.generation
            || index.location.start_block != self.actual.index_start
            || observed.dp_eod <= self.actual.index_start
            || group.header.group != self.intent.intent.group
        {
            return Err(CatalogError::Corrupt("归档根缺同一组实际Full DP观察"));
        }
        Ok(Some((
            &root.source,
            GroupRootReference {
                group: group.header.group.clone(),
                physical_attempt: group.header.batch.clone(),
                batch: group.header.batch.clone(),
                plan,
                observation: digest(b"tape-rs/reclaim-index-observed/1", observed)?,
                target_uid: root.uid,
            },
        )))
    }

    pub(crate) fn references(&self) -> impl Iterator<Item = PendingReclaimReference<'_>> {
        self.intent
            .members
            .iter()
            .zip(&self.members)
            .map(|(native, actual)| PendingReclaimReference {
                donor_path: &actual.donor_path,
                donor: &actual.donor,
                archive: &native.archive,
                archive_uid: actual.archive_uid,
                source: &native.source,
            })
    }

    fn require_full_dp_tree(&self, capture: &CapturedIndexSource) -> Result<()> {
        let construct = capture.construct();
        self.intent.require_full_dp_construct(construct)?;
        if construct.generation != self.actual.generation
            || construct.location.start_block != self.actual.index_start
            || capture.header().highest_file_uid != self.actual.highest_uid
            || capture.construct_end() <= construct.location.start_block
        {
            return Err(CatalogError::Invalid(
                "回收Full DP代次、位置、UID或关闭边界不符",
            ));
        }
        let mut tree = source::TargetTree::new(construct.volume_uuid);
        let mut files = 0u64;
        capture
            .for_each_entry(|entry| {
                files += u64::from(matches!(&entry, IndexEntry::File { .. }));
                tree.add_entry(entry)
                    .map_err(|e| crate::error::TapeError::Catalog(e.to_string()))
            })
            .map_err(cache)?;
        if tree.objects() != self.actual.target_objects
            || files != self.actual.target_files
            || tree.finish() != self.actual.target_tree
        {
            return Err(CatalogError::Invalid("回收组最终完整目录树与原计划不同"));
        }
        if let Some(root) = &self.intent.intent.root {
            if capture.entry_by_path(&root.archive).map_err(cache)? != Some(root.expected()?) {
                return Err(CatalogError::Invalid("回收组最终归档根的UID或原属性不同"));
            }
        }
        for (native, member) in self.intent.members.iter().zip(&self.members) {
            let donor = capture
                .entry_by_path(member.donor_path.trim_start_matches('/'))
                .map_err(cache)?;
            if donor
                != Some(IndexEntry::File {
                    path: member.donor_path.trim_start_matches('/').into(),
                    node: member.donor.clone(),
                })
                || capture.entry_by_path(&native.archive).map_err(cache)?
                    != Some(expected_reference(native, member)?)
            {
                return Err(CatalogError::Invalid("回收组最终正文或归档引用缺失"));
            }
        }
        Ok(())
    }
    /// Recorded DP only: caller must separately prove the current tail and
    /// owner and persist a typed repair intent. This never creates S4 proof.
    pub(crate) fn verify_dp_repair_candidate(&self, capture: &CapturedIndexSource) -> Result<()> {
        if capture.scope() != IndexSourceScope::Recorded {
            return Err(CatalogError::Invalid("DP修复候选必须为原Recorded物理捕获"));
        }
        self.require_full_dp_tree(capture)
    }
    pub(crate) fn observe(&self, capture: &CapturedIndexSource) -> Result<ReclaimIndexObservation> {
        let IndexSourceScope::Current {
            dp_eod,
            history_truncated: false,
            ..
        } = capture.scope()
        else {
            return Err(CatalogError::Invalid("回收组最终索引需要完整当前物理证明"));
        };
        self.require_full_dp_tree(capture)?;
        if capture.construct_end() != dp_eod {
            return Err(CatalogError::Invalid("回收Full DP关闭边界与当前EOD不同"));
        }
        let mut index = Vec::new();
        active_volume::encode_index(&mut index, capture.construct().into());
        Ok(ReclaimIndexObservation(Observed {
            version: 1,
            plan: self.fingerprint()?,
            index,
            source: capture.identity().map_err(cache)?.to_bytes(),
            dp_eod,
            construct_end: capture.construct_end(),
        }))
    }

    fn validate(&self, parent: &ReclaimReservation, group: &ReclaimCopyGroup) -> Result<()> {
        self.intent.validate(parent, group)?;
        let previous = self.previous()?;
        let root = self
            .intent
            .intent
            .root
            .as_ref()
            .map(IntentRoot::actual)
            .transpose()?;
        if self.actual.version != if root.is_some() { 2 } else { 1 }
            || self.actual.root != root
            || self.actual.intent != self.intent.fingerprint()?
            || self.actual.generation
                != previous
                    .generation
                    .checked_add(1)
                    .ok_or(CatalogError::Incomplete)?
            || self.actual.index_start <= self.intent.intent.previous_dp_eod
            || self.members.len() != group.members.len()
            || self.actual.members.len() != self.members.len()
            || self.actual.target_objects < self.actual.target_files
        {
            return Err(CatalogError::Corrupt("回收组实际索引计划头损坏"));
        }
        let mut receipts = Vec::with_capacity(self.members.len());
        let mut previous_uid = root.as_ref().map(|root| root.uid);
        for ((native, member), expected) in self
            .intent
            .members
            .iter()
            .zip(&self.members)
            .zip(&self.actual.members)
        {
            if member.ordinal != native.ordinal
                || member.receipt.len() != 32
                || member.donor.meta.file_uid <= 1
                || root
                    .as_ref()
                    .is_some_and(|root| member.donor.meta.file_uid >= root.uid)
                || member.archive_uid <= member.donor.meta.file_uid
                || previous_uid.is_some_and(|uid| uid.checked_add(1) != Some(member.archive_uid))
                || digest(b"tape-rs/reclaim-index-actual-member/1", member)? != *expected
                || digest(
                    b"tape-rs/reclaim-index-expected/1",
                    &expected_reference(native, member)?,
                )? != member.expected
            {
                return Err(CatalogError::Corrupt("回收组实际正文或归档计划损坏"));
            }
            previous_uid = Some(member.archive_uid);
            receipts.push(member.receipt.clone());
            native_bytes(member)?;
        }
        if previous_uid != Some(self.actual.highest_uid)
            || digest(b"tape-rs/reclaim-index-receipts/1", &receipts)? != self.actual.receipts
        {
            return Err(CatalogError::Corrupt("回收组实际计划UID或原收据集合改变"));
        }
        native_bytes(&self.actual)?;
        Ok(())
    }

    /// No I/O. The caller persists this exact recipe before staging it on the
    /// same working volume or sealing the original active_batch.
    pub(crate) fn derive(
        intent: &ReclaimIndexIntent,
        group: &ReclaimCopyGroup,
        receipts: &[CommitReceipt],
        paths: &[String],
        volume: &crate::ltfs::volume::LtfsVolume<'_>,
        target: &CapturedIndexSource,
    ) -> Result<Self> {
        if receipts.len() != group.members.len()
            || paths.len() != receipts.len()
            || target.identity().map_err(cache)?.to_bytes() != intent.intent.previous_source
        {
            return Err(CatalogError::Invalid(
                "回收组实际计划必须覆盖原完整成员及原目标",
            ));
        }
        let previous =
            active_volume::decode_index(group.header.destination.volume, &intent.intent.previous)?;
        if ActiveIndexIdentity::from(volume.committed_index_construct()) != previous {
            return Err(CatalogError::Invalid("回收组实际计划卷头改变"));
        }
        let (working, index_start) = volume.reclaim_pending_index().map_err(cache)?;
        let mut next_uid = working.highest_file_uid;
        let mut members = Vec::with_capacity(receipts.len());
        let mut changes = std::collections::BTreeMap::new();
        let mut receipt_digests = Vec::with_capacity(receipts.len());
        let root = if let Some(root) = &intent.intent.root {
            if next_uid.checked_add(1) != Some(root.uid)
                || working.find_directory(&root.archive).is_some()
                || working.find_file(&root.archive).is_some()
            {
                return Err(CatalogError::Invalid("回收待追加归档根的原UID或路径改变"));
            }
            next_uid = root.uid;
            changes.insert(root.archive.clone(), root.expected()?);
            Some(root.actual()?)
        } else {
            None
        };
        for (native, selected) in intent.members.iter().zip(&group.members) {
            let position = receipts
                .iter()
                .position(|r| {
                    r.operation_id.to_string() == native.child.operation
                        && r.request_epoch == native.child.epoch
                })
                .ok_or(CatalogError::Incomplete)?;
            let receipt = &receipts[position];
            validate_receipt(receipt)?;
            if receipt.file_id != selected.file.file_id()
                || receipt.expected_version != Some(selected.file.version())
                || receipt.volume_id != previous.volume_uuid
                || Some(receipt.volume_generation) != previous.generation.checked_add(1)
                || receipt.volume_scan_id.to_string() != group.header.scan
                || receipt.content_digest != ContentDigest::Sha256(selected.known_sha256)
                || receipt.size != selected.file.size()
            {
                return Err(CatalogError::Invalid("回收组正文实际收据与原成员不同"));
            }
            let path = &paths[position];
            let donor = volume
                .pending_file_node(path)
                .ok_or(CatalogError::Incomplete)?;
            if donor.meta.file_uid != receipt.ltfs_file_uid
                || donor.length != receipt.size
                || ContentDigest::from_ltfs_xattrs(&donor.xattrs)? != receipt.content_digest
                || NodeAttributes::from_file(donor)?.digest()
                    != receipt.metadata_digest.ok_or(CatalogError::Incomplete)?
                || working.find_file(&native.archive).is_some()
            {
                return Err(CatalogError::Invalid(
                    "回收组待提交正文或归档位置偏离原计划",
                ));
            }
            next_uid = next_uid
                .checked_add(1)
                .ok_or(CatalogError::Invalid("回收组UID耗尽"))?;
            let mut member = ActualMember {
                ordinal: native.ordinal,
                receipt: receipt_fingerprint(receipt),
                donor_path: path.clone(),
                donor: donor.clone(),
                archive_uid: next_uid,
                expected: [0; 32],
            };
            let expected = expected_reference(native, &member)?;
            member.expected = digest(b"tape-rs/reclaim-index-expected/1", &expected)?;
            native_bytes(&member)?;
            receipt_digests.push(member.receipt.clone());
            changes.insert(
                path.trim_start_matches('/').to_owned(),
                IndexEntry::File {
                    path: path.trim_start_matches('/').into(),
                    node: donor.clone(),
                },
            );
            changes.insert(native.archive.clone(), expected);
            for file in [path.trim_start_matches('/'), native.archive.as_str()] {
                let mut ancestor = file.rsplit_once('/').map_or("", |(p, _)| p);
                loop {
                    if !changes.contains_key(ancestor) {
                        changes.insert(ancestor.into(), child_free_directory(ancestor, working)?);
                    }
                    if ancestor.is_empty() {
                        break;
                    }
                    ancestor = ancestor.rsplit_once('/').map_or("", |(p, _)| p);
                }
            }
            members.push(member);
        }
        let (target_tree, target_objects, target_files) =
            final_tree(previous.volume_uuid, target, changes)?;
        let actual = Actual {
            version: if root.is_some() { 2 } else { 1 },
            intent: intent.fingerprint()?,
            receipts: digest(b"tape-rs/reclaim-index-receipts/1", &receipt_digests)?,
            members: members
                .iter()
                .map(|m| digest(b"tape-rs/reclaim-index-actual-member/1", m))
                .collect::<Result<_>>()?,
            generation: previous
                .generation
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?,
            index_start,
            highest_uid: next_uid,
            target_tree,
            target_objects,
            target_files,
            root,
        };
        native_bytes(&actual)?;
        Ok(Self {
            intent: intent.clone(),
            actual,
            members,
        })
    }
}

pub(super) async fn actual_for_attempt_at(tr:&Transaction,parent:&ReclaimReservation,group:&ReclaimCopyGroup)->Result<Option<ReclaimIndexAugmentation>> {
    if tr.get(&key(parent,b'a',Some(group.id()),None),false).await?.is_none(){return Ok(None);}
    require_actual_at(tr,parent,group).await.map(Some)
}
async fn require_actual_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<ReclaimIndexAugmentation> {
    let intent = require_intent_at(tr, parent, group).await?;
    let raw = tr
        .get(&key(parent, b'a', Some(group.id()), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let actual = native_decode(&raw)?;
    let mut members = Vec::with_capacity(group.members.len());
    for ordinal in 0..group.header.count {
        let raw = tr
            .get(&key(parent, b'd', Some(group.id()), Some(ordinal)), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        members.push(native_decode(&raw)?);
    }
    let result = ReclaimIndexAugmentation {
        intent,
        actual,
        members,
    };
    result.validate(parent, group)?;
    Ok(result)
}

/// Recreate only the exact original physical receipt from sealed facts.
/// Current HA ownership never rewrites its issuing execution epoch.
async fn original_member_receipt_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
    plan: &ReclaimIndexAugmentation,
    ordinal: u8,
) -> Result<PreparedActiveFile> {
    let selected = group
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let actual = plan
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let mut intent = original_child_at(tr, parent, owner, selected).await?;
    intent.owner_epoch = group
        .header
        .execution_epoch
        .ok_or(CatalogError::Incomplete)?;
    let file = crate::managed_catalog::physical::prepare_file_node(
        &actual.donor_path,
        &intent,
        &actual.donor,
    )?;
    if receipt_fingerprint(&file.receipt) != actual.receipt {
        return Err(CatalogError::Invalid(
            "原实际收据重建不匹配，必须核验原操作",
        ));
    }
    Ok(file)
}

pub(in crate::managed_catalog) async fn require_publication_at(
    tr: &Transaction,
    batch: Uuid,
    receipt: &CommitReceipt,
    index: ActiveIndexIdentity,
) -> Result<()> {
    let Some((parent, _)) = children::parent_for_receipt_at(tr, receipt).await? else {
        return Ok(());
    };
    if parent.intent()["version"] == 1 {
        return Ok(());
    }
    let owner = super::super::decode(
        &tr.get(&parent.journal_key(), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?["owner_epoch"]
        .as_u64()
        .ok_or(CatalogError::Incomplete)?;
    let reverse = tr
        .get(&key(&parent, b'r', Some(receipt.operation_id), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if reverse.len() != 17 {
        return Err(CatalogError::Corrupt("回收收据的原分组edge缺失或改变"));
    }
    let id = Uuid::from_slice(&reverse[..16])
        .map_err(|_| CatalogError::Corrupt("回收收据的原组ID无效"))?;
    let ordinal = reverse[16];
    let group = evidence_group_at(tr, &parent, id).await?;
    if group.header.version != 3 || group.batch_id() != batch || group.header.owner > owner {
        return Err(CatalogError::Invalid(
            "回收收据必须发布原有augmentation的S4批次",
        ));
    }
    let plan = require_actual_at(tr, &parent, &group).await?;
    let member = plan
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    if member.receipt != receipt_fingerprint(receipt) {
        return Err(CatalogError::Invalid("回收发布不是原成员实际收据"));
    }
    let raw = tr
        .get(&key(&parent, b'p', Some(group.id()), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let observed: Observed = native_decode(&raw)?;
    observed.require_full_dp(&plan)?;
    let mut actual_index = Vec::new();
    active_volume::encode_index(&mut actual_index, index);
    let identity = IndexSourceIdentity::from_bytes(&observed.source).map_err(cache)?;
    if observed.version != 1
        || observed.plan != plan.fingerprint()?
        || observed.index != actual_index
        || ActiveIndexIdentity::from(identity.head()) != index
        || observed.dp_eod != observed.construct_end
        || index.generation != plan.actual.generation
        || index.location.start_block != plan.actual.index_start
    {
        return Err(CatalogError::Invalid("回收最终索引/归档证明缺失或不匹配"));
    }
    let raw = tr
        .get(&key(&parent, b'b', Some(group.id()), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let binding: BatchBinding = native_decode(&raw)?;
    if binding.version != 1
        || binding.batch != batch.to_string()
        || binding.plan != plan.fingerprint()?
    {
        return Err(CatalogError::Corrupt("回收最终证明未绑定原active_batch"));
    }
    Ok(())
}

/// Read physical group evidence without resolving current logical paths.
/// Native publication may itself be the provider of those logical paths.
async fn evidence_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    id: Uuid,
) -> Result<ReclaimCopyGroup> {
    let raw = tr
        .get(&key(parent, b'h', Some(id), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let header: Header = decode(&raw)?;
    let rows = read_page(
        tr,
        &member_prefix(parent, id),
        None,
        usize::from(MAX_MEMBERS) + 1,
        false,
    )
    .await?;
    if rows.next.is_some() || rows.items.len() != usize::from(header.count) {
        return Err(CatalogError::Incomplete);
    }
    let mut members = Vec::with_capacity(rows.items.len());
    for (ordinal, (actual_key, raw)) in rows.items.into_iter().enumerate() {
        if actual_key != key(parent, b'm', Some(id), Some(ordinal as u8)) {
            return Err(CatalogError::Corrupt("回收物理证明成员ordinal改变"));
        }
        members.push(decode(&raw)?);
    }
    let group = ReclaimCopyGroup { header, members };
    group.require_members(parent)?;
    group_layout::require_identity_at(tr, parent, &group).await?;
    if group.id() != id {
        return Err(CatalogError::Corrupt("回收物理证明组身份改变"));
    }
    group_attempt::require_group_at(tr,parent,&group).await?;
    if group.header.phase == Phase::Closed {
        group_close::require_closed_evidence_at(tr, parent, &group).await?;
    }
    Ok(group)
}

#[path = "reclaim_group_destination_history.rs"]
mod destination_history;
#[path = "reclaim_group_observation.rs"]
mod group_observation;
pub(crate) use group_observation::ReclaimGroupObservation;
#[path = "reclaim_original_checkpoint.rs"]
mod original_checkpoint;
pub(crate) use original_checkpoint::{OriginalCheckpointContext, OriginalCheckpointPublication};

#[path = "reclaim_group_member_context.rs"]
mod member_context;
pub(crate) use member_context::ReclaimGroupMemberReadContext;
#[path = "reclaim_group_close.rs"]
mod group_close;
pub(super) async fn audit_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    id: Uuid,
) -> Result<(u64, Vec<u8>, [u8; 32])> {
    group_close::audit_group_at(tr, parent, owner, id).await
}
pub(super) async fn audit_row_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    tail: &[u8],
    raw: &[u8],
) -> Result<()> {
    group_close::audit_row_at(tr, parent, tail, raw).await
}

#[path = "reclaim_group_settlement.rs"]
mod group_settlement;
pub(in crate::managed_catalog) use group_settlement::{
    GroupFileReference, group_member_coverage_at, group_member_coverage_in_at, GroupCoverageMode, GroupRetirementPins,
};

/// Header protocol 3 calls this unconditionally. A missing route, intent,
/// actual plan or physical proof is never interpreted as an ordinary upload.
pub(in crate::managed_catalog) async fn require_checkpoint_at(
    tr: &Transaction,
    namespace: Uuid,
    batch: Uuid,
    manifest: [u8; 32],
    index: ActiveIndexIdentity,
    file_count: Option<u64>,
) -> Result<()> {
    let raw = tr
        .get(&batch_route_key(namespace, batch), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let route: BatchRoute = native_decode(&raw)?;
    validate_parent_intent(&route.parent)?;
    if route.version != 2
        || route.parent["version"] != 2
        || route.parent["namespace"] != namespace.to_string()
        || route.batch != batch.to_string()
        || route.manifest != manifest
    {
        return Err(CatalogError::Corrupt("required回收批次路由与原Header不同"));
    }
    let serial = route.parent["library"]
        .as_str()
        .ok_or(CatalogError::Incomplete)?;
    let active_route = routing::decode_active(
        &tr.get(&routing::active_key(serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let parent = ReclaimReservation {
        intent: route.parent.clone(),
        route: active_route,
    };
    let group_id = parse_uuid(&route.group)?;
    if parse_uuid(&route.physical_attempt)? != batch {
        return Err(CatalogError::Invalid("原回收batch路由physical attempt不同"));
    }
    if tr
        .get(
            &group_layout::control_key(&parent, b'b', Some(batch)),
            false,
        )
        .await?
        .as_deref()
        != Some(raw.as_ref())
    {
        return Err(CatalogError::Corrupt("回收parent/global batch路由不一致"));
    }
    let group = evidence_group_at(tr, &parent, group_id).await?;
    if group.header.phase != Phase::Closed {
        let raw = tr
            .get(&parent.journal_key(), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let owner = super::super::decode(&raw)?["owner_epoch"]
            .as_u64()
            .ok_or(CatalogError::Incomplete)?;
        parent_at(tr, &parent, owner).await?;
        source_state_at(tr, &parent).await?;
        if group.header.owner > owner
            || tr
                .get(&key(&parent, b'a', None, None), false)
                .await?
                .as_deref()
                != Some(group_id.as_bytes())
        {
            return Err(CatalogError::OwnerFenced);
        }
    }
    if group.header.version != 3
        || group.batch_id() != batch
        || !matches!(
            group.header.phase,
            Phase::Admitted | Phase::Settling | Phase::Closed
        )
    {
        return Err(CatalogError::Invalid("required回收批次不是原已执行组"));
    }
    let plan = require_actual_at(tr, &parent, &group).await?;
    let raw = tr
        .get(&key(&parent, b'b', Some(batch), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let binding: BatchBinding = native_decode(&raw)?;
    let raw = tr
        .get(&key(&parent, b'p', Some(batch), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let observed: Observed = native_decode(&raw)?;
    observed.require_full_dp(&plan)?;
    let mut actual_index = Vec::new();
    active_volume::encode_index(&mut actual_index, index);
    let identity = IndexSourceIdentity::from_bytes(&observed.source).map_err(cache)?;
    let file_count = match file_count {
        Some(count) => count,
        None => active_volume::stored_file_count_at(tr, index)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    };
    if route.plan != plan.fingerprint()?
        || binding.version != 1
        || binding.plan != route.plan
        || binding.batch != route.batch
        || binding.manifest != manifest
        || observed.version != 1
        || observed.plan != route.plan
        || observed.index != actual_index
        || ActiveIndexIdentity::from(identity.head()) != index
        || observed.construct_end != observed.dp_eod
        || index.generation != plan.actual.generation
        || index.location.start_block != plan.actual.index_start
        || index.location.partition != plan.previous()?.location.partition
        || file_count != plan.actual.target_files
    {
        return Err(CatalogError::Invalid(
            "required回收批次缺少原完整索引、归档或文件数证明",
        ));
    }
    Ok(())
}

pub(super) async fn extend_expected_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    expected: &mut std::collections::BTreeSet<Vec<u8>>,
) -> Result<()> {
    let id = group.id();
    if tr
        .get(&key(parent, b'i', Some(id), None), false)
        .await?
        .is_some()
    {
        require_intent_at(tr, parent, group).await?;
        expected.insert(key(parent, b'i', Some(id), None));
        for ordinal in 0..group.header.count {
            expected.insert(key(parent, b'n', Some(id), Some(ordinal)));
        }
    }
    let actual = if tr
        .get(&key(parent, b'a', Some(id), None), false)
        .await?
        .is_some()
    {
        let actual = require_actual_at(tr, parent, group).await?;
        if !matches!(group.header.phase, Phase::Admitted | Phase::Settling) {
            return Err(CatalogError::Corrupt("未准入回收组不能有实际追加计划"));
        }
        expected.insert(key(parent, b'a', Some(id), None));
        for ordinal in 0..group.header.count {
            expected.insert(key(parent, b'd', Some(id), Some(ordinal)));
        }
        Some(actual)
    } else {
        None
    };
    if let Some(raw) = tr.get(&key(parent, b'p', Some(id), None), false).await? {
        let actual = actual.ok_or(CatalogError::Incomplete)?;
        let observed: Observed = native_decode(&raw)?;
        observed.require_full_dp(&actual)?;
        if observed.version != 1
            || observed.plan != actual.fingerprint()?
            || observed.dp_eod != observed.construct_end
        {
            return Err(CatalogError::Corrupt("回收组物理证明未绑定原实际计划"));
        }
        expected.insert(key(parent, b'p', Some(id), None));
    }
    if let Some(raw) = tr.get(&key(parent, b'b', Some(id), None), false).await? {
        let binding: BatchBinding = native_decode(&raw)?;
        let actual = require_actual_at(tr, parent, group).await?;
        let mut receipts = actual
            .members
            .iter()
            .map(|m| m.receipt.clone())
            .collect::<Vec<_>>();
        receipts.sort();
        if binding.version != 1
            || binding.batch != group.header.batch
            || binding.plan != actual.fingerprint()?
            || binding.receipts != digest(b"tape-rs/reclaim-index-batch-receipts/1", &receipts)?
        {
            return Err(CatalogError::Corrupt("回收组与原S4批次绑定损坏"));
        }
        let raw = tr
            .get(
                &batch_route_key(parent.namespace(), group.batch_id()),
                false,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let route: BatchRoute = native_decode(&raw)?;
        if route.version != 2
            || route.group != group.header.group
            || route.physical_attempt != group.header.batch
            || tr
                .get(
                    &group_layout::control_key(parent, b'b', Some(group.batch_id())),
                    false,
                )
                .await?
                .as_deref()
                != Some(raw.as_ref())
            || route.parent != *parent.intent()
            || route.batch != binding.batch
            || route.manifest != binding.manifest
            || route.plan != binding.plan
        {
            return Err(CatalogError::Corrupt("回收组原S4强制路由丢失或改变"));
        }
        expected.insert(key(parent, b'b', Some(id), None));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Freeze the exact offsets and final tree recipe learned from the
    /// original append. An acknowledged or unknown retry never replaces it.
    pub(crate) async fn stage_reclaim_index_augmentation(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        plan: &ReclaimIndexAugmentation,
        receipts: &[CommitReceipt],
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || group.header.version != 3
                                || group.header.phase != Phase::Admitted
                            {
                                return Err(CatalogError::Invalid(
                                    "实际索引计划只能绑定原已准入v2组",
                                ));
                            }
                            plan.validate(parent, &group)?;
                            let original = require_intent_at(&tr, parent, &group).await?;
                            if original.intent != plan.intent.intent
                                || original.members != plan.intent.members
                                || receipts.len() != group.members.len()
                            {
                                return Err(CatalogError::Invalid(
                                    "回收组实际计划改变原生输入或成员集合",
                                ));
                            }
                            for (selected, actual) in group.members.iter().zip(&plan.members) {
                                let child = original_child_at(&tr, parent, owner, selected).await?;
                                let receipt = receipts
                                    .iter()
                                    .find(|r| {
                                        r.operation_id == child.operation_id
                                            && r.request_epoch == child.request_epoch
                                    })
                                    .ok_or(CatalogError::Incomplete)?;
                                validate_receipt(receipt)?;
                                if actual.receipt != receipt_fingerprint(receipt)
                                    || actual.donor.meta.file_uid != receipt.ltfs_file_uid
                                    || actual.donor.length != receipt.size
                                {
                                    return Err(CatalogError::Invalid(
                                        "回收组实际计划不是原正文收据",
                                    ));
                                }
                            }
                            if tr
                                .get(&key(parent, b'a', Some(id), None), false)
                                .await?
                                .is_some()
                            {
                                let saved = require_actual_at(&tr, parent, &group).await?;
                                if saved.actual != plan.actual || saved.members != plan.members {
                                    return Err(CatalogError::Invalid(
                                        "回收组原实际索引计划不能更换或重放追加",
                                    ));
                                }
                                return Ok(());
                            }
                            for member in &plan.members {
                                tr.set(
                                    &key(parent, b'd', Some(id), Some(member.ordinal)),
                                    &native_bytes(member)?,
                                );
                            }
                            tr.set(
                                &key(parent, b'a', Some(id), None),
                                &native_bytes(&plan.actual)?,
                            );
                            Ok(())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }

    pub(crate) async fn record_reclaim_index_observation(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        observation: &ReclaimIndexObservation,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || !matches!(group.header.phase, Phase::Admitted | Phase::Settling)
                            {
                                return Err(CatalogError::Invalid(
                                    "回收组最终物理证明必须结清原已准入组",
                                ));
                            }
                            let plan = require_actual_at(&tr, parent, &group).await?;
                            let observed = &observation.0;
                            observed.require_full_dp(&plan)?;
                            if observed.version != 1
                                || observed.plan != plan.fingerprint()?
                                || observed.dp_eod != observed.construct_end
                            {
                                return Err(CatalogError::Invalid(
                                    "回收组最终证明不同于原实际计划",
                                ));
                            }
                            let proof_key = key(parent, b'p', Some(id), None);
                            if let Some(raw) = tr.get(&proof_key, false).await? {
                                if native_decode::<Observed>(&raw)? != *observed {
                                    return Err(CatalogError::Invalid("回收组原物理证明不能更换"));
                                }
                                return Ok(());
                            }
                            tr.set(&proof_key, &native_bytes(observed)?);
                            Ok(())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{NodeMeta, Xattr};

    fn native() -> NativeMember {
        NativeMember {
            version: 1,
            ordinal: 0,
            selected: [2; 32],
            child: ChildRef {
                operation: Uuid::new_v4().to_string(),
                epoch: 7,
                request: [3; 32],
                source_read: [4; 32],
            },
            archive: "_reclaim_original/data-9".into(),
            source: IndexEntry::File {
                path: "original/name".into(),
                node: FileNode {
                    name: "name".into(),
                    meta: NodeMeta {
                        file_uid: 9,
                        readonly: true,
                        creation_time: "2026-11-17T00:00:00.000000001Z".into(),
                        ..Default::default()
                    },
                    xattrs: vec![Xattr {
                        key: "user.original".into(),
                        value: "原始属性".into(),
                        base64: false,
                    }],
                    ..Default::default()
                },
            },
        }
    }

    #[test]
    fn native_payload_is_reconstructible_and_truncation_never_becomes_intent() {
        let original = native();
        let raw = native_bytes(&original).unwrap();
        assert_eq!(native_decode::<NativeMember>(&raw).unwrap(), original);
        for length in 0..raw.len() {
            assert!(
                native_decode::<NativeMember>(&raw[..length]).is_err(),
                "truncated at {length}"
            );
        }
        let mut changed = original.clone();
        let IndexEntry::File { node, .. } = &mut changed.source else {
            unreachable!()
        };
        node.xattrs[0].value.push('x');
        assert_ne!(
            payload_digest(&original).unwrap(),
            payload_digest(&changed).unwrap()
        );
    }

    #[test]
    fn payload_codec_rejects_unknown_fields_and_oversized_native_attributes() {
        let original = native();
        let mut value = serde_json::to_value(&original).unwrap();
        value["success"] = serde_json::json!(true);
        assert!(native_decode::<NativeMember>(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut oversized = original;
        let IndexEntry::File { node, .. } = &mut oversized.source else {
            unreachable!()
        };
        node.xattrs[0].value = "x".repeat(MAX_PAYLOAD_BYTES);
        assert!(native_bytes(&oversized).is_err());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeMember {
    version: u8,
    ordinal: u8,
    selected: [u8; 32],
    child: ChildRef,
    archive: String,
    // Retain the full original native representation, including unknown
    // attributes. A node fingerprint cannot reconstruct an LTFS checkpoint.
    source: IndexEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GroupCheckpointPolicy {
    FullDp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentRoot {
    archive: String,
    source: IndexEntry,
    previous_highest_uid: u64,
    uid: u64,
}

impl IntentRoot {
    fn prepare(
        archive: &str,
        source: IndexEntry,
        target: &CapturedIndexSource,
        donors: &[String],
    ) -> Result<Self> {
        let paths: std::collections::BTreeSet<_> = donors
            .iter()
            .map(|path| path.trim_start_matches('/'))
            .collect();
        if paths.len() != donors.len()
            || !matches!(
                target.entry_by_path("").map_err(cache)?,
                Some(IndexEntry::Directory { .. })
            )
        {
            return Err(CatalogError::Invalid("回收正文路径重复或目标卷根缺失"));
        }
        let mut missing = std::collections::BTreeSet::new();
        for donor in &paths {
            let donor = *donor;
            if donor == archive
                || donor
                    .strip_prefix(archive)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            {
                return Err(CatalogError::Invalid("回收正文路径占用原归档根"));
            }
            if target.entry_by_path(donor).map_err(cache)?.is_some() {
                return Err(CatalogError::Invalid("回收正文路径已被原目标节点占用"));
            }
            let mut ancestor = donor.rsplit_once('/').map(|(parent, _)| parent);
            while let Some(path) = ancestor {
                if paths.contains(path) {
                    return Err(CatalogError::Invalid("回收正文之间存在父路径冲突"));
                }
                match target.entry_by_path(path).map_err(cache)? {
                    Some(IndexEntry::Directory { .. }) => break,
                    Some(_) => return Err(CatalogError::Invalid("回收正文父路径已被文件占用")),
                    None => {
                        missing.insert(path);
                    }
                }
                ancestor = path.rsplit_once('/').map(|(parent, _)| parent);
            }
        }
        let previous_highest_uid = target.header().highest_file_uid;
        let count = donors.len() as u64;
        let uid = previous_highest_uid
            .checked_add(count + missing.len() as u64 + 1)
            .filter(|uid| uid.checked_add(count).is_some())
            .ok_or(CatalogError::Invalid("回收组归档UID预算耗尽"))?;
        Ok(Self {
            archive: archive.into(),
            source,
            previous_highest_uid,
            uid,
        })
    }

    fn relocated(&self) -> Result<IndexEntry> {
        source::relocated_group_root(&self.source, &self.archive)
    }

    fn expected(&self) -> Result<IndexEntry> {
        self.relocated()?
            .reclaimed_metadata_node(&self.archive, self.uid)
            .map_err(cache)
    }

    fn actual(&self) -> Result<ActualRoot> {
        Ok(ActualRoot {
            uid: self.uid,
            expected: source::source_entry_fingerprint(self.expected()?)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u8,
    checkpoint_policy: GroupCheckpointPolicy,
    parent: String,
    group: String,
    batch: String,
    members: [u8; 32],
    source: Vec<u8>,
    destination: ReclaimDriveBinding,
    scan: String,
    previous: Vec<u8>,
    previous_source: Vec<u8>,
    previous_dp_eod: u64,
    additional_files: u64,
    payloads: Vec<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<IntentRoot>,
}

/// Constructible only after all original child requests and complete native
/// payload rows have been checked in the same owner-fenced transaction.
#[derive(Clone, Debug)]
pub(crate) struct ReclaimIndexIntent {
    intent: Intent,
    members: Vec<NativeMember>,
}

/// Fresh-write scope for exactly one original admitted group. Recovery-only
/// admissions cannot mint this value. Every CDB revalidates its durable facts.
pub(crate) struct ReclaimGroupExecution {
    parent: ReclaimReservation,
    group: ReclaimCopyGroup,
    intent: ReclaimIndexIntent,
    execution_epoch: u64,
}

impl ReclaimCopyGroupAdmission {
    pub(crate) fn execution_scope(
        self,
        parent: &ReclaimReservation,
        intent: &ReclaimIndexIntent,
    ) -> Result<ReclaimGroupExecution> {
        if parent.intent()["version"] != 2
            || self.needs_observation
            || self.group.header.version != 3
            || self.group.header.phase != Phase::Admitted
            || self.group.header.execution_epoch != Some(self.group.header.owner)
        {
            return Err(CatalogError::Invalid(
                "未知或接管的回收组只能核验原操作，不能重新追加",
            ));
        }
        intent.validate(parent, &self.group)?;
        Ok(ReclaimGroupExecution {
            parent: parent.clone(),
            group: self.group.clone(),
            intent: intent.clone(),
            execution_epoch: self.group.header.owner,
        })
    }
}

struct BatchBindingRows {
    rows: [(Vec<u8>, Vec<u8>); 3],
    present: [bool; 3],
}

impl ReclaimGroupExecution {
    pub(crate) fn batch_id(&self) -> Uuid {
        self.group.batch_id()
    }
    pub(crate) fn execution_epoch(&self) -> u64 {
        self.execution_epoch
    }
    pub(crate) fn parent(&self) -> &ReclaimReservation {
        &self.parent
    }
    pub(crate) fn intent(&self) -> &ReclaimIndexIntent {
        &self.intent
    }
    pub(crate) fn group(&self) -> &ReclaimCopyGroup {
        &self.group
    }
    pub(crate) fn destination_drive_serial(&self) -> &str {
        &self.group.header.destination.drive_serial
    }

    pub(crate) fn relocation_metadata(
        &self,
        intent: &ActiveFileIntent,
    ) -> Result<(crate::ltfs::volume::FileMetadata, usize)> {
        let member = self
            .intent
            .members
            .iter()
            .find(|member| {
                member.child.operation == intent.operation_id.to_string()
                    && member.child.epoch == intent.request_epoch
                    && member.child.request == intent.request().fingerprint()
            })
            .ok_or(CatalogError::Invalid("回收属性不是原分组请求"))?;
        let IndexEntry::File { node, .. } = &member.source else {
            return Err(CatalogError::Corrupt("回收原生正文属性不是文件"));
        };
        NodeAttributes::from_file(node)?.data_relocation()
    }

    async fn require_at(
        &self,
        tr: &Transaction,
        intents: &[ActiveFileIntent],
    ) -> Result<ReclaimCopyGroup> {
        source::group_only::require_seal_at(tr,&self.parent).await?;
        isolation::require_fresh_at(tr, &self.parent).await?;
        let current = load_at(tr, &self.parent, self.execution_epoch)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if current.id() != self.group.id()
            || current.header.version != 3
            || current.header.phase != Phase::Admitted
            || current.header.owner != self.execution_epoch
            || current.header.execution_epoch != Some(self.execution_epoch)
            || current.header.members != self.group.header.members
            || current.header.previous != self.group.header.previous
            || current.header.layout != self.group.header.layout
            || current.header.order != self.group.header.order
            || intents.len() != current.members.len()
        {
            return Err(CatalogError::OwnerFenced);
        }
        let saved = require_intent_at(tr, &self.parent, &current).await?;
        if saved.intent != self.intent.intent || saved.members != self.intent.members {
            return Err(CatalogError::Corrupt("回收组执行期原生元数据封存改变"));
        }
        require_parent_pool_at(tr, &self.parent).await?;
        let layout = layout::at(tr, &self.parent)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if layout.destination != current.header.destination
            || digest(b"tape-rs/reclaim-group-layout/1", &layout)? != current.header.layout
        {
            return Err(CatalogError::Invalid("回收组执行期原布局改变"));
        }
        let assignment = ManagedCatalog::verified_assignment(
            tr,
            self.parent.serial(),
            &current.header.destination.barcode,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
        if assignment.namespace != self.parent.namespace()
            || assignment.pool_id != self.parent.spec().pool()
            || assignment.volume_id != Some(current.header.destination.volume)
            || assignment.revision != current.header.destination.revision
            || !assignment.allows_io()
        {
            return Err(CatalogError::Invalid(
                "回收组执行期目标分配、修订或策略改变",
            ));
        }
        let (scan, index) =
            active_volume::observed_at(tr, self.parent.serial(), current.header.destination.volume)
                .await?;
        let mut observed = Vec::new();
        active_volume::encode_index(&mut observed, index);
        if scan.id.to_string() != current.header.scan || observed != current.header.previous {
            return Err(CatalogError::Invalid("回收组Fresh执行期目标卷头或scan改变"));
        }
        if tr
            .get(&key(&self.parent, b'p', Some(current.id()), None), false)
            .await?
            .is_some()
        {
            return Err(CatalogError::Invalid(
                "已观察原checkpoint的回收组不能重用Fresh写入范围",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for (position, ordinal) in current.header.order.iter().enumerate() {
            let member = current
                .members
                .get(usize::from(*ordinal))
                .ok_or(CatalogError::Incomplete)?;
            let original =
                original_child_at(tr, &self.parent, self.execution_epoch, member).await?;
            if !seen.insert(original.operation_id) || intents.get(position) != Some(&original) {
                return Err(CatalogError::Invalid("回收组执行必须覆盖原完整物理请求"));
            }
        }
        source::require_metadata_idle_at(tr, &self.parent).await?;
        mechanics::require_parent_idle_at(tr, &self.parent).await?;
        Ok(current)
    }

    pub(in crate::managed_catalog) async fn require_request_at(
        &self,
        tr: &Transaction,
        request: &ActiveFileRequest,
        owner: u64,
    ) -> Result<()> {
        if owner != self.execution_epoch {
            return Err(CatalogError::OwnerFenced);
        }
        let mut intents = Vec::with_capacity(self.group.members.len());
        for ordinal in &self.group.header.order {
            let member = self
                .group
                .members
                .get(usize::from(*ordinal))
                .ok_or(CatalogError::Incomplete)?;
            intents.push(original_child_at(tr, &self.parent, owner, member).await?);
        }
        self.require_at(tr, &intents).await?;
        if intents
            .iter()
            .filter(|intent| intent.request() == *request)
            .count()
            != 1
        {
            return Err(CatalogError::Invalid("写入请求不是原固定回收组成员"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn stage_binding_keys_for_test(&self) -> [Vec<u8>; 3] {
        [
            key(&self.parent, b'b', Some(self.group.id()), None),
            batch_route_key(self.parent.namespace(), self.batch_id()),
            group_layout::control_key(&self.parent, b'b', Some(self.batch_id())),
        ]
    }
    async fn checked_batch_binding_rows_at(
        &self,
        tr: &Transaction,
        batch_id: Uuid,
        receipts: &[CommitReceipt],
        manifest: [u8; 32],
    ) -> Result<BatchBindingRows> {
        if batch_id != self.batch_id() {
            return Err(CatalogError::Invalid("回收组不得改用新S4批次"));
        }
        let mut intents = Vec::with_capacity(self.group.members.len());
        for ordinal in &self.group.header.order {
            let member = self
                .group
                .members
                .get(usize::from(*ordinal))
                .ok_or(CatalogError::Incomplete)?;
            intents.push(original_child_at(tr, &self.parent, self.execution_epoch, member).await?);
        }
        let current = self.require_at(tr, &intents).await?;
        let actual = require_actual_at(tr, &self.parent, &current).await?;
        let mut supplied = receipts.iter().map(receipt_fingerprint).collect::<Vec<_>>();
        let mut expected = actual
            .members
            .iter()
            .map(|m| m.receipt.clone())
            .collect::<Vec<_>>();
        supplied.sort();
        expected.sort();
        if supplied != expected {
            return Err(CatalogError::Invalid("回收S4收据不是原完整成员集合"));
        }
        let binding = BatchBinding {
            version: 1,
            batch: batch_id.to_string(),
            plan: actual.fingerprint()?,
            receipts: digest(b"tape-rs/reclaim-index-batch-receipts/1", &expected)?,
            manifest,
        };
        let route = BatchRoute {
            version: 2,
            parent: self.parent.intent().clone(),
            group: current.header.group.clone(),
            physical_attempt: batch_id.to_string(),
            batch: batch_id.to_string(),
            manifest,
            plan: actual.fingerprint()?,
        };
        let route_raw = native_bytes(&route)?;
        let rows = [
            (
                key(&self.parent, b'b', Some(current.id()), None),
                native_bytes(&binding)?,
            ),
            (
                batch_route_key(self.parent.namespace(), batch_id),
                route_raw.clone(),
            ),
            (
                group_layout::control_key(&self.parent, b'b', Some(batch_id)),
                route_raw,
            ),
        ];
        let mut present = [false; 3];
        for (at, (key, expected)) in rows.iter().enumerate() {
            if let Some(raw) = tr.get(key, false).await? {
                if raw.as_ref() != expected.as_slice() {
                    return Err(CatalogError::Corrupt(
                        "回收S4原三binding与固定batch/plan不符",
                    ));
                }
                present[at] = true;
            }
        }
        Ok(BatchBindingRows { rows, present })
    }
    /// Only a newly accepted Header3 may create all three rows together. No
    /// conflict or missing-part repair is allowed to leave a partial binding.
    pub(in crate::managed_catalog) async fn bind_active_batch_at(
        &self,
        tr: &Transaction,
        batch_id: Uuid,
        receipts: &[CommitReceipt],
        manifest: [u8; 32],
    ) -> Result<()> {
        let checked = self
            .checked_batch_binding_rows_at(tr, batch_id, receipts, manifest)
            .await?;
        match checked.present {
            [true, true, true] => Ok(()),
            [false, false, false] => {
                for (key, value) in checked.rows {
                    tr.set(&key, &value);
                }
                Ok(())
            }
            _ => Err(CatalogError::Corrupt(
                "原回收三binding不完整，禁止stage补造",
            )),
        }
    }
    /// Existing header/page/seal transactions may validate, never recreate,
    /// the original mandatory routing before returning any business outcome.
    pub(in crate::managed_catalog) async fn require_active_batch_binding_at(
        &self,
        tr: &Transaction,
        batch_id: Uuid,
        receipts: &[CommitReceipt],
        manifest: [u8; 32],
    ) -> Result<()> {
        let checked = self
            .checked_batch_binding_rows_at(tr, batch_id, receipts, manifest)
            .await?;
        if checked.present != [true, true, true] {
            return Err(CatalogError::Corrupt("原回收Header3缺少完整三binding"));
        }
        Ok(())
    }
}

impl ManagedCatalog {
    /// One fresh read version for the original group before/after each CDB.
    /// The ordinary public writer retains its v1-only execution lane.
    pub(crate) async fn check_reclaim_write_group(
        &self,
        scope: &ReclaimGroupExecution,
        intents: &[ActiveFileIntent],
    ) -> Result<Vec<bool>> {
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(
            &tr,
            scope.parent.serial(),
            scope.parent.route,
            scope.execution_epoch,
        )
        .await?;
        scope.require_at(&tr, intents).await?;
        let mut observed = Vec::with_capacity(intents.len());
        for original in intents {
            let current = reservation::active_file_attempt_at(&tr, &original.request()).await?;
            if current.as_ref().is_some_and(|current| current != original) {
                return Err(CatalogError::Invalid("回收组原物理尝试已经改变"));
            }
            observed.push(current.is_some());
        }
        Ok(observed)
    }
}

fn native_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(value).map_err(cache)?;
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(CatalogError::Invalid("回收组原生元数据单行超过预算"));
    }
    Ok(raw)
}

fn native_decode<T: for<'a> Deserialize<'a> + Serialize>(raw: &[u8]) -> Result<T> {
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(CatalogError::Corrupt("回收组原生元数据单行超过预算"));
    }
    let value = serde_json::from_slice(raw).map_err(cache)?;
    if native_bytes(&value)? != raw {
        return Err(CatalogError::Corrupt("回收组原生元数据编码不规范"));
    }
    Ok(value)
}

fn payload_digest(member: &NativeMember) -> Result<[u8; 32]> {
    digest(b"tape-rs/reclaim-index-native-member/1", member)
}

impl ReclaimIndexIntent {
    pub(crate) fn require_full_dp_checkpoint_policy(&self) -> Result<()> {
        if self.intent.version != if self.intent.root.is_some() { 3 } else { 2 }
            || self.intent.checkpoint_policy != GroupCheckpointPolicy::FullDp
        {
            return Err(CatalogError::Invalid(
                "旧私有回收Intent1不支持恢复或迁移，须新建Intent2 FullDp操作",
            ));
        }
        Ok(())
    }
    pub(crate) fn require_full_dp_construct(
        &self,
        construct: crate::ltfs::volume::CommittedIndexConstruct,
    ) -> Result<()> {
        self.require_full_dp_checkpoint_policy()?;
        let previous =
            active_volume::decode_index(self.intent.destination.volume, &self.intent.previous)?;
        if construct.incremental
            || construct.volume_uuid != self.intent.destination.volume
            || construct.location.partition != previous.location.partition
        {
            return Err(CatalogError::Invalid(
                "回收Intent2要求原数据分区Full DP，禁止Delta或换卷",
            ));
        }
        Ok(())
    }
    pub(super) fn attempt_base(&self)->Result<(IndexSourceIdentity,u64)> {
        Ok((IndexSourceIdentity::from_bytes(&self.intent.previous_source).map_err(cache)?,self.intent.previous_dp_eod))
    }
    pub(crate) fn archive_paths(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|member| member.archive.as_str())
    }

    pub(crate) fn require_archive_base(&self, index: &LtfsIndex) -> Result<()> {
        if let Some(root) = &self.intent.root {
            if index.highest_file_uid != root.previous_highest_uid
                || index.find_directory(&root.archive).is_some()
                || index.find_file(&root.archive).is_some()
            {
                return Err(CatalogError::Invalid("回收组原目标归档根路径或UID基准不同"));
            }
        }
        Ok(())
    }

    pub(crate) fn require_target(&self, target: &CapturedIndexSource) -> Result<()> {
        let IndexSourceScope::Current {
            dp_eod,
            history_truncated: false,
            ..
        } = target.scope()
        else {
            return Err(CatalogError::Invalid("回收目标必须有完整当前物理索引证明"));
        };
        if target.identity().map_err(cache)?.to_bytes() != self.intent.previous_source
            || dp_eod != self.intent.previous_dp_eod
            || target.construct_end() != dp_eod
        {
            return Err(CatalogError::Invalid("回收目标证明改变原索引或干净EOD边界"));
        }
        Ok(())
    }
    pub(crate) fn fingerprint(&self) -> Result<[u8; 32]> {
        digest(b"tape-rs/reclaim-index-intent/1", &self.intent)
    }

    fn validate(&self, parent: &ReclaimReservation, group: &ReclaimCopyGroup) -> Result<()> {
        self.require_full_dp_checkpoint_policy()?;
        let i = &self.intent;
        let source = IndexSourceIdentity::from_bytes(&i.source).map_err(cache)?;
        let previous_source = IndexSourceIdentity::from_bytes(&i.previous_source).map_err(cache)?;
        let mut source_head = Vec::new();
        active_volume::encode_index(&mut source_head, source.head().into());
        let mut previous_head = Vec::new();
        active_volume::encode_index(&mut previous_head, previous_source.head().into());
        let original_source: Vec<u8> =
            serde_json::from_value(parent.intent()["index"].clone()).map_err(cache)?;
        if i.root.is_some() != parent.groups_archive_root()
            || i.parent != parent.fingerprint()
            || i.group != group.header.group
            || i.batch != group.header.batch
            || i.members != group.header.members
            || i.destination != group.header.destination
            || i.scan != group.header.scan
            || i.previous != group.header.previous
            || source_head != original_source
            || previous_head != i.previous
            || i.previous_dp_eod == 0
            || i.additional_files != u64::from(group.header.count)
            || i.payloads.len() != group.members.len()
            || self.members.len() != group.members.len()
        {
            return Err(CatalogError::Corrupt("回收组索引意图与原固定组不同"));
        }
        if let Some(root) = &i.root {
            if root.archive != archive_path(parent)?
                || root
                    .previous_highest_uid
                    .checked_add(i.additional_files + 1)
                    .is_none_or(|minimum| root.uid < minimum)
                || root.uid.checked_add(i.additional_files).is_none()
            {
                return Err(CatalogError::Corrupt("回收组原归档根路径或UID不符"));
            }
            root.expected()?;
        }
        for ((native, selected), expected) in
            self.members.iter().zip(&group.members).zip(&i.payloads)
        {
            let IndexEntry::File { node, .. } = &native.source else {
                return Err(CatalogError::Corrupt("回收组原生正文不是文件"));
            };
            if native.version != 1
                || native.ordinal != selected.ordinal
                || native.selected != selected.digest()?
                || selected.child.as_ref() != Some(&native.child)
                || node.meta.file_uid != selected.body_uid
                || node.length != selected.file.size()
                || node.symlink.is_some()
                || node.open_for_write == Some(true)
                || node.delta_fields.is_some()
                || source::source_entry_fingerprint(native.source.clone())? != selected.node
                || ContentDigest::from_ltfs_xattrs(&node.xattrs)?
                    != ContentDigest::Sha256(selected.known_sha256)
                || native.archive
                    != format!(
                        "_reclaim_{}/data-{}",
                        parent.intent()["operation"]
                            .as_str()
                            .ok_or(CatalogError::Incomplete)?,
                        selected.body_uid
                    )
                || payload_digest(native)? != *expected
            {
                return Err(CatalogError::Corrupt("回收组原生成员或原子请求改变"));
            }
            native_bytes(native)?;
        }
        native_bytes(i)?;
        Ok(())
    }
}

fn decode_index_intent(raw: &[u8]) -> Result<Intent> {
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(CatalogError::Corrupt("回收组索引意图单行超过预算"));
    }
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(cache)?;
    if !matches!(value["version"].as_u64(), Some(2 | 3)) {
        return Err(CatalogError::Invalid(
            "旧私有回收Intent1或未知版本禁止自动恢复、迁移",
        ));
    }
    let intent: Intent = native_decode(raw)?;
    if (intent.version == 3) != intent.root.is_some() {
        return Err(CatalogError::Corrupt("回收索引协议与归档根意图不同"));
    }
    Ok(intent)
}

fn archive_path(parent: &ReclaimReservation) -> Result<String> {
    Ok(format!(
        "_reclaim_{}",
        parent.intent()["operation"]
            .as_str()
            .ok_or(CatalogError::Incomplete)?
    ))
}

pub(super) async fn require_intent_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<ReclaimIndexIntent> {
    let raw = tr
        .get(&key(parent, b'i', Some(group.id()), None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let intent: Intent = decode_index_intent(&raw)?;
    let mut members = Vec::with_capacity(group.members.len());
    for ordinal in 0..group.header.count {
        let raw = tr
            .get(&key(parent, b'n', Some(group.id()), Some(ordinal)), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        members.push(native_decode(&raw)?);
    }
    let result = ReclaimIndexIntent { intent, members };
    result.validate(parent, group)?;
    Ok(result)
}

impl ManagedCatalog {
    /// Freeze reconstructible native metadata before the original group is
    /// admitted. No body offsets or success receipts are invented here.
    pub(crate) async fn prepare_reclaim_index_intent(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        source: &CapturedIndexSource,
        target: &CapturedIndexSource,
    ) -> Result<ReclaimIndexIntent> {
        let IndexSourceScope::Current {
            dp_eod,
            history_truncated: false,
            ..
        } = target.scope()
        else {
            return Err(CatalogError::Invalid("回收组目标需要完整当前索引证明"));
        };
        let mut previous = Vec::new();
        active_volume::encode_index(&mut previous, target.construct().into());
        let mut source_head = Vec::new();
        active_volume::encode_index(&mut source_head, source.construct().into());
        let expected_source: Vec<u8> =
            serde_json::from_value(parent.intent()["index"].clone()).map_err(cache)?;
        if source_head != expected_source {
            return Err(CatalogError::Invalid("回收组原生元数据来源不是原卷头"));
        }
        if !matches!(
            source.scope(),
            IndexSourceScope::Current {
                history_truncated: false,
                ..
            }
        ) {
            return Err(CatalogError::Invalid("回收组来源需要完整当前索引证明"));
        }
        let archive = archive_path(parent)?;
        let target_root = target.entry_by_path(&archive).map_err(cache)?;
        let source_root = if parent.groups_archive_root() {
            if target_root.is_some() {
                return Err(CatalogError::Invalid("回收组原目标归档路径已有节点"));
            }
            Some(
                source
                    .entry_by_path("")
                    .map_err(cache)?
                    .ok_or(CatalogError::Incomplete)?,
            )
        } else if !matches!(target_root, Some(IndexEntry::Directory { .. })) {
            return Err(CatalogError::Invalid(
                "回收组归档根目录尚未完成原单节点路径",
            ));
        } else {
            None
        };
        let previous_source = target.identity().map_err(cache)?.to_bytes();
        let source_identity = source.identity().map_err(cache)?.to_bytes();
        self.db
            .run(|tr, _| {
                let previous = previous.clone();
                let previous_source = previous_source.clone();
                let source_identity = source_identity.clone();
                let archive = &archive;
                let source_root = &source_root;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || group.header.version != 3
                                || !group.header.selected
                                || !matches!(group.header.phase, Phase::Selecting | Phase::Prepared)
                                || previous != group.header.previous
                            {
                                return Err(CatalogError::Invalid(
                                    "原生元数据必须封存于原v2组准入前",
                                ));
                            }
                            require_previous_at(&tr, parent, owner, &group).await?;
                            let mut members = Vec::with_capacity(group.members.len());
                            let mut donors = Vec::with_capacity(group.members.len());
                            for selected in &group.members {
                                let child = original_child_at(&tr, parent, owner, selected).await?;
                                if source_root.is_some() {
                                    donors.push(child.ltfs_path()?);
                                }
                                let source = source
                                    .entry_by_uid(selected.body_uid)
                                    .map_err(cache)?
                                    .ok_or(CatalogError::Incomplete)?;
                                members.push(NativeMember {
                                    version: 1,
                                    ordinal: selected.ordinal,
                                    selected: selected.digest()?,
                                    child: selected
                                        .child
                                        .clone()
                                        .ok_or(CatalogError::Incomplete)?,
                                    archive: format!(
                                        "_reclaim_{}/data-{}",
                                        parent.intent()["operation"]
                                            .as_str()
                                            .ok_or(CatalogError::Incomplete)?,
                                        selected.body_uid
                                    ),
                                    source,
                                });
                            }
                            let root = if let Some(entry) = source_root {
                                source::require_metadata_idle_at(&tr, parent).await?;
                                let original =
                                    source::sealed_entry_node_at(&tr, parent, entry).await?;
                                source::group_only::require_group_root_at(
                                    &tr,
                                    parent,
                                    group.id(),
                                    &original,
                                )
                                .await?;
                                source::require_group_absent_at(&tr, parent, original.uid())
                                    .await?;
                                Some(IntentRoot::prepare(
                                    archive,
                                    entry.clone(),
                                    target,
                                    &donors,
                                )?)
                            } else {
                                None
                            };
                            let intent = Intent {
                                version: if root.is_some() { 3 } else { 2 },
                                checkpoint_policy: GroupCheckpointPolicy::FullDp,
                                parent: parent.fingerprint(),
                                group: group.header.group.clone(),
                                batch: group.header.batch.clone(),
                                members: group.header.members,
                                source: source_identity,
                                destination: group.header.destination.clone(),
                                scan: group.header.scan.clone(),
                                previous,
                                previous_source,
                                previous_dp_eod: dp_eod,
                                additional_files: u64::from(group.header.count),
                                payloads: members
                                    .iter()
                                    .map(payload_digest)
                                    .collect::<Result<_>>()?,
                                root,
                            };
                            let result = ReclaimIndexIntent { intent, members };
                            result.validate(parent, &group)?;
                            if let Some(_) =
                                tr.get(&key(parent, b'i', Some(id), None), false).await?
                            {
                                let saved = require_intent_at(&tr, parent, &group).await?;
                                if saved.intent != result.intent || saved.members != result.members
                                {
                                    return Err(CatalogError::Invalid(
                                        "原组原生元数据意图不能更换",
                                    ));
                                }
                                return Ok(saved);
                            }
                            if remaining_slots_at(&tr, parent, group.header.destination.volume)
                                .await?
                                < u64::from(group.header.count)
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            for member in &result.members {
                                tr.set(
                                    &key(parent, b'n', Some(id), Some(member.ordinal)),
                                    &native_bytes(member)?,
                                );
                            }
                            tr.set(
                                &key(parent, b'i', Some(id), None),
                                &native_bytes(&result.intent)?,
                            );
                            Ok(result)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

#[cfg(test)]
#[path="reclaim_full_dp_policy_tests.rs"]
mod full_dp_policy_tests;

/// Exact original physical group history, without calling qualification back.
pub(in crate::managed_catalog::control::reclaim) async fn group_only_history_link_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group_id: Uuid,
    index: ActiveIndexIdentity,
) -> Result<(ActiveIndexIdentity, u64, Option<ReclaimSourceNode>)> {
    let group = evidence_group_at(tr, parent, group_id).await?;
    let plan = require_actual_at(tr, parent, &group).await?;
    let route = tr
        .get(
            &batch_route_key(parent.namespace(), group.batch_id()),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let route: BatchRoute = native_decode(&route)?;
    if route.parent != *parent.intent()
        || route.group != group_id.to_string()
        || route.batch != group.batch_id().to_string()
    {
        return Err(CatalogError::Invalid("group-only历史不是原唯一组"));
    }
    require_checkpoint_at(tr,parent.namespace(),group.batch_id(),route.manifest,index,Some(plan.target_file_count())).await?;
    let root = if let Some(root) = &plan.intent.intent.root {
        Some(source::sealed_entry_node_at(tr, parent, &root.source).await?)
    } else {
        None
    };
    Ok((plan.previous()?, plan.target_file_count(), root))
}

pub(in crate::managed_catalog::control::reclaim) async fn require_group_root_coverage_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    reference: &GroupRootReference,
) -> Result<ReclaimSourceNode> {
    let group = evidence_group_at(tr, parent, parse_uuid(&reference.group)?).await?;
    if group.header.phase != Phase::Closed {
        return Err(CatalogError::Invalid("归档根覆盖缺原组关闭证明"));
    }
    let plan = require_actual_at(tr, parent, &group).await?;
    let observed = native_decode(
        &tr.get(&key(parent, b'p', Some(group.id()), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let (entry, expected) = plan
        .root_reference(&group, &observed)?
        .ok_or(CatalogError::Incomplete)?;
    if expected != *reference {
        return Err(CatalogError::Corrupt("归档根覆盖改变原计划、回执或实际UID"));
    }
    source::sealed_entry_node_at(tr, parent, entry).await
}
