//! tape-fuse 的逻辑层：把 FUSE 操作翻译成对 ltfsd 的请求。不依赖 fuser，可以脱离内核测试；
//! 挂载与 inode/回复的适配在 `src/bin/tape-fuse.rs`。
//!
//! 语义见研究笔记 file-contract.md（票据 05）。要点：
//! - 写入先落到共享本地缓存文件，支持随机写、追加和任意截断；`flush`（close）把整个文件上传到 ltfsd 的暂存区，
//!   **不是归档确认**；`fsync` 上传并等到落带，返回 0 才是已归档。
//! - 首次读从服务端下载已提交内容；本挂载的句柄共享 inode 的本地文件，写入立即可见。
//!   覆盖/截断影响已有读句柄；删除只移除名字，旧句柄继续引用旧文件。
//! - mkdir/rmdir 经后端提交到 LTFS 索引，空目录跨挂载保存。
//! - 同写入卷 rename 原子提交并保持 inode；跨卷返回 EXDEV。
//!
//! 每个操作只在短时间内持有内部锁，网络请求在锁外做：一个等落带的 `fsync` 不会挡住别的操作。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;

use nix::libc;

use crate::client::{Client, ClientError, DirItem, PathStat};

/// FUSE 回给内核的错误码（libc errno）。
pub type Errno = i32;
pub type Res<T> = std::result::Result<T, Errno>;

pub const ROOT_INO: u64 = 1;

/// 根目录下不出现在列举里的目录：墓碑与 lost+found。
const HIDDEN_AT_ROOT: &[&str] = &[".tapers", "_ltfs_lostandfound"];

/// 只读扩展属性。
pub const XATTRS: &[&str] = &["user.tape.state", "user.tape.generation", "user.tape.version", "user.tape.barcode", "user.tape.sha256"];

/// 对 ltfsd 的访问。`ClientBackend` 是实际实现；测试用内存实现。
pub trait Backend: Send + Sync {
    /// 路径状态；`None` 表示没有这个文件（可能是目录）。
    fn stat(&self, path: &str) -> Res<Option<PathStat>>;
    /// 目录的直接子项，包括在途文件；`None` 表示目录不存在。
    fn list_dir(&self, dir: &str) -> Res<Option<Vec<DirItem>>>;
    /// 把已提交的当前版本整个写进 `out`，返回字节数。
    fn fetch(&self, path: &str, out: &mut File) -> Res<u64>;
    /// 上传本地文件。`wait` 为真时等到落带；返回是否已落带。
    fn upload(&self, path: &str, local: &Path, create_only: bool, wait: bool) -> Res<bool>;
    fn delete(&self, path: &str) -> Res<()>;
    fn change_xattr(
        &self,
        _path: &str,
        _name: &str,
        _value: Option<&[u8]>,
        _flags: u32,
    ) -> Res<()> {
        Err(libc::EOPNOTSUPP)
    }
    fn rename(&self, _from: &str, _to: &str, _no_replace: bool) -> Res<()> {
        Err(libc::EXDEV)
    }
    fn symlink(&self, _target: &str, _path: &str) -> Res<()> {
        Err(libc::EOPNOTSUPP)
    }
    fn mkdir(&self, _path: &str) -> Res<()> {
        Err(libc::EOPNOTSUPP)
    }
    fn rmdir(&self, _path: &str) -> Res<()> {
        Err(libc::EOPNOTSUPP)
    }
}

/// 客户端库错误到 errno 的映射（file-contract.md "错误映射"）。
pub fn errno_of(e: &ClientError) -> Errno {
    match e {
        ClientError::NotFound => libc::ENOENT,
        ClientError::Exists(_) => libc::EEXIST,
        // 没有节点在服务：恢复中，不是不存在
        ClientError::NoLeader(_) => libc::EBUSY,
        ClientError::Io(_) => libc::EIO,
        ClientError::Indeterminate(_) => libc::EIO,
        ClientError::Rejected { status, body } => match status {
            409 => match serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .as_ref()
                .and_then(|v| v["error"].as_str())
            {
                Some("no_attribute") => libc::ENODATA,
                Some("cross_device") => libc::EXDEV,
                Some("is_directory") => libc::EISDIR,
                Some("not_directory") => libc::ENOTDIR,
                Some("not_empty") => libc::ENOTEMPTY,
                _ => libc::EBUSY,
            },
            403 => libc::EPERM,
            413 => libc::E2BIG,
            412 => libc::EEXIST,
            400 | 416 => libc::EINVAL,
            503 if body.contains("read_only") => libc::EROFS,
            503 => libc::EBUSY,
            507 if body.contains("too_large") => libc::EFBIG,
            507 => libc::ENOSPC,
            _ => libc::EIO,
        },
    }
}

/// 经客户端库访问集群。每个操作用模板的一个副本发请求，结束后把副本（连同它找到的 Leader）
/// 放回模板：并发的操作互不阻塞。
pub struct ClientBackend {
    template: Mutex<Client>,
}

impl ClientBackend {
    pub fn new(client: Client) -> Self {
        Self { template: Mutex::new(client) }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Client) -> Result<T, ClientError>) -> Res<T> {
        let mut c = self.template.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let r = f(&mut c);
        *self.template.lock().unwrap_or_else(|e| e.into_inner()) = c;
        r.map_err(|e| errno_of(&e))
    }
}

impl Backend for ClientBackend {
    fn stat(&self, path: &str) -> Res<Option<PathStat>> {
        self.with(|c| c.stat_path(path))
    }

    fn list_dir(&self, dir: &str) -> Res<Option<Vec<DirItem>>> {
        self.with(|c| match c.list_dir(dir, true) {
            Ok(v) => Ok(Some(v)),
            Err(ClientError::NotFound) => Ok(None),
            Err(e) => Err(e),
        })
    }

    fn fetch(&self, path: &str, out: &mut File) -> Res<u64> {
        self.with(|c| c.get_to(path, out))
    }

    fn upload(&self, path: &str, local: &Path, create_only: bool, wait: bool) -> Res<bool> {
        self.with(|c| c.put_file(path, local, create_only, wait).map(|o| o.committed))
    }

    fn delete(&self, path: &str) -> Res<()> {
        self.with(|c| c.delete(path))
    }
    fn rename(&self, from: &str, to: &str, no_replace: bool) -> Res<()> {
        self.with(|c| c.rename(from, to, no_replace))
    }
    fn change_xattr(&self, path: &str, name: &str, value: Option<&[u8]>, flags: u32) -> Res<()> {
        self.with(|c| match value {
            Some(v) => c.setxattr(path, name, v, flags),
            None => c.removexattr(path, name),
        })
    }
    fn symlink(&self, target: &str, path: &str) -> Res<()> {
        self.with(|c| c.symlink(target, path))
    }
    fn mkdir(&self, path: &str) -> Res<()> {
        self.with(|c| c.mkdir(path))
    }
    fn rmdir(&self, path: &str) -> Res<()> {
        self.with(|c| c.rmdir(path))
    }
}

/// 文件或目录的属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: SystemTime,
}

/// 同一 inode 的所有句柄共享一个本地文件；最后一个引用释放后清理。
struct CachedFile {
    ino: u64,
    cache: PathBuf,
    file: File,
    mtime: Mutex<SystemTime>,
    // 已下载或本挂载已上传的内容身份；写入中为 None。
    content: Mutex<Option<(u64, String)>>,
    source: Mutex<Option<crate::client::FileStat>>,
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.cache);
    }
}

enum Handle {
    Read { data: Arc<CachedFile> },
    Write {
        data: Arc<CachedFile>,
        path: String,
        len: u64,
        create_only: bool,
        dirty: bool,
        uploaded: bool,
        committed: bool,
    },
}

type FileSlot = Arc<Mutex<Weak<CachedFile>>>;

#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn get_or_assign(&mut self, path: &str) -> u64 {
        if path == "/" {
            return ROOT_INO;
        }
        if let Some(i) = self.by_path.get(path) {
            return *i;
        }
        self.next = self.next.max(ROOT_INO) + 1;
        self.by_ino.insert(self.next, path.to_string());
        self.by_path.insert(path.to_string(), self.next);
        self.next
    }

    fn path(&self, ino: u64) -> Option<String> {
        if ino == ROOT_INO {
            return Some("/".to_string());
        }
        self.by_ino.get(&ino).cloned()
    }
}

pub struct TapeFs<B: Backend> {
    backend: B,
    namespace: RwLock<()>,
    namespace_uncertain: std::sync::atomic::AtomicBool,
    cache_dir: PathBuf,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Arc<Mutex<Handle>>>>,
    files: Mutex<HashMap<u64, FileSlot>>,
    /// close 已暂存但尚未确认落带：保留本地文件，避免重开退回旧版本。
    pending: Mutex<HashMap<u64, Arc<CachedFile>>>,
    next_fh: Mutex<u64>,
    /// 以写方式打开着的路径：(句柄, 当前长度)。句柄锁在上传期间会被长时间持有，
    /// `getattr`/`readdir` 只看这张表，不去锁句柄
    writers: Mutex<HashMap<String, (u64, u64)>>,
    /// `fsync` 等落带的上限
    pub commit_timeout: Duration,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn io_errno(e: std::io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// 父目录路径 + 名字。
pub fn join(parent: &str, name: &str) -> String {
    if parent == "/" { format!("/{}", name) } else { format!("{}/{}", parent, name) }
}

fn xattr_name(key: &str) -> String {
    if key.starts_with("user.") { key.to_string() } else { format!("user.{key}") }
}

fn symlink_target(metadata: &serde_json::Value) -> Res<&str> {
    metadata["symlink"].as_str().filter(|target| !target.is_empty() && !target.contains('\0')).ok_or(libc::EIO)
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

impl<B: Backend> TapeFs<B> {
    /// 在 `cache_dir` 下建立本挂载独占的私有目录，不清空调用方提供的目录。
    pub fn new(backend: B, cache_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        let cache_dir = cache_dir.join(format!("tape-fuse-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&cache_dir)?;
        Ok(Self {
            backend,
            namespace: RwLock::new(()),
            namespace_uncertain: std::sync::atomic::AtomicBool::new(false),
            cache_dir,
            inodes: Mutex::new(Inodes::default()),
            handles: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_fh: Mutex::new(1),
            writers: Mutex::new(HashMap::new()),
            commit_timeout: Duration::from_secs(300),
        })
    }

    fn check_namespace(&self) -> Res<()> {
        if self
            .namespace_uncertain
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Err(libc::EIO)
        } else {
            Ok(())
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn path_of(&self, ino: u64) -> Res<String> {
        lock(&self.inodes).path(ino).ok_or(libc::ENOENT)
    }

    fn ino_of(&self, path: &str) -> u64 {
        lock(&self.inodes).get_or_assign(path)
    }

    fn file_slot(&self, ino: u64) -> FileSlot {
        lock(&self.files).entry(ino).or_insert_with(|| Arc::new(Mutex::new(Weak::new()))).clone()
    }

    fn cached(&self, ino: u64) -> Option<Arc<CachedFile>> {
        lock(&self.file_slot(ino)).upgrade()
    }

    fn cached_attr(data: &CachedFile) -> Res<Attr> {
        Ok(Attr {
            ino: data.ino, is_dir: false, is_symlink: false,
            size: data.file.metadata().map_err(io_errno)?.len(),
            mtime: *lock(&data.mtime),
        })
    }

    fn new_cached(&self, ino: u64, mtime: SystemTime) -> Res<Arc<CachedFile>> {
        let cache = self.cache_path("inode");
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&cache).map_err(io_errno)?;
        Ok(Arc::new(CachedFile { ino, cache, file, mtime: Mutex::new(mtime), content: Mutex::new(None), source: Mutex::new(None) }))
    }

    fn remember_content(data: &CachedFile) -> Res<()> {
        let len = data.file.metadata().map_err(io_errno)?.len();
        let digest = crate::client::sha256_file(&data.cache).map_err(io_errno)?;
        *lock(&data.content) = Some((len, digest));
        Ok(())
    }

    fn handle(&self, fh: u64) -> Res<Arc<Mutex<Handle>>> {
        lock(&self.handles).get(&fh).cloned().ok_or(libc::EBADF)
    }

    /// 本挂载里以写方式打开着的这个路径的长度。
    fn open_write_len(&self, path: &str) -> Option<u64> {
        lock(&self.writers).get(path).map(|w| w.1)
    }

    fn open_write_paths(&self) -> Vec<String> {
        lock(&self.writers).keys().cloned().collect()
    }

    fn new_fh(&self, h: Handle) -> u64 {
        self.new_shared_fh(Arc::new(Mutex::new(h)))
    }

    fn new_shared_fh(&self, h: Arc<Mutex<Handle>>) -> u64 {
        let fh = {
            let mut n = lock(&self.next_fh);
            *n += 1;
            *n
        };
        lock(&self.handles).insert(fh, h);
        fh
    }

    fn cache_path(&self, tag: &str) -> PathBuf {
        let n = {
            let mut n = lock(&self.next_fh);
            *n += 1;
            *n
        };
        self.cache_dir.join(format!("{}-{}", tag, n))
    }

    /// 路径的属性。文件大小取已提交的当前版本（读到的就是它）；只有在途版本时取已接收长度。
    pub fn attr_of(&self, path: &str) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.attr_of_inner(path)
    }

    fn attr_of_inner(&self, path: &str) -> Res<Attr> {
        if path == "/" {
            return Ok(Attr {
                mtime: UNIX_EPOCH,
                ino: ROOT_INO,
                is_dir: true, is_symlink: false,
                size: 0,
            });
        }
        if let Some(len) = self.open_write_len(path) {
            return Ok(Attr {
                mtime: UNIX_EPOCH,
                ino: self.ino_of(path),
                is_dir: false, is_symlink: false,
                size: len,
            });
        }
        if let Some(st) = self.backend.stat(path)? {
            let ino = self.ino_of(path);
            if st
                .current
                .as_ref()
                .is_some_and(|c| c.metadata["kind"] == "directory")
            {
                let mtime = st
                    .current
                    .as_ref()
                    .and_then(|c| c.metadata["modify_time"].as_str())
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(SystemTime::from)
                    .unwrap_or(UNIX_EPOCH);
                return Ok(Attr {
                    mtime,
                    ino,
                    is_dir: true, is_symlink: false,
                    size: 0,
                });
            }
            if let Some(current) = st.current.as_ref().filter(|c| c.metadata["kind"] == "symlink") {
                let target = symlink_target(&current.metadata)?;
                let mtime = current.metadata["modify_time"].as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(SystemTime::from).unwrap_or(UNIX_EPOCH);
                return Ok(Attr { ino, is_dir: false, is_symlink: true, size: target.len() as u64, mtime });
            }
            if let Some(data) = self.cached(ino) {
                return Self::cached_attr(&data);
            }
            let size = st.current.as_ref().map(|c| c.length).unwrap_or(st.length);
            let mtime = st
                .current
                .as_ref()
                .and_then(|c| c.metadata["modify_time"].as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(SystemTime::from)
                .unwrap_or(UNIX_EPOCH);
            return Ok(Attr {
                mtime,
                ino: self.ino_of(path),
                is_dir: false, is_symlink: false,
                size,
            });
        }
        if self.backend.list_dir(path)?.is_some() {
            return Ok(Attr {
                mtime: UNIX_EPOCH,
                ino: self.ino_of(path),
                is_dir: true, is_symlink: false,
                size: 0,
            });
        }
        Err(libc::ENOENT)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.lookup_inner(parent, name)
    }

    fn lookup_inner(&self, parent: u64, name: &str) -> Res<Attr> {
        let p = self.path_of(parent)?;
        self.attr_of_inner(&join(&p, name))
    }

    pub fn getattr(&self, ino: u64) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.getattr_inner(ino)
    }

    fn getattr_inner(&self, ino: u64) -> Res<Attr> {
        if let Some(data) = self.cached(ino) { return Self::cached_attr(&data); }
        let p = self.path_of(ino)?;
        let a = self.attr_of_inner(&p)?;
        if a.ino != ino { return Err(libc::ESTALE); }
        Ok(a)
    }

    /// 返回链接本身的目标，由内核负责相对路径、链、悬空链接和环的解析。
    pub fn readlink(&self, ino: u64) -> Res<Vec<u8>> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        let path = self.path_of(ino)?;
        if lock(&self.inodes).by_path.get(&path).copied() != Some(ino) { return Err(libc::ESTALE); }
        let current = self.backend.stat(&path)?.ok_or(libc::ENOENT)?.current.ok_or(libc::EBUSY)?;
        if current.metadata["kind"] != "symlink" { return Err(libc::EINVAL); }
        Ok(symlink_target(&current.metadata)?.as_bytes().to_vec())
    }

    /// 目录的子项：(名字, 是否目录, inode)。
    pub fn readdir(&self, ino: u64) -> Res<Vec<(String, bool, u64)>> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.readdir_inner(ino)
    }

    fn readdir_inner(&self, ino: u64) -> Res<Vec<(String, bool, u64)>> {
        let dir = self.path_of(ino)?;
        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        let server = self.backend.list_dir(&dir)?;
        let local_dir = dir == "/";
        if server.is_none() && !local_dir {
            return Err(libc::ENOENT);
        }
        for e in server.unwrap_or_default() {
            out.insert(e.name, e.is_dir);
        }
        for p in self.open_write_paths() {
            if parent_of(&p) == dir {
                out.entry(p.rsplit('/').next().unwrap_or_default().to_string())
                    .or_insert(false);
            }
        }
        if dir == "/" {
            for h in HIDDEN_AT_ROOT {
                out.remove(*h);
            }
        }
        Ok(out
            .into_iter()
            .map(|(n, d)| {
                let ino = self.ino_of(&join(&dir, &n));
                (n, d, ino)
            })
            .collect())
    }

    /// 新建文件并以写方式打开。`excl` 对应 `O_EXCL`：只创建。
    pub fn create(&self, parent: u64, name: &str, excl: bool) -> Res<(Attr, u64)> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.create_inner(parent, name, excl)
    }

    fn create_inner(&self, parent: u64, name: &str, excl: bool) -> Res<(Attr, u64)> {
        let path = join(&self.path_of(parent)?, name);
        if self.open_write_len(&path).is_some() {
            return Err(libc::EBUSY);
        }
        if excl && self.backend.stat(&path)?.is_some() {
            return Err(libc::EEXIST);
        }
        let fh = self.open_for_write(&path, excl, true)?;
        Ok((Attr { mtime: UNIX_EPOCH, ino: self.ino_of(&path), is_dir: false, is_symlink: false, size: 0 }, fh))
    }

    fn open_for_write(&self, path: &str, create_only: bool, truncate: bool) -> Res<u64> {
        let ino = self.ino_of(path);
        // 非截断写必须先取得完整旧内容。临时读句柄让副本存活到写句柄接管。
        let read = if !truncate && self.cached(ino).is_none() {
            Some(self.open_inner(ino, libc::O_RDONLY)?)
        } else { None };
        let result = (|| {
            let slot = self.file_slot(ino);
            let mut cached = lock(&slot);
            if lock(&self.inodes).by_path.get(path).copied() != Some(ino) { return Err(libc::ESTALE); }
            let mut writers = lock(&self.writers);
            if let Some((fh, _)) = writers.get(path) {
                let shared = self.handle(*fh)?;
                let fh = self.new_shared_fh(shared);
                drop(writers);
                drop(cached);
                if truncate && let Err(e) = self.truncate_inner(ino, 0) {
                    self.release(fh);
                    return Err(e);
                }
                return Ok(fh);
            }
            let data = match cached.upgrade() {
                Some(data) => data,
                None => self.new_cached(ino, SystemTime::now())?,
            };
            if truncate {
                data.file.set_len(0).map_err(io_errno)?;
                *lock(&data.mtime) = SystemTime::now();
                *lock(&data.content) = None;
            }
            let len = data.file.metadata().map_err(io_errno)?.len();
            *cached = Arc::downgrade(&data);
            let committed = !truncate && !lock(&self.pending).contains_key(&ino);
            let fh = self.new_fh(Handle::Write {
                data, path: path.to_string(), len, create_only,
                dirty: truncate, uploaded: !truncate, committed,
            });
            writers.insert(path.to_string(), (fh, len));
            Ok(fh)
        })();
        if let Some(fh) = read { self.release(fh); }
        result
    }

    /// 同一 inode 共享本地内容；外部修改与仍打开的旧缓存冲突时返回 ESTALE。
    pub fn open(&self, ino: u64, flags: i32) -> Res<u64> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.open_inner(ino, flags)
    }

    fn open_inner(&self, ino: u64, flags: i32) -> Res<u64> {
        let path = self.path_of(ino)?;
        if lock(&self.inodes).by_path.get(&path).copied() != Some(ino) {
            return Err(libc::ESTALE);
        }
        if self.attr_of_inner(&path)?.is_symlink { return Err(libc::ELOOP); }
        match flags & libc::O_ACCMODE {
            libc::O_RDONLY => {
                let slot = self.file_slot(ino);
                let mut cached = lock(&slot);
                if lock(&self.inodes).by_path.get(&path).copied() != Some(ino) { return Err(libc::ESTALE); }
                if let Some(data) = cached.upgrade() {
                    if self.open_write_len(&path).is_none() {
                        let st = match self.backend.stat(&path)? {
                            Some(st) => st,
                            None => { lock(&self.pending).remove(&ino); return Err(libc::ENOENT); }
                        };
                        if st.state == "committed" { lock(&self.pending).remove(&ino); }
                        let content = lock(&data.content);
                        let (len, sha) = content.as_ref().ok_or(libc::EIO)?;
                        // 在途时本挂载仍可读本地内容。已提交的其他内容不能混入同一 inode 的页缓存。
                        if st.state == "committed" && !st.current.as_ref().is_some_and(|c| c.length == *len && if c.sha256.is_empty() { lock(&data.source).as_ref() == Some(c) } else { c.sha256 == *sha }) {
                            return Err(libc::ESTALE);
                        }
                    }
                    return Ok(self.new_fh(Handle::Read { data }));
                }
                let st = self.backend.stat(&path)?.ok_or(libc::ENOENT)?;
                let current = st.current.ok_or(libc::EBUSY)?;
                let mtime = current.metadata["modify_time"].as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(SystemTime::from).unwrap_or(UNIX_EPOCH);
                let data = self.new_cached(ino, mtime)?;
                let mut out = data.file.try_clone().map_err(io_errno)?;
                self.backend.fetch(&path, &mut out)?;
                Self::remember_content(&data)?;
                // stat 与 GET 之间可能发生替换，拒绝把另一版本填入本次缓存。
                if !lock(&data.content).as_ref().is_some_and(|(len, sha)| *len == current.length && (current.sha256.is_empty() || *sha == current.sha256)) {
                    return Err(libc::EAGAIN);
                }
                if current.sha256.is_empty()
                    && self.backend.stat(&path)?.and_then(|s| s.current).as_ref() != Some(&current)
                { return Err(libc::EAGAIN); }
                *lock(&data.source) = Some(current);
                *cached = Arc::downgrade(&data);
                Ok(self.new_fh(Handle::Read { data }))
            }
            libc::O_WRONLY | libc::O_RDWR => {
                self.open_for_write(&path, false, flags & libc::O_TRUNC != 0)
            }
            _ => Err(libc::EOPNOTSUPP),
        }
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        match &mut *g {
            Handle::Read { data } | Handle::Write { data, .. } => {
                let f = &data.file;
                let mut buf = vec![0u8; size as usize];
                let mut got = 0;
                while got < buf.len() {
                    let n = f.read_at(&mut buf[got..], offset + got as u64).map_err(io_errno)?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                buf.truncate(got);
                Ok(buf)
            }
        }
    }

    /// 任意偏移写；追加的偏移由 write-through 模式的内核串行计算。
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        self.check_namespace()?;
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        self.check_namespace()?;
        match &mut *g {
            Handle::Write { path, data: shared, len, dirty, committed, .. } => {
                let file = &shared.file;
                let end = offset.checked_add(data.len() as u64).filter(|n| *n <= i64::MAX as u64).ok_or(libc::EFBIG)?;
                file.write_all_at(data, offset).map_err(io_errno)?;
                *lock(&shared.mtime) = SystemTime::now();
                *lock(&shared.content) = None;
                *len = (*len).max(end);
                if let Some(w) = lock(&self.writers).get_mut(path.as_str()) {
                    w.1 = *len;
                }
                *dirty = true;
                *committed = false;
                Ok(data.len() as u32)
            }
            Handle::Read { .. } => Err(libc::EBADF),
        }
    }

    /// 截断或扩展共享文件；没有写句柄时临时打开并上传修改。
    pub fn truncate(&self, ino: u64, size: u64) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.truncate_inner(ino, size)
    }

    fn truncate_inner(&self, ino: u64, size: u64) -> Res<Attr> {
        let path = self.path_of(ino)?;
        let writer = lock(&self.writers).get(&path).map(|w| w.0);
        if let Some(fh) = writer {
            let h = self.handle(fh)?;
            if let Handle::Write { data, len, dirty, committed, .. } = &mut *lock(&h) {
                if size == *len {
                    return Ok(Attr { mtime: UNIX_EPOCH, ino, is_dir: false, is_symlink: false, size });
                }
                if size > i64::MAX as u64 { return Err(libc::EFBIG); }
                data.file.set_len(size).map_err(io_errno)?;
                *lock(&data.mtime) = SystemTime::now();
                *lock(&data.content) = None;
                *len = size;
                *dirty = true;
                *committed = false;
                if let Some(w) = lock(&self.writers).get_mut(&path) {
                    w.1 = size;
                }
                return Self::cached_attr(data);
            }
        }
        let a = self.attr_of_inner(&path)?;
        if a.is_dir { return Err(libc::EISDIR); }
        if a.size == size { return Ok(a); }
        let fh = self.open_for_write(&path, false, false)?;
        let result = self.truncate_inner(ino, size).and_then(|a| self.flush(fh).map(|_| a));
        self.release(fh);
        result
    }

    /// close：把内容上传到暂存区。不等落带。
    pub fn flush(&self, fh: u64) -> Res<()> {
        self.check_namespace()?;
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        self.check_namespace()?;
        match &mut *g {
            Handle::Write { path, data, create_only, dirty, uploaded, committed, .. } => {
                if path.is_empty() { return data.file.sync_data().map_err(io_errno); }
                if !*dirty && *uploaded {
                    return Ok(());
                }
                data.file.sync_data().map_err(io_errno)?;
                let c = self.backend.upload(path, &data.cache, *create_only && !*uploaded, false)?;
                Self::remember_content(data)?;
                *dirty = false;
                *uploaded = true;
                *committed = c;
                if c { lock(&self.pending).remove(&data.ino); }
                else { lock(&self.pending).insert(data.ino, data.clone()); }
                Ok(())
            }
            Handle::Read { .. } => Ok(()),
        }
    }

    /// fsync：上传（如需要）并等到落带。返回 `Ok` 即截至此刻的内容已归档。
    pub fn fsync(&self, fh: u64) -> Res<()> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        self.sync_handle(&mut g)
    }

    fn sync_handle(&self, h: &mut Handle) -> Res<()> {
        self.check_namespace()?;
        let Handle::Write { path, data, create_only, dirty, uploaded, committed, .. } = h else {
            return Ok(());
        };
        if path.is_empty() { return data.file.sync_data().map_err(io_errno); }
        if *committed && !*dirty {
            return Ok(());
        }
        data.file.sync_data().map_err(io_errno)?;
        if *dirty || !*uploaded {
            let c = self.backend.upload(path, &data.cache, *create_only && !*uploaded, true)?;
            Self::remember_content(data)?;
            *dirty = false;
            *uploaded = true;
            *committed = c;
            if c { lock(&self.pending).remove(&data.ino); }
            else { lock(&self.pending).insert(data.ino, data.clone()); }
            return if c { Ok(()) } else { Err(libc::EIO) };
        }
        // 已经暂存过、内容没变：等那次上传落带，按哈希确认是这份内容
        let digest = crate::client::sha256_file(&data.cache).map_err(io_errno)?;
        let deadline = Instant::now() + self.commit_timeout;
        loop {
            match self.backend.stat(path)? {
                Some(st) if st.state == "committed" => {
                    if st.current.is_some_and(|c| c.sha256 == digest) {
                        *committed = true;
                        lock(&self.pending).remove(&data.ino);
                        return Ok(());
                    }
                    // 落带的不是这份内容（暂存在换届时作废，或被别人替换）：结果对这次写入不成立
                    return Err(libc::EIO);
                }
                // 暂存已作废，路径上什么都没有
                None => return Err(libc::EIO),
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(libc::EIO);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn release(&self, fh: u64) {
        let Ok(h) = self.handle(fh) else { return; };
        let ino = match &*lock(&h) {
            Handle::Read { data } | Handle::Write { data, .. } => data.ino,
        };
        // 与 open 的句柄复用串行，不能先删除 writers 指向的句柄再更新该表。
        let slot = self.file_slot(ino);
        let _cached = lock(&slot);
        if lock(&self.handles).remove(&fh).is_none() { return; }
        if let Handle::Write { path, .. } = &*lock(&h) {
            let mut w = lock(&self.writers);
            if w.get(path).is_some_and(|e| e.0 == fh) {
                let next = lock(&self.handles).iter().find_map(|(id, other)| Arc::ptr_eq(&h, other).then_some(*id));
                if let Some(next) = next { w.get_mut(path).unwrap().0 = next; }
                else { w.remove(path); }
            }
        }
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Res<()> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.unlink_inner(parent, name)
    }

    fn unlink_inner(&self, parent: u64, name: &str) -> Res<()> {
        let path = join(&self.path_of(parent)?, name);
        let attr = self.attr_of_inner(&path)?;
        if attr.is_dir { return Err(libc::EISDIR); }
        let slot = self.file_slot(attr.ino);
        let _cached = lock(&slot);
        if lock(&self.inodes).by_path.get(&path).copied() != Some(attr.ino) { return Err(libc::ESTALE); }
        if self.open_write_len(&path).is_some() { return Err(libc::EBUSY); }
        let old_stat = self.backend.stat(&path)?.and_then(|s| s.current);
        self.backend.delete(&path)?;
        if let Some(data) = _cached.upgrade() { *lock(&data.source) = old_stat; }
        // 旧 inode 和引用仍有效；后续同名 create 必须得到新的 inode。
        lock(&self.inodes).by_path.remove(&path);
        lock(&self.pending).remove(&attr.ino);
        Ok(())
    }

    pub fn symlink(&self, parent: u64, name: &str, target: &str) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        let path = join(&self.path_of(parent)?, name);
        match self.attr_of_inner(&path) {
            Ok(_) => return Err(libc::EEXIST),
            Err(e) if e != libc::ENOENT => return Err(e),
            Err(_) => {}
        }
        self.backend.symlink(target, &path)?;
        self.attr_of_inner(&path)
    }

    pub fn mkdir(&self, parent: u64, name: &str) -> Res<Attr> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.mkdir_inner(parent, name)
    }

    fn mkdir_inner(&self, parent: u64, name: &str) -> Res<Attr> {
        let path = join(&self.path_of(parent)?, name);
        match self.attr_of_inner(&path) {
            Ok(_) => return Err(libc::EEXIST),
            Err(e) if e != libc::ENOENT => return Err(e),
            Err(_) => {}
        }
        self.backend.mkdir(&path)?;
        self.attr_of_inner(&path)
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Res<()> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.rmdir_inner(parent, name)
    }

    fn rmdir_inner(&self, parent: u64, name: &str) -> Res<()> {
        let path = join(&self.path_of(parent)?, name);
        let a = self.attr_of_inner(&path)?;
        if !a.is_dir {
            return Err(libc::ENOTDIR);
        }
        if !self.readdir_inner(a.ino)?.is_empty() {
            return Err(libc::ENOTEMPTY);
        }
        self.backend.rmdir(&path)?;
        lock(&self.inodes).by_path.remove(&path);
        Ok(())
    }

    /// 后端提交成功后原子更新本挂载路径映射；已打开的源 inode 保持不变。
    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
        no_replace: bool,
    ) -> Res<()> {
        let _ns = self.namespace.write().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
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
        let within =
            |p: &str, root: &str| p == root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/'));
        if within(&to, &from) || within(&from, &to) {
            return Err(libc::EINVAL);
        }
        // open/create 等路径操作被 namespace 锁挡住；冻结句柄防止写入和 fsync 越过改名。
        let affected: BTreeSet<u64> = lock(&self.inodes)
            .by_path
            .iter()
            .filter(|(p, _)| within(p, &from) || within(p, &to))
            .map(|(_, i)| *i)
            .collect();
        let mut handles: Vec<_> = lock(&self.handles).values().cloned().collect();
        handles.retain(|h| match &*lock(h) {
            Handle::Read { data } | Handle::Write { data, .. } => affected.contains(&data.ino),
        });
        handles.sort_by_key(Arc::as_ptr);
        handles.dedup_by(|a, b| Arc::ptr_eq(a, b));
        let mut guards: Vec<_> = handles.iter().map(|h| lock(h)).collect();
        for h in &mut guards {
            if let Handle::Write { path, .. } = &**h
                && within(path, &from)
            {
                self.sync_handle(h)?;
            }
        }
        // close 之后的暂存上传可能没有句柄；未确认时不能让改名越过它。
        let pending: Vec<_> = lock(&self.pending)
            .iter()
            .filter(|(i, _)| affected.contains(i))
            .map(|(i, d)| (*i, d.clone()))
            .collect();
        for (ino, data) in pending {
            let p = self.path_of(ino)?;
            let st = self.backend.stat(&p)?.ok_or(libc::EBUSY)?;
            if st.state != "committed"
                || !st.current.as_ref().is_some_and(|c| {
                    lock(&data.content)
                        .as_ref()
                        .is_some_and(|(n, h)| c.length == *n && c.sha256 == *h)
                })
            {
                return Err(libc::EBUSY);
            }
            lock(&self.pending).remove(&ino);
        }
        if let Err(e) = self.backend.rename(&from, &to, no_replace) {
            if e == libc::EIO {
                self.namespace_uncertain
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            return Err(e);
        }
        for h in &guards {
            let data = match &**h {
                Handle::Read { data } | Handle::Write { data, .. } => data,
            };
            let p = self.path_of(data.ino)?;
            if within(&p, &from) {
                let new = format!("{}{}", to, &p[from.len()..]);
                *lock(&data.source) = self
                    .backend
                    .stat(&new)
                    .ok()
                    .flatten()
                    .and_then(|s| s.current);
            }
        }
        // 被替换目标的写句柄变成无名字对象，后续 flush 不能重建旧目标。
        for h in &mut guards {
            if let Handle::Write {
                path, create_only, ..
            } = &mut **h
            {
                if within(path, &from) {
                    *path = format!("{}{}", to, &path[from.len()..]);
                    *create_only = false;
                } else if within(path, &to) {
                    path.clear();
                }
            }
        }
        let mut writers = lock(&self.writers);
        let moved: Vec<_> = writers
            .iter()
            .filter(|(p, _)| within(p, &from))
            .map(|(p, v)| (format!("{}{}", to, &p[from.len()..]), *v))
            .collect();
        writers.retain(|p, _| !within(p, &from) && !within(p, &to));
        writers.extend(moved);
        drop(writers);
        let mut ino = lock(&self.inodes);
        let moved: Vec<_> = ino
            .by_path
            .iter()
            .filter(|(p, _)| within(p, &from))
            .map(|(p, i)| (format!("{}{}", to, &p[from.len()..]), *i))
            .collect();
        ino.by_path
            .retain(|p, _| !within(p, &from) && !within(p, &to));
        for (p, i) in moved {
            ino.by_ino.insert(i, p.clone());
            ino.by_path.insert(p, i);
        }
        debug_assert_eq!(ino.by_path.get(&to), Some(&source.ino));
        Ok(())
    }

    fn stat_for_inode(&self, ino: u64) -> Res<Option<PathStat>> {
        let path = self.path_of(ino)?;
        if lock(&self.inodes).by_path.get(&path).copied() == Some(ino) {
            return self.backend.stat(&path);
        }
        Ok(self.cached(ino).and_then(|data| lock(&data.source).clone()).map(|c| PathStat {
            state: "committed".into(), length: c.length, current: Some(c),
        }))
    }

    pub fn change_xattr(&self, ino: u64, name: &str, value: Option<&[u8]>, flags: u32) -> Res<()> {
        let _ns = self.namespace.write().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        crate::daemon::files::writable_xattr_key(name).map_err(|e| match e {
            crate::daemon::files::ServiceError::ProtectedAttribute(_) => libc::EPERM,
            _ => libc::EINVAL,
        })?;
        if flags > 2 || (value.is_none() && flags != 0) {
            return Err(libc::EINVAL);
        }
        if value.is_some_and(|v| v.len() > 65536) {
            return Err(libc::E2BIG);
        }
        if ino == ROOT_INO {
            return Err(libc::EOPNOTSUPP);
        }
        let path = self.path_of(ino)?;
        if lock(&self.inodes).by_path.get(&path).copied() != Some(ino) {
            return Err(libc::ENOENT);
        }
        let mut handles: Vec<_> = lock(&self.handles).values().cloned().collect();
        handles.retain(|h| match &*lock(h) {
            Handle::Read { data } | Handle::Write { data, .. } => data.ino == ino,
        });
        handles.sort_by_key(Arc::as_ptr);
        handles.dedup_by(|a, b| Arc::ptr_eq(a, b));
        let mut guards: Vec<_> = handles.iter().map(|h| lock(h)).collect();
        for h in &mut guards {
            if matches!(&**h, Handle::Write { .. }) {
                self.sync_handle(h)?;
            }
        }
        if let Some(data) = lock(&self.pending).get(&ino).cloned() {
            let st = self.backend.stat(&path)?.ok_or(libc::EBUSY)?;
            if st.state != "committed"
                || !st.current.as_ref().is_some_and(|c| {
                    lock(&data.content)
                        .as_ref()
                        .is_some_and(|(n, h)| c.length == *n && c.sha256 == *h)
                })
            {
                return Err(libc::EBUSY);
            }
        }
        if let Err(e) = self.backend.change_xattr(&path, name, value, flags) {
            if e == libc::EIO {
                self.namespace_uncertain
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            return Err(e);
        }
        if let Some(data) = self.cached(ino) {
            *lock(&data.source) = self
                .backend
                .stat(&path)
                .ok()
                .flatten()
                .and_then(|s| s.current);
        }
        Ok(())
    }

    pub fn getxattr(&self, ino: u64, name: &str) -> Res<Vec<u8>> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.getxattr_inner(ino, name)
    }

    fn getxattr_inner(&self, ino: u64, name: &str) -> Res<Vec<u8>> {
        if name == "user.tape.state"
            && let Some(data) = self.cached(ino)
            && lock(&data.content).is_none()
        { return Ok(b"local".to_vec()); }
        let st = self.stat_for_inode(ino)?.ok_or(libc::ENODATA)?;
        let cur = st.current.as_ref();
        let v = match name {
            "user.tape.state" => st.state.clone(),
            "user.tape.version" => cur.map(|c| format!("{}.{}", c.version.0, c.version.1)).ok_or(libc::ENODATA)?,
            "user.tape.generation" => cur.map(|c| c.generation.to_string()).ok_or(libc::ENODATA)?,
            "user.tape.barcode" => cur.map(|c| c.barcode.clone()).ok_or(libc::ENODATA)?,
            "user.tape.sha256" => cur.map(|c| c.sha256.clone()).ok_or(libc::ENODATA)?,
            _ => {
                let attrs = cur.and_then(|c| c.metadata["xattrs"].as_array()).ok_or(libc::ENODATA)?;
                let x = attrs.iter().find(|x| x["key"].as_str().is_some_and(|k| xattr_name(k) == name)).ok_or(libc::ENODATA)?;
                let value = x["value"].as_str().ok_or(libc::EIO)?;
                return if x["base64"].as_bool().unwrap_or(false) {
                    let encoded: Vec<u8> = value.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
                    base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| libc::EIO)
                } else { Ok(value.as_bytes().to_vec()) };
            }
        };
        Ok(v.into_bytes())
    }

    pub fn listxattr(&self, ino: u64) -> Res<Vec<u8>> {
        let _ns = self.namespace.read().unwrap_or_else(|e| e.into_inner());
        self.check_namespace()?;
        self.listxattr_inner(ino)
    }

    fn listxattr_inner(&self, ino: u64) -> Res<Vec<u8>> {
        self.getattr_inner(ino)?;
        let mut out = Vec::new();
        let mut names: BTreeSet<String> = XATTRS.iter().map(|s| s.to_string()).collect();
        if let Some(cur) = self.stat_for_inode(ino)?.and_then(|s| s.current)
            && let Some(attrs) = cur.metadata["xattrs"].as_array()
        {
            for x in attrs {
                if let Some(key) = x["key"].as_str().filter(|k| !k.is_empty() && !k.contains('\0')) {
                    names.insert(xattr_name(key));
                }
            }
        }
        for n in names {
            out.extend_from_slice(n.as_bytes());
            out.push(0);
        }
        Ok(out)
    }
}

impl<B: Backend> Drop for TapeFs<B> {
    fn drop(&mut self) {
        // 只清理由 new 创建的唯一子目录；挂载异常退出留下的其他目录不动。
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

// ---------------- 内存后端（测试用） ----------------

/// 按 file-contract.md 的语义模拟 ltfsd：暂存与已提交分开，`commit_all` 模拟一次合批落带。
#[derive(Default)]
pub struct MemBackend {
    pub state: Mutex<MemState>,
}

#[derive(Default)]
pub struct MemState {
    pub xattrs: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub committed: BTreeMap<String, Vec<u8>>,
    pub directories: BTreeSet<String>,
    pub staged: BTreeMap<String, Vec<u8>>,
    pub generation: u64,
    /// 为真时 `upload(wait = true)` 不落带（模拟换届作废）
    pub drop_waits: bool,
}

fn sha_hex(d: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(d).iter().map(|b| format!("{:02x}", b)).collect()
}

impl MemBackend {
    pub fn commit_all(&self) {
        let mut s = lock(&self.state);
        s.generation += 1;
        let staged = std::mem::take(&mut s.staged);
        s.committed.extend(staged);
    }
}

impl Backend for MemBackend {
    fn change_xattr(&self, path: &str, name: &str, value: Option<&[u8]>, flags: u32) -> Res<()> {
        let mut s = lock(&self.state);
        if !s.committed.contains_key(path) && !s.directories.contains(path) {
            return Err(libc::ENOENT);
        }
        let attrs = s.xattrs.entry(path.into()).or_default();
        if flags == 1 && attrs.contains_key(name) {
            return Err(libc::EEXIST);
        }
        if (flags == 2 || value.is_none()) && !attrs.contains_key(name) {
            return Err(libc::ENODATA);
        }
        if let Some(v) = value {
            attrs.insert(name.into(), v.to_vec());
        } else {
            attrs.remove(name);
        }
        s.generation += 1;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str, no_replace: bool) -> Res<()> {
        let mut s = lock(&self.state);
        let dir = s.directories.contains(from);
        if !dir && !s.committed.contains_key(from) {
            return Err(libc::ENOENT);
        }
        let exists = s.directories.contains(to) || s.committed.contains_key(to);
        if no_replace && exists {
            return Err(libc::EEXIST);
        }
        if exists && dir != s.directories.contains(to) {
            return Err(if dir { libc::ENOTDIR } else { libc::EISDIR });
        }
        if s.committed
            .keys()
            .chain(s.directories.iter())
            .any(|p| p.starts_with(&format!("{to}/")))
        {
            return Err(libc::ENOTEMPTY);
        }
        let moved: Vec<_> = s
            .committed
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&format!("{from}/")))
            .map(|(p, d)| (p.clone(), format!("{}{}", to, &p[from.len()..]), d.clone()))
            .collect();
        s.committed.remove(to);
        for (old, new, data) in moved {
            s.committed.remove(&old);
            s.committed.insert(new, data);
        }
        let moved: Vec<_> = s
            .directories
            .iter()
            .filter(|p| *p == from || p.starts_with(&format!("{from}/")))
            .cloned()
            .collect();
        s.directories.remove(to);
        for p in moved {
            s.directories.remove(&p);
            s.directories.insert(format!("{}{}", to, &p[from.len()..]));
        }
        let attrs: Vec<_> = s
            .xattrs
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&format!("{from}/")))
            .map(|(p, v)| (p.clone(), v.clone()))
            .collect();
        s.xattrs.remove(to);
        for (p, v) in attrs {
            s.xattrs.remove(&p);
            s.xattrs.insert(format!("{}{}", to, &p[from.len()..]), v);
        }
        s.generation += 1;
        Ok(())
    }
    fn mkdir(&self, path: &str) -> Res<()> {
        let mut s = lock(&self.state);
        if s.committed.contains_key(path) || !s.directories.insert(path.into()) {
            return Err(libc::EEXIST);
        }
        Ok(())
    }
    fn rmdir(&self, path: &str) -> Res<()> {
        let mut s = lock(&self.state);
        let prefix = format!("{}/", path);
        if s.committed
            .keys()
            .chain(s.staged.keys())
            .chain(s.directories.iter())
            .any(|p| p.starts_with(&prefix))
        {
            return Err(libc::ENOTEMPTY);
        }
        if !s.directories.remove(path) {
            return Err(libc::ENOENT);
        }
        s.xattrs.remove(path);
        Ok(())
    }
    fn stat(&self, path: &str) -> Res<Option<PathStat>> {
        let s = lock(&self.state);
        let current = s.committed.get(path).map(Vec::as_slice).or_else(|| s.directories.contains(path).then_some(&[])).map(|d| crate::client::FileStat {
            length: d.len() as u64,
            generation: s.generation,
            round: 1,
            barcode: "MEM001L8".into(),
            sha256: sha_hex(d),
            version: (1, s.generation),
            metadata: serde_json::json!({
                "kind": if s.directories.contains(path) { "directory" } else { "file" },
                "xattrs": s.xattrs.get(path).into_iter().flat_map(|a| a.iter()).map(|(k,v)|
                    serde_json::json!({"key": k.strip_prefix("user.").unwrap_or(k), "value": base64::engine::general_purpose::STANDARD.encode(v), "base64": true})
                ).collect::<Vec<_>>()
            }),
        });
        Ok(match (s.staged.get(path), current) {
            (Some(d), cur) => Some(PathStat {
                state: "staged".into(),
                length: d.len() as u64,
                current: cur,
            }),
            (None, Some(c)) => Some(PathStat {
                state: "committed".into(),
                length: c.length,
                current: Some(c),
            }),
            (None, None) => None,
        })
    }

    fn list_dir(&self, dir: &str) -> Res<Option<Vec<DirItem>>> {
        let s = lock(&self.state);
        let prefix = if dir == "/" { "/".to_string() } else { format!("{}/", dir) };
        let mut out: BTreeMap<String, DirItem> = BTreeMap::new();
        for (p, staged) in s.committed.keys().map(|p| (p, false)).chain(s.staged.keys().map(|p| (p, true))) {
            let Some(rest) = p.strip_prefix(&prefix) else { continue };
            let (name, is_dir) = match rest.split_once('/') {
                Some((d, _)) => (d.to_string(), true),
                None => (rest.to_string(), false),
            };
            out.entry(name.clone()).or_insert(DirItem {
                name,
                is_dir,
                state: if is_dir { String::new() } else if staged { "staged".into() } else { "committed".into() },
                committed_length: None,
                staged_length: None,
            });
        }
        for p in &s.directories {
            let Some(rest) = p.strip_prefix(&prefix) else { continue };
            let name = rest.split('/').next().unwrap_or_default();
            if !name.is_empty() { out.insert(name.into(), DirItem { name:name.into(), is_dir:true, state:String::new(), committed_length:None, staged_length:None }); }
        }
        if out.is_empty() && dir != "/" && !s.directories.contains(dir) { return Ok(None); }
        Ok(Some(out.into_values().collect()))
    }

    fn fetch(&self, path: &str, out: &mut File) -> Res<u64> {
        let d = lock(&self.state).committed.get(path).cloned().ok_or(libc::ENOENT)?;
        std::io::Write::write_all(out, &d).map_err(io_errno)?;
        Ok(d.len() as u64)
    }

    fn upload(&self, path: &str, local: &Path, create_only: bool, wait: bool) -> Res<bool> {
        let data = std::fs::read(local).map_err(io_errno)?;
        let mut s = lock(&self.state);
        if s.staged.contains_key(path) {
            return Err(libc::EBUSY);
        }
        if create_only && s.committed.contains_key(path) {
            return Err(libc::EEXIST);
        }
        if wait {
            if s.drop_waits {
                return Err(libc::EIO);
            }
            s.generation += 1;
            s.committed.insert(path.to_string(), data);
            Ok(true)
        } else {
            s.staged.insert(path.to_string(), data);
            Ok(false)
        }
    }

    fn delete(&self, path: &str) -> Res<()> {
        let mut s = lock(&self.state);
        if s.staged.contains_key(path) {
            return Err(libc::EBUSY);
        }
        s.committed.remove(path).ok_or(libc::ENOENT)?;
        s.xattrs.remove(path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(name: &str) -> TapeFs<MemBackend> {
        let dir = std::env::temp_dir().join(format!("tapefs-{}-{}", name, std::process::id()));
        TapeFs::new(MemBackend::default(), dir).unwrap()
    }

    fn write_file(t: &TapeFs<MemBackend>, parent: u64, name: &str, data: &[u8]) -> u64 {
        let (_, fh) = t.create(parent, name, false).unwrap();
        assert_eq!(t.write(fh, 0, data).unwrap() as usize, data.len());
        fh
    }

    /// FC04：close 之后本挂载仍可读暂存内容；fsync 返回 0 才已提交。
    #[test]
    fn close_stages_and_fsync_commits() {
        let t = fs("fc04");
        let fh = write_file(&t, ROOT_INO, "a.bin", b"hello");
        t.flush(fh).unwrap();
        t.release(fh);
        let a = t.lookup(ROOT_INO, "a.bin").unwrap();
        assert_eq!(a.size, 5);
        assert_eq!(t.getxattr(a.ino, "user.tape.state").unwrap(), b"staged");
        let local = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(local, 0, 100).unwrap(), b"hello");
        t.release(local);
        t.backend().commit_all();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 1, 100).unwrap(), b"ello");
        t.release(r);

        // fsync：上传并等落带
        let fh = write_file(&t, ROOT_INO, "b.bin", b"durable");
        t.fsync(fh).unwrap();
        assert_eq!(lock(&t.backend().state).committed.get("/b.bin").map(|v| v.as_slice()), Some(&b"durable"[..]));
        // fsync 之后再写，close 时作为新版本整体上传
        t.write(fh, 7, b"!").unwrap();
        t.flush(fh).unwrap();
        t.release(fh);
        assert_eq!(lock(&t.backend().state).staged.get("/b.bin").map(|v| v.as_slice()), Some(&b"durable!"[..]));
    }

    /// close 已暂存后再 fsync：等那次上传落带并按哈希确认；暂存作废则 EIO。
    #[test]
    fn fsync_after_close_waits_for_the_staged_upload() {
        let t = fs("fsync-after");
        let fh = write_file(&t, ROOT_INO, "c.bin", b"abc");
        t.flush(fh).unwrap();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                t.backend().commit_all();
            });
            t.fsync(fh).unwrap();
        });
        t.release(fh);

        let mut t = fs("fsync-lost");
        t.commit_timeout = Duration::from_millis(500);
        let fh = write_file(&t, ROOT_INO, "d.bin", b"xyz");
        t.flush(fh).unwrap();
        lock(&t.backend().state).staged.clear();
        assert_eq!(t.fsync(fh), Err(libc::EIO), "暂存作废");
    }

    #[test]
    fn random_writes_multiple_writers_and_resize_share_content() {
        let t = fs("le-writes");
        let w = write_file(&t, ROOT_INO, "a", b"12345");
        t.fsync(w).unwrap();
        t.release(w);
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let first = t.open(a.ino, libc::O_RDWR).unwrap();
        let second = t.open(a.ino, libc::O_WRONLY).unwrap();
        t.write(first, 1, b"XX").unwrap();
        assert_eq!(t.read(first, 0, 100).unwrap(), b"1XX45");
        t.release(first);
        t.write(second, 7, b"Z").unwrap();
        assert_eq!(t.getattr(a.ino).unwrap().size, 8);
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"1XX45\0\0Z");
        t.truncate(a.ino, 3).unwrap();
        t.truncate(a.ino, 6).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"1XX\0\0\0");
        t.fsync(second).unwrap();
        t.release(second);
        t.release(r);
        assert_eq!(lock(&t.backend().state).committed["/a"], b"1XX\0\0\0");
        t.truncate(a.ino, 2).unwrap();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"1X");
        t.release(r);
    }

    /// 只创建：O_EXCL 遇到已有文件 EEXIST。
    #[test]
    fn exclusive_create() {
        let t = fs("excl");
        let fh = write_file(&t, ROOT_INO, "a", b"1");
        t.fsync(fh).unwrap();
        t.release(fh);
        assert_eq!(t.create(ROOT_INO, "a", true).err(), Some(libc::EEXIST));
        assert!(t.create(ROOT_INO, "b", true).is_ok());
    }

    #[test]
    fn uncertain_rename_blocks_old_path_uploads() {
        struct LostReply(MemBackend);
        impl Backend for LostReply {
            fn stat(&self, p: &str) -> Res<Option<PathStat>> {
                self.0.stat(p)
            }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<DirItem>>> {
                self.0.list_dir(p)
            }
            fn fetch(&self, p: &str, f: &mut File) -> Res<u64> {
                self.0.fetch(p, f)
            }
            fn upload(&self, p: &str, f: &Path, new: bool, wait: bool) -> Res<bool> {
                self.0.upload(p, f, new, wait)
            }
            fn delete(&self, p: &str) -> Res<()> {
                self.0.delete(p)
            }
            fn rename(&self, a: &str, b: &str, n: bool) -> Res<()> {
                self.0.rename(a, b, n)?;
                Err(libc::EIO)
            }
        }
        let t = TapeFs::new(
            LostReply(MemBackend::default()),
            std::env::temp_dir().join(format!("rename-lost-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let (_, fh) = t.create(ROOT_INO, "old", true).unwrap();
        t.write(fh, 0, b"data").unwrap();
        assert_eq!(
            t.rename(ROOT_INO, "old", ROOT_INO, "new", false),
            Err(libc::EIO)
        );
        assert_eq!(t.write(fh, 0, b"bad!"), Err(libc::EIO));
        assert_eq!(t.fsync(fh), Err(libc::EIO));
        assert_eq!(t.flush(fh), Err(libc::EIO));
        assert_eq!(t.mkdir(ROOT_INO, "another").err(), Some(libc::EIO));
        assert!(t.backend.stat("/old").unwrap().is_none());
        assert!(t.backend.stat("/new").unwrap().is_some());
        assert_eq!(t.read(fh, 0, 4).unwrap(), b"data");
        t.release(fh);
    }

    #[test]
    fn uncertain_xattr_blocks_further_uploads() {
        struct LostReply(MemBackend);
        impl Backend for LostReply {
            fn stat(&self, p: &str) -> Res<Option<PathStat>> {
                self.0.stat(p)
            }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<DirItem>>> {
                self.0.list_dir(p)
            }
            fn fetch(&self, p: &str, f: &mut File) -> Res<u64> {
                self.0.fetch(p, f)
            }
            fn upload(&self, p: &str, f: &Path, new: bool, wait: bool) -> Res<bool> {
                self.0.upload(p, f, new, wait)
            }
            fn delete(&self, p: &str) -> Res<()> {
                self.0.delete(p)
            }
            fn change_xattr(&self, _p: &str, _n: &str, _v: Option<&[u8]>, _flags: u32) -> Res<()> {
                Err(libc::EIO)
            }
        }
        let t = TapeFs::new(
            LostReply(MemBackend::default()),
            std::env::temp_dir().join(format!("xattr-lost-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let (attr, fh) = t.create(ROOT_INO, "old", true).unwrap();
        t.write(fh, 0, b"data").unwrap();
        assert_eq!(
            t.change_xattr(attr.ino, "user.test", Some(b"value"), 0),
            Err(libc::EIO)
        );
        assert_eq!(t.write(fh, 0, b"bad!"), Err(libc::EIO));
        assert_eq!(t.fsync(fh), Err(libc::EIO));
        assert_eq!(t.flush(fh), Err(libc::EIO));
        assert_eq!(t.mkdir(ROOT_INO, "another").err(), Some(libc::EIO));
        assert!(t.backend.stat("/old").unwrap().is_some());
        assert_eq!(t.read(fh, 0, 4).unwrap(), b"data");
        t.release(fh);
    }

    #[test]
    fn rename_preserves_open_writers_readers_and_directory_inodes() {
        let t = fs("rename-open");
        let d = t.mkdir(ROOT_INO, "source").unwrap();
        let empty = t.mkdir(d.ino, "empty").unwrap();
        let writer = write_file(&t, d.ino, "file", b"original");
        let ino = t.lookup(d.ino, "file").unwrap().ino;
        t.rename(ROOT_INO, "source", ROOT_INO, "moved", false)
            .unwrap();
        assert_eq!(t.lookup(ROOT_INO, "moved").unwrap().ino, d.ino);
        assert_eq!(t.path_of(empty.ino).unwrap(), "/moved/empty");
        assert_eq!(t.lookup(d.ino, "file").unwrap().ino, ino);
        t.write(writer, 0, b"changed!").unwrap();
        t.fsync(writer).unwrap();
        assert!(
            !lock(&t.backend().state)
                .committed
                .contains_key("/source/file")
        );
        assert_eq!(
            lock(&t.backend().state).committed["/moved/file"],
            b"changed!"
        );
        let target_writer = write_file(&t, ROOT_INO, "target", b"old data");
        t.fsync(target_writer).unwrap();
        let target_ino = t.lookup(ROOT_INO, "target").unwrap().ino;
        let reader = t.open(target_ino, libc::O_RDONLY).unwrap();
        assert_eq!(
            t.rename(d.ino, "file", ROOT_INO, "target", true),
            Err(libc::EEXIST)
        );
        t.rename(d.ino, "file", ROOT_INO, "target", false).unwrap();
        assert_eq!(t.lookup(ROOT_INO, "target").unwrap().ino, ino);
        t.write(target_writer, 0, b"detached").unwrap();
        t.fsync(target_writer).unwrap();
        t.flush(target_writer).unwrap();
        assert_eq!(t.read(reader, 0, 8).unwrap(), b"detached");
        assert_eq!(lock(&t.backend().state).committed["/target"], b"changed!");
        t.write(writer, 0, b"new name").unwrap();
        t.fsync(writer).unwrap();
        assert_eq!(lock(&t.backend().state).committed["/target"], b"new name");
        for fh in [writer, target_writer, reader] {
            t.release(fh);
        }
    }

    /// FC11：目录交给后端保存，rmdir 只对空目录。
    #[test]
    fn directories_and_rename() {
        let t = fs("dirs");
        let d = t.mkdir(ROOT_INO, "d").unwrap();
        assert_eq!(t.mkdir(ROOT_INO, "d").err(), Some(libc::EEXIST));
        assert!(t.readdir(d.ino).unwrap().is_empty());
        let fh = write_file(&t, d.ino, "f", b"z");
        // 写着的文件在列举里可见
        assert_eq!(t.readdir(d.ino).unwrap().iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), vec!["f"]);
        assert_eq!(t.rmdir(ROOT_INO, "d"), Err(libc::ENOTEMPTY));
        t.fsync(fh).unwrap();
        t.release(fh);
        // 服务端也有了这个目录
        let names: Vec<_> = t.readdir(ROOT_INO).unwrap().into_iter().map(|e| (e.0, e.1)).collect();
        assert_eq!(names, vec![("d".to_string(), true)]);
        let e = t.mkdir(d.ino, "empty").unwrap();
        assert_eq!(t.lookup(d.ino, "empty").unwrap().ino, e.ino);
        t.rmdir(d.ino, "empty").unwrap();
        assert_eq!(t.lookup(d.ino, "empty").err(), Some(libc::ENOENT));
        assert_eq!(t.rename(ROOT_INO, "absent", ROOT_INO, "new", false), Err(libc::ENOENT));
        // 根目录下的墓碑与 lost+found 不出现
        lock(&t.backend().state).committed.insert("/.tapers/deleted/ab".into(), Vec::new());
        assert!(t.readdir(ROOT_INO).unwrap().iter().all(|e| e.0 != ".tapers"));
    }

    #[test]
    fn unlink_rules() {
        let t = fs("unlink");
        let fh = write_file(&t, ROOT_INO, "a", b"1");
        assert_eq!(t.unlink(ROOT_INO, "a"), Err(libc::EBUSY), "正在写");
        t.fsync(fh).unwrap();
        t.release(fh);
        t.unlink(ROOT_INO, "a").unwrap();
        assert_eq!(t.lookup(ROOT_INO, "a").err(), Some(libc::ENOENT));
        assert_eq!(t.unlink(ROOT_INO, "a"), Err(libc::ENOENT));
    }

    #[test]
    fn readers_share_overwrite_but_unlink_recreate_has_new_inode() {
        let t = fs("read-snapshot");
        let w = write_file(&t, ROOT_INO, "a", b"old");
        t.fsync(w).unwrap();
        t.release(w);
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        let w = t.open(a.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        t.write(w, 0, b"new").unwrap();
        t.fsync(w).unwrap();
        t.release(w);
        t.unlink(ROOT_INO, "a").unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"new");
        let w = write_file(&t, ROOT_INO, "a", b"reborn");
        t.fsync(w).unwrap();
        t.release(w);
        let new = t.lookup(ROOT_INO, "a").unwrap();
        assert_ne!(new.ino, a.ino);
        assert_eq!(t.getxattr(a.ino, "user.tape.sha256").unwrap(), sha_hex(b"new").as_bytes());
        assert_eq!(t.getattr(a.ino).unwrap().size, 3);
        assert_eq!(t.read(r, 0, 100).unwrap(), b"new");
        t.release(r);
    }

    #[test]
    fn local_writer_is_visible_before_upload_and_truncates_existing_readers() {
        let t = fs("local-visibility");
        let w = write_file(&t, ROOT_INO, "a", b"before close");
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"before close");
        assert_eq!(t.getxattr(a.ino, "user.tape.state").unwrap(), b"local");
        t.fsync(w).unwrap();
        t.release(w);
        let w = t.open(a.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"");
        assert_eq!(t.getattr(a.ino).unwrap().size, 0);
        t.write(w, 0, b"short").unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"short");
        t.fsync(w).unwrap();
        t.release(w);
        t.release(r);
    }

    #[test]
    fn remote_replace_rejects_mixing_live_cache_then_refreshes_after_close() {
        let t = fs("remote-change");
        lock(&t.backend().state).committed.insert("/a".into(), b"old".to_vec());
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        lock(&t.backend().state).committed.insert("/a".into(), b"external".to_vec());
        assert_eq!(t.open(a.ino, libc::O_RDONLY), Err(libc::ESTALE));
        assert_eq!(t.read(r, 0, 100).unwrap(), b"old");
        t.release(r);
        let a = t.lookup(ROOT_INO, "a").unwrap();
        assert_eq!(a.size, 8);
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"external");
        t.release(r);
    }

    #[test]
    fn superseded_staging_does_not_pin_stale_cache_forever() {
        let t = fs("superseded-stage");
        let w = write_file(&t, ROOT_INO, "a", b"local");
        t.flush(w).unwrap();
        t.release(w);
        t.backend().commit_all();
        lock(&t.backend().state).committed.insert("/a".into(), b"remote".to_vec());
        let a = t.lookup(ROOT_INO, "a").unwrap();
        assert_eq!(t.open(a.ino, libc::O_RDONLY), Err(libc::ESTALE));
        assert!(lock(&t.pending).is_empty());
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"remote");
        t.release(r);
    }

    #[test]
    fn local_resize_can_reopen_without_another_lookup() {
        let t = fs("local-reopen");
        lock(&t.backend().state).committed.insert("/a".into(), b"old".to_vec());
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let w = t.open(a.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        t.write(w, 0, b"longer").unwrap();
        t.fsync(w).unwrap();
        t.release(w);
        let r = t.open(a.ino, libc::O_RDONLY).unwrap();
        assert_eq!(t.read(r, 0, 100).unwrap(), b"longer");
        t.release(r);
    }

    #[test]
    fn open_refreshes_size_changed_since_last_attributes() {
        for replacement in [b"longer".as_slice(), b"x".as_slice()] {
            let t = fs("lookup-open-size");
            lock(&t.backend().state).committed.insert("/a".into(), b"old".to_vec());
            let a = t.lookup(ROOT_INO, "a").unwrap();
            lock(&t.backend().state).committed.insert("/a".into(), replacement.to_vec());
            let r = t.open(a.ino, libc::O_RDONLY).unwrap();
            assert_eq!(t.getattr(a.ino).unwrap().size, replacement.len() as u64);
            assert_eq!(t.read(r, 0, 100).unwrap(), replacement);
            t.release(r);
        }
    }

    #[test]
    fn download_rejects_content_changed_between_stat_and_fetch() {
        struct ChangedOnFetch(MemBackend);
        impl Backend for ChangedOnFetch {
            fn stat(&self, p: &str) -> Res<Option<PathStat>> { self.0.stat(p) }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<DirItem>>> { self.0.list_dir(p) }
            fn fetch(&self, p: &str, f: &mut File) -> Res<u64> {
                lock(&self.0.state).committed.insert(p.into(), b"new".to_vec());
                self.0.fetch(p, f)
            }
            fn upload(&self, p: &str, f: &Path, c: bool, w: bool) -> Res<bool> { self.0.upload(p, f, c, w) }
            fn delete(&self, p: &str) -> Res<()> { self.0.delete(p) }
        }
        let backend = MemBackend::default();
        lock(&backend.state).committed.insert("/a".into(), b"old".to_vec());
        let root = std::env::temp_dir().join(format!("tape-download-race-{}", uuid::Uuid::new_v4()));
        let t = TapeFs::new(ChangedOnFetch(backend), root.clone()).unwrap();
        let a = t.lookup(ROOT_INO, "a").unwrap();
        assert_eq!(t.open(a.ino, libc::O_RDONLY), Err(libc::EAGAIN));
        assert_eq!(std::fs::read_dir(&t.cache_dir).unwrap().count(), 0);
        drop(t);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn index_metadata_reaches_attributes_and_binary_xattrs() {
        struct MetadataBackend(MemBackend);
        impl Backend for MetadataBackend {
            fn stat(&self, p: &str) -> Res<Option<PathStat>> {
                let mut st = self.0.stat(p)?;
                if let Some(c) = st.as_mut().and_then(|s| s.current.as_mut()) {
                    c.version = (7, 12);
                    c.metadata = serde_json::json!({"modify_time": "2026-09-24T01:02:03.123456789Z", "xattrs": [
                        {"key": "note", "value": "中文", "base64": false},
                        {"key": "user.binary", "value": "AAEC /w==", "base64": true},
                        {"key": "bad", "value": "!", "base64": true},
                        {"key": "tape.version", "value": "伪造", "base64": false}
                    ]});
                }
                Ok(st)
            }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<DirItem>>> { self.0.list_dir(p) }
            fn fetch(&self, p: &str, f: &mut File) -> Res<u64> { self.0.fetch(p, f) }
            fn upload(&self, p: &str, f: &Path, c: bool, w: bool) -> Res<bool> { self.0.upload(p, f, c, w) }
            fn delete(&self, p: &str) -> Res<()> { self.0.delete(p) }
        }
        let backend = MemBackend::default();
        lock(&backend.state).committed.insert("/a".into(), b"data".to_vec());
        let root = std::env::temp_dir().join(format!("tape-meta-{}", uuid::Uuid::new_v4()));
        let t = TapeFs::new(MetadataBackend(backend), root.clone()).unwrap();
        let a = t.lookup(ROOT_INO, "a").unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-09-24T01:02:03.123456789Z").unwrap();
        assert_eq!(a.mtime, SystemTime::from(expected));
        assert_eq!(t.getxattr(a.ino, "user.tape.version").unwrap(), b"7.12");
        assert_eq!(t.getxattr(a.ino, "user.note").unwrap(), "中文".as_bytes());
        assert_eq!(t.getxattr(a.ino, "user.binary").unwrap(), [0, 1, 2, 255]);
        assert_eq!(t.getxattr(a.ino, "user.bad"), Err(libc::EIO));
        let names = t.listxattr(a.ino).unwrap();
        assert!(names.split(|b| *b == 0).any(|n| n == b"user.binary"));
        assert_eq!(names.split(|b| *b == 0).filter(|n| *n == b"user.tape.version").count(), 1);
        drop(t);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn errno_mapping() {
        let r = |status: u16, body: &str| errno_of(&ClientError::Rejected { status, body: body.into() });
        assert_eq!(r(409, "path_busy"), libc::EBUSY);
        assert_eq!(r(409, r#"{"error":"is_directory"}"#), libc::EISDIR);
        assert_eq!(r(409, r#"{"error":"not_directory"}"#), libc::ENOTDIR);
        assert_eq!(r(409, r#"{"error":"not_empty"}"#), libc::ENOTEMPTY);
        assert_eq!(r(409, r#"{"error":"path_busy","path":"/is_directory"}"#), libc::EBUSY);
        assert_eq!(r(412, "exists"), libc::EEXIST);
        assert_eq!(r(503, "read_only"), libc::EROFS);
        assert_eq!(r(503, "not_serving"), libc::EBUSY);
        assert_eq!(r(507, "too_large"), libc::EFBIG);
        assert_eq!(r(507, "no_tape"), libc::ENOSPC);
        assert_eq!(errno_of(&ClientError::NoLeader(String::new())), libc::EBUSY, "恢复中不是不存在");
        assert_eq!(errno_of(&ClientError::Indeterminate(String::new())), libc::EIO);
    }

    #[test]
    fn cache_directory_is_private_and_preserves_existing_files() {
        let dir = std::env::temp_dir().join(format!("tapefs-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let sentinel = dir.join("keep");
        std::fs::write(&sentinel, b"existing").unwrap();
        let a = TapeFs::new(MemBackend::default(), dir.clone()).unwrap();
        let b = TapeFs::new(MemBackend::default(), dir.clone()).unwrap();
        assert_ne!(a.cache_dir, b.cache_dir);
        let child = a.cache_dir.clone();
        drop(a);
        assert!(!child.exists());
        assert!(b.cache_dir.is_dir());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing");
        drop(b);
        std::fs::remove_file(sentinel).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
