//! A namespace-pinned FDB client with explicitly enabled durable filesystem changes.
use super::*;
use crate::client::fdb::FdbClient;
use crate::client::{ClientError, FileStat, FileVersion};
use serde_json::{Value, json};
use uuid::Uuid;
mod journals;
mod renames;
use journals::{JournalStore, SavedJournal};

pub struct FdbBackend {
    client: FdbClient,
    library: String,
    namespace: Uuid,
    pool: Uuid,
    root: FileVersion,
    uploads: Option<JournalStore>,
}

impl FdbBackend {
    pub fn new(
        client: FdbClient,
        library: String,
        namespace: Uuid,
        pool: Uuid,
    ) -> crate::client::Result<Self> {
        if namespace.is_nil() || pool.is_nil() {
            return Err(ClientError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "FDB 挂载的 namespace 和 pool 必须是非空 UUID",
            )));
        }
        let root = client
            .stat(&library, pool, "/")?
            .ok_or(ClientError::NotFound)?;
        if root["namespace"] != namespace.to_string() || root["kind"] != "directory" {
            return Err(ClientError::Verification(
                "FDB 挂载池根或命名空间不一致".into(),
            ));
        }
        let root_inode = Uuid::parse_str(root["id"].as_str().unwrap_or(""))
            .ok()
            .filter(|id| !id.is_nil())
            .ok_or_else(|| ClientError::Verification("FDB 池根身份无效".into()))?;
        let root = FileVersion::Fdb {
            namespace,
            inode: root_inode,
            version: root["version"]
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| ClientError::Verification("FDB 池根版本无效".into()))?,
        };
        Ok(Self {
            client,
            library,
            namespace,
            pool,
            root,
            uploads: None,
        })
    }

    /// Recover every original request before exposing a writable mount.
    /// The supplied local directory stays exclusively leased until drop.
    pub fn new_writable(
        client: FdbClient,
        library: String,
        namespace: Uuid,
        pool: Uuid,
        journal_directory: &Path,
    ) -> crate::client::Result<Self> {
        let mut backend = Self::new(client, library, namespace, pool)?;
        let store = JournalStore::open(journal_directory)?;
        // Validate all local records before any recovery can submit a mutation.
        // Streaming passes keep memory and open descriptors bounded.
        for entry in store.entries()? {
            match store.entry(entry?)? {
                SavedJournal::Upload(saved) => {
                    backend.verify_journal(&saved, saved.path(), saved.condition()?)?;
                    saved.acknowledged()?;
                }
                SavedJournal::Rename(saved) => {
                    backend.verify_rename(&saved, saved.spec())?;
                    saved.terminal()?;
                }
                SavedJournal::Node(saved) => {
                    backend.verify_node(&saved, saved.path(), saved.condition(), saved.change())?;
                    saved.terminal()?;
                }
            }
        }
        for entry in store.entries()? {
            match store.entry(entry?)? {
                SavedJournal::Upload(mut saved) => {
                    backend.verify_journal(&saved, saved.path(), saved.condition()?)?;
                    backend.client.upload_journal(&mut saved).map_err(|error| {
                        ClientError::Indeterminate(format!(
                            "恢复原上传 {} {} 失败，日志保留: {error}",
                            saved.operation(),
                            saved.path()
                        ))
                    })?;
                }
                SavedJournal::Rename(mut saved) => {
                    backend.verify_rename(&saved, saved.spec())?;
                    backend
                        .client
                        .run_rename_journal(&mut saved)
                        .map_err(|error| {
                            ClientError::Indeterminate(format!(
                                "恢复原重命名 {} {} -> {} 失败，日志保留: {error}",
                                saved.operation(),
                                saved.spec().from,
                                saved.spec().to
                            ))
                        })?;
                }
                SavedJournal::Node(mut saved) => {
                    backend.verify_node(&saved, saved.path(), saved.condition(), saved.change())?;
                    backend
                        .client
                        .run_node_journal(&mut saved)
                        .map_err(|error| {
                            ClientError::Indeterminate(format!(
                                "恢复原节点 {} {} 失败，日志保留: {error}",
                                saved.operation(),
                                saved.path()
                            ))
                        })?;
                }
            }
        }
        backend.uploads = Some(store);
        Ok(backend)
    }

    fn verify_journal(
        &self,
        saved: &crate::client::fdb::UploadJournal,
        path: &str,
        condition: crate::client::fdb::UploadCondition,
    ) -> crate::client::Result<()> {
        if saved.library() != self.library
            || saved.namespace() != self.namespace
            || saved.pool() != self.pool
            || saved.path() != path
            || saved.condition()? != condition
        {
            return Err(ClientError::Verification(
                "原上传日志与挂载的库、namespace、池、路径或条件不一致".into(),
            ));
        }
        Ok(())
    }

    fn verify_node(
        &self,
        saved: &crate::client::fdb::NodeJournal,
        path: &str,
        condition: crate::client::fdb::NodeCondition,
        change: &Value,
    ) -> crate::client::Result<()> {
        if saved.library() != self.library
            || saved.namespace() != self.namespace
            || saved.pool() != self.pool
            || saved.path() != path
            || saved.condition() != condition
            || saved.change() != change
        {
            return Err(ClientError::Verification(
                "原节点日志与挂载范围、路径或条件不一致".into(),
            ));
        }
        Ok(())
    }

    fn namespace_matches(&self, value: &Value) -> Res<()> {
        if value["namespace"] == self.namespace.to_string() {
            Ok(())
        } else {
            Err(libc::ESTALE)
        }
    }

    fn node(&self, path: &str) -> Res<Option<Value>> {
        let result = self
            .client
            .stat(&self.library, self.pool, path)
            .map_err(|e| errno_of(&e))?;
        if let Some(node) = &result {
            self.namespace_matches(node)?;
        }
        Ok(result)
    }

    fn node_version(&self, node: &Value) -> Res<FileVersion> {
        Ok(FileVersion::Fdb {
            namespace: self.namespace,
            inode: Uuid::parse_str(node["id"].as_str().ok_or(libc::EIO)?).map_err(|_| libc::EIO)?,
            version: node["version"].as_u64().ok_or(libc::EIO)?,
        })
    }

    fn same_node(a: &Value, b: &Value) -> bool {
        ["namespace", "pool_id", "id", "version", "kind"]
            .iter()
            .all(|field| a[*field] == b[*field])
    }
}

impl Backend for FdbBackend {
    fn root_version(&self) -> Option<FileVersion> {
        Some(self.root)
    }
    fn stat(&self, path: &str) -> Res<Option<PathStat>> {
        let Some(node) = self.node(path)? else {
            return Ok(None);
        };
        let attributes = self
            .client
            .node_attributes(&self.library, self.pool, path)
            .map_err(|e| errno_of(&e))?
            .ok_or(libc::EAGAIN)?;
        self.namespace_matches(&attributes)?;
        if !Self::same_node(&node, &attributes) {
            return Err(libc::EAGAIN);
        }
        let kind = node["kind"].as_str().ok_or(libc::EIO)?;
        let length = if kind == "directory" {
            0
        } else {
            node["size"].as_u64().ok_or(libc::EIO)?
        };
        let generation = if kind == "directory" {
            None
        } else {
            Some(node["generation"].as_u64().ok_or(libc::EIO)?)
        };
        let metadata = json!({
            "kind":kind, "symlink":attributes["symlink_target"],
            "modify_time":attributes["metadata"]["modify_time"],
            "node":attributes["metadata"], "xattrs":attributes["xattrs"]
        });
        Ok(Some(PathStat {
            state: "committed".into(),
            length,
            current: Some(FileStat {
                length,
                generation,
                round: None,
                barcode: node["barcode"].as_str().unwrap_or("").into(),
                sha256: if node["digest"]["algorithm"] == "sha256" {
                    node["digest"]["hex"].as_str().ok_or(libc::EIO)?.into()
                } else {
                    String::new()
                },
                version: self.node_version(&node)?,
                metadata,
            }),
        }))
    }

    fn list_dir(&self, path: &str) -> Res<Option<DirectoryListing>> {
        let Some(before) = self.node(path)? else {
            return Ok(None);
        };
        if before["kind"] != "directory" {
            return Err(libc::ENOTDIR);
        }
        let mut after = None;
        let mut last: Option<String> = None;
        let mut items = Vec::new();
        loop {
            let page = self
                .client
                .list_children(&self.library, self.pool, path, 100, after.as_deref())
                .map_err(|e| errno_of(&e))?;
            self.namespace_matches(&page)?;
            for child in page["items"].as_array().ok_or(libc::EIO)? {
                let name = child["name"].as_str().ok_or(libc::EILSEQ)?;
                if last
                    .as_ref()
                    .is_some_and(|previous| previous.as_bytes() >= name.as_bytes())
                {
                    return Err(libc::EIO);
                }
                let node = self.node(&join(path, name))?.ok_or(libc::EAGAIN)?;
                if node["id"] != child["id"] {
                    return Err(libc::EAGAIN);
                }
                let kind = node["kind"].as_str().ok_or(libc::EIO)?;
                items.push(DirItem {
                    version: Some(self.node_version(&node)?),
                    name: name.into(),
                    is_dir: kind == "directory",
                    is_symlink: Some(kind == "symlink"),
                    state: if kind == "directory" {
                        String::new()
                    } else {
                        "committed".into()
                    },
                    committed_length: node["size"].as_u64(),
                    staged_length: None,
                });
                last = Some(name.into());
            }
            match page["next"].as_str() {
                Some(next) => after = Some(next.to_owned()),
                None if page["next"].is_null() => break,
                _ => return Err(libc::EIO),
            }
        }
        let current = self.node(path)?.ok_or(libc::EAGAIN)?;
        if !Self::same_node(&before, &current) {
            return Err(libc::EAGAIN);
        }
        Ok(Some(DirectoryListing {
            version: Some(self.node_version(&before)?),
            entries: items,
        }))
    }

    fn fetch(&self, path: &str, out: &mut File) -> Res<u64> {
        self.client
            .download_to_scoped(&self.library, self.namespace, self.pool, path, out)
            .map_err(|e| errno_of(&e))
    }
    fn upload(
        &self,
        path: &str,
        source: &Path,
        state: &mut UploadState,
        _: bool,
    ) -> Res<UploadOutcome> {
        let store = self.uploads.as_ref().ok_or(libc::EROFS)?;
        let UploadTarget::Fdb(condition) = state.target else {
            return Err(libc::EINVAL);
        };
        let namespace = match condition {
            crate::client::fdb::UploadCondition::Create { namespace, .. } => namespace,
            crate::client::fdb::UploadCondition::Replace(FileVersion::Fdb {
                namespace, ..
            }) => namespace,
            _ => return Err(libc::EINVAL),
        };
        if namespace != self.namespace
            || state.path.as_ref().is_some_and(|original| original != path)
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
                // Keep this reservation even when create reports a failed fsync;
                // a published original record must never become a new operation.
                state.location = Some((dev, ino, directory));
                state.path = Some(path.into());
                (true, directory)
            }
        };
        if state.journal.is_none() {
            let name = directory.to_string();
            state.journal = Some(
                if new {
                    store.create(std::ffi::OsStr::new(&name), self, path, source, condition)
                } else {
                    store.open_journal(std::ffi::OsStr::new(&name))
                }
                .map_err(|e| errno_of(&e))?,
            );
        }
        let saved = state.journal.as_mut().ok_or(libc::EIO)?;
        self.verify_journal(saved, path, condition)
            .map_err(|e| errno_of(&e))?;
        if saved.size() != std::fs::metadata(source).map_err(io_errno)?.len()
            || saved.sha256() != crate::client::sha256_file(source).map_err(io_errno)?
        {
            return Err(libc::EBUSY);
        }
        self.client
            .upload_journal(saved)
            .map(UploadOutcome::Fdb)
            .map_err(|e| errno_of(&e))
    }
    fn mutate_node(&self, state: &mut NodeState) -> Res<NodeOutcome> {
        let store = self.uploads.as_ref().ok_or(libc::EROFS)?;
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
            let name = format!("node-{directory}");
            state.journal = Some(
                if new {
                    store.create_node(std::ffi::OsStr::new(&name), self, state)
                } else {
                    store.open_node(std::ffi::OsStr::new(&name))
                }
                .map_err(|e| errno_of(&e))?,
            );
        }
        let saved = state.journal.as_mut().ok_or(libc::EIO)?;
        self.verify_node(saved, &state.path, state.condition, &state.change)
            .map_err(|e| errno_of(&e))?;
        let terminal = self
            .client
            .run_node_journal(saved)
            .map_err(|e| errno_of(&e))?;
        state.resolved = true;
        if terminal["status"] != "completed" {
            return Err(libc::ECANCELED);
        }
        Ok(NodeOutcome {
            version: FileVersion::Fdb {
                namespace: self.namespace,
                inode: Uuid::parse_str(terminal["result"]["node"].as_str().ok_or(libc::EIO)?)
                    .map_err(|_| libc::EIO)?,
                version: terminal["result"]["version"].as_u64().ok_or(libc::EIO)?,
            },
            deleted: terminal["result"]["deleted"].as_bool().ok_or(libc::EIO)?,
        })
    }
    fn mutate_rename(&self, state: &mut RenameState) -> Res<RenameOutcome> {
        self.rename_saved(state)
    }
    fn delete(&self, _: &str) -> Res<()> {
        Err(libc::EROFS)
    }
    fn sync(&self) -> Res<()> {
        Ok(())
    }
    fn change_metadata(&self, _: &str, _: &crate::ltfs::index::MetadataChange) -> Res<()> {
        Err(libc::EROFS)
    }
    fn change_xattr(&self, _: &str, _: &str, _: Option<&[u8]>, _: u32) -> Res<()> {
        Err(libc::EROFS)
    }
    fn rename(&self, _: &str, _: &str, _: bool) -> Res<()> {
        Err(if self.uploads.is_some() {
            libc::EOPNOTSUPP
        } else {
            libc::EROFS
        })
    }
    fn symlink(&self, _: &str, _: &str) -> Res<()> {
        Err(libc::EROFS)
    }
    fn mkdir(&self, _: &str) -> Res<()> {
        Err(libc::EROFS)
    }
    fn rmdir(&self, _: &str) -> Res<()> {
        Err(libc::EROFS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn route(operation: &str, pool: Uuid, path: &str) -> String {
        format!(
            "GET /v1/fdb/{operation}/SIM?path={}&pool={pool}",
            URL_SAFE_NO_PAD.encode(path)
        )
    }
    fn directory(namespace: Uuid, pool: Uuid) -> Value {
        json!({"library_serial":"SIM","namespace":namespace.to_string(),"pool_id":pool.to_string(),
            "kind":"directory","id":Uuid::new_v4().to_string(),"version":1})
    }
    fn file(namespace: Uuid, pool: Uuid, kind: &str) -> Value {
        json!({"library_serial":"SIM","namespace":namespace.to_string(),"pool_id":pool.to_string(),
            "kind":kind,"id":Uuid::new_v4().to_string(),"version":1,"size":0,"generation":4,
            "volume_id":Uuid::new_v4().to_string(),"barcode":"READ01L8","digest":null})
    }
    fn attributes(node: &Value, path: &str) -> Value {
        json!({"schema_version":1,"library_serial":"SIM","namespace":node["namespace"],
            "pool_id":node["pool_id"],"path":path,"id":node["id"],"version":node["version"],"kind":node["kind"],
            "attributes_digest":"ab".repeat(32),"symlink_target":null,
            "metadata":{"readonly":true,"file_uid":7,"creation_time":"original","change_time":"", "modify_time":"2026-11-01T12:34:56.000000000Z","access_time":"","backup_time":"unknown"},
            "xattrs":[{"key":"user.binary","value":"AP8=","base64":true}]})
    }
    fn server(steps: Vec<(String, Value)>) -> (FdbClient, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let join = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for (route, value) in steps {
                let mut connection = loop {
                    match listener.accept() {
                        Ok((connection, _)) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing {route}");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(connection.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert_eq!(first.trim_end(), format!("{route} HTTP/1.1"));
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let body = value.to_string();
                write!(
                    connection,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        (
            FdbClient::new([address])
                .unwrap()
                .with_timeouts(Duration::from_millis(100), Duration::from_secs(2)),
            join,
        )
    }

    #[test]
    fn stat_refuses_attributes_from_another_node_version_kind_or_namespace() {
        let namespace = Uuid::new_v4();
        let pool = Uuid::new_v4();
        for mismatch in ["id", "version", "kind", "namespace"] {
            let node = file(namespace, pool, "file");
            let mut attrs = attributes(&node, "/a");
            attrs[mismatch] = match mismatch {
                "version" => json!(2),
                "kind" => json!("directory"),
                _ => json!(Uuid::new_v4().to_string()),
            };
            let (client, join) = server(vec![
                (route("stat", pool, "/"), directory(namespace, pool)),
                (route("stat", pool, "/a"), node),
                (route("attributes", pool, "/a"), attrs),
            ]);
            let backend = FdbBackend::new(client, "SIM".into(), namespace, pool).unwrap();
            assert_eq!(
                backend.stat("/a"),
                Err(if mismatch == "namespace" {
                    libc::ESTALE
                } else {
                    libc::EAGAIN
                }),
                "{mismatch}"
            );
            join.join().unwrap();
        }
    }

    #[test]
    fn directory_metadata_keeps_original_values_without_inventing_a_physical_source() {
        let namespace = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let node = directory(namespace, pool);
        let attrs = attributes(&node, "/目录");
        let (client, join) = server(vec![
            (route("stat", pool, "/"), directory(namespace, pool)),
            (route("stat", pool, "/目录"), node.clone()),
            (route("attributes", pool, "/目录"), attrs.clone()),
        ]);
        let backend = FdbBackend::new(client, "SIM".into(), namespace, pool).unwrap();
        let stat = backend.stat("/目录").unwrap().unwrap().current.unwrap();
        assert_eq!(stat.generation, None);
        assert_eq!(stat.round, None);
        assert!(stat.barcode.is_empty() && stat.sha256.is_empty());
        assert_eq!(stat.metadata["node"], attrs["metadata"]);
        assert_eq!(stat.metadata["xattrs"], attrs["xattrs"]);
        assert_eq!(
            stat.version,
            FileVersion::Fdb {
                namespace,
                inode: Uuid::parse_str(node["id"].as_str().unwrap()).unwrap(),
                version: 1
            }
        );
        join.join().unwrap();
    }

    #[test]
    fn directory_pages_keep_types_and_reject_a_changed_parent() {
        for changed in [false, true] {
            let namespace = Uuid::new_v4();
            let pool = Uuid::new_v4();
            let parent = directory(namespace, pool);
            let mut after = parent.clone();
            if changed {
                after["version"] = json!(2);
            }
            let a = file(namespace, pool, "file");
            let d = directory(namespace, pool);
            let l = file(namespace, pool, "symlink");
            let child = |name: &str, node: &Value| json!({"name":name,"name_b64":URL_SAFE_NO_PAD.encode(name),"id":node["id"]});
            let cursor = URL_SAFE_NO_PAD.encode(b"b");
            let page = |items: Vec<Value>, next: Option<&str>| json!({"library_serial":"SIM","namespace":namespace.to_string(),"pool_id":pool.to_string(),"items":items,"next":next});
            let first = format!("GET /v1/fdb/children/SIM?path=Lw&limit=100&pool={pool}");
            let (client, join) = server(vec![
                (route("stat", pool, "/"), parent.clone()),
                (route("stat", pool, "/"), parent),
                (first.clone(), page(vec![child("a", &a)], Some(&cursor))),
                (route("stat", pool, "/a"), a),
                (
                    format!("{first}&after={cursor}"),
                    page(vec![child("d", &d), child("l", &l)], None),
                ),
                (route("stat", pool, "/d"), d),
                (route("stat", pool, "/l"), l),
                (route("stat", pool, "/"), after),
            ]);
            let backend = FdbBackend::new(client, "SIM".into(), namespace, pool).unwrap();
            let result = backend.list_dir("/");
            if changed {
                assert!(matches!(result, Err(libc::EAGAIN)));
            } else {
                let items = result.unwrap().unwrap().entries;
                assert_eq!(
                    items
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                    vec!["a", "d", "l"]
                );
                assert!(!items[0].is_dir && items[0].is_symlink == Some(false));
                assert!(items[1].is_dir && items[1].is_symlink == Some(false));
                assert!(!items[2].is_dir && items[2].is_symlink == Some(true));
            }
            join.join().unwrap();
        }
    }
}
