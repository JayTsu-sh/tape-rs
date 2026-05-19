//! CLI 子模块：各子命令按设备族（changer / tape / ltfs / catalog）分组。
//!
//! main.rs 只负责 clap 定义和 dispatcher，业务逻辑放在这些子模块里。

pub mod catalog_cli;
pub mod changer;
pub mod format_util;
pub mod inquiry;
pub mod ltfs_cli;
pub mod tape;
