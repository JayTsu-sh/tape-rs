use thiserror::Error;

#[derive(Error, Debug)]
pub enum TapeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SCSI command failed: status={status:#04x}, sense_key={sense_key:#04x}, asc={asc:#04x}, ascq={ascq:#04x}")]
    ScsiCommand {
        status: u8,
        sense_key: u8,
        asc: u8,
        ascq: u8,
    },

    /// SCSI status 0x18: 另一个 initiator 持有 Persistent Reservation。
    /// 排查：`sg_persist --in -r /dev/sgX` 看 reservation holder key。
    #[error("SCSI reservation conflict on {device}: another initiator holds the reservation \
             (run `sg_persist --in -r {device}` to see holder key, then coordinate release)")]
    ReservationConflict { device: String },

    /// SCSI status 0x08: 目标暂忙（例如机械手或 drive 正在执行其它命令）。
    #[error("SCSI target busy on {device}")]
    Busy { device: String },

    /// 其它非 GOOD/CHECK CONDITION 的 SCSI status（task set full、ACA active 等）。
    #[error("SCSI command returned unexpected status {status:#04x} on {device}")]
    ScsiStatus { device: String, status: u8 },

    #[error("SCSI host error: {0:#06x}")]
    ScsiHost(u16),

    #[error("SCSI driver error: {0:#06x}")]
    ScsiDriver(u16),

    /// 命令在 SCSI 层报成功，但事后读到的库状态与预期不符
    /// （比如 MOVE MEDIUM 返回 OK 但 source/dest 没换过来）。
    #[error("Library state inconsistent after operation: {0}")]
    InconsistentState(String),

    #[error("Device not ready: {0}")]
    NotReady(String),

    #[error("Move failed: {reason}")]
    MoveFailed { reason: String },

    /// 命令被前置守护拒绝（破坏性操作未带确认 flag、参数互斥违反等）。
    /// 区别于 MoveFailed：本错误表示命令没有发出，硬件状态未变化。
    #[error("Refused: {reason}")]
    Refused { reason: String },

    #[error("Invalid response: expected {expected} bytes, got {actual}")]
    InvalidResponse { expected: usize, actual: usize },

    #[error("Nix errno: {0}")]
    Nix(#[from] nix::errno::Errno),

    #[error("LTFS format error: {0}")]
    Ltfs(String),

    #[error("XML error: {0}")]
    Xml(String),

    /// XML 库返回的原始错误（保留 source 链便于 `Error::source()` 调试）。
    /// `?` 操作符从 `quick_xml::Error` 自动归到这里。
    #[error("XML parse error")]
    XmlSource(#[from] quick_xml::Error),

    #[error("Catalog error: {0}")]
    Catalog(String),

    /// SQLite 返回的原始错误（保留 source 链）。
    /// `?` 从 `rusqlite::Error` 自动归到这里。
    #[error("Catalog DB error")]
    CatalogDb(#[from] rusqlite::Error),
}

impl From<std::str::Utf8Error> for TapeError {
    fn from(e: std::str::Utf8Error) -> Self {
        TapeError::Ltfs(format!("utf8: {}", e))
    }
}

impl From<std::num::ParseIntError> for TapeError {
    fn from(e: std::num::ParseIntError) -> Self {
        TapeError::Ltfs(format!("parse int: {}", e))
    }
}

pub type Result<T> = std::result::Result<T, TapeError>;
