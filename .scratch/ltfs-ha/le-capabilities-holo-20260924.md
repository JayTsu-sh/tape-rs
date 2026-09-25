# LE 声明能力的 Holo 现场摸测（2026-09-24）

用户要求：重新实际验证只读 mmap，并对 LE 声明支持的能力在 Holo 环境摸测，作为 tape-fuse 对齐依据。

## 环境与影响

- ee1 `10.131.9.71`，LE 2.4.8.3-10521，FUSE 2.9.9，Rocky 内核 5.14.0-427.20.1.el9_4。
- EE 的 `/ltfs` FUSE 挂载，Holo LISA4300 / EETEST 的 `EE8005L8`，驱动 `IBMlisa41D6C`（ee1 `/dev/sg5`）。不是 `EE8005L08`，不是 TAPERS 池。
- 开始前无 EE 活动任务，驱动空闲，介质 ASSIGNED / UNMOUNTED / WRITABLE，在槽 1030。
- 只写 `/ltfs/EE8005L8/codex-le-capabilities-20260924`；根目录仅对 `user.ltfs.sync` 执行显式同步。有虚拟介质装卸、数据和索引写入，没有物理带库操作、格式化或回收。
- [可重复探针](probes/le-capabilities.py)：prepare → permissions（补查）→ 正常卸载/挂载 → verify → extended → cleanup。不自动装卸设备。全部以 root 运行；权限结果必须按这一身份解读。

依据：[IBM Supported system calls](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=system-supported-calls)。此页面列举的是调用能力，以下为实际 API 行为摸测，不是每个调用所有参数和故障路径的兼容性认证。参考代码和同步文档见 [补充研究](le-alignment-details.md)。

## 实际结果

| 能力/API | 测试与结果 |
| --- | --- |
| mmap / munmap | 65536 字节内容，PROT_READ 的 MAP_SHARED 和 MAP_PRIVATE 均成功，与 pread 全量一致；关闭 fd 后两个映射仍可读取；解除映射成功。正常卸载再挂载后共享只读映射再次通过。 |
| open / creat / close | O_RDWR 创建/打开和 C libc creat 入口成功，正常关闭。 |
| read / write / pread / pwrite | 顺序读写及指定偏移覆盖，逐字节比较正确。 |
| readv / writev | 两缓冲区散布/聚集读写，长度和内容正确。 |
| dup / dup2 | 复制描述符后共享当前位置，一端 seek 后另一端 read 与原 fd offset 一致。 |
| lseek / llseek 功能 | 正常定位及超过 4 GiB 的 64 位偏移成功；x86_64 使用 lseek 表达 64 位定位，没有独立调用 32 位 ABI 的 `_llseek`。 |
| truncate / ftruncate | 缩小、扩大、空洞补零成功；4 GiB 以上位置写一个字节后大小正确，再缩到 16 字节。没有写入 4 GiB 实体数据。 |
| 多 writer / O_APPEND | 两个同时存活的 writer 共享修改；一个关闭后另一个继续追加。额外两个并发子进程各追加 64 条 8 字节记录，128 条全部完整、唯一。 |
| rename 文件 | 覆盖目标成功，源 inode 保留；已打开的旧目标 fd 保留旧内容；源写 fd 在 rename 后继续修改新路径；源 fd 未关闭时 unlink 成功，fd 仍可读取。 |
| rename 目录 | 跨父目录移动成功，inode 保留；覆盖空目录成功；覆盖非空目录返回 ENOTEMPTY（39），两边内容不变。重挂载后目录结构及内容正确。 |
| mkdir / rmdir / getdents 功能 | 建目录、列举（Python listdir）、删除空目录成功；空目录重挂载后仍存在。64 位 Linux 的列举包装使用 getdents64，未测试旧 ABI。 |
| chdir / fchdir / chroot | 改工作目录及通过目录 fd 恢复成功；chroot 仅在独立子进程执行，子进程内相对新根读取正确，父进程不受影响。 |
| symlink / readlink / lstat | 相对链接、悬空链接、读取链接字符串和区分链接本身状态成功；链接及目标内容重挂载后正确。 |
| stat / fstat | 路径与 fd 的 inode/大小一致。 |
| statfs / fstatfs | 直接调用 libc 两入口都成功。statvfs 报 block size 524288、blocks/free 都是 4294967295；此结果不能用于证明准确的磁带容量报告。 |
| chmod / fchmod | chmod 0400 后模式为 0400；root 仍能写。fchmod 0777 返回成功但模式为挂载约束的 0600，没有保留执行位。 |
| chown / fchown / lchown | 指定非零 uid/gid 均成功返回，但文件和链接实际仍为 0:0。符合所有权修改被忽略的挂载行为。未覆盖普通用户和不同挂载权限。 |
| setxattr / getxattr / listxattr / removexattr | 二进制值含 NUL/0xff/UTF-8，读回正确；CREATE 已存在报 EEXIST（17），REPLACE 生效，删除后 ENODATA（61）；另一个属性重挂载后保留。 |
| fdatasync / fsync / sync | 三者成功，索引代数均维持 6；这不是索引提交。没有以调用成功推断 Holo 缓冲数据已抗断电。 |
| user.ltfs.sync | 卷根显式写属性后索引代数 6→7；随后正常卸载/挂载为 8，数据和元数据验证通过。 |
| umount / umount2 | 两个子进程分别创建独立挂载命名空间，并先将挂载传播设为递归 private，然后卸载其 `/ltfs` 副本，两入口均成功。宿主 mountinfo 与可访问状态不变。没有停止 EE 或销毁其主 FUSE 会话；最终卷卸载走 LE leadm。 |

### 权限测试中的修正

初版探针假设 root 对 0400 文件写入必须失败，实际成功，因此初始日志记录一项 FAIL。后续探针改为记录真实行为，完整执行 chmod/fchmod/chown/fchown，得到上表结果。没有把初版失败删除，也不把 root 可写视作 LE 的权限实现故障；挂载为 noallow_other，本轮没有证明普通用户的行为。

### mmap 证据

测试数据为 bytes(range(256)) 重复 256 次，SHA256：

`7daca2095d0438260fa849183dfc67faa459fdf4936e1bc91eec6b281b27e4c2`

共享/私有均明确使用 PROT_READ。未进行共享可写 mmap；IBM 文档只承诺只读。这也不是冷缓存吞吐测试。

## 清理与最终状态

临时目录已删除；LE 正常卸载，保留 assignment，没有 unassign。最终槽 1030，ASSIGNED / UNMOUNTED / WRITABLE，驱动空闲，EE 无任务，原 `/gpfs/tapers-del/f3` 为正常 premigrated。

原 LE `.LTFSEE_DATA/3943857032761222777-8982614630178882119-801097431-37526-0` 与 GPFS f3 均校验为：

`cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`

total_blocks 73→102，valid_blocks 最终仍为 8。测试删除不会回收已写入的物理块；既有 TapeAlert 数值保留，未据此猜测设备故障。

原始结果：[初测](probes/results/le-20260924-prepare.jsonl)、[权限补查及卸载](probes/results/le-20260924-remount.log)、[重挂载](probes/results/le-20260924-verify.jsonl)、[并发追加及私有命名空间卸载](probes/results/le-20260924-extended.jsonl)、[清理与介质状态](probes/results/le-20260924-cleanup.log)。

## 对 tape-fuse 的实施约束

本轮本地代码开始解除顺序写/O_WRONLY限制，增加多写句柄共享上传状态、随机写、追加与任意 truncate，单测及内核挂载测试通过；尚未部署。LE 对齐工作未全部完成。

仍需要服务端及持久化模型支持：保留 inode 的原子 rename（含目录）、空目录、符号链接和属性修改；数据 flush 与索引同步分离，以及显式整卷同步。当前 fsync 的归档保证不能简单替换成“HTTP 暂存成功”。KEEP_CACHE 也需配套外部版本失效机制。LE 没有在本次测试中提供独立 HTTP 客户端更新的一致性依据，不能据此取消 tape-fuse 的远端版本冲突保护。
