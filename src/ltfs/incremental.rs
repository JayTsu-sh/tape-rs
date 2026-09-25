//! LTFS 2.5 §9.2 的增量差分与原子重放。介质定位与链验证由 recovery 负责。
use std::collections::BTreeSet;

use super::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta};
use crate::error::{Result, TapeError};

type Fields = Option<BTreeSet<String>>;
fn fields(names: &[&str]) -> Fields {
    Some(names.iter().map(|s| s.to_string()).collect())
}
fn has(f: &Fields, key: &str) -> bool {
    f.as_ref().is_none_or(|s| s.contains(key))
}
fn deleted(f: &Fields) -> bool {
    f.as_ref().is_some_and(|s| s.contains("deleted"))
}
fn bad(s: impl Into<String>) -> TapeError {
    TapeError::Ltfs(format!("增量索引: {}", s.into()))
}

impl LtfsIndex {
    /// 从两个完整内存视图生成标准增量；新视图的位置、代数由调用方先设置。
    pub fn incremental_since(&self, base: &Self) -> Result<Self> {
        if self.volume_uuid != base.volume_uuid || self.generation <= base.generation {
            return Err(bad("卷身份或代数不匹配"));
        }
        let mut delta = self.clone();
        delta.version = "2.5.0".into();
        delta.incremental = true;
        delta.materialized = false;
        delta.previous_location = if base.incremental {
            base.previous_location
        } else {
            Some(base.self_location)
        };
        delta.previous_incremental_location = base.incremental.then_some(base.self_location);
        delta.root = difference(&self.root, &base.root);
        delta.validate_incremental()?;
        Ok(delta)
    }

    pub(crate) fn validate_incremental(&self) -> Result<()> {
        if !self.incremental {
            return Ok(());
        }
        if self.version != "2.5.0" || self.generation == 0 || self.previous_location.is_none() {
            return Err(bad("缺少完整基线、有效代数或 2.5.0 格式版本"));
        }
        for loc in [self.previous_location, self.previous_incremental_location]
            .into_iter()
            .flatten()
        {
            if loc.partition != self.self_location.partition
                || loc.start_block >= self.self_location.start_block
            {
                return Err(bad("前驱位置或分区非法"));
            }
        }
        if !self.unknown_elements.is_empty() {
            return Err(bad("包含未知元素，不能安全重放"));
        }
        validate_dir(&self.root, true)
    }

    /// 原子应用：失败时 base 不变。返回完整内存视图，保留末增量的介质头信息。
    pub fn apply_incremental(&self, delta: &Self) -> Result<Self> {
        delta.validate_incremental()?;
        if !delta.incremental
            || delta.volume_uuid != self.volume_uuid
            || delta.generation <= self.generation
            || (delta.highest_file_uid != 0
                && (self.highest_file_uid == 0 || delta.highest_file_uid < self.highest_file_uid))
        {
            return Err(bad("卷、代数或 highestfileuid 不匹配"));
        }
        let full = if self.incremental {
            self.previous_location
        } else {
            Some(self.self_location)
        };
        if delta.previous_location != full
            || delta.previous_incremental_location != self.incremental.then_some(self.self_location)
        {
            return Err(bad("完整基线或增量前驱不匹配"));
        }
        let mut root = self.root.clone();
        remove_deleted(&mut root, &delta.root);
        merge_dir(&mut root, &delta.root, true)?;
        let mut seen = BTreeSet::new();
        validate_uids(&root, delta.highest_file_uid, &mut seen)?;
        let mut out = delta.clone();
        out.root = root;
        out.materialized = true;
        out.allow_policy_update = self.allow_policy_update;
        Ok(out)
    }
}

fn difference(new: &DirectoryNode, old: &DirectoryNode) -> DirectoryNode {
    let mut out = new.clone();
    out.files.clear();
    out.subdirs.clear();
    if new.meta == old.meta && new.xattrs == old.xattrs {
        out.delta_fields = fields(&["name", "contents"]);
    } else {
        out.delta_fields = None;
    }
    for file in &new.files {
        if !old.files.iter().any(|f| f == file) {
            out.files.push(file.clone());
        }
    }
    for file in &old.files {
        if !new.files.iter().any(|f| f.name == file.name)
            && !new.subdirs.iter().any(|d| d.name == file.name)
        {
            out.files.push(FileNode {
                name: file.name.clone(),
                delta_fields: fields(&["name", "deleted"]),
                ..Default::default()
            });
        }
    }
    for dir in &new.subdirs {
        match old
            .subdirs
            .iter()
            .find(|d| d.name == dir.name && d.meta.file_uid == dir.meta.file_uid)
        {
            Some(prior) if prior == dir => {}
            Some(prior) => out.subdirs.push(difference(dir, prior)),
            None => out.subdirs.push(dir.clone()),
        }
    }
    for dir in &old.subdirs {
        if !new.subdirs.iter().any(|d| d.name == dir.name)
            && !new.files.iter().any(|f| f.name == dir.name)
        {
            out.subdirs.push(DirectoryNode {
                name: dir.name.clone(),
                delta_fields: fields(&["name", "deleted"]),
                ..Default::default()
            });
        }
    }
    out
}

fn validate_fields(f: &Fields, name: &str, directory: bool, root: bool) -> Result<()> {
    if (!root && name.is_empty())
        || name.contains('/')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(bad("非法对象名"));
    }
    if deleted(f) {
        if root || f.as_ref().unwrap().len() != 2 || !has(f, "name") {
            return Err(bad("删除项只能包含 name 和空 deleted"));
        }
    } else {
        if !has(f, "name") || (directory && !has(f, "contents")) {
            return Err(bad("缺少 name/contents"));
        }
        if !has(f, "fileuid") && (!directory || f.as_ref().unwrap().len() != 2) {
            return Err(bad("修改缺少 fileuid"));
        }
    }
    Ok(())
}

fn validate_dir(d: &DirectoryNode, root: bool) -> Result<()> {
    validate_fields(&d.delta_fields, &d.name, true, root)?;
    let mut names = BTreeSet::new();
    for f in &d.files {
        validate_fields(&f.delta_fields, &f.name, false, false)?;
        if f.delta_fields
            .as_ref()
            .is_some_and(|v| v.contains("symlink") && v.contains("extentinfo"))
        {
            return Err(bad("symlink 与 extentinfo 互斥"));
        }
        if !names.insert(&f.name) {
            return Err(bad("同一路径出现多次"));
        }
    }
    for child in &d.subdirs {
        if !names.insert(&child.name) {
            return Err(bad("同一路径出现多次"));
        }
        validate_dir(child, false)?;
    }
    Ok(())
}

fn complete(f: &Fields, directory: bool) -> Result<()> {
    for key in [
        "name",
        "fileuid",
        "readonly",
        "creationtime",
        "changetime",
        "modifytime",
    ] {
        if !has(f, key) {
            return Err(bad(format!("新对象缺少 {key}")));
        }
    }
    if !directory && !has(f, "length") {
        return Err(bad("新文件缺少 length"));
    }
    Ok(())
}

fn remove_deleted(dst: &mut DirectoryNode, delta: &DirectoryNode) {
    for f in &delta.files {
        if deleted(&f.delta_fields) {
            dst.files.retain(|v| v.name != f.name);
        }
    }
    for d in &delta.subdirs {
        if deleted(&d.delta_fields) {
            dst.subdirs.retain(|v| v.name != d.name);
        } else if let Some(child) = dst.subdirs.iter_mut().find(|v| v.name == d.name) {
            remove_deleted(child, d);
        }
    }
}

fn merge_meta(dst: &mut NodeMeta, src: &NodeMeta, f: &Fields) {
    if has(f, "fileuid") {
        dst.file_uid = src.file_uid;
    }
    if has(f, "readonly") {
        dst.readonly = src.readonly;
    }
    for (key, out, value) in [
        ("creationtime", &mut dst.creation_time, &src.creation_time),
        ("changetime", &mut dst.change_time, &src.change_time),
        ("modifytime", &mut dst.modify_time, &src.modify_time),
        ("accesstime", &mut dst.access_time, &src.access_time),
        ("backuptime", &mut dst.backup_time, &src.backup_time),
    ] {
        if has(f, key) {
            *out = value.clone();
        }
    }
}

fn merge_dir(dst: &mut DirectoryNode, patch: &DirectoryNode, root: bool) -> Result<()> {
    if root && has(&patch.delta_fields, "fileuid") && dst.meta.file_uid != patch.meta.file_uid {
        return Err(bad("根目录 UID 变化"));
    }
    if root {
        dst.name = patch.name.clone();
    }
    merge_meta(&mut dst.meta, &patch.meta, &patch.delta_fields);
    if has(&patch.delta_fields, "extendedattributes") {
        dst.xattrs = patch.xattrs.clone();
    }
    dst.delta_fields = None;
    for f in &patch.files {
        if deleted(&f.delta_fields) {
            continue;
        }
        let existing = dst
            .files
            .iter()
            .position(|v| v.name == f.name && v.meta.file_uid == f.meta.file_uid);
        if let Some(i) = existing {
            let out = &mut dst.files[i];
            merge_meta(&mut out.meta, &f.meta, &f.delta_fields);
            if has(&f.delta_fields, "length") {
                out.length = f.length;
            }
            if has(&f.delta_fields, "openforwrite") {
                out.open_for_write = f.open_for_write;
            }
            if has(&f.delta_fields, "extentinfo") {
                out.extents = f.extents.clone();
                out.symlink = None;
            }
            if f.symlink.is_some() && has(&f.delta_fields, "symlink") {
                out.symlink = f.symlink.clone();
                out.extents.clear();
            }
            if has(&f.delta_fields, "extendedattributes") {
                out.xattrs = f.xattrs.clone();
            }
            out.delta_fields = None;
        } else {
            complete(&f.delta_fields, false)?;
            dst.files.retain(|v| v.name != f.name);
            dst.subdirs.retain(|v| v.name != f.name);
            let mut node = f.clone();
            node.delta_fields = None;
            dst.files.push(node);
        }
    }
    for d in &patch.subdirs {
        if deleted(&d.delta_fields) {
            continue;
        }
        let existing = dst.subdirs.iter().position(|v| {
            v.name == d.name
                && (!has(&d.delta_fields, "fileuid") || v.meta.file_uid == d.meta.file_uid)
        });
        if let Some(i) = existing {
            merge_dir(&mut dst.subdirs[i], d, false)?;
        } else {
            complete(&d.delta_fields, true)?;
            dst.files.retain(|v| v.name != d.name);
            dst.subdirs.retain(|v| v.name != d.name);
            let mut node = DirectoryNode {
                name: d.name.clone(),
                ..Default::default()
            };
            merge_dir(&mut node, d, false)?;
            dst.subdirs.push(node);
        }
    }
    Ok(())
}

fn validate_uids(d: &DirectoryNode, highest: u64, seen: &mut BTreeSet<u64>) -> Result<()> {
    for uid in std::iter::once(d.meta.file_uid).chain(d.files.iter().map(|f| f.meta.file_uid)) {
        if uid == 0 || (highest != 0 && uid > highest) || !seen.insert(uid) {
            return Err(bad("UID 为零、重复或超过 highestfileuid"));
        }
    }
    for child in &d.subdirs {
        validate_uids(child, highest, seen)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{Extent, IndexLocation, Xattr};
    fn base() -> LtfsIndex {
        let mut i = LtfsIndex::empty(uuid::Uuid::nil(), "test".into(), 'b');
        i.self_location.start_block = 5;
        i.highest_file_uid = 2;
        i.root.files.push(FileNode {
            name: " old ".into(),
            length: 3,
            meta: NodeMeta {
                file_uid: 2,
                ..i.root.meta.clone()
            },
            extents: vec![Extent {
                partition: 'b',
                start_block: 10,
                byte_offset: 0,
                byte_count: 3,
                file_offset: 0,
            }],
            xattrs: vec![Xattr {
                key: "a".into(),
                value: "1".into(),
                base64: false,
            }],
            ..Default::default()
        });
        i
    }
    fn next(i: &LtfsIndex) -> LtfsIndex {
        let mut n = i.clone();
        n.generation += 1;
        n.self_location = IndexLocation {
            partition: 'b',
            start_block: i.self_location.start_block + 20,
        };
        n
    }
    #[test]
    fn rename_delete_and_recreate_roundtrip() {
        let old = base();
        let mut n = next(&old);
        n.root.files[0].name = "new".into();
        let delta = n.incremental_since(&old).unwrap();
        let xml = delta.to_xml().unwrap();
        assert!(std::str::from_utf8(&xml).unwrap().contains("<deleted/>"));
        let parsed = LtfsIndex::parse(&xml).unwrap();
        let applied = old.apply_incremental(&parsed).unwrap();
        assert_eq!(applied.root, n.root);
        let mut recreated = next(&applied);
        recreated.highest_file_uid = 3;
        recreated.root.files[0].meta.file_uid = 3;
        let delta = recreated.incremental_since(&applied).unwrap();
        assert_eq!(delta.root.files.len(), 1);
        assert_eq!(
            applied
                .apply_incremental(&LtfsIndex::parse(&delta.to_xml().unwrap()).unwrap())
                .unwrap()
                .root,
            recreated.root
        );
    }
    #[test]
    fn partial_metadata_keeps_data_and_replaces_xattr_set() {
        let old = base();
        let mut delta = next(&old).incremental_since(&old).unwrap();
        let mut f = old.root.files[0].clone();
        f.meta.readonly = true;
        f.xattrs.clear();
        f.delta_fields = fields(&["name", "fileuid", "readonly", "extendedattributes"]);
        delta.root.files.push(f);
        let parsed = LtfsIndex::parse(&delta.to_xml().unwrap()).unwrap();
        let applied = old.apply_incremental(&parsed).unwrap();
        assert_eq!(applied.root.files[0].extents, old.root.files[0].extents);
        assert_eq!(applied.root.files[0].length, 3);
        assert!(applied.root.files[0].meta.readonly && applied.root.files[0].xattrs.is_empty());
    }
    #[test]
    fn invalid_dependencies_and_partial_creation_are_rejected_atomically() {
        let old = base();
        let mut delta = next(&old).incremental_since(&old).unwrap();
        delta.previous_location.as_mut().unwrap().start_block += 1;
        assert!(old.apply_incremental(&delta).is_err());
        delta.previous_location = Some(old.self_location);
        delta.root.files.push(FileNode {
            name: "new".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            delta_fields: fields(&["name", "fileuid"]),
            ..Default::default()
        });
        delta.highest_file_uid = 3;
        assert!(old.apply_incremental(&delta).is_err());
        assert_eq!(old.root.files.len(), 1);
    }
    #[test]
    fn empty_delta_and_absent_delete_are_valid() {
        let old = base();
        let mut delta = next(&old).incremental_since(&old).unwrap();
        delta.root.files.push(FileNode {
            name: "absent".into(),
            delta_fields: fields(&["name", "deleted"]),
            ..Default::default()
        });
        assert_eq!(
            old.apply_incremental(&LtfsIndex::parse(&delta.to_xml().unwrap()).unwrap())
                .unwrap()
                .root,
            old.root
        );
        let bad = String::from_utf8(delta.to_xml().unwrap().to_vec())
            .unwrap()
            .replace("<deleted/>", "<deleted>true</deleted>");
        assert!(LtfsIndex::parse(bad.as_bytes()).is_err());
    }
    #[test]
    #[ignore = "为独立 XSD 校验输出 XML，不属于默认测试"]
    fn export_xml_for_schema_validation() {
        let mut old = base();
        old.root.files[0].xattrs.clear();
        let mut n = next(&old);
        n.root.files[0].name = "renamed".into();
        let delta = n.incremental_since(&old).unwrap();
        let path =
            std::env::temp_dir().join(format!("tape-rs-incremental-{}.xml", uuid::Uuid::new_v4()));
        std::fs::write(&path, delta.to_xml().unwrap()).unwrap();
        println!("SCHEMA_XML={}", path.display());
    }
}
