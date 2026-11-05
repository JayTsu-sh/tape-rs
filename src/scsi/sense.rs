//! SCSI Sense Data 解析

/// 解析后的 Sense Data
///
/// 捕获长度由解析器确定，调用方不能将被截断证据改成完整证据：
/// ```compile_fail
/// use tape_rs::scsi::sense::SenseInfo;
/// let mut info = SenseInfo::from_bytes(&[0x70]);
/// info.received_len = 255;
/// ```
#[derive(Debug, Clone)]
pub struct SenseInfo {
    pub response_code: u8,
    /// 实际收到的长度；原始诊断最多保留 SG 单字节长度能表示的 255 字节。
    received_len: usize,
    /// 实际数据与 additional sense length 共同限定的有效范围。
    effective_len: usize,
    raw: Vec<u8>,
    pub sense_key: u8,
    pub asc: u8,
    pub ascq: u8,
    pub valid: bool,
    /// 固定格式 byte 2 bit 7：读到文件标记（SSC）。
    pub filemark: bool,
    /// 固定格式 byte 2 bit 6：到达介质起点/终点（BOP/EOP）。
    pub eom: bool,
    /// 固定格式 byte 2 bit 5：记录长度与请求长度不符（ILI）。
    pub ili: bool,
    /// 固定格式 bytes 3..7 的 INFORMATION 字段；仅当 byte 0 的 VALID 位为 1 时有意义。
    /// 变长读时 = 请求长度 − 实际记录长度（有符号）。
    pub information: Option<i32>,
}

impl SenseInfo {
    /// 从原始 sense buffer 解析
    pub fn from_bytes(buf: &[u8]) -> Self {
        let raw = buf[..buf.len().min(u8::MAX as usize)].to_vec();
        let effective_len = raw.len().min(
            raw.get(7)
                .map_or(raw.len(), |additional| 8 + usize::from(*additional)),
        );
        let mut sense = Self {
            response_code: raw.first().copied().unwrap_or(0) & 0x7f,
            received_len: buf.len(),
            effective_len,
            raw,
            sense_key: 0,
            asc: 0,
            ascq: 0,
            valid: false,
            filemark: false,
            eom: false,
            ili: false,
            information: None,
        };
        // Preserve existing SSC flag/key parsing. Strict fault attribution must
        // use current_condition(), not the legacy recognized-format flag alone.
        let raw = &sense.raw;
        match sense.response_code {
            0x70 | 0x71 => {
                let flags = raw.get(2).copied().unwrap_or(0);
                sense.sense_key = flags & 0x0f;
                sense.asc = raw.get(12).copied().unwrap_or(0);
                sense.ascq = raw.get(13).copied().unwrap_or(0);
                sense.valid = true;
                sense.filemark = flags & 0x80 != 0;
                sense.eom = flags & 0x40 != 0;
                sense.ili = flags & 0x20 != 0;
                if raw[0] & 0x80 != 0 && raw.len() > 6 {
                    sense.information = Some(i32::from_be_bytes([raw[3], raw[4], raw[5], raw[6]]));
                }
            }
            0x72 | 0x73 => {
                sense.sense_key = raw.get(1).copied().unwrap_or(0) & 0x0f;
                sense.asc = raw.get(2).copied().unwrap_or(0);
                sense.ascq = raw.get(3).copied().unwrap_or(0);
                sense.valid = true;
            }
            _ => {}
        }
        sense
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn received_len(&self) -> usize {
        self.received_len
    }

    pub fn effective_len(&self) -> usize {
        self.effective_len
    }

    /// 只返回长度充分的 current condition；不推断命令或介质归属。
    pub fn current_condition(&self) -> Option<(u8, u8, u8)> {
        let raw = &self.raw;
        let declared = 8 + usize::from(*raw.get(7)?);
        if self.received_len != raw.len() || raw.len() < declared {
            return None;
        }
        // Derive from immutable captured bytes, not mutable legacy fields.
        match raw[0] & 0x7f {
            0x70 if declared >= 14 => Some((raw[2] & 0x0f, raw[12], raw[13])),
            0x72 => Some((raw[1] & 0x0f, raw[2], raw[3])),
            _ => None,
        }
    }

    pub fn sense_key_str(&self) -> &'static str {
        match self.sense_key {
            0x00 => "NO SENSE",
            0x01 => "RECOVERED ERROR",
            0x02 => "NOT READY",
            0x03 => "MEDIUM ERROR",
            0x04 => "HARDWARE ERROR",
            0x05 => "ILLEGAL REQUEST",
            0x06 => "UNIT ATTENTION",
            0x07 => "DATA PROTECT",
            0x08 => "BLANK CHECK",
            0x09 => "VENDOR SPECIFIC",
            0x0B => "ABORTED COMMAND",
            0x0D => "VOLUME OVERFLOW",
            0x0E => "MISCOMPARE",
            _ => "UNKNOWN",
        }
    }

    pub fn is_ok(&self) -> bool {
        self.sense_key == 0x00 || self.sense_key == 0x01
    }
}

impl std::fmt::Display for SenseInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] ASC={:#04x} ASCQ={:#04x}",
            self.sense_key_str(),
            self.asc,
            self.ascq
        )
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::*;

    #[test]
    fn current_condition_requires_complete_declared_data_and_never_accepts_deferred() {
        for (response, size, fields) in [
            (0x70, 18, (2, 12, 13)),
            (0x71, 18, (2, 12, 13)),
            (0x72, 8, (1, 2, 3)),
            (0x73, 8, (1, 2, 3)),
        ] {
            let mut raw = vec![0u8; size];
            raw[0] = response;
            raw[7] = (size - 8) as u8;
            raw[fields.0] = 3;
            raw[fields.1] = 0x11;
            for end in 0..=size {
                let sense = SenseInfo::from_bytes(&raw[..end]);
                assert_eq!(sense.received_len, end);
                assert_eq!(sense.raw_bytes(), &raw[..end]);
                assert_eq!(sense.effective_len, end);
                assert_eq!(
                    sense.current_condition(),
                    (end == size && matches!(response, 0x70 | 0x72)).then_some((3, 0x11, 0)),
                    "response={response:#x} end={end}"
                );
            }
        }
    }

    #[test]
    fn captures_unknown_and_bounds_raw_data_without_trusting_padding_or_cached_fields() {
        let mut raw = [0u8; 18];
        raw[0] = 0x70;
        raw[2] = 3;
        raw[12] = 0x11;
        assert_eq!(SenseInfo::from_bytes(&raw).effective_len, 8);
        assert!(SenseInfo::from_bytes(&raw).current_condition().is_none());
        raw[7] = 6;
        let mut sense = SenseInfo::from_bytes(&raw);
        assert_eq!(sense.effective_len, 14);
        sense.sense_key = 0;
        sense.asc = 0;
        assert_eq!(sense.current_condition(), Some((3, 0x11, 0)));
        raw[0] = 0x7f;
        let unknown = SenseInfo::from_bytes(&raw);
        assert_eq!(unknown.raw_bytes(), raw);
        assert_eq!(unknown.response_code, 0x7f);
        assert!(!unknown.valid);
        assert!(unknown.current_condition().is_none());
        raw[0] = 0x70;
        let mut oversized = vec![0u8; 4096];
        oversized[..18].copy_from_slice(&raw);
        let mut sense = SenseInfo::from_bytes(&oversized);
        sense.response_code = 0x70;
        sense.sense_key = 3;
        sense.asc = 0x11;
        sense.valid = true;
        assert_eq!(sense.received_len, 4096);
        assert_eq!(sense.raw_bytes().len(), 255);
        assert!(sense.current_condition().is_none());
    }

    #[test]
    fn legacy_fixed_flags_and_information_are_preserved() {
        let mut raw = [0u8; 18];
        raw[0] = 0xf0;
        raw[2] = 0xe0;
        raw[3..7].copy_from_slice(&(-42i32).to_be_bytes());
        raw[7] = 10;
        let sense = SenseInfo::from_bytes(&raw);
        assert!(sense.filemark && sense.eom && sense.ili);
        assert_eq!(sense.information, Some(-42));
        assert!(sense.is_ok());
        assert_eq!(sense.current_condition(), Some((0, 0, 0)));
    }

    #[test]
    fn ibm_extended_sense_requires_the_full_declared_96_bytes() {
        let mut raw = [0u8; 96];
        raw[0] = 0x70;
        raw[2] = 5;
        raw[7] = 88;
        raw[12] = 0x24;
        let short = SenseInfo::from_bytes(&raw[..64]);
        assert_eq!(short.effective_len, 64);
        assert!(short.current_condition().is_none());
        let full = SenseInfo::from_bytes(&raw);
        assert_eq!(full.effective_len, 96);
        assert_eq!(full.raw_bytes(), raw);
        assert_eq!(full.current_condition(), Some((5, 0x24, 0)));
    }
}
