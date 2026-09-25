//! LTFS Index XML 读写。
//!
//! Index 是 LTFS 的"文件系统快照"：一棵目录树 + 每个文件的 extent 列表
//! （extent = tape 上连续块的一段区间）。Index 每次 session commit 时整份
//! 重新写出（同时写到 P1 data 尾部和 P0 开头）。
//!
//! 本文件同时提供解析（read path）与生成（write path）。写路径对应 Task #5；
//! 保持在同一模块便于两端共享数据结构。

use std::fmt::Write as _;
use std::collections::BTreeSet;

use bytes::Bytes;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::ltfs::label::LTFS_VERSION;

/// Index 的"位置"：partition + 起始块号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLocation {
    pub partition: char,
    pub start_block: u64,
}

/// 单个 extent：文件在磁带上连续的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeMeta {
    pub readonly: bool,
    pub creation_time: String,
    pub change_time: String,
    pub modify_time: String,
    pub access_time: String,
    pub backup_time: String,
    pub file_uid: u64,
}

/// 扩展属性（LTFS 2.5.1 §9.2.13）。`value` 原样保存索引里的文本；
/// `base64 == true` 表示索引里带 `type="base64"`，`value` 是编码后的文本。
/// 原样保存是为了回写时逐字节不变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    pub key: String,
    pub value: String,
    pub base64: bool,
}

/// 文件内容哈希（LTFS 2.5.1 附录 F.3，十六进制小写）。一个文件可以同时带多种。
/// IBM LTFS 写的是 md5sum；本实现两个都写，校验时优先 sha256sum。
pub const XATTR_MD5: &str = "ltfs.hash.md5sum";
pub const XATTR_SHA256: &str = "ltfs.hash.sha256sum";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNode {
    /// 增量中明确出现的字段；None 表示完整对象。
    pub delta_fields: Option<BTreeSet<String>>,
    pub open_for_write: Option<bool>,
    pub name: String,
    pub length: u64,
    pub meta: NodeMeta,
    pub extents: Vec<Extent>,
    pub xattrs: Vec<Xattr>,
    /// 符号链接目标。有值时文件没有 extent（§9.2.9）。
    pub symlink: Option<String>,
}

impl FileNode {
    pub fn xattr(&self, key: &str) -> Option<&str> {
        self.xattrs.iter().find(|x| x.key == key).map(|x| x.value.as_str())
    }

    /// 设置文本型扩展属性，同名覆盖。
    pub fn set_xattr(&mut self, key: &str, value: &str) {
        self.xattrs.retain(|x| x.key != key);
        self.xattrs.push(Xattr { key: key.to_string(), value: value.to_string(), base64: false });
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryNode {
    /// 增量导航目录可能只有 name 与 contents。
    pub delta_fields: Option<BTreeSet<String>>,
    pub name: String,
    pub meta: NodeMeta,
    pub files: Vec<FileNode>,
    pub subdirs: Vec<DirectoryNode>,
    pub xattrs: Vec<Xattr>,
}

#[derive(Debug, Clone)]
pub struct LtfsIndex {
    pub incremental: bool,
    /// 已重放的完整内存视图不是原始增量 XML，禁止误序列化成缺少删除项的增量。
    pub(crate) materialized: bool,
    pub previous_incremental_location: Option<IndexLocation>,
    pub comment: Option<String>,
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
    /// `<volumelockstate>`（IBM LTFS 会写）。None 时不写出。
    pub volume_lock_state: Option<String>,
    /// 解析时遇到、本实现不会回写的元素名（去重）。非空表示重写索引会丢信息，
    /// 挂载层据此拒绝写入。
    pub unknown_elements: Vec<String>,
    pub root: DirectoryNode,
}

impl LtfsIndex {
    /// 新建一个空索引（首代）。
    pub fn empty(volume_uuid: Uuid, creator: String, data_partition: char) -> Self {
        let now = crate::ltfs::label::ltfs_time_now();
        Self {
            incremental: false,
            materialized: false,
            previous_incremental_location: None,
            comment: None,
            version: LTFS_VERSION.to_string(),
            creator,
            volume_uuid,
            generation: 1,
            update_time: now.clone(),
            self_location: IndexLocation { partition: data_partition, start_block: 0 },
            previous_location: None,
            allow_policy_update: true,
            highest_file_uid: 1,
            volume_lock_state: None,
            unknown_elements: Vec::new(),
            root: DirectoryNode {
                delta_fields: None,
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
                xattrs: Vec::new(),
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
        reader.config_mut().trim_text(false);
        let mut p = IndexParser::new();
        let mut bufv = Vec::new();
        loop {
            bufv.clear();
            match reader.read_event_into(&mut bufv)? {
                Event::Eof => break,
                Event::Start(e) => p.on_start(e)?,
                Event::Empty(e) => {
                    let end = e.to_end().into_owned();
                    p.on_start(e)?;
                    p.on_end(end)?;
                }
                Event::End(e) => p.on_end(e)?,
                Event::Text(t) => p.on_text(&t.unescape()?),
                Event::CData(t) => p.on_text(std::str::from_utf8(t.as_ref()).map_err(|e| TapeError::Ltfs(e.to_string()))?),
                _ => {}
            }
        }
        let index = p.finish()?;
        index.validate_incremental()?;
        Ok(index)
    }

    /// 序列化为 XML。self_location / previous_location / generation 由调用方
    /// 在 commit 前设好。
    pub fn to_xml(&self) -> Result<Bytes> {
        if self.incremental && self.materialized { return Err(TapeError::Ltfs("已重放视图不能作为原始增量序列化，请生成差分或完整检查点".into())); }
        let mut out = Vec::with_capacity(4096);
        let mut w = Writer::new_with_indent(&mut out, b' ', 2);
        w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

        let root_name = if self.incremental { "ltfsincrementalindex" } else { "ltfsindex" };
        let mut root = BytesStart::new(root_name);
        root.push_attribute(("version", self.version.as_str()));
        w.write_event(Event::Start(root))?;

        write_text(&mut w, "creator", &self.creator)?;
        if let Some(comment) = &self.comment { write_text(&mut w, "comment", comment)?; }
        write_text(&mut w, "volumeuuid", &self.volume_uuid.to_string())?;
        write_u64(&mut w, "generationnumber", self.generation)?;
        write_text(&mut w, "updatetime", &self.update_time)?;
        write_location(&mut w, "location", &self.self_location)?;
        if let Some(prev) = self.previous_location {
            write_location(&mut w, "previousgenerationlocation", &prev)?;
        }
        if let Some(prev) = self.previous_incremental_location {
            write_location(&mut w, "previousincrementallocation", &prev)?;
        }
        if !self.incremental {
            write_text(&mut w, "allowpolicyupdate", if self.allow_policy_update { "true" } else { "false" })?;
        }
        if let Some(state) = &self.volume_lock_state {
            write_text(&mut w, "volumelockstate", state)?;
        }
        write_u64(&mut w, "highestfileuid", self.highest_file_uid)?;

        write_directory(&mut w, &self.root, /*is_root=*/ true)?;

        w.write_event(Event::End(BytesEnd::new(root_name)))?;
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
    let mut start = BytesStart::new(tag);
    let encoded;
    let text = if matches!(tag, "name" | "key" | "symlink") && text.chars().any(|c| c.is_ascii_control()) {
        start.push_attribute(("percentencoded", "true"));
        encoded = text.chars().map(|c| if c.is_ascii_control() || c == '%' { format!("%{:02X}", c as u32) } else { c.to_string() }).collect::<String>();
        encoded.as_str()
    } else { text };
    w.write_event(Event::Start(start))?;
    w.write_event(Event::Text(BytesText::new(text)))?;
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

fn decode_name(text: &str) -> Result<String> {
    let mut bytes = Vec::new(); let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'%' {
            let pair = text.as_bytes().get(i + 1..i + 3).ok_or_else(|| TapeError::Ltfs("名称百分号编码截断".into()))?;
            let hex = std::str::from_utf8(pair).map_err(|e| TapeError::Ltfs(e.to_string()))?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|e| TapeError::Ltfs(e.to_string()))?); i += 3;
        } else { bytes.push(text.as_bytes()[i]); i += 1; }
    }
    String::from_utf8(bytes).map_err(|e| TapeError::Ltfs(e.to_string()))
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

fn present(fields: &Option<BTreeSet<String>>, key: &str) -> bool {
    fields.as_ref().is_none_or(|f| f.contains(key))
}

fn write_meta<W: std::io::Write>(w: &mut Writer<W>, meta: &NodeMeta, fields: &Option<BTreeSet<String>>) -> Result<()> {
    if present(fields, "readonly") { write_text(w, "readonly", if meta.readonly { "true" } else { "false" })?; }
    if present(fields, "creationtime") { write_text(w, "creationtime", &meta.creation_time)?; }
    if present(fields, "changetime") { write_text(w, "changetime", &meta.change_time)?; }
    if present(fields, "modifytime") { write_text(w, "modifytime", &meta.modify_time)?; }
    if present(fields, "accesstime") { write_text(w, "accesstime", &meta.access_time)?; }
    if present(fields, "backuptime") { write_text(w, "backuptime", &meta.backup_time)?; }
    if present(fields, "fileuid") { write_u64(w, "fileuid", meta.file_uid)?; }
    Ok(())
}

fn write_xattrs<W: std::io::Write>(w: &mut Writer<W>, xattrs: &[Xattr]) -> Result<()> {
    if xattrs.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("extendedattributes")))?;
    for x in xattrs {
        w.write_event(Event::Start(BytesStart::new("xattr")))?;
        write_text(w, "key", &x.key)?;
        let mut value = BytesStart::new("value");
        if x.base64 {
            value.push_attribute(("type", "base64"));
        }
        // IBM treats an explicit empty base64 body as a decode error; the
        // empty element represents a zero-length value without decoding it.
        if x.value.is_empty() {
            w.write_event(Event::Empty(value))?;
        } else {
            w.write_event(Event::Start(value))?;
            w.write_event(Event::Text(BytesText::new(&x.value)))?;
            w.write_event(Event::End(BytesEnd::new("value")))?;
        }
        w.write_event(Event::End(BytesEnd::new("xattr")))?;
    }
    w.write_event(Event::End(BytesEnd::new("extendedattributes")))?;
    Ok(())
}

fn write_file<W: std::io::Write>(w: &mut Writer<W>, f: &FileNode) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("file")))?;
    write_text(w, "name", &f.name)?;
    if f.delta_fields.as_ref().is_some_and(|v| v.contains("deleted")) {
        w.write_event(Event::Empty(BytesStart::new("deleted")))?;
        w.write_event(Event::End(BytesEnd::new("file")))?;
        return Ok(());
    }
    if present(&f.delta_fields, "length") { write_u64(w, "length", f.length)?; }
    write_meta(w, &f.meta, &f.delta_fields)?;
    if present(&f.delta_fields, "extendedattributes") {
        write_xattrs(w, &f.xattrs)?;
        if f.xattrs.is_empty() && f.delta_fields.is_some() { w.write_event(Event::Empty(BytesStart::new("extendedattributes")))?; }
    }
    if let Some(value) = f.open_for_write && present(&f.delta_fields, "openforwrite") {
        write_text(w, "openforwrite", if value { "true" } else { "false" })?;
    }
    if let Some(target) = &f.symlink && present(&f.delta_fields, "symlink") {
        write_text(w, "symlink", target)?;
        w.write_event(Event::End(BytesEnd::new("file")))?;
        return Ok(());
    }
    if present(&f.delta_fields, "extentinfo") {
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
    }
    w.write_event(Event::End(BytesEnd::new("file")))?;
    Ok(())
}

fn write_directory<W: std::io::Write>(
    w: &mut Writer<W>,
    d: &DirectoryNode,
    is_root: bool,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("directory")))?;
    let _ = is_root;
    write_text(w, "name", &d.name)?;
    if d.delta_fields.as_ref().is_some_and(|v| v.contains("deleted")) {
        w.write_event(Event::Empty(BytesStart::new("deleted")))?;
        w.write_event(Event::End(BytesEnd::new("directory")))?;
        return Ok(());
    }
    write_meta(w, &d.meta, &d.delta_fields)?;
    if present(&d.delta_fields, "extendedattributes") {
        write_xattrs(w, &d.xattrs)?;
        if d.xattrs.is_empty() && d.delta_fields.is_some() { w.write_event(Event::Empty(BytesStart::new("extendedattributes")))?; }
    }
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
    incremental: bool,
    previous_incremental_location: Option<IndexLocation>,
    header_fields: BTreeSet<String>,
    percent_encoded: BTreeSet<usize>,
    comment: Option<String>,
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
    /// 正在解析的 xattr。
    cur_xattr: Option<Xattr>,
    volume_lock_state: Option<String>,
    unknown_elements: Vec<String>,
}

/// 本实现能解析并原样回写的元素。其余的记入 `unknown_elements`。
const KNOWN_ELEMENTS: &[&str] = &[
    "ltfsincrementalindex", "previousincrementallocation", "deleted", "comment", "openforwrite",
    "ltfsindex", "creator", "volumeuuid", "generationnumber", "updatetime", "location",
    "previousgenerationlocation", "allowpolicyupdate", "volumelockstate", "highestfileuid",
    "directory", "contents", "file", "name", "length", "readonly", "creationtime", "changetime",
    "modifytime", "accesstime", "backuptime", "fileuid", "extendedattributes", "xattr", "key",
    "value", "symlink", "extentinfo", "extent", "partition", "startblock", "byteoffset",
    "bytecount", "fileoffset",
];

impl IndexParser {
    fn new() -> Self {
        Self {
            incremental: false,
            previous_incremental_location: None,
            header_fields: BTreeSet::new(),
            percent_encoded: BTreeSet::new(),
            comment: None,
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
            cur_xattr: None,
            volume_lock_state: None,
            unknown_elements: Vec::new(),
        }
    }

    fn on_start(&mut self, e: BytesStart<'_>) -> Result<()> {
        let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
        self.text.clear();
        if self.path.len() >= 512 { return Err(TapeError::Ltfs("索引 XML 嵌套深度超过预算".into())); }
        if matches!(name.as_str(), "name" | "key" | "symlink") && e.attributes().flatten().any(|a| a.key.as_ref() == b"percentencoded" && matches!(a.value.as_ref(), b"true" | b"1")) {
            self.percent_encoded.insert(self.path.len());
        }

        if !KNOWN_ELEMENTS.contains(&name.as_str()) && !self.unknown_elements.contains(&name) {
            self.unknown_elements.push(name.clone());
        }

        if name == "deleted" && !self.incremental { return Err(TapeError::Ltfs("完整索引不能包含 deleted".into())); }
        if self.incremental && matches!(name.as_str(), "allowpolicyupdate" | "dataplacementpolicy") {
            return Err(TapeError::Ltfs("增量索引不允许放置策略".into()));
        }
        match name.as_str() {
            "ltfsindex" | "ltfsincrementalindex" => {
                self.incremental = name == "ltfsincrementalindex";
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == b"version" {
                        self.version = String::from_utf8_lossy(&a.value).to_string();
                    }
                }
            }
            "directory" => {
                self.dir_stack.push(DirectoryNode { delta_fields: self.incremental.then(BTreeSet::new), ..Default::default() });
            }
            "file" => {
                self.file_stack.push(FileNode { delta_fields: self.incremental.then(BTreeSet::new), ..Default::default() });
            }
            "xattr" => {
                self.cur_xattr = Some(Xattr { key: String::new(), value: String::new(), base64: false });
            }
            "value" => {
                if let Some(x) = self.cur_xattr.as_mut() {
                    x.base64 = e
                        .attributes()
                        .flatten()
                        .any(|a| a.key.as_ref() == b"type" && a.value.as_ref() == b"base64");
                }
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
            "location" | "previousgenerationlocation" | "previousincrementallocation" => {
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
        let mut text = std::mem::take(&mut self.text);
        if self.percent_encoded.remove(&self.path.len()) { text = decode_name(&text)?; }
        let trimmed = text.trim();

        if self.incremental {
            if parent == "ltfsincrementalindex" && !self.header_fields.insert(name.clone()) { return Err(TapeError::Ltfs(format!("增量头字段重复: {name}"))); }
            if matches!(name.as_str(), "readonly" | "openforwrite") && !matches!(trimmed, "true" | "false" | "1" | "0") { return Err(TapeError::Ltfs("增量 readonly 非法".into())); }
            if name == "partition" && (trimmed.len() != 1 || !trimmed.as_bytes()[0].is_ascii_alphabetic()) { return Err(TapeError::Ltfs("增量 partition 非法".into())); }
            if matches!(name.as_str(), "length" | "fileuid" | "generationnumber" | "highestfileuid" | "startblock" | "byteoffset" | "bytecount" | "fileoffset") && trimmed.parse::<u64>().is_err() {
                return Err(TapeError::Ltfs(format!("增量数值非法: {name}")));
            }
            let fields = if parent == "file" { self.file_stack.last_mut().and_then(|f| f.delta_fields.as_mut()) }
                else if parent == "directory" { self.dir_stack.last_mut().and_then(|d| d.delta_fields.as_mut()) }
                else { None };
            if let Some(fields) = fields && !fields.insert(name.clone()) { return Err(TapeError::Ltfs(format!("增量字段重复: {name}"))); }
            if name == "deleted" && !trimmed.is_empty() { return Err(TapeError::Ltfs("deleted 必须为空元素".into())); }
        }
        // location 字段
        if self.cur_location.is_some() && (parent == "location" || parent == "previousgenerationlocation" || parent == "previousincrementallocation") {
            if let Some(loc) = self.cur_location.as_mut() {
                match name.as_str() {
                    "partition" => loc.partition = trimmed.chars().next().unwrap_or('b'),
                    "startblock" => loc.start_block = trimmed.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        if name == "location" || name == "previousgenerationlocation" || name == "previousincrementallocation" {
            if let (Some(loc), Some(tag)) = (self.cur_location.take(), self.cur_location_tag.take()) {
                if tag == "location" {
                    self.self_location = Some(loc);
                } else if tag == "previousincrementallocation" {
                    self.previous_incremental_location = Some(loc);
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

        // xattr 字段：属于最近的 file，没有 file 在解析时属于当前目录
        if parent == "xattr" {
            if let Some(x) = self.cur_xattr.as_mut() {
                match name.as_str() {
                    "key" => x.key = text.clone(),
                    "value" => x.value = text.clone(),
                    _ => {}
                }
            }
            return Ok(());
        }
        if name == "xattr" {
            if let Some(x) = self.cur_xattr.take() {
                if let Some(file) = self.file_stack.last_mut() {
                    file.xattrs.push(x);
                } else if let Some(dir) = self.dir_stack.last_mut() {
                    dir.xattrs.push(x);
                }
            }
            return Ok(());
        }

        // file 的标量字段
        if parent == "file" {
            if let Some(file) = self.file_stack.last_mut() {
                match name.as_str() {
                    "name" => file.name = text.clone(),
                    "length" => file.length = trimmed.parse().unwrap_or(0),
                    "symlink" => file.symlink = Some(text.clone()),
                    "openforwrite" => file.open_for_write = Some(matches!(trimmed, "true" | "1")),
                    _ => apply_meta(&mut file.meta, &name, trimmed),
                }
            }
        }

        // directory 的标量字段（<name>/<creationtime>/...）
        if parent == "directory" {
            if let Some(dir) = self.dir_stack.last_mut() {
                match name.as_str() {
                    "name" => dir.name = text.clone(),
                    _ => apply_meta(&mut dir.meta, &name, trimmed),
                }
            }
        }

        // 顶层 ltfsindex 字段
        if parent == "ltfsindex" || parent == "ltfsincrementalindex" {
            match name.as_str() {
                "creator" => self.creator = trimmed.to_string(),
                "comment" => self.comment = Some(text.clone()),
                "volumeuuid" => self.volume_uuid = Uuid::parse_str(trimmed).ok(),
                "generationnumber" => self.generation = trimmed.parse().unwrap_or(0),
                "updatetime" => self.update_time = trimmed.to_string(),
                "allowpolicyupdate" => self.allow_policy_update = matches!(trimmed, "true" | "1" | "yes"),
                "highestfileuid" => self.highest_file_uid = trimmed.parse().unwrap_or(0),
                "volumelockstate" => self.volume_lock_state = Some(trimmed.to_string()),
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
        if self.incremental && (!self.path.is_empty() || self.self_location.is_none()) {
            return Err(TapeError::Ltfs("增量 XML 截断或缺少 location".into()));
        }
        if self.incremental {
            for key in ["creator", "volumeuuid", "generationnumber", "updatetime", "location", "previousgenerationlocation", "highestfileuid", "directory"] {
                if !self.header_fields.contains(key) { return Err(TapeError::Ltfs(format!("增量缺少 {key}"))); }
            }
        }
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
            incremental: self.incremental,
            materialized: false,
            previous_incremental_location: self.previous_incremental_location,
            comment: self.comment,
            version: if self.version.is_empty() { LTFS_VERSION.into() } else { self.version },
            creator: self.creator,
            volume_uuid,
            generation: self.generation,
            update_time: self.update_time,
            self_location,
            previous_location: self.previous_location,
            allow_policy_update: self.allow_policy_update,
            highest_file_uid: self.highest_file_uid,
            volume_lock_state: self.volume_lock_state,
            unknown_elements: self.unknown_elements,
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
            ..Default::default()
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
            ..Default::default()
        });
        idx.root.subdirs.push(sub);
        idx.root.files.push(FileNode {
            name: "top.txt".into(),
            length: 1,
            meta: NodeMeta::default(),
            extents: vec![],
            ..Default::default()
        });

        let mut paths = Vec::new();
        idx.walk_files(|p, _| paths.push(p.to_string()));
        paths.sort();
        assert_eq!(paths, vec!["sub/deep.bin".to_string(), "top.txt".to_string()]);
    }

    /// IBM LTFS 2.4.8.3 在 EE 下写出的索引形状（取自实验室克隆带，内容已缩短）。
    const IBM_STYLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ltfsindex version="2.4.0">
<creator>IBM LTFS 2.4.8.3 (10521) - Linux - ltfs</creator>
<volumeuuid>c0cb5ba8-0000-0000-0000-000000000000</volumeuuid>
<generationnumber>3</generationnumber>
<updatetime>2026-09-15T00:00:00.000000000Z</updatetime>
<location><partition>b</partition><startblock>35</startblock></location>
<previousgenerationlocation><partition>b</partition><startblock>24</startblock></previousgenerationlocation>
<allowpolicyupdate>true</allowpolicyupdate>
<volumelockstate>unlocked</volumelockstate>
<highestfileuid>8</highestfileuid>
<directory><name></name><fileuid>1</fileuid>
<extendedattributes><xattr><key>user.note</key><value type="base64">AAEC</value></xattr></extendedattributes>
<contents>
<directory><name>.LTFSEE_DATA</name><fileuid>2</fileuid><contents>
<file><name>obj-1</name><length>5</length><fileuid>3</fileuid>
<extendedattributes>
<xattr><key>ltfs.hash.md5sum</key><value>94f70734e5e5561dd811a3453404ef44</value></xattr>
<xattr><key>ibm.ltfsee.gpfs.path</key><value>/gpfs/a &amp; b.bin</value></xattr>
<xattr><key>user.empty</key><value/></xattr>
</extendedattributes>
<extentinfo><extent><partition>b</partition><startblock>5</startblock><byteoffset>0</byteoffset><bytecount>5</bytecount><fileoffset>0</fileoffset></extent></extentinfo>
</file>
</contents></directory>
<directory><name>gpfs</name><fileuid>4</fileuid><contents>
<file><name>a.bin</name><length>0</length><fileuid>5</fileuid><symlink>../.LTFSEE_DATA/obj-1</symlink></file>
</contents></directory>
</contents></directory>
</ltfsindex>"#;

    #[test]
    fn empty_base64_xattr_uses_ibm_compatible_empty_element() {
        let mut idx = LtfsIndex::parse(IBM_STYLE.as_bytes()).unwrap();
        idx.root.xattrs = vec![Xattr {
            key: "empty".into(),
            value: String::new(),
            base64: true,
        }];
        let xml = idx.to_xml().unwrap();
        assert!(String::from_utf8_lossy(&xml).contains("<value type=\"base64\"/>"));
        let parsed = LtfsIndex::parse(&xml).unwrap();
        assert_eq!(parsed.root.xattrs, idx.root.xattrs);
    }

    #[test]
    fn ibm_style_index_roundtrips_xattrs_symlink_and_lockstate() {
        let first = LtfsIndex::parse(IBM_STYLE.as_bytes()).unwrap();
        // 再走一遍写出和解析，两次的结论必须相同
        let second = LtfsIndex::parse(&first.to_xml().unwrap()).unwrap();
        for idx in [&first, &second] {
            assert!(idx.unknown_elements.is_empty(), "{:?}", idx.unknown_elements);
            assert_eq!(idx.volume_lock_state.as_deref(), Some("unlocked"));
            let f = idx.find_file(".LTFSEE_DATA/obj-1").unwrap();
            assert_eq!(f.xattr(XATTR_MD5), Some("94f70734e5e5561dd811a3453404ef44"));
            assert_eq!(f.xattr("ibm.ltfsee.gpfs.path"), Some("/gpfs/a & b.bin"));
            assert_eq!(f.xattr("user.empty"), Some(""));
            assert_eq!(f.xattrs.len(), 3);
            assert_eq!(f.extents.len(), 1);
            let link = idx.find_file("gpfs/a.bin").unwrap();
            assert_eq!(link.symlink.as_deref(), Some("../.LTFSEE_DATA/obj-1"));
            assert!(link.extents.is_empty());
            assert_eq!(
                idx.root.xattrs,
                vec![Xattr { key: "user.note".into(), value: "AAEC".into(), base64: true }]
            );
        }
    }

    #[test]
    fn unknown_elements_are_reported_once() {
        let xml = IBM_STYLE.replace(
            "<highestfileuid>8</highestfileuid>",
            "<highestfileuid>8</highestfileuid><dataplacementpolicy><x/><x/></dataplacementpolicy>",
        );
        let idx = LtfsIndex::parse(xml.as_bytes()).unwrap();
        assert_eq!(idx.unknown_elements, vec!["dataplacementpolicy".to_string(), "x".to_string()]);
    }
}
