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
