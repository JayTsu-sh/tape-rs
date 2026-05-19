//! SCSI Sense Data 解析

/// 解析后的 Sense Data
#[derive(Debug, Clone)]
pub struct SenseInfo {
    pub response_code: u8,
    pub sense_key: u8,
    pub asc: u8,
    pub ascq: u8,
    pub valid: bool,
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
                Self { response_code, sense_key, asc, ascq, valid: true }
            }
            // Descriptor format (0x72, 0x73)
            0x72 | 0x73 => {
                let sense_key = if buf.len() > 1 { buf[1] & 0x0F } else { 0 };
                let asc = if buf.len() > 2 { buf[2] } else { 0 };
                let ascq = if buf.len() > 3 { buf[3] } else { 0 };
                Self { response_code, sense_key, asc, ascq, valid: true }
            }
            _ => Self::empty(),
        }
    }

    fn empty() -> Self {
        Self { response_code: 0, sense_key: 0, asc: 0, ascq: 0, valid: false }
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
        write!(f, "[{}] ASC={:#04x} ASCQ={:#04x}", self.sense_key_str(), self.asc, self.ascq)
    }
}
