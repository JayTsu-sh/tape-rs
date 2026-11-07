//! Actual unchanged/discarded evidence for the original per-volume rename.
use super::*;
use crate::ltfs::volume::{XATTR_ABANDONED, XATTR_EMPTY_RECOVERY, empty_recovery_marker};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRenameAbsenceKind {
    Unchanged,
    Discarded,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameAbsenceEvidence {
    plan: NodeRenameMediaPlan,
    current: ActiveIndexIdentity,
    file_count: u64,
    kind: NodeRenameAbsenceKind,
}
impl NodeRenameAbsenceEvidence {
    pub fn plan(&self) -> &NodeRenameMediaPlan {
        &self.plan
    }
    pub fn current_index(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn kind(&self) -> NodeRenameAbsenceKind {
        self.kind
    }
}
fn single<'a>(attrs: &'a [Xattr], key: &str) -> Result<Option<&'a str>> {
    let mut found = attrs.iter().filter(|a| a.key == key);
    let Some(attr) = found.next() else {
        return Ok(None);
    };
    if attr.base64 || found.next().is_some() {
        return Err(bad());
    }
    Ok(Some(&attr.value))
}
impl NodeRenameMediaPlan {
    pub(crate) fn require_absence_source(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
        budget: u64,
    ) -> Result<()> {
        if current != self.recipe.source_index() {
            return Err(bad());
        }
        self.require_source(index, budget)?;
        let f = &self.recipe.record.facts;
        if volume_plan::RecoveryAttribute::capture(&index.root.xattrs, XATTR_ABANDONED)?
            != f.recovery_abandoned
            || volume_plan::RecoveryAttribute::capture(&index.root.xattrs, XATTR_EMPTY_RECOVERY)?
                != f.recovery_marker
            || single(&index.root.xattrs, RENAME_MARKER)? == Some(self.marker().as_str())
        {
            return Err(bad());
        }
        Ok(())
    }
    pub(crate) fn absence_evidence(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
        budget: u64,
    ) -> Result<NodeRenameAbsenceEvidence> {
        let before = self.recipe.source_index();
        let f = &self.recipe.record.facts;
        require_marker(index, None)?;
        if single(&index.root.xattrs, RENAME_MARKER)? == Some(self.marker().as_str())
            || index.volume_uuid != current.volume_uuid
            || index.generation != current.generation
            || index.self_location != current.location
            || index.highest_file_uid != f.highest_before
        {
            return Err(bad());
        }
        let kind = if current == before {
            self.require_absence_source(index, current, budget)?;
            NodeRenameAbsenceKind::Unchanged
        } else {
            if current.volume_uuid != before.volume_uuid
                || before.generation.checked_add(1) != Some(current.generation)
                || current.location.partition != before.location.partition
                || current.location.start_block <= before.location.start_block
                || current.incremental
                || index.incremental
                || (if before.incremental {
                    index.previous_incremental_location
                } else {
                    index.previous_location
                }) != Some(before.location)
                || single(&index.root.xattrs, XATTR_EMPTY_RECOVERY)?
                    != Some(
                        empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
                            volume_uuid: before.volume_uuid,
                            generation: before.generation,
                            location: before.location,
                            sha256: before.sha256,
                            incremental: before.incremental,
                        })
                        .as_str(),
                    )
            {
                return Err(bad());
            }
            let abandoned = single(&index.root.xattrs, XATTR_ABANDONED)?.ok_or_else(bad)?;
            let prefix = f
                .recovery_abandoned
                .as_ref()
                .map(|a| format!("{};", a.value))
                .unwrap_or_default();
            abandoned
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_prefix("b:"))
                .and_then(|s| s.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                .filter(|(a, b)| {
                    *a > before.location.start_block
                        && a <= b
                        && *b < current.location.start_block
                        && abandoned == format!("{prefix}b:{a}-{b}")
                })
                .ok_or_else(bad)?;
            // Reinsert only the two validated recovery values at their original
            // positions. All other root and child bytes remain hash-protected.
            physical_source::attributes(
                &index.root.meta,
                &index.root.xattrs,
                None,
                index.highest_file_uid,
            )?;
            let mut attrs = index
                .root
                .xattrs
                .iter()
                .filter(|a| !matches!(a.key.as_str(), XATTR_ABANDONED | XATTR_EMPTY_RECOVERY))
                .cloned()
                .collect::<Vec<_>>();
            let mut original = [
                (&f.recovery_abandoned, XATTR_ABANDONED),
                (&f.recovery_marker, XATTR_EMPTY_RECOVERY),
            ]
            .into_iter()
            .filter_map(|(v, k)| v.as_ref().map(|v| (v, k)))
            .collect::<Vec<_>>();
            original.sort_by_key(|(v, _)| v.position);
            for (attr, key) in original {
                let position = attr.position as usize;
                if position > attrs.len() {
                    return Err(bad());
                }
                attrs.insert(
                    position,
                    Xattr {
                        key: key.into(),
                        value: attr.value.clone(),
                        base64: false,
                    },
                );
            }
            let (digest, count, _) = self.summary_with_root(index, false, budget, Some(&attrs))?;
            if digest != self.recipe.before_sha256() || count != f.before_files {
                return Err(bad());
            }
            NodeRenameAbsenceKind::Discarded
        };
        Ok(NodeRenameAbsenceEvidence {
            plan: self.clone(),
            current,
            file_count: f.before_files,
            kind,
        })
    }
}

impl NodeRenameMediaPlan {
    pub(crate) fn require_discard_source(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
        budget: u64,
    ) -> Result<()> {
        self.require_absence_source(index, current, budget)?;
        // Reserve the largest possible new range before the first tail write.
        let mut attrs = index.root.xattrs.clone();
        let old = single(&attrs, XATTR_ABANDONED)?.map(str::to_owned);
        let range = format!("b:{}-{}", u64::MAX, u64::MAX);
        let abandoned = old.map(|s| format!("{s};{range}")).unwrap_or(range);
        let marker = empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
            incremental: current.incremental,
        });
        for (key, value) in [(XATTR_ABANDONED, abandoned), (XATTR_EMPTY_RECOVERY, marker)] {
            if let Some(attr) = attrs.iter_mut().find(|a| a.key == key) {
                attr.value = value;
            } else {
                attrs.push(Xattr {
                    key: key.into(),
                    value,
                    base64: false,
                });
            }
        }
        volume_plan::RecoveryAttribute::capture(&attrs, XATTR_ABANDONED)?;
        volume_plan::RecoveryAttribute::capture(&attrs, XATTR_EMPTY_RECOVERY)?;
        physical_source::attributes(&index.root.meta, &attrs, None, index.highest_file_uid)?;
        encode(&Ref::Directory {
            meta: &index.root.meta,
            xattrs: &attrs,
        })?;
        Ok(())
    }
}
