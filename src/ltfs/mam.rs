//! MAM (Medium Auxiliary Memory) 属性访问。
//!
//! MAM 是磁带盒内的小容量 NVRAM（LTO 上约 8 KiB），通过 SCSI
//! READ ATTRIBUTE (0x8C) / WRITE ATTRIBUTE (0x8D) 访问，每条属性由
//! identifier(2) + format(1) + length(2) + value 的 TLV 构成。
//!
//! 我们实际使用的 LTFS 相关属性（SPC-5 Table 394）：
//! - 0x0800 Application Vendor         (TEXT, 8)
//! - 0x0801 Application Name           (TEXT, 32)
//! - 0x0802 Application Version        (TEXT, 8)
//! - 0x0806 Barcode                    (ASCII, 32)
//! - 0x080B Application Format Version (ASCII, 16, "2.4.0")
//! - 0x080C Volume Coherency Info      (BINARY, variable, LTFS 2.5.1 §10.2)
//! - 0x0009 Volume Change Reference    (BINARY, 只读, 驱动器维护)

use bytes::Bytes;
use log::debug;
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::scsi::cdb;
use crate::scsi::transport::{TapeTransport, retry_unit_attention};

/// MAM attribute format code（SPC-5 Table 395）。
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum AttributeFormat {
    Binary = 0,
    Ascii = 1,
    Text = 2,
}

/// 0x0000 REMAINING CAPACITY IN PARTITION，u64 BE。
/// SPC-5 名义上是 MB (10^6)，但 IBM / HP LTO 驱动器实际返回的是 MiB (2^20)，
/// sg3_utils `sg_read_attr` 输出也标 `[MiB]`，按实际行为统一成 MiB。
pub const ATTR_REMAINING_CAPACITY: u16 = 0x0000;
/// 0x0001 MAXIMUM CAPACITY IN PARTITION，u64 BE，单位同上（MiB）。
pub const ATTR_MAXIMUM_CAPACITY: u16 = 0x0001;
/// LTFS Application Vendor / Name / Version。
pub const ATTR_APP_VENDOR: u16 = 0x0800;
pub const ATTR_APP_NAME: u16 = 0x0801;
pub const ATTR_APP_VERSION: u16 = 0x0802;
/// 0x0806 = Barcode (ASCII, 32 bytes)。
/// **注意**：早期代码错用 0x0804（那是 "Date and Time Last Written"，驱动器拒写）。
pub const ATTR_BARCODE: u16 = 0x0806;
/// 0x080B = Application Format Version（LTFS 2.4 Annex B.3.1，ASCII 16 bytes "2.4.0"）
pub const ATTR_APP_FORMAT_VERSION: u16 = 0x080B;
/// 0x080C = Volume Coherency Information（LTFS 2.4 Annex B.3.2，Binary）。
/// **注意**：早期代码错用 0x080B（那是 ASCII 的 Format Version）。
pub const ATTR_VCI: u16 = 0x080C;

/// 解析后的单个 MAM attribute。
#[derive(Debug, Clone)]
pub struct MamAttribute {
    pub id: u16,
    pub format: u8,
    pub value: Bytes,
}

/// 单个 partition 的容量信息（字节，从 MAM 0x0000/0x0001 的 MB 字段换算）。
#[derive(Debug, Clone, Copy)]
pub struct PartitionCapacity {
    pub partition: u8,
    pub remaining: u64,
    pub maximum: u64,
}

/// 整卷容量 = 所有 partition 相加。
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeCapacity {
    pub total: u64,
    pub remaining: u64,
}

impl VolumeCapacity {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.remaining)
    }
}

/// 0x0009 = VOLUME CHANGE REFERENCE（SSC 设备属性，二进制、变长，驱动器维护、只读）。
/// 介质每次被写入后变化；LTFS 2.5.1 §10.3 用它判断 VCI 是否仍然新鲜。
pub const ATTR_VCR: u16 = 0x0009;

/// LTFS 2.5.1 §10.2 Table 17 / §10.3 Table 18 的 Volume Coherency Information。
///
/// 二进制布局：
/// ```text
/// 0            VCR Length (1 byte)
/// 1..1+L       VCR（写 VCI 时读到的介质 VCR 原样）
/// +0..+8       generation number（VOLUME COHERENCY COUNT，u64 BE）
/// +8..+16      block number（VOLUME COHERENCY SET IDENTIFIER，u64 BE；该分区上最新索引的起始块，0 非法）
/// +16..+18     ACSI Length (u16 BE) = 43
/// +18..+61     ACSI: "LTFS" 0x00 <UUID 36 字节> 0x00 0x01
/// ```
///
/// 每个分区各有一份 VCI，`block` 指向该分区自己的最新索引。读端只有在 VCI 里的 VCR
/// 与当前介质 VCR 相等时才把 `block` 当作定位提示，且仍要读取并核验实际索引。
pub const VCI_ACSI_LEN: u16 = 43;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeCoherencyInfo {
    pub vcr: Vec<u8>,
    pub generation: u64,
    pub block: u64,
    pub volume_uuid: Uuid,
}

/// VCR 全 0 或全 1 视为无效（§10.3 第 4 步）。
/// VCR 按数值比较。MAM 0x0009 是 4 字节，而 IBM LTFS 在 VCI 里把它存成 8 字节
/// （实测 IBM LTFS 2.4.8.3：`08 00 00 00 00 00 00 00 33`），所以不能按字节串比。
pub fn vcr_matches(a: &[u8], b: &[u8]) -> bool {
    fn strip(v: &[u8]) -> &[u8] {
        let n = v.iter().take_while(|&&x| x == 0).count();
        &v[n..]
    }
    strip(a) == strip(b)
}

/// VCI 里 VCR 的写出形式：与 IBM LTFS 相同，零扩展到 8 字节。更长的原样保留。
pub fn vcr_for_vci(vcr: &[u8]) -> Vec<u8> {
    if vcr.len() >= 8 {
        return vcr.to_vec();
    }
    let mut out = vec![0u8; 8 - vcr.len()];
    out.extend_from_slice(vcr);
    out
}

pub fn vcr_is_valid(vcr: &[u8]) -> bool {
    !vcr.is_empty() && !vcr.iter().all(|&b| b == 0) && !vcr.iter().all(|&b| b == 0xFF)
}

impl VolumeCoherencyInfo {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let vcr = vcr_for_vci(&self.vcr);
        let vcr_len = u8::try_from(vcr.len())
            .map_err(|_| TapeError::Ltfs(format!("VCR 长度 {} 超过 255", vcr.len())))?;
        let mut b = Vec::with_capacity(1 + vcr.len() + 8 + 8 + 2 + VCI_ACSI_LEN as usize);
        b.push(vcr_len);
        b.extend_from_slice(&vcr);
        b.extend_from_slice(&self.generation.to_be_bytes());
        b.extend_from_slice(&self.block.to_be_bytes());
        b.extend_from_slice(&VCI_ACSI_LEN.to_be_bytes());
        b.extend_from_slice(b"LTFS\0");
        let uuid_str = self.volume_uuid.as_hyphenated().to_string();
        debug_assert_eq!(uuid_str.len(), 36);
        b.extend_from_slice(uuid_str.as_bytes());
        b.push(0x00);
        b.push(0x01);
        Ok(b)
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let need = |n: usize| -> Result<()> {
            if buf.len() < n {
                Err(TapeError::Ltfs(format!(
                    "VCI 截断: len={} 需要 {}",
                    buf.len(),
                    n
                )))
            } else {
                Ok(())
            }
        };
        need(1)?;
        let vcr_len = buf[0] as usize;
        let mut off = 1;
        need(off + vcr_len + 8 + 8 + 2)?;
        let vcr = buf[off..off + vcr_len].to_vec();
        off += vcr_len;
        let generation = u64::from_be_bytes(buf[off..off + 8].try_into().unwrap());
        off += 8;
        let block = u64::from_be_bytes(buf[off..off + 8].try_into().unwrap());
        off += 8;
        let acsi_len = u16::from_be_bytes([buf[off], buf[off + 1]]) as usize;
        off += 2;
        need(off + acsi_len)?;
        let acsi = &buf[off..off + acsi_len];
        if acsi.len() < 41 || &acsi[0..5] != b"LTFS\0" {
            return Err(TapeError::Ltfs(format!(
                "VCI ACSI 不是 LTFS 格式: {:02x?}",
                &acsi[..acsi.len().min(8)]
            )));
        }
        let uuid_str = std::str::from_utf8(&acsi[5..41])
            .map_err(|e| TapeError::Ltfs(format!("VCI UUID 非 UTF-8: {}", e)))?;
        let volume_uuid = Uuid::parse_str(uuid_str)
            .map_err(|e| TapeError::Ltfs(format!("VCI UUID 解析失败 {:?}: {}", uuid_str, e)))?;
        if block == 0 {
            return Err(TapeError::Ltfs("VCI block number 为 0（非法）".into()));
        }
        Ok(Self {
            vcr,
            generation,
            block,
            volume_uuid,
        })
    }
}

/// MAM 访问封装。固定操作 partition 0（LTFS 中 VCI 写在两个 partition 都行，我们
/// 只用 P0 保持一致）。
pub struct Mam<'a> {
    device: &'a dyn TapeTransport,
    partition: u8,
}

impl<'a> Mam<'a> {
    pub fn new(device: &'a dyn TapeTransport) -> Self {
        Self {
            device,
            partition: 0,
        }
    }

    pub fn with_partition(device: &'a dyn TapeTransport, partition: u8) -> Self {
        Self { device, partition }
    }

    /// READ ATTRIBUTE service_action=0x00 (VALUES) 读取单个属性。
    ///
    /// 响应格式：header 4 字节（total length BE）+ attribute entries
    /// （id(2)+fmt(1)+len(2)+value）。
    pub fn read_attribute(&self, attr_id: u16) -> Result<Option<MamAttribute>> {
        const ALLOC: u32 = 512;
        let cdb_bytes = cdb::read_attribute(0x00, self.partition, attr_id, ALLOC);
        let mut buf = [0u8; ALLOC as usize];
        let result = retry_unit_attention("READ ATTRIBUTE", || self.device.execute_read(&cdb_bytes, &mut buf, 30_000))?;

        if result.transferred < 4 {
            return Ok(None);
        }
        let avail = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let end = (4 + avail).min(result.transferred);

        let mut off = 4;
        while off + 5 <= end {
            let id = u16::from_be_bytes([buf[off], buf[off + 1]]);
            let fmt = buf[off + 2] & 0x03;
            let len = u16::from_be_bytes([buf[off + 3], buf[off + 4]]) as usize;
            let val_start = off + 5;
            let val_end = val_start + len;
            if val_end > end {
                break;
            }
            if id == attr_id {
                debug!("MAM attr {:#06x} fmt={} len={}", id, fmt, len);
                return Ok(Some(MamAttribute {
                    id,
                    format: fmt,
                    value: Bytes::copy_from_slice(&buf[val_start..val_end]),
                }));
            }
            off = val_end;
        }
        Ok(None)
    }

    /// WRITE ATTRIBUTE 写入单个属性。
    pub fn write_attribute(&self, attr: &MamAttribute) -> Result<()> {
        let value_len = u16::try_from(attr.value.len()).map_err(|_| {
            TapeError::Ltfs(format!("attribute 值 {} 字节超 u16", attr.value.len()))
        })?;
        // parameter list = 4-byte total length header + entry (id 2 + fmt 1 + len 2 + value)
        let entry_len = 5 + value_len as usize;
        let total_len = entry_len; // total length field covers attribute entries only
        let param_len = 4 + entry_len;

        let mut buf = vec![0u8; param_len];
        buf[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
        buf[4..6].copy_from_slice(&attr.id.to_be_bytes());
        buf[6] = attr.format & 0x03;
        buf[7..9].copy_from_slice(&value_len.to_be_bytes());
        buf[9..9 + attr.value.len()].copy_from_slice(&attr.value);

        let cdb_bytes = cdb::write_attribute(true, self.partition, param_len as u32);
        retry_unit_attention("WRITE ATTRIBUTE", || self.device.execute_write(&cdb_bytes, &buf, 30_000))?;
        debug!("MAM write attr {:#06x} {} bytes", attr.id, attr.value.len());
        Ok(())
    }

    /// 读 8-byte 无符号 big-endian 数值型属性（0x0000/0x0001/0x0003 等）。
    /// 长度不是 8 字节时返回 `None`。
    pub fn read_u64(&self, attr_id: u16) -> Result<Option<u64>> {
        match self.read_attribute(attr_id)? {
            Some(a) if a.value.len() == 8 => {
                let arr: [u8; 8] = a.value[..].try_into().unwrap();
                Ok(Some(u64::from_be_bytes(arr)))
            }
            _ => Ok(None),
        }
    }

    /// 读单个 partition 的 Remaining / Maximum 容量（MAM 0x0000 / 0x0001），
    /// 换算成 bytes（原值 × 1 MiB）。两个属性都缺失时返回 `Ok(None)`。
    pub fn read_partition_capacity(&self) -> Result<Option<PartitionCapacity>> {
        const MIB: u64 = 1 << 20;
        let remaining = self.read_u64(ATTR_REMAINING_CAPACITY)?;
        let maximum = self.read_u64(ATTR_MAXIMUM_CAPACITY)?;
        match (remaining, maximum) {
            (None, None) => Ok(None),
            _ => Ok(Some(PartitionCapacity {
                partition: self.partition,
                remaining: remaining.unwrap_or(0).saturating_mul(MIB),
                maximum: maximum.unwrap_or(0).saturating_mul(MIB),
            })),
        }
    }

    /// 读取介质 VCR（0x0009）。驱动器不支持或属性缺失时返回 `None`。
    pub fn read_vcr(&self) -> Result<Option<Bytes>> {
        Ok(self.read_attribute(ATTR_VCR)?.map(|a| a.value))
    }

    pub fn read_vci(&self) -> Result<Option<VolumeCoherencyInfo>> {
        match self.read_attribute(ATTR_VCI)? {
            Some(a) => Ok(Some(VolumeCoherencyInfo::decode(&a.value)?)),
            None => Ok(None),
        }
    }

    pub fn write_vci(&self, vci: &VolumeCoherencyInfo) -> Result<()> {
        let attr = MamAttribute {
            id: ATTR_VCI,
            format: AttributeFormat::Binary as u8,
            value: Bytes::from(vci.encode()?),
        };
        self.write_attribute(&attr)
    }

    /// 写入 ASCII 标识属性（会按 max_len 空格补齐/截断）。
    /// 用于 0x0800/0x0801/0x0802/0x0804 等 LTFS identification attributes。
    pub fn write_ascii(&self, attr_id: u16, text: &str, max_len: usize) -> Result<()> {
        self.write_string(attr_id, text, max_len, AttributeFormat::Ascii)
    }

    /// 写入 TEXT 格式属性（0x0805/0x0806 这类 multi-locale 文本）。
    pub fn write_text(&self, attr_id: u16, text: &str, max_len: usize) -> Result<()> {
        self.write_string(attr_id, text, max_len, AttributeFormat::Text)
    }

    fn write_string(
        &self,
        attr_id: u16,
        text: &str,
        max_len: usize,
        format: AttributeFormat,
    ) -> Result<()> {
        let mut buf = vec![b' '; max_len];
        let n = text.len().min(max_len);
        buf[..n].copy_from_slice(&text.as_bytes()[..n]);
        let attr = MamAttribute {
            id: attr_id,
            format: format as u8,
            value: Bytes::from(buf),
        };
        self.write_attribute(&attr)
    }
}

/// 读 P0 + P1 的容量并合成卷级容量。
///
/// 单 partition 磁带（裸带或只用 P0）读 P1 会失败，这里把单个 partition 的错误
/// 降级为 "该 partition 不可用"，只要至少有一个读到就返回 Ok；全部失败才报错。
pub fn read_volume_capacity(device: &dyn TapeTransport) -> Result<VolumeCapacity> {
    let mut cap = VolumeCapacity::default();
    let mut any_ok = false;
    let mut last_err: Option<TapeError> = None;

    for partition in [0u8, 1u8] {
        let mam = Mam::with_partition(device, partition);
        match mam.read_partition_capacity() {
            Ok(Some(pc)) => {
                cap.total = cap.total.saturating_add(pc.maximum);
                cap.remaining = cap.remaining.saturating_add(pc.remaining);
                any_ok = true;
            }
            Ok(None) => {
                debug!("MAM partition {} 容量属性缺失", partition);
            }
            Err(e) => {
                debug!("MAM partition {} 容量读失败: {}", partition, e);
                last_err = Some(e);
            }
        }
    }

    if any_ok {
        Ok(cap)
    } else if let Some(e) = last_err {
        Err(e)
    } else {
        Ok(cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IBM LTFS 2.4.8.3 在 EE 下写出的 DP VCI，取自实验室克隆带的 MAM 0x080C。
    #[test]
    fn decodes_vci_written_by_ibm_ltfs() {
        let hex = "08000000000000003300000000000000030000000000000023002B4C54465300\
                   63306362356261382D376565342D343666652D383864312D376136363234306230363535\
                   0001";
        let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        let raw: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let v = VolumeCoherencyInfo::decode(&raw).unwrap();
        assert_eq!((v.generation, v.block), (3, 0x23));
        assert_eq!(v.volume_uuid.to_string(), "c0cb5ba8-7ee4-46fe-88d1-7a66240b0655");
        // 驱动器的 MAM 0x0009 是 4 字节
        assert!(vcr_matches(&v.vcr, &[0, 0, 0, 0x33]));
        assert!(!vcr_matches(&v.vcr, &[0, 0, 0, 0x34]));
        // tape-rs 对同一内容的编码与 IBM 逐字节相同
        assert_eq!(v.encode().unwrap(), raw);
    }

    #[test]
    fn vci_roundtrip() {
        let uuid = Uuid::parse_str("b321d431-ad2e-469d-bb6e-30d8e2a42f08").unwrap();
        let v = VolumeCoherencyInfo {
            vcr: vec![0, 0, 1, 7],
            generation: 42,
            block: 1632,
            volume_uuid: uuid,
        };
        let b = v.encode().unwrap();
        // 与 IBM LTFS 相同：VCR 零扩展到 8 字节
        assert_eq!(b.len(), 1 + 8 + 8 + 8 + 2 + 43);
        assert_eq!(&b[..9], &[8, 0, 0, 0, 0, 0, 0, 1, 7]);
        assert_eq!(&b[25..27], &43u16.to_be_bytes());
        assert_eq!(&b[27..32], b"LTFS\0");
        assert_eq!(&b[b.len() - 2..], &[0x00, 0x01]);
        let back = VolumeCoherencyInfo::decode(&b).unwrap();
        assert!(vcr_matches(&back.vcr, &v.vcr));
        assert_eq!((back.generation, back.block, back.volume_uuid), (42, 1632, uuid));
        assert!(VolumeCoherencyInfo::decode(&b[..20]).is_err());
        assert!(
            !vcr_is_valid(&[0, 0, 0, 0])
                && !vcr_is_valid(&[0xFF; 4])
                && vcr_is_valid(&[0, 0, 1, 7])
        );
    }
}
