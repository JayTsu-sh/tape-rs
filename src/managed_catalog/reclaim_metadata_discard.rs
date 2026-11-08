//! Read-only recognition of deterministic Discard; no recovery I/O authority.
use super::*;
use crate::ltfs::index::Xattr;
use crate::ltfs::volume::{XATTR_ABANDONED, XATTR_EMPTY_RECOVERY, empty_recovery_marker};
use metadata_plan::TargetTree;

/// An exact recovered tree, not a normal metadata commit or permission to write.
/// This fact still needs a fresh owner/attempt proof and atomic recovery receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataDiscardObservation {
    plan: [u8; 32],
    current: ActiveIndexIdentity,
    physical_files: u64,
    first: u64,
    end: u64,
}
impl ReclaimMetadataDiscardObservation {
    pub fn plan_fingerprint(&self) -> [u8; 32] {
        self.plan
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn physical_files(&self) -> u64 {
        self.physical_files
    }
    pub fn abandoned_range(&self) -> std::ops::RangeInclusive<u64> {
        self.first..=self.end
    }
}

fn single<'a>(attrs: &'a [Xattr], key: &str) -> Result<Option<&'a str>> {
    let mut matching = attrs.iter().filter(|attr| attr.key == key);
    let Some(attr) = matching.next() else {
        return Ok(None);
    };
    if attr.base64 || matching.next().is_some() {
        return Err(CatalogError::Invalid("元数据修尾标记重复或编码非法"));
    }
    Ok(Some(&attr.value))
}
fn recovery_attr(attr: &&Xattr) -> bool {
    !matches!(attr.key.as_str(), XATTR_ABANDONED | XATTR_EMPTY_RECOVERY)
}

impl ReclaimMetadataPlan {
    /// Shared original-media validation before repair and when recognizing it.
    /// It never grants I/O authority, mutates the source, or materializes a tree.
    pub(crate) fn require_discard_source(
        &self,
        original: &CapturedIndexSource,
    ) -> Result<Vec<Xattr>> {
        self.encode()?;
        let previous = self.previous_source()?;
        let old = previous.head();
        if original.scope() != IndexSourceScope::Recorded
            || original.construct_end() != self.previous_dp_eod()
            || original.identity().map_err(cache)? != previous
            || !original.header().unknown_elements.is_empty()
            || original.header().highest_file_uid.checked_add(1) != Some(self.uid())
        {
            return Err(CatalogError::Invalid(
                "修尾来源不是原 Recorded 构造、闭合边界或完整链",
            ));
        }
        let donor = original
            .entry_by_uid(self.0.donor.uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if Object::capture(donor)? != self.0.donor {
            return Err(CatalogError::Invalid("元数据修尾改变原正文 donor"));
        }
        let mut root = None;
        let mut tree = TargetTree::new(self.volume());
        let mut files = 0u64;
        original
            .for_each_entry(|entry| {
                match &entry {
                    IndexEntry::Directory { path, node } if path.is_empty() => {
                        root = Some(node.xattrs.clone())
                    }
                    IndexEntry::File { node, .. } => {
                        // Variable-length records cannot be bounded by dividing the
                        // byte count by blocksize. The retained original closing FM
                        // separates every earlier start from the discarded tail.
                        if node.extents.iter().any(|extent| {
                            extent.partition != old.location.partition
                                || extent.byte_count > 0
                                    && extent.start_block >= self.previous_dp_eod()
                        }) {
                            return Err(callback(CatalogError::Invalid(
                                "原索引引用 IP 或修尾放弃区域",
                            )));
                        }
                        files = files
                            .checked_add(1)
                            .ok_or_else(|| callback(CatalogError::Invalid("修尾文件计数溢出")))?;
                    }
                    _ => {}
                }
                tree.add(&Object::capture(entry).map_err(callback)?)
                    .map_err(callback)
            })
            .map_err(cache)?;
        if files != self.0.physical_files
            || tree.objects != self.0.target_objects
            || tree.finish() != self.0.target_tree
        {
            return Err(CatalogError::Invalid("元数据修尾历史来源不是原固定完整树"));
        }
        let attrs = root.ok_or(CatalogError::Incomplete)?;
        single(&attrs, XATTR_ABANDONED)?;
        single(&attrs, XATTR_EMPTY_RECOVERY)?;
        Ok(attrs)
    }

    /// Compare actual Current/Recorded reads without issuing any device command.
    /// Only the two precisely validated system attributes may differ from the
    /// original complete tree; source blocks, file counts and UIDs stay exact.
    pub fn observe_discarded(
        &self,
        target: &CapturedIndexSource,
        original: &CapturedIndexSource,
    ) -> Result<ReclaimMetadataDiscardObservation> {
        let original_attrs = self.require_discard_source(original)?;
        current_proof(target.scope(), target.header())?;
        let previous = self.previous_source()?;
        let old = previous.head();
        let head = target.header();
        let current = target.construct();
        let before = original.header();
        let expected_full = if old.incremental {
            before.previous_location
        } else {
            Some(old.location)
        };
        let expected_delta = old.incremental.then_some(old.location);
        if current.volume_uuid != self.volume()
            || current.location.partition != old.location.partition
            || current.incremental
            || head.incremental
            || old.generation.checked_add(1) != Some(current.generation)
            || head.generation != current.generation
            || head.volume_uuid != current.volume_uuid
            || head.self_location != current.location
            || head.previous_location != expected_full
            || head.previous_incremental_location != expected_delta
            || head.highest_file_uid != before.highest_file_uid
            || head.creator != before.creator
            || head.comment != before.comment
            || head.allow_policy_update != before.allow_policy_update
            || head.volume_lock_state != before.volume_lock_state
            || head.version
                != if old.incremental {
                    "2.5.0"
                } else {
                    &before.version
                }
        {
            return Err(CatalogError::Invalid(
                "元数据修尾结果改变原卷头、UID 或完整历史来源",
            ));
        }
        let end = current
            .location
            .start_block
            .checked_sub(2)
            .filter(|end| self.previous_dp_eod() <= *end)
            .ok_or(CatalogError::Invalid(
                "元数据修尾范围不在原 EOD 与新索引之间",
            ))?;
        let prefix = single(&original_attrs, XATTR_ABANDONED)?
            .map(|old| format!("{old};"))
            .unwrap_or_default();
        single(&original_attrs, XATTR_EMPTY_RECOVERY)?;
        let attrs = &head.root.xattrs;
        if single(attrs, XATTR_ABANDONED)?
            != Some(format!("{prefix}b:{}-{end}", self.previous_dp_eod()).as_str())
            || single(attrs, XATTR_EMPTY_RECOVERY)? != Some(empty_recovery_marker(old).as_str())
            || !attrs
                .iter()
                .filter(recovery_attr)
                .eq(original_attrs.iter().filter(recovery_attr))
        {
            return Err(CatalogError::Invalid(
                "元数据修尾双标记、原前缀或其他根属性不同",
            ));
        }
        let mut tree = TargetTree::new(self.volume());
        let mut files = 0u64;
        target
            .for_each_entry(|mut entry| {
                match &mut entry {
                    IndexEntry::Directory { path, node } if path.is_empty() => {
                        node.xattrs.clone_from(&original_attrs)
                    }
                    IndexEntry::File { .. } => {
                        files = files
                            .checked_add(1)
                            .ok_or_else(|| callback(CatalogError::Invalid("修尾文件计数溢出")))?
                    }
                    _ => {}
                }
                tree.add(&Object::capture(entry).map_err(callback)?)
                    .map_err(callback)
            })
            .map_err(cache)?;
        if files != self.0.physical_files
            || tree.objects != self.0.target_objects
            || tree.finish() != self.0.target_tree
        {
            return Err(CatalogError::Invalid("元数据修尾改变原完整对象树或文件数"));
        }
        Ok(ReclaimMetadataDiscardObservation {
            plan: self.fingerprint()?,
            current: current.into(),
            physical_files: files,
            first: self.previous_dp_eod(),
            end,
        })
    }
}

/// Fresh task-bound T1/T2 precondition. This authorizes only persisting the
/// explicit Discard goal; execution must re-mount under its new phase guard.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimMetadataTailEvidence =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataTailEvidence {
    task: ReclaimMetadataWrite,
    tail_eod: u64,
}
impl ReclaimMetadataTailEvidence {
    pub fn tail_eod(&self) -> u64 {
        self.tail_eod
    }
    pub(crate) fn from_report(
        task: &ReclaimMetadataWrite,
        owner: u64,
        original: &CapturedIndexSource,
        report: &crate::ltfs::recovery::RecoveryReport,
        current: crate::ltfs::volume::CommittedIndexConstruct,
    ) -> Result<Self> {
        use crate::ltfs::recovery::TailKind;
        let plan = task.plan();
        if task.owner() != owner
            || !(task.writing() || task.discarding())
            || task
                .observation()?
                .is_some_and(|value| value.kind() == ReclaimMetadataObservationKind::Committed)
        {
            return Err(CatalogError::Invalid("修尾前提不属于当前未决原尝试"));
        }
        plan.require_discard_source(original)?;
        let last = report
            .dp
            .last_index
            .as_ref()
            .ok_or(CatalogError::Incomplete)?;
        if current != plan.previous_source()?.head()
            || last.index.volume_uuid != current.volume_uuid
            || last.index.generation != current.generation
            || last.index.self_location != current.location
            || last.construct_sha256 != current.sha256
            || last.index.incremental != current.incremental
            || last.end_block.checked_add(1) != Some(plan.previous_dp_eod())
            || report.dp.eod <= plan.previous_dp_eod()
            || report.dp.eod.checked_add(3).is_none()
            || !matches!(
                report.dp.tail,
                TailKind::UnindexedData | TailKind::TruncatedIndex
            )
            || !report.chain_ok
            || !report.history_checked
            || report.chain_truncated_by_budget
            || report.ip_ahead()
            || report.restricted_reason().is_some()
            || original
                .header()
                .volume_lock_state
                .as_deref()
                .is_some_and(|lock| lock != "unlocked")
        {
            return Err(CatalogError::Invalid(
                "修尾前提要求原实际卷头/闭合边界及完整 T1/T2 恢复视图",
            ));
        }
        Ok(Self {
            task: task.clone(),
            tail_eod: report.dp.eod,
        })
    }
    pub(in crate::managed_catalog) fn require(
        &self,
        task: &ReclaimMetadataWrite,
        owner: u64,
    ) -> Result<()> {
        if self.task != *task
            || task.owner() != owner
            || self.tail_eod <= task.plan().previous_dp_eod()
        {
            return Err(CatalogError::Invalid(
                "修尾前提改变原 owner、尝试或持久快照",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "reclaim_metadata_discard_tests.rs"]
mod tests;
