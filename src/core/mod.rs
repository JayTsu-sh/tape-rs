//! ltfsd-core：与设备无关的引擎状态（见 `.scratch/ltfs-ha/module-seams.md`）。
//!
//! 目前只包含 D03 的每卷状态根与差量发布；调度、资源目录、控制面门控后续加入。

pub mod volume_state;
