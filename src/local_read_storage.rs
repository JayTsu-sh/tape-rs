//! Node-local, rebuildable read storage. Filesystem reservations survive the
//! return of a plain File handle; cooperative cache pins protect live readers.

use std::fs::{self, DirBuilder, File, OpenOptions, ReadDir};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{FallocateFlags, Flock, FlockArg, fallocate};
use nix::sys::statvfs::fstatvfs;
use rusqlite::Connection;
use uuid::Uuid;

use crate::error::{Result, TapeError};

const PAGE: u64 = 4096;
const CHUNK: u64 = 16 << 20;

#[derive(Debug, Clone, Copy)]
pub struct ReadStorageBudget {
    pub index_bytes: u64,
    pub cache_bytes: u64,
    pub download_bytes: u64,
    pub headroom_bytes: u64,
}

impl Default for ReadStorageBudget {
    fn default() -> Self {
        Self {
            index_bytes: 128 << 30,
            cache_bytes: 128 << 30,
            download_bytes: 128 << 30,
            headroom_bytes: 1 << 30,
        }
    }
}

fn capacity(reason: impl std::fmt::Display) -> TapeError {
    TapeError::LocalCapacity {
        reason: reason.to_string(),
    }
}

fn available(file: &File) -> Result<u64> {
    let stat = fstatvfs(file)?;
    Ok(stat.blocks_available().saturating_mul(stat.fragment_size()))
}

fn reserve(file: &File, from: u64, to: u64, headroom: u64) -> Result<()> {
    if to > i64::MAX as u64 || to < from {
        return Err(capacity("预留长度溢出"));
    }
    let extra = to - from;
    if extra == 0 {
        return Ok(());
    }
    let free = available(file)?;
    if free.saturating_sub(headroom) < extra {
        return Err(TapeError::LocalSpace {
            reason: format!("可用 {free} bytes，需预留 {extra} bytes，保留 {headroom} bytes"),
        });
    }
    fallocate(
        file.as_raw_fd(),
        FallocateFlags::FALLOC_FL_KEEP_SIZE,
        from as i64,
        extra as i64,
    )
    .map_err(|error| {
        if matches!(error, nix::errno::Errno::ENOSPC | nix::errno::Errno::EDQUOT) {
            TapeError::LocalSpace {
                reason: format!("物理空间预留失败: {error}"),
            }
        } else {
            capacity(format!("物理空间预留失败: {error}"))
        }
    })?;
    Ok(())
}

pub(crate) fn classify_error(error: TapeError) -> TapeError {
    match error {
        TapeError::Io(error)
            if matches!(
                error.raw_os_error(),
                Some(nix::libc::ENOSPC | nix::libc::EDQUOT)
            ) =>
        {
            TapeError::LocalSpace {
                reason: error.to_string(),
            }
        }
        TapeError::CatalogDb(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) =>
        {
            capacity(format!("SQLite 达到页数或磁盘上限: {error}"))
        }
        error => error,
    }
}

pub(crate) fn reclaim_cache_space(directory: &Path, budget: ReadStorageBudget) -> Result<bool> {
    if directory.exists() {
        CacheBuild::new(directory, budget)?.reclaim_unpinned()
    } else {
        Ok(false)
    }
}

pub(crate) fn retry_after_space_reclaim<T>(
    mut work: impl FnMut() -> Result<T>,
    reclaim: impl FnOnce() -> Result<bool>,
) -> Result<T> {
    match work().map_err(classify_error) {
        Err(error @ TapeError::LocalSpace { .. }) => {
            if reclaim().map_err(classify_error)? {
                work().map_err(classify_error)
            } else {
                Err(error)
            }
        }
        result => result,
    }
}

pub(crate) fn download_file(
    directory: &Path,
    length: u64,
    budget: ReadStorageBudget,
) -> Result<File> {
    if length > budget.download_bytes {
        return Err(capacity("文件超过下载暂存单次上限"));
    }
    let path = directory.join(format!("fdb-read-{}", Uuid::new_v4()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    // No directory entry survives admission, failure, process exit or a caller
    // retaining the returned descriptor. The blocks follow the inode lifetime.
    fs::remove_file(&path)?;
    reserve(&file, 0, length, budget.headroom_bytes)?;
    Ok(file)
}

pub(crate) struct CachePin {
    _file: Flock<File>,
}

impl CachePin {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(TapeError::Catalog("FDB extent 缓存不是普通文件".into()));
        }
        let file = Flock::lock(file, FlockArg::LockSharedNonblock)
            .map_err(|(_, error)| capacity(format!("extent 缓存正在回收: {error}")))?;
        let metadata = file.metadata()?;
        let current = fs::symlink_metadata(path)?;
        if metadata.nlink() == 0
            || metadata.dev() != current.dev()
            || metadata.ino() != current.ino()
        {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
        }
        Ok(Self { _file: file })
    }
}

fn cache_name(name: &str) -> bool {
    name.strip_prefix("extent-")
        .and_then(|name| name.strip_suffix(".sqlite"))
        .is_some_and(|stem| stem.len() == 64 && stem.bytes().all(|c| c.is_ascii_hexdigit()))
}

fn provisional_name(name: &str) -> bool {
    name.strip_prefix(".extent-building-")
        .or_else(|| name.strip_prefix(".extent-verifying-"))
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(|id| Uuid::parse_str(id).is_ok())
}

pub(crate) struct CacheBuild {
    // Acquired once per cold build, never once per indexed tape file.
    _lock: Flock<File>,
    directory: PathBuf,
    budget: ReadStorageBudget,
    total: u64,
    victims: ReadDir,
    file: Option<File>,
    allocated: u64,
}

impl CacheBuild {
    pub(crate) fn new(directory: &Path, budget: ReadStorageBudget) -> Result<Self> {
        if budget.cache_bytes < 16 * PAGE || budget.cache_bytes > i64::MAX as u64 {
            return Err(capacity("缓存预算必须介于 64 KiB 和 i64::MAX bytes 之间"));
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join(".cache-budget.lock"))?;
        let lock = Flock::lock(lock, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| capacity(format!("同目录的 extent 缓存构建正忙: {error}")))?;
        let mut total = 0u64;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if provisional_name(&name) {
                // This directory is owned exclusively by the managed builder.
                // An active managed builder always holds the directory lock.
                fs::remove_file(entry.path())?;
            } else if name != ".cache-budget.lock" {
                // Unknown files consume budget but are never deletion targets.
                total = total
                    .checked_add(metadata.blocks().saturating_mul(512))
                    .ok_or_else(|| capacity("缓存目录大小溢出"))?;
            }
        }
        Ok(Self {
            _lock: lock,
            directory: directory.into(),
            budget,
            total,
            victims: fs::read_dir(directory)?,
            file: None,
            allocated: 0,
        })
    }

    pub(crate) fn reclaim_unpinned(&mut self) -> Result<bool> {
        let mut removed = false;
        while self.evict_one()? {
            removed = true;
        }
        Ok(removed)
    }

    pub(crate) fn trim_to_budget(&mut self) -> Result<()> {
        while self.total > self.budget.cache_bytes {
            if !self.evict_one()? {
                return Err(capacity("现有缓存超过预算，且没有可回收的未使用缓存"));
            }
        }
        Ok(())
    }

    fn evict_one(&mut self) -> Result<bool> {
        for entry in self.victims.by_ref() {
            let entry = entry?;
            if !entry.file_name().to_str().is_some_and(cache_name) {
                continue;
            }
            let path = entry.path();
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(file) if file.metadata()?.is_file() => file,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => continue,
                Err(error) => return Err(error.into()),
            };
            let file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(file) => file,
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => continue,
                Err((_, error)) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            fs::remove_file(path)?;
            self.total = self
                .total
                .saturating_sub(metadata.blocks().saturating_mul(512));
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn admit(&mut self, conn: &Connection, path: &Path, row_bytes: usize) -> Result<()> {
        if path.parent() != Some(self.directory.as_path()) {
            return Err(capacity("缓存构建越出预算目录"));
        }
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(nix::libc::O_NOFOLLOW)
                    .open(path)?,
            );
        }
        let pages: u64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let required = pages
            .checked_mul(PAGE)
            .and_then(|n| n.checked_add((row_bytes as u64).saturating_mul(4)))
            .and_then(|n| n.checked_add(8 * PAGE))
            .ok_or_else(|| capacity("缓存页数溢出"))?;
        if required > self.budget.cache_bytes {
            return Err(capacity("extent 缓存超过目录预算上限"));
        }
        if required <= self.allocated {
            return Ok(());
        }
        let target = required
            .div_ceil(CHUNK)
            .saturating_mul(CHUNK)
            .min(self.budget.cache_bytes / PAGE * PAGE);
        let extra = target - self.allocated;
        loop {
            let free = available(self.file.as_ref().unwrap())?;
            if self.total.saturating_add(extra) <= self.budget.cache_bytes
                && free.saturating_sub(self.budget.headroom_bytes) >= extra
            {
                break;
            }
            if !self.evict_one()? {
                return Err(capacity("extent 缓存空间不足；没有可回收的未使用缓存"));
            }
        }
        reserve(
            self.file.as_ref().unwrap(),
            self.allocated,
            target,
            self.budget.headroom_bytes,
        )?;
        let actual: u64 = conn.query_row(
            &format!("PRAGMA max_page_count={}", target / PAGE),
            [],
            |r| r.get(0),
        )?;
        if actual != target / PAGE {
            return Err(capacity("缓存 SQLite 页数上限设置失败"));
        }
        self.total += extra;
        self.allocated = target;
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| capacity("缓存构建没有数据库"))?;
        // SQLite is closed before this call. Truncating at the existing EOF
        // releases KEEP_SIZE preallocation past EOF without moving any data.
        file.set_len(file.metadata()?.len())?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::os::unix::fs::PermissionsExt;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("tape-read-storage-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn budget() -> ReadStorageBudget {
        ReadStorageBudget {
            index_bytes: 128 << 30,
            cache_bytes: 16 << 20,
            download_bytes: 1 << 20,
            headroom_bytes: 0,
        }
    }
    fn cached(directory: &Path, id: u64) -> PathBuf {
        directory.join(format!("extent-{id:064x}.sqlite"))
    }
    fn reserve_cache(path: &Path, bytes: u64) {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        fallocate(file.as_raw_fd(), FallocateFlags::empty(), 0, bytes as i64).unwrap();
    }

    #[test]
    fn space_pressure_reclaims_once_but_policy_limits_never_reclaim() {
        use std::cell::Cell;
        let attempts = Cell::new(0);
        let reclaimed = Cell::new(0);
        let result = retry_after_space_reclaim(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(TapeError::Io(std::io::Error::from_raw_os_error(
                        nix::libc::ENOSPC,
                    )))
                } else {
                    Ok(7)
                }
            },
            || {
                reclaimed.set(reclaimed.get() + 1);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!((result, attempts.get(), reclaimed.get()), (7, 2, 1));
        assert!(
            retry_after_space_reclaim::<()>(
                || Err(capacity("configured limit")),
                || { panic!("a policy limit must not evict other caches") }
            )
            .is_err()
        );
        attempts.set(0);
        assert!(
            retry_after_space_reclaim::<()>(
                || {
                    attempts.set(attempts.get() + 1);
                    Err(TapeError::LocalSpace {
                        reason: "still full".into(),
                    })
                },
                || Ok(true)
            )
            .is_err()
        );
        assert_eq!(attempts.get(), 2, "no unbounded retry loop");
    }

    #[test]
    fn download_reservation_follows_unlinked_descriptor_and_denies_limits() {
        let dir = Directory::new();
        let mut file = download_file(&dir.0, 1 << 20, budget()).unwrap();
        let meta = file.metadata().unwrap();
        assert_eq!(meta.nlink(), 0);
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 0);
        assert!(meta.blocks() * 512 >= 1 << 20);
        file.write_all(b"verified").unwrap();
        file.rewind().unwrap();
        let mut returned = file;
        let mut data = Vec::new();
        returned.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"verified");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        assert!(download_file(&dir.0, (1 << 20) + 1, budget()).is_err());
        assert!(
            download_file(
                &dir.0,
                1,
                ReadStorageBudget {
                    headroom_bytes: u64::MAX,
                    ..budget()
                }
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn cache_eviction_skips_pins_and_unknown_files_and_releases_unused_tail() {
        let dir = Directory::new();
        let pinned = cached(&dir.0, 1);
        let victim = cached(&dir.0, 2);
        reserve_cache(&pinned, 1 << 20);
        reserve_cache(&victim, 1 << 20);
        let marker = dir.0.join("operator-notes");
        fs::write(&marker, b"keep").unwrap();
        let pin = CachePin::open(&pinned).unwrap();
        let mut builder = CacheBuild::new(&dir.0, budget()).unwrap();
        let path = dir
            .0
            .join(format!(".extent-building-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=OFF;")
            .unwrap();
        // A 16 MiB chunk plus the pinned file exceeds the directory quota.
        assert!(builder.admit(&conn, &path, 0).is_err());
        assert!(pinned.exists());
        assert!(!victim.exists());
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        drop(pin);
        drop(builder);
        drop(conn);
        fs::remove_file(path).unwrap();
        let mut builder = CacheBuild::new(&dir.0, budget()).unwrap();
        let path = dir
            .0
            .join(format!(".extent-building-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=OFF;")
            .unwrap();
        // Unknown marker is deliberately retained; grant room for that inode.
        builder.budget.cache_bytes += 1 << 20;
        builder.admit(&conn, &path, 0).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1);")
            .unwrap();
        drop(conn);
        let before = builder.file.as_ref().unwrap().metadata().unwrap().blocks() * 512;
        builder.finish().unwrap();
        let after = builder.file.as_ref().unwrap().metadata().unwrap().blocks() * 512;
        assert!(
            after < before,
            "unused KEEP_SIZE allocation must be released"
        );
        assert!(after >= fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn startup_trim_enforces_lower_budget_and_preserves_active_pins() {
        let dir = Directory::new();
        let pinned = cached(&dir.0, 1);
        let victim = cached(&dir.0, 2);
        reserve_cache(&pinned, 1 << 20);
        reserve_cache(&victim, 1 << 20);
        let pin = CachePin::open(&pinned).unwrap();
        let mut storage = CacheBuild::new(
            &dir.0,
            ReadStorageBudget {
                cache_bytes: 3 << 19,
                ..budget()
            },
        )
        .unwrap();
        storage.trim_to_budget().unwrap();
        assert!(pinned.exists());
        assert!(!victim.exists());
        storage.budget.cache_bytes = 1 << 19;
        assert!(storage.trim_to_budget().is_err());
        assert!(pinned.exists());
        drop(pin);
        drop(storage);
        CacheBuild::new(
            &dir.0,
            ReadStorageBudget {
                cache_bytes: 1 << 19,
                ..budget()
            },
        )
        .unwrap()
        .trim_to_budget()
        .unwrap();
        assert!(!pinned.exists());
    }

    #[test]
    fn cache_directory_lock_and_stale_build_cleanup_are_scoped() {
        let dir = Directory::new();
        let stale = dir
            .0
            .join(format!(".extent-building-{}.db", Uuid::new_v4()));
        fs::write(&stale, b"abandoned").unwrap();
        let unknown = dir.0.join(".extent-building-not-a-uuid.db");
        fs::write(&unknown, b"keep").unwrap();
        let builder = CacheBuild::new(&dir.0, budget()).unwrap();
        assert!(!stale.exists());
        assert!(unknown.exists());
        assert!(CacheBuild::new(&dir.0, budget()).is_err());
        drop(builder);
        CacheBuild::new(&dir.0, budget()).unwrap();
        assert!(unknown.exists());
    }

    #[test]
    #[ignore = "需要 TAPE_RS_SCRATCH_TEST_DIR 指向独占 24 MiB 文件系统；不接触磁带"]
    fn constrained_filesystem_download_reservation_prevents_overcommit() {
        let path = PathBuf::from(
            std::env::var_os("TAPE_RS_SCRATCH_TEST_DIR").expect("explicit filesystem"),
        );
        let stat = nix::sys::statvfs::statvfs(&path).unwrap();
        assert_eq!(stat.blocks() * stat.fragment_size(), 24 << 20);
        let budget = ReadStorageBudget {
            download_bytes: 32 << 20,
            headroom_bytes: 0,
            ..budget()
        };
        let file = download_file(&path, 16 << 20, budget).unwrap();
        assert!(download_file(&path, 16 << 20, budget).is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        drop(file);
        drop(download_file(&path, 16 << 20, budget).unwrap());
        // The cache policy allows 64 MiB, but this filesystem only has 24.
        // Admission must reclaim real blocks rather than just count quota.
        let budget = ReadStorageBudget {
            cache_bytes: 64 << 20,
            ..budget
        };
        let victim = cached(&path, 1);
        reserve_cache(&victim, 16 << 20);
        let mut storage = CacheBuild::new(&path, budget).unwrap();
        let building = path.join(format!(".extent-building-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&building).unwrap();
        conn.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=OFF;")
            .unwrap();
        storage.admit(&conn, &building, 0).unwrap();
        assert!(
            !victim.exists(),
            "filesystem pressure reclaims the unused cache"
        );
        drop(conn);
        storage.finish().unwrap();
        drop(storage);
        fs::remove_file(building).unwrap();
        reserve_cache(&victim, 16 << 20);
        let pin = CachePin::open(&victim).unwrap();
        assert!(
            retry_after_space_reclaim(
                || download_file(&path, 16 << 20, budget),
                || reclaim_cache_space(&path, budget)
            )
            .is_err()
        );
        assert!(victim.exists());
        drop(pin);
        let returned = retry_after_space_reclaim(
            || download_file(&path, 16 << 20, budget),
            || reclaim_cache_space(&path, budget),
        )
        .unwrap();
        assert!(!victim.exists());
        drop(returned);
        fs::remove_file(path.join(".cache-budget.lock")).unwrap();
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
    }
}
