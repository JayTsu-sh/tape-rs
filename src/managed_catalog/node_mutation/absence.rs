//! A node result and an unchanged/discarded attempt are distinct media facts.
use super::*;
use crate::ltfs::volume::{XATTR_ABANDONED, XATTR_EMPTY_RECOVERY, empty_recovery_marker};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeMutationAbsenceKind {
    Unchanged,
    Discarded,
}

/// Minted only from a clean committed view under the current device PR. This
/// does not close a persistent attempt or authorize replay of its old plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationAbsenceEvidence {
    context: NodeMutationContext,
    fingerprint: [u8; 32],
    current: ActiveIndexIdentity,
    file_count: u64,
    kind: NodeMutationAbsenceKind,
}
impl NodeMutationAbsenceEvidence {
    pub fn context(&self) -> &NodeMutationContext {
        &self.context
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn kind(&self) -> NodeMutationAbsenceKind {
        self.kind
    }
}

fn single<'a>(attrs: &'a [Xattr], key: &str) -> Result<Option<&'a str>> {
    let mut matching = attrs.iter().filter(|a| a.key == key);
    let Some(attr) = matching.next() else {
        return Ok(None);
    };
    if attr.base64 || matching.next().is_some() {
        return Err(invalid("节点修尾私有属性重复或编码非法"));
    }
    Ok(Some(&attr.value))
}

impl NodeMutationPlan {
    /// Before the explicit tail discard, the last valid source must still be
    /// exactly the originally admitted construct, not another completed S4.
    pub(crate) fn require_absence_source(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
    ) -> Result<u64> {
        if current != self.context.previous {
            return Err(invalid("节点修尾的最后有效索引已偏离原尝试来源"));
        }
        self.require_source_header(index)?;
        // Discard interprets/appends these private values. Reject an unexpected
        // encoding before the first physical write, not after closing the tail.
        single(&index.root.xattrs, XATTR_ABANDONED)?;
        single(&index.root.xattrs, XATTR_EMPTY_RECOVERY)?;
        let (digest, count) = tree_summary(index, None)?;
        if digest.as_slice() != self.wire.tree_before {
            return Err(invalid("节点修尾的完整来源树已改变"));
        }
        Ok(count)
    }

    pub(crate) fn absence_evidence(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
    ) -> Result<NodeMutationAbsenceEvidence> {
        let previous = self.context.previous;
        let original = &self.wire.ancestors[0].before_xattrs;
        let marker = single(&index.root.xattrs, MUTATION_MARKER)?;
        if marker == Some(self.marker().as_str())
            || marker != single(original, MUTATION_MARKER)?
            || index.volume_uuid != current.volume_uuid
            || index.generation != current.generation
            || index.self_location != current.location
            || index.highest_file_uid != self.wire.highest_uid
        {
            return Err(invalid("节点变更标记或来源已改变，不能证明未发生"));
        }
        let (kind, file_count) = if current == previous {
            (
                NodeMutationAbsenceKind::Unchanged,
                self.require_absence_source(index, current)?,
            )
        } else {
            if current.volume_uuid != previous.volume_uuid
                || previous.generation.checked_add(1) != Some(current.generation)
                || current.location.partition != 'b'
                || current.location.start_block <= previous.location.start_block
                || current.incremental
                || index.incremental
                || (if previous.incremental {
                    index.previous_incremental_location
                } else {
                    index.previous_location
                }) != Some(previous.location)
                || single(&index.root.xattrs, XATTR_EMPTY_RECOVERY)?
                    != Some(
                        empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
                            volume_uuid: previous.volume_uuid,
                            generation: previous.generation,
                            location: previous.location,
                            sha256: previous.sha256,
                            incremental: previous.incremental,
                        })
                        .as_str(),
                    )
            {
                return Err(invalid("节点未发生证明要求原代数的确定性 Discard 收尾"));
            }
            let abandoned = single(&index.root.xattrs, XATTR_ABANDONED)?
                .ok_or(invalid("节点 Discard 缺少放弃块范围"))?;
            let prefix = single(original, XATTR_ABANDONED)?
                .map(|old| format!("{old};"))
                .unwrap_or_default();
            abandoned
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_prefix("b:"))
                .and_then(|s| s.split_once('-'))
                .and_then(|(first, end)| {
                    Some((first.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
                })
                .filter(|(first, end)| {
                    *first > previous.location.start_block
                        && first <= end
                        && *end < current.location.start_block
                        && abandoned == format!("{prefix}b:{first}-{end}")
                })
                .ok_or(invalid("节点 Discard 放弃块范围与来源或新索引不符"))?;
            let other = |attrs: &[Xattr]| {
                attrs
                    .iter()
                    .filter(|attr| {
                        !matches!(attr.key.as_str(), XATTR_ABANDONED | XATTR_EMPTY_RECOVERY)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if NodeAttributes::new(index.root.meta.clone(), other(&index.root.xattrs), None)?
                .digest()
                != NodeAttributes::new(index.root.meta.clone(), other(original), None)?.digest()
            {
                return Err(invalid("节点 Discard 改变了其他卷根属性"));
            }
            // Normalize only the two checked recovery attributes. No tree clone,
            // recursive traversal, or normalization of child nodes is needed.
            let (digest, count) = tree_summary_with_root(index, None, Some(original))?;
            if digest.as_slice() != self.wire.tree_before {
                return Err(invalid("节点未发生证明的完整逻辑树已改变"));
            }
            (NodeMutationAbsenceKind::Discarded, count)
        };
        Ok(NodeMutationAbsenceEvidence {
            context: self.context.clone(),
            fingerprint: self.fingerprint(),
            current,
            file_count,
            kind,
        })
    }
}
