# EE / LE 的 FUSE 缓存现场核对（2026-09-24）

用户要求先做 EE 调研，暂停 tape-fuse 修改。本轮未修改 Rust 代码，未重启服务、读写磁带文件或触发迁移/召回；仅查询挂载、进程、包信息、帮助、参考源码和已安装程序的静态反汇编。

## 实际访问路径

在 ee1（10.131.9.71）查询 `findmnt -t gpfs,fuse,fuse.ltfs`：

| 挂载点 | source | fstype |
| --- | --- | --- |
| /gpfs | gpfsfs | gpfs |
| /ltfs | ltfs:IBMlisa41D6C_LL3 | fuse |

进程为 `/opt/ibm/ltfsle/bin/ltfs /ltfs -o changer_serial=IBMlisa41D6C_LL3 -o work_directory=/gpfs/.ltfsee/meta -o admin_port=7600 -o umask=177 -o noallow_other`；EE 的 ltfsee_md 指向同一 /ltfs。启动参数没有 direct_io；仅凭 mount 输出不能确定逐 OPEN 的缓存标志。

现场版本：ltfs-mig 1.3.6.5-53251、ltfsle 2.4.8.3-10521、fuse-libs 2.9.9-15.el9、GPFS 5.2.3-6。`mmlsconfig pagepool` 返回 4G。GPFS pagepool 是另一层配置，不能与 LE 的 FUSE 页缓存或 LTFS 用户态缓存混称。

IBM 官方说明 EE 管理 GPFS 数据向 LTFS 的迁移及其元数据关联，所以通常业务入口是 GPFS，而不是直接打开 EE 内部 /ltfs。[IBM operational storage](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=edition-operational-storage)

## 已安装 LE 的缓存标志（静态证据）

不是只参考开源实现：实际检查了 `/opt/ibm/ltfsle/bin/ltfs` 的导出函数。文件 SHA256：`9c55eeb8a26ecb386699c61873f8163c3fcae820ef22313ae87a700f4e679cc8`。

命令：

```bash
objdump -d --disassemble=ltfs_fuse_open /opt/ibm/ltfsle/bin/ltfs
objdump -d --disassemble=ltfs_fuse_create /opt/ibm/ltfsle/bin/ltfs
ldd /opt/ibm/ltfsle/bin/ltfs
```

成功路径中，fi 指针第二参数保存后重新读入 rax；对 `fi + 0x14` 的字节作如下修改：

| 回调 | 清 bit 0 | 置 bit 1 |
| --- | --- | --- |
| ltfs_fuse_open | 0x111a8: AND 0xfffffffe | 0x111b6: OR 0x2 |
| ltfs_fuse_create | 0x12009: AND 0xfffffffe | 0x12017: OR 0x2 |

按 x86_64 libfuse 2 的 `fuse_file_info` 布局，这是 **direct_io=0、keep_cache=1**；也已核对 tsclient 安装的 `/usr/include/fuse/fuse_common.h`。现场程序链接 `/lib64/libfuse.so.2`。[libfuse 2.9.9 结构定义](https://github.com/libfuse/libfuse/blob/fuse-2.9.9/include/fuse_common.h)

结论边界：这确认了现场已安装 LE 的 open/create 回调选择页缓存并保留缓存，不是仅凭 FUSE 名称推断。尚未抓取运行进程实际 OPEN 响应；通用 FUSE 配置可能进一步影响最终响应，不能把静态证据表述成逐请求追踪结果。也未实测 EE/LE mmap、脏页回写或跨节点缓存一致性。

## 参考源码与帮助交叉核对

tsclient（10.131.9.73）参考源码 `/home/rocky/ltfs-ref/src/ltfs_fuse.c`：open 约 397–414 行、create 约 701–717 行，在 Linux FUSE_VERSION > 27 分支明确设置 direct_io=0、keep_cache=1；旧 FUSE 分支为写打开/创建设置 direct_io。源码目录无 .git，不能报告其固定提交 ID；现场 LE 二进制检查提供独立证据。

现场 `ltfs -a` 列出 direct_io、kernel_cache、auto_cache、entry_timeout、attr_timeout，也列出 dcache_backend、min_pool_size/max_pool_size。普通帮助的 library_cache 明确针对 MAM/index/label。它们分别涉及 FUSE 数据缓存、元数据缓存、用户态缓存等，不能把 library_cache 当作文件数据页缓存开关。

`keep_cache=1` 表示打开时不必丢弃已有文件数据缓存；它不等同启用 FUSE_WRITEBACK_CACHE，也不等于内容已落带。[libfuse 字段定义](https://github.com/libfuse/libfuse/blob/fuse-2.9.9/include/fuse_common.h) Linux FUSE cached 模式支持 mmap，write-through 是默认写入模式，writeback-cache 需要另行协商。[Linux FUSE I/O](https://docs.kernel.org/filesystems/fuse/fuse-io.html)

## 对 tape-fuse 的启示（尚未实施）

使用内核页缓存符合现场 LE 的方向，但不能只照搬两个标志。当前 tape-fuse 的旧读句柄保留整文件快照；同路径不同版本如果共用 inode 页缓存，需要先定义覆盖、截断、删除重建及远端修改后的可见性和失效机制。是否保留现有快照语义，不能从 LE 的缓存标志推导。若后续实现，须测试 mmap 与旧句柄并存、跨客户端更新及 fsync 落带保证。

本轮没有 EE mmap 成功结论，也没有改造 tape-fuse。官方资料补充见 [资料调研](ee-fuse-cache-sources.md)。

## 后续：现场 LE mmap 验证（用户明确要求，2026-09-24）

本节是后续有状态现场验证，区别于前面的静态调研。没有改 tape-fuse 或 LE 代码。

目标：ee1 /ltfs（LE 2.4.8.3、FUSE 2.9.9、内核 5.14.0-427.20.1.el9_4.x86_64），LISA4300 的 EE8005L8（注意不是 EE8005L08），EETEST 池。EE 起初无活动任务、驱动器未挂载；GPFS `/gpfs/tapers-del/f3` 为 premigrated，4 MiB。

经 LE assign、move、mount 管理路径接入已有卷，使用驱动 IBMlisa41D6C（ee1 /dev/sg5）。实际测试路径：

`/ltfs/EE8005L8/.LTFSEE_DATA/3943857032761222777-8982614630178882119-801097431-37526-0`

以 O_RDONLY 打开，Python 显式指定映射参数：

```python
shared = mmap.mmap(fd, 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ)
private = mmap.mmap(fd, 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ)
```

| 检查 | 结果 |
| --- | --- |
| 只读共享 MAP_SHARED | 通过，无 ENODEV |
| 只读私有 MAP_PRIVATE | 通过 |
| 映射与 pread 比较 | 全部 4194304 字节按 64 KiB 分块逐字节一致 |
| 关闭原 fd 后继续读 shared | 通过 |
| 再次打开普通读取 | 内容一致 |
| 文件 size/mtime | 验证前后不变 |
| GPFS premigrated 副本 | SHA256 与 LE 映射结果一致 |

共同 SHA256：`cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`。测试程序段耗时 0.113 秒，**不是冷缓存或磁带吞吐量测试**；没有清空页缓存，也未证明每次读取都访问磁带。未测试共享可写 mmap、并发覆盖/截断、跨节点缓存一致性或崩溃持久性。

### 收尾中的缓存副作用与恢复

LE unmount 将介质卸回原槽 1030；随后执行 LE unassign 会删除卷缓存。这导致 EE `file state` 临时显示 premigrated (check tape state)、file state=missing。没有将这个状态当作正常收尾，也未直接修改 EE 数据库。

使用 EE 自身的 `eeadm tape validate EE8005L8 -p EETEST -l LISA4300` 恢复管理状态；任务 1114 成功，重新写入 GPFS 中的 index cache。再次检查：f3 恢复正常 premigrated、池 EETEST/appendable 不变、介质在原槽、所有驱动 not_mounted、无活动任务，GPFS 文件哈希匹配。**最终 LE assignment 为 ASSIGNED**（由 EE validate 建立），与开始的 NEED_ASSIGN 不完全相同；保留这个由 EE 建立的缓存状态，不再手动 unassign。

LE 日志显示卸载时 volume not dirty，跳过写带索引；本次测试程序没有写文件内容，但确实执行了虚拟介质移动、装卸、LE 资源及缓存变更。现场还记录到 LOGSENSE Invalid Field in CDB 和 TapeAlert page 0x17 解析告警；这是独立的 Holo/LE 协议兼容问题，未在本次验证中修复，不据此宣称物理带库行为已验证。

结论：**在同一系列 Rocky 5.14 内核上，现场 LE 的页缓存路径支持只读共享 mmap；此前 tape-fuse direct I/O 路径的 ENODEV 不是该内核完全不支持 FUSE mmap。**

## 后续：LE 文件可见性实测（2026-09-24）

用户要求根据 tape-fuse 建议继续调研 LE 行为。本次在同一 ee1 /ltfs、EE8005L8 上使用独立临时目录 `codex-le-visibility-7f8f1b9f-8b9b-473c-aaea-d4b73f205083`。测试前 EE 无活动任务、驱动空闲，LE 卷保持上次 EE validate 建立的 ASSIGNED 状态。经 `leadm tape mount -d IBMlisa41D6C EE8005L8` 挂载。

此次**确实写入了少量测试数据并更新索引**，不是只读试验。只操作临时目录，不写 `.LTFSEE_DATA` 或 GPFS 原文件。测试写入采用 os.write/os.pwrite/os.open，避免 Python 文件缓冲影响判断；mmap 显式指定只读 MAP_SHARED 和 MAP_PRIVATE。

| 操作 | 现场结果 |
| --- | --- |
| 新建并写入 8192 字节 A，写 fd 尚未 fsync/close，另开读 fd | 立即读到 A，大小 8192 |
| 保留读 fd、共享及私有只读映射；pwrite 同长度 B | 原读 fd、新读 fd、原共享映射、原只读私有映射均读到 B |
| 关闭写 fd，再 O_WRONLY|O_TRUNC 打开同一路径 | 原读 fd 的 fstat 大小变 0，pread 返回 EOF |
| 通过截断后的写 fd 写 4096 字节 T | 原读 fd 读到 T，原共享映射的前 4096 字节读到 T |
| fsync、关闭写 fd，unlink 路径 | 路径不存在；原读 fd 和原共享映射仍能读到 T |
| 同名 O_CREAT|O_EXCL 新建，写 8192 字节 C | 新 inode=8，旧 inode=7；新读 fd 读 C，旧读 fd/映射仍读 T |

只读 MAP_PRIVATE 在本次覆盖后也看见更新，**不能作为不可变快照保证**。没有测试发生私有写入后的 COW 页面。截断后只读映射的有效前缀，没有访问新 EOF 之后的页，也没有把未测试的 SIGBUS 行为写成现场结果。

### 对先前方案的判断

现场 LE 的同挂载行为支持以下选择：同一 inode 的打开句柄共享内容状态；普通写入在 fsync/close 之前可见；O_TRUNC 改变已有读句柄看到的文件；unlink 保留仍被引用的旧文件，同名重建分配新 inode。**它不提供 tape-fuse 目前“每个读句柄保留打开时内容”的覆盖快照语义。**

这支持 tape-fuse 若对齐 LE，应把本地可见性与归档提交分开，并将缓存从每句柄独立文件改为共享文件对象。该结论只涉及已测的同挂载行为；没有验证多个 LE 节点并发打开同一卷、HTTP/其他客户端替换后的缓存失效、共享可写 mmap、写入故障或断电。

### fsync/close：已有证据与边界

现场 LE 帮助提供 `sync_type`，默认说明为 time@5，另有 close/unmount。不能从“读得到”推断磁带索引已提交。

同一个已安装 LE 二进制的静态反汇编表明：ltfs_fuse_fsync 和 ltfs_fuse_flush 都调用 helper 0x1182e；helper 在 dirty 标记为真时，library 模式调用 changer_fsops_flush，single-drive 模式调用 ltfs_fsops_flush，成功后清理 dirty 标记。这里未继续证明闭源 changer_fsops_flush 的全部持久性边界，因此不声称现场 LE 的 fsync 与 tape-rs 卷提交完全等价。官方源码补充见 ee-fuse-cache-sources.md。

### 收尾

程序关闭所有映射和句柄，删除测试文件并移除临时目录，然后通过 LE 正常 unmount。没有再次 unassign，避免删除 EE 卷缓存。磁带返回原槽 1030，LE 为 ASSIGNED/UNMOUNTED/WRITABLE；EE 所有驱动 not_mounted、无活动任务；f3 为正常 premigrated。原有 4 MiB 数据 SHA256 仍为 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`。

LE 报告 total_blocks 从 65 增至 73、valid_blocks 仍为 8：临时文件删除不意味着物理空间立即回收。本轮未进行 reclaim 或格式化。未修改 tape-fuse 或其他 Rust 代码。

补充产品文档证据：IBM LE 的 [Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=tips-sync-operation) 明确说明 Linux 的 sync/fdatasync/fsync 不自动触发 LE 的索引同步操作；索引同步由 sync_type 配置、卸载、ltfs.sync 属性或 leadm tape sync 等触发。官方默认说明为 time@5，不能由此推定现场 EE 的全部有效配置。因而“tape-fuse fsync 等待归档”是自研服务明确提供的更强契约，不应以照搬 LE 为由撤销。
