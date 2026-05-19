//! VOL1 (ECMA-13/ANSI) + LTFS Label XML 的解析和生成。
//!
//! 每个 partition 起始布局：
//!   block 0 : VOL1 (80 B)         | filemark
//!   block 1 : LTFS Label XML       | filemark
//!   block 2+: P0=Index，P1=数据+index
//!
//! 两个 partition 的 label 对除 `<location>` 的 `a`/`b` 之外完全一致。

use std::fmt::Write as _;

use bytes::Bytes;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use uuid::Uuid;

use crate::error::{Result, TapeError};

/// LTFS 协议版本。写入端固定使用该字符串。
pub const LTFS_VERSION: &str = "2.4.0";
/// Implementation Identifier（VOL1 byte 24..37，13 字节，"LTFS" + 9 空格）。
pub const VOL1_IMPL_ID: &[u8; 13] = b"LTFS         ";
/// Label Standard Version: '4' 表示 LTFS/SCSI 磁带（byte 79）。
pub const VOL1_LABEL_VERSION: u8 = b'4';
/// LTFS partition 代号：a = Index，b = Data。
pub const PART_INDEX: char = 'a';
pub const PART_DATA: char = 'b';

/// VOL1 label 解析后的关键字段。
#[derive(Debug, Clone)]
pub struct Vol1Label {
    /// 6 字符 volume identifier（一般是 barcode 去掉 checksum 后左对齐）。
    pub volume_id: String,
    /// Owner identifier（14 字节，去尾 space）。
    pub owner: String,
}

impl Vol1Label {
    /// 从 80 字节 VOL1 label 解析。
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 80 {
            return Err(TapeError::Ltfs(format!("VOL1 长度 {} < 80", buf.len())));
        }
        if &buf[0..4] != b"VOL1" {
            return Err(TapeError::Ltfs(format!("VOL1 魔数错误: {:02x?}", &buf[0..4])));
        }
        if buf[79] != VOL1_LABEL_VERSION {
            return Err(TapeError::Ltfs(format!(
                "VOL1 Label Standard Version = {:#04x}, 期望 '4'",
                buf[79]
            )));
        }
        let volume_id = ascii_trimmed(&buf[4..10])?;
        let owner = ascii_trimmed(&buf[37..51])?;
        Ok(Self { volume_id, owner })
    }

    /// 生成 80 字节 VOL1 label。`volume_id` 会被截断/空格补齐到 6 字符。
    pub fn encode(volume_id: &str, owner: &str) -> [u8; 80] {
        let mut buf = [b' '; 80];
        buf[0..4].copy_from_slice(b"VOL1");
        pad_ascii(&mut buf[4..10], volume_id);
        // byte 10: accessibility = ' '
        // byte 11..24: reserved (spaces already)
        buf[24..37].copy_from_slice(VOL1_IMPL_ID);
        pad_ascii(&mut buf[37..51], owner);
        // byte 51..79: reserved (spaces)
        buf[79] = VOL1_LABEL_VERSION;
        buf
    }
}

/// LTFS XML label 关键字段（完整 schema 见 LTFS spec 9.3）。
#[derive(Debug, Clone)]
pub struct LtfsLabel {
    pub version: String,
    pub creator: String,
    pub format_time: String,
    pub volume_uuid: Uuid,
    /// 本 label 所在 partition: 'a' (Index) 或 'b' (Data)。
    pub location: char,
    /// partition 到角色的映射，通常 index='a' data='b'。
    pub index_partition: char,
    pub data_partition: char,
    /// 块大小（字节），写入默认 512 KiB。
    pub blocksize: u32,
    pub compression: bool,
}

impl LtfsLabel {
    /// 新建一个面向指定 partition 的 label。
    pub fn new(volume_uuid: Uuid, location: char, blocksize: u32, creator: String) -> Self {
        Self {
            version: LTFS_VERSION.to_string(),
            creator,
            format_time: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            volume_uuid,
            location,
            index_partition: PART_INDEX,
            data_partition: PART_DATA,
            blocksize,
            compression: false,
        }
    }

    /// 解析 LTFS XML label。接受任意合法的 LTFS 2.x label（未知字段忽略）。
    pub fn parse(xml: &[u8]) -> Result<Self> {
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(true);

        let mut version = String::new();
        let mut creator = String::new();
        let mut format_time = String::new();
        let mut volume_uuid: Option<Uuid> = None;
        let mut location: Option<char> = None;
        let mut index_partition = PART_INDEX;
        let mut data_partition = PART_DATA;
        let mut blocksize: u32 = 0;
        let mut compression = false;

        // 路径栈：用于区分 <partitions><index>a</index></partitions> 和顶层 <index>。
        let mut path: Vec<String> = Vec::new();
        let mut cur_text = String::new();

        let mut bufv = Vec::new();
        loop {
            bufv.clear();
            match reader.read_event_into(&mut bufv)? {
                Event::Eof => break,
                Event::Start(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "ltfslabel" {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"version" {
                                version = String::from_utf8_lossy(&a.value).to_string();
                            }
                        }
                    }
                    path.push(name);
                    cur_text.clear();
                }
                Event::Text(t) => {
                    let s = t.unescape()?;
                    cur_text.push_str(&s);
                }
                Event::End(_) => {
                    let name = path.pop().unwrap_or_default();
                    let parent = path.last().map(String::as_str).unwrap_or("");
                    match (parent, name.as_str()) {
                        ("ltfslabel", "creator") => creator = cur_text.clone(),
                        ("ltfslabel", "formattime") => format_time = cur_text.clone(),
                        ("ltfslabel", "volumeuuid") => {
                            volume_uuid = Uuid::parse_str(cur_text.trim()).ok();
                        }
                        ("ltfslabel", "blocksize") => {
                            blocksize = cur_text.trim().parse().unwrap_or(0);
                        }
                        ("ltfslabel", "compression") => {
                            compression = parse_bool(cur_text.trim());
                        }
                        ("location", "partition") => {
                            location = cur_text.trim().chars().next();
                        }
                        ("partitions", "index") => {
                            index_partition = cur_text.trim().chars().next().unwrap_or(PART_INDEX);
                        }
                        ("partitions", "data") => {
                            data_partition = cur_text.trim().chars().next().unwrap_or(PART_DATA);
                        }
                        _ => {}
                    }
                    cur_text.clear();
                }
                _ => {}
            }
        }

        let volume_uuid = volume_uuid
            .ok_or_else(|| TapeError::Ltfs("LTFS label 缺少 volumeuuid".into()))?;
        let location = location
            .ok_or_else(|| TapeError::Ltfs("LTFS label 缺少 location/partition".into()))?;
        if blocksize == 0 {
            return Err(TapeError::Ltfs("LTFS label blocksize 为 0".into()));
        }

        Ok(Self {
            version,
            creator,
            format_time,
            volume_uuid,
            location,
            index_partition,
            data_partition,
            blocksize,
            compression,
        })
    }

    /// 序列化为 XML（UTF-8，带 XML 声明）。
    pub fn to_xml(&self) -> Result<Bytes> {
        let mut out = Vec::with_capacity(512);
        let mut w = Writer::new_with_indent(&mut out, b' ', 2);
        w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

        let mut root = BytesStart::new("ltfslabel");
        root.push_attribute(("version", self.version.as_str()));
        w.write_event(Event::Start(root))?;

        write_text_el(&mut w, "creator", &self.creator)?;
        write_text_el(&mut w, "formattime", &self.format_time)?;
        write_text_el(&mut w, "volumeuuid", &self.volume_uuid.to_string())?;

        w.write_event(Event::Start(BytesStart::new("location")))?;
        write_text_el(&mut w, "partition", &self.location.to_string())?;
        w.write_event(Event::End(BytesEnd::new("location")))?;

        w.write_event(Event::Start(BytesStart::new("partitions")))?;
        write_text_el(&mut w, "index", &self.index_partition.to_string())?;
        write_text_el(&mut w, "data", &self.data_partition.to_string())?;
        w.write_event(Event::End(BytesEnd::new("partitions")))?;

        let mut bs = String::new();
        let _ = write!(bs, "{}", self.blocksize);
        write_text_el(&mut w, "blocksize", &bs)?;
        write_text_el(&mut w, "compression", if self.compression { "true" } else { "false" })?;

        w.write_event(Event::End(BytesEnd::new("ltfslabel")))?;
        Ok(Bytes::from(out))
    }
}

fn write_text_el<W: std::io::Write>(w: &mut Writer<W>, tag: &str, text: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag)))?;
    w.write_event(Event::Text(BytesText::new(text)))?;
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

fn ascii_trimmed(b: &[u8]) -> Result<String> {
    let s = std::str::from_utf8(b)?;
    Ok(s.trim_end_matches(|c: char| c == ' ' || c == '\0').to_string())
}

fn pad_ascii(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len());
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    for b in &mut dst[n..] {
        *b = b' ';
    }
}

fn parse_bool(s: &str) -> bool {
    matches!(s, "true" | "1" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vol1_roundtrip() {
        let buf = Vol1Label::encode("ABC123", "owner");
        let l = Vol1Label::parse(&buf).unwrap();
        assert_eq!(l.volume_id, "ABC123");
        assert_eq!(l.owner, "owner");
        assert_eq!(buf[79], b'4');
        assert_eq!(&buf[24..37], VOL1_IMPL_ID);
    }

    #[test]
    fn ltfs_label_roundtrip() {
        let uuid = Uuid::nil();
        let orig = LtfsLabel::new(uuid, PART_INDEX, 524288, "tape-rs".into());
        let xml = orig.to_xml().unwrap();
        let back = LtfsLabel::parse(&xml).unwrap();
        assert_eq!(back.volume_uuid, uuid);
        assert_eq!(back.location, 'a');
        assert_eq!(back.blocksize, 524288);
        assert!(!back.compression);
    }
}
