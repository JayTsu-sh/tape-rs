//! Stable, exclusively leased request root. Entries are opened through its FD.
use super::*;
use crate::client::fdb::{NodeJournal, RenameJournal, RenameSpec, UploadJournal};
use nix::fcntl::{Flock, FlockArg};
use std::ffi::OsStr;

pub(super) struct JournalStore {
    directory: Flock<File>,
}

pub(super) enum SavedJournal {
    Upload(UploadJournal),
    Node(NodeJournal),
    Rename(RenameJournal),
}

impl JournalStore {
    pub(super) fn open(path: &Path) -> crate::client::Result<Self> {
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {
                File::open(
                    path.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new(".")),
                )?
                .sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        if directory.metadata()?.mode() & 0o077 != 0 {
            return Err(ClientError::Verification(
                "FDB 上传日志根目录必须仅允许当前用户访问（0700）".into(),
            ));
        }
        let directory = Flock::lock(directory, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| std::io::Error::from(error))?;
        directory.sync_all()?;
        Ok(Self { directory })
    }

    pub(super) fn identity(&self) -> std::io::Result<(u64, u64)> {
        let metadata = self.directory.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(super) fn entries(&self) -> std::io::Result<std::fs::ReadDir> {
        std::fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }

    pub(super) fn entry(&self, entry: std::fs::DirEntry) -> crate::client::Result<SavedJournal> {
        let name = entry.file_name();
        let valid_name = name.to_str().is_some_and(|name| {
            let id = name
                .strip_prefix(".fdb-upload-")
                .or_else(|| name.strip_prefix("rename-"))
                .or_else(|| name.strip_prefix(".fdb-rename-"))
                .or_else(|| name.strip_prefix("node-"))
                .or_else(|| name.strip_prefix(".fdb-node-"))
                .unwrap_or(name);
            Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id)
        });
        if !valid_name || !entry.file_type()?.is_dir() {
            return Err(ClientError::Verification(
                "FDB 日志根目录存在未知文件或非请求目录；保留现场并拒绝恢复".into(),
            ));
        }
        if name
            .to_str()
            .is_some_and(|s| s.starts_with("rename-") || s.starts_with(".fdb-rename-"))
        {
            self.open_rename(&name).map(SavedJournal::Rename)
        } else if name
            .to_str()
            .is_some_and(|s| s.starts_with("node-") || s.starts_with(".fdb-node-"))
        {
            self.open_node(&name).map(SavedJournal::Node)
        } else {
            self.open_journal(&name).map(SavedJournal::Upload)
        }
    }

    pub(super) fn open_rename(&self, name: &OsStr) -> crate::client::Result<RenameJournal> {
        RenameJournal::open_in(&self.directory, name)
    }
    pub(super) fn create_rename(
        &self,
        name: &OsStr,
        backend: &FdbBackend,
        spec: &RenameSpec,
    ) -> crate::client::Result<RenameJournal> {
        RenameJournal::create_in(&self.directory, name, &backend.library, spec)
    }
    pub(super) fn open_journal(&self, name: &OsStr) -> crate::client::Result<UploadJournal> {
        UploadJournal::open_in(&self.directory, name)
    }

    pub(super) fn open_node(&self, name: &OsStr) -> crate::client::Result<NodeJournal> {
        NodeJournal::open_in(&self.directory, name)
    }

    pub(super) fn create_node(
        &self,
        name: &OsStr,
        backend: &FdbBackend,
        state: &NodeState,
    ) -> crate::client::Result<NodeJournal> {
        NodeJournal::create_in(
            &self.directory,
            name,
            &backend.library,
            backend.pool,
            &state.path,
            state.condition,
            state.change.clone(),
        )
    }

    pub(super) fn create(
        &self,
        name: &OsStr,
        backend: &FdbBackend,
        path: &str,
        source: &Path,
        condition: crate::client::fdb::UploadCondition,
    ) -> crate::client::Result<UploadJournal> {
        UploadJournal::create_in(
            &self.directory,
            name,
            &backend.library,
            backend.pool,
            path,
            source,
            condition,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::fdb::UploadCondition;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rename_domain_uses_the_held_root_and_cannot_masquerade_as_a_node() {
        use crate::client::fdb::{RenameCondition, RenameSpec};
        let root = std::env::temp_dir().join(format!("fdb-rename-store-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("journals");
        let store = JournalStore::open(&path).unwrap();
        let namespace = Uuid::new_v4();
        let parent = FileVersion::Fdb {
            namespace,
            inode: Uuid::new_v4(),
            version: 1,
        };
        let backend = FdbBackend {
            client: FdbClient::new(["127.0.0.1:0"]).unwrap(),
            library: "SIM".into(),
            namespace,
            pool: Uuid::new_v4(),
            root: parent,
            uploads: None,
        };
        let spec = RenameSpec {
            pool: backend.pool,
            from: "/a".into(),
            to: "/b".into(),
            no_replace: true,
            condition: RenameCondition {
                source: FileVersion::Fdb {
                    namespace,
                    inode: Uuid::new_v4(),
                    version: 1,
                },
                source_parent: parent,
                destination_parent: parent,
                target: None,
            },
        };
        let held = root.join("held");
        std::fs::rename(&path, &held).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("foreign"), b"untouched").unwrap();
        let name = format!("rename-{}", Uuid::new_v4());
        let saved = store
            .create_rename(OsStr::new(&name), &backend, &spec)
            .unwrap();
        let operation = saved.operation();
        drop(saved);
        let SavedJournal::Rename(saved) = store
            .entry(store.entries().unwrap().next().unwrap().unwrap())
            .unwrap()
        else {
            panic!("rename domain")
        };
        assert_eq!(saved.operation(), operation);
        assert_eq!(saved.spec(), &spec);
        drop(saved);
        assert!(!path.join(&name).exists());
        std::fs::rename(
            held.join(&name),
            held.join(format!("node-{}", Uuid::new_v4())),
        )
        .unwrap();
        assert!(
            store
                .entry(store.entries().unwrap().next().unwrap().unwrap())
                .is_err()
        );
        assert_eq!(std::fs::read(path.join("foreign")).unwrap(), b"untouched");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn node_records_use_the_held_store_and_an_explicit_domain() {
        let root = std::env::temp_dir().join(format!("fdb-node-store-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("journals");
        let store = JournalStore::open(&path).unwrap();
        let namespace = Uuid::new_v4();
        let parent = FileVersion::Fdb {
            namespace,
            inode: Uuid::new_v4(),
            version: 1,
        };
        let backend = FdbBackend {
            client: FdbClient::new(["127.0.0.1:0"]).unwrap(),
            library: "SIM".into(),
            namespace,
            pool: Uuid::new_v4(),
            root: parent,
            uploads: None,
        };
        let state = NodeState {
            path: "/dir".into(),
            condition: crate::client::fdb::NodeCondition { parent, node: None },
            change: json!({"kind":"create-directory"}),
            journal: None,
            location: None,
            resolved: false,
        };
        let held = root.join("held");
        std::fs::rename(&path, &held).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("foreign"), b"untouched").unwrap();
        let name = format!("node-{}", Uuid::new_v4());
        let saved = store
            .create_node(OsStr::new(&name), &backend, &state)
            .unwrap();
        let operation = saved.operation();
        drop(saved);
        assert!(held.join(&name).is_dir());
        assert!(!path.join(&name).exists());
        let SavedJournal::Node(saved) = store
            .entry(store.entries().unwrap().next().unwrap().unwrap())
            .unwrap()
        else {
            panic!("expected node domain")
        };
        assert_eq!(saved.operation(), operation);
        assert_eq!(saved.condition(), state.condition);
        assert!(saved.terminal().unwrap().is_none());
        drop(saved);
        // A node header cannot masquerade as an upload just by changing its name.
        std::fs::rename(held.join(&name), held.join(Uuid::new_v4().to_string())).unwrap();
        assert!(
            store
                .entry(store.entries().unwrap().next().unwrap().unwrap())
                .is_err()
        );
        assert_eq!(std::fs::read(path.join("foreign")).unwrap(), b"untouched");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_root_is_exclusive_private_and_anchored_across_path_replacement() {
        let root = std::env::temp_dir().join(format!("fdb-store-cap-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("journals");
        let store = JournalStore::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert!(JournalStore::open(&path).is_err());
        let held = root.join("held");
        std::fs::rename(&path, &held).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("foreign"), b"untouched").unwrap();
        let namespace = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let backend = FdbBackend {
            client: FdbClient::new(["127.0.0.1:0"]).unwrap(),
            library: "SIM".into(),
            namespace,
            pool: Uuid::new_v4(),
            root: FileVersion::Fdb {
                namespace,
                inode: parent,
                version: 1,
            },
            uploads: None,
        };
        let source = root.join("source");
        std::fs::write(&source, b"abc").unwrap();
        let name = Uuid::new_v4().to_string();
        let saved = store
            .create(
                OsStr::new(&name),
                &backend,
                "/a",
                &source,
                UploadCondition::Create { namespace, parent },
            )
            .unwrap();
        let operation = saved.operation();
        assert_eq!(
            std::fs::read(held.join(&name).join("payload")).unwrap(),
            b"abc"
        );
        assert_eq!(std::fs::read(path.join("foreign")).unwrap(), b"untouched");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
        drop(saved);
        let saved = store
            .entry(store.entries().unwrap().next().unwrap().unwrap())
            .unwrap();
        let SavedJournal::Upload(saved) = saved else {
            panic!("expected upload journal")
        };
        assert_eq!(saved.operation(), operation);
        drop(saved);
        drop(store);
        drop(JournalStore::open(&held).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_root_refuses_symlinks_public_permissions_and_unknown_entries() {
        let root = std::env::temp_dir().join(format!("fdb-store-refuse-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let public = root.join("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(JournalStore::open(&public).is_err());
        assert_eq!(std::fs::metadata(&public).unwrap().mode() & 0o777, 0o755);
        let real = root.join("real");
        let store = JournalStore::open(&real).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(JournalStore::open(&alias).is_err());
        let unknown = real.join("unknown");
        std::fs::write(&unknown, b"foreign").unwrap();
        assert!(
            store
                .entry(store.entries().unwrap().next().unwrap().unwrap())
                .is_err()
        );
        std::fs::remove_file(&unknown).unwrap();
        let link = real.join(Uuid::new_v4().to_string());
        std::os::unix::fs::symlink(&public, &link).unwrap();
        assert!(
            store
                .entry(store.entries().unwrap().next().unwrap().unwrap())
                .is_err()
        );
        assert!(std::fs::symlink_metadata(link).unwrap().is_symlink());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
