# LE API 轨迹的参考源码依据（2026-09-27）

## 证据边界

本记录审阅 LinearTapeFileSystem 官方仓库固定提交 `4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb`。安装版 IBM LE 2.4.8.2 包含闭源带库管理、缓存与自动装载逻辑，不能将此参考实现直接当作安装版轨迹。此子任务未访问硬件、未改生产代码；现场分段 syscall/SG_IO 轨迹应由主任务补充。

成功标准：区分 POSIX sync/fsync/fdatasync、LTFS 卷同步与带库管理操作，逐项给出参考调用顺序及会改变轨迹的条件；不能把一个 API 的耗时用于另一个 API 的性能结论。

## IBM LE 2.4.8 官方语义

IBM 明确说明：Linux 的 sync 命令及 sync、fdatasync、fsync 系统调用不会自动触发 LE 卷同步。显式入口是覆盖 ltfs.sync 属性或 leadm tape sync。卷同步把 index 写入 DP；卸载时还将 index 写到 IP，以便下次装载。未完成卷同步就异常关机，最新文件即使已有数据块，也可能因缺失 index 无法检索。本调研未做崩溃注入，不能据此宣称已验证某次 fsync 后的断电结果或裁定 POSIX 合规性。来源：[IBM LE 2.4.8 Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.8?topic=tips-sync-operation)。

## 同步入口

| 入口 | 参考调用路径 | 参考实现的效果及边界 |
| --- | --- | --- |
| `fsync(fd)` | `ltfs_fuse_fsync(path, isdatasync=0)` → `_ltfs_fuse_do_flush` → dirty 时 `ltfs_fsops_flush(dentry,false)` → `iosched_flush` | 只刷新该文件的调度器队列；本入口没有 `ltfs_sync_index`。 |
| `fdatasync(fd)` | 同上，`isdatasync=1` | 参数仅记录 trace，不控制代码分支，和 fsync 使用相同路径。 |
| close 导致的 FUSE FLUSH | `ltfs_fuse_flush` → 同一 `_ltfs_fuse_do_flush` | 同样只在句柄 dirty 时刷新；成功清除句柄 dirty。FLUSH 与最后一次 RELEASE 是不同回调。 |
| 最后 RELEASE | `ltfs_fuse_release` → `ltfs_fsops_close(dirty,open_write,true)` → `iosched_close` | 仅 `sync_type=close` 且文件 `write_index` 时再 `ltfs_sync_index(SYNC_CLOSE,AUTO)`。`sync_type=unmount` 不在此处自动提交卷索引。 |
| 定时同步 | `periodic_sync_thread` → `ltfs_fsops_flush(NULL,false)` → `ltfs_sync_index(SYNC_PERIODIC,AUTO)` | 全卷调度器刷新，再提交索引；只在启用定时模式时运行。 |
| 库的卷同步 | `ltfs_fsops_volume_sync` → `ltfs_fsops_flush(NULL,false)` → `ltfs_sync_index(reason,AUTO)` | 显式全卷索引同步。 |
| 根目录 `setxattr(user.ltfs.sync,...)` | `ltfs_fsops_setxattr` → readiness 检查 → 全卷 flush → 独占卷锁 → `_xattr_set_virtual` → `ltfs_sync_index(SYNC_EA,AUTO)` | 若未编译 `FORMAT_SPEC25`，`ltfs_sync_index` 强制 FULL；编译时为 AUTO。不要将此误写为所有版本必定 Full。 |
| 根目录 `getxattr(ltfs.sync)` 特殊虚拟属性 | `_xattr_get_virtual` → `ltfs_sync_index(SYNC_EA,FULL)` | GET 也有同步副作用，不能当成无副作用的状态查询；这是参考源码事实。 |
| POSIX `sync()` / `syncfs()` | 内核的文件系统同步入口；参考 `struct fuse_operations` 没有独立 sync/syncfs 回调 | 不能仅凭名字将其等同 `ltfs_fsops_volume_sync` 或管理命令 `leadm tape sync`。实际下发 FUSE/SG 请求受内核、libfuse 和 LE 实现影响，必须单独测量。 |

来源：[FUSE 回调](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L427)、[文件 flush 与卷 sync](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c#L1889)、[xattr 前置 flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c#L1071)、[虚拟属性读/写](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/xattr.c#L883)、[定时同步](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/periodic_sync.c#L75)。

调度器的 flush 不等于持久化索引。unified 的 `_unified_flush_unlocked` 将尚未落到 DP 的队列交给 `ltfs_fsraw_write`；此函数没有无条件追加的 `WRITE FILEMARKS(0)` 或卷索引提交。下层 raw write 在需要保护另一分区时可调用 `ltfs_write_index_conditional`，异常写入还可能触发恢复性 index；因此不能断言任何 fsync 轨迹都绝无 FM/index。队列已空时直接返回。FCFS 的 flush 是空操作（数据此前由 write 路径交设备）。因此 **fsync 成功不能仅凭此参考代码解释为卷索引已持久化**，也不能把参考 flush 等价为关闭硬件写缓存。来源：[raw write 条件索引与正常 WRITE 结束路径](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops_raw.c#L117)、[unified flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/iosched/unified.c#L1925)、[FCFS flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/iosched/fcfs.c#L190)。

## 真正的索引同步：预期 SCSI 顺序

`ltfs_sync_index` 先检查 dirty；无变化不必重写索引。通常选择 DP；若 DP 已以 index 结束、IP 尚未以 index 结束，则可选 IP。随后：

1. `ltfs_write_index` → `tape_seek_append_position`，必要时定位 append 点；缓存位置已吻合时不必发 LOCATE。
2. 获取 index self pointer；IP 尚未以 index 结束的特定分支先 `WRITE FILEMARKS count=0, IMMED=0` 刷新。
3. `WRITE FILEMARKS count=1, IMMED=1`，打开 index 文件。
4. XML index 通过 WRITE 写入，XML writer 关闭 FM 使用非 IMMED，以等待介质写入完成。
5. Full index 成功后更新卷一致性 MAM（VCR/VCI），更新内存索引状态。

这解释为什么普通文件 fsync 与显式卷 sync 可能差几个数量级：后者还有索引、同步 FM 和 MAM。具体命令数受 dirty 状态、分区、索引大小和缓存位置影响，不能把一个固定列表当作所有操作的必然轨迹。来源：[index 同步选择](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L3647)、[index 写入](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L2580)、[XML writer](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/xml_writer_libltfs.c#L1336)。

## 打开与关闭文件

已挂载、卷状态有效时：`open` → `ltfs_fuse_open` → `ltfs_fsops_open` → `iosched_open` / `ltfs_fsraw_open` → 目录缓存或内存 index 查找 → 分配句柄。unified open 直接委托 raw open，没有预读文件内容。写打开额外检查只读状态和匹配规则。普通只读打开本身无需 `READ(6)` 文件数据；首次 `read` 才经 extent 定位与读取路径。失效卷的 revalidation、LE 带库自动装载、缓存未命中，可能让安装版 open 触发额外动作，不能据参考代码排除。

源码还明确区分 FUSE 缓存开关：Linux FUSE >2.7 的 open 设 `fi->direct_io=0, keep_cache=1`；旧 FUSE 写打开设 direct_io。这里是 **FUSE page cache 行为**，与 SG_IO 的 `SG_FLAG_DIRECT_IO` 不是同一个开关。不要拿重复读取命中内核缓存的速度代表磁带速度。

来源：[open/release](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L359)、[raw open](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops_raw.c#L62)、[unified open](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/iosched/unified.c#L429)。

## 装载、挂载、卸载必须拆开

参考单驱动库的 `ltfs_mount` 不包含机械手 MOVE MEDIUM；LE 带库管理层先从槽位移动磁带，才进入 drive load 与 LTFS mount。以下是参考库调用，不是安装版 LE 的机械管理实现。

- **Drive load**：`tape_load_tape` → backend load → 等设备 ready → PREVENT MEDIUM REMOVAL → READ POSITION → 设置变长块等默认参数 → 清密钥（按设备/配置）→ 查询容量、驱动参数和提前预警。实际可见 `LOAD/UNLOAD(1B)`、TUR、PREVENT/ALLOW(1E)、READ POSITION(34)、MODE/LOG/安全类命令，具体后端可能省略或追加。
- **LTFS mount**：`ltfs_mount` → `ltfs_start_mount` → 上述 load → seek 分区0开头 → 确认双分区容量 → 读两分区 label → 设置压缩 → 检查最大块长 → 读 MAM 锁/WORM/加密状态 → 检查 EOD → 读 IP/DP coherency、VCR → 按一致性条件定位/读最新 index 或恢复扫描 → 建立目录及 append 状态。正常挂载和恢复挂载的 LOCATE/READ 数量不同。
- **LTFS unmount**：`ltfs_fuse_umount` → 停定时线程 → 全卷 flush → 销毁调度器 → `ltfs_unmount`。正常可写卷在 dirty 或最新 self pointer 不在 IP 时写 IP Full index；已干净且最新 self pointer 在 IP 时跳过。随后更新磁带健康信息。**逻辑 unmount 不必等于物理 eject**。
- **Drive unload**：`tape_unload_tape(keep_on_drive)` → ALLOW MEDIUM REMOVAL → REWIND → 若非 keep_on_drive 执行 backend unload → 关闭 append-only 模式。LE 返回槽位还需要管理层 MOVE MEDIUM 和库存确认；这些不在开源单驱动 `ltfs_unmount` 内。

来源：[load/unload](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/tape.c#L371)、[start mount](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L1403)、[mount](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L1536)、[unmount](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L2063)、[FUSE destroy](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L1151)。

## 安装版待现场核对的最小矩阵

1. 同尺寸不同文件：write→fsync；write→fdatasync；write→POSIX sync；write→LE 管理 sync，分别标明 syscall 返回时间、CDB 序列和后台尾随命令。
2. 每项再重复一次无新写入的 clean sync，以排除前一次已完成的工作；记录 index generation 是否变化。
3. 已装载已挂载时 open-only / open+first read / close；归槽后 cold open-only 与 cold first-read，以确定自动装载触发点。
4. 管理 load 和 unload 分段为机械 MOVE、drive LOAD/UNLOAD、LTFS mount/index checkpoint；记录配置 `sync_type=unmount`，避免 close-sync 或定时线程串入。
5. SG direct-I/O 开关由主任务研究；不擅自重新开启此前已关闭的 allow_dio。
