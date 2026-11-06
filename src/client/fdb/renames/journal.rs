//! Durable original rename intent, held across preparation and submission.
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
const DOMAIN: &[u8] = b"tape-rs/local-rename/1";
const RECEIPT_DOMAIN: &[u8] = b"tape-rs/local-rename-receipt/1";

#[derive(Clone, Debug)]
pub(super) struct OriginalRename {
    pub library: String,
    pub spec: RenameSpec,
    pub operation: Uuid,
    pub time: String,
}
impl OriginalRename {
    fn new(library: &str, spec: &RenameSpec) -> Result<Self> {
        let original = Self {
            library: library.into(),
            spec: spec.clone(),
            operation: Uuid::new_v4(),
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };
        original.validate()?;
        Ok(original)
    }
    fn validate(&self) -> Result<()> {
        Client::new(Vec::<String>::new()).with_library(&self.library)?;
        self.spec.validate()?;
        if self.operation.is_nil() {
            return Err(invalid("rename 原 operation 无效"));
        }
        crate::ltfs::index::MetadataChange {
            modify_time: Some(self.time.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid("rename 原时间无效"))?;
        if self.input().to_string().len() > MAX_REQUEST {
            return Err(invalid("rename 原准备正文超过预算"));
        }
        Ok(())
    }
    fn value(&self) -> Value {
        serde_json::json!({"version":1,"library":self.library,"spec":self.spec.value(),"operation":self.operation.to_string(),"time":self.time})
    }
    fn parse(value: &Value) -> Result<Self> {
        if !fields(value, &["version", "library", "spec", "operation", "time"])
            || value["version"] != 1
        {
            return Err(invalid("rename 原意图字段或版本无效"));
        }
        let original = Self {
            library: value["library"]
                .as_str()
                .ok_or_else(|| invalid("rename 原带库缺失"))?
                .into(),
            spec: RenameSpec::parse(&value["spec"])?,
            operation: uuid(value, "operation")?,
            time: value["time"]
                .as_str()
                .ok_or_else(|| invalid("rename 原时间缺失"))?
                .into(),
        };
        original.validate()?;
        Ok(original)
    }
    pub(super) fn input(&self) -> Value {
        let mut value = self.spec.condition.value();
        let fields = value.as_object_mut().expect("validated condition");
        fields.insert("version".into(), 1.into());
        fields.insert("pool".into(), self.spec.pool.to_string().into());
        fields.insert("operation".into(), self.operation.to_string().into());
        fields.insert("from".into(), self.spec.from.clone().into());
        fields.insert("to".into(), self.spec.to.clone().into());
        fields.insert("no_replace".into(), self.spec.no_replace.into());
        fields.insert("time".into(), self.time.clone().into());
        value
    }
    pub(super) fn verify(&self, prepared: &PreparedRename) -> Result<()> {
        if prepared.library() != self.library
            || prepared.namespace() != self.spec.condition.namespace()?.to_string()
            || RenameSpec::from_request(prepared.request())? != self.spec
            || prepared.request()["operation"] != self.operation.to_string()
            || prepared.request()["time"] != self.time
        {
            return Err(ClientError::Verification(
                "rename 准备应答改变了持久保存的原两端条件、操作或时间".into(),
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
        return Err(invalid("rename日志超过字节预算"));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| invalid("rename日志 JSON 损坏"))?;
    if bytes.strip_suffix(b"\n").unwrap_or(bytes) != value.to_string().as_bytes()
        || !fields(&value, names)
        || value["version"] != 1
    {
        return Err(invalid("rename日志字段、版本或规范编码无效"));
    }
    let mut unsigned = value.clone();
    unsigned
        .as_object_mut()
        .expect("validated fields")
        .remove("record_digest");
    if value["record_digest"] != digest(domain, &unsigned) {
        return Err(invalid("rename日志校验摘要不符"));
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
        return Err(ClientError::Verification("原rename日志文件已被替换".into()));
    }
    Ok(())
}
fn read(file: &mut File) -> Result<Vec<u8>> {
    if !file.metadata()?.is_file() || file.metadata()?.len() > LIMIT as u64 {
        return Err(invalid("rename日志不是普通文件或超过字节预算"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(invalid("rename日志超过字节预算"));
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
pub struct RenameJournal {
    parent: File,
    directory: Flock<File>,
    header: File,
    exclusive: bool,
    pub(super) original: OriginalRename,
    pub(super) prepared: Option<PreparedRename>,
}
impl RenameJournal {
    /// Persist intent, operation and time before any RPC; no task is admitted here.
    pub fn create(library: &str, spec: &RenameSpec, directory: &Path) -> Result<Self> {
        let original = OriginalRename::new(library, spec)?;
        let (parent, name) = parent(directory)?;
        Self::create_original(original, parent, name)
    }
    pub(crate) fn create_in(
        parent: &File,
        name: &OsStr,
        library: &str,
        spec: &RenameSpec,
    ) -> Result<Self> {
        Self::create_original(
            OriginalRename::new(library, spec)?,
            parent.try_clone()?,
            name,
        )
    }
    pub(crate) fn open_in(parent: &File, name: &OsStr) -> Result<Self> {
        Self::open_at(parent.try_clone()?, name, true)
    }
    fn create_original(original: OriginalRename, parent: File, name: &OsStr) -> Result<Self> {
        match fstatat(Some(parent.as_raw_fd()), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(_) => return Err(ClientError::Exists(name.to_string_lossy().into_owned())),
            Err(nix::errno::Errno::ENOENT) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        let temporary = OsString::from(format!(".fdb-rename-{}", Uuid::new_v4()));
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
            return Err(invalid("rename日志超过字节预算"));
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
        let original = OriginalRename::parse(&value["original"])?;
        let prepared = if value["prepared"].is_null() {
            None
        } else {
            let request = PreparedRename::parse(value["prepared"].clone())?;
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
        self.original.spec.pool
    }
    pub fn namespace(&self) -> Uuid {
        self.original
            .spec
            .condition
            .namespace()
            .expect("validated original namespace")
    }
    pub fn spec(&self) -> &RenameSpec {
        &self.original.spec
    }
    pub fn operation(&self) -> Uuid {
        self.original.operation
    }
    pub fn time(&self) -> &str {
        &self.original.time
    }
    pub fn prepared(&self) -> Option<&PreparedRename> {
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
                "原rename请求头已被替换或修改".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn durable(&self) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("rename写入流程需要独占原日志租约"));
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
    pub(super) fn record_prepared(&mut self, prepared: PreparedRename) -> Result<()> {
        self.durable()?;
        self.original.verify(&prepared)?;
        if self.prepared.is_some() {
            return Err(invalid("rename请求已经准备，不能更换原请求"));
        }
        let bytes = seal(
            DOMAIN,
            serde_json::json!({"version":1,"original":self.original.value(),"prepared":prepared.value}),
        );
        let name = OsString::from(format!(".rename-prepare-{}", Uuid::new_v4()));
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
            return Err(invalid("rename终态不属于原请求头"));
        }
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("未准备的rename请求不能有终态"))?;
        let receipt = prepared.verify(value["receipt"].clone())?;
        if !matches!(
            receipt["status"].as_str(),
            Some("completed" | "rejected" | "cancelled")
        ) {
            return Err(invalid("本地rename回执不是终态"));
        }
        Ok(Some((file, receipt)))
    }
    pub(super) fn record_terminal(&self, receipt: &Value) -> Result<()> {
        self.durable()?;
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("rename请求尚未准备"))?;
        prepared.verify(receipt.clone())?;
        if !matches!(
            receipt["status"].as_str(),
            Some("completed" | "rejected" | "cancelled")
        ) {
            return Err(invalid("不能缓存未结束的rename回执"));
        }
        if let Some(previous) = self.terminal()? {
            if previous != *receipt {
                return Err(ClientError::Verification("原rename终态不能覆盖".into()));
            }
            return self.durable();
        }
        let bytes = seal(
            RECEIPT_DOMAIN,
            serde_json::json!({"version":1,"request_digest":format!("{:x}",Sha256::digest(self.header_bytes())),"receipt":receipt}),
        );
        if bytes.len() > LIMIT {
            return Err(invalid("rename终态超过字节预算"));
        }
        let name = OsString::from(format!(".rename-receipt-{}", Uuid::new_v4()));
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
}
