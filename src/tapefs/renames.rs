//! Original filesystem rename state. A pending journal outlives an RPC failure.
use super::*;
use crate::client::{
    FileVersion,
    fdb::{RenameCondition, RenameJournal},
};
use std::collections::BTreeMap;

pub struct RenameState {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) condition: RenameCondition,
    pub(super) no_replace: bool,
    pub(super) journal: Option<RenameJournal>,
    pub(super) location: Option<(u64, u64, uuid::Uuid)>,
    pub(super) resolved: bool,
}

impl RenameState {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        condition: RenameCondition,
        no_replace: bool,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            condition,
            no_replace,
            journal: None,
            location: None,
            resolved: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenameOutcome {
    pub source: FileVersion,
    pub source_parent: FileVersion,
    pub destination_parent: FileVersion,
    pub replaced: Option<FileVersion>,
    pub directory: bool,
}

pub(super) struct PendingRename {
    state: RenameState,
    source_ino: u64,
    source_named: BTreeSet<u64>,
    directory: bool,
}

fn within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}
fn next_version(value: FileVersion) -> Res<FileVersion> {
    match value {
        FileVersion::Fdb {
            namespace,
            inode,
            version,
        } => Ok(FileVersion::Fdb {
            namespace,
            inode,
            version: version.checked_add(1).ok_or(libc::EOVERFLOW)?,
        }),
        _ => Err(libc::ESTALE),
    }
}

impl<B: Backend> TapeFs<B> {
    fn rename_parent(&self, ino: u64) -> Res<FileVersion> {
        let current = self.node_snapshot(ino, None)?;
        if current.metadata["kind"] != "directory" {
            return Err(libc::ENOTDIR);
        }
        Ok(current.version)
    }
    fn rename_handles(&self, from: &str, to: &str) -> Vec<Arc<Mutex<Handle>>> {
        let mut handles: Vec<_> = lock(&self.handles).values().cloned().collect();
        handles.retain(|handle| {
            let value = lock(handle);
            let data = match &*value { Handle::Read {data} | Handle::Write {data,..} => data };
            let named = self.path_of(data.ino).is_ok_and(|path| within(&path,from) || within(&path,to));
            named || matches!(&*value, Handle::Write {path,..} if within(path,from) || within(path,to))
        });
        handles.sort_by_key(Arc::as_ptr);
        handles.dedup_by(|a, b| Arc::ptr_eq(a, b));
        handles
    }
    pub(super) fn fdb_rename_locked(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
        no_replace: bool,
    ) -> Res<()> {
        for name in [name, new_name] {
            if name.is_empty() || name.contains(['/', '\0']) || matches!(name, "." | "..") {
                return Err(libc::EINVAL);
            }
        }
        self.rename_parent(parent)?;
        self.rename_parent(new_parent)?;
        let from = join(&self.path_of(parent)?, name);
        let to = join(&self.path_of(new_parent)?, new_name);
        let source = self.attr_of_inner(&from)?;
        if from == to {
            return if no_replace {
                Err(libc::EEXIST)
            } else {
                Ok(())
            };
        }
        if within(&to, &from) || within(&from, &to) {
            return Err(libc::EINVAL);
        }
        let target = match self.attr_of_inner(&to) {
            Ok(value) => Some(value),
            Err(libc::ENOENT) => None,
            Err(error) => return Err(error),
        };
        if no_replace && target.is_some() {
            return Err(libc::EEXIST);
        }
        if let Some(target) = &target {
            if source.is_dir != target.is_dir {
                return Err(if source.is_dir {
                    libc::ENOTDIR
                } else {
                    libc::EISDIR
                });
            }
            if target.is_dir && !self.readdir_inner(target.ino)?.is_empty() {
                return Err(libc::ENOTEMPTY);
            }
        }
        let source_named: BTreeSet<_> = lock(&self.inodes)
            .by_path
            .iter()
            .filter(|(path, _)| within(path, &from))
            .map(|(_, ino)| *ino)
            .collect();
        let source_cache = self.cached(source.ino);
        let target_cache = target.as_ref().and_then(|target| self.cached(target.ino));
        let handles = self.rename_handles(&from, &to);
        let mut guards: Vec<_> = handles.iter().map(|handle| lock(handle)).collect();
        for value in &guards {
            if let Handle::Write { path, data, .. } = &**value
                && within(path, &to)
                && lock(&data.upload)
                    .as_ref()
                    .is_some_and(UploadState::pending)
            {
                return Err(libc::EBUSY);
            }
        }
        let pending: Vec<_> = lock(&self.pending)
            .iter()
            .map(|(ino, data)| (*ino, data.clone()))
            .collect();
        for (ino, data) in &pending {
            let path = self.path_of(*ino)?;
            if within(&path, &to)
                && lock(&data.upload)
                    .as_ref()
                    .is_some_and(UploadState::pending)
            {
                return Err(libc::EBUSY);
            }
        }
        for value in &mut guards {
            if let Handle::Write { path, data, .. } = &**value
                && within(path, &from)
                && source_named.contains(&data.ino)
            {
                self.sync_handle(value)?;
            }
        }
        for (ino, data) in pending {
            if source_named.contains(&ino)
                && data.unconfirmed.load(std::sync::atomic::Ordering::Acquire)
            {
                let path = self.path_of(ino)?;
                if !self.upload_cached(&path, &data, true)? {
                    return Err(libc::EBUSY);
                }
                data.unconfirmed
                    .store(false, std::sync::atomic::Ordering::Release);
                lock(&self.pending).remove(&ino);
            }
        }
        let source_version = self
            .node_snapshot(source.ino, source_cache.as_ref())?
            .version;
        let target_version = target
            .as_ref()
            .map(|target| {
                self.node_snapshot(target.ino, target_cache.as_ref())
                    .map(|node| node.version)
            })
            .transpose()?;
        // Refuse an already stale local snapshot before creating a durable intent.
        for (path, expected) in [(&from, Some(source_version)), (&to, target_version)] {
            if self
                .backend
                .stat(path)?
                .and_then(|stat| stat.current)
                .map(|stat| stat.version)
                != expected
            {
                return Err(libc::ESTALE);
            }
        }
        let source_parent = self.rename_parent(parent)?;
        let destination_parent = if parent == new_parent {
            source_parent
        } else {
            self.rename_parent(new_parent)?
        };
        let condition = RenameCondition {
            source: source_version,
            source_parent,
            destination_parent,
            target: target_version,
        };
        let mut pending = lock(&self.rename_pending);
        if pending.is_some() {
            return Err(libc::EBUSY);
        }
        *pending = Some(PendingRename {
            state: RenameState::new(from, to, condition, no_replace),
            source_ino: source.ino,
            source_named,
            directory: source.is_dir,
        });
        self.resolve_rename(&mut pending, &handles, &mut guards)
    }

    pub(super) fn resume_rename(&self) -> Res<()> {
        let mut pending = lock(&self.rename_pending);
        let Some(original) = pending.as_ref() else {
            return Ok(());
        };
        let handles = self.rename_handles(&original.state.from, &original.state.to);
        let mut guards: Vec<_> = handles.iter().map(|handle| lock(handle)).collect();
        self.resolve_rename(&mut pending, &handles, &mut guards)
    }
    fn resolve_rename(
        &self,
        pending: &mut Option<PendingRename>,
        handles: &[Arc<Mutex<Handle>>],
        guards: &mut [MutexGuard<'_, Handle>],
    ) -> Res<()> {
        let original = pending.as_mut().ok_or(libc::EIO)?;
        let result = self.backend.mutate_rename(&mut original.state);
        let outcome = match result {
            Ok(value) => value,
            Err(error) => {
                if !original.state.resolved && original.state.location.is_some() {
                    self.namespace_uncertain
                        .store(true, std::sync::atomic::Ordering::Release);
                } else {
                    pending.take();
                    self.namespace_uncertain
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                return Err(error);
            }
        };
        if let Err(error) = self.apply_rename(original, outcome, handles, guards) {
            self.namespace_uncertain
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
        pending.take();
        self.namespace_uncertain
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    fn apply_rename(
        &self,
        original: &PendingRename,
        outcome: RenameOutcome,
        handles: &[Arc<Mutex<Handle>>],
        guards: &mut [MutexGuard<'_, Handle>],
    ) -> Res<()> {
        let state = &original.state;
        if outcome.source != next_version(state.condition.source)?
            || outcome.source_parent != next_version(state.condition.source_parent)?
            || outcome.destination_parent != next_version(state.condition.destination_parent)?
            || outcome.replaced != state.condition.target.map(next_version).transpose()?
            || outcome.directory != original.directory
        {
            return Err(libc::EIO);
        }
        let current = self
            .backend
            .stat(&state.to)?
            .and_then(|stat| stat.current)
            .ok_or(libc::ESTALE)?;
        if current.version != outcome.source {
            return Err(libc::ESTALE);
        }
        let mut updates = BTreeMap::new();
        for value in guards.iter() {
            let data = match &**value {
                Handle::Read { data } | Handle::Write { data, .. } => data,
            };
            if !original.source_named.contains(&data.ino) || updates.contains_key(&data.ino) {
                continue;
            }
            let old = self.path_of(data.ino)?;
            if !within(&old, &state.from) {
                return Err(libc::ESTALE);
            }
            let path = format!("{}{}", state.to, &old[state.from.len()..]);
            let before = lock(&data.source).clone().ok_or(libc::ESTALE)?;
            let expected = if data.ino == original.source_ino {
                outcome.source
            } else {
                before.version
            };
            let after = self
                .backend
                .stat(&path)?
                .and_then(|stat| stat.current)
                .ok_or(libc::ESTALE)?;
            if after.version != expected
                || after.length != before.length
                || after.sha256 != before.sha256
            {
                return Err(libc::ESTALE);
            }
            updates.insert(data.ino, (data.clone(), after));
        }
        for (_, (data, after)) in updates {
            if let Some(time) = after.metadata["modify_time"]
                .as_str()
                .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            {
                *lock(&data.mtime) = SystemTime::from(time);
            }
            *lock(&data.upload) = Some(UploadState::new(UploadTarget::Fdb(
                crate::client::fdb::UploadCondition::Replace(after.version),
            )));
            *lock(&data.source) = Some(after);
        }
        for value in guards.iter_mut() {
            if let Handle::Write { path, data, .. } = &mut **value {
                if original.source_named.contains(&data.ino) && within(path, &state.from) {
                    *path = format!("{}{}", state.to, &path[state.from.len()..]);
                } else if within(path, &state.from) || within(path, &state.to) {
                    path.clear();
                    if !lock(&data.upload)
                        .as_ref()
                        .is_some_and(UploadState::pending)
                    {
                        data.unconfirmed
                            .store(false, std::sync::atomic::Ordering::Release);
                    }
                }
            }
        }
        let mut writers = lock(&self.writers);
        writers.retain(|path, _| !within(path, &state.from) && !within(path, &state.to));
        let registered = lock(&self.handles);
        for (handle, value) in handles.iter().zip(guards.iter()) {
            if let Handle::Write {
                path, data, len, ..
            } = &**value
                && original.source_named.contains(&data.ino)
                && !path.is_empty()
                && let Some(fh) = registered
                    .iter()
                    .find_map(|(fh, active)| Arc::ptr_eq(handle, active).then_some(*fh))
            {
                writers.insert(path.clone(), (fh, *len));
            }
        }
        drop(registered);
        drop(writers);
        let mut inodes = lock(&self.inodes);
        let moved: Vec<_> = inodes
            .by_path
            .iter()
            .filter(|(path, ino)| within(path, &state.from) && original.source_named.contains(*ino))
            .map(|(path, ino)| (format!("{}{}", state.to, &path[state.from.len()..]), *ino))
            .collect();
        let targets: Vec<_> = inodes
            .by_path
            .iter()
            .filter(|(path, _)| within(path, &state.to))
            .map(|(_, ino)| *ino)
            .collect();
        inodes
            .by_path
            .retain(|path, _| !within(path, &state.from) && !within(path, &state.to));
        for (path, ino) in moved {
            inodes.by_ino.get_mut(&ino).ok_or(libc::ESTALE)?.path = path.clone();
            inodes.by_path.insert(path, ino);
        }
        drop(inodes);
        for ino in targets {
            lock(&self.pending).remove(&ino);
        }
        Ok(())
    }
}
