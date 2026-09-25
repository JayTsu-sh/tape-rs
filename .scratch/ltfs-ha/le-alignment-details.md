# LE FUSE 剩余行为对齐调查

日期：2026-09-24。目标：为用户要求的 tape-fuse 对齐提供可执行边界。仅查阅官方资料和已有参考源码；本轮 SSH 仅读取 `.73:/home/rocky/ltfs-ref/src`，未挂载、访问设备、装卸或写入现场。

## 证据分级

- **LE 产品承诺**：IBM LE 官方文档；2.4.8 sync 页面可搜索读取，supported calls 搜索实际返回 2.4.6。不能把后者伪称为针对 2.4.8.3 的逐项实验。
- **现场已验证**：已有 [ee-fuse-cache-lab.md](ee-fuse-cache-lab.md) 的 LE 2.4.8.3 实验/二进制证据。本轮没有新增现场行为实验。
- **参考实现**：LinearTapeFileSystem 官方公开实现，不等于 IBM 闭源 LE 源码。现场参考目录没有 `.git`，下面行号来自读取的快照；可与先前调查记录的固定提交 `4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb` 对照。

## 实现目标

| 项目 | 证据与目标 | 验证边界 |
| --- | --- | --- |
| O_RDWR、非截断写打开 | 参考 `ltfs_fuse_open` 明确接受 O_RDWR 与 O_WRONLY，不要求 O_TRUNC | 单测 + 真 FUSE 验证读写权限、保留原有内容 |
| 随机覆盖、追加、稀疏写 | LE 文档列出 pwrite/lseek；参考 write 按传入 offset 执行，没有顺序写限制 | 应支持任意非负偏移；O_APPEND 每次写采用当前 EOF，多个句柄串行决定偏移 |
| 任意 truncate/ftruncate | 文档支持；参考 truncate 更新任意非负长度，缩小时裁 extent，扩大部分由 read 补零 | 缩小、扩大、洞、旧句柄/映射可见性；路径 truncate 也需工作 |
| 多个 writer | 参考 `_file_open` 共享 dentry/file_info 并增加 open_count，没有单 writer 排斥 | 共享修改状态，不能每句柄保留独立整文件副本互相覆盖；现场并发写时序尚未实测 |
| 只读 mmap | LE 官方支持范围为 PROT_READ；已有现场成功实验 | 保留只读 MAP_SHARED/MAP_PRIVATE 验证 |
| 共享可写 mmap | **不能列作 LE 必须补齐功能**：官方文档限制 PROT_READ | 不把内核偶然允许建立映射说成 LE 产品承诺；本轮未验证 LE 可写映射 |
| rename | 文档支持；参考实现直接移动原 dentry，支持目录，覆盖目标保留已有引用 | 源 inode 身份不变，目标旧引用保留；不能把复制+删除称为等价 rename |
| KEEP_CACHE | 现场二进制和参考实现均为 Linux 新 FUSE direct_io=0 / keep_cache=1 | 必须同时设计版本改变后的缓存失效，不能只改 OPEN 标志 |
| fsync/flush | LE 数据刷新与索引提交分离 | 不应要求每次 fsync 必然提交新索引；但 staging 成功也不等价数据已落带 |
| ltfs.sync | 卷根虚拟属性，写任意值触发整卷同步 | 需要覆盖本卷全部待提交修改、传播失败，不只同步某个 fd |

上述系统调用范围来自 [IBM Supported system calls](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=system-supported-calls)。其中 mmap 原文限制为 “Only the PROT_READ protection flag is supported.”

## 写入与句柄机制

参考 `src/ltfs_fuse.c:163–191` 的 `_file_open` 按 dentry 指针共享对象，已有对象只增加 open_count。`open:356–419` 接受 O_RDWR，`write:898–931` 将 offset 原样传入 fsops。修改与上传状态应归属 inode，共享文件锁负责多个句柄写入顺序；句柄只保存访问模式和打开标志。这样某个 fd 的 flush 不会上传另一个 fd 修改前的旧快照。[固定提交 ltfs_fuse.c](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c)

`ltfs_fsops_write:1750` 只对 immutable/appendonly 文件施加额外限制，正常文件没有顺序偏移限制；`ltfs_fsops_truncate:1809` 拒绝负长度和目录。raw truncate `757` 设置新的 size；raw read `614–624,721–726` 为洞和稀疏尾部填零。追加位置通常由 VFS/FUSE 的 cached write 路径处理，参考 write 回调自身没有 O_APPEND 分支，不能据它断言多机并发追加原子性。[fsops](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c)、[raw fsops](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops_raw.c)

## rename 与持久化

参考 `ltfs_fsops_rename:546–882` 使用 rename_lock 和父目录锁，检查循环移动；目标是非空目录时报错。覆盖时把旧目标标 deleted，移动源 dentry 的父指针和名字，而非复制内容；源 UID 不变。Linux 允许目录移动，禁止跨父目录移动的分支仅在 `__APPLE__` 下。高层 VFS 同时承担文件/目录类型检查。[rename 实现](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_fsops.c#L546)

对 tape-rs：服务端需提供一个可恢复的命名空间变更事务（源移除、目标替换、目录子树重定位），本地 inode 映射与打开句柄随同更新。客户端两次 HTTP 调用的 copy/delete 在失败、并发和观察窗口上不等价。空目录也需在服务端持久化，否则“目录 rename 成功”只存在于当前挂载。LE 是每盒磁带的卷命名空间；未取得 LE 跨卷 rename 行为证据，不能从单卷源码推断跨带移动一定成功。

## 页缓存与外部更新

`keep_cache=1` 表示 open 时不自动丢弃已有页，并非永不失效或分布式一致性机制。[libfuse 2.9.9 结构定义](https://github.com/libfuse/libfuse/blob/fuse-2.9.9/include/fuse_common.h)

LE 的同一挂载由同一 dentry/内核 inode 表达修改；tape-fuse 还允许独立 HTTP 客户端替换后端文件，存在额外版本来源。对齐本地行为不意味着 LE 已承诺这种外部更新实时可见。若启用 KEEP_CACHE，可在确认后端版本变化且旧引用已释放后使 inode 页缓存失效，或为远端新对象分配新 inode；失效完成前不得让旧缓存与新下载内容混合。活跃映射遇到远端替换的处理属于 tape-fuse 自有协议，应保留明确契约和回归测试。没有现场外部更新实验可以证明需要删除 ESTALE 保护。

cached write-through 与 WRITEBACK_CACHE 是不同模式，支持页缓存和只读 mmap 不要求启用后者。[Linux FUSE I/O modes](https://docs.kernel.org/filesystems/fuse/fuse-io.html)

## fsync 与显式同步

IBM 明确区分 Linux fsync/fdatasync/sync 和 LTFS sync；默认 time@5，每次关闭同步是可选策略。LTFS sync 会写索引，卸载时还更新索引分区。显式方式有 ltfs.sync 和 leadm tape sync。[IBM LE 2.4.8 Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.8?topic=tips-sync-operation)

参考 `_ltfs_fuse_do_flush:533–558` 仅 dirty 时调用 fsops_flush；fsync/flush 共用它。索引同步不是此回调直接完成。`xattr.c:901–902` 在卷根接受 ltfs.sync 并调用 ltfs_sync_index；这里不检查属性值内容。[FUSE 回调](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/ltfs_fuse.c#L533)、[xattr 实现](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/xattr.c#L901)

IBM 把 ltfs.sync 定义为根目录 write-only 虚拟属性；虚拟属性不通过 listxattr 列出。Linux 用户工具可能显示或使用 `user.ltfs.sync` 名称，适配层应核对其前缀处理。不要为了复刻参考源码某些 read 分支的副作用，把 GET 属性实现成隐式提交。[IBM virtual extended attributes](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=attributes-virtual-extended)、[IBM 红皮书 p.186](https://www.redbooks.ibm.com/redbooks/pdfs/sg248090.pdf)

**架构约束**：若现有服务端只有 staging 和“文件数据+索引一起提交”两种状态，改成 fsync 仅等 staging 只能达到“与索引提交解耦”，不能声称精确达到 LE 的数据 flush。精确对齐需服务端增加可恢复的数据已写入、索引待同步状态，并使显式同步覆盖整个卷；也需定义异步归档、定时同步、失败重试及卸载的关系。短期保留更强持久化保证是明确差异，不能标作已完全对齐。

## 建议验收顺序

1. 纯软件 inode 共享模型：多句柄随机写、追加、truncate、权限和读回；失败上传可重试且新修改不丢。
2. 实际 FUSE：两个 writer + reader；旧 readonly mmap 对覆盖可见；扩大洞清零；flush 后继续写再次 flush；KEEP_CACHE 重开及外部版本变化。
3. 服务端 rename 事务：覆盖、目录子树、空目录、目标旧引用、故障恢复；确认后再宣称替代 EXDEV fallback。
4. 同步：记录 staging/数据写入/索引提交三个里程碑，分别验证 fsync 与 ltfs.sync；需要现场实验时另行使用已授权隔离介质并记录影响。

未完成的证据：现场 LE 多 writer 并发、O_APPEND 时序、任意扩大 truncate、rename 覆盖目录及跨卷返回值；参考实现支持不等于这些 LE 2.4.8.3 实验已经运行。
