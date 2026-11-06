//! Media-only reconstruction and evidence. A plan is data, never admission:
//! callers still need the original durable participant, current policy and PR.
use super::physical_source::{Ref, encode, row_hash, visit_index};
use super::*;
use crate::ltfs::index::{LtfsIndex, Xattr};
use rusqlite::params;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameMediaPlan {
    recipe: NodeRenameVolumePlan,
    attempt: Uuid,
    owner: u64,
    fingerprint: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameS4Evidence {
    plan: NodeRenameMediaPlan,
    current: ActiveIndexIdentity,
}
fn bad() -> CatalogError {
    invalid("rename 原物理尝试、完整索引或 S4 证据不符")
}
fn require_marker(index: &LtfsIndex, expected: Option<&str>) -> Result<()> {
    let mut found = None;
    for attr in &index.root.xattrs {
        if attr.key != RENAME_MARKER {
            continue;
        }
        if found.is_some()
            || attr.base64
            || attr.value.len() != 66
            || !attr.value.starts_with("1:")
            || !attr.value.as_bytes()[2..]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Err(bad());
        }
        found = Some(attr.value.as_str());
    }
    if expected.is_some() && found != expected {
        return Err(bad());
    }
    Ok(())
}
impl NodeRenameMediaPlan {
    /// Construct an immutable physical identity from a retained recipe. This
    /// does not record an intent or authorize any device command.
    pub fn new(recipe: NodeRenameVolumePlan, attempt: Uuid, owner: u64) -> Result<Self> {
        if attempt.is_nil() || owner == 0 || owner > 0xFFFF_FFFF_FFFF {
            return Err(bad());
        }
        recipe
            .source_index()
            .generation
            .checked_add(1)
            .ok_or_else(bad)?;
        let fingerprint = Sha256::new()
            .chain_update(b"tape-rs/node-rename-media-attempt/1")
            .chain_update(recipe.fingerprint())
            .chain_update(attempt.as_bytes())
            .chain_update(owner.to_be_bytes())
            .finalize()
            .into();
        Ok(Self {
            recipe,
            attempt,
            owner,
            fingerprint,
        })
    }
    pub fn recipe(&self) -> &NodeRenameVolumePlan {
        &self.recipe
    }
    pub fn attempt(&self) -> Uuid {
        self.attempt
    }
    pub fn owner_epoch(&self) -> u64 {
        self.owner
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
    fn marker(&self) -> String {
        let hex: String = self
            .fingerprint
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("1:{hex}")
    }
    pub(crate) fn require_source(&self, index: &LtfsIndex, budget: u64) -> Result<()> {
        let source = self.recipe.source_index();
        if index.volume_uuid != source.volume_uuid
            || index.generation != source.generation
            || index.self_location != source.location
            || index.incremental != source.incremental
            || index.highest_file_uid != self.recipe.record.facts.highest_before
        {
            return Err(bad());
        }
        require_marker(index, None)?;
        let (digest, files, _) = self.summary(index, false, budget)?;
        if digest != self.recipe.before_sha256() || files != self.recipe.record.facts.before_files {
            return Err(bad());
        }
        Ok(())
    }
    pub(crate) fn require_projection(&self, view: &NodeRenameVolumeProjection) -> Result<()> {
        self.recipe.require_projection(view)
    }
    /// The caller must discard the old working root first and freeze its volume
    /// on any error. Published state is never passed to this function.
    pub(crate) fn stage_tree(
        &self,
        index: &mut LtfsIndex,
        view: &NodeRenameVolumeProjection,
    ) -> Result<()> {
        let name = std::mem::take(&mut index.root.name);
        index.root = volume_projection::tree_from_rows(view, name)?;
        index.highest_file_uid = self.recipe.highest_file_uid();
        index.root.xattrs.retain(|a| a.key != RENAME_MARKER);
        index.root.xattrs.push(Xattr {
            key: RENAME_MARKER.into(),
            value: self.marker(),
            base64: false,
        });
        physical_source::attributes(
            &index.root.meta,
            &index.root.xattrs,
            None,
            index.highest_file_uid,
        )?;
        encode(&Ref::Directory {
            meta: &index.root.meta,
            xattrs: &index.root.xattrs,
        })?;
        Ok(())
    }
    pub(crate) fn evidence(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
        budget: u64,
    ) -> Result<NodeRenameS4Evidence> {
        let before = self.recipe.source_index();
        if current.volume_uuid != before.volume_uuid
            || Some(current.generation) != before.generation.checked_add(1)
            || current.location.partition != before.location.partition
            || current.location.start_block <= before.location.start_block
            || current.incremental
            || index.volume_uuid != current.volume_uuid
            || index.generation != current.generation
            || index.self_location != current.location
            || index.incremental
            || index.highest_file_uid != self.recipe.highest_file_uid()
        {
            return Err(bad());
        }
        require_marker(index, Some(&self.marker()))?;
        let (sha, files, dirs) = self.summary(index, true, budget)?;
        if sha != self.recipe.after_sha256()
            || files != self.recipe.record.facts.files
            || dirs != self.recipe.record.facts.directories
        {
            return Err(bad());
        }
        Ok(NodeRenameS4Evidence {
            plan: self.clone(),
            current,
        })
    }
    fn summary(&self, index: &LtfsIndex, after: bool, budget: u64) -> Result<([u8; 32], u64, u64)> {
        self.summary_with_root(index, after, budget, None)
    }
    fn summary_with_root(
        &self,
        index: &LtfsIndex,
        after: bool,
        budget: u64,
        root: Option<&[Xattr]>,
    ) -> Result<([u8; 32], u64, u64)> {
        let conn = projection::temporary_database(budget)?;
        conn.execute_batch("CREATE TABLE nodes(path TEXT PRIMARY KEY COLLATE BINARY,uid BLOB NOT NULL UNIQUE CHECK(length(uid)=8),sha BLOB NOT NULL CHECK(length(sha)=32)) WITHOUT ROWID;").map_err(projection::sql)?;
        // Bound the original attributes before cloning or filtering the root.
        physical_source::attributes(
            &index.root.meta,
            &index.root.xattrs,
            None,
            index.highest_file_uid,
        )?;
        let attrs: Vec<Xattr> = root
            .unwrap_or(&index.root.xattrs)
            .iter()
            .filter(|a| !after || a.key != RENAME_MARKER)
            .cloned()
            .collect();
        let (mut files, mut dirs) = (0u64, 0u64);
        visit_index(index, &attrs, |path, row| {
            if path.len() > MAX_POOL_PATH_BYTES {
                return Err(bad());
            }
            let uid = match &row {
                Ref::File { node } => {
                    files += 1;
                    node.meta.file_uid
                }
                Ref::Directory { meta, .. } => {
                    dirs += 1;
                    meta.file_uid
                }
            };
            let bytes = encode(&row)?;
            conn.execute(
                "INSERT INTO nodes(path,uid,sha) VALUES(?,?,?)",
                params![
                    path,
                    uid.to_be_bytes().as_slice(),
                    row_hash(path, &bytes).as_slice()
                ],
            )
            .map_err(projection::sql)?;
            Ok(())
        })?;
        let f = &self.recipe.record.facts;
        let mut hash = if after {
            Sha256::new()
                .chain_update(b"tape-rs/node-rename-volume-projection/1")
                .chain_update(f.fingerprint)
                .chain_update(self.recipe.before_sha256())
                .chain_update(f.highest_before.to_be_bytes())
                .chain_update(index.highest_file_uid.to_be_bytes())
        } else {
            Sha256::new()
                .chain_update(b"tape-rs/node-rename-physical-source/1")
                .chain_update(f.guard)
                .chain_update(index.highest_file_uid.to_be_bytes())
        };
        let mut stmt = conn
            .prepare("SELECT path,uid,sha FROM nodes ORDER BY path COLLATE BINARY")
            .map_err(projection::sql)?;
        let mut rows = stmt.query([]).map_err(projection::sql)?;
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let path: String = row.get(0).map_err(projection::sql)?;
            let uid: Vec<u8> = row.get(1).map_err(projection::sql)?;
            let sha: Vec<u8> = row.get(2).map_err(projection::sql)?;
            if !after {
                hash.update(&uid);
            }
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            if after {
                hash.update(&uid);
            }
            hash.update(&sha);
        }
        Ok((hash.finalize().into(), files, dirs))
    }
}
impl NodeRenameS4Evidence {
    pub fn plan(&self) -> &NodeRenameMediaPlan {
        &self.plan
    }
    pub fn current_index(&self) -> ActiveIndexIdentity {
        self.current
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;

#[path = "absence.rs"]
mod absence;
pub use absence::{NodeRenameAbsenceEvidence, NodeRenameAbsenceKind};
