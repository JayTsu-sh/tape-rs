# 文件客户端与 FUSE 挂载

`ltfsctl` 与 `tape-fuse` 都通过 ltfsd 客户端接口寻找 Leader；节点地址可以重复指定。它们不直接打开磁带设备。

```bash
cargo build --bin ltfsctl --bin tape-fuse --features fuse
ltfsctl -e 127.0.0.1:7401 put ./archive.bin /archive.bin
ltfsctl -e 127.0.0.1:7401 get /archive.bin ./download.bin
ltfsctl -e 127.0.0.1:7401 delete /archive.bin
mkdir -p /tmp/tape-mount /tmp/tape-cache
tape-fuse -e 127.0.0.1:7401 --cache-dir /tmp/tape-cache /tmp/tape-mount
```

挂载进程在前台运行，需要 Linux FUSE 与挂载权限。另一个终端执行 `fusermount3 -u /tmp/tape-mount` 卸载。默认只向挂载用户开放，属主权限由内核检查。

## 完成语义

- `ltfsctl put` 等待文件落带；`delete` 等待删除墓碑落带，不立即回收介质空间。
- FUSE `close` 仅确认上传到服务端暂存区。需要归档确认的应用必须在关闭前调用 `fsync`；`fsync` 成功才表示该内容已落带。
- 删除响应丢失会返回结果未定，不自动按路径重发，避免删除随后创建的新版本。
- 写入支持 `O_RDWR`、非截断打开、任意偏移覆盖、`O_APPEND` 和任意长度截断；多个写句柄共享内容及上传状态。非截断写首次需要完整下载旧内容。`O_EXCL` 提供只创建保护。
- 服务端读取只选择已提交文件；只有服务端在途版本且没有本地文件时返回 `EBUSY`。首次读打开下载整个已提交文件，因此可能等待磁带读取。同一 inode 的本地句柄共享内容，本挂载写入在 fsync/close 前可读；覆盖和截断对旧句柄及映射可见。删除后已有句柄继续引用旧文件，同名重建使用新 inode。
- `mkdir`/`rmdir` 等待 LTFS 索引提交；空目录跨挂载和节点接管保留。`mkdir` 要求父目录存在，`rmdir` 拒绝非空目录及在途子路径。目录流在打开时取快照。
- `rename` 支持当前写入卷内的文件和目录原子改名、同类型目标替换，以及 `RENAME_NOREPLACE`；目录替换要求目标为空。保留源 inode、UID、extent、mtime 和 xattr。跨带源/目标或不在当前写入卷的源仍返回 `EXDEV`；复制加删除回退不具备原子性。权限/时间修改及链接创建不支持；已有符号链接可通过 FUSE 的 lstat/readlink 和内核路径解析读取，读取链接目标时仍校验目标文件版本。可写扩展属性的范围见下文。

`user.tape.state=local` 表示本地内容尚未上传，不能据此认为已归档。其他 user.tape 属性描述服务端已提交版本。

缓存使用每次挂载独占、权限为 0700 的子目录，正常退出清理本次目录，保留缓存根目录中的其他内容。缓存空间需要容纳打开的文件（同一 inode 的句柄共享一个文件）；异常终止后的旧子目录不会自动删除。

## 当前限制与验证范围

当前提供 `user.tape.state`、`user.tape.generation`、`user.tape.version`（轮次.序号）、`user.tape.barcode`、`user.tape.sha256`，并透传已提交文件的索引修改时间与文件 xattr。索引 xattr 名称放在 `user.` 命名空间，已有该前缀则不重复添加；内置 `user.tape.*` 属性优先。文本按 UTF-8 返回，Base64 值解码为字节，损坏编码返回 EIO。原生目录的修改时间已透传；合成根目录、旧缓存推导的目录及缺失或非法时间回退为 Unix epoch。HTTP 下载已去掉 256 MiB 上限：设备线程分块写入服务端暂存目录下的匿名临时文件，完成后由 HTTP 线程流式发送。临时文件在最后一个句柄关闭时自动回收；服务端需要容纳并发下载的临时磁盘空间。Range 仍先读完整文件到临时磁盘再切片，尚未优化成只读所需磁带块。采用内核页缓存和默认 write-through，不启用异步 writeback-cache，也不再依赖 `FUSE_DIRECT_IO_ALLOW_MMAP`。每次 open 不设置 KEEP_CACHE，避免无条件复用旧页；支持读写混合打开；共享可写映射不属于当前承诺或 LE 官方只读 mmap 支持范围。

纯软件测试覆盖删除提交、接管、墓碑回收、删除响应丢失不重放，257 MiB 文件上传/下载逐字节校验、提交等待超时后的查询、提前文件标记拒绝短读，以及适配器的关闭/同步、随机写、多写句柄、追加、截断、目录和错误映射。可显式运行使用内存后端的内核挂载测试，不接触磁带：

```bash
cargo test --features fuse --bin tape-fuse -- --ignored
```

Holo 端到端实测已覆盖 fsync、覆盖、删除、旧句柄读取、mv、Range 和计划接管；EE 删除/覆盖现场对照与物理带库验证仍未完成。详见[现场记录](../.scratch/ltfs-ha/holo-file-client-20260924.md)。


## 元数据兼容性

目录日志条目增加可选 metadata 字段，SQLite 的 files/staging 表按需增加同名 TEXT 列。新版本可以解码旧日志、迁移旧库，已有版本号保留；原来未缓存的修改时间和 xattr 保持未知，直到从磁带索引重新形成目录记录。迁移不会主动装带、改写磁带或强制刷新全部旧记录。

建议整组节点统一升级后使用这些字段，不承诺新旧二进制混跑或回退时的元数据完整性。Rust 的 FileStat/FileRec 增加字段，直接构造这些结构的调用方需要更新。传输与缓存保留原索引属性；回收复制已保留源文件的时间、只读标记和原始自定义 xattr（包括 Base64 标记）；新副本分配新的卷内 UID 和版本号，重新计算内容哈希。普通上传替换仍生成新时间，并从当前目录版本继承原 xattr（包括二进制属性），内容哈希和版本号重新生成；跨带覆盖同样适用。目录不可读或旧记录缺少 xattr 元数据时拒绝覆盖，需先从磁带重新对账，不能把未知属性当成空属性。不能通过客户端注入回收元数据。


## 工具兼容性实测

本机 Linux 6.8.0-124-generic、GNU coreutils mv 9.4 下，显式内核测试已验证：只读共享 mmap 读取正确；读写混合打开返回 EOPNOTSUPP；mv 经 EXDEV 回退后目标内容一致、源路径删除；目标暂存返回 ENOSPC 时 mv 失败并保留源文件。测试使用内存后端，不能代替集群或磁带验证。

mv 的复制和删除不是原子操作，目标 close 成功仍只表示暂存。测试显式模拟后台提交后再检查目标内容；需要归档保证的应用应使用带 fsync/提交确认的工作流。

内核能力依据：[Linux FUSE I/O 文档](https://docs.kernel.org/filesystems/fuse/fuse-io.html)。旧内核或发行版回移情况需实际协商确认，不能仅按版本号推定。

实验室 Rocky Linux 5.14.0-427.20.1.el9_4、GNU mv 8.32 下：普通 FUSE 读写和 mv 回退通过，但只读共享 mmap 返回 ENODEV；mv 会提示无法保留权限和时间。不能将本机 Linux 6.8 的 mmap 通过结果视为所有目标系统可用。实验室使用 `fusermount -u` 卸载。

## 页缓存版本的可见性边界

2026-09-24 的改造对齐现场 LE 的同挂载行为，取代此前每读句柄独立快照的设计。写入和截断共享本地文件；unlink 后文件由已有句柄/映射保持，最后引用释放后删除本地缓存；仅暂存的上传额外保留本地副本，直到后续打开确认已提交、被替代或消失，或挂载结束。没有后续访问时，该副本可能占用磁盘直到卸载。顺序写和 O_WRONLY 限制已在后续 LE 对齐中解除；共享可写 mmap 不在支持承诺内。仅 write-through 的普通写路径开放，归档确认仍通过写句柄 fsync 等待服务端提交，比 LE 的普通 fsync 提供更强的明确保证。

已有本地引用时，新读 open 会检查服务端已提交内容；如与当前缓存不符，返回 ESTALE，避免在同一 inode 混入不同内容。关闭该文件所有句柄并解除映射后重新打开，才重新下载。对于仅有服务端在途版本的本地文件，本挂载可继续读暂存内容。跨客户端没有实时缓存一致性或自动刷新承诺；已打开句柄不会后台查询远端变化。没有 SHA256 的旧记录用完整 stat 前后核对，内容证明能力弱于具有哈希的记录。

首次下载有哈希的内容必须匹配打开前的长度和 SHA256；不匹配返回 EAGAIN。内核页缓存之外仍保留本地磁盘文件作为缺页/预读来源，当前不提供跨挂载缓存复用或常驻缓存淘汰服务。既有 direct I/O 版本在 Rocky 5.14 上的 ENODEV 记录保留为历史证据，新版本须单独验收。

页缓存版本已在 Rocky Linux 5.14.0-427.20.1.el9_4 上通过 Holo 实测：共享只读 mmap、写入在 fsync 前可见、截断影响旧句柄/映射、删除重建隔离、关闭 fd 后映射读取均通过。外部 ltfsctl 覆盖时新 open 返回 ESTALE，原映射保持一致；关闭全部引用后重开读到新内容。旧版 ENODEV 不再适用于此版本。


本机内核另有两项显式挂载回归：远端在 LOOKUP 与 OPEN 之间变长/变短后，逐块读取仍得到完整新内容；已有只读映射时外部替换触发新 open 的 ESTALE，旧映射保留内容，全部释放后重开刷新。未保留实验性的“首次 OPEN 与上次属性长度不同即拒绝”限制，因为移除该限制的实际挂载测试同样通过；这不代表提供跨客户端实时一致性。两项测试使用内存后端，尚未在 Rocky 5.14 重跑。


LE 后续能力实测及实现进度见 [LE/Holo 能力矩阵](../.scratch/ltfs-ha/le-capabilities-holo-20260924.md)。本次写入接口扩展仅完成本机软件和 FUSE 验证，尚未部署到 Rocky 客户端；原子 rename、空目录持久化、可写 xattr/符号链接、LE 权限语义及数据/索引分离同步仍待实现，不能称为完整 LE 兼容。

### LTFS 索引持久化（2.5.1）

服务端上传和删除批次使用标准 DP 增量索引（XML 格式版本 2.5.0）。连续最多 5 份增量后，下一次有变更的提交生成 DP/IP 全量检查点；更小的恢复链预算会进一步缩短此间隔。正常停机和换出写入带前也生成全量检查点。全量只重写索引，不复制文件数据。

提交成功仍要求磁带持久化屏障及 VCI 更新完成；202 仍仅表示节点暂存。恢复从 Full 顺序应用依赖增量，缺链不得当作最新完整视图。旧版 LTFS 工具对异常增量尾部的恢复不在兼容性保证内；清洁 Full 与现场 LE 的互操作仍需独立验证。


## 持久化目录接口

Rust `Client::mkdir(path)` / `Client::rmdir(path)` 和 FUSE 的目录操作使用 `POST` / `DELETE /directories/<path>`。201 表示索引已提交；202 表示仍在暂存，客户端返回 `Indeterminate`。请求或提交响应丢失时不自动重放；只有服务端明确答复任务未落带时才允许重试。调用方需查询当前状态后决定如何处理结果未定，不能把“目录存在”直接等同于自己的创建成功。

目录仍是标准 LTFS 目录节点，使用 `tapers.version` 排序，删除通过带 `tapers.deletedKind=directory` 的版本化墓碑发布；普通文件列表不混入目录，`list_dir` 和 `stat` 可读目录，后者通过 `metadata.kind=directory` 区分。回收会搬迁空目录、时间、只读标记和原始 xattr，并将目录纳入格式化前的两道检查；目录不会作为零长度普通文件复制。

目录记录复用 SQLite 的 metadata 字段，不新增表或生产依赖；旧程序不能正确解释目录类型，必须统一升级所有 ltfsd 节点和客户端。旧介质在重新装载并对账后才补入原生目录记录，即使索引代数相同也会刷新。升级后应先完成相关磁带对账，再执行目录写操作；未装载的旧磁带空目录不会凭空出现在缓存中。目录删除与文件删除一样依赖池内版本/墓碑：单独导出旧带不能反映其他带上的删除。

本次目录功能通过三节点模拟集群的创建/删除、跨带旧索引对账、接管和空目录回收测试，以及本机真实 FUSE 卸载/重挂载测试（内存后端）。目录功能尚未部署到 Holo，也未新增 LE 互操作现场验收；rename 仍为 EXDEV。

## 同写入卷 rename（本地实现，尚未部署）

`Client::rename(from, to, no_replace)` 对应 `POST /rename?from=<编码路径>&to=<编码路径>&noreplace=0|1`。201 committed 才确认源删除和目标出现已由同一索引提交；202、504或响应丢失返回 Indeterminate，不能盲目重放。已有目录版本的服务端尚未提供此接口，需配套升级。本阶段不支持跨带原子 rename、RENAME_EXCHANGE 或 WHITEOUT。

源子树与目标同时预留，拒绝相关在途上传、删除和回收；允许替换文件或空目录，禁止类型不一致、目标父目录缺失及源目标互为祖先。旧源路径逐项写入版本化墓碑，目标保留原生节点，沿用增量提交和正常卸载 Full 检查点。引用已有数据不消耗第二份文件容量。

FUSE 改名保持源 inode 和目录下已知子 inode；打开的源写句柄先提交脏数据，改名成功后继续写入新路径。已打开的被替换目标仍可访问缓存，后续 flush/fsync 不会覆盖新目标。路径操作在改名期间串行，已有数据句柄通过句柄锁协调；close后尚未确认的暂存上传可使改名返回 EBUSY。

若改名返回 EIO（包括提交结果未定），当前挂载停止后续路径访问及写入，已有句柄仍可读缓存；使用独立 Client 核对后重新挂载。这样不会在结果未定后向旧名字继续上传。本机 FUSE 和三节点模拟验证不代表已经完成 Holo/LE rename 互操作。

LE 改回曾删除或改名的路径时可能保留 tape-rs 私有墓碑。恢复时同一索引内的原生文件/目录优先于旧墓碑，保留节点自己的版本号；跨磁带版本比较规则不变。此行为已通过 [Holo/LE rename 往返验收](../.scratch/ltfs-ha/holo-rename-20260924.md)，原集群尚未升级 rename 版本。

2026-09-24 后续部署：原 Holo 三节点已统一升级上述 rename/LE 修复版本，实际验证目录与文件改名、打开 writer、正常重启接管和 mmap 读回。正式程序位于 `/home/rocky/tape-rs-rename-prod-20260924`，详见 [部署验收记录](../.scratch/ltfs-ha/holo-rename-upgrade-20260924.md)。跨带 rename 仍返回 EXDEV。

读取故障切换修复（尚未部署）：服务端读到设备预留丢失时关闭旧执行者，并在任何成功内容发送前返回503；现有Client可自动换节点重读。普通500读取错误仍返回失败；`get_to`若已写入部分内容后发生网络错误，不自动拼接第二次响应，调用方须丢弃部分输出再重试。详见 [诊断与验证记录](../.scratch/ltfs-ha/client-read-failover-20260924.md)。

后续 [隔离 Holo 验收](../.scratch/ltfs-ha/holo-read-failover-20260924.md) 已实际触发旧执行者的06/2A05及HTTP503，保留旧Leader缓存的get/get_to/get_range均成功自动切换。修复仍待部署原集群。

2026-09-24 部署更新：读取故障切换修复已统一部署原Holo三节点，服务端目录 `/home/rocky/tape-rs-read-prod-20260924`；现有Client和FUSE保持原版本，原9文件读取与mmap验收通过。详见 [上线记录](../.scratch/ltfs-ha/holo-read-upgrade-20260924.md)。


### 可写用户扩展属性（已部署并通过原集群验收）

Client::setxattr(path, name, value, flags) 与 removexattr(path, name) 已接通 HTTP 和 FUSE。仅支持当前写入卷上的文件与原生目录；其他卷返回 EXDEV。flags=0 表示创建或替换，1（CREATE）遇到已有属性返回 EEXIST，2（REPLACE）以及删除不存在的属性返回 ENODATA。支持空值和二进制值，单值最多 65536 字节，属性名最多 255 字节。

属性名必须属于 user. 命名空间；user.tape.*、user.tapers.*、user.ltfs.* 保留，不能写入或删除。拒绝 user.user.* 别名；合成根目录不支持修改，尚未实现 LE 的 user.ltfs.sync 虚拟控制属性。索引值使用 Base64，元数据通过现有 LTFS 增量索引提交，不重写文件内容，保留 UID、extent、mtime 和内容哈希。正常停机的完整索引策略不变。

HTTP POST /xattrs?path=…&name=…&flags=… 的请求体是原始字节，DELETE 同一路径删除属性。201 才代表提交成功；响应丢失或提交未定返回 Indeterminate，不自动重放。FUSE 修改前同步同一文件的脏句柄，提交结果不确定后阻止继续写回。后续覆盖文件内容继续保留已提交的用户属性。新增接口需要服务端同步升级，旧服务端不支持。

模拟集群已验证二进制/空值/64 KiB、目录属性、CREATE/REPLACE/删除、原生数据布局不变、后续内容覆盖及 Leader 接管；新增设置/删除已通过实际内核调用及隔离 Holo/LE 双向回写，包含 LE 2.4.0 降级后的替换、创建、删除和空值读回。空 Base64 值必须输出自闭合 XML 元素，已修复并由 IBM 解析器及实际挂载验证；详见 [验收报告](../.scratch/ltfs-ha/holo-writable-xattr-20260924.md)。

原三节点已统一部署可写 xattr 与空值 XML 修复，实际 FUSE 专用目录写入、计划停机接管、全新挂载读回及清理均通过；原文件校验一致，详见[部署记录](../.scratch/ltfs-ha/holo-writable-xattr-upgrade-20260924.md)。

隔离 Holo 已验证属性提交中断以及设置/删除已提交但响应丢失：Client 返回 Indeterminate，恢复后分别保留旧值或已提交的新值/删除状态，没有重放。实际中断点为周期性 Full 索引数据写入后、结束文件标记前；不代表全部 Incremental 阶段或存储断电已覆盖。[故障验收记录](../.scratch/ltfs-ha/holo-xattr-faults-20260925.md)。

独立 Incremental 索引也已补测：确认增量 XML 数据写入后、结束文件标记前终止隔离 Leader，恢复排除未完成增量并保留旧属性，收尾后可继续提交。[增量截断验收](../.scratch/ltfs-ha/holo-incremental-cut-20260925.md)。这仍是进程崩溃测试，不是存储断电。

“索引已提交、目录未发布”窗口已通过隔离Holo验证：磁带gen72/目录gen71时中断，Client返回Indeterminate；新Leader按磁带对账补齐三节点目录，保留原提交版本及新属性，没有重放。[目录发布前故障验收](../.scratch/ltfs-ha/holo-publication-cut-20260925.md)。


已有 LTFS 符号链接读取（2026-09-25）：服务端对链接记录增加 `metadata.kind="symlink"` 和 `metadata.symlink`，普通文件元数据不变，HTTP stat 的 length仍是LTFS链接记录长度；FUSE lstat的size为目标字符串字节数。升级服务端并重挂介质对账后，新FUSE才能识别链接；旧服务端没有这些字段时不能提供这项能力。相对路径、链接链、目录链接、悬空和循环由内核解析。链接创建、硬链接及链接写操作的完整LE对齐仍不在本次验收范围。目录打开为非目录项补查属性以得到准确d_type，因此大目录会增加元数据请求；本轮未测大目录性能。修复版已在隔离Holo的LE Full85验证，并于2026-09-25统一部署原集群；正式服务端及客户端目录为/home/rocky/tape-rs-symlink-prod-20260925。

符号链接创建（2026-09-25，隔离验收完成，尚未正式部署）：`Client::symlink(target, path)` 对应 `POST /symlinks?path=...&target=...`，FUSE 支持 `symlink(2)`。父目录必须存在，已有路径返回 EEXIST；目标原样保存，允许相对、绝对、悬空和循环目标，空目标及 XML 非法字符返回 EINVAL。成功表示原生 LTFS 索引已提交；响应丢失返回 Indeterminate，不自动重放。链接改名、删除及通过链接写入目标已在隔离 Holo 验证，LE 能读取新建链接；硬链接仍不支持。新客户端需配套新服务端；旧正式服务端没有创建端点。详见[创建验收](../.scratch/ltfs-ha/holo-symlink-create-20260925.md)。

2026-09-25部署更新：符号链接创建版已统一发布到原三节点及tsclient，正式程序目录 `/home/rocky/tape-rs-symlink-create-prod-20260925`。实际FUSE创建、链接写入、改名删除、计划接管后全新缓存读回及清理通过，原9文件校验一致；取代上文“尚未正式部署”。见[部署验收](../.scratch/ltfs-ha/holo-symlink-create-upgrade-20260925.md)。

符号链接跨带回收修复（2026-09-25，尚未正式部署）：回收直接迁移链接节点，不解析或读取目标，保留链接目标、时间、只读标记及属性，并在目标卷分配UID。悬空、循环和目录链接不再因目标不可读而中止回收。模拟集群及两盘专用Holo虚拟带完成回收、源带重新格式化、全新FUSE缓存和接管读回；详见[验收记录](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md)。
