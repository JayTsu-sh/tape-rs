//! FDB namespace changes retain the original condition and journal until resolved.
use super::*;
use crate::client::{
    FileVersion,
    fdb::{NodeCondition, NodeJournal},
};
use serde_json::Value;

pub struct NodeState {
    pub(super) path: String,
    pub(super) condition: NodeCondition,
    pub(super) change: Value,
    pub(super) journal: Option<NodeJournal>,
    pub(super) location: Option<(u64, u64, uuid::Uuid)>,
    pub(super) resolved: bool,
}

pub struct NodeOutcome {
    pub version: FileVersion,
    pub deleted: bool,
}

pub(super) struct PendingNode {
    state: NodeState,
    ino: Option<u64>,
    cached: Option<Arc<CachedFile>>,
}

impl<B: Backend> TapeFs<B> {
    pub(super) fn node_version_for_inode(&self, ino: u64) -> Res<FileVersion> {
        self.node_snapshot(ino, self.cached(ino).as_ref())
            .map(|s| s.version)
    }

    pub(super) fn node_snapshot(
        &self,
        ino: u64,
        cached: Option<&Arc<CachedFile>>,
    ) -> Res<crate::client::FileStat> {
        let source = cached.and_then(|data| lock(&data.source).clone());
        let snapshot = match source {
            Some(source) => source,
            None => self
                .backend
                .stat(&self.path_of(ino)?)?
                .and_then(|s| s.current)
                .ok_or(libc::ESTALE)?,
        };
        self.check_node_identity(ino, snapshot.version)?;
        Ok(snapshot)
    }

    pub(super) fn check_node_identity(&self, ino: u64, version: FileVersion) -> Res<()> {
        let FileVersion::Fdb {
            namespace, inode, ..
        } = version
        else {
            return Err(libc::ESTALE);
        };
        if lock(&self.inodes)
            .by_ino
            .get(&ino)
            .and_then(|r| r.fdb_identity)
            != Some((namespace, inode))
        {
            return Err(libc::ESTALE);
        }
        Ok(())
    }

    // Caller holds namespace write lock. Parent versions may advance between
    // operations, but a re-created same-name parent must never replace this inode.
    pub(super) fn fdb_node(
        &self,
        path: &str,
        node: Option<(u64, FileVersion)>,
        change: Value,
    ) -> Res<Option<Attr>> {
        let cached = node.and_then(|(ino, _)| self.cached(ino));
        self.fdb_node_cached(path, node, change, cached)
    }

    pub(super) fn fdb_node_cached(
        &self,
        path: &str,
        node: Option<(u64, FileVersion)>,
        change: Value,
        cached: Option<Arc<CachedFile>>,
    ) -> Res<Option<Attr>> {
        let parent_path = parent_of(path);
        let parent_ino = lock(&self.inodes)
            .by_path
            .get(parent_path)
            .copied()
            .ok_or(libc::ESTALE)?;
        let parent = self
            .backend
            .stat(parent_path)?
            .and_then(|s| s.current)
            .ok_or(libc::ESTALE)?;
        self.check_node_identity(parent_ino, parent.version)?;
        if parent.metadata["kind"] != "directory" {
            return Err(libc::ENOTDIR);
        }
        if let Some((ino, version)) = node {
            self.check_node_identity(ino, version)?;
        }
        let condition = NodeCondition {
            parent: parent.version,
            node: node.map(|(_, v)| v),
        };
        condition.check(&change).map_err(|_| libc::EINVAL)?;
        let mut pending = lock(&self.node_pending);
        if pending.is_some() {
            return Err(libc::EBUSY);
        }
        *pending = Some(PendingNode {
            state: NodeState {
                path: path.into(),
                condition,
                change,
                journal: None,
                location: None,
                resolved: false,
            },
            ino: node.map(|(ino, _)| ino),
            cached,
        });
        self.resolve_node(&mut pending)
    }

    pub(super) fn resume_node(&self) -> Res<()> {
        let mut pending = lock(&self.node_pending);
        if pending.is_some() {
            self.resolve_node(&mut pending).map(|_| ())
        } else {
            Ok(())
        }
    }

    fn resolve_node(&self, pending: &mut Option<PendingNode>) -> Res<Option<Attr>> {
        let original = pending.as_mut().ok_or(libc::EIO)?;
        let outcome = self.backend.mutate_node(&mut original.state);
        if outcome.is_err() && !original.state.resolved && original.state.location.is_some() {
            self.namespace_uncertain
                .store(true, std::sync::atomic::Ordering::Release);
            return outcome.map(|_| None);
        }
        let original = pending.take().expect("original node operation");
        self.namespace_uncertain
            .store(false, std::sync::atomic::Ordering::Release);
        let outcome = outcome?;
        if outcome.deleted {
            let mut inodes = lock(&self.inodes);
            if inodes.by_path.get(&original.state.path).copied() == original.ino {
                inodes.by_path.remove(&original.state.path);
            }
            if let Some(ino) = original.ino {
                lock(&self.pending).remove(&ino);
            }
            return Ok(None);
        }
        // A receipt proves only the original operation. A later same-name object
        // cannot supply its attributes or change the old open file's snapshot.
        let current = self
            .backend
            .stat(&original.state.path)?
            .and_then(|s| s.current)
            .ok_or(libc::ESTALE)?;
        if current.version != outcome.version {
            return Err(libc::ESTALE);
        }
        if let Some(data) = original.cached {
            *lock(&data.source) = Some(current.clone());
            *lock(&data.upload) = Some(UploadState::new(UploadTarget::Fdb(
                crate::client::fdb::UploadCondition::Replace(current.version),
            )));
            if let Some(time) = current.metadata["modify_time"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            {
                *lock(&data.mtime) = SystemTime::from(time);
            }
        }
        let ino = self.ino_for_version(&original.state.path, Some(current.version));
        Ok(Some(Attr {
            ino,
            is_dir: current.metadata["kind"] == "directory",
            is_symlink: current.metadata["kind"] == "symlink",
            size: current.length,
            mtime: current.metadata["modify_time"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(SystemTime::from)
                .unwrap_or(UNIX_EPOCH),
            ..Attr::from_metadata(&current.metadata)
        }))
    }
}
