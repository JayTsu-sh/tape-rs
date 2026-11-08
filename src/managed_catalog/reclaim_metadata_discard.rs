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
    /// Compare actual Current/Recorded reads without issuing any device command.
    /// Only the two precisely validated system attributes may differ from the
    /// original complete tree; source blocks, file counts and UIDs stay exact.
    pub fn observe_discarded(
        &self,
        target: &CapturedIndexSource,
        original: &CapturedIndexSource,
    ) -> Result<ReclaimMetadataDiscardObservation> {
        self.encode()?;
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
        if original.scope() != IndexSourceScope::Recorded
            || original.construct_end() != self.previous_dp_eod()
            || original.identity().map_err(cache)? != previous
            || !before.unknown_elements.is_empty()
            || before.highest_file_uid.checked_add(1) != Some(self.uid())
            || current.volume_uuid != self.volume()
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
        let original_attrs = root.ok_or(CatalogError::Incomplete)?;
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

#[cfg(test)]
#[path = "reclaim_metadata_discard_tests.rs"]
mod tests;
