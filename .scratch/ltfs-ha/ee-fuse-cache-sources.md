# EE、LE、FUSE 缓存与 mmap：一手资料调查

调查日期：2026-09-24。范围仅为官方资料、上游源码与设计结论；本笔记不代表完成了 EE/LE 的 mmap 实测。未修改 tape-fuse。现场证据另见 [ee-fuse-cache-lab.md](ee-fuse-cache-lab.md)。

## 结论与证据边界

| 问题 | 结论 | 证据范围 |
| --- | --- | --- |
| EE 的业务应用是否直接访问 FUSE？ | 标准 EE 使用方式是应用访问 Storage Scale/GPFS；LE 的 LTFS 挂载属于磁带数据访问层。不能把两者混为一谈。 | IBM EE 产品架构与 transparent recall 文档 |
| EE 是否只靠内存缓存屏蔽磁带？ | 不是。透明召回先把整文件写回 GPFS，随后由 GPFS 服务请求；这是磁盘驻留层。 | IBM 明确文档 |
| GPFS 有缓存吗？ | 有专用 pagepool，缓存用户数据和元数据；不能直接称为 FUSE 的 Linux 页缓存。 | IBM Storage Scale 文档 |
| LTFS FUSE 可否使用内核页缓存？ | 可以。公开 LTFS 参考实现的 Linux、新于 FUSE 2.7 的 open/create 分支设 `direct_io=0`、`keep_cache=1`。 | 上游源码；并非闭源 LE 全版本承诺 |
| 已安装 LE 是否也这样？ | 现场二进制静态检查与公开实现一致，详见现场笔记；实际 OPEN 响应和 mmap 行为仍需运行验证。 | 主任务现场取证，不是本研究远程执行 |
| 开启页缓存是否必须开启 writeback-cache？ | 不必。cached read 与 writeback-cache 是不同选择；write-through 是默认写模式。 | Linux FUSE 官方文档 |

## 1. EE 的应用入口与召回

IBM 将 EE 定义为与 Storage Scale 集成的磁带存储层；应用通过统一文件系统名称空间访问文件。[IBM EE 安装配置指南，章节 1.1](https://www.redbooks.ibm.com/redbooks/pdfs/sg248333.pdf)

EE 1.3.6 的 transparent recall 文档明确：已迁移文件被 GPFS 引用后，读写请求触发整文件从磁带召回到 GPFS，之后由 GPFS 提供文件访问。因此对普通 EE 应用讨论 mmap，首先应调查 GPFS 的 mmap 路径，而不是仅检查 `/ltfs` 的 FUSE 标志。[IBM EE 1.3.6 Transparent recall](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=recall-transparent)

文件状态分为 resident（仅磁盘）、premigrated（磁盘与磁带都有）、migrated（数据仅在磁带）。常规召回使 migrated 变为 premigrated；这个磁盘层与 RAM 缓存是两回事。[IBM Archiving and retrieving files](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=managing-archiving-retrieving-files-from-tape)

GPFS pagepool 缓存用户数据和文件系统元数据，用于异步读写。Storage Scale 5.2.3 安装指南将它描述为 GPFS daemon 与 kernel 使用的共享内存的一部分。因此“EE 有缓存”成立，但“EE 就是一个启用 FUSE kernel_cache 的应用文件系统”不成立。[IBM pagepool 说明](https://www.ibm.com/docs/en/linux-on-systems?topic=usage-setting-pagepool-size)、[Storage Scale 5.2.3 Concepts, Planning, and Installation Guide](https://www.ibm.com/docs/en/STXKQY_5.2.3/pdf/scale_ins.pdf)

IBM 的 `mmcachectl show` 可观察本节点 pagepool 中的文件及数据缓存情况。这可作为后续 GPFS 层调查工具，不能替代 FUSE OPEN 标志观察。[IBM mmcachectl](https://www.ibm.com/docs/en/storage-scale/5.2.3?topic=reference-mmcachectl-command)

## 2. LTFS 与 libfuse 的公开源码

公开仓库自称独立磁带驱动器的 LTFS 格式参考实现，不能将其自动等同于现场 IBM LE 2.4.8.3 的源码。[LTFS 仓库](https://github.com/LinearTapeFileSystem/ltfs)

调查时 `master` 的 `src/ltfs_fuse.c` 中，`ltfs_fuse_open` 和 `ltfs_fuse_create` 均按平台与编译期 `FUSE_VERSION` 分支：

- Linux、`FUSE_VERSION > 27`：`direct_io=0`、`keep_cache=1`。
- 旧 FUSE 分支：写打开/create 使用 direct I/O，`keep_cache=0`。
- macOS 有独立分支，不能用 Linux 结论替代。

这是源代码结论，`master` 会变化；本次 `git ls-remote` 确认 HEAD 为 `4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb`，复核时应读取同名函数，而非依赖网页展示行号。[LTFS ltfs_fuse.c 固定提交](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c)

libfuse 2.9.9 的 `fuse_file_info` 定义明确：`direct_io` 决定该打开文件使用 direct I/O；`keep_cache` 表示打开时已有数据缓存不必失效。后者不是“保证永久保留”或“预先把整文件装入 RAM”。[libfuse 2.9.9 fuse_common.h](https://github.com/libfuse/libfuse/blob/fuse-2.9.9/include/fuse_common.h)

**必须检查回调之后的覆盖逻辑。** libfuse 2.9.9 的 `fuse_lib_open`/`fuse_lib_create` 在文件系统回调成功后，还会根据全局 `direct_io` 配置将 `fi->direct_io` 设为 1，根据 `kernel_cache` 将 `keep_cache` 设为 1。因此静态回调结果需要结合实际参数/配置或 OPEN 响应判断。[libfuse 2.9.9 fuse.c](https://github.com/libfuse/libfuse/blob/fuse-2.9.9/lib/fuse.c)

IBM SDE/LE 红皮书在 SDE `ltfs` 命令帮助中列出 `-o direct_io`、`-o kernel_cache`、`-o [no]auto_cache`。这证明相关配置接口存在，不能据此推出现场 LE 默认值或已启用的参数。[IBM SDE/LE 安装配置指南，印刷页 96](https://www.redbooks.ibm.com/redbooks/pdfs/sg248090.pdf)

## 3. 对 mmap 的直接含义

Linux 官方 FUSE 文档区分 direct I/O 和 cached I/O。cached 读可以命中页缓存并预读，支持 mmap；cached 写又分 write-through 和 writeback-cache，前者是默认。因此为只读 mmap 调研缓存方案，不必同时改变写确认时机。[Linux FUSE I/O Modes](https://docs.kernel.org/filesystems/fuse/fuse-io.html)

上游 Linux 5.14 的 `fuse_file_mmap` 对 `FOPEN_DIRECT_IO` 且 `VM_MAYSHARE` 返回 `ENODEV`；非 direct I/O 分支设置正常 FUSE VM 操作。这是旧内核下缓存模式与共享 mmap 的直接机制依据。Rocky 厂商内核可能含回移补丁，最终仍以目标内核和现场实测为准。[Linux v5.14 fs/fuse/file.c](https://github.com/torvalds/linux/blob/v5.14/fs/fuse/file.c)

`keep_cache=1` 不等同于开启 `FUSE_WRITEBACK_CACHE`，更不代表 fsync 已持久化到磁带。后续如修改 tape-fuse，必须分别定义读缓存、共享可写 mmap、写提交/错误传播，以及覆盖和旧句柄的缓存一致性。本轮不实施这些修改。

## 4. 不应混淆的其他缓存

LE 的 index cache 保存 MAM、标签、索引与容量信息；默认 work directory 下的 volume_cache 是磁带元数据缓存，不是文件内容页缓存。[IBM LE Index cache](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=management-index-cache)

LE 的 on-disk metadata storage/dcache 面向大量文件索引的内存压力，也不能当作文件数据缓存或 mmap 支持的证据。[IBM LE Metadata storage](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=management-metadata-storage)

## 5. 尚未证实与建议验证边界

- 没有从 IBM LE 产品文档找到针对现场版本的完整 mmap 兼容性承诺，或明示默认 OPEN 标志的说明。
- GPFS 支持 mmap 有官方实现与修复说明支撑，但这不等于已验证当前 EE 管理的 migrated 文件从 mmap 首次访问到透明召回的完整行为。[IBM Storage Scale 5.2.2 mmap writeback 改进](https://www.ibm.com/docs/en/STXKQY_5.2.2/pdf/scale_adm.pdf)
- 后续应分别验证 `/gpfs` 与 LE `/ltfs`：只读 `MAP_PRIVATE`、只读 `MAP_SHARED`、重复读取、覆盖后缓存一致性；共享可写映射另设范围。
- mmap 建立成功不等于读取成功：还要实际访问映射页并验证内容。对 migrated 文件，读页可能触发召回及磁带装载，应在明确测试介质/文件后执行，不能把它当作仅查看配置。
- 不凭本次资料决定将 tape-fuse 全局设为 `keep_cache=1`。现有每打开句柄快照与 inode 共享页缓存如何共存，需要独立设计。

## 6. 进一步调查：句柄、覆盖、unlink 与同步语义

本节继续使用固定提交 `4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb` 的官方公开参考实现。它解释实现机制；现场闭源 LE 的具体行为以 [现场实验记录](ee-fuse-cache-lab.md) 为准，两类证据不能互相替代。

### 打开句柄引用活对象，不是每次打开的内容快照

`ltfs_fsraw_open` 查找已有 dentry 后直接返回其指针；`_file_open` 以 dentry 指针为键共享 `file_info`，每次打开仅另建 `ltfs_file_handle`。`ltfs_fuse_read` 通过保存的 dentry 读取，不是在每次读取时重新查当前路径。源码没有在 open 时复制整文件或 extent 快照。[ltfs_fsops_raw.c：ltfs_fsraw_open](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops_raw.c#L62)、[ltfs_fuse.c：_file_open / ltfs_fuse_read](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L171)

`ltfs_fsops_truncate_path` 先找到该 dentry，随后调用 truncate；raw truncate 原地移除/缩短 extent，更新同一对象的 size/realsize 和修改时间。由此推导：同一文件原地覆盖或 truncate 后，旧打开句柄没有“保留打开时内容”的保证；应把它与 unlink 后重新创建同名文件区分。[ltfs_fsops.c：ltfs_fsops_truncate_path](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c)、[ltfs_fsops_raw.c：ltfs_fsraw_truncate](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops_raw.c#L757)

unlink 从父目录移除名字，将对象标为 deleted，释放目录持有的引用；`fs_release_dentry_unlocked` 仅在引用数降至 0 且不处于 out_of_sync 时销毁对象。因此对象生命期可长于路径生命期，已打开句柄的保留不要求采用打开时快照。这个机制与现场观察到“unlink 后旧句柄可读；同名重建得到新 inode”相容。libfuse 高层还可能使用隐藏文件处理打开中的 unlink，不能仅据 raw unlink 函数断言现场具体走哪条路径。[ltfs_fsops.c：ltfs_fsops_unlink](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c)、[fs.c：fs_release_dentry_unlocked](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/fs.c#L863)

### fsync/flush 与 LTFS 索引同步不是同一个操作

参考实现的 `ltfs_fuse_fsync` 和 `ltfs_fuse_flush` 调用同一 helper：仅句柄 dirty 时调用 `ltfs_fsops_flush`；`isdatasync` 没有选择额外的索引写入。`fsyncdir` 直接成功返回。该路径不直接调用 `ltfs_sync_index`。[ltfs_fuse.c：fsyncdir / _ltfs_fuse_do_flush / fsync / flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L532)

`ltfs_fsops_flush` 委派已启用的 I/O scheduler，并刷新已启用的 dcache。unified scheduler 将排队数据经 `ltfs_fsraw_write` 写入数据分区，传播记录的写错误；其文件 flush 本身不发布常规新索引。FCFS scheduler 没有同样的缓冲队列，flush 直接返回。因此不能把通用名字 flush 解释为无条件驱动器缓存清空、索引提交或掉电安全证明。[ltfs_fsops.c：ltfs_fsops_flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c)、[unified.c：unified_flush / _unified_flush_unlocked](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/iosched/unified.c#L898)、[fcfs.c：fcfs_flush](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/iosched/fcfs.c#L190)

这是 IBM LE 产品文档也明确说明的区别：Linux 的 `sync`、`fdatasync`、`fsync` 不自动触发 LE 的 LTFS sync operation；后者写入索引。默认策略为 `sync_type=time@5`，可选择 close 或 unmount；另有 `ltfs.sync` 虚拟扩展属性和 `leadm tape sync <tape_ID>` 显式途径。未写新索引时，即使文件数据已写入磁带，异常关闭后也可能无法经正常索引找到文件。此处只列出语义，未执行这些有状态操作。[IBM LE Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=tips-sync-operation)

参考实现的 release 在 `sync_type=close` 且该文件有待同步写入时调用 `ltfs_sync_index`；定时线程也独立调用它；该函数在索引 dirty 时调用 `ltfs_write_index`。`main.c` 解析 time/close/unmount 策略。这些源码路径与上述产品文档相符，但不能据参考实现推断现场闭源 LE 的所有异常处理细节。[ltfs_fuse.c：release](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L427)、[periodic_sync.c](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/periodic_sync.c)、[ltfs.c：ltfs_sync_index](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L3647)、[main.c：同步选项解析](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/main.c#L477)

对后续 tape-fuse 方案的意义：如果目标是接近 LE 常规文件语义，应区分“同 inode 的原地修改让旧句柄看见新内容”和“unlink/recreate 后旧句柄继续持有旧对象”，而非普遍为每次 open 固定内容版本。至于 fsync，现有 tape-rs 如承诺更强的提交语义，不能为了模仿 LE 页缓存而无意削弱；这需单独制定契约。本轮只有调查与记录，没有实施代码变更。
