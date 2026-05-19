//! Multi-volume catalog：跨盘 SQLite 索引。
//!
//! 单盘的 catalog 本身是 LTFS Index XML（存在于 P0 block 4 起），
//! 本模块只做**外层聚合**：把很多盘的 index 快照 + extent 列表灌进本地 SQLite，
//! 使得不插带也能做 "foo.mp4 在哪盘 / 按 block 顺序规划读" 这类操作。
//!
//! 文件位置解析优先级（见 `resolve_path`）：
//! 1. 显式 `--catalog <path>`；
//! 2. `$TAPE_RS_CATALOG` 环境变量；
//! 3. `$XDG_DATA_HOME/tape-rs/catalog.db`，或 `$HOME/.local/share/tape-rs/catalog.db`。

pub mod db;
pub mod query;
pub mod sync;

pub use db::{Catalog, resolve_path};
pub use query::{CapacitySnapshot, FileHit, VolumeRow};
pub use sync::{SyncStats, sync_from_device};
