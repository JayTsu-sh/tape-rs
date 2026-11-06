# 文件客户端与 FUSE 挂载

命令语法、选项、异步任务与常用流程见[命令行参考手册](cli-reference.md)。

当前能力与部署状态见 [剩余实施队列](../.scratch/ltfs-ha/remaining-implementation.md) 和 [交接入口](CLAUDE-HANDOFF.md)。本文带日期的实验记录保留当时验证边界；后续记录取代早期状态。

`ltfsctl` 与 `tape-fuse` 都通过 ltfsd 客户端接口寻找 Leader；节点地址可以重复指定。它们不直接打开磁带设备。

希望客户端固定访问一个地址时，可启用 [ltfsd 内置 HTTP VIP](http-vip.md)：首版支持同子网 IPv4，由有效设备 owner 持有，无需 Keepalived。

FUSE 与 Rust Client/lib 使用各节点物理地址访问原文件接口，不使用 VIP；管理 HTTP 与 S3 使用 VIP，见 [S3 接口](s3.md)。

数据访问现在以池为命名空间：`/libraries/<真实带库序列号>/pools/<池名称或UUID>/files/...`，`stat`、`list`、`tasks`、`sync`、目录、属性和改名操作也使用同一池前缀。库内恰好一个池时可以省略池；存在多个池时省略会返回 `400 pool_required`，避免把文件写入任意池。管理接口继续使用库作用域的 `/admin/...`。

文件任务 ID 包含本次设备 owner 轮次，受理与终态随 Raft 日志复制到各节点目录。`GET /tasks/<id>` 在接管或进程重启后仍可查询；接管时尚未获多数派确认的任务会显示 `indeterminate`，此时应按路径查询内容与哈希。写入响应只有在任务记录已应用后才确认任务 ID；等待超时返回带任务 ID 的 504，不代表请求未执行。文件任务与 `/admin/tasks` 管理任务仍使用各自的查询路径。

已持久化的文件任务通过固定 HTTP 入口在本地复制目录查询；即使该库暂时没有设备 owner，`GET /tasks/<id>` 和带池前缀的查询仍可返回终态。新任务受理与文件读写仍需要有效的设备 owner。目录快照正在发布时查询可暂时返回 `catalog_pending`，稍后重试。

```bash
ltfsctl -e 192.0.2.11:7401 --library 55L3A78011W9LL01 --pool multilib-test put ./file.bin /file.bin
tape-fuse -e 192.0.2.11:7401 --library 55L3A78011W9LL01 --pool multilib-test \
  --cache-dir /var/cache/tape-client /mnt/tape
```

Rust 客户端使用 `Client::new(endpoints).with_library(serial)?.with_pool(pool)?`；重定向及 owner 变化不会改变调用方选定的库和池。一次 FUSE 挂载固定一个池。同一个池的不同驱动器共享路径预留、任务号和最新目录视图，每台驱动器独占自己的卷会话及容量预算；文件版本序号由库级执行者统一分配。跨卷的原生改名仍返回 `cross_device`。

并发上传按需申请额外写驱动。一台驱动的批次到期时，也启动其他驱动已收齐的队列；普通工作每台只提交一批，随后返回 owner 循环处理控制请求。`mountlimit` 限制同池的写入驱动租约，包含准备写入的装带阶段；只读装载、维护源带和闲置但没有写入租约的介质不占该配额。资源不足时，读请求或其他池可以借用已排空的写驱动；借用前先关闭新准入、再次核对空闲状态并生成卸载检查点。整库维护与计划停机要等待所有驱动写入作业归还句柄，才会移动介质或释放 PR。

当前 FDB 数据链路优先复用已装载的合格磁带，其次按槽位中条码的字典序选择第一盘合格磁带。候选必须属于指定 Library/Pool，卷 UUID 已绑定且索引已核验、在线、不在黑名单、不是清洗带，符合池介质限制和物理文件条目上限，并有启用的 m 角色兼容驱动可调度；驱动兼容性来自 REPORT DENSITY SUPPORT / WRTOK。待显式导入的磁带会被跳过；预约、恢复和节点修改占用仍须通过各自闸门。

这个 FDB 策略没有按 MAM 剩余容量或已用比例排序，也没有轮询均衡。驱动调度先复用目标带所在驱动，再找兼容空驱动，最后按挂载上限和 LRU 选择已核验、可卸载的读取挂载。导入目录和原文件所在磁带都不绑定之后的写入目标：同名覆盖可落在同池另一盘带上，公开路径以 FDB 最新已提交 head 为准。

上传尝试一旦绑定卷 UUID、代数和原请求，重试保留这些身份。实际写入重新核验索引和占用；发生介质错误或提交结果未定时先恢复并查询原请求，不把同一正文自动换带重发。当前 FDB 路径不承诺基于物理剩余空间的提前换带，也不承诺写满后透明切到下一盘。

大目录的原子发布在后台执行，该库的数据入口暂时返回 `503 catalog_pending`；目录发布完成后可重试。控制查询和多数派许可继续工作。目录快照固定同一日志索引，制作快照本身不暂停数据请求。

HTTP 接入使用 axum / Hyper，保持现有端点、状态码、JSON 字段和每次请求后关闭连接的行为；客户端无需升级。同步文件服务和设备任务等待在有界阻塞任务中执行，上传和下载逐块传输，不把整个文件装入内存，也不套用缓冲型提取器的默认 2 MiB 限制。

每节点最多同时接入 256 条连接、处理 32 个业务请求；下载发送期间仍占业务额度。业务额度用尽时返回 `503 http_busy`，连接额度用尽时关闭新连接；`/readyz` 不占业务额度，但仍受连接总量限制。请求头总量最多 32 KiB，须在 30 秒内接收完整；请求体连续 60 秒未收到下一帧时中止接收。文件上传仍要求 `Content-Length`，不支持 chunked 上传。直连 owner 时，`Expect: 100-continue` 在准入成功、开始读取内容时才发送；经固定 VIP 转发时，入口的 `100 Continue` 只表示可以发送正文，最终准入结果以后续状态码为准。owner 提前拒绝转发请求时，最多用 5 秒排空 64 MiB 剩余正文后返回错误；正文超限或过慢、缺少长度以及业务额度已满时，客户端可能在发送过程中遇到断连而收不到原始错误。HTTP 断开或等待超时不代表已经提交的设备任务被取消，结果未定时仍须查询，不能自动重放有副作用的操作。

设备接管时，驱动器内仍有盒带不代表介质已经上带。新 owner 在设备隔离完成后显式执行 LOAD 并等待就绪，再识别和恢复卷；这覆盖计划停机已执行 UNLOAD、但没有把盒带移回槽位的情况。磁带恢复时间还包含机械定位和索引链检查，HTTP 就绪不等于池里已有可写磁带。

普通接管时，两分区的新鲜 VCI 与实际完整末索引一致、DP 为最新视图时，允许 IP 仍为旧检查点。Full 自包含；增量必须完整读取、核验并重放到其 Full 基线，才能省去基线之前的历史 Full 链审计，IP 维护债务保留。仍读取并核验两分区索引、卷身份、EOD、尾部和追加位置；增量链不完整会拒绝开放，过期或不符的 VCI、异常尾部及显式历史审计沿用原路径。依据为 [LTFS 2.5.1 §9.2、§10.3](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)，不改变卷提交屏障或设备隔离。


与 IBM EE 的最新物理对照、已确认差距及待验边界见 [EE GAP 现场审计（2026-10-21）](ee-gap-physical-audit-20261021.md)。

## 集群 HTTP 管理接口

当前集群管理以 FDB 为权威，关系为 Library → Pool → Tape；每库有参与 nodes 和唯一就绪 active_node。完整命令、HTTP 层级及介质安全边界见 [管理模型](management-model.md)，FDB 配置、持久请求与数据接口见 [FoundationDB](foundationdb.md)。不再提供节点分组、G0 或旧的非池作用域管理写入入口。

```sh
ltfsctl -e HOST:7401 library list --json
ltfsctl -e HOST:7401 library show -l SERIAL --json
ltfsctl -e HOST:7401 library rescan -l SERIAL --request rescan.json
ltfsctl -e HOST:7401 pool create archive -l SERIAL --request pool.json
ltfsctl -e HOST:7401 pool list -l SERIAL --json
ltfsctl -e HOST:7401 pool show archive -l SERIAL
ltfsctl -e HOST:7401 tape list -l SERIAL -p archive --json
ltfsctl -e HOST:7401 tape import BARCODE -l SERIAL -p archive --request import.json
ltfsctl -e HOST:7401 task show NAMESPACE:EPOCH:OPERATION -l SERIAL
```

`library rescan` 和介质命令异步返回持久任务号；列表/详情读取缓存观测，不逐卷读取索引。所有设备任务先保存规范原请求，再提交到相应 Library/Pool/Tape 的 HTTP 路径；不能用空正文或临时 `{location:...}` 代替完整请求。重试保留原 namespace、epoch、operation 和操作参数。任务状态、进度及终态在 FDB，SQLite 只承担本地缓存；owner 切换接续原任务。

`tape assign` 可能初始化介质，必须 `--yes`，覆盖有文件的带还须 `--force-format`。`tape import` 保留介质内容、恢复 FDB 文件索引。`tape move -L homeslot|ieslot` 核验库存和持久 home 证明；已分配介质到 I/E 前须离线。`tape unassign` 要求无引用和在途依赖，不擦除介质。`tape validate` 核验标签/完整索引链，报告 `payload_verified=false`；不提供 `--full-read`，不隐式修复或导入。

查询的设备路径属于 `host_device_node`，不作为其他节点的设备路径。缺失或不可信的容量、可回收比例返回 null，不使用型号或文件字节数推算。历史物理容量验证见 [现场报告](../.scratch/ltfs-ha/reports/admin-capacity-physical-20261020.html)；该报告不代替当前 FDB 管理模型的现场验收。

## 文件客户端使用

```bash
cargo build --bin tape-fuse --features fuse
mkdir -p /tmp/tape-mount /tmp/tape-cache /var/lib/tape-client
tape-fuse -e NODE1:7401 -e NODE2:7401 -e NODE3:7401 \
  --library SERIAL --pool POOL_UUID --fdb-namespace NAMESPACE_UUID \
  --fdb-journal-dir /var/lib/tape-client/fdb-journals \
  --cache-dir /tmp/tape-cache /tmp/tape-mount
```

当前FDB集群使用上述显式namespace与池UUID；替换示例中的节点地址和标识。省略`--fdb-journal-dir`时为内核只读；日志根由程序创建为0700，父目录须存在。完整文件CLI及原操作恢复见[FoundationDB手册](foundationdb.md#fdb-挂载)。

挂载进程在前台运行，需要 Linux FUSE 与挂载权限。另一个终端执行 `fusermount3 -u /tmp/tape-mount` 卸载。默认只向挂载用户开放，属主权限由内核检查。

## 完成语义

- `ltfsctl put` 等待文件落带；`delete` 等待删除墓碑落带，不立即回收介质空间。
- FDB挂载的`flush`/`close`和`fsync`同步确认S4及原回执；未确认时保留原日志并报告错误。普通文件接口模式的`close`确认服务端暂存，归档确认使用`fsync`。
- 删除响应丢失会返回结果未定，不自动按路径重发，避免删除随后创建的新版本。
- 写入支持 `O_RDWR`、非截断打开、任意偏移覆盖、`O_APPEND` 和任意长度截断；多个写句柄共享内容及上传状态。非截断写首次需要完整下载旧内容。`O_EXCL` 提供只创建保护。
- 服务端读取只选择已提交文件；只有服务端在途版本且没有本地文件时返回 `EBUSY`。首次读打开下载整个已提交文件，因此可能等待磁带读取。 FDB 模式读取 EE 等外部文件时允许带内未记录正文摘要；此时私有缓存仍核对完整长度和读取前后的命名空间、inode、版本及介质位置，不能声称已按带内摘要验证正文。有 SHA-256 或 MD5 时严格校验，错误或部分正文不发布缓存。显式校验导出 `ltfsctl fdb get` 仍要求已登记的摘要。同一 inode 的本地句柄共享内容，本挂载写入在 fsync/close 前可读；覆盖和截断对旧句柄及映射可见。删除后已有句柄继续引用旧文件，同名重建使用新 inode。
- `mkdir`/`rmdir` 等待 LTFS 索引提交；空目录跨挂载和节点接管保留。`mkdir` 要求父目录存在，`rmdir` 拒绝非空目录及在途子路径。目录流在打开时取快照。
- `rename` 支持当前写入卷内的文件和目录原子改名、同类型目标替换，以及 `RENAME_NOREPLACE`；目录替换要求目标为空。保留源 inode、UID、extent、mtime 和 xattr。跨带源/目标或不在当前写入卷的源仍返回 `EXDEV`；复制加删除回退不具备原子性。权限/时间修改的原生映射见下文；符号链接支持创建及读取，已有符号链接可通过 FUSE 的 lstat/readlink 和内核路径解析读取，读取链接目标时仍校验目标文件版本。可写扩展属性的范围见下文。

`user.tape.state=local` 表示本地内容尚未上传，不能据此认为已归档。其他 user.tape 属性描述服务端已提交版本。

缓存使用每次挂载独占、权限为 0700 的子目录，正常退出清理本次目录，保留缓存根目录中的其他内容。缓存空间需要容纳打开的文件（同一 inode 的句柄共享一个文件）；异常终止后的旧子目录不会自动删除。

## 当前限制与验证范围

当前提供 `user.tape.state`、`user.tape.generation`、`user.tape.version`（轮次.序号）、`user.tape.barcode`、`user.tape.sha256`，并透传已提交文件的索引修改时间与文件 xattr。索引 xattr 名称放在 `user.` 命名空间，已有该前缀则不重复添加；内置 `user.tape.*` 属性优先。文本按 UTF-8 返回，Base64 值解码为字节，损坏编码返回 EIO。原生目录的修改时间已透传；合成根目录、旧缓存推导的目录及缺失或非法时间回退为 Unix epoch。HTTP 下载已去掉 256 MiB 上限：设备线程分块写入服务端暂存目录下的匿名临时文件，完成后由 HTTP 异步任务流式发送。临时文件在最后一个句柄关闭时自动回收；服务端需要容纳并发下载的临时磁盘空间。Range 仍先读完整文件到临时磁盘再切片，尚未优化成只读所需磁带块。采用内核页缓存和默认 write-through，不启用异步 writeback-cache，也不再依赖 `FUSE_DIRECT_IO_ALLOW_MMAP`。每次 open 不设置 KEEP_CACHE，避免无条件复用旧页；支持读写混合打开；共享可写映射不属于当前承诺或 LE 官方只读 mmap 支持范围。

纯软件测试覆盖删除提交、接管、墓碑回收、删除响应丢失不重放，257 MiB 文件上传/下载逐字节校验、提交等待超时后的查询、提前文件标记拒绝短读，以及适配器的关闭/同步、随机写、多写句柄、追加、截断、目录和错误映射。可显式运行使用内存后端的内核挂载测试，不接触磁带：

```bash
cargo test --features fuse --bin tape-fuse -- --ignored
```

2026-10-20 已在物理节点显式运行四项默认忽略用例：内核 VIP 地址生命周期、VIP 进程暂停/恢复/退出/强杀、增量 XML 导出及官方 XSD 校验、百万文件索引解析，均通过。VIP 使用隔离网络中的真实内核、HTTP 和 ARP，控制日志提交按用例模拟；本轮不访问磁带。结果、资源测量、清理核验及复用步骤见[验收摘要](../.scratch/ltfs-ha/reports/ignored-four-physical-20261020-summary.html)。默认测试的环境门控保持不变。

2026-11-04 已在209的真实内核FUSE和A库AA0017L8上验证显式FDB模式的创建、读写、fsync、随机写/追加/截断、多句柄、目录与文件改名、二进制xattr、链接、删除/同名重建及全新缓存重挂；独立S3读回的长度和摘要一致。首轮发现并修复了删除历史head导致后续rename误判的问题，原任务接续与剩余检查通过。见[FUSE物理验收报告](../.scratch/ltfs-ha/reports/fuse-fdb-physical-20261104.md)。写入中故障迁移和断电未在本轮测试。此前Holo范围作为[历史记录](../.scratch/ltfs-ha/holo-file-client-20260924.md)保留。


## 元数据与文件身份

传输与缓存保留原索引属性；未登记的属性保持未知，不能当作空属性。回收复制保留源文件的时间、只读标记和原始自定义 xattr（包括 Base64 标记）；新副本分配新的卷内 UID 和版本号，重新计算内容哈希。普通上传替换生成新时间，并从当前目录版本继承原 xattr；跨带覆盖同样适用。目录不可读或缺少必要属性时拒绝覆盖，需先从磁带重新对账。不能通过客户端注入回收元数据。

Rust `FileStat.version` 使用 `FileVersion` 区分真实身份域：Raft 的轮次/序号、原生 LTFS 的卷 UUID/fileuid/索引代数，以及 FDB 的 namespace/inode/逻辑版本。不同身份域不排序；LTFS 只在同卷同 fileuid、FDB 只在同 namespace 同 inode 内排序。版本显示分别为 `round.sequence`、`ltfs:VOLUME/UID@GENERATION` 和 `fdb:NAMESPACE/INODE@VERSION`。`FileStat.round` 为可选的查询执行轮次；纯元数据与原生 LTFS 查询不填写执行轮次。`FileStat.generation` 同样可选：只有真实物理来源才填写代数，逻辑目录不补0。

FDB 文件缓存即使 SHA-256 相同，也核对 namespace、inode 和逻辑版本。已有读句柄及映射保留原内容、版本、哈希和扩展属性。同一 FDB inode 的版本变化时，新 open 返回 ESTALE，最后一个旧引用关闭后可重新下载；同名重建为不同 FDB inode 时分配独立的本地 inode，新文件可在旧句柄仍打开时读取。文件重建为目录或链接也按各自身份报告类型。首次下载期间身份变化返回 EAGAIN，并丢弃私有临时缓存。

`Backend::list_dir` 返回 `DirectoryListing`，携带本次捕获的目录版本和子项；`DirItem.version` 携带后端提供的子项版本。FDB 列举保留真实父子身份，目录项与 lookup 使用相同的本地 inode；原目录被同名重建后，旧 inode 的列举与子项查询返回 ESTALE；子项查询前后均核对 FDB 父目录身份，同一父 inode 仅版本变化仍可查询。Raft 和原生 LTFS 当前列举没有提供这些版本，明确保留 None。所有调用方直接使用当前接口。

Raft 路径查询要求已知状态、真实的双整数版本字段及真实索引代数，缺失或错误编码返回 Verification。条件删除接收类型化文件版本，先拒绝其他身份域，再发送 Raft If-Match；FDB 操作继续使用保存的原请求。上传结果可通过只读入口查询，不依赖当前 owner；未确认请求的写入仍须连接受当前许可保护的 writer。首版直接更新所有 Rust 调用方，不提供旧 tuple 转换接口。FDB 已提供[显式挂载入口](foundationdb.md#fdb-挂载)，固定 namespace 与池，保留原属性并按登记算法核验下载。默认内核只读；指定专用 `--fdb-journal-dir` 后持久保存原操作并支持普通文件创建、覆盖和原操作恢复。FDB 的 flush 同步确认原 S4 与完整回执，未知结果冻结原正文，根 `user.ltfs.sync` 继续原上传；不追加原生后端的完整检查点。FDB 已支持目录、链接、删除、属性和 xattr 修改。Rust SDK、[rename CLI](foundationdb.md#rename-cli) 和 FUSE 均使用双路径改名及持久原日志。FUSE 改名前提交源文件及子树的脏数据，成功后保留源 inode 并更新打开 writer 的完整版本条件；结果未定时冻结路径操作和写入，根 `user.ltfs.sync` 继续同一原请求。


## 工具兼容性早期实验（后续页缓存版本已取代）

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


LE 后续能力实测及实现进度见 [LE/Holo 能力矩阵](../.scratch/ltfs-ha/le-capabilities-holo-20260924.md)。随机写、原生目录、同写入卷 rename、可写 xattr 和符号链接已实现并部署。LE 权限/时间语义已接通，现场验收状态见交接入口；user.ltfs.sync 已隔离验收，普通 fsync 保留更强归档确认，不能称为完全相同的 LE 同步语义。

### LTFS 索引持久化（2.5.1）

服务端上传和删除批次使用标准 DP 增量索引（XML 格式版本 2.5.0）。连续最多 5 份增量后，下一次有变更的提交生成 DP/IP 全量检查点；更小的恢复链预算会进一步缩短此间隔。正常停机和换出写入带前也生成全量检查点。全量只重写索引，不复制文件数据。

提交成功仍要求磁带持久化屏障及 VCI 更新完成；202 仍仅表示节点暂存。恢复从 Full 顺序应用依赖增量，缺链不得当作最新完整视图。现场 EE 1.3.4 / LE 2.4.6 使用 LTFS 2.4 Full，不能把本项目的增量恢复能力当作 EE 能力。交接给 EE 前必须正常完成 Full 检查点及卸载，并确认最新有效索引为 Full；异常中断留下的增量末索引须先由本项目恢复，不能交给旧版 EE 修复。Full 同代重写按已理解字段的逻辑值核验，保留原始 XML 摘要和物理来源身份。真实互通结果与边界见[本轮报告](../.scratch/ltfs-ha/reports/fuse-s3-ee-physical-20261104.md)。


## 持久化目录接口

Rust `Client::mkdir(path)` / `Client::rmdir(path)` 和 FUSE 的目录操作使用 `POST` / `DELETE /directories/<path>`。201 表示索引已提交；202 表示仍在暂存，客户端返回 `Indeterminate`。请求或提交响应丢失时不自动重放；只有服务端明确答复任务未落带时才允许重试。调用方需查询当前状态后决定如何处理结果未定，不能把“目录存在”直接等同于自己的创建成功。

目录仍是标准 LTFS 目录节点，使用 `tapers.version` 排序，删除通过带 `tapers.deletedKind=directory` 的版本化墓碑发布；普通文件列表不混入目录，`list_dir` 和 `stat` 可读目录，后者通过 `metadata.kind=directory` 区分。回收会搬迁空目录、时间、只读标记和原始 xattr，并将目录纳入格式化前的两道检查；目录不会作为零长度普通文件复制。

目录记录复用 SQLite 的 metadata 字段，不新增表或生产依赖；旧程序不能正确解释目录类型，必须统一升级所有 ltfsd 节点和客户端。旧介质在重新装载并对账后才补入原生目录记录，即使索引代数相同也会刷新。升级后应先完成相关磁带对账，再执行目录写操作；未装载的旧磁带空目录不会凭空出现在缓存中。目录删除与文件删除一样依赖池内版本/墓碑：单独导出旧带不能反映其他带上的删除。

本次目录功能通过三节点模拟集群的创建/删除、跨带旧索引对账、接管和空目录回收测试，以及本机真实 FUSE 卸载/重挂载测试（内存后端）。后续 Holo/LE 往返和正式部署已完成，见下文 rename 验收记录。

## 同写入卷 rename

`Client::rename(from, to, no_replace)` 对应 `POST /rename?from=<编码路径>&to=<编码路径>&noreplace=0|1`。201 committed 才确认源删除和目标出现已由同一索引提交；202、504或响应丢失返回 Indeterminate，不能盲目重放。早期仅支持目录的服务端没有此接口；现正式版本已包含。本阶段不支持跨带原子 rename、RENAME_EXCHANGE 或 WHITEOUT。

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

属性名必须属于 user. 命名空间；user.tape.*、user.tapers.*、user.ltfs.* 保留，不能作为普通属性写入或删除。唯一控制例外是挂载根上的 user.ltfs.sync（见下）。拒绝 user.user.* 别名；合成根目录不支持普通属性修改。索引值使用 Base64，元数据通过现有 LTFS 增量索引提交，不重写文件内容，保留 UID、extent、mtime 和内容哈希。正常停机的完整索引策略不变。

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

2026-09-27 部署更新：回收修复已统一部署三节点，配套客户端位于 `/home/rocky/tape-rs-symlink-reclaim-prod-20260927`，取代上文“尚未正式部署”。实际FUSE链接操作、计划接管后冷缓存读回、清理及原文件校验通过；本轮未在原数据带执行回收。见[部署验收](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md)。

目录枚举使用服务器返回的子项类型快照，避免为每个文件单独查询属性；符号链接按链接自身类型返回。旧服务器没有 `is_symlink` 字段时客户端回退到逐项查询。该字段只扩展目录响应，不改变磁带索引或 catalog 持久化格式；目录打开后仍按该次快照分页，后续 lookup/getattr 查询当前状态。


显式同步：向 **tape-fuse 挂载根**（例如 `/mnt/tape`，不是其下普通目录 `/mnt/tape/sr`）的 `user.ltfs.sync` 写入任意值或空值，成功表示本挂载已有写句柄的内容、已关闭暂存内容得到确认，并完成当前服务卷的完整索引检查点。后端还覆盖同步执行时已完成接收的上传批次；其他客户端尚未完成的上传、随后到来的写入及尚未同步到服务端的本地缓存不属于此次保证。切换走的旧写入卷已按既有流程做检查点；不扫描或改写库中所有介质。

```python
os.setxattr('/mnt/tape', 'user.ltfs.sync', b'1')
```

该控制属性只写、不列举、不存入索引；读取返回 ENODATA，删除返回 EPERM，写在文件或子目录上返回 EACCES。CREATE/REPLACE 均允许；空闲且索引已完整时不会增加代数。调用期间本挂载已有writer被串行化；失败或响应丢失可能已有部分写入完成，调用者需核对，不自动跨节点重放或声称成功。Rust客户端对应 `Client::sync()`；HTTP `POST /sync` 同样只同步服务端已完成接收的写入。旧服务器不支持该操作，应先升级服务器。

现有 fsync 仍提供文件内容与索引归档确认，比 IBM LE 仅刷新数据的 fsync 保证更强；这里没有把暂存成功改称持久化成功。

硬链接不支持：LTFS 2.5.1 没有标准化的共享inode硬链接表示，tape-fuse保留EOPNOTSUPP，目标不会被创建。IBM LE 2.4.8.3实测返回ENOSYS；不以复制或符号链接替代硬链接。符号链接保持现有支持。


## 原生权限与时间修改

FUSE chmod/fchmod 将任意写位映射为 LTFS readonly=false；没有写位则为 true。文件基准模式保持0644，目录0755；readonly清除全部写位，符号链接仍报告0777。其他 mode 位不写入索引。chown/fchown/lchown 成功但不改变挂载属主 uid/gid，与 LE 的挂载所有权模型一致。创建时的 mode 仍不作为逐文件 POSIX 权限保存；需要只读标志时显式 chmod。合成挂载根不支持原生属性修改。

utime/utimes/utimensat/futimens 的显式 atime/mtime 使用标准 LTFS 纳秒 UTC 字段持久化，未指定字段保留；change_time由服务端更新，creation_time/backup_time/UID/extent/哈希保持。不会因读操作自动提交 atime。FUSE只读标志由内核 default_permissions 检查，root及已打开写句柄仍按Linux权限语义处理；HTTP/Rust接口不提供逐用户POSIX权限检查。

Rust Client::change_metadata(path, MetadataChange) 对应 POST /metadata?path=...，JSON仅接受readonly、access_time、modify_time，可省略或null。时间必须为四位年份、九位小数的UTC规范格式；空修改、未知字段、错误类型和非法时间拒绝。只支持当前写入卷原生文件/目录/链接；跨卷EXDEV，缺失路径ENOENT，在途冲突EBUSY。FUSE修改前同步已有writer；201才表示索引提交，结果未定或响应丢失不重放。

文件catalog metadata新增node对象，沿用已有JSON列与日志字段，没有新表、私有磁带格式或依赖。旧记录在装带对账后补齐；新接口需服务端与客户端统一升级。Attr新增只读与时间字段，直接构造该Rust结构的调用方需适配。内容覆盖保留已知readonly标志，时间仍遵循内容写入规则。

### 多带库的固定入口

`ltfsd --libraries-config` 启用同一 Raft 集群的多库管理。客户端访问固定 VIP，使用 `/libraries/<serial>/files/...`、`/libraries/<serial>/admin/...` 等路径使用真实 changer 序列号选择库；入口将请求转到对应 owner 或控制 Leader。原路径仍访问显式指定的默认库。两库可以由同一节点或不同节点持有，调用方无需跟踪节点。配置、兼容迁移、查询及故障返回语义见 [多带库说明](multi-library.md)。

FUSE 和 `ltfsctl` 可通过 `--library <真实 changer 序列号>` 选择目标库；每个挂载固定一个库。带库不再使用自定义 l8/l9 别名，新配置无需填写 id；多库旧别名状态不兼容。两个挂载的完整示例及配置见[多带库说明](multi-library.md#fuse-与命令行选库)。

### 跨库普通文件内容复制

`ltfsctl copy` 可从显式源库/池读取一个已提交普通文件，并将内容只创建到显式目标库/池的空路径。例如：

```bash
ltfsctl -e 10.131.7.219:17821 --library 55L3A7802K19LL01 --pool P1 \
  copy /archive.bin /archive.bin --to-library 55L3A78011W9LL01 --to-pool P2 \
  --spool-dir /var/tmp/tape-copy
```

临时目录必须容纳整个文件；临时文件权限为 0600，命令结束后清除。客户端在写目标前校验下载字节的长度、SHA-256 和源版本未变；目标落带后核对目录记录中的长度、SHA-256 和可读原生属性，再从目标流式读回全部字节计算 SHA-256，并复核两端版本。读回失败或不符时保留两端文件并报错，不能把目录哈希相等当作介质读回成功。现代目录记录中的创建/修改/访问/备份时间、只读标记和用户扩展属性随一次目标写带保存；目标生成自己的卷内 UID、版本和内容摘要。旧记录缺少完整属性时只复制内容；属性超过 12 KiB 或格式损坏时拒绝写目标。目标已存在时失败，不覆盖，也不删除源。只创建上传的响应若丢失，即使查询到同内容目标，也返回结果未定：相同哈希不能证明该目标是本次请求创建的。目标已落带而源版本随后变化时返回校验失败，保留目标供人工核对。此命令不建立后续改写自动同步、故障读取或全局副本身份；因此不能代替 EE 式副本或安全迁池。

元数据传输使用新的内部 `X-Tapers-Copy-Metadata` 请求头。所有数据 owner 升级前不要用新客户端执行跨库复制：旧服务可能忽略该头并写下缺失属性的目标，新客户端会在落带后的核验阶段拒绝把它报告为成功；此时源仍保留，目标须人工核对。

源记录必须包含可判定普通文件类型的元数据和 SHA-256；旧目录记录缺这些字段时须先装带对账。

`Client::delete_if_version` 和 `ltfsctl delete --if-version round.sequence` 只在路径当前版本仍与参数一致时提交删除墓碑；版本可从 `ltfsctl stat` 读取，版本变化返回 412，源内容不变。`If-Match` 请求头必须是双引号括起的 `round.sequence`。条件检查与路径预留在同一次写入准入中完成；结果未定时仍需查询路径和任务，不自动重发。它是后续迁池删除源的必要闸门，目前没有自动迁池命令。
