//! LTFS (Linear Tape File System) 实现。
//!
//! 按 SNIA LTFS Format 2.5.1 处理 Full / Incremental 索引（XML 版本 2.5.0）。
//! P0 为索引分区，只写完整索引；P1 为数据分区，追加文件数据和完整或增量索引。
//! `commit_incremental` 通常只追加 P1；连续增量达到上限后写 P1/P0 完整检查点。
//! `commit` 和清洁 `unmount` 收束为完整索引，IP 回指同代 DP Full。
//! 索引构造持久化屏障完成后更新各分区 MAM VCI，再发布已提交视图。
//!
//! 挂载通过 VCI 提示或反向扫描定位索引，校验依赖链，从 Full 重放后续增量。
//! 未索引数据、残缺构造和链断裂按恢复协议限制写入，不自动覆盖尾部。

pub mod index;
pub mod incremental;
mod namespace;
pub mod label;
pub mod mam;
pub mod mkltfs;
pub mod recovery;
pub mod volume;
