//! LTFS (Linear Tape File System) 实现。
//!
//! 对标 SNIA LTFS Format Specification 2.4 / ISO 20919。
//! 磁带被分成两个 partition：
//! - P0 Index Partition（容量约 37.5 GB on LTO-8）：仅保存 label + 最新 index XML
//! - P1 Data Partition（容量约 12 TB on LTO-8）：用户数据 + 每次 session close 时的 index snapshot
//!
//! 写入流程：挂载后数据 append 到 P1；commit/umount 时将当前 index 写到 P1 末尾（形成
//! "previous generation → current" 的链），然后 LOCATE 到 P0 覆盖写入最新 index；
//! 同时通过 WRITE ATTRIBUTE 更新 MAM 的 Volume Coherency Info (0x080B/0x080C)，
//! 指向 P1 中最新 index 所在 logical block。
//!
//! 读取流程：读 VOL1 + LTFS label → 读 MAM VCI 找到 P1 最新 index 位置 → LOCATE +
//! READ → quick-xml 解析出 extent map → 对具体文件再 LOCATE(16) 到数据 extent 读出。

pub mod index;
pub mod label;
pub mod mam;
pub mod mkltfs;
pub mod volume;
