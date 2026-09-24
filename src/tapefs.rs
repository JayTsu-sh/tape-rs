//! tape-fuse 的逻辑层：把 FUSE 操作翻译成对 ltfsd 的请求。不依赖 fuser，可以脱离内核测试；
//! 挂载与 inode/回复的适配在 `src/bin/tape-fuse.rs`。
//!
//! 语义见研究笔记 file-contract.md（票据 05）。要点：
//! - 写入先落到本地缓存文件，只允许顺序写；`flush`（close）把整个文件上传到 ltfsd 的暂存区，
//!   **不是归档确认**；`fsync` 上传并等到落带，返回 0 才是已归档。
//! - 读只服务已提交的当前版本；只有在途版本的路径读返回 `EBUSY`。第一次读时把整个文件
//!   拉到本地缓存，之后从缓存读（磁带随机定位很慢，按块向服务端要不划算）。
//! - 目录是隐式的；`mkdir` 只在本挂载的内存里生效，直到有文件写进去。
//! - `rename` 返回 `EXDEV`，`mv` 会自动退回复制 + 删除。
//!
//! 每个操作只在短时间内持有内部锁，网络请求在锁外做：一个等落带的 `fsync` 不会挡住别的操作。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nix::libc;

use crate::client::{Client, ClientError, DirItem, PathStat};

/// FUSE 回给内核的错误码（libc errno）。
pub type Errno = i32;
pub type Res<T> = std::result::Result<T, Errno>;

pub const ROOT_INO: u64 = 1;

/// 根目录下不出现在列举里的目录：墓碑与 lost+found。
const HIDDEN_AT_ROOT: &[&str] = &[".tapers", "_ltfs_lostandfound"];

/// 只读扩展属性。
pub const XATTRS: &[&str] = &["user.tape.state", "user.tape.generation", "user.tape.barcode", "user.tape.sha256"];

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
}

/// 客户端库错误到 errno 的映射（file-contract.md "错误映射"）。
pub fn errno_of(e: &ClientError) -> Errno {
    match e {
        ClientError::NotFound => libc::ENOENT,
        ClientError::Exists(_) => libc::EEXIST,
        // 没有节点在服务：恢复中，不是不存在
        ClientError::NoLeader(_) => libc::EBUSY,
        ClientError::Io(_) => libc::EIO,
        ClientError::Rejected { status, body } => match status {
            409 => libc::EBUSY,
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

    fn delete(&self, _path: &str) -> Res<()> {
        // TODO(F2)：删除接口合入后接上 Client::delete
        Err(libc::EOPNOTSUPP)
    }
}

/// 文件或目录的属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub is_dir: bool,
    pub size: u64,
}

enum Handle {
    Read {
        path: String,
        cache: PathBuf,
        /// 第一次读时拉取
        file: Option<File>,
    },
    Write {
        path: String,
        cache: PathBuf,
        file: File,
        len: u64,
        /// 以 `O_EXCL` 创建：第一次上传用只创建
        create_only: bool,
        /// 有尚未上传的内容（新建的空文件也算：`touch` 要在 close 时建出来）
        dirty: bool,
        /// 至少上传过一次
        uploaded: bool,
        /// 最近一次上传已确认落带
        committed: bool,
    },
}

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
    cache_dir: PathBuf,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Arc<Mutex<Handle>>>>,
    next_fh: Mutex<u64>,
    mem_dirs: Mutex<BTreeSet<String>>,
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

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

impl<B: Backend> TapeFs<B> {
    /// `cache_dir` 放写入缓存与读取缓存，启动时清空。
    pub fn new(backend: B, cache_dir: PathBuf) -> std::io::Result<Self> {
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            backend,
            cache_dir,
            inodes: Mutex::new(Inodes::default()),
            handles: Mutex::new(HashMap::new()),
            next_fh: Mutex::new(1),
            mem_dirs: Mutex::new(BTreeSet::new()),
            writers: Mutex::new(HashMap::new()),
            commit_timeout: Duration::from_secs(300),
        })
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
        let fh = {
            let mut n = lock(&self.next_fh);
            *n += 1;
            *n
        };
        lock(&self.handles).insert(fh, Arc::new(Mutex::new(h)));
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
        if path == "/" {
            return Ok(Attr { ino: ROOT_INO, is_dir: true, size: 0 });
        }
        if let Some(len) = self.open_write_len(path) {
            return Ok(Attr { ino: self.ino_of(path), is_dir: false, size: len });
        }
        if let Some(st) = self.backend.stat(path)? {
            let size = st.current.as_ref().map(|c| c.length).unwrap_or(st.length);
            return Ok(Attr { ino: self.ino_of(path), is_dir: false, size });
        }
        if lock(&self.mem_dirs).contains(path) || self.backend.list_dir(path)?.is_some() {
            return Ok(Attr { ino: self.ino_of(path), is_dir: true, size: 0 });
        }
        Err(libc::ENOENT)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Res<Attr> {
        let p = self.path_of(parent)?;
        self.attr_of(&join(&p, name))
    }

    pub fn getattr(&self, ino: u64) -> Res<Attr> {
        let p = self.path_of(ino)?;
        self.attr_of(&p)
    }

    /// 目录的子项：(名字, 是否目录, inode)。
    pub fn readdir(&self, ino: u64) -> Res<Vec<(String, bool, u64)>> {
        let dir = self.path_of(ino)?;
        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        let server = self.backend.list_dir(&dir)?;
        let local_dir = dir == "/" || lock(&self.mem_dirs).contains(&dir);
        if server.is_none() && !local_dir {
            return Err(libc::ENOENT);
        }
        for e in server.unwrap_or_default() {
            out.insert(e.name, e.is_dir);
        }
        for d in lock(&self.mem_dirs).iter() {
            if parent_of(d) == dir && d != "/" {
                out.insert(d.rsplit('/').next().unwrap_or_default().to_string(), true);
            }
        }
        for p in self.open_write_paths() {
            if parent_of(&p) == dir {
                out.entry(p.rsplit('/').next().unwrap_or_default().to_string()).or_insert(false);
            }
        }
        if dir == "/" {
            for h in HIDDEN_AT_ROOT {
                out.remove(*h);
            }
        }
        Ok(out.into_iter().map(|(n, d)| {
            let ino = self.ino_of(&join(&dir, &n));
            (n, d, ino)
        }).collect())
    }

    /// 新建文件并以写方式打开。`excl` 对应 `O_EXCL`：只创建。
    pub fn create(&self, parent: u64, name: &str, excl: bool) -> Res<(Attr, u64)> {
        let path = join(&self.path_of(parent)?, name);
        if self.open_write_len(&path).is_some() {
            return Err(libc::EBUSY);
        }
        if excl && self.backend.stat(&path)?.is_some() {
            return Err(libc::EEXIST);
        }
        let fh = self.open_for_write(&path, excl)?;
        Ok((Attr { ino: self.ino_of(&path), is_dir: false, size: 0 }, fh))
    }

    fn open_for_write(&self, path: &str, create_only: bool) -> Res<u64> {
        let cache = self.cache_path("w");
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&cache).map_err(io_errno)?;
        let mut writers = lock(&self.writers);
        if writers.contains_key(path) {
            let _ = std::fs::remove_file(&cache);
            return Err(libc::EBUSY);
        }
        let fh = self.new_fh(Handle::Write {
            path: path.to_string(),
            cache,
            file,
            len: 0,
            create_only,
            dirty: true,
            uploaded: false,
            committed: false,
        });
        writers.insert(path.to_string(), (fh, 0));
        Ok(fh)
    }

    /// 打开已有文件。只读：要有已提交的当前版本；写：必须带 `O_TRUNC`（整体替换）。
    pub fn open(&self, ino: u64, flags: i32) -> Res<u64> {
        let path = self.path_of(ino)?;
        match flags & libc::O_ACCMODE {
            libc::O_RDONLY => {
                match self.backend.stat(&path)? {
                    Some(st) if st.current.is_some() => {}
                    // 只有在途版本：尚未提交，不能读
                    Some(_) => return Err(libc::EBUSY),
                    None if self.open_write_len(&path).is_some() => return Err(libc::EBUSY),
                    None => return Err(libc::ENOENT),
                }
                let cache = self.cache_path("r");
                Ok(self.new_fh(Handle::Read { path, cache, file: None }))
            }
            libc::O_WRONLY => {
                if flags & libc::O_APPEND != 0 || flags & libc::O_TRUNC == 0 {
                    return Err(libc::EOPNOTSUPP);
                }
                if self.open_write_len(&path).is_some() {
                    return Err(libc::EBUSY);
                }
                self.open_for_write(&path, false)
            }
            _ => Err(libc::EOPNOTSUPP),
        }
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        match &mut *g {
            Handle::Read { path, cache, file } => {
                if file.is_none() {
                    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&*cache).map_err(io_errno)?;
                    self.backend.fetch(path, &mut f)?;
                    *file = Some(f);
                }
                let f = file.as_ref().expect("已拉取");
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
            Handle::Write { .. } => Err(libc::EBADF),
        }
    }

    /// 顺序写：`offset` 必须等于当前长度。
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        match &mut *g {
            Handle::Write { path, file, len, dirty, committed, .. } => {
                if offset != *len {
                    return Err(libc::EINVAL);
                }
                file.write_all_at(data, offset).map_err(io_errno)?;
                *len += data.len() as u64;
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

    /// 截断：只允许把正以写方式打开的文件截到 0（`O_TRUNC` 不走原子路径时内核这样发），
    /// 或者截到当前长度（无事可做）。
    pub fn truncate(&self, ino: u64, size: u64) -> Res<Attr> {
        let path = self.path_of(ino)?;
        let writer = lock(&self.writers).get(&path).map(|w| w.0);
        if let Some(fh) = writer {
            let h = self.handle(fh)?;
            if let Handle::Write { file, len, dirty, committed, .. } = &mut *lock(&h) {
                if size == *len {
                    return Ok(Attr { ino, is_dir: false, size });
                }
                if size != 0 {
                    return Err(libc::EOPNOTSUPP);
                }
                file.set_len(0).map_err(io_errno)?;
                *len = 0;
                *dirty = true;
                *committed = false;
                if let Some(w) = lock(&self.writers).get_mut(&path) {
                    w.1 = 0;
                }
                return Ok(Attr { ino, is_dir: false, size: 0 });
            }
        }
        let a = self.attr_of(&path)?;
        if !a.is_dir && a.size == size { Ok(a) } else { Err(libc::EOPNOTSUPP) }
    }

    /// close：把内容上传到暂存区。不等落带。
    pub fn flush(&self, fh: u64) -> Res<()> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        match &mut *g {
            Handle::Write { path, cache, create_only, dirty, uploaded, committed, file, .. } => {
                if !*dirty && *uploaded {
                    return Ok(());
                }
                file.sync_data().map_err(io_errno)?;
                let c = self.backend.upload(path, cache, *create_only && !*uploaded, false)?;
                *dirty = false;
                *uploaded = true;
                *committed = c;
                Ok(())
            }
            Handle::Read { .. } => Ok(()),
        }
    }

    /// fsync：上传（如需要）并等到落带。返回 `Ok` 即截至此刻的内容已归档。
    pub fn fsync(&self, fh: u64) -> Res<()> {
        let h = self.handle(fh)?;
        let mut g = lock(&h);
        let Handle::Write { path, cache, create_only, dirty, uploaded, committed, file, .. } = &mut *g else {
            return Ok(());
        };
        if *committed && !*dirty {
            return Ok(());
        }
        file.sync_data().map_err(io_errno)?;
        if *dirty || !*uploaded {
            let c = self.backend.upload(path, cache, *create_only && !*uploaded, true)?;
            *dirty = false;
            *uploaded = true;
            *committed = c;
            return if c { Ok(()) } else { Err(libc::EIO) };
        }
        // 已经暂存过、内容没变：等那次上传落带，按哈希确认是这份内容
        let digest = crate::client::sha256_file(cache).map_err(io_errno)?;
        let deadline = Instant::now() + self.commit_timeout;
        loop {
            match self.backend.stat(path)? {
                Some(st) if st.state == "committed" => {
                    if st.current.is_some_and(|c| c.sha256 == digest) {
                        *committed = true;
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
        if let Some(h) = lock(&self.handles).remove(&fh) {
            let cache = match &*lock(&h) {
                Handle::Read { cache, .. } => cache.clone(),
                Handle::Write { path, cache, .. } => {
                    let mut w = lock(&self.writers);
                    if w.get(path).is_some_and(|e| e.0 == fh) {
                        w.remove(path);
                    }
                    cache.clone()
                }
            };
            let _ = std::fs::remove_file(cache);
        }
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Res<()> {
        let path = join(&self.path_of(parent)?, name);
        if self.open_write_len(&path).is_some() {
            return Err(libc::EBUSY);
        }
        self.backend.delete(&path)
    }

    pub fn mkdir(&self, parent: u64, name: &str) -> Res<Attr> {
        let path = join(&self.path_of(parent)?, name);
        match self.attr_of(&path) {
            Ok(_) => return Err(libc::EEXIST),
            Err(e) if e != libc::ENOENT => return Err(e),
            Err(_) => {}
        }
        lock(&self.mem_dirs).insert(path.clone());
        Ok(Attr { ino: self.ino_of(&path), is_dir: true, size: 0 })
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Res<()> {
        let path = join(&self.path_of(parent)?, name);
        let a = self.attr_of(&path)?;
        if !a.is_dir {
            return Err(libc::ENOTDIR);
        }
        if !self.readdir(a.ino)?.is_empty() {
            return Err(libc::ENOTEMPTY);
        }
        if lock(&self.mem_dirs).remove(&path) { Ok(()) } else { Err(libc::ENOENT) }
    }

    /// 首版不支持重命名；`EXDEV` 让 `mv` 退回复制 + 删除。
    pub fn rename(&self) -> Res<()> {
        Err(libc::EXDEV)
    }

    pub fn getxattr(&self, ino: u64, name: &str) -> Res<Vec<u8>> {
        let path = self.path_of(ino)?;
        let st = self.backend.stat(&path)?.ok_or(libc::ENODATA)?;
        let cur = st.current.as_ref();
        let v = match name {
            "user.tape.state" => st.state.clone(),
            "user.tape.generation" => cur.map(|c| c.generation.to_string()).ok_or(libc::ENODATA)?,
            "user.tape.barcode" => cur.map(|c| c.barcode.clone()).ok_or(libc::ENODATA)?,
            "user.tape.sha256" => cur.map(|c| c.sha256.clone()).ok_or(libc::ENODATA)?,
            _ => return Err(libc::ENODATA),
        };
        Ok(v.into_bytes())
    }

    pub fn listxattr(&self, ino: u64) -> Res<Vec<u8>> {
        if self.getattr(ino)?.is_dir {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for n in XATTRS {
            out.extend_from_slice(n.as_bytes());
            out.push(0);
        }
        Ok(out)
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
    pub committed: BTreeMap<String, Vec<u8>>,
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
    fn stat(&self, path: &str) -> Res<Option<PathStat>> {
        let s = lock(&self.state);
        let current = s.committed.get(path).map(|d| crate::client::FileStat {
            length: d.len() as u64,
            generation: s.generation,
            round: 1,
            barcode: "MEM001L8".into(),
            sha256: sha_hex(d),
        });
        Ok(match (s.staged.get(path), current) {
            (Some(d), cur) => Some(PathStat { state: "staged".into(), length: d.len() as u64, current: cur }),
            (None, Some(c)) => Some(PathStat { state: "committed".into(), length: c.length, current: Some(c) }),
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
        if out.is_empty() && dir != "/" {
            return Ok(None);
        }
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
        s.committed.remove(path).map(|_| ()).ok_or(libc::ENOENT)
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

    /// FC04：close 之后是已暂存、读 EBUSY；fsync 返回 0 之后已提交且可读。
    #[test]
    fn close_stages_and_fsync_commits() {
        let t = fs("fc04");
        let fh = write_file(&t, ROOT_INO, "a.bin", b"hello");
        t.flush(fh).unwrap();
        t.release(fh);
        let a = t.lookup(ROOT_INO, "a.bin").unwrap();
        assert_eq!(a.size, 5);
        assert_eq!(t.getxattr(a.ino, "user.tape.state").unwrap(), b"staged");
        assert_eq!(t.open(a.ino, libc::O_RDONLY), Err(libc::EBUSY), "暂存中不可读");
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

    /// FC03：非顺序写 EINVAL；已存在文件不带 O_TRUNC 写、O_RDWR、O_APPEND 都不支持。
    #[test]
    fn only_sequential_whole_file_writes() {
        let t = fs("fc03");
        let fh = write_file(&t, ROOT_INO, "a", b"12345");
        assert_eq!(t.write(fh, 3, b"x"), Err(libc::EINVAL));
        assert_eq!(t.write(fh, 9, b"x"), Err(libc::EINVAL));
        t.fsync(fh).unwrap();
        t.release(fh);
        let a = t.lookup(ROOT_INO, "a").unwrap();
        assert_eq!(t.open(a.ino, libc::O_WRONLY), Err(libc::EOPNOTSUPP));
        assert_eq!(t.open(a.ino, libc::O_RDWR), Err(libc::EOPNOTSUPP));
        assert_eq!(t.open(a.ino, libc::O_WRONLY | libc::O_TRUNC | libc::O_APPEND), Err(libc::EOPNOTSUPP));
        // O_TRUNC：整体替换
        let w = t.open(a.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        t.write(w, 0, b"new").unwrap();
        t.fsync(w).unwrap();
        t.release(w);
        assert_eq!(t.lookup(ROOT_INO, "a").unwrap().size, 3);
        // 截断只允许写句柄截到 0
        assert_eq!(t.truncate(a.ino, 1), Err(libc::EOPNOTSUPP));
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

    /// FC11：隐式目录、mkdir 只在内存里、rmdir 只对空目录；rename 返回 EXDEV。
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
        assert_eq!(t.rename(), Err(libc::EXDEV));
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
    fn errno_mapping() {
        let r = |status: u16, body: &str| errno_of(&ClientError::Rejected { status, body: body.into() });
        assert_eq!(r(409, "path_busy"), libc::EBUSY);
        assert_eq!(r(412, "exists"), libc::EEXIST);
        assert_eq!(r(503, "read_only"), libc::EROFS);
        assert_eq!(r(503, "not_serving"), libc::EBUSY);
        assert_eq!(r(507, "too_large"), libc::EFBIG);
        assert_eq!(r(507, "no_tape"), libc::ENOSPC);
        assert_eq!(errno_of(&ClientError::NoLeader(String::new())), libc::EBUSY, "恢复中不是不存在");
    }
}
