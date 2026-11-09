//! Index-only copy of raw file attributes over a committed target body.
//! LTFS 2.5.1 sections 6.1/6.3.2 permit distinct files to share data extents.
//! The managed actor must separately persist its intent and prove both bodies.
use super::*;
use crate::ltfs::index::{Extent, IndexEntry, IndexLocation, XATTR_MD5, XATTR_SHA256};
use crate::managed_catalog::ContentDigest;

fn md5(node: &FileNode) -> Result<ContentDigest> {
    let attrs: Vec<_> = node
        .xattrs
        .iter()
        .filter(|attr| attr.key == XATTR_MD5)
        .cloned()
        .collect();
    ContentDigest::from_ltfs_xattrs(&attrs).map_err(|e| error(&e.to_string()))
}

fn require_body(source: &FileNode, body: &FileNode) -> Result<()> {
    for node in [source, body] {
        relocation::attributes(&node.meta, &node.xattrs)?;
        if node.delta_fields.is_some()
            || node.open_for_write == Some(true)
            || node.symlink.is_some()
        {
            return Err(error("正文引用需要完整关闭的普通文件"));
        }
    }
    let original =
        ContentDigest::from_ltfs_xattrs(&source.xattrs).map_err(|e| error(&e.to_string()))?;
    let actual =
        ContentDigest::from_ltfs_xattrs(&body.xattrs).map_err(|e| error(&e.to_string()))?;
    // Values here are index metadata, never a substitute for actual byte
    // verification by the source reader and destination CRC/readback path.
    if source.length != body.length
        || matches!(original, ContentDigest::Sha256(_) if original != actual)
        || matches!(md5(source)?, ContentDigest::Md5(value) if md5(body)? != ContentDigest::Md5(value))
    {
        return Err(error("回收原始文件与已提交正文的长度或已知摘要不一致"));
    }
    Ok(())
}

impl FileNode {
    /// Deterministic new-body recipe. Source extents never reach the target;
    /// known hashes must match and a previously unknown SHA is recorded.
    pub(crate) fn reclaimed_body(
        path: &str,
        uid: u64,
        source: &IndexEntry,
        at: IndexLocation,
        sha: [u8; 32],
        md5_sum: Option<[u8; 16]>,
    ) -> Result<Self> {
        let IndexEntry::File { node: original, .. } = source else {
            return Err(error("原始正文必须是普通文件"));
        };
        let mut node = Self::reclaimed_reference(path, uid, original, source)?;
        if !matches!(at.partition, 'a' | 'b')
            || at.start_block == 0
            || parts(path)?.join("/") != path.strip_prefix('/').unwrap_or(path)
            || matches!(ContentDigest::from_ltfs_xattrs(&original.xattrs).map_err(|e|error(&e.to_string()))?,ContentDigest::Sha256(value) if value!=sha)
            || md5(original)? != md5_sum.map_or(ContentDigest::Unknown, ContentDigest::Md5)
        {
            return Err(error("原始正文目的位置或已知摘要与固定计划不同"));
        }
        node.extents = if node.length == 0 {
            Vec::new()
        } else {
            vec![Extent {
                partition: at.partition,
                start_block: at.start_block,
                byte_offset: 0,
                byte_count: node.length,
                file_offset: 0,
            }]
        };
        node.set_xattr(
            XATTR_SHA256,
            &sha.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
        if let Some(md5) = md5_sum {
            node.set_xattr(
                XATTR_MD5,
                &md5.iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            );
        }
        Ok(node)
    }
    /// Pure staging recipe shared by the durable reclaim planner and writer.
    /// The caller must separately prove actual source/donor bytes and scope.
    pub(crate) fn reclaimed_reference(
        path: &str,
        uid: u64,
        body: &FileNode,
        source: &IndexEntry,
    ) -> Result<Self> {
        let IndexEntry::File { node: source, .. } = source else {
            return Err(error("正文引用的原节点必须是普通文件"));
        };
        require_body(source, body)?;
        let p = parts(path)?;
        let name = p.last().ok_or_else(|| error("不能用文件覆盖根目录"))?;
        if uid <= 1 || !crate::ltfs::xml_text_valid(path) {
            return Err(error("正文引用的目的 UID 或路径无效"));
        }
        let mut meta = source.meta.clone();
        meta.file_uid = uid;
        Ok(Self {
            name: name.to_string(),
            length: source.length,
            meta,
            extents: body.extents.clone(),
            xattrs: source.xattrs.clone(),
            open_for_write: source.open_for_write,
            ..Default::default()
        })
    }
}

impl LtfsIndex {
    /// Check the actual append's geometry and hashes before restoring the
    /// source's exact closed-state representation and selected hash policy.
    pub(in crate::ltfs) fn finish_reclaimed_body(
        &mut self,
        path: &str,
        expected: &FileNode,
    ) -> Result<()> {
        let p = parts(path)?;
        let (name, parents) = p.split_last().ok_or_else(|| error("正文不能覆盖卷根"))?;
        let target = dir_mut(&mut self.root, parents)?
            .files
            .iter_mut()
            .find(|node| node.name == *name)
            .ok_or_else(|| error("原始正文追加后的节点缺失"))?;
        require_body(expected, target)?;
        let mut actual = target.clone();
        actual.xattrs = expected.xattrs.clone();
        actual.open_for_write = expected.open_for_write;
        if actual != *expected {
            return Err(error("原始正文实际追加位置、UID 或元数据偏离计划"));
        }
        *target = actual;
        Ok(())
    }
    pub(in crate::ltfs) fn reference_reclaimed_body(
        &mut self,
        path: &str,
        uid: u64,
        body: &FileNode,
        source: &IndexEntry,
    ) -> Result<()> {
        let node = FileNode::reclaimed_reference(path, uid, body, source)?;
        let next_uid = self
            .highest_file_uid
            .checked_add(1)
            .filter(|value| *value > 1)
            .ok_or_else(|| error("UID 空间耗尽"))?;
        if uid != next_uid || !crate::ltfs::xml_text_valid(path) {
            return Err(error("正文引用的目的 UID 或路径与原计划不符"));
        }
        let p = parts(path)?;
        let (name, parents) = p
            .split_last()
            .ok_or_else(|| error("不能用文件覆盖根目录"))?;
        let parent = dir_mut(&mut self.root, parents)?;
        if parent.files.iter().any(|node| node.name == *name)
            || parent.subdirs.iter().any(|node| node.name == *name)
        {
            return Err(error("正文引用的目的路径已存在"));
        }
        // All fallible checks precede mutation. Keep the source attributes,
        // but never its physical UID, name or source-volume block addresses.
        parent.files.push(node);
        self.highest_file_uid = uid;
        Ok(())
    }
}

#[cfg(test)]
#[path = "namespace_body_reference_tests.rs"]
mod tests;
