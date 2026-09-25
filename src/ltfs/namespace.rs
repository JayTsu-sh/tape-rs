//! 工作索引上的元数据操作。先验证完整变更，再发布到调用方索引；不访问介质。
use super::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta, Xattr};
use crate::error::{Result, TapeError};

fn error(s: &str) -> TapeError {
    TapeError::Ltfs(s.into())
}
fn parts(path: &str) -> Result<Vec<&str>> {
    let out: Vec<_> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if out
        .iter()
        .any(|p| *p == "." || *p == ".." || p.contains('\0'))
    {
        return Err(error("元数据路径非法"));
    }
    Ok(out)
}
fn dir_mut<'a>(root: &'a mut DirectoryNode, path: &[&str]) -> Result<&'a mut DirectoryNode> {
    let mut dir = root;
    for part in path {
        dir = dir
            .subdirs
            .iter_mut()
            .find(|d| d.name == *part)
            .ok_or_else(|| error("父目录不存在"))?;
    }
    Ok(dir)
}
fn touch(d: &mut DirectoryNode) {
    let now = super::label::ltfs_time_now();
    d.meta.change_time = now.clone();
    d.meta.modify_time = now;
}
enum Node {
    File(FileNode),
    Directory(DirectoryNode),
}
fn take(d: &mut DirectoryNode, name: &str) -> Option<Node> {
    if let Some(i) = d.files.iter().position(|f| f.name == name) {
        return Some(Node::File(d.files.remove(i)));
    }
    d.subdirs
        .iter()
        .position(|v| v.name == name)
        .map(|i| Node::Directory(d.subdirs.remove(i)))
}
impl LtfsIndex {
    pub fn find_directory(&self, path: &str) -> Option<&DirectoryNode> {
        let p = parts(path).ok()?;
        let mut node = &self.root;
        for part in p {
            node = node.subdirs.iter().find(|d| d.name == part)?;
        }
        Some(node)
    }

    /// 不含根目录；路径不带开头的 /，与 walk_files 一致。
    pub fn walk_directories(&self, mut f: impl FnMut(&str, &DirectoryNode)) {
        fn walk(d: &DirectoryNode, prefix: &str, f: &mut impl FnMut(&str, &DirectoryNode)) {
            for child in &d.subdirs {
                let p = if prefix.is_empty() {
                    child.name.clone()
                } else {
                    format!("{}/{}", prefix, child.name)
                };
                f(&p, child);
                walk(child, &p, f);
            }
        }
        walk(&self.root, "", &mut f);
    }

    pub fn create_directory(&mut self, path: &str) -> Result<()> {
        let p = parts(path)?;
        let (name, parents) = p.split_last().ok_or_else(|| error("根目录已存在"))?;
        let uid = self
            .highest_file_uid
            .checked_add(1)
            .filter(|n| *n > 1)
            .ok_or_else(|| error("UID 空间耗尽"))?;
        let dir = dir_mut(&mut self.root, parents)?;
        if dir.files.iter().any(|f| f.name == *name) || dir.subdirs.iter().any(|d| d.name == *name)
        {
            return Err(error("路径已存在"));
        }
        let now = super::label::ltfs_time_now();
        dir.subdirs.push(DirectoryNode {
            name: name.to_string(),
            meta: NodeMeta {
                file_uid: uid,
                creation_time: now.clone(),
                change_time: now.clone(),
                modify_time: now.clone(),
                access_time: now.clone(),
                backup_time: now,
                readonly: false,
            },
            ..Default::default()
        });
        touch(dir);
        self.highest_file_uid = uid;
        Ok(())
    }

    pub fn remove_directory(&mut self, path: &str) -> Result<()> {
        let p = parts(path)?;
        let (name, parents) = p.split_last().ok_or_else(|| error("不能删除根目录"))?;
        let dir = dir_mut(&mut self.root, parents)?;
        let i = dir
            .subdirs
            .iter()
            .position(|d| d.name == *name)
            .ok_or_else(|| error("目录不存在"))?;
        if !dir.subdirs[i].files.is_empty() || !dir.subdirs[i].subdirs.is_empty() {
            return Err(error("目录非空"));
        }
        dir.subdirs.remove(i);
        touch(dir);
        Ok(())
    }

    /// 同卷原子 rename，保留源文件/整棵目录树的 UID 与 extent。
    pub fn rename_path(&mut self, from: &str, to: &str) -> Result<()> {
        let src = parts(from)?;
        let dst = parts(to)?;
        let (old, parent) = src.split_last().ok_or_else(|| error("不能重命名根目录"))?;
        let (new, target_parent) = dst.split_last().ok_or_else(|| error("不能覆盖根目录"))?;
        if dst.len() > src.len() && dst.starts_with(&src) {
            return Err(error("不能移动目录到自身子目录"));
        }
        let mut next = self.root.clone();
        let source = dir_mut(&mut next, parent)?;
        let mut node = take(source, old).ok_or_else(|| error("源路径不存在"))?;
        if src == dst {
            return Ok(());
        }
        touch(source);
        let target = dir_mut(&mut next, target_parent)?;
        if let Some(existing) = take(target, new) {
            match (&node, existing) {
                (Node::File(_), Node::File(_)) => {}
                (Node::Directory(_), Node::Directory(d))
                    if d.files.is_empty() && d.subdirs.is_empty() => {}
                (Node::Directory(_), Node::Directory(_)) => return Err(error("目标目录非空")),
                _ => return Err(error("源与目标类型不一致")),
            }
        }
        let now = super::label::ltfs_time_now();
        match &mut node {
            Node::File(f) => {
                f.name = new.to_string();
                f.meta.change_time = now;
            }
            Node::Directory(d) => {
                d.name = new.to_string();
                d.meta.change_time = now;
            }
        }
        match node {
            Node::File(f) => target.files.push(f),
            Node::Directory(d) => target.subdirs.push(d),
        }
        touch(target);
        self.root = next;
        Ok(())
    }

    fn node_metadata(&mut self, path: &str) -> Result<(&mut NodeMeta, &mut Vec<Xattr>)> {
        let p = parts(path)?;
        if p.is_empty() {
            return Ok((&mut self.root.meta, &mut self.root.xattrs));
        }
        let (name, parents) = p.split_last().unwrap();
        let parent = dir_mut(&mut self.root, parents)?;
        if let Some(f) = parent.files.iter_mut().find(|f| f.name == *name) {
            return Ok((&mut f.meta, &mut f.xattrs));
        }
        let d = parent
            .subdirs
            .iter_mut()
            .find(|d| d.name == *name)
            .ok_or_else(|| error("路径不存在"))?;
        Ok((&mut d.meta, &mut d.xattrs))
    }

    pub(crate) fn set_catalog_version(&mut self, path: &str, version: &str) -> Result<()> {
        let (_, attrs) = self.node_metadata(path)?;
        attrs.retain(|x| x.key != super::volume::XATTR_VERSION);
        attrs.push(Xattr {
            key: super::volume::XATTR_VERSION.into(),
            value: version.into(),
            base64: false,
        });
        Ok(())
    }

    pub fn set_node_xattr(
        &mut self,
        path: &str,
        attr: Xattr,
        create_only: bool,
        replace_only: bool,
    ) -> Result<()> {
        if attr.key.is_empty() || (create_only && replace_only) {
            return Err(error("扩展属性参数非法"));
        }
        let (meta, attrs) = self.node_metadata(path)?;
        let found = attrs.iter().position(|a| a.key == attr.key);
        if found.is_some() && create_only {
            return Err(error("扩展属性已存在"));
        }
        if found.is_none() && replace_only {
            return Err(error("扩展属性不存在"));
        }
        if let Some(i) = found {
            attrs[i] = attr;
        } else {
            attrs.push(attr);
        }
        meta.change_time = super::label::ltfs_time_now();
        Ok(())
    }

    pub fn remove_node_xattr(&mut self, path: &str, key: &str) -> Result<()> {
        let (meta, attrs) = self.node_metadata(path)?;
        let i = attrs
            .iter()
            .position(|a| a.key == key)
            .ok_or_else(|| error("扩展属性不存在"))?;
        attrs.remove(i);
        meta.change_time = super::label::ltfs_time_now();
        Ok(())
    }

    pub fn set_node_readonly(&mut self, path: &str, readonly: bool) -> Result<()> {
        let (meta, _) = self.node_metadata(path)?;
        meta.readonly = readonly;
        meta.change_time = super::label::ltfs_time_now();
        Ok(())
    }
}
