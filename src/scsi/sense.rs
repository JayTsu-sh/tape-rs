//! SCSI Sense Data 解析

/// 解析后的 Sense Data
#[derive(Debug, Clone)]
pub struct SenseInfo {
    pub response_code: u8,
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
        if buf.is_empty() {
            return Self::empty();
        }

        let response_code = buf[0] & 0x7F;

        match response_code {
            // Fixed format (0x70, 0x71)
            0x70 | 0x71 => {
                let sense_key = if buf.len() > 2 { buf[2] & 0x0F } else { 0 };
                let asc = if buf.len() > 12 { buf[12] } else { 0 };
                let ascq = if buf.len() > 13 { buf[13] } else { 0 };
                let flags = if buf.len() > 2 { buf[2] } else { 0 };
                let information = if buf[0] & 0x80 != 0 && buf.len() > 6 {
                    Some(i32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]))
                } else {
                    None
                };
                Self {
                    response_code,
                    sense_key,
                    asc,
                    ascq,
                    valid: true,
                    filemark: flags & 0x80 != 0,
                    eom: flags & 0x40 != 0,
                    ili: flags & 0x20 != 0,
                    information,
                }
            }
            // Descriptor format (0x72, 0x73)
            0x72 | 0x73 => {
                let sense_key = if buf.len() > 1 { buf[1] & 0x0F } else { 0 };
                let asc = if buf.len() > 2 { buf[2] } else { 0 };
                let ascq = if buf.len() > 3 { buf[3] } else { 0 };
                Self {
                    response_code,
                    sense_key,
                    asc,
                    ascq,
                    valid: true,
                    filemark: false,
                    eom: false,
                    ili: false,
                    information: None,
                }
            }
            _ => Self::empty(),
        }
    }

    fn empty() -> Self {
        Self {
            response_code: 0,
            sense_key: 0,
            asc: 0,
            ascq: 0,
            valid: false,
            filemark: false,
            eom: false,
            ili: false,
            information: None,
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
