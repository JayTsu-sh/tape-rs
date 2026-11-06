//! Original upload identity and an independent, durable payload.
use super::{
    ClientError, PreparedUpload, Result, UploadCondition, invalid,
    upload_intent::{self, OriginalUpload},
};
use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag, openat};
use nix::sys::stat::{Mode, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, unlinkat};
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use uuid::Uuid;

pub(super) const HEADER: &str = "request.json";
pub(super) const PAYLOAD: &str = "payload";
const HEADER_LIMIT: usize = 16_384;

pub(super) fn parent(path: &Path) -> Result<(File, &OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("上传请求目录名无效"))?;
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((
        OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(directory)?,
        name,
    ))
}

pub(super) fn open_file(
    directory: &File,
    name: &OsStr,
    flags: OFlag,
    mode: Mode,
) -> std::io::Result<File> {
    let fd = openat(
        Some(directory.as_raw_fd()),
        name,
        flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        mode,
    )?;
    // SAFETY: successful openat returns a fresh owned descriptor, including
    // descriptor zero when available. File takes that ownership exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn lock(directory: File, exclusive: bool) -> std::io::Result<Flock<File>> {
    Flock::lock(
        directory,
        if exclusive {
            FlockArg::LockExclusiveNonblock
        } else {
            FlockArg::LockSharedNonblock
        },
    )
    .map_err(|(_, error)| std::io::Error::from(error))
}

pub(super) fn publish(parent: &File, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
    let from = CString::new(from.as_bytes())?;
    let to = CString::new(to.as_bytes())?;
    // SAFETY: both C strings are NUL-terminated and live through the call.
    // The borrowed parent descriptor stays open; both names are single path
    // components. Linux RENAME_NOREPLACE atomically rejects any existing entry.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_renameat2,
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            nix::libc::RENAME_NOREPLACE,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Draft {
    parent: File,
    name: OsString,
    directory: Flock<File>,
    published: bool,
}

impl Drop for Draft {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let Ok(own) = self.directory.metadata() else {
            return;
        };
        let Ok(current) = fstatat(
            Some(self.parent.as_raw_fd()),
            self.name.as_os_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) else {
            return;
        };
        if current.st_dev != own.dev() || current.st_ino != own.ino() {
            return;
        }
        // Only these two files in this owned draft can have been created.
        // Unknown entries prevent rmdir rather than being recursively deleted.
        for name in [PAYLOAD, HEADER] {
            match unlinkat(
                Some(self.directory.as_raw_fd()),
                name,
                UnlinkatFlags::NoRemoveDir,
            ) {
                Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
                // Keep the original identity when its payload cannot be removed.
                Err(_) => return,
            }
        }
        let _ = unlinkat(
            Some(self.parent.as_raw_fd()),
            self.name.as_os_str(),
            UnlinkatFlags::RemoveDir,
        );
        let _ = self.parent.sync_all();
    }
}

fn stream_original(
    source: &mut File,
    output: &mut impl Write,
    request: &OriginalUpload,
) -> Result<()> {
    let size = request.size;
    if !source.metadata()?.is_file() || source.metadata()?.len() != size {
        return Err(ClientError::Verification(
            "原正文类型或长度与保存身份不符".into(),
        ));
    }
    source.rewind()?;
    let limit = size
        .checked_add(1)
        .ok_or_else(|| invalid("原正文长度无效"))?;
    let mut input = source.take(limit);
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != size || format!("{:x}", hash.finalize()) != request.sha256 {
        return Err(ClientError::Verification(
            "原正文与保存身份的长度或 SHA-256 不符".into(),
        ));
    }
    Ok(())
}

pub(super) fn save_new(request: &OriginalUpload, path: &Path, source: &Path) -> Result<()> {
    let (parent, destination) = parent(path)?;
    save_new_in(request, parent, destination, source)
}

fn save_new_in(
    request: &OriginalUpload,
    parent: File,
    destination: &OsStr,
    source: &Path,
) -> Result<()> {
    match fstatat(
        Some(parent.as_raw_fd()),
        destination,
        AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Ok(_) => {
            return Err(ClientError::Exists(
                destination.to_string_lossy().into_owned(),
            ));
        }
        Err(nix::errno::Errno::ENOENT) => {}
        Err(error) => return Err(std::io::Error::from(error).into()),
    }
    let mut original = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(source)?;
    let bytes = upload_intent::encode(request, None);
    if bytes.len() > HEADER_LIMIT {
        return Err(invalid("上传请求超过字节上限"));
    }
    let name = OsString::from(format!(".fdb-upload-{}", Uuid::new_v4()));
    mkdirat(Some(parent.as_raw_fd()), name.as_os_str(), Mode::S_IRWXU)
        .map_err(std::io::Error::from)?;
    let directory = (|| {
        lock(
            open_file(
                &parent,
                &name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                Mode::empty(),
            )?,
            true,
        )
    })()
    .inspect_err(|_| {
        let _ = unlinkat(
            Some(parent.as_raw_fd()),
            name.as_os_str(),
            UnlinkatFlags::RemoveDir,
        );
        let _ = parent.sync_all();
    })?;
    let mut draft = Draft {
        parent,
        name,
        directory,
        published: false,
    };
    draft.parent.sync_all()?;
    let mut header = open_file(
        &draft.directory,
        OsStr::new(HEADER),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    header.write_all(&bytes)?;
    header.sync_all()?;
    // A crash during a large copy leaves the original identity available.
    // The preparation lock and payload verification still prevent admission.
    draft.directory.sync_all()?;
    let mut payload = open_file(
        &draft.directory,
        OsStr::new(PAYLOAD),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    stream_original(&mut original, &mut payload, request)?;
    payload.sync_all()?;
    draft.directory.sync_all()?;
    publish(&draft.parent, &draft.name, destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            ClientError::Exists(destination.to_string_lossy().into_owned())
        } else {
            error.into()
        }
    })?;
    draft.published = true;
    // A failed parent fsync leaves the complete published request intact.
    draft.parent.sync_all()?;
    Ok(())
}

/// Owns one validated original request, its directory descriptors and an
/// exclusive lease across prepare/upload calls. Dropping it retains disk state.
pub struct UploadJournal {
    parent: File,
    directory: Flock<File>,
    header: File,
    exclusive: bool,
    pub(super) original: OriginalUpload,
    pub(super) request: Option<PreparedUpload>,
}

impl UploadJournal {
    /// Filesystem uploads always use automatic media selection. Anchor a new
    /// request to the held journal root, even if its pathname was replaced.
    pub(crate) fn create_in(
        parent: &File,
        name: &OsStr,
        library: &str,
        pool: Uuid,
        path: &str,
        source: &Path,
        condition: UploadCondition,
    ) -> Result<Self> {
        let original = OriginalUpload::new(library, pool, path, source, condition, None)?;
        save_new_in(&original, parent.try_clone()?, name, source)?;
        let saved = Self::open_in(parent, name)?;
        if saved.original != original {
            return Err(ClientError::Verification("保存后的原上传目录被替换".into()));
        }
        Ok(saved)
    }

    pub(crate) fn open_in(parent: &File, name: &OsStr) -> Result<Self> {
        Self::open_at(parent.try_clone()?, name, true)
    }
    /// Save the original intent and independent bytes durably, without any RPC.
    pub fn create(
        library: &str,
        pool: Uuid,
        path: &str,
        source: &Path,
        condition: UploadCondition,
        barcode: Option<&str>,
        directory: &Path,
    ) -> Result<Self> {
        let original = OriginalUpload::new(library, pool, path, source, condition, barcode)?;
        save_new(&original, directory, source)?;
        let saved = Self::open_locked(directory, true)?;
        if saved.original != original {
            return Err(ClientError::Verification("保存后的原上传目录被替换".into()));
        }
        Ok(saved)
    }

    /// Reopen an existing request under an exclusive lease; never create one.
    pub fn open(directory: &Path) -> Result<Self> {
        Self::open_locked(directory, true)
    }

    pub fn library(&self) -> &str {
        &self.original.library
    }
    pub fn namespace(&self) -> Uuid {
        self.original.namespace
    }
    pub fn pool(&self) -> Uuid {
        self.original.pool
    }
    pub fn path(&self) -> &str {
        &self.original.path
    }
    pub fn operation(&self) -> Uuid {
        self.original.operation
    }
    pub fn condition(&self) -> Result<UploadCondition> {
        self.original.condition()
    }
    pub fn size(&self) -> u64 {
        self.original.size
    }
    pub fn sha256(&self) -> &str {
        &self.original.sha256
    }
    pub fn prepared(&self) -> Option<&PreparedUpload> {
        self.request.as_ref()
    }

    pub(super) fn require_exclusive(&self) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("准备和提交原上传请求需要独占目录句柄"));
        }
        self.current_header()
    }

    fn current_file(&self, name: &str, expected: &File) -> Result<()> {
        let own = expected.metadata()?;
        let current = fstatat(
            Some(self.directory.as_raw_fd()),
            name,
            AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(std::io::Error::from)?;
        if own.dev() != current.st_dev || own.ino() != current.st_ino {
            return Err(ClientError::Verification(
                "原上传目录中的文件已被替换".into(),
            ));
        }
        Ok(())
    }

    fn current_header(&self) -> Result<()> {
        self.current_file(HEADER, &self.header)?;
        let current = open_file(
            &self.directory,
            OsStr::new(HEADER),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        let own = self.header.metadata()?;
        let actual = current.metadata()?;
        if own.dev() != actual.dev()
            || own.ino() != actual.ino()
            || actual.len() > HEADER_LIMIT as u64
        {
            return Err(ClientError::Verification("原上传请求头已被替换".into()));
        }
        let mut bytes = Vec::new();
        current
            .take(HEADER_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        let expected = upload_intent::encode(&self.original, self.request.as_ref());
        if bytes.strip_suffix(b"\n").unwrap_or(&bytes)
            != expected.strip_suffix(b"\n").unwrap_or(&expected)
        {
            return Err(ClientError::Verification("原上传请求头已被修改".into()));
        }
        Ok(())
    }

    fn read_ack(&self) -> Result<Option<(File, super::CommittedUpload)>> {
        let mut file = match open_file(
            &self.directory,
            OsStr::new(super::upload_ack::ACK),
            OFlag::O_RDONLY,
            Mode::empty(),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > super::upload_ack::LIMIT as u64 {
            return Err(invalid("本地原回执不是普通文件或超过字节预算"));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(super::upload_ack::LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        let receipt = super::upload_ack::decode(&bytes, self)?;
        self.current_header()?;
        Ok(Some((file, receipt)))
    }

    pub fn acknowledged(&self) -> Result<Option<super::CommittedUpload>> {
        Ok(self.read_ack()?.map(|(_, receipt)| receipt))
    }

    pub(super) fn record_acknowledged(&mut self, receipt: &super::CommittedUpload) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("保存原提交回执需要独占请求目录锁"));
        }
        self.current_header()?;
        let bytes = super::upload_ack::encode(self, receipt)?;
        if let Some((file, previous)) = self.read_ack()? {
            if previous != *receipt {
                return Err(ClientError::Verification(
                    "原提交回执已固定，不能覆盖".into(),
                ));
            }
            file.sync_all()?;
            self.directory.sync_all()?;
            self.parent.sync_all()?;
            return Ok(());
        }
        let name = OsString::from(format!(".fdb-ack-{}", Uuid::new_v4()));
        let mut file = open_file(
            &self.directory,
            &name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.current_header()?;
            publish(&self.directory, &name, OsStr::new(super::upload_ack::ACK))?;
            self.directory.sync_all()?;
            self.parent.sync_all()?;
            Ok(())
        })();
        if result.is_err()
            && self
                .current_file(name.to_str().expect("generated ASCII name"), &file)
                .is_ok()
        {
            let _ = unlinkat(
                Some(self.directory.as_raw_fd()),
                name.as_os_str(),
                UnlinkatFlags::NoRemoveDir,
            );
        }
        result
    }

    pub(super) fn release_payload(&self) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("清理已提交正文需要独占请求目录锁"));
        }
        let (ack, _) = self
            .read_ack()?
            .ok_or_else(|| invalid("缺少持久化原完整回执，不能清理正文"))?;
        ack.sync_all()?;
        self.header.sync_all()?;
        self.directory.sync_all()?;
        self.parent.sync_all()?;
        let payload = match self.payload() {
            Ok(file) => file,
            Err(ClientError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.current_header()?;
        self.current_file(super::upload_ack::ACK, &ack)?;
        self.current_file(PAYLOAD, &payload)?;
        unlinkat(
            Some(self.directory.as_raw_fd()),
            PAYLOAD,
            UnlinkatFlags::NoRemoveDir,
        )
        .map_err(std::io::Error::from)?;
        self.directory.sync_all()?;
        self.parent.sync_all()?;
        Ok(())
    }

    /// Transition the required state file atomically. A missing state file is
    /// always corruption, never permission to prepare a fresh physical target.
    pub(super) fn record_prepared(&mut self, request: PreparedUpload) -> Result<()> {
        if !self.exclusive {
            return Err(invalid("准备状态转换需要独占请求目录锁"));
        }
        self.original.verify(&request)?;
        if self.request.is_some() {
            return Err(invalid("原上传目标已经固定，不能重新准备"));
        }
        let bytes = upload_intent::encode(&self.original, Some(&request));
        if bytes.len() > HEADER_LIMIT {
            return Err(invalid("上传请求超过字节上限"));
        }
        let name = OsString::from(format!(".fdb-prepare-{}", Uuid::new_v4()));
        let mut replacement = open_file(
            &self.directory,
            &name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        let result = (|| -> Result<()> {
            replacement.write_all(&bytes)?;
            replacement.sync_all()?;
            let expected = self.header.metadata()?;
            let current = fstatat(
                Some(self.directory.as_raw_fd()),
                HEADER,
                AtFlags::AT_SYMLINK_NOFOLLOW,
            )
            .map_err(std::io::Error::from)?;
            if expected.dev() != current.st_dev || expected.ino() != current.st_ino {
                return Err(ClientError::Verification("原上传状态文件已被替换".into()));
            }
            nix::fcntl::renameat(
                Some(self.directory.as_raw_fd()),
                name.as_os_str(),
                Some(self.directory.as_raw_fd()),
                HEADER,
            )
            .map_err(std::io::Error::from)?;
            self.directory.sync_all()?;
            self.parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            if let (Ok(own), Ok(current)) = (
                replacement.metadata(),
                fstatat(
                    Some(self.directory.as_raw_fd()),
                    name.as_os_str(),
                    AtFlags::AT_SYMLINK_NOFOLLOW,
                ),
            ) && own.dev() == current.st_dev
                && own.ino() == current.st_ino
            {
                let _ = unlinkat(
                    Some(self.directory.as_raw_fd()),
                    name.as_os_str(),
                    UnlinkatFlags::NoRemoveDir,
                );
            }
            return result;
        }
        self.header = replacement;
        self.request = Some(request);
        Ok(())
    }

    pub(super) fn open_locked(path: &Path, exclusive: bool) -> Result<Self> {
        let (parent, name) = parent(path)?;
        Self::open_at(parent, name, exclusive)
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
        if !header.metadata()?.is_file() || header.metadata()?.len() > HEADER_LIMIT as u64 {
            return Err(invalid("上传请求头过大或不是普通文件"));
        }
        let mut bytes = Vec::new();
        (&mut header)
            .take(HEADER_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > HEADER_LIMIT {
            return Err(invalid("上传请求头过大"));
        }
        let (original, request) = upload_intent::decode(&bytes)?;
        Ok(Self {
            parent,
            directory,
            header,
            exclusive,
            original,
            request,
        })
    }

    pub(super) fn payload(&self) -> Result<File> {
        let mut file = open_file(
            &self.directory,
            OsStr::new(PAYLOAD),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        stream_original(&mut file, &mut std::io::sink(), &self.original)?;
        // Resumption establishes durability again before admitting any PUT.
        file.sync_all()?;
        self.header.sync_all()?;
        self.directory.sync_all()?;
        self.parent.sync_all()?;
        file.rewind()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("fdb-upload-state-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            eprintln!("fdb_upload_state_workspace={}", path.display());
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn no_drafts(root: &Path) {
        assert!(!fs::read_dir(root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".fdb-upload-")
        }));
    }

    #[test]
    fn preparation_requires_exclusive_lock_and_preserves_original_directory_identity() {
        let root = Scratch::new();
        let source = root.0.join("source");
        let state = root.0.join("request");
        fs::write(&source, b"abc").unwrap();
        let request = super::super::tests::request();
        let original = OriginalUpload::fixture(&request);
        save_new(&original, &state, &source).unwrap();
        let before = fs::read(state.join(HEADER)).unwrap();
        let mut reader = UploadJournal::open_locked(&state, false).unwrap();
        assert!(reader.record_prepared(request.clone()).is_err());
        assert_eq!(fs::read(state.join(HEADER)).unwrap(), before);
        assert!(UploadJournal::open_locked(&state, true).is_err());
        drop(reader);
        let mut writer = UploadJournal::open_locked(&state, true).unwrap();
        assert!(UploadJournal::open_locked(&state, false).is_err());
        let moved = root.0.join("moved");
        fs::rename(&state, &moved).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(state.join(HEADER), b"foreign header").unwrap();
        writer.record_prepared(request.clone()).unwrap();
        assert_eq!(fs::read(state.join(HEADER)).unwrap(), b"foreign header");
        let ready = fs::read(moved.join(HEADER)).unwrap();
        let (saved, selected) = upload_intent::decode(&ready).unwrap();
        assert_eq!(saved, original);
        assert_eq!(selected.unwrap().value, request.value);
        assert!(writer.record_prepared(request.clone()).is_err());
        assert_eq!(fs::read(moved.join(HEADER)).unwrap(), ready);

        let another = root.0.join("another");
        save_new(&original, &another, &source).unwrap();
        let mut writer = UploadJournal::open_locked(&another, true).unwrap();
        fs::rename(another.join(HEADER), another.join("original-header")).unwrap();
        fs::write(another.join(HEADER), b"replacement").unwrap();
        assert!(matches!(
            writer.record_prepared(request),
            Err(ClientError::Verification(_))
        ));
        assert_eq!(fs::read(another.join(HEADER)).unwrap(), b"replacement");
        assert!(!fs::read_dir(&another).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".fdb-prepare-")
        }));
    }

    #[test]
    fn saving_requires_original_bytes_and_cleans_only_its_drafts() {
        let root = Scratch::new();
        let source = root.0.join("source");
        let state = root.0.join("request");
        let request = super::super::tests::request();
        for bad in [b"xy".as_slice(), b"xyz".as_slice()] {
            fs::write(&source, bad).unwrap();
            assert!(matches!(
                request.save_fixture(&state, &source),
                Err(ClientError::Verification(_))
            ));
            assert!(!state.exists());
            no_drafts(&root.0);
        }
        let directory = root.0.join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(request.save_fixture(&state, &directory).is_err());
        let fifo = root.0.join("fifo");
        nix::unistd::mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert!(request.save_fixture(&state, &fifo).is_err());
        assert!(!state.exists());
        no_drafts(&root.0);
        assert_eq!(fs::read(&source).unwrap(), b"xyz");
        assert!(directory.is_dir());
    }

    #[test]
    fn saved_payload_has_an_independent_inode_and_open_files_keep_their_identity() {
        let root = Scratch::new();
        let source = root.0.join("source");
        let state = root.0.join("request");
        fs::write(&source, b"abc").unwrap();
        super::super::tests::request()
            .save_fixture(&state, &source)
            .unwrap();
        let saved = UploadJournal::open_locked(&state, false).unwrap();
        let moved = root.0.join("moved-request");
        fs::rename(&state, &moved).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(state.join(PAYLOAD), b"xyz").unwrap();
        let mut payload = saved.payload().unwrap();
        let original = fs::metadata(&source).unwrap();
        let copy = payload.metadata().unwrap();
        assert_ne!((original.dev(), original.ino()), (copy.dev(), copy.ino()));
        assert_eq!(copy.nlink(), 1);
        fs::write(&source, b"xyz").unwrap();
        fs::remove_file(moved.join(PAYLOAD)).unwrap();
        fs::write(moved.join(PAYLOAD), b"xyz").unwrap();
        let mut bytes = Vec::new();
        payload.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        assert!(matches!(saved.payload(), Err(ClientError::Verification(_))));
    }

    #[test]
    fn request_entries_reject_symlinks_special_files_and_incomplete_saves() {
        let root = Scratch::new();
        let source = root.0.join("source");
        let state = root.0.join("request");
        fs::write(&source, b"abc").unwrap();
        super::super::tests::request()
            .save_fixture(&state, &source)
            .unwrap();
        let backup = state.join("original-header");
        fs::rename(state.join(HEADER), &backup).unwrap();
        std::os::unix::fs::symlink(&backup, state.join(HEADER)).unwrap();
        assert!(PreparedUpload::load(&state).is_err());
        fs::remove_file(state.join(HEADER)).unwrap();
        nix::unistd::mkfifo(&state.join(HEADER), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert!(PreparedUpload::load(&state).is_err());
        fs::remove_file(state.join(HEADER)).unwrap();
        fs::rename(backup, state.join(HEADER)).unwrap();
        fs::remove_file(state.join(PAYLOAD)).unwrap();
        std::os::unix::fs::symlink(&source, state.join(PAYLOAD)).unwrap();
        assert!(
            UploadJournal::open_locked(&state, false)
                .unwrap()
                .payload()
                .is_err()
        );
        let preparing = lock(File::open(&state).unwrap(), true).unwrap();
        assert!(PreparedUpload::load(&state).is_err());
        drop(preparing);
        assert!(PreparedUpload::load(&state).is_ok());
        assert_eq!(fs::read(source).unwrap(), b"abc");
    }

    #[test]
    fn concurrent_saves_publish_one_complete_identity_without_replacement() {
        let root = Scratch::new();
        let source = root.0.join("source");
        let state = root.0.join("request");
        fs::write(&source, b"abc").unwrap();
        let requests = [
            super::super::tests::request(),
            super::super::tests::request(),
        ];
        let identities = requests.each_ref().map(|r| r.value.clone());
        let barrier = Arc::new(Barrier::new(3));
        let workers = requests.map(|request| {
            let barrier = barrier.clone();
            let source = source.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                barrier.wait();
                request.save_fixture(&state, &source)
            })
        });
        barrier.wait();
        let outcomes = workers.map(|worker| worker.join().unwrap());
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(
            outcomes
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|error| matches!(error, ClientError::Exists(_)))
        );
        let saved = UploadJournal::open_locked(&state, false).unwrap();
        assert!(identities.contains(&saved.request.as_ref().unwrap().value));
        let mut bytes = Vec::new();
        saved.payload().unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        no_drafts(&root.0);
        let empty = root.0.join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(
            super::super::tests::request()
                .save_fixture(&empty, &source)
                .is_err()
        );
        assert_eq!(fs::read_dir(empty).unwrap().count(), 0);
    }
}
