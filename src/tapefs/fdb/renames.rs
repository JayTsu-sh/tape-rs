//! Durable rename submission through the mount's exclusively held journal root.
use super::*;
use crate::client::fdb::{RenameJournal, RenameSpec};

fn revision(value: &Value) -> Res<FileVersion> {
    Ok(FileVersion::Fdb {
        namespace: value["namespace"]
            .as_str()
            .ok_or(libc::EIO)?
            .parse()
            .map_err(|_| libc::EIO)?,
        inode: value["inode"]
            .as_str()
            .ok_or(libc::EIO)?
            .parse()
            .map_err(|_| libc::EIO)?,
        version: value["version"].as_u64().ok_or(libc::EIO)?,
    })
}

impl FdbBackend {
    pub(super) fn rename_spec(&self, state: &RenameState) -> RenameSpec {
        RenameSpec {
            pool: self.pool,
            from: state.from.clone(),
            to: state.to.clone(),
            condition: state.condition,
            no_replace: state.no_replace,
        }
    }
    pub(super) fn verify_rename(
        &self,
        saved: &RenameJournal,
        spec: &RenameSpec,
    ) -> crate::client::Result<()> {
        if saved.library() != self.library
            || saved.namespace() != self.namespace
            || saved.pool() != self.pool
            || saved.spec() != spec
        {
            return Err(ClientError::Verification(
                "原重命名日志与挂载范围或两端条件不一致".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn rename_saved(&self, state: &mut RenameState) -> Res<RenameOutcome> {
        let store = self.uploads.as_ref().ok_or(libc::EROFS)?;
        let spec = self.rename_spec(state);
        spec.validate().map_err(|_| libc::EINVAL)?;
        if !matches!(spec.condition.source, FileVersion::Fdb { namespace, .. } if namespace == self.namespace)
        {
            return Err(libc::ESTALE);
        }
        let (dev, ino) = store.identity().map_err(io_errno)?;
        let (new, directory) = match state.location {
            Some((original_dev, original_ino, directory)) => {
                if (dev, ino) != (original_dev, original_ino) {
                    return Err(libc::ESTALE);
                }
                (false, directory)
            }
            None => {
                let directory = Uuid::new_v4();
                state.location = Some((dev, ino, directory));
                (true, directory)
            }
        };
        if state.journal.is_none() {
            let name = format!("rename-{directory}");
            state.journal = Some(
                if new {
                    store.create_rename(std::ffi::OsStr::new(&name), self, &spec)
                } else {
                    store.open_rename(std::ffi::OsStr::new(&name))
                }
                .map_err(|error| errno_of(&error))?,
            );
        }
        let saved = state.journal.as_mut().ok_or(libc::EIO)?;
        self.verify_rename(saved, &spec)
            .map_err(|error| errno_of(&error))?;
        let terminal = self.client.run_rename_journal(saved).map_err(|error| {
            log::warn!(
                "FDB 重命名 {} -> {} 未完成，保留原日志: {error}",
                spec.from,
                spec.to
            );
            errno_of(&error)
        })?;
        state.resolved = true;
        if terminal["status"] != "completed" {
            return Err(libc::ECANCELED);
        }
        let value = &terminal["result"];
        Ok(RenameOutcome {
            source: revision(&value["source"])?,
            source_parent: revision(&value["source_parent"])?,
            destination_parent: revision(&value["destination_parent"])?,
            replaced: if value["replaced"].is_null() {
                None
            } else {
                Some(revision(&value["replaced"])?)
            },
            directory: value["directory"].as_bool().ok_or(libc::EIO)?,
        })
    }
}
