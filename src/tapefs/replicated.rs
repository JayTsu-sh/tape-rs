//! FUSE client for a replicated namespace, with an independent durable original.
use super::fdb::journals::JournalStore;
use super::*;
use crate::client::fdb::upload_state::{open_file, save_body_in};
use crate::client::fdb::{FdbClient, UploadCondition};
use crate::client::{FileStat, FileVersion};
use crate::replicated_pool::{ReplicatedArchive, inode};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::{UnlinkatFlags, unlinkat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::{Read, Seek, Write};
use uuid::Uuid;

const DRAFT: &str = ".fdb-replica";
const BODY: &str = "payload";
const COMMITTED: &str = "committed";

pub struct ReplicatedBackend {
    client: FdbClient,
    pool: Uuid,
    store: Option<JournalStore>,
}

pub struct ReplicaUploadReceipt {
    pub(super) pool: Uuid,
    pub(super) input: ReplicatedArchive,
    pub(super) modified: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    pool: Uuid,
    input: ReplicatedArchive,
}

struct Journal {
    record: Record,
    dir: File,
    committed: bool,
}

fn malformed() -> ClientError {
    ClientError::Verification("多副本 FUSE 日志身份或正文不符".into())
}

fn sha_hex(sha: &[u8; 32]) -> String {
    sha.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn body(source: &mut File, mut out: impl Write, size: u64) -> crate::client::Result<[u8; 32]> {
    let meta = source.metadata()?;
    if !meta.is_file() || meta.len() != size {
        return Err(malformed());
    }
    source.rewind()?;
    let mut read = source.take(size.saturating_add(1));
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut buf = [0; 64 * 1024];
    loop {
        let len = read.read(&mut buf)?;
        if len == 0 {
            break;
        }
        count += len as u64;
        hash.update(&buf[..len]);
        out.write_all(&buf[..len])?;
    }
    if count != size {
        return Err(malformed());
    }
    Ok(hash.finalize().into())
}

impl Journal {
    fn open(store: &JournalStore, pool: Uuid, operation: Uuid) -> crate::client::Result<Self> {
        let dir = open_file(
            store.directory(),
            OsStr::new(&operation.to_string()),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )?;
        if dir.metadata()?.mode() & 0o077 != 0 {
            return Err(malformed());
        }
        let mut bytes = Vec::new();
        let header = open_file(
            &dir,
            OsStr::new("request.json"),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        if !header.metadata()?.is_file() {
            return Err(malformed());
        }
        header.take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(malformed());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
        if record.version != 1 || record.pool != pool || record.input.operation != operation {
            return Err(malformed());
        }
        let committed = match open_file(&dir, OsStr::new(COMMITTED), OFlag::O_RDONLY, Mode::empty())
        {
            Ok(marker) if marker.metadata()?.is_file() && marker.metadata()?.len() == 0 => true,
            Ok(_) => return Err(malformed()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            record,
            dir,
            committed,
        })
    }

    fn payload(&self) -> PathBuf {
        format!("/proc/self/fd/{}/{BODY}", self.dir.as_raw_fd()).into()
    }

    fn verify_payload(&self) -> crate::client::Result<()> {
        if !self.committed {
            let mut file = open_file(&self.dir, OsStr::new(BODY), OFlag::O_RDONLY, Mode::empty())?;
            if body(&mut file, std::io::sink(), self.record.input.size)? != self.record.input.sha256
            {
                return Err(malformed());
            }
        }
        Ok(())
    }

    fn create(
        store: &JournalStore,
        pool: Uuid,
        input: &ReplicatedArchive,
        source: &mut File,
    ) -> crate::client::Result<()> {
        let record = Record {
            version: 1,
            pool,
            input: input.clone(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| malformed())?;
        save_body_in(
            store.directory().try_clone()?,
            OsStr::new(&input.operation.to_string()),
            DRAFT,
            &bytes,
            |payload| {
                if body(source, payload, input.size)? != input.sha256 {
                    return Err(malformed());
                }
                Ok(())
            },
        )
    }

    fn commit(&self) -> crate::client::Result<()> {
        open_file(
            &self.dir,
            OsStr::new(COMMITTED),
            OFlag::O_WRONLY | OFlag::O_CREAT,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?
        .sync_all()?;
        self.dir.sync_all()?;
        match unlinkat(Some(self.dir.as_raw_fd()), BODY, UnlinkatFlags::NoRemoveDir) {
            Ok(()) => self.dir.sync_all()?,
            Err(nix::errno::Errno::ENOENT) => {}
            Err(err) => return Err(std::io::Error::from(err).into()),
        }
        Ok(())
    }
}

impl ReplicatedBackend {
    pub fn new(
        client: FdbClient,
        pool: Uuid,
        journal: Option<&Path>,
    ) -> crate::client::Result<Self> {
        let root = client
            .replicated_file_stat(pool, "/")?
            .ok_or(ClientError::NotFound)?;
        if root["kind"] != "directory"
            || root["id"] != pool.to_string()
            || root["file_version"] != 1
        {
            return Err(ClientError::Verification("多副本池根目录身份无效".into()));
        }
        let store = journal.map(JournalStore::open).transpose()?;
        let backend = Self {
            client,
            pool,
            store,
        };
        if let Some(store) = &backend.store {
            // Validate each published original before performing recovery writes.
            for entry in store.entries()? {
                if let Some(saved) = backend.entry(store, entry?)? {
                    saved.verify_payload()?;
                }
            }
            for entry in store.entries()? {
                if let Some(saved) = backend.entry(store, entry?)? {
                    if !saved.committed {
                        backend.resume(&saved)?;
                    } else {
                        // Complete cleanup after an interrupted marker publication.
                        saved.commit()?;
                    }
                }
            }
        }
        Ok(backend)
    }

    fn entry(
        &self,
        store: &JournalStore,
        entry: std::fs::DirEntry,
    ) -> crate::client::Result<Option<Journal>> {
        let file_name = entry.file_name();
        let name = file_name.to_str().ok_or_else(malformed)?;
        let draft = name.strip_prefix(".fdb-replica-");
        let id = Uuid::parse_str(draft.unwrap_or(name))
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == draft.unwrap_or(name))
            .ok_or_else(malformed)?;
        if !entry.file_type()?.is_dir() {
            return Err(malformed());
        }
        // Drafts are never submitted. The complete original is published atomically first.
        if draft.is_some() {
            return Ok(None);
        }
        Journal::open(store, self.pool, id).map(Some)
    }

    fn resume(&self, journal: &Journal) -> crate::client::Result<ReplicaUploadReceipt> {
        let input = &journal.record.input;
        let path = (!journal.committed).then(|| journal.payload());
        let job = self
            .client
            .archive_replicated(self.pool, input, path.as_deref())?;
        if job["status"] != "committed" {
            if matches!(job["status"].as_str(), Some("cancelling" | "cancelled")) {
                return Err(ClientError::Rejected {
                    status: 409,
                    body: job.to_string(),
                });
            }
            return Err(ClientError::Rejected {
                status: 503,
                body: format!("原归档 {} 等待副本", input.operation),
            });
        }
        if job["members"]
            .as_array()
            .is_none_or(|members| members.iter().any(|v| v["commit"].is_null()))
        {
            return Err(ClientError::Verification("多副本缺少成员提交回执".into()));
        }
        journal.commit()?;
        Ok(ReplicaUploadReceipt {
            pool: self.pool,
            input: input.clone(),
            modified: job["modified"].as_i64(),
        })
    }

    fn previous(&self, path: &str, target: UploadTarget) -> Res<Option<u64>> {
        match target {
            UploadTarget::Fdb(UploadCondition::Create { namespace, parent })
                if namespace == self.pool && parent == inode(self.pool, parent_of(path)) =>
            {
                Ok(None)
            }
            UploadTarget::Fdb(UploadCondition::Replace(FileVersion::Fdb {
                namespace,
                inode: id,
                version,
            })) if namespace == self.pool && id == inode(self.pool, path) => Ok(Some(version)),
            _ => Err(libc::ESTALE),
        }
    }
}

fn version(pool: Uuid, path: &str, version: u64) -> FileVersion {
    FileVersion::Fdb {
        namespace: pool,
        inode: inode(pool, path),
        version,
    }
}

pub(super) fn committed_stat(receipt: &ReplicaUploadReceipt) -> FileStat {
    let input = &receipt.input;
    file_stat(
        receipt.pool,
        &input.path,
        input.previous.unwrap_or(0) + 1,
        input.size,
        sha_hex(&input.sha256),
        "file",
        receipt.modified,
    )
}

fn file_stat(
    pool: Uuid,
    path: &str,
    number: u64,
    length: u64,
    digest: String,
    kind: &str,
    modified: Option<i64>,
) -> FileStat {
    FileStat {
        length,
        generation: None,
        round: None,
        barcode: String::new(),
        sha256: digest,
        version: version(pool, path, number),
        metadata: json!({
            "kind": kind,
            "modify_time": modified.and_then(|time| chrono::DateTime::from_timestamp(time, 0)).map(|time| time.to_rfc3339()),
        }),
    }
}

impl Backend for ReplicatedBackend {
    fn root_version(&self) -> Option<FileVersion> {
        Some(version(self.pool, "/", 1))
    }

    fn stat(&self, path: &str) -> Res<Option<PathStat>> {
        let Some(info) = self
            .client
            .replicated_file_stat(self.pool, path)
            .map_err(|err| errno_of(&err))?
        else {
            return Ok(None);
        };
        let kind = info["kind"].as_str().ok_or(libc::EIO)?;
        let length = info["size"].as_u64().ok_or(libc::EIO)?;
        let number = info["file_version"].as_u64().ok_or(libc::EIO)?;
        Ok(Some(PathStat {
            state: "committed".into(),
            length,
            current: Some(file_stat(
                self.pool,
                path,
                number,
                length,
                info["sha256"].as_str().unwrap_or("").into(),
                kind,
                info["modified"].as_i64(),
            )),
        }))
    }

    fn list_dir(&self, path: &str) -> Res<Option<DirectoryListing>> {
        let Some(stat) = self.stat(path)? else {
            return Ok(None);
        };
        let current = stat.current.ok_or(libc::EIO)?;
        if current.metadata["kind"] != "directory" {
            return Err(libc::ENOTDIR);
        }
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut entries = Vec::new();
        let mut after = None;
        let mut last = String::new();
        loop {
            let page = self
                .client
                .replicated_children(self.pool, path, after.as_deref())
                .map_err(|err| errno_of(&err))?;
            for row in page["items"].as_array().ok_or(libc::EIO)? {
                let child = row["path"].as_str().ok_or(libc::EIO)?;
                let name = child
                    .strip_prefix(&prefix)
                    .filter(|name| !name.is_empty() && !name.contains('/'))
                    .ok_or(libc::EIO)?;
                if name <= last.as_str() {
                    return Err(libc::EIO);
                }
                last = name.into();
                let kind = row["kind"].as_str().ok_or(libc::EIO)?;
                entries.push(DirItem {
                    name: name.into(),
                    is_dir: kind == "directory",
                    is_symlink: Some(kind == "symlink"),
                    state: "committed".into(),
                    committed_length: row["size"].as_u64(),
                    staged_length: None,
                    version: Some(version(
                        self.pool,
                        child,
                        row["file_version"].as_u64().ok_or(libc::EIO)?,
                    )),
                });
            }
            after = match &page["next"] {
                Value::Null => break,
                Value::String(next) if *next == last && Some(next) != after.as_ref() => {
                    Some(next.clone())
                }
                _ => return Err(libc::EIO),
            };
        }
        Ok(Some(DirectoryListing {
            version: Some(current.version),
            entries,
        }))
    }

    fn fetch(&self, path: &str, out: &mut File) -> Res<u64> {
        self.client
            .download_replicated_to(self.pool, path, out)
            .map_err(|err| errno_of(&err))
    }

    fn upload(
        &self,
        path: &str,
        local: &Path,
        state: &mut UploadState,
        _wait: bool,
    ) -> Res<UploadOutcome> {
        let store = self.store.as_ref().ok_or(libc::EROFS)?;
        let (device, ino) = store.identity().map_err(io_errno)?;
        let previous = self.previous(path, state.target())?;
        let operation = match state.location {
            Some((dev, inode, operation))
                if (dev, inode) == (device, ino) && state.path.as_deref() == Some(path) =>
            {
                operation
            }
            Some(_) => return Err(libc::ESTALE),
            None => Uuid::new_v4(),
        };
        let mut source = File::open(local).map_err(io_errno)?;
        let size = source.metadata().map_err(io_errno)?.len();
        let sha256 = body(&mut source, std::io::sink(), size).map_err(|err| errno_of(&err))?;
        let input = ReplicatedArchive {
            operation,
            path: path.into(),
            previous,
            size,
            sha256,
        };
        state.location = Some((device, ino, operation));
        state.path = Some(path.into());
        let journal = match Journal::open(store, self.pool, operation) {
            Err(ClientError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                Journal::create(store, self.pool, &input, &mut source)
                    .map_err(|err| errno_of(&err))?;
                Journal::open(store, self.pool, operation)
            }
            result => result,
        }
        .map_err(|err| errno_of(&err))?;
        if journal.record.input != input {
            return Err(libc::ESTALE);
        }
        // An earlier parent fsync may have reported a local error after rename.
        store.directory().sync_all().map_err(io_errno)?;
        self.resume(&journal)
            .map(UploadOutcome::Replicated)
            .map_err(|err| errno_of(&err))
    }

    fn delete(&self, _path: &str) -> Res<()> {
        Err(libc::EOPNOTSUPP)
    }
}
