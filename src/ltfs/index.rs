//! LTFS Index XML 读写。
//!
//! Index 是 LTFS 的"文件系统快照"：一棵目录树 + 每个文件的 extent 列表
//! （extent = tape 上连续块的一段区间）。Index 每次 session commit 时整份
//! 重新写出（同时写到 P1 data 尾部和 P0 开头）。
//!
//! 本文件同时提供解析（read path）与生成（write path）。写路径对应 Task #5；
//! 保持在同一模块便于两端共享数据结构。

use std::fmt::Write as _;

use bytes::Bytes;
use chrono::Utc;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::ltfs::label::LTFS_VERSION;

/// Index 的"位置"：partition + 起始块号。
#[derive(Debug, Clone, Copy)]
pub struct IndexLocation {
    pub partition: char,
    pub start_block: u64,
}

/// 单个 extent：文件在磁带上连续的一段。
#[derive(Debug, Clone)]
pub struct Extent {
    pub partition: char,
    pub start_block: u64,
    /// 块内起始字节偏移（非 0 时表示块首有其他文件数据）。
    pub byte_offset: u64,
    pub byte_count: u64,
    /// extent 对应于文件内的 offset。
    pub file_offset: u64,
}

/// 通用的时间戳/uid 字段集合（文件与目录共用）。
#[derive(Debug, Clone, Default)]
pub struct NodeMeta {
    pub readonly: bool,
    pub creation_time: String,
    pub change_time: String,
    pub modify_time: String,
    pub access_time: String,
    pub backup_time: String,
    pub file_uid: u64,
}

#[derive(Debug, Clone)]
pub struct FileNode {
    pub name: String,
    pub length: u64,
    pub meta: NodeMeta,
    pub extents: Vec<Extent>,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryNode {
    pub name: String,
    pub meta: NodeMeta,
    pub files: Vec<FileNode>,
    pub subdirs: Vec<DirectoryNode>,
}

#[derive(Debug, Clone)]
pub struct LtfsIndex {
    pub version: String,
    pub creator: String,
    pub volume_uuid: Uuid,
    pub generation: u64,
    pub update_time: String,
    /// 本 index 自身的位置（写入前可置 0，commit 时由调用方回填）。
    pub self_location: IndexLocation,
    /// 上一代 index 在 P1 中的位置（generation chain）。None 表示首代。
    pub previous_location: Option<IndexLocation>,
    pub allow_policy_update: bool,
    pub highest_file_uid: u64,
    pub root: DirectoryNode,
}

impl LtfsIndex {
    /// 新建一个空索引（首代）。
    pub fn empty(volume_uuid: Uuid, creator: String, data_partition: char) -> Self {
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        Self {
            version: LTFS_VERSION.to_string(),
            creator,
            volume_uuid,
            generation: 1,
            update_time: now.clone(),
            self_location: IndexLocation { partition: data_partition, start_block: 0 },
            previous_location: None,
            allow_policy_update: true,
            highest_file_uid: 1,
            root: DirectoryNode {
                name: String::new(),
                meta: NodeMeta {
                    readonly: false,
                    creation_time: now.clone(),
                    change_time: now.clone(),
                    modify_time: now.clone(),
                    access_time: now.clone(),
                    backup_time: now,
                    file_uid: 1,
                },
                files: Vec::new(),
                subdirs: Vec::new(),
            },
        }
    }

    /// 深度优先遍历所有文件，回调收到相对路径（以 '/' 分隔）。
    pub fn walk_files<F: FnMut(&str, &FileNode)>(&self, mut f: F) {
        walk_dir(&self.root, "", &mut f);
    }

    /// 按路径查找文件（路径用 '/' 分隔，不以 '/' 开头）。
    pub fn find_file(&self, path: &str) -> Option<&FileNode> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return None;
        }
        let (file_name, dir_parts) = parts.split_last().unwrap();
        let mut dir = &self.root;
        for part in dir_parts {
            dir = dir.subdirs.iter().find(|d| d.name == *part)?;
        }
        dir.files.iter().find(|f| f.name == *file_name)
    }

    /// 解析 XML index。
    pub fn parse(xml: &[u8]) -> Result<Self> {
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(true);
        let mut p = IndexParser::new();
        let mut bufv = Vec::new();
        loop {
            bufv.clear();
            match reader.read_event_into(&mut bufv)? {
                Event::Eof => break,
                Event::Start(e) => p.on_start(e)?,
                Event::End(e) => p.on_end(e)?,
                Event::Text(t) => p.on_text(&t.unescape()?),
                _ => {}
            }
        }
        p.finish()
    }

    /// 序列化为 XML。self_location / previous_location / generation 由调用方
    /// 在 commit 前设好。
    pub fn to_xml(&self) -> Result<Bytes> {
        let mut out = Vec::with_capacity(4096);
        let mut w = Writer::new_with_indent(&mut out, b' ', 2);
        w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

        let mut root = BytesStart::new("ltfsindex");
        root.push_attribute(("version", self.version.as_str()));
        w.write_event(Event::Start(root))?;

        write_text(&mut w, "creator", &self.creator)?;
        write_text(&mut w, "volumeuuid", &self.volume_uuid.to_string())?;
        write_u64(&mut w, "generationnumber", self.generation)?;
        write_text(&mut w, "updatetime", &self.update_time)?;
        write_location(&mut w, "location", &self.self_location)?;
        if let Some(prev) = self.previous_location {
            write_location(&mut w, "previousgenerationlocation", &prev)?;
        }
        write_text(
            &mut w,
            "allowpolicyupdate",
            if self.allow_policy_update { "true" } else { "false" },
        )?;
        write_u64(&mut w, "highestfileuid", self.highest_file_uid)?;

        write_directory(&mut w, &self.root, /*is_root=*/ true)?;

        w.write_event(Event::End(BytesEnd::new("ltfsindex")))?;
        Ok(Bytes::from(out))
    }
}

fn walk_dir<F: FnMut(&str, &FileNode)>(dir: &DirectoryNode, prefix: &str, f: &mut F) {
    for file in &dir.files {
        let p = if prefix.is_empty() {
            file.name.clone()
        } else {
            format!("{}/{}", prefix, file.name)
        };
        f(&p, file);
    }
    for sub in &dir.subdirs {
        let p = if prefix.is_empty() {
            sub.name.clone()
        } else {
            format!("{}/{}", prefix, sub.name)
        };
        walk_dir(sub, &p, f);
    }
}

fn write_text<W: std::io::Write>(w: &mut Writer<W>, tag: &str, text: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag)))?;
    w.write_event(Event::Text(BytesText::new(text)))?;
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

fn write_u64<W: std::io::Write>(w: &mut Writer<W>, tag: &str, v: u64) -> Result<()> {
    let mut s = String::new();
    let _ = write!(s, "{}", v);
    write_text(w, tag, &s)
}

fn write_location<W: std::io::Write>(w: &mut Writer<W>, tag: &str, loc: &IndexLocation) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag)))?;
    write_text(w, "partition", &loc.partition.to_string())?;
    write_u64(w, "startblock", loc.start_block)?;
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

fn write_meta<W: std::io::Write>(w: &mut Writer<W>, meta: &NodeMeta) -> Result<()> {
    write_text(w, "readonly", if meta.readonly { "true" } else { "false" })?;
    write_text(w, "creationtime", &meta.creation_time)?;
    write_text(w, "changetime", &meta.change_time)?;
    write_text(w, "modifytime", &meta.modify_time)?;
    write_text(w, "accesstime", &meta.access_time)?;
    write_text(w, "backuptime", &meta.backup_time)?;
    write_u64(w, "fileuid", meta.file_uid)?;
    Ok(())
}

fn write_file<W: std::io::Write>(w: &mut Writer<W>, f: &FileNode) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("file")))?;
    write_text(w, "name", &f.name)?;
    write_u64(w, "length", f.length)?;
    write_meta(w, &f.meta)?;
    w.write_event(Event::Start(BytesStart::new("extentinfo")))?;
    for ext in &f.extents {
        w.write_event(Event::Start(BytesStart::new("extent")))?;
        write_text(w, "partition", &ext.partition.to_string())?;
        write_u64(w, "startblock", ext.start_block)?;
        write_u64(w, "byteoffset", ext.byte_offset)?;
        write_u64(w, "bytecount", ext.byte_count)?;
        write_u64(w, "fileoffset", ext.file_offset)?;
        w.write_event(Event::End(BytesEnd::new("extent")))?;
    }
    w.write_event(Event::End(BytesEnd::new("extentinfo")))?;
    w.write_event(Event::End(BytesEnd::new("file")))?;
    Ok(())
}

fn write_directory<W: std::io::Write>(
    w: &mut Writer<W>,
    d: &DirectoryNode,
    is_root: bool,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("directory")))?;
    if !is_root {
        write_text(w, "name", &d.name)?;
    } else {
        // root 的 name 为空串，但节点必须存在
        write_text(w, "name", "")?;
    }
    write_meta(w, &d.meta)?;
    w.write_event(Event::Start(BytesStart::new("contents")))?;
    for f in &d.files {
        write_file(w, f)?;
    }
    for sub in &d.subdirs {
        write_directory(w, sub, false)?;
    }
    w.write_event(Event::End(BytesEnd::new("contents")))?;
    w.write_event(Event::End(BytesEnd::new("directory")))?;
    Ok(())
}

// ----- 解析器 -----

/// 栈式 XML 状态机：维护 path 栈 + "当前"目录/文件/extent。
struct IndexParser {
    path: Vec<String>,
    text: String,
    version: String,

    creator: String,
    volume_uuid: Option<Uuid>,
    generation: u64,
    update_time: String,
    self_location: Option<IndexLocation>,
    previous_location: Option<IndexLocation>,
    allow_policy_update: bool,
    highest_file_uid: u64,

    /// 目录栈：顶端是"当前正在填充的目录"。
    dir_stack: Vec<DirectoryNode>,
    /// 正在解析的 file（进入 <file> 时 push，离开时 pop 进父目录）。
    file_stack: Vec<FileNode>,
    /// 正在解析的 extent。
    cur_extent: Option<Extent>,
    /// 正在构造的 location（<location> / <previousgenerationlocation>）。
    cur_location: Option<IndexLocation>,
    cur_location_tag: Option<String>,
}

impl IndexParser {
    fn new() -> Self {
        Self {
            path: Vec::new(),
            text: String::new(),
            version: String::new(),
            creator: String::new(),
            volume_uuid: None,
            generation: 0,
            update_time: String::new(),
            self_location: None,
            previous_location: None,
            allow_policy_update: true,
            highest_file_uid: 0,
            dir_stack: Vec::new(),
            file_stack: Vec::new(),
            cur_extent: None,
            cur_location: None,
            cur_location_tag: None,
        }
    }

    fn on_start(&mut self, e: BytesStart<'_>) -> Result<()> {
        let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
        self.text.clear();

        match name.as_str() {
            "ltfsindex" => {
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == b"version" {
                        self.version = String::from_utf8_lossy(&a.value).to_string();
                    }
                }
            }
            "directory" => {
                self.dir_stack.push(DirectoryNode::default());
            }
            "file" => {
                self.file_stack.push(FileNode {
                    name: String::new(),
                    length: 0,
                    meta: NodeMeta::default(),
                    extents: Vec::new(),
                });
            }
            "extent" => {
                self.cur_extent = Some(Extent {
                    partition: 'b',
                    start_block: 0,
                    byte_offset: 0,
                    byte_count: 0,
                    file_offset: 0,
                });
            }
            "location" | "previousgenerationlocation" => {
                self.cur_location = Some(IndexLocation { partition: 'b', start_block: 0 });
                self.cur_location_tag = Some(name.clone());
            }
            _ => {}
        }
        self.path.push(name);
        Ok(())
    }

    fn on_text(&mut self, t: &str) {
        self.text.push_str(t);
    }

    fn on_end(&mut self, _e: BytesEnd<'_>) -> Result<()> {
        let name = self.path.pop().unwrap_or_default();
        let parent = self.path.last().cloned().unwrap_or_default();
        let text = std::mem::take(&mut self.text);
        let trimmed = text.trim();

        // location 字段
        if self.cur_location.is_some() && (parent == "location" || parent == "previousgenerationlocation") {
            if let Some(loc) = self.cur_location.as_mut() {
                match name.as_str() {
                    "partition" => loc.partition = trimmed.chars().next().unwrap_or('b'),
                    "startblock" => loc.start_block = trimmed.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        if name == "location" || name == "previousgenerationlocation" {
            if let (Some(loc), Some(tag)) = (self.cur_location.take(), self.cur_location_tag.take()) {
                if tag == "location" {
                    self.self_location = Some(loc);
                } else {
                    self.previous_location = Some(loc);
                }
            }
            return Ok(());
        }

        // extent 字段
        if let Some(ext) = self.cur_extent.as_mut() {
            if parent == "extent" {
                match name.as_str() {
                    "partition" => ext.partition = trimmed.chars().next().unwrap_or('b'),
                    "startblock" => ext.start_block = trimmed.parse().unwrap_or(0),
                    "byteoffset" => ext.byte_offset = trimmed.parse().unwrap_or(0),
                    "bytecount" => ext.byte_count = trimmed.parse().unwrap_or(0),
                    "fileoffset" => ext.file_offset = trimmed.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        if name == "extent" {
            if let (Some(ext), Some(file)) = (self.cur_extent.take(), self.file_stack.last_mut()) {
                file.extents.push(ext);
            }
            return Ok(());
        }

        // file 的标量字段
        if parent == "file" {
            if let Some(file) = self.file_stack.last_mut() {
                match name.as_str() {
                    "name" => file.name = trimmed.to_string(),
                    "length" => file.length = trimmed.parse().unwrap_or(0),
                    _ => apply_meta(&mut file.meta, &name, trimmed),
                }
            }
        }

        // directory 的标量字段（<name>/<creationtime>/...）
        if parent == "directory" {
            if let Some(dir) = self.dir_stack.last_mut() {
                match name.as_str() {
                    "name" => dir.name = trimmed.to_string(),
                    _ => apply_meta(&mut dir.meta, &name, trimmed),
                }
            }
        }

        // 顶层 ltfsindex 字段
        if parent == "ltfsindex" {
            match name.as_str() {
                "creator" => self.creator = trimmed.to_string(),
                "volumeuuid" => self.volume_uuid = Uuid::parse_str(trimmed).ok(),
                "generationnumber" => self.generation = trimmed.parse().unwrap_or(0),
                "updatetime" => self.update_time = trimmed.to_string(),
                "allowpolicyupdate" => self.allow_policy_update = matches!(trimmed, "true" | "1" | "yes"),
                "highestfileuid" => self.highest_file_uid = trimmed.parse().unwrap_or(0),
                _ => {}
            }
        }

        // 离开 <file>：把它加到当前目录的 files 里
        if name == "file" {
            if let (Some(file), Some(dir)) = (self.file_stack.pop(), self.dir_stack.last_mut()) {
                dir.files.push(file);
            }
            return Ok(());
        }

        // 离开 <directory>：pop 出来，附加到父目录 subdirs（若还有父目录）
        if name == "directory" {
            if let Some(done) = self.dir_stack.pop() {
                if let Some(parent_dir) = self.dir_stack.last_mut() {
                    parent_dir.subdirs.push(done);
                } else {
                    // root：保留到 finish() 里取
                    self.dir_stack.push(done);
                }
            }
            return Ok(());
        }

        Ok(())
    }

    fn finish(mut self) -> Result<LtfsIndex> {
        let volume_uuid = self
            .volume_uuid
            .ok_or_else(|| TapeError::Ltfs("index 缺少 volumeuuid".into()))?;
        let self_location = self
            .self_location
            .unwrap_or(IndexLocation { partition: 'b', start_block: 0 });
        let root = self
            .dir_stack
            .pop()
            .ok_or_else(|| TapeError::Ltfs("index 缺少 root directory".into()))?;
        Ok(LtfsIndex {
            version: if self.version.is_empty() { LTFS_VERSION.into() } else { self.version },
            creator: self.creator,
            volume_uuid,
            generation: self.generation,
            update_time: self.update_time,
            self_location,
            previous_location: self.previous_location,
            allow_policy_update: self.allow_policy_update,
            highest_file_uid: self.highest_file_uid,
            root,
        })
    }
}

fn apply_meta(m: &mut NodeMeta, tag: &str, text: &str) {
    match tag {
        "readonly" => m.readonly = matches!(text, "true" | "1" | "yes"),
        "creationtime" => m.creation_time = text.to_string(),
        "changetime" => m.change_time = text.to_string(),
        "modifytime" => m.modify_time = text.to_string(),
        "accesstime" => m.access_time = text.to_string(),
        "backuptime" => m.backup_time = text.to_string(),
        "fileuid" => m.file_uid = text.parse().unwrap_or(0),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roundtrip() {
        let idx = LtfsIndex::empty(Uuid::nil(), "tape-rs".into(), 'b');
        let xml = idx.to_xml().unwrap();
        let back = LtfsIndex::parse(&xml).unwrap();
        assert_eq!(back.volume_uuid, Uuid::nil());
        assert_eq!(back.generation, 1);
        assert!(back.root.files.is_empty());
        assert!(back.root.subdirs.is_empty());
    }

    #[test]
    fn one_file_roundtrip() {
        let mut idx = LtfsIndex::empty(Uuid::nil(), "tape-rs".into(), 'b');
        idx.root.files.push(FileNode {
            name: "hello.txt".into(),
            length: 5,
            meta: NodeMeta { file_uid: 2, ..Default::default() },
            extents: vec![Extent {
                partition: 'b',
                start_block: 10,
                byte_offset: 0,
                byte_count: 5,
                file_offset: 0,
            }],
        });
        let xml = idx.to_xml().unwrap();
        let back = LtfsIndex::parse(&xml).unwrap();
        let f = back.find_file("hello.txt").unwrap();
        assert_eq!(f.length, 5);
        assert_eq!(f.extents.len(), 1);
        assert_eq!(f.extents[0].start_block, 10);
    }

    #[test]
    fn walk_files_collects_paths() {
        let mut idx = LtfsIndex::empty(Uuid::nil(), "tape-rs".into(), 'b');
        let mut sub = DirectoryNode::default();
        sub.name = "sub".into();
        sub.files.push(FileNode {
            name: "deep.bin".into(),
            length: 1,
            meta: NodeMeta::default(),
            extents: vec![],
        });
        idx.root.subdirs.push(sub);
        idx.root.files.push(FileNode {
            name: "top.txt".into(),
            length: 1,
            meta: NodeMeta::default(),
            extents: vec![],
        });

        let mut paths = Vec::new();
        idx.walk_files(|p, _| paths.push(p.to_string()));
        paths.sort();
        assert_eq!(paths, vec!["sub/deep.bin".to_string(), "top.txt".to_string()]);
    }
}
