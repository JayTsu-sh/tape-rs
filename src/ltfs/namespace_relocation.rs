//! Preserve raw directory/link attributes without creating nodes or touching
//! parent timestamps. The managed caller must first admit its reclaim intent.
use super::*;
use crate::ltfs::index::{IndexEntry, MetadataChange};
use base64::Engine;
use std::collections::HashSet;

pub(super) fn attributes(meta: &NodeMeta, xattrs: &[Xattr]) -> Result<()> {
    if meta.file_uid == 0 {
        return Err(error("回收源节点 UID 不能为零"));
    }
    for time in [
        &meta.creation_time,
        &meta.change_time,
        &meta.modify_time,
        &meta.access_time,
        &meta.backup_time,
    ] {
        if time.len() != 30 {
            return Err(error("回收源时间必须为规范 LTFS UTC 纳秒格式"));
        }
        MetadataChange {
            access_time: Some(time.clone()),
            ..Default::default()
        }
        .validate()?;
    }
    let mut keys = HashSet::new();
    for attr in xattrs {
        if attr.key.is_empty()
            || !crate::ltfs::xml_text_valid(&attr.key)
            || !crate::ltfs::xml_text_valid(&attr.value)
            || attr.base64
                && base64::engine::general_purpose::STANDARD
                    .decode(&attr.value)
                    .is_err()
        {
            return Err(error("回收源扩展属性名称、XML 文本或 Base64 编码无效"));
        }
        if !keys.insert(&attr.key) {
            return Err(error("回收源节点包含重复扩展属性"));
        }
    }
    Ok(())
}

impl IndexEntry {
    /// Pure fixed-UID recipe for a newly created reclaim metadata node. The
    /// caller separately proves source capture, destination and write intent.
    pub(crate) fn reclaimed_metadata_node(&self, path: &str, uid: u64) -> Result<Self> {
        self.validate_reclaimed_metadata()?;
        let relative = path.strip_prefix('/').unwrap_or(path);
        let p = parts(path)?;
        let name = p.last().ok_or_else(|| error("回收新节点不能覆盖卷根"))?;
        if uid <= 1 || !super::super::xml_text_valid(path) || p.join("/") != relative {
            return Err(error("回收新节点的 UID 或规范路径无效"));
        }
        Ok(match self {
            Self::Directory { node, .. } => {
                let mut node = node.clone();
                node.name = (*name).into();
                node.meta.file_uid = uid;
                Self::Directory {
                    path: relative.into(),
                    node,
                }
            }
            Self::File { node, .. } => {
                let mut node = node.clone();
                node.name = (*name).into();
                node.meta.file_uid = uid;
                Self::File {
                    path: relative.into(),
                    node,
                }
            }
        })
    }
    /// Shared staging/readback validation for a complete raw metadata node.
    pub(crate) fn validate_reclaimed_metadata(&self) -> Result<()> {
        match self {
            Self::Directory { node, .. } => {
                if node.delta_fields.is_some() || !node.files.is_empty() || !node.subdirs.is_empty()
                {
                    return Err(error("回收目录属性需要完整的无子树源节点"));
                }
                attributes(&node.meta, &node.xattrs)
            }
            Self::File { node, .. } => {
                if node.delta_fields.is_some()
                    || node.open_for_write == Some(true)
                    || node.symlink.as_ref().is_none_or(|target| {
                        target.is_empty() || !crate::ltfs::xml_text_valid(target)
                    })
                    || !node.extents.is_empty()
                {
                    return Err(error("回收节点属性只接受完整目录或无正文的符号链接"));
                }
                attributes(&node.meta, &node.xattrs)
            }
        }
    }
}

impl LtfsIndex {
    /// Preserve a colliding target file under one fixed root name. Only its
    /// name changes; UID, extents, link text and every attribute remain exact.
    #[cfg(feature = "fdb")]
    pub(in crate::ltfs) fn displace_reclaimed_file(
        &mut self,
        path: &str,
        uid: u64,
        archive: &str,
    ) -> Result<()> {
        let src = parts(path)?;
        let dst = parts(archive)?;
        let (name, parents) = src.split_last().ok_or_else(|| error("不能移动卷根"))?;
        if dst.len() != 1
            || archive != dst[0]
            || self.root.files.iter().any(|n| n.name == archive)
            || self.root.subdirs.iter().any(|n| n.name == archive)
        {
            return Err(error("同名节点保留路径必须为未占用的固定根路径"));
        }
        let parent = dir_mut(&mut self.root, parents)?;
        let at = parent
            .files
            .iter()
            .position(|n| {
                n.name == *name
                    && n.meta.file_uid == uid
                    && n.delta_fields.is_none()
                    && n.open_for_write != Some(true)
            })
            .ok_or_else(|| error("同名节点的原文件身份不符"))?;
        let mut node = parent.files.remove(at);
        node.name = archive.into();
        self.root.files.push(node);
        Ok(())
    }

    /// Add exactly one fixed raw directory/link. Unlike a user mkdir/symlink,
    /// this preserves both source attributes and every existing parent field.
    pub(in crate::ltfs) fn reference_reclaimed_node(
        &mut self,
        path: &str,
        uid: u64,
        source: &IndexEntry,
    ) -> Result<()> {
        let node = source.reclaimed_metadata_node(path, uid)?;
        if self.highest_file_uid.checked_add(1) != Some(uid) {
            return Err(error("回收新节点 UID 与原固定计划不符"));
        }
        let p = parts(path)?;
        let (name, parents) = p
            .split_last()
            .ok_or_else(|| error("回收新节点不能覆盖卷根"))?;
        let parent = dir_mut(&mut self.root, parents)?;
        if parent.files.iter().any(|node| node.name == *name)
            || parent.subdirs.iter().any(|node| node.name == *name)
        {
            return Err(error("回收新节点路径已存在"));
        }
        // No fallible work follows the first mutation.
        match node {
            IndexEntry::Directory { node, .. } => parent.subdirs.push(node),
            IndexEntry::File { node, .. } => parent.files.push(node),
        }
        self.highest_file_uid = uid;
        Ok(())
    }
    /// Metadata only: target existence/type and the whole source are checked
    /// before mutation. Source UID and subtree/data extents are never copied.
    pub(in crate::ltfs) fn preserve_reclaimed_metadata(
        &mut self,
        path: &str,
        source: &IndexEntry,
    ) -> Result<()> {
        let p = parts(path)?;
        source.validate_reclaimed_metadata()?;
        match source {
            IndexEntry::Directory { node, .. } => {
                let target = dir_mut(&mut self.root, &p)?;
                if target.meta.file_uid == 0 || target.delta_fields.is_some() {
                    return Err(error("回收目的目录尚未完整初始化"));
                }
                let uid = target.meta.file_uid;
                target.meta = node.meta.clone();
                target.meta.file_uid = uid;
                target.xattrs = node.xattrs.clone();
            }
            IndexEntry::File { node, .. } => {
                let (name, parents) = p
                    .split_last()
                    .ok_or_else(|| error("根节点不能是符号链接"))?;
                let target = dir_mut(&mut self.root, parents)?
                    .files
                    .iter_mut()
                    .find(|f| f.name == *name)
                    .ok_or_else(|| error("回收目的符号链接不存在"))?;
                if target.meta.file_uid == 0
                    || target.delta_fields.is_some()
                    || target.open_for_write == Some(true)
                    || target.symlink != node.symlink
                    || target.length != node.length
                    || !target.extents.is_empty()
                {
                    return Err(error("回收目的节点不是相同目标与长度的完整符号链接"));
                }
                let uid = target.meta.file_uid;
                target.meta = node.meta.clone();
                target.meta.file_uid = uid;
                target.xattrs = node.xattrs.clone();
                target.open_for_write = node.open_for_write;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "namespace_relocation_tests.rs"]
mod tests;
