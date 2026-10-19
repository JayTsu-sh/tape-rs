//! IBM EE 保存对象的只读语义视图与普通文件系统恢复。
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use base64::Engine;
use nix::errno::Errno;
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::symlinkat;

use super::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta, Xattr};
use super::volume::LtfsVolume;
use crate::error::{Result, TapeError};

const DATA: &str = ".LTFSEE_DATA/";
const TYPE: &str = "ibm.ltfsee.gpfs.objtype";
const PATH: &str = "ibm.ltfsee.gpfs.path";
const TARGET: &str = "ibm.ltfsee.gpfs.target";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EeObjectKind {
    Directory,
    RegularFile,
    Symlink { target: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EeObject {
    pub object_path: String,
    pub saved_path: String,
    pub aliases: Vec<String>,
    pub kind: EeObjectKind,
    pub length: u64,
    pub meta: NodeMeta,
    pub xattrs: Vec<Xattr>,
}

#[derive(Debug, Clone)]
pub struct EeManifest {
    objects: Vec<EeObject>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EeRestoreReport {
    pub directories: usize,
    pub regular_files: usize,
    pub symlinks: usize,
    pub bytes_written: u64,
}

impl EeManifest {
    /// 仅解释 `.LTFSEE_DATA` 中有 EE 属性的保存对象；不修改索引或 objgen。
    pub fn from_index(index: &LtfsIndex) -> Result<Self> {
        let mut files = BTreeMap::new();
        collect_files(&index.root, "", &mut files)?;
        let mut objects = Vec::new();
        for (path, file) in &files {
            if !path.starts_with(DATA) {
                continue;
            }
            let Some(kind) = attribute(file, TYPE)? else {
                if file.xattrs.iter().any(|x| x.key.starts_with("ibm.ltfsee.")) {
                    return Err(invalid(format!("EE 对象缺少 gpfs.objtype: {path}")));
                }
                continue;
            };
            let saved_path = attribute(file, PATH)?
                .ok_or_else(|| invalid(format!("EE 对象缺少 gpfs.path: {path}")))?;
            let target = attribute(file, TARGET)?;
            let kind = match kind.as_str() {
                "dir" => EeObjectKind::Directory,
                "regfile" => EeObjectKind::RegularFile,
                "symlink" => EeObjectKind::Symlink {
                    target: target
                        .clone()
                        .ok_or_else(|| invalid(format!("EE 链接缺少 gpfs.target: {path}")))?,
                },
                other => return Err(invalid(format!("未知 EE objtype {other}: {path}"))),
            };
            if file.symlink.is_some()
                || (!matches!(kind, EeObjectKind::RegularFile)
                    && (file.length != 0 || !file.extents.is_empty()))
                || (!matches!(kind, EeObjectKind::Symlink { .. }) && target.is_some())
            {
                return Err(invalid(format!("EE 保存对象类型与 LTFS 数据不符: {path}")));
            }
            objects.push(EeObject {
                object_path: path.clone(),
                saved_path,
                aliases: Vec::new(),
                kind,
                length: file.length,
                meta: file.meta.clone(),
                xattrs: file.xattrs.clone(),
            });
        }
        if !objects.is_empty() {
            for (path, file) in &files {
                if file.symlink.is_none() || !ee_alias_candidate(&files, path) {
                    continue;
                }
                let resolved = resolve_alias(&files, path)?;
                if let Some(object) = objects.iter_mut().find(|o| o.object_path == resolved) {
                    object.aliases.push(path.clone());
                } else {
                    return Err(invalid(format!("EE 原始别名未指向已识别保存对象: {path}")));
                }
            }
        }
        Ok(Self { objects })
    }

    pub fn objects(&self) -> &[EeObject] {
        &self.objects
    }

    /// 恢复普通文件、目录和链接，不恢复 GPFS/HSM 登记、权限或私有 xattr。
    ///
    /// destination 必须是空目录或已有父目录下的新目录；所有祖先禁止符号链接。
    /// read 只收到实际 EE 数据对象路径，且必须流式写入其 writer 并返回字节数。
    /// 空文件不调用 read。目录/路径冲突在创建前拒绝；I/O 失败可留下部分恢复，
    /// 调用方应保留该目录作诊断并选择新的空目录重试，不能覆盖重试。
    pub fn restore_with<F>(
        &self,
        destination: impl AsRef<Path>,
        mut read: F,
    ) -> Result<EeRestoreReport>
    where
        F: FnMut(&str, &mut dyn Write) -> Result<u64>,
    {
        let directories = self.preflight()?;
        let (parent, name) = destination_parent(destination.as_ref())?;
        let (root, created) = match directory_at(Some(&parent), Path::new(&name)) {
            Ok(dir) => {
                if !directory_empty(&dir)? {
                    return Err(invalid("EE 恢复目标目录必须为空"));
                }
                (dir, false)
            }
            Err(TapeError::Nix(Errno::ENOENT)) => {
                mkdirat(Some(parent.as_raw_fd()), name.as_os_str(), Mode::S_IRWXU)?;
                (directory_at(Some(&parent), Path::new(&name))?, true)
            }
            Err(e) => return Err(e),
        };
        for path in &directories {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let dir = relative_directory(&root, parent)?;
            mkdirat(Some(dir.as_raw_fd()), name, Mode::S_IRWXU)?;
        }
        let mut report = EeRestoreReport::default();
        for object in &self.objects {
            let path = saved_relative(&object.saved_path)?;
            let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
            let parent = relative_directory(&root, parent_path)?;
            match &object.kind {
                EeObjectKind::Directory => report.directories += 1,
                EeObjectKind::Symlink { target } => {
                    symlinkat(target.as_str(), Some(parent.as_raw_fd()), name)?;
                    report.symlinks += 1;
                }
                EeObjectKind::RegularFile => {
                    let fd = owned_open(
                        Some(&parent),
                        Path::new(name),
                        OFlag::O_WRONLY
                            | OFlag::O_CREAT
                            | OFlag::O_EXCL
                            | OFlag::O_NOFOLLOW
                            | OFlag::O_CLOEXEC,
                        Mode::S_IRUSR | Mode::S_IWUSR,
                    )?;
                    let mut file = File::from(fd);
                    if object.length > 0 {
                        let count = read(&object.object_path, &mut file)?;
                        if count != object.length || file.metadata()?.len() != object.length {
                            return Err(invalid(format!(
                                "EE 恢复数据长度不符: {}，预期 {}，回调 {}，实际 {}",
                                object.object_path,
                                object.length,
                                count,
                                file.metadata()?.len()
                            )));
                        }
                    }
                    file.sync_all()?;
                    report.regular_files += 1;
                    report.bytes_written = report
                        .bytes_written
                        .checked_add(object.length)
                        .ok_or_else(|| invalid("EE 恢复总长度溢出"))?;
                }
            }
        }
        for path in directories.iter().rev() {
            File::from(relative_directory(&root, path)?).sync_all()?;
        }
        File::from(root).sync_all()?;
        if created {
            File::from(parent).sync_all()?;
        }
        Ok(report)
    }

    fn preflight(&self) -> Result<BTreeSet<String>> {
        let mut paths = BTreeMap::new();
        let mut directories = BTreeSet::new();
        for object in &self.objects {
            let path = saved_relative(&object.saved_path)?;
            if paths.insert(path, &object.kind).is_some() {
                return Err(invalid(format!("EE 保存路径冲突: {path}")));
            }
            if let EeObjectKind::Symlink { target } = &object.kind
                && (target.is_empty() || target.contains('\0') || target.len() > 4095)
            {
                return Err(invalid(format!("EE 保存链接目标非法: {path}")));
            }
            if matches!(object.kind, EeObjectKind::Directory) {
                directories.insert(path.to_string());
            }
            let mut ancestor = path;
            while let Some((parent, _)) = ancestor.rsplit_once('/') {
                directories.insert(parent.to_string());
                ancestor = parent;
            }
        }
        for directory in &directories {
            if let Some(kind) = paths.get(directory.as_str())
                && !matches!(kind, EeObjectKind::Directory)
            {
                return Err(invalid(format!("EE 保存路径父级不是目录: {directory}")));
            }
        }
        Ok(directories)
    }
}

/// 从当前卷的已发布索引解释 EE 对象，再通过真实卷读取器流式恢复。
pub fn restore(volume: &LtfsVolume<'_>, destination: impl AsRef<Path>) -> Result<EeRestoreReport> {
    EeManifest::from_index(volume.index())?.restore_with(destination, |path, mut writer| {
        volume.read_file_to_writer(path, &mut writer)
    })
}

fn saved_relative(path: &str) -> Result<&str> {
    let relative = path
        .strip_prefix('/')
        .ok_or_else(|| invalid("EE gpfs.path 必须为绝对保存路径"))?;
    if relative.split('/').any(|part| {
        part.is_empty() || part == "." || part == ".." || part.contains('\0') || part.len() > 255
    }) {
        return Err(invalid(format!("EE 保存路径非法: {path}")));
    }
    Ok(relative)
}

fn owned_open(parent: Option<&OwnedFd>, path: &Path, flags: OFlag, mode: Mode) -> Result<OwnedFd> {
    let raw = openat(parent.map(AsRawFd::as_raw_fd), path, flags, mode)?;
    // SAFETY: openat 成功返回全新且有效的 fd，此处接管唯一所有权，自动关闭。
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn directory_at(parent: Option<&OwnedFd>, path: &Path) -> Result<OwnedFd> {
    owned_open(
        parent,
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

fn relative_directory(root: &OwnedFd, path: &str) -> Result<OwnedFd> {
    let mut dir = root.try_clone()?;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        dir = directory_at(Some(&dir), Path::new(part))?;
    }
    Ok(dir)
}

fn destination_parent(path: &Path) -> Result<(OwnedFd, OsString)> {
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) if !name.as_bytes().contains(&0) && name.len() <= 255 => {
                parts.push(name)
            }
            _ => return Err(invalid("EE 恢复目标路径非法")),
        }
    }
    let (name, parents) = parts
        .split_last()
        .ok_or_else(|| invalid("EE 恢复必须指定目标目录"))?;
    let mut parent = directory_at(None, Path::new(if path.is_absolute() { "/" } else { "." }))?;
    for part in parents {
        parent = directory_at(Some(&parent), Path::new(part))?;
    }
    Ok((parent, name.to_os_string()))
}

fn directory_empty(dir: &OwnedFd) -> Result<bool> {
    // 单独 open 避免共享目录游标。fdopendir 成功后由 closedir 唯一负责关闭 fd。
    let raw = directory_at(Some(dir), Path::new("."))?.into_raw_fd();
    // SAFETY: raw 是新打开的有效目录 fd，所有权在成功时转交给 DIR。
    let stream = unsafe { nix::libc::fdopendir(raw) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: fdopendir 失败没有接管 raw，重新接管以关闭它。
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        return Err(error.into());
    }
    let result = loop {
        Errno::clear();
        // SAFETY: stream 是本函数独占的有效 DIR，直到下方 closedir。
        let entry = unsafe { nix::libc::readdir(stream) };
        if entry.is_null() {
            let errno = Errno::last();
            break if errno == Errno::UnknownErrno {
                Ok(true)
            } else {
                Err(errno.into())
            };
        }
        // SAFETY: readdir 的非空结果在下一次调用前有效，d_name 以 NUL 结束。
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            break Ok(false);
        }
    };
    // SAFETY: stream 尚未关闭；closedir 释放 DIR 并关闭其唯一拥有的 fd。
    unsafe {
        nix::libc::closedir(stream);
    }
    result
}

fn invalid(message: impl Into<String>) -> TapeError {
    TapeError::Ltfs(message.into())
}

fn attribute(file: &FileNode, key: &str) -> Result<Option<String>> {
    let mut values = file.xattrs.iter().filter(|x| x.key == key);
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid(format!("EE 语义属性重复: {key}")));
    }
    if !value.base64 {
        return Ok(Some(value.value.clone()));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&value.value)
        .map_err(|_| invalid(format!("EE 语义属性 base64 无效: {key}")))?;
    Ok(Some(String::from_utf8(bytes).map_err(|_| {
        invalid(format!("EE 语义属性不是 UTF-8: {key}"))
    })?))
}

fn collect_files<'a>(
    dir: &'a DirectoryNode,
    prefix: &str,
    out: &mut BTreeMap<String, &'a FileNode>,
) -> Result<()> {
    let mut names = BTreeSet::new();
    for name in dir
        .files
        .iter()
        .map(|f| &f.name)
        .chain(dir.subdirs.iter().map(|d| &d.name))
    {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\0')
            || name.len() > 255
            || !names.insert(name)
        {
            return Err(invalid(format!(
                "EE 视图中的原始索引名称非法或重复: {prefix}{name}"
            )));
        }
    }
    for file in &dir.files {
        out.insert(format!("{prefix}{}", file.name), file);
    }
    for child in &dir.subdirs {
        collect_files(child, &format!("{prefix}{}/", child.name), out)?;
    }
    Ok(())
}

fn resolve_alias(files: &BTreeMap<String, &FileNode>, path: &str) -> Result<String> {
    let mut path = path.to_string();
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(path.clone()) {
            return Err(invalid(format!("EE 原始别名形成循环: {path}")));
        }
        let file = files
            .get(&path)
            .ok_or_else(|| invalid(format!("EE 原始别名目标不存在: {path}")))?;
        let Some(target) = &file.symlink else {
            return Ok(path);
        };
        path = alias_target(&path, target)?;
    }
}

fn ee_alias_candidate(files: &BTreeMap<String, &FileNode>, start: &str) -> bool {
    let mut path = start.to_string();
    let mut visited = BTreeSet::new();
    while visited.insert(path.clone()) {
        let Some(file) = files.get(&path) else {
            break;
        };
        let Some(target) = &file.symlink else {
            break;
        };
        if file.xattr("ltfs.vendor.IBM.prefixLength").is_some()
            || target.split('/').any(|part| part == ".LTFSEE_DATA")
        {
            return true;
        }
        let Ok(next) = alias_target(&path, target) else {
            break;
        };
        path = next;
    }
    false
}

fn alias_target(path: &str, target: &str) -> Result<String> {
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return Err(invalid(format!("EE 原始别名目标非法: {path}")));
    }
    let mut parts: Vec<_> = path.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| invalid(format!("EE 原始别名越出卷根: {path}")))?;
            }
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}
