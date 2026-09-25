//! tape-fuse：独立的 ltfsd FUSE 客户端，前台运行。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use fuser::*;
use nix::libc;
use tape_rs::client::Client;
use tape_rs::tapefs::{Attr, Backend, ClientBackend, Res, TapeFs};

#[derive(Parser)]
#[command(
    name = "tape-fuse",
    about = "挂载 ltfsd 文件服务；close 仅暂存，fsync 才确认落带"
)]
struct Args {
    /// 节点的客户端接口地址，可重复指定
    #[arg(short = 'e', long = "endpoint", required = true)]
    endpoints: Vec<String>,
    /// 已存在的挂载目录
    mountpoint: PathBuf,
    /// 本地缓存目录，每次挂载创建独占子目录
    #[arg(long)]
    cache_dir: PathBuf,
}

type DirectoryEntries = Vec<(String, FileType, u64)>;

struct Adapter<B: Backend> {
    fs: TapeFs<B>,
    uid: u32,
    gid: u32,
    directories: Mutex<HashMap<u64, DirectoryEntries>>,
    next_directory: AtomicU64,
}

impl<B: Backend> Adapter<B> {
    fn new(fs: TapeFs<B>) -> Self {
        Self {
            fs,
            // getuid/getgid 不带指针，也不修改进程身份。
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            directories: Mutex::new(HashMap::new()),
            next_directory: AtomicU64::new(1),
        }
    }

    fn attr(&self, a: Attr) -> FileAttr {
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: a.mtime,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if a.is_symlink { FileType::Symlink } else { kind(a.is_dir) },
            perm: if a.is_symlink { 0o777 } else if a.is_dir { 0o755 } else { 0o644 },
            nlink: if a.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

fn kind(dir: bool) -> FileType {
    if dir {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

fn name(s: &OsStr) -> Res<&str> {
    s.to_str().ok_or(libc::EINVAL)
}

fn empty(reply: ReplyEmpty, result: Res<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}

fn xattr(reply: ReplyXattr, size: u32, result: Res<Vec<u8>>) {
    match result {
        Ok(v) if size == 0 => reply.size(v.len() as u32),
        Ok(v) if v.len() <= size as usize => reply.data(&v),
        Ok(_) => reply.error(Errno::ERANGE),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}

impl<B: Backend + 'static> Filesystem for Adapter<B> {
    fn init(&mut self, _: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // 不允许内核先截断、再剥掉 O_TRUNC；逻辑层必须知道这是整体替换。
        config
            .add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC)
            .map_err(|_| std::io::Error::other("内核不支持原子 O_TRUNC"))?;
        // 使用内核页缓存与默认 write-through，不协商异步 writeback-cache。
        Ok(())
    }

    fn lookup(&self, _: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEntry) {
        match name(n).and_then(|n| self.fs.lookup(parent.0, n)) {
            Ok(a) => reply.entry(&Duration::ZERO, &self.attr(a), Generation(0)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        match self.fs.getattr(ino.0) {
            Ok(a) => reply.attr(&Duration::ZERO, &self.attr(a)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn readlink(&self, _: &Request, ino: INodeNo, reply: ReplyData) {
        match self.fs.readlink(ino.0) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn setattr(
        &self,
        _: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        _: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if mode.is_some()
            || uid.is_some()
            || gid.is_some()
            || atime.is_some()
            || mtime.is_some()
            || ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some()
            || bkuptime.is_some()
            || flags.is_some()
        {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        match size.map_or_else(|| self.fs.getattr(ino.0), |s| self.fs.truncate(ino.0, s)) {
            Ok(a) => reply.attr(&Duration::ZERO, &self.attr(a)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.fs.open(ino.0, flags.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn create(
        &self,
        _: &Request,
        parent: INodeNo,
        n: &OsStr,
        _: u32,
        _: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match name(n).and_then(|n| self.fs.create(parent.0, n, flags & libc::O_EXCL != 0)) {
            Ok((a, fh)) => reply.created(
                &Duration::ZERO,
                &self.attr(a),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn read(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.fs.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn write(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.fs.write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn flush(&self, _: &Request, _: INodeNo, fh: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        empty(reply, self.fs.flush(fh.0));
    }

    fn fsync(&self, _: &Request, _: INodeNo, fh: FileHandle, _: bool, reply: ReplyEmpty) {
        empty(reply, self.fs.fsync(fh.0));
    }

    fn release(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        self.fs.release(fh.0);
        reply.ok();
    }

    fn opendir(&self, _: &Request, ino: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        let result = (|| {
            if !self.fs.getattr(ino.0)?.is_dir {
                return Err(libc::ENOTDIR);
            }
            let path = self.fs.path_of(ino.0)?;
            let parent = Path::new(&path)
                .parent()
                .and_then(Path::to_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("/");
            let mut entries = vec![
                (".".into(), FileType::Directory, ino.0),
                ("..".into(), FileType::Directory, self.fs.attr_of(parent)?.ino),
            ];
            for (name, dir, child) in self.fs.readdir(ino.0)? {
                let file_type = if dir { FileType::Directory } else {
                    match self.fs.getattr(child) {
                        Ok(a) if a.is_symlink => FileType::Symlink,
                        Ok(_) => FileType::RegularFile,
                        Err(libc::ENOENT | libc::ESTALE) => continue,
                        Err(e) => return Err(e),
                    }
                };
                entries.push((name, file_type, child));
            }
            Ok(entries)
        })();
        match result {
            Ok(entries) => {
                let fh = self.next_directory.fetch_add(1, Ordering::Relaxed);
                self.directories
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(fh, entries);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn readdir(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // 每个打开的目录保存一次快照，分页期间并发增删不会让 offset 跳过或重复条目。
        let directories = self.directories.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = directories.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        for (i, (n, dir, ino)) in entries
            .iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *dir, n) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _: &Request, _: INodeNo, fh: FileHandle, _: OpenFlags, reply: ReplyEmpty) {
        self.directories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh.0);
        reply.ok();
    }

    fn mkdir(&self, _: &Request, parent: INodeNo, n: &OsStr, _: u32, _: u32, reply: ReplyEntry) {
        match name(n).and_then(|n| self.fs.mkdir(parent.0, n)) {
            Ok(a) => reply.entry(&Duration::ZERO, &self.attr(a), Generation(0)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn unlink(&self, _: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        empty(reply, name(n).and_then(|n| self.fs.unlink(parent.0, n)));
    }

    fn rmdir(&self, _: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        empty(reply, name(n).and_then(|n| self.fs.rmdir(parent.0, n)));
    }

    fn rename(
        &self,
        _: &Request,
        parent: INodeNo,
        old: &OsStr,
        new_parent: INodeNo,
        new: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.bits() & !1 != 0 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        empty(
            reply,
            name(old).and_then(|old| {
                name(new).and_then(|new| {
                    self.fs
                        .rename(parent.0, old, new_parent.0, new, flags.bits() & 1 != 0)
                })
            }),
        );
    }

    fn fsyncdir(&self, _: &Request, _: INodeNo, _: FileHandle, _: bool, reply: ReplyEmpty) {
        // unlink 本身同步等待墓碑落带，没有后台删除队列。
        reply.ok();
    }

    fn getxattr(&self, _: &Request, ino: INodeNo, n: &OsStr, size: u32, reply: ReplyXattr) {
        xattr(
            reply,
            size,
            name(n).and_then(|n| self.fs.getxattr(ino.0, n)),
        );
    }

    fn listxattr(&self, _: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        xattr(reply, size, self.fs.listxattr(ino.0));
    }

    fn setxattr(
        &self,
        _: &Request,
        ino: INodeNo,
        n: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        let result = if position != 0 || flags < 0 {
            Err(libc::EINVAL)
        } else {
            name(n).and_then(|n| self.fs.change_xattr(ino.0, n, Some(value), flags as u32))
        };
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn removexattr(&self, _: &Request, ino: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        match name(n).and_then(|n| self.fs.change_xattr(ino.0, n, None, 0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn link(&self, _: &Request, _: INodeNo, _: INodeNo, _: &OsStr, reply: ReplyEntry) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn symlink(&self, _: &Request, parent: INodeNo, n: &OsStr, target: &Path, reply: ReplyEntry) {
        let result = name(n).and_then(|n| {
            let target = target.to_str().ok_or(libc::EINVAL)?;
            self.fs.symlink(parent.0, n, target)
        });
        match result {
            Ok(a) => reply.entry(&Duration::ZERO, &self.attr(a), Generation(0)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }
}

fn main() {
    env_logger::init();
    let args = Args::parse();
    let result = (|| {
        let fs = TapeFs::new(
            ClientBackend::new(Client::new(args.endpoints)),
            args.cache_dir,
        )?;
        let adapter = Adapter::new(fs);
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("tape-fuse".into()),
            MountOption::DefaultPermissions,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        config.n_threads = Some(4);
        fuser::mount(adapter, args.mountpoint, &config)
    })();
    if let Err(e) = result {
        eprintln!("挂载失败: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tape_rs::tapefs::MemBackend;

    struct SharedMem {
        inner: std::sync::Arc<MemBackend>,
        reject_upload: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Backend for SharedMem {
        fn change_xattr(&self, p: &str, n: &str, v: Option<&[u8]>, flags: u32) -> Res<()> {
            self.inner.change_xattr(p, n, v, flags)
        }

        fn mkdir(&self, p: &str) -> Res<()> {
            self.inner.mkdir(p)
        }
        fn rename(&self, from: &str, to: &str, no_replace: bool) -> Res<()> {
            // 故障探针模拟跨卷 EXDEV，随后拒绝 mv 回退的目标上传。
            if self.reject_upload.load(Ordering::Relaxed) { return Err(libc::EXDEV); }
            self.inner.rename(from, to, no_replace)
        }
        fn rmdir(&self, p: &str) -> Res<()> {
            self.inner.rmdir(p)
        }
        fn stat(&self, p: &str) -> Res<Option<tape_rs::client::PathStat>> {
            self.inner.stat(p)
        }
        fn list_dir(&self, p: &str) -> Res<Option<Vec<tape_rs::client::DirItem>>> {
            self.inner.list_dir(p)
        }
        fn fetch(&self, p: &str, f: &mut std::fs::File) -> Res<u64> {
            self.inner.fetch(p, f)
        }
        fn upload(&self, p: &str, f: &Path, c: bool, w: bool) -> Res<bool> {
            if self.reject_upload.load(Ordering::Relaxed) {
                return Err(libc::ENOSPC);
            }
            self.inner.upload(p, f, c, w)
        }
        fn delete(&self, p: &str) -> Res<()> {
            self.inner.delete(p)
        }
    }

    /// 需要 FUSE 挂载权限、GNU mv 和 Python 3；仅内存后端。
    #[test]
    #[ignore = "需要本机 FUSE、mv 和 python3"]
    fn kernel_mv_and_mmap() {
        let root = std::env::temp_dir().join(format!("tape-fuse-tools-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let backend = std::sync::Arc::new(MemBackend::default());
        let reject_upload = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fs = TapeFs::new(
            SharedMem {
                inner: backend.clone(),
                reject_upload: reject_upload.clone(),
            },
            root.join("cache"),
        )
        .unwrap();
        let mut config = Config::default();
        config.n_threads = Some(4);
        config.mount_options = vec![
            MountOption::DefaultPermissions,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        let session = fuser::spawn_mount(Adapter::new(fs), &mount, &config).unwrap();
        let path = mount.join("source");
        let mut w = std::fs::File::create(&path).unwrap();
        w.write_all(b"mapped content").unwrap();
        w.sync_all().unwrap();
        drop(w);
        let mmap = std::process::Command::new("python3").args(["-c", "import mmap,sys; f=open(sys.argv[1],'rb'); m=mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ); assert m[:]==b'mapped content'; m.close(); f.close(); f=open(sys.argv[1],'r+b'); assert f.read()==b'mapped content'; f.close()"]).arg(&path).output().unwrap();
        let dest = mount.join("destination");
        let moved = std::process::Command::new("mv")
            .arg(&path)
            .arg(&dest)
            .output()
            .unwrap();
        // close 仅暂存：模拟后台提交后再核对可读内容。
        backend.commit_all();
        let source_exists = path.exists();
        let data = std::fs::read(&dest);
        // 跨卷 EXDEV 回退且目标暂存失败时，mv 必须保留已提交源文件。
        reject_upload.store(true, Ordering::Relaxed);
        let failed_move = std::process::Command::new("mv")
            .arg(&dest)
            .arg(mount.join("rejected"))
            .output()
            .unwrap();
        let retained = std::fs::read(&dest);
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            !failed_move.status.success(),
            "目标暂存失败不能报告移动成功"
        );
        assert_eq!(retained.unwrap(), b"mapped content");
        assert!(
            mmap.status.success(),
            "mmap: {}; mv status {:?}: {}",
            String::from_utf8_lossy(&mmap.stderr),
            moved.status,
            String::from_utf8_lossy(&moved.stderr)
        );
        assert!(
            moved.status.success(),
            "mv: {}",
            String::from_utf8_lossy(&moved.stderr)
        );
        assert!(!source_exists);
        assert_eq!(data.unwrap(), b"mapped content");
    }

    /// 在 LOOKUP 返回旧属性后替换远端内容，覆盖实际内核的 ESTALE 重试路径。
    #[test]
    #[ignore = "需要本机 FUSE 挂载权限"]
    fn kernel_remote_resize_between_lookup_and_open() {
        struct ReplaceAfterStat {
            inner: MemBackend,
            replacements: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        }
        impl Backend for ReplaceAfterStat {
            fn stat(&self, p: &str) -> Res<Option<tape_rs::client::PathStat>> {
                let old = self.inner.stat(p)?;
                if let Some(data) = self.replacements.lock().unwrap().remove(p) {
                    self.inner
                        .state
                        .lock()
                        .unwrap()
                        .committed
                        .insert(p.into(), data);
                }
                Ok(old)
            }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<tape_rs::client::DirItem>>> {
                self.inner.list_dir(p)
            }
            fn fetch(&self, p: &str, f: &mut std::fs::File) -> Res<u64> {
                self.inner.fetch(p, f)
            }
            fn upload(&self, p: &str, f: &Path, c: bool, w: bool) -> Res<bool> {
                self.inner.upload(p, f, c, w)
            }
            fn delete(&self, p: &str) -> Res<()> {
                self.inner.delete(p)
            }
        }
        let root = std::env::temp_dir().join(format!("tape-fuse-resize-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let inner = MemBackend::default();
        let replacements = std::collections::BTreeMap::from([
            ("/grow".to_string(), vec![b'G'; 16384]),
            ("/shrink".to_string(), vec![b'S'; 1024]),
        ]);
        for path in replacements.keys() {
            inner
                .state
                .lock()
                .unwrap()
                .committed
                .insert(path.clone(), vec![b'O'; 8192]);
        }
        let backend = ReplaceAfterStat {
            inner,
            replacements: std::sync::Mutex::new(replacements.clone()),
        };
        let fs = TapeFs::new(backend, root.join("cache")).unwrap();
        let mut config = Config::default();
        config.n_threads = Some(4);
        let session = fuser::spawn_mount(Adapter::new(fs), &mount, &config).unwrap();
        let results: Vec<_> = replacements
            .iter()
            .map(|(name, expected)| {
                let path = mount.join(name.trim_start_matches('/'));
                // Linux 可以自行重新 LOOKUP；若把 ESTALE 返回应用，则显式刷新后重试。
                // 不主动查询属性，避免读取辅助函数掩盖 OPEN 后的旧长度。
                let read = || -> std::io::Result<Vec<u8>> {
                    let mut file = std::fs::File::open(&path)?;
                    let mut out = Vec::new();
                    let mut buf = [0; 4096];
                    loop {
                        let n = file.read(&mut buf)?;
                        if n == 0 {
                            return Ok(out);
                        }
                        out.extend_from_slice(&buf[..n]);
                    }
                };
                let first = read();
                let result = match first {
                    Err(e) if e.raw_os_error() == Some(libc::ESTALE) => {
                        std::fs::metadata(&path).and_then(|_| read())
                    }
                    other => other,
                };
                (name.clone(), result, expected.clone())
            })
            .collect();
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        for (name, result, expected) in results {
            assert_eq!(result.unwrap(), expected, "{name}: 不得按旧长度截断或补零");
        }
    }

    /// 外部替换不能混入仍存活的映射，全部释放后可以重新下载。
    #[test]
    #[ignore = "需要本机 FUSE 和 python3"]
    fn kernel_remote_replace_preserves_mapping_until_release() {
        use std::io::BufRead;
        use std::process::Stdio;
        let root = std::env::temp_dir().join(format!("tape-fuse-remote-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let inner = std::sync::Arc::new(MemBackend::default());
        inner
            .state
            .lock()
            .unwrap()
            .committed
            .insert("/file".into(), vec![b'A'; 8192]);
        let backend = SharedMem {
            inner: inner.clone(),
            reject_upload: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let fs = TapeFs::new(backend, root.join("cache")).unwrap();
        let mut config = Config::default();
        config.n_threads = Some(4);
        let session = fuser::spawn_mount(Adapter::new(fs), &mount, &config).unwrap();
        let mut child = std::process::Command::new("python3")
            .args([
                "-c",
                r#"
import errno,mmap,os,sys,time
p=sys.argv[1]+'/file'
r=os.open(p,os.O_RDONLY)
m=mmap.mmap(r,0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ)
assert m[:4096]==b'A'*4096
print('ready',flush=True)
assert sys.stdin.readline().strip()=='replace'
try:
    unexpected=os.open(p,os.O_RDONLY)
except OSError as e:
    assert e.errno==errno.ESTALE,e
else:
    os.close(unexpected)
    raise AssertionError('live old mapping must reject remote replacement')
# 映射中尚未显式读取的部分也必须保持旧内容。
assert m[:]==b'A'*8192
os.close(r)
assert m[:]==b'A'*8192
m.close()
for attempt in range(100):
    try:
        with open(p,'rb') as f:
            assert f.read()==b'B'*16384
        break
    except OSError as e:
        if e.errno!=errno.ESTALE or attempt==99: raise
        time.sleep(0.01)
"#,
            ])
            .arg(&mount)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        if ready.trim() == "ready" {
            inner
                .state
                .lock()
                .unwrap()
                .committed
                .insert("/file".into(), vec![b'B'; 16384]);
            child.stdin.take().unwrap().write_all(b"replace\n").unwrap();
        }
        let output = child.wait_with_output().unwrap();
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(ready.trim(), "ready");
    }

    /// 页缓存下同 inode 的可见性；仅内存后端，不接触磁带。
    #[test]
    #[ignore = "需要本机 FUSE 和 python3"]
    fn kernel_cached_visibility() {
        let root = std::env::temp_dir().join(format!("tape-fuse-cache-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let fs = TapeFs::new(MemBackend::default(), root.join("cache")).unwrap();
        let mut config = Config::default();
        config.n_threads = Some(4);
        let session = fuser::spawn_mount(Adapter::new(fs), &mount, &config).unwrap();
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                r#"
import os,mmap,sys
p=sys.argv[1]+'/file'
w=os.open(p,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
os.write(w,b'A'*8192)
r=os.open(p,os.O_RDONLY)
assert os.pread(r,8192,0)==b'A'*8192, 'write must be visible before fsync'
m=mmap.mmap(r,0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ)
assert m[:]==b'A'*8192
os.fsync(w); os.close(w)
w=os.open(p,os.O_WRONLY|os.O_TRUNC)
assert os.fstat(r).st_size==0 and os.pread(r,8192,0)==b''
os.write(w,b'B'*4096)
assert os.pread(r,8192,0)==b'B'*4096
assert m[:4096]==b'B'*4096
os.fsync(w); os.close(w)
old_ino=os.fstat(r).st_ino
os.unlink(p)
assert os.pread(r,8192,0)==b'B'*4096
w=os.open(p,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
os.write(w,b'C'*8192); os.fsync(w); os.close(w)
assert os.stat(p).st_ino!=old_ino
assert open(p,'rb').read()==b'C'*8192
assert os.fstat(r).st_size==4096
assert os.pread(r,8192,0)==b'B'*4096 and m[:4096]==b'B'*4096
os.close(r)
assert m[:4096]==b'B'*4096
m.close(); os.unlink(p)
# LE 支持 O_RDWR、多个 writer、追加、随机覆盖和任意截断。
a=os.open(p,os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
os.write(a,b'12345')
b=os.open(p,os.O_WRONLY|os.O_APPEND)
os.write(b,b'67')
assert os.pread(a,100,0)==b'1234567'
os.pwrite(a,b'XX',1)
os.ftruncate(a,10)
assert os.pread(a,100,0)==b'1XX4567'+b'\0'*3
os.ftruncate(a,3)
os.fsync(a); os.close(a)
os.write(b,b'89')
os.fsync(b); os.close(b)
assert open(p,'rb').read()==b'1XX89'
os.unlink(p)
"#,
            ])
            .arg(&mount)
            .output()
            .unwrap();
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// 索引→catalog元数据→实际内核：不访问磁带。
    #[test]
    #[ignore = "需要本机 FUSE 挂载权限"]
    fn kernel_existing_symlinks() {
        struct Links {
            inner: MemBackend,
            records: Mutex<Vec<tape_rs::daemon::state::FileRec>>,
        }
        impl Backend for Links {
            fn stat(&self, p: &str) -> Res<Option<tape_rs::client::PathStat>> {
                let mut s = self.inner.stat(p)?;
                let records = self.records.lock().unwrap();
                if let Some(r) = records.iter().find(|r| r.path == p)
                    && let Some(c) = s.as_mut().and_then(|s| s.current.as_mut())
                {
                    c.length = r.length;
                    c.sha256.clear();
                    c.metadata = r.metadata.clone();
                }
                Ok(s)
            }
            fn list_dir(&self, p: &str) -> Res<Option<Vec<tape_rs::client::DirItem>>> {
                self.inner.list_dir(p)
            }
            fn fetch(&self, p: &str, f: &mut std::fs::File) -> Res<u64> {
                self.inner.fetch(p, f)
            }
            fn upload(&self, _: &str, _: &Path, _: bool, _: bool) -> Res<bool> {
                Err(libc::EROFS)
            }
            fn symlink(&self, target: &str, p: &str) -> Res<()> {
                let mut records = self.records.lock().unwrap();
                let mut state = self.inner.state.lock().unwrap();
                if state.committed.contains_key(p) {
                    return Err(libc::EEXIST);
                }
                state.committed.insert(p.into(), Vec::new());
                records.push(tape_rs::daemon::state::FileRec {
                    path: p.into(),
                    length: 0,
                    sha256: String::new(),
                    version: (1, 1),
                    deleted: false,
                    metadata: serde_json::json!({"kind":"symlink", "symlink":target}),
                });
                Ok(())
            }
            fn delete(&self, _: &str) -> Res<()> {
                Err(libc::EROFS)
            }
        }
        let mut index =
            tape_rs::ltfs::index::LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        let inner = MemBackend::default();
        inner
            .state
            .lock()
            .unwrap()
            .committed
            .insert("/target".into(), b"target content".to_vec());
        inner
            .state
            .lock()
            .unwrap()
            .directories
            .insert("/dir".into());
        inner
            .state
            .lock()
            .unwrap()
            .committed
            .insert("/dir/child".into(), b"child".to_vec());
        for (name, target) in [
            ("link", "target"),
            ("chain", "link"),
            ("dangling", "absent"),
            ("loop", "loop"),
            ("directory", "dir"),
            ("中文", "target"),
        ] {
            index.root.files.push(tape_rs::ltfs::index::FileNode {
                name: name.into(),
                symlink: Some(target.into()),
                ..Default::default()
            });
            inner
                .state
                .lock()
                .unwrap()
                .committed
                .insert(format!("/{name}"), b"target content".to_vec());
        }
        let records = tape_rs::daemon::files::catalog_of(&index).0;
        let root = std::env::temp_dir().join(format!("tape-symlink-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let fs = TapeFs::new(
            Links {
                inner,
                records: Mutex::new(records),
            },
            root.join("cache"),
        )
        .unwrap();
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::DefaultPermissions,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        let session = fuser::spawn_mount(Adapter::new(fs), &mount, &config).unwrap();
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                r#"
import os,sys,mmap,errno
p=sys.argv[1]
assert os.path.islink(p+'/link')
assert os.readlink(p+'/link')=='target'
assert os.lstat(p+'/link').st_size==len('target')
for n in ('link','chain','中文'):
    assert open(p+'/'+n,'rb').read()==b'target content'
assert open(p+'/directory/child','rb').read()==b'child'
assert os.readlink(p+'/dangling')=='absent'
for n,e in (('dangling',errno.ENOENT),('loop',errno.ELOOP)):
    try: open(p+'/'+n,'rb')
    except OSError as ex: assert ex.errno==e
    else: raise AssertionError(n)
with open(p+'/link','rb') as f:
    with mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ) as m: assert m[:]==b'target content'
assert next(x for x in os.scandir(p) if x.name=='link').is_symlink()
for n,target in [('created','target'),('created-dangling','absent'),('created-dir','dir'),('created-loop','created-loop'),('新链接','target')]:
    os.symlink(target,p+'/'+n)
    assert os.path.islink(p+'/'+n)
    assert os.readlink(p+'/'+n)==target
    assert os.lstat(p+'/'+n).st_size==len(target.encode())
assert open(p+'/created','rb').read()==b'target content'
assert open(p+'/created-dir/child','rb').read()==b'child'
try: os.symlink('other',p+'/created')
except FileExistsError: pass
else: raise AssertionError('duplicate link accepted')
with open(p+'/created','rb') as f:
    with mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ) as m: assert m[:]==b'target content'

"#,
            ])
            .arg(&mount)
            .output()
            .unwrap();
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// 只使用内存后端；需本机 /dev/fuse 与挂载权限，不接触磁带。
    #[test]
    #[ignore = "需要本机 FUSE 挂载权限"]
    fn kernel_file_operations() {
        let root = std::env::temp_dir().join(format!("tape-fuse-test-{}", uuid::Uuid::new_v4()));
        let mount = root.join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let backend = std::sync::Arc::new(MemBackend::default());
        let make_fs = || {
            TapeFs::new(
                SharedMem {
                    inner: backend.clone(),
                    reject_upload: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
                root.join("cache"),
            )
            .unwrap()
        };
        let fs = make_fs();
        let adapter = Adapter::new(fs);
        let mut config = Config::default();
        config.n_threads = Some(4);
        config.mount_options = vec![
            MountOption::DefaultPermissions,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        let session = fuser::spawn_mount(adapter, &mount, &config).unwrap();
        let dir = mount.join("dir");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("file");
        let mut w = std::fs::File::create(&path).unwrap();
        w.write_all(b"old data").unwrap();
        w.sync_all().unwrap();
        drop(w);
        let mut reader = std::fs::File::open(&path).unwrap();
        let mut w = std::fs::File::create(&path).unwrap();
        w.write_all(b"replacement").unwrap();
        w.sync_all().unwrap();
        drop(w);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        use std::os::unix::fs::MetadataExt;
        let ino = std::fs::metadata(&path).unwrap().ino();
        let renamed = dir.join("renamed");
        let mut renamed_writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::fs::rename(&path, &renamed).unwrap();
        renamed_writer.write_all(b"replacement").unwrap();
        renamed_writer.sync_all().unwrap();
        drop(renamed_writer);
        assert!(!path.exists());
        assert_eq!(std::fs::metadata(&renamed).unwrap().ino(), ino);
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                r#"
import os,sys,errno
p,d=sys.argv[1:]
def error(code, fn):
    try: fn()
    except OSError as e: assert e.errno == code, (e.errno, code)
    else: raise AssertionError('expected errno '+str(code))
v=b'\x00\xff<>&'+bytes(range(256))
os.setxattr(p,'user.binary',v,os.XATTR_CREATE)
assert os.getxattr(p,'user.binary')==v
error(errno.EEXIST,lambda:os.setxattr(p,'user.binary',b'x',os.XATTR_CREATE))
error(errno.ENODATA,lambda:os.setxattr(p,'user.missing',b'x',os.XATTR_REPLACE))
os.setxattr(p,'user.binary',b'\xff\x00',os.XATTR_REPLACE)
assert os.getxattr(p,'user.binary')==b'\xff\x00'
os.setxattr(p,'user.empty',b'')
os.setxattr(p,'user.large',bytes(range(256))*256)
assert len(os.getxattr(p,'user.large'))==65536
assert 'user.binary' in os.listxattr(p)
os.removexattr(p,'user.binary')
error(errno.ENODATA,lambda:os.getxattr(p,'user.binary'))
error(errno.ENODATA,lambda:os.removexattr(p,'user.binary'))
error(errno.EPERM,lambda:os.setxattr(p,'user.tape.state',b'bad'))
os.setxattr(d,'user.dir',v)
assert os.getxattr(d,'user.dir')==v
assert 'user.dir' in os.listxattr(d)
with open(p,'r+b',buffering=0) as f:
    f.write(b'new')
    os.setxattr(p,'user.dirty',v)
    f.write(b'data')
    os.fsync(f.fileno())
assert os.getxattr(p,'user.dirty')==v
print('PASS kernel binary/create/replace/remove/directory/dirty-writer/64KiB xattrs')
"#,
            )
            .arg(&renamed)
            .arg(&dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        println!("{}", String::from_utf8_lossy(&output.stdout));
        std::fs::remove_file(&renamed).unwrap();
        let mut old = Vec::new();
        reader.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"newdatament");
        drop(reader);
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        std::fs::remove_dir(&dir).unwrap();
        std::fs::create_dir(mount.join("empty-before-rename")).unwrap();
        std::fs::rename(mount.join("empty-before-rename"), mount.join("empty-after-remount")).unwrap();
        session.umount_and_join().unwrap();
        let session = fuser::spawn_mount(Adapter::new(make_fs()), &mount, &config).unwrap();
        let empty = mount.join("empty-after-remount");
        assert!(empty.is_dir());
        assert!(std::fs::read_dir(&empty).unwrap().next().is_none());
        std::fs::remove_dir(&empty).unwrap();
        session.umount_and_join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
