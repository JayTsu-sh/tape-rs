//! Durable original node intent, held across preparation and submission.
use super::*;
use crate::client::fdb::upload_state::{lock, open_file, parent, publish};
use nix::fcntl::{AtFlags, Flock, OFlag};
use nix::sys::stat::{Mode, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, unlinkat};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

const HEADER: &str = "request.json";
const RECEIPT: &str = "receipt.json";
const LIMIT: usize = 2 * MAX_REQUEST + 8192;
const DOMAIN: &[u8] = b"tape-rs/local-node/1";
const RECEIPT_DOMAIN: &[u8] = b"tape-rs/local-node-receipt/1";

#[derive(Clone, Debug)]
pub(super) struct OriginalNode {
    pub library: String,
    pub pool: Uuid,
    pub path: String,
    pub condition: NodeCondition,
    pub operation: Uuid,
    pub time: String,
    pub change: Value,
}

impl OriginalNode {
    fn new(
        library: &str,
        pool: Uuid,
        path: &str,
        condition: NodeCondition,
        change: Value,
    ) -> Result<Self> {
        let original = Self {
            library: library.into(),
            pool,
            path: path.into(),
            condition,
            operation: Uuid::new_v4(),
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            change,
        };
        original.validate()?;
        Ok(original)
    }
    fn validate(&self) -> Result<()> {
        Client::new(Vec::<String>::new()).with_library(&self.library)?;
        validate_path(&self.path)?;
        self.condition.validate(&self.change)?;
        if self.pool.is_nil() || self.operation.is_nil() {
            return Err(invalid("原节点请求池或 operation 无效"));
        }
        crate::ltfs::index::MetadataChange {
            modify_time: Some(self.time.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid("原节点请求时间无效"))?;
        if self.input().to_string().len() > MAX_REQUEST {
            return Err(invalid("节点准备正文超过 8 MiB"));
        }
        Ok(())
    }
    fn value(&self) -> Value {
        serde_json::json!({"version":1,"library":self.library,"pool":self.pool.to_string(),"path":self.path,
            "condition":self.condition.value(),"operation":self.operation.to_string(),"time":self.time,"mutation":self.change})
    }
    fn parse(value: &Value) -> Result<Self> {
        if !fields(
            value,
            &[
                "version",
                "library",
                "pool",
                "path",
                "condition",
                "operation",
                "time",
                "mutation",
            ],
        ) || value["version"] != 1
        {
            return Err(invalid("原节点意图字段或版本无效"));
        }
        let text = |key: &str| {
            value[key]
                .as_str()
                .ok_or_else(|| invalid("原节点意图缺少文本字段"))
        };
        let original = Self {
            library: text("library")?.into(),
            pool: uuid(value, "pool")?,
            path: text("path")?.into(),
            condition: NodeCondition::parse(&value["condition"])?,
            operation: uuid(value, "operation")?,
            time: text("time")?.into(),
            change: value["mutation"].clone(),
        };
        original.validate()?;
        Ok(original)
    }
    pub(super) fn input(&self) -> Value {
        serde_json::json!({"version":1,"pool":self.pool.to_string(),"operation":self.operation.to_string(),
            "path":self.path,"expected_version":self.condition.parts().expect("validated condition").expected_version,
            "time":self.time,"mutation":self.change})
    }
    pub(super) fn verify(&self, prepared: &PreparedNodeMutation) -> Result<()> {
        self.condition.verify(prepared)?;
        let input = self.input();
        if prepared.library() != self.library
            || prepared.namespace() != self.condition.parts()?.namespace.to_string()
            || [
                "pool",
                "operation",
                "path",
                "expected_version",
                "time",
                "mutation",
            ]
            .iter()
            .any(|key| prepared.request()[*key] != input[*key])
        {
            return Err(ClientError::Verification(
                "节点准备应答改变了持久保存的原请求".into(),
            ));
        }
        Ok(())
    }
}

fn digest(domain: &[u8], value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(domain)
            .chain_update(value.to_string().as_bytes())
            .finalize()
    )
}
fn seal(domain: &[u8], mut value: Value) -> Vec<u8> {
    value["record_digest"] = Value::String(digest(domain, &value));
    format!("{value}\n").into_bytes()
}
fn decode(bytes: &[u8], domain: &[u8], names: &[&str]) -> Result<Value> {
    if bytes.len() > LIMIT {
        return Err(invalid("节点日志超过字节预算"));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid("节点日志 JSON 损坏"))?;
    if bytes.strip_suffix(b"\n").unwrap_or(bytes) != value.to_string().as_bytes()
        || !fields(&value, names)
        || value["version"] != 1
    {
        return Err(invalid("节点日志字段、版本或规范编码无效"));
    }
    let mut unsigned = value.clone();
    unsigned
        .as_object_mut()
        .expect("validated fields")
        .remove("record_digest");
    if value["record_digest"] != digest(domain, &unsigned) {
        return Err(invalid("节点日志校验摘要不符"));
    }
    Ok(value)
}
fn same_file(directory: &File, name: &OsStr, file: &File) -> Result<()> {
    let own = file.metadata()?;
    let current = fstatat(
        Some(directory.as_raw_fd()),
        name,
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(std::io::Error::from)?;
    if own.dev() != current.st_dev || own.ino() != current.st_ino {
        return Err(ClientError::Verification("原节点日志文件已被替换".into()));
    }
    Ok(())
}
fn read(file: &mut File) -> Result<Vec<u8>> {
    if !file.metadata()?.is_file() || file.metadata()?.len() > LIMIT as u64 {
        return Err(invalid("节点日志不是普通文件或超过字节预算"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(invalid("节点日志超过字节预算"));
    }
    Ok(bytes)
}

struct Draft {
    parent: File,
    name: OsString,
    directory: Option<Flock<File>>,
    header: Option<File>,
    published: bool,
}
impl Drop for Draft {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        if same_file(&self.parent, &self.name, directory).is_err() {
            return;
        }
        if let Some(header) = &self.header {
            if same_file(directory, OsStr::new(HEADER), header).is_err() {
                return;
            }
            if unlinkat(
                Some(directory.as_raw_fd()),
                HEADER,
                UnlinkatFlags::NoRemoveDir,
            )
            .is_err()
            {
                return;
            }
        }
        let _ = unlinkat(
            Some(self.parent.as_raw_fd()),
            &*self.name,
            UnlinkatFlags::RemoveDir,
        );
        let _ = self.parent.sync_all();
    }
}

/// Owns an immutable original operation and an exclusive lease. Drop keeps disk state.
pub struct NodeJournal {
    parent: File,
    directory: Flock<File>,
    header: File,
    exclusive: bool,
    pub(super) original: OriginalNode,
    pub(super) prepared: Option<PreparedNodeMutation>,
}
impl NodeJournal {
    /// Persist intent, operation and time before any RPC; no task is admitted here.
    pub fn create(
        library: &str,
        pool: Uuid,
        path: &str,
        condition: NodeCondition,
        change: Value,
        directory: &Path,
    ) -> Result<Self> {
        let original = OriginalNode::new(library, pool, path, condition, change)?;
        let (parent, name) = parent(directory)?;
        Self::create_original(original, parent, name)
    }
    fn create_original(original: OriginalNode, parent: File, name: &OsStr) -> Result<Self> {
        match fstatat(Some(parent.as_raw_fd()), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(_) => return Err(ClientError::Exists(name.to_string_lossy().into_owned())),
            Err(nix::errno::Errno::ENOENT) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        let temporary = OsString::from(format!(".fdb-node-{}", Uuid::new_v4()));
        mkdirat(Some(parent.as_raw_fd()), &*temporary, Mode::S_IRWXU)
            .map_err(std::io::Error::from)?;
        let directory = lock(
            open_file(
                &parent,
                &temporary,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                Mode::empty(),
            )?,
            true,
        )?;
        let mut draft = Draft {
            parent,
            name: temporary,
            directory: Some(directory),
            header: None,
            published: false,
        };
        draft.parent.sync_all()?;
        let directory = draft.directory.as_ref().expect("held draft directory");
        draft.header = Some(open_file(
            directory,
            OsStr::new(HEADER),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?);
        let bytes = seal(
            DOMAIN,
            serde_json::json!({"version":1,"original":original.value(),"prepared":null}),
        );
        if bytes.len() > LIMIT {
            return Err(invalid("节点日志超过字节预算"));
        }
        let header = draft.header.as_mut().expect("created header");
        header.write_all(&bytes)?;
        header.sync_all()?;
        directory.sync_all()?;
        publish(&draft.parent, &draft.name, name)?;
        draft.published = true;
        draft.parent.sync_all()?;
        let header = open_file(
            directory,
            OsStr::new(HEADER),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        Ok(Self {
            parent: draft.parent.try_clone()?,
            directory: draft.directory.take().expect("transfer exclusive lease"),
            header,
            original,
            prepared: None,
            exclusive: true,
        })
    }

    pub fn open(directory: &Path) -> Result<Self> {
        let (parent, name) = parent(directory)?;
        Self::open_at(parent, name, true)
    }
    pub(crate) fn create_in(
        parent: &File,
        name: &OsStr,
        library: &str,
        pool: Uuid,
        path: &str,
        condition: NodeCondition,
        change: Value,
    ) -> Result<Self> {
        let original = OriginalNode::new(library, pool, path, condition, change)?;
        Self::create_original(original, parent.try_clone()?, name)
    }
    pub(crate) fn open_in(parent: &File, name: &OsStr) -> Result<Self> {
        Self::open_at(parent.try_clone()?, name, true)
    }
    pub(super) fn open_shared(path: &Path) -> Result<Self> {
        let (parent, name) = parent(path)?;
        Self::open_at(parent, name, false)
    }
    fn open_at(parent: File, name: &OsStr, exclusive: bool) -> Result<Self> {
        let directory = lock(
            open_file(
                &parent,
                name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                Mode::empty(),
            )?,
            exclusive,
        )?;
        let mut header = open_file(
            &directory,
            OsStr::new(HEADER),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        let value = decode(
            &read(&mut header)?,
            DOMAIN,
            &["version", "original", "prepared", "record_digest"],
        )?;
        let original = OriginalNode::parse(&value["original"])?;
        let prepared = if value["prepared"].is_null() {
            None
        } else {
            let request = PreparedNodeMutation::parse(value["prepared"].clone())?;
            original.verify(&request)?;
            Some(request)
        };
        Ok(Self {
            parent,
            directory,
            header,
            original,
            prepared,
            exclusive,
        })
    }
    pub fn library(&self) -> &str {
        &self.original.library
    }
    pub fn pool(&self) -> Uuid {
        self.original.pool
    }
    pub fn namespace(&self) -> Uuid {
        self.original
            .condition
            .parts()
            .expect("validated original")
            .namespace
    }
    pub fn path(&self) -> &str {
        &self.original.path
    }
    pub fn operation(&self) -> Uuid {
        self.original.operation
    }
    pub fn time(&self) -> &str {
        &self.original.time
    }
    pub fn condition(&self) -> NodeCondition {
        self.original.condition
    }
    pub fn change(&self) -> &Value {
        &self.original.change
    }
    pub fn prepared(&self) -> Option<&PreparedNodeMutation> {
        self.prepared.as_ref()
    }

    fn header_bytes(&self) -> Vec<u8> {
        seal(
            DOMAIN,
            serde_json::json!({"version":1,"original":self.original.value(),"prepared":self.prepared.as_ref().map(|p| &p.value)}),
        )
    }
    pub(super) fn current(&self) -> Result<()> {
        same_file(&self.directory, OsStr::new(HEADER), &self.header)?;
        let mut current = open_file(
            &self.directory,
            OsStr::new(HEADER),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        same_file(&self.directory, OsStr::new(HEADER), &current)?;
        let own = self.header.metadata()?;
        let actual = current.metadata()?;
        let bytes = read(&mut current)?;
        let expected = self.header_bytes();
        if own.dev() != actual.dev()
            || own.ino() != actual.ino()
            || bytes.strip_suffix(b"\n").unwrap_or(&bytes)
                != expected.strip_suffix(b"\n").unwrap_or(&expected)
        {
            return Err(ClientError::Verification(
                "原节点请求头已被替换或修改".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn durable(&self) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("节点写入流程需要独占原日志租约"));
        }
        self.current()?;
        self.header.sync_all()?;
        if let Some((file, _)) = self.read_terminal()? {
            file.sync_all()?;
        }
        self.directory.sync_all()?;
        self.parent.sync_all()?;
        Ok(())
    }
    pub(super) fn record_prepared(&mut self, prepared: PreparedNodeMutation) -> Result<()> {
        self.durable()?;
        self.original.verify(&prepared)?;
        if self.prepared.is_some() {
            return Err(invalid("节点请求已经准备，不能更换原请求"));
        }
        let bytes = seal(
            DOMAIN,
            serde_json::json!({"version":1,"original":self.original.value(),"prepared":prepared.value}),
        );
        let name = OsString::from(format!(".node-prepare-{}", Uuid::new_v4()));
        let mut replacement = open_file(
            &self.directory,
            &name,
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        let result = (|| -> Result<()> {
            replacement.write_all(&bytes)?;
            replacement.sync_all()?;
            self.current()?;
            nix::fcntl::renameat(
                Some(self.directory.as_raw_fd()),
                &*name,
                Some(self.directory.as_raw_fd()),
                HEADER,
            )
            .map_err(std::io::Error::from)?;
            Ok(())
        })();
        if let Err(error) = result {
            if same_file(&self.directory, &name, &replacement).is_ok() {
                let _ = unlinkat(
                    Some(self.directory.as_raw_fd()),
                    &*name,
                    UnlinkatFlags::NoRemoveDir,
                );
            }
            return Err(error);
        }
        self.header = replacement;
        self.prepared = Some(prepared);
        self.durable()
    }
    pub fn terminal(&self) -> Result<Option<Value>> {
        Ok(self.read_terminal()?.map(|(_, receipt)| receipt))
    }

    fn read_terminal(&self) -> Result<Option<(File, Value)>> {
        self.current()?;
        let mut file = match open_file(
            &self.directory,
            OsStr::new(RECEIPT),
            OFlag::O_RDONLY,
            Mode::empty(),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value = decode(
            &read(&mut file)?,
            RECEIPT_DOMAIN,
            &["version", "request_digest", "receipt", "record_digest"],
        )?;
        let expected = format!("{:x}", Sha256::digest(self.header_bytes()));
        if value["request_digest"] != expected {
            return Err(invalid("节点终态不属于原请求头"));
        }
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("未准备的节点请求不能有终态"))?;
        let receipt = prepared.verify(value["receipt"].clone())?;
        if !matches!(
            receipt["status"].as_str(),
            Some("completed" | "rejected" | "cancelled")
        ) {
            return Err(invalid("本地节点回执不是终态"));
        }
        Ok(Some((file, receipt)))
    }
    pub(super) fn record_terminal(&self, receipt: &Value) -> Result<()> {
        self.durable()?;
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("节点请求尚未准备"))?;
        prepared.verify(receipt.clone())?;
        if !matches!(
            receipt["status"].as_str(),
            Some("completed" | "rejected" | "cancelled")
        ) {
            return Err(invalid("不能缓存未结束的节点回执"));
        }
        if let Some(previous) = self.terminal()? {
            if previous != *receipt {
                return Err(ClientError::Verification("原节点终态不能覆盖".into()));
            }
            return self.durable();
        }
        let bytes = seal(
            RECEIPT_DOMAIN,
            serde_json::json!({"version":1,"request_digest":format!("{:x}",Sha256::digest(self.header_bytes())),"receipt":receipt}),
        );
        if bytes.len() > LIMIT {
            return Err(invalid("节点终态超过字节预算"));
        }
        let name = OsString::from(format!(".node-receipt-{}", Uuid::new_v4()));
        let mut file = open_file(
            &self.directory,
            &name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.current()?;
            publish(&self.directory, &name, OsStr::new(RECEIPT))?;
            self.durable()
        })();
        if result.is_err() && same_file(&self.directory, &name, &file).is_ok() {
            let _ = unlinkat(
                Some(self.directory.as_raw_fd()),
                &*name,
                UnlinkatFlags::NoRemoveDir,
            );
        }
        result
    }

    #[cfg(test)]
    pub(super) fn fixture(prepared: &PreparedNodeMutation, path: &Path) -> Result<Self> {
        let request = prepared.request();
        let namespace = uuid(request, "namespace")?;
        let condition = NodeCondition {
            parent: FileVersion::Fdb {
                namespace,
                inode: uuid(request, "parent")?,
                version: request["parent_version"].as_u64().unwrap(),
            },
            node: request["expected_version"]
                .as_u64()
                .map(|version| FileVersion::Fdb {
                    namespace,
                    inode: uuid(request, "node").unwrap(),
                    version,
                }),
        };
        let original = OriginalNode {
            library: prepared.library().into(),
            pool: uuid(request, "pool")?,
            path: request["path"].as_str().unwrap().into(),
            condition,
            operation: uuid(request, "operation")?,
            time: request["time"].as_str().unwrap().into(),
            change: request["mutation"].clone(),
        };
        original.validate()?;
        let (parent, name) = parent(path)?;
        let mut saved = Self::create_original(original, parent, name)?;
        saved.record_prepared(prepared.clone())?;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn root() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("node-journal-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        eprintln!("node_journal_workspace={}", path.display());
        path
    }
    fn condition(prepared: &PreparedNodeMutation) -> NodeCondition {
        let request = prepared.request();
        let namespace = uuid(request, "namespace").unwrap();
        NodeCondition {
            parent: FileVersion::Fdb {
                namespace,
                inode: uuid(request, "parent").unwrap(),
                version: request["parent_version"].as_u64().unwrap(),
            },
            node: None,
        }
    }
    fn create(prepared: &PreparedNodeMutation, path: &Path) -> NodeJournal {
        NodeJournal::create(
            prepared.library(),
            uuid(prepared.request(), "pool").unwrap(),
            "/child",
            condition(prepared),
            serde_json::json!({"kind":"create-directory"}),
            path,
        )
        .unwrap()
    }

    #[test]
    fn local_draft_owns_one_exclusive_lease_and_preserves_original_scope_and_time() {
        let root = root();
        let path = root.join("original");
        let prepared = super::super::tests::prepared();
        let saved = create(&prepared, &path);
        let operation = saved.operation();
        let time = saved.time().to_owned();
        assert!(saved.prepared().is_none());
        assert!(saved.terminal().unwrap().is_none());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(path.join(HEADER)).unwrap().mode() & 0o777,
            0o600
        );
        assert!(
            NodeJournal::open(&path).is_err(),
            "publication must transfer the lease without unlocking a duplicate fd"
        );
        let bytes = fs::read(path.join(HEADER)).unwrap();
        drop(saved);
        let saved = NodeJournal::open(&path).unwrap();
        assert_eq!(saved.operation(), operation);
        assert_eq!(saved.time(), time);
        assert_eq!(saved.condition(), condition(&prepared));
        drop(saved);
        assert_eq!(fs::read(path.join(HEADER)).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_header_is_canonical_checked_and_never_downgraded_or_followed() {
        let root = root();
        let path = root.join("original");
        let p = super::super::tests::prepared();
        drop(create(&p, &path));
        let header = path.join(HEADER);
        let bytes = fs::read(&header).unwrap();
        for index in 0..bytes.len() - 1 {
            let mut bad = bytes.clone();
            bad[index] ^= 1;
            fs::write(&header, &bad).unwrap();
            assert!(NodeJournal::open(&path).is_err(), "byte {index}");
        }
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        for kind in ["missing", "unknown", "version"] {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove("record_digest");
            match kind {
                "missing" => {
                    bad.as_object_mut().unwrap().remove("prepared");
                }
                "unknown" => bad["extra"] = Value::Bool(true),
                _ => bad["version"] = serde_json::json!(0),
            }
            fs::write(&header, seal(DOMAIN, bad)).unwrap();
            assert!(NodeJournal::open(&path).is_err());
        }
        fs::write(&header, &bytes[..bytes.len() / 2]).unwrap();
        assert!(NodeJournal::open(&path).is_err());
        fs::write(&header, p.value.to_string()).unwrap();
        assert!(
            NodeJournal::open(&path).is_err(),
            "old bare prepared file is not a journal"
        );
        fs::remove_file(&header).unwrap();
        nix::unistd::mkfifo(&header, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let start = Instant::now();
        assert!(NodeJournal::open(&path).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        fs::remove_file(&header).unwrap();
        File::create(&header)
            .unwrap()
            .set_len(LIMIT as u64 + 1)
            .unwrap();
        assert!(NodeJournal::open(&path).is_err());
        fs::remove_file(&header).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, &bytes).unwrap();
        std::os::unix::fs::symlink(&outside, &header).unwrap();
        assert!(NodeJournal::open(&path).is_err());
        assert_eq!(fs::read(outside).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_receipts_survive_offline_restart_but_bad_or_foreign_receipts_never_repair() {
        let root = root();
        let offline = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(20));
        for state in ["completed", "rejected", "cancelled"] {
            let p = super::super::tests::prepared();
            let path = root.join(state);
            let saved = NodeJournal::fixture(&p, &path).unwrap();
            let mut terminal = super::super::tests::receipt(&p);
            terminal["status"] = serde_json::json!(state);
            if state != "completed" {
                terminal["result"] = Value::Null;
            }
            if state == "rejected" {
                terminal["rejection_reason"] = serde_json::json!("original condition changed");
            }
            saved.record_terminal(&terminal).unwrap();
            drop(saved);
            let header = fs::read(path.join(HEADER)).unwrap();
            let receipt = fs::read(path.join(RECEIPT)).unwrap();
            assert_eq!(
                offline.node_mutation_status_saved(&path).unwrap(),
                Some(terminal.clone())
            );
            if state == "completed" {
                assert_eq!(offline.run_node_mutation_saved(&path).unwrap(), terminal);
            } else {
                assert!(matches!(
                    offline.run_node_mutation_saved(&path),
                    Err(ClientError::Rejected { status: 409, .. })
                ));
            }
            assert_eq!(offline.cancel_node_mutation_saved(&path).unwrap(), terminal);
            assert_eq!(fs::read(path.join(HEADER)).unwrap(), header);
            assert_eq!(fs::read(path.join(RECEIPT)).unwrap(), receipt);
            let saved = NodeJournal::open(&path).unwrap();
            let mut changed = terminal.clone();
            changed["operation"] = serde_json::json!(Uuid::new_v4().to_string());
            assert!(saved.record_terminal(&changed).is_err());
            drop(saved);
            let mut bad = receipt.clone();
            bad[10] ^= 1;
            fs::write(path.join(RECEIPT), &bad).unwrap();
            assert!(offline.run_node_mutation_saved(&path).is_err());
            assert_eq!(fs::read(path.join(RECEIPT)).unwrap(), bad);
            fs::write(path.join(RECEIPT), &receipt).unwrap();
            fs::remove_file(path.join(HEADER)).unwrap();
            assert!(offline.node_mutation_status_saved(&path).is_err());
            assert_eq!(fs::read(path.join(RECEIPT)).unwrap(), receipt);
        }
        let p = super::super::tests::prepared();
        let a = root.join("a");
        let saved = NodeJournal::fixture(&p, &a).unwrap();
        saved
            .record_terminal(&super::super::tests::receipt(&p))
            .unwrap();
        drop(saved);
        let b = root.join("b");
        let other = super::super::tests::prepared();
        drop(NodeJournal::fixture(&other, &b).unwrap());
        fs::copy(a.join(RECEIPT), b.join(RECEIPT)).unwrap();
        assert!(offline.node_mutation_status_saved(&b).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_lease_and_replaced_header_refuse_writes_before_rpc_and_lease_moves_threads() {
        let root = root();
        let path = root.join("original");
        let p = super::super::tests::prepared();
        drop(create(&p, &path));
        let offline = FdbClient::new(["127.0.0.1:0"]).unwrap();
        let mut shared = NodeJournal::open_shared(&path).unwrap();
        assert!(
            matches!(offline.prepare_node_journal(&mut shared),Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(offline.run_node_journal(&mut shared),Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
        );
        drop(shared);
        let saved = NodeJournal::open(&path).unwrap();
        let operation = saved.operation();
        let (send, receive) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(saved.operation()).unwrap();
            wait.recv().unwrap();
            saved
        });
        assert_eq!(receive.recv().unwrap(), operation);
        assert!(NodeJournal::open(&path).is_err());
        release.send(()).unwrap();
        let mut saved = worker.join().unwrap();
        let header = fs::read(path.join(HEADER)).unwrap();
        fs::rename(path.join(HEADER), path.join("original-header")).unwrap();
        fs::write(path.join(HEADER), &header).unwrap();
        assert!(matches!(
            offline.prepare_node_journal(&mut saved),
            Err(ClientError::Verification(_))
        ));
        assert_eq!(fs::read(path.join("original-header")).unwrap(), header);
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lost_prepare_reply_and_replaced_directory_keep_original_operation_and_terminal_receipt() {
        let root = root();
        let path = root.join("original");
        let p = super::super::tests::prepared();
        let mut saved = create(&p, &path);
        let original = saved.original.clone();
        let original_header = fs::read(path.join(HEADER)).unwrap();
        let mut answer = p.value.clone();
        answer["request"]["operation"] = serde_json::json!(original.operation.to_string());
        answer["request"]["time"] = serde_json::json!(original.time);
        let namespace = original.condition.parts().unwrap().namespace;
        let mut inode: [u8; 16] = Sha256::new()
            .chain_update(b"tape-rs/node-mutation-inode/1")
            .chain_update(namespace.as_bytes())
            .chain_update(1u64.to_be_bytes())
            .chain_update(original.operation.as_bytes())
            .finalize()[..16]
            .try_into()
            .unwrap();
        inode[6] = (inode[6] & 15) | 0x80;
        inode[8] = (inode[8] & 63) | 0x80;
        answer["request"]["node"] = serde_json::json!(Uuid::from_bytes(inode).to_string());
        answer["fingerprint"] = serde_json::json!(fingerprint(&answer["request"]));
        let prepared = PreparedNodeMutation::parse(answer.clone()).unwrap();
        let terminal = super::super::tests::receipt(&prepared);
        let expected = terminal.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let handled = count.clone();
        let saved_path = path.clone();
        let original_op = original.operation.to_string();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            for index in 0..4 {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut input = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                input.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                handled.fetch_add(1, Ordering::SeqCst);
                if index < 2 {
                    assert!(first.starts_with("POST /v1/fdb/nodes/NODE-CLIENT/prepare"));
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["operation"], original_op);
                    assert_eq!(value["time"], original.time);
                    if index == 0 {
                        let disk: Value =
                            serde_json::from_slice(&fs::read(saved_path.join(HEADER)).unwrap())
                                .unwrap();
                        assert!(disk["prepared"].is_null());
                        assert_eq!(disk["original"]["operation"], original_op);
                        continue;
                    }
                }
                let (status, value) = match index {
                    1 => ("200 OK", answer.clone()),
                    2 => {
                        assert!(first.starts_with("GET /v1/fdb/nodes/NODE-CLIENT/requests/"));
                        ("404 Not Found", serde_json::json!({}))
                    }
                    3 => {
                        assert!(first.starts_with("POST /v1/fdb/nodes/NODE-CLIENT/requests "));
                        assert_eq!(
                            serde_json::from_slice::<Value>(&body).unwrap(),
                            answer["request"]
                        );
                        ("200 OK", terminal.clone())
                    }
                    _ => unreachable!(),
                };
                let body = value.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let client = FdbClient::new([address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(1));
        assert!(client.prepare_node_journal(&mut saved).is_err());
        assert_eq!(fs::read(path.join(HEADER)).unwrap(), original_header);
        let held = root.join("held");
        fs::rename(&path, &held).unwrap();
        let foreign = create(&p, &path);
        let foreign_header = fs::read(path.join(HEADER)).unwrap();
        let foreign_op = foreign.operation();
        drop(foreign);
        assert_ne!(foreign_op, saved.operation());
        client.prepare_node_journal(&mut saved).unwrap();
        assert!(NodeJournal::open(&held).is_err());
        assert_eq!(client.run_node_journal(&mut saved).unwrap(), expected);
        worker.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert_eq!(fs::read(path.join(HEADER)).unwrap(), foreign_header);
        assert_eq!(saved.operation(), original.operation);
        drop(saved);
        let offline = FdbClient::new(["127.0.0.1:0"]).unwrap();
        assert_eq!(offline.run_node_mutation_saved(&held).unwrap(), expected);
        fs::remove_dir_all(root).unwrap();
    }
}
