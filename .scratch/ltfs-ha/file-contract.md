# 文件接口契约：原生接口与 FUSE 的共同语义

关联：[票据 05](issues/05-file-contract.md) · [提交契约 04](issues/04-durability.md#answer) · [池化切片](pooling-slice-plan.md) · [任务与调度](ee-task-scheduling.md) · [资源与状态模型](ee-resource-model.md)

状态：**已裁定（2026-09-23，票据 05 resolved）**。2026-09-17 的首版候选写在 ltfsd 实现之前，按"先整文件接收、再批量落带"的写入模型设计了流式写、partial 状态和默认 create_only；这些与实现不符的部分已按用户裁定改写，旧版见 git 历史（`a1d163c` 及更早）。本文只定语义、线上表示和错误映射，数值（合批时长、重试窗口、暂存区额度）沿用配置项，不在此固定。

## 写入模型（已实现，契约以此为准）

- 一次上传 = 一个完整对象。服务端先把整个内容接收到执行者节点的暂存区，校验长度后标记完成，再由合批提交整批写带（`files.rs`：准入 → 接收 → 完成 → 冻结 → 落带 → 发布）。上传前必须知道长度（HTTP `Content-Length`）。
- 索引只引用完整文件，所以**单个文件不会处于"部分已提交"**。04 裁定的"保留已可靠提交的部分文件"在这个模型下只剩一种来源：接管收尾时 `TailPolicy::Salvage` 把未索引尾部登记成 `_ltfs_lostandfound/` 下的只读文件。文件状态里不再有 `partial`。
- 暂存不复制。执行者更换时：仅暂存的任务失败（需重传），已进入提交批次的任务结果未定（查询该路径判定）。
- 不提供按任务 ID 的持久查询、服务端幂等键或去重：`/tasks/<id>` 只在受理它的节点、同一进程内有效。结果未定由客户端按"路径 + 长度 + sha256"判定（见"结果未定与安全重试"）。

## 文件状态

| 状态 | 含义 | 读 | 线上表示 |
| --- | --- | --- | --- |
| committed | 当前版本已随卷提交落带并发布 | 可读 | `stat` 200，`state: committed` |
| uploading | 已准入，内容还在接收 | 读已提交旧版本（如有），否则 `not_committed` | `stat` 200，`state: uploading` |
| staged | 已完整暂存，等待合批落带 | 同上 | `stat` 200，`state: staged` |
| 不存在 | 视图可信且没有当前版本，也没有在途任务；包括被墓碑覆盖 | — | `stat` 404 |
| 恢复中 | 本节点不在服务（换届、接管、恢复） | — | 503；客户端库等待并跟随 Leader |

- `stat` 对 uploading/staged 在顶层给出在途状态与已接收长度，已提交的当前版本放在字段 `current`（没有则省略），调用方能看到"新版本在途、旧版本仍是当前"。已提交且无在途时字段在顶层，与旧格式兼容。
- `list` 默认只列已提交；`pending=1` 时附上在途路径及其状态。
- 恢复中绝不回答 404：视图不可信时只能答 503。

## 创建与替换

- `PUT /files/<path>` 默认**替换**：新版本在它所在的卷提交发布时原子成为当前版本；之前旧版本一直可读。替换不回收旧版本占用的磁带空间，由回收（reclaim）处理。
- **只创建**：请求带 `If-None-Match: *`。路径有已提交的当前版本 → 412 `exists`；路径有在途任务 → 409 `path_busy`（与普通上传相同）。客户端库 `Client::put_new`，FUSE 的 `O_CREAT|O_EXCL`。
  - 判定依据是准入时的已提交视图（本轮刚落带的 + 目录库）。已知边界：旧执行者已落带、但 `TapeCommitted` 尚未进 Raft 就失效，而新执行者又选了别的写入带时，这个文件要等那盘带下次被打开并对账后才进目录；这段时间里只创建与 `stat` 都看不到它。`ensure_write_tape` 优先已装载的带，通常新执行者会打开同一盘带。
- 同一路径同一时刻只能有一个在途任务（`VolumeState` 的路径预留）。
- 全集群的版本顺序由 `tapers.version = 轮次.序号` 给出（见 CLAUDE.md "File versions"），替换与删除都按它比较。

## 删除（墓碑）

EE 的做法（[资源模型](ee-resource-model.md)、[39](issues/39-ee-fault-recovery-evidence.md)）：删除 GPFS 里的文件并不动磁带，要等 `reconcile` 装上那盘带、改写它的索引才从带上去掉，reconcile 与写入互斥；没对账过的带导出再导入时，已删除的文件会重新出现。tape-rs 没有 GPFS，磁带是权威、目录只是缓存，而且被删文件可能在任何一盘带上，所以删除写**墓碑**而不改旧带：

- `DELETE /files/<path>`：路径没有已提交的当前版本 → 404；有在途任务 → 409 `path_busy`；否则受理，202 = 已暂存，`?wait=1` 等到落带后 200。
- 墓碑是当前写入带上的一个零长度文件 `.tapers/deleted/<sha256(路径)>`，带 `tapers.version` 与 `tapers.deletedPath = <路径>`（`ltfs::volume::tombstone_path`；用哈希做文件名，是因为被删路径的层次不能在墓碑目录里重建，`/a` 与 `/a/b` 会冲突）。它和普通上传走同一条合批落带路径，同一批次里的墓碑与上传一起发布。IBM LTFS 能读这盘带，只是把墓碑当普通空文件。
- 目录按版本取大者：墓碑版本更高则该路径不存在；之后重新上传得到更大的版本，路径复活。
- 回收时墓碑按活对象搬到目标带（很小，不做逐条判断是否仍有旧副本）。墓碑的最终清理留到后续（需要"池里不再有更旧副本"的证明）。
- 与 EE 相同的局限：单独拿出一盘旧带（不带墓碑所在的带）重建目录时，被删文件会重新出现。

## 重命名

当前写入卷内支持文件及目录原子 rename；源子树及已有目标必须属于该卷。目录目标必须为空，类型必须一致，父目录必须存在；支持 no-replace，禁止移动到自身后代或覆盖祖先。跨带或源位于另一盘读带时返回 EXDEV，不把复制加删除当作原子操作。RENAME_EXCHANGE/WHITEOUT 返回 EOPNOTSUPP。

源、目标及全部子项在同一服务锁内预留并入同一个 LTFS 索引批次；保留 native UID/extent/属性，旧路径写版本化墓碑。Client::rename / POST /rename 只以201 committed确认，结果未定不自动重放。FUSE保留源inode并更新打开句柄路径，被替换目标句柄解绑名字；改名EIO后挂载停止路径操作及写入，需核对后重挂载。此实现尚未部署到Holo。

## 读取

- 只服务已提交的当前版本。路径只有在途任务、没有已提交版本时答 409 `not_committed`。
- `GET /files/<path>` 支持单段 `Range: bytes=a-b`（206）。性能按顺序读优化，随机读可能触发重新定位。
- 读可以服务池里任何一盘带（`read_any`）；单驱动器正在写时答 503 `drive_busy`，客户端稍后重试。

## 目录

原生目录进入版本化 catalog，空目录不再依赖文件前缀。旧缓存仍可由文件前缀推导非空目录；旧介质目录记录在重新装载对账后补入。

- `GET /list?dir=<d>` 返回该目录的直接子项：文件（含长度、状态）和子目录名。不带 `dir` 时保持现有的全量列举（ltfsctl 用）。
- FUSE / Client 的 `mkdir` 和 `rmdir` 等到索引提交才返回成功。mkdir 要求父目录存在；rmdir 只允许空目录，拒绝在途的相关路径。结果未定不自动重放。
- `.tapers/` 与 `_ltfs_lostandfound/` 不出现在 FUSE 的根目录列举里；原生 `list` 照常可见。

## 完成与归档确认

| 入口 | 已暂存 | 已归档（卷提交） |
| --- | --- | --- |
| HTTP | `PUT` 202 | `PUT ?wait=1` 201，或 `GET /tasks/<id>?wait=1` |
| 客户端库 | —（库不暴露"仅暂存"） | `Client::put` / `put_new` 返回即已落带（现有行为） |
| FUSE | `close` 返回 0 | `fsync` 返回 0 |

- FUSE `close`（`flush`）把本地缓存的内容上传到暂存区后返回；被拒绝时 `close` 返回对应 errno。**`close` 成功不代表已归档**：换届时暂存会作废，文件随之消失（或回到旧版本）。需要确认的应用必须在 `close` 前 `fsync`。
- FUSE `fsync` 在写句柄上：上传截至此刻的内容并等到落带（最长约为合批的 `max_wait`，默认 60 s）；返回 0 即该内容已归档。之后继续写入的内容在下次 `close`/`fsync` 时作为新版本整体上传（替换）。`fsync` 在目录上：等本挂载发出的删除都落带。
- 结果未定时客户端库先按路径判定（下节）；仍无法判定时 `fsync` 返回 `EIO`，应用用 `getxattr user.tape.state` 核对。

## 结果未定与安全重试

应用（或 FUSE 适配器）持有源数据。上传途中连接断开、服务端答 `indeterminate`，或换届后查不到任务时：

1. 在当前 Leader 上 `stat(path)`：committed 且长度与 sha256 与本次上传一致 → 已完成，不重传（`Client::put` 已实现，`resolved_by_query`）。
2. committed 但内容不一致、或 404 → 按原模式重传（替换，或只创建）。
3. staged/uploading → 等它结束再判定；503 → 等待。
4. 可能已到达服务端的请求不会被库悄悄发到另一个节点重放。

## 接管与句柄

- 换届或执行者重启：服务端所有在途任务终止（仅暂存 → failed，已入批 → indeterminate）；读在新执行者恢复后继续。
- FUSE 是独立进程，经客户端库连接集群，可装在任何主机上；它的打开句柄不绑定某个 ltfsd 节点。换届期间的调用阻塞到客户端库的 `retry_for`，超时返回 `EBUSY`。
- FUSE 进程自身重启：打开的句柄全部失效（`ESTALE`），本地缓存里未 `close` 的内容丢弃。
- 不承诺内核 fd 迁移。

## FUSE 操作子集

| 操作 | 行为 |
| --- | --- |
| `lookup`/`getattr` | 已提交：长度、索引的修改时间；在途且无已提交版本：长度为暂存长度。文件 0644、目录 0755，属主为挂载用户 |
| `readdir` | `list?dir=&pending=1` + 本地正在写入的文件 |
| `open` 只读 | 已提交可读；否则 `EBUSY` |
| `create` / `open(O_WRONLY\|O_TRUNC)` | 本地缓存文件；`O_EXCL` → 只创建 |
| `open` 已存在文件写但无 `O_TRUNC`，或 `O_RDWR`、`O_APPEND` | `EOPNOTSUPP` |
| `write` | offset 必须等于当前长度，否则 `EINVAL` |
| `flush`（close） | 上传到暂存区（见上） |
| `fsync` | 上传并等落带（见上） |
| `unlink` | `DELETE`；在途 `EBUSY`，不存在 `ENOENT` |
| `rename` | 当前写入卷原子改名；跨带 EXDEV；其他规则见“重命名” |
| `mkdir`/`rmdir` | 见"目录" |
| `truncate` | 只允许截到 0 且文件正以写方式打开；其他 `EOPNOTSUPP` |
| `chmod`/`chown`/`utimens` | `EOPNOTSUPP`（`cp -p` 会告警但继续） |
| `getxattr` | 只读：`user.tape.state`、`user.tape.version`、`user.tape.barcode`、`user.tape.sha256`，以及索引里文件自带的 xattr |
| `link`/`symlink`/可写 `mmap` | `EOPNOTSUPP`（只读 `mmap` 由内核经 `read` 支持） |

## 错误映射

| HTTP | 原因 | 客户端库 | FUSE errno |
| --- | --- | --- | --- |
| 404 | 视图可信且不存在 | `NotFound` | `ENOENT` |
| 409 `path_busy` | 路径有在途任务 | `Rejected` | `EBUSY` |
| 409 `not_committed` | 读在途文件 | `Rejected` | `EBUSY` |
| 412 `exists` | 只创建冲突 | `Exists` | `EEXIST` |
| 400 `bad_path` | 路径非法 | `Rejected` | `EINVAL` |
| 503 `not_serving`/`switching_tape`/`drive_busy` | 恢复中、换带、驱动器忙 | 库内等待重试；`retry_for` 到期 `NoLeader` | `EBUSY` |
| 503 `read_only` | 卷受限只读 | `Rejected` | `EROFS` |
| 507 `too_large` | 比一盘带还大 | `Rejected` | `EFBIG` |
| 507 `no_tape` / `insufficient_capacity` | 池里没有可写带 | `Rejected` | `ENOSPC` |
| 500 `indeterminate` | 结果未定且按路径无法判定 | `Indeterminate` | `EIO` |
| 500 `failed`/`spool_io` | 明确失败 | `Rejected` | `EIO` |

恢复中与未定状态不得映射为 `ENOENT`。

## 实施状态

- **F1（2026-09-23）已实现**：`FileService::begin_with`（只创建）、`stat_full`、`list_dir`；HTTP `If-None-Match: *` → 412、`/stat` 在途状态、`/list?dir=&pending=1`、单段 `Range`（206/416）、只在途路径读取 409 `not_committed`、不存在 404；客户端库 `put_new`、`stat_path`、`get_range`、`list_dir`，结果未定时遇到在途状态先等它结束再判定；`ltfsctl put --new`、`stat` 显示状态、`list --dir [--pending]`。FC01/02/09/10/11 由 `ltfsd_cluster.rs::file_contract_create_only_in_flight_state_range_and_directories` 覆盖。已知的简化：`Range` 仍是整文件读出后切片（`read_file` 256 MiB 上限不变），`list_dir` 取全量再按前缀过滤；两处都标了 TODO，F3 FUSE 做大文件读与大目录时要换成按块读和目录库前缀查询。
- **F2（2026-09-24，Codex 接续）软件链路已接通**：DELETE HTTP、客户端和 ltfsctl；删除响应丢失不自动重放。模拟器测试覆盖提交/在途拒绝/接管/重新上传，以及墓碑回收后继续隐藏旧文件。EE/Holo 场景未在本轮执行。
- **F3（2026-09-24）挂载层已接通，尚未完整验收**：前台 `tape-fuse`、原子 O_TRUNC、直接 I/O、关闭/同步、目录快照、删除和只读 xattr。内存后端的实际内核挂载测试通过。打开读句柄时取本地内容快照，替换/删除后仍可读。缓存使用独占私有子目录。未完成项：按 Range 只读所需磁带块、内核 mmap 和实际 mv/故障切换/Holo 联合验收。详见 `docs/file-client.md`。

## 验证 FC01—FC12

| 编号 | 内容 | 层 |
| --- | --- | --- |
| FC01 | 只创建遇到已提交路径 412，不产生新版本；遇到在途路径 409 | 模拟器集群 |
| FC02 | 替换在途时旧版本一直可读，落带后切到新版本 | 模拟器集群 |
| FC03 | FUSE 非顺序写 `EINVAL`，已存在文件无 `O_TRUNC` 写 `EOPNOTSUPP` | FUSE 适配器（模拟后端） |
| FC04 | `close` 后 `stat` 为 staged、读 `EBUSY`；`fsync` 返回 0 后为 committed 且可读 | 两层 |
| FC05 | `fsync` 期间换届：返回 0（经路径判定）或 `EIO`；之后 `stat` 为 committed 或不存在，不会"磁带有数据但被隐藏" | 模拟器集群 + Holo |
| FC06 | 删除后 `stat` 404；装入被删文件所在的旧带并对账后仍 404；重新上传后路径复活 | 模拟器集群 + Holo |
| FC07 | 删除在途路径 409；删除不存在路径 404 | 模拟器集群 |
| FC08 | 回收一盘带着墓碑的带后，被删路径仍不存在 | 模拟器集群 |
| FC09 | 恢复中 `stat` 503，不回答 404 | 模拟器集群 |
| FC10 | `Range` 读返回正确片段 | 模拟器集群 + Holo |
| FC11 | `list?dir=` 只返回直接子项；FUSE `mkdir`/`rmdir` 规则 | 两层 |
| FC12 | FUSE `mv` 经 `EXDEV` 退回复制 + 删除，结果路径内容一致、源路径不存在 | FUSE + Holo |

## 与 EE 的对照（切片实施时先在实验室观察）

- 删除：EE 删除 GPFS 文件后 `eeadm file state` 与磁带索引的变化、`reconcile` 何时去掉带上副本、未对账带导入的表现。
- 覆盖写：EE 覆盖一个 migrated 文件后旧副本在带上的状态（预期为"已失效、待 reclaim"），对照 tape-rs 的版本 + 回收。
- 只读访问 migrated 文件时的召回与 `EBUSY`/阻塞行为，对照 FUSE 读在途文件的 `EBUSY`。


### 大文件下载接续（2026-09-24）

HTTP GET 改用 `ExecRequest::ReadTo`：设备线程逐块写入 `FileService` 暂存目录中的匿名文件，读取成功后交还文件句柄给 HTTP 线程，再流式发送。去掉原来 256 MiB 的限制和整文件 Vec；慢客户端不会继续占用设备线程。临时磁盘空间仍随文件大小和并发数增长，Range 暂时从完整暂存文件切片；不宣称已完成按范围磁带定位优化。底层读取提前遇到文件标记或长度与索引不符时返回错误，HTTP 读失败答 500，不伪装成 404。客户端等待落带收到 202 时继续查询，不重新上传。


### 文件元数据透传接续（2026-09-24）

索引 `modify_time` 与原始 xattr 经 CatalogPart、SQLite、FileService、HTTP stat 和 Client 到达 FUSE；另传递原有 `(round, seq)` 文件版本。FUSE 的 mtime 使用索引修改时间，`user.tape.version` 返回版本，其他文件属性只读透传，Base64 在读取 xattr 时解码（损坏返回 EIO）。内置属性优先，目录时间和未知时间仍为 epoch。

旧日志缺少 metadata 时解为 null，旧 files/staging 表添加 metadata 列并默认 null；迁移不伪造旧记录未缓存的属性。旧库中的版本列正常提供。索引格式未变。新旧节点混跑、回退与回收重写时元数据保留不在本轮验收范围；统一升级与旧缓存补齐边界见 `docs/file-client.md`。


### 回收保留元数据（2026-09-24）

回收读取源索引后，内部上传任务携带 NodeMeta 与原始 xattr；新副本写入保留时间、只读标记和属性编码，重新分配卷内 UID 和 tapers.version，按实际数据重算已有 MD5/SHA256。普通用户上传不接受这组内部元数据。源带格式化的原有双重检查保持不变。

模拟器回归先复现回收后 mtime 被重置，再验证修复后的时间/自定义属性、内容、版本递增、源带清空和接管后一致性；卷级测试另覆盖二进制 xattr、只读标记、空文件和旧哈希不被照搬。尚未完成 Holo/IBM 联合演练。


### 本机 mv/mmap 验证（2026-09-24）

首次实测 Python 只读共享 mmap 因 direct I/O 默认关闭共享映射而返回 ENODEV；增加可选 FUSE_DIRECT_IO_ALLOW_MMAP 能力协商后通过。不支持该能力的内核告警，保留普通读写；读写混合打开仍 EOPNOTSUPP。GNU mv 9.4 回退复制/删除成功，模拟目标暂存 ENOSPC 时源文件保留。两个显式 FUSE 测试在 Linux 6.8.0-124-generic 上通过，仅内存后端，不代表 Holo 联合验证。mv 成功不提升 close 的持久化保证。

实验室只读核对：EE node list 三节点 available，task list 无活动任务；tsclient 内核 5.14.0-427.20.1.el9_4、glibc 2.34，非交互 PATH 有 fusermount，没有 fusermount3/cargo/rustc，未运行 ltfsd；EE1 则仍运行旧 ltfsd（节点 1、7400/7401、TAPERS 的两个驱动器和换带器），其 status.json 自述池 rc、TS1000L08/TS1001L08。文件自述不是实时介质库存，不能据此直接格式化或接管。没有部署、停服、挂载远程文件系统或向设备发命令。

### 页缓存契约调整（2026-09-24，用户根据 LE 实测授权实施）

FUSE 本挂载同 inode 改为共享可变文件：写入在 fsync/close 前对本地读取可见；覆盖和截断影响原读句柄及映射。删除后旧引用仍指向被删文件，同名创建新 inode。此项取代前文每读句柄快照语义。HTTP/其他客户端仍只读取服务端已提交内容。

启用内核页缓存 + write-through，不启用 writeback-cache；暂不设置 KEEP_CACHE，每次 open 由内核使旧缓存失效。仍只允许顺序写、整体替换和只读 mmap。写句柄 fsync 等待归档的保证不变。尚未上传的本地数据通过 user.tape.state=local 区分；本地可见不表示归档成功。

已有引用的本地文件如在新 open 时检测到远端内容变化，返回 ESTALE，关闭全部相关句柄/映射后重开刷新；不是分布式强一致缓存。首次下载核对长度和已有 SHA256，发现 stat/GET 内容不一致时 EAGAIN。无哈希记录额外核对前后 stat，仍不能给出密码学内容证明。

## LE 写入接口对齐增量（2026-09-24）

本节替代上文 FC03 的旧限制：允许 O_RDWR、非截断写打开、随机覆盖、O_APPEND 与任意 truncate。多个写句柄共享 inode 内容及上传状态；非截断写首次取回完整已提交内容，追加偏移由内核 write-through 路径串行计算。FC03 验收改为多写句柄共享修改、关闭一方后另一方继续、洞补零和扩大/缩小截断。仅本机验证，未部署；close/fsync 契约及其他尚未实现的接口仍按前文，不宣称全部与 LE 一致。现场目标行为见 le-capabilities-holo-20260924.md。

### 2026-09-24：命名空间准入前置修复

为后续持久化目录和 rename 接入补齐文件路径互斥：PUT、DELETE 与回收墓碑的准入会检查在途的同名、祖先和后代路径；兄弟路径仍可并行。文件占用祖先路径时，PUT/DELETE 返回 409 `not_directory`；目标已有活文件后代或当前卷的原生目录节点时，返回 409 `is_directory`。FUSE 分别映射 ENOTDIR / EISDIR，在途冲突仍为 EBUSY。NUL 路径在准入前拒绝。无索引 XML、SQLite schema 或客户端签名变化；这些原先可能被错误准入的请求现在会提前失败。

目录库按池和路径分量查询同名、祖先、后代文件，用路径范围查询，路径里的 `%` / `_` 不作通配符。查询不持服务锁；重新获得锁后核对同一个 VolumeState 实例（换带可能不换 round），用当前卷 recent 记录覆盖查询期间已发布的结果，再做预留。数据库不可读则拒绝准入，不把未知当作不存在。回收墓碑仍允许搬迁已删除路径，但受同样的父子路径在途互斥约束。

边界：这是文件命名空间安全检查，**不是目录持久化完成**。其他磁带上的空目录仍不在 catalog 中；FUSE mkdir/rmdir 仍为此前的内存行为，rename 仍返回 EXDEV。下一阶段需要原生目录的版本化 catalog、目录操作的提交/接管与回收，以及 FUSE/client 的目录接口，然后才能安全接通 rename。

软件验证：`tests/file_namespace.rs` 覆盖并发父子路径、兄弟路径、原生空目录、NUL、跨带目录记录、SQL 通配字符以及 recent 墓碑遮蔽旧目录库；`namespace_conflicts_across_tapes_and_takeover` 覆盖真实 Client/HTTP 与模拟三节点接管。全套 `cargo test` 180 通过、1 忽略，`cargo check --all-targets` / `cargo clippy --all-targets --all-features` 成功，clippy 保留既有告警；全局 fmt 仍有历史差异。本轮未部署或访问 Holo/真实硬件。


### 2026-09-24：持久化目录已接通（软件验证）

取代上一节“目录仍仅内存”的状态：mkdir/rmdir 经 FileService 准入、VolumeState 冻结、执行线程标准 LTFS 增量提交，再发布目录记录和任务结果。Core 发布目录节点而非伪造空文件。catalog metadata.kind 标记 directory，并保存重定位所需的原生时间/readonly/xattrs；回收按原生目录搬迁，源带目录与墓碑均进入格式化证明。读取旧带对账不会覆盖其他带的新目录/墓碑；服务开放也排除已被其他带取代的记录。父子路径互斥覆盖目录创建、删除及回收。

HTTP POST/DELETE /directories/<path>，Client::mkdir/rmdir；FUSE 移除 mem_dirs。普通 list 仍仅文件，list_dir 包含空目录，stat 返回目录类型和原生修改时间。目录操作的 201 才是提交确认；202、传输中断和提交结果未知返回 Indeterminate。仅明确 failed（未落带）才重试。新旧二进制不能混跑；旧磁带先装载对账再执行目录写操作。本轮未部署 Holo，rename/符号链接创建/可写 xattr 等差距仍待后续阶段。


### 2026-09-24：当前写入卷原子 rename 已接通

取代前文历史进度中“rename始终EXDEV”的状态。Client、HTTP、FUSE使用同一次源/目标索引提交；目录子项逐项发布和墓碑防复活。跨带仍EXDEV，尚未完成池内任意磁带之间的原子改名。软件测试覆盖准入、响应丢失不重放、源/被替换目标打开句柄、目录inode保持、Leader接管、UID/extent/mtime/xattr保留、增量提交故障和模拟断电；实际本机FUSE验证独立报告，不冒充Holo实测。

### 2026-09-24：LE 改回旧路径的兼容规则

LE 往返实测通过，详见 `holo-rename-20260924.md`。外部 LTFS 可能保留 tapers 私有墓碑并把原生节点改回被删除过的名称。同一恢复索引内出现两者时，以实际原生节点为准；节点版本不提升，其他磁带的较新记录仍按现有版本规则处理。不存在原生节点的墓碑继续有效。本规则覆盖文件与目录，无须改写外部索引。rename版本尚未部署原集群。


### 可写 xattr 软件实现（2026-09-24，尚未部署）

取代历史“可写 xattr 不支持”的状态：Client、HTTP、FUSE 已实现当前写入卷文件/目录的 user 属性创建、替换和删除，经原生 LTFS 增量索引持久化。二进制值使用 Base64；不重写文件数据，保留 UID/extent/mtime/内容哈希。CREATE 冲突 EEXIST，REPLACE/删除缺失 ENODATA，最大值 64 KiB。其他卷 EXDEV；根目录虚拟控制属性未开放，内部 tape/tapers/ltfs 属性禁止修改。未定结果不重放，FUSE 阻止后续写回。细节与软件验证范围见 docs/file-client.md；实际内核 xattr 和 Holo/LE 回写尚待验收。


### 可写 xattr 隔离验收（2026-09-24）

本机内核属性调用与 Holo/LE 双向回写已通过，取代上一条“尚待验收”。现场发现空 Base64 值显式闭合元素导致 IBM -5030，公共序列化改为自闭合元素；先红后绿回归和真实 LE 挂载均通过。LE 降级到2.4.0 gen58后，文件/目录属性替换、创建、删除、空值均由tape-fuse读回。原集群已恢复旧版本；可写属性尚未正式部署，限制不变。详见holo-writable-xattr-20260924.md。


### 可写 xattr 已发布（2026-09-24）

原三节点统一部署验收版，原带专用目录写入、计划停机接管、重挂载读回、清理及原文件校验通过，取代上文“尚未部署”的历史状态。详见holo-writable-xattr-upgrade-20260924.md；当前写入卷/内部属性/根目录限制不变。

### 2026-09-25：已有符号链接读取

catalog仅对链接增加metadata.kind=symlink及metadata.symlink；FUSE lookup/getattr报告S_IFLNK和目标字符串字节长度、readlink返回原目标，内核负责相对目标/链接链/悬空/循环。不得通过取消下载长度检查来伪装普通文件。目录快照对非目录项补查getattr，正确报告链接d_type（消失条目跳过，其他错误返回）。需要服务端升级并挂载对账，旧服务端目录不含目标；不改变LTFS格式或SQLite schema。创建链接仍EOPNOTSUPP。本机内核及Holo只读验收见holo-symlink-20260925.md；原集群尚未升级。

2026-09-25部署补充：已有符号链接读取已统一发布到原集群与客户端，正式目录tape-rs-symlink-prod-20260925；详见holo-symlink-upgrade-20260925.md。前段“原集群尚未升级”是隔离验收时状态，已被本条更新。

### 2026-09-25：符号链接创建隔离验收

工作树新增 Client::symlink(target,path)、POST /symlinks 和 FUSE symlink 回调。复用元数据队列及增量提交，预留链接路径，检查父目录、同名冲突和文件数限制，无数据 spool；target 不解析，仅检查非空和 XML 字符有效性。提交成功后再返回；Client 未知结果不重放。Holo RT2502L8 从 LE Full85 写至99，LE副本读取、重启/新缓存、链接改名删除及通过链接覆盖新测试目标均通过。上文“创建仍EOPNOTSUPP”仅适用于当前正式部署版本；创建功能尚未部署原集群，硬链接仍EOPNOTSUPP。见holo-symlink-create-20260925.md。

2026-09-25部署更新：创建版正式部署完成，原三节点和tsclient使用tape-rs-symlink-create-prod-20260925，专用测试目录创建/写入/改名/删除/接管重挂/清理通过。原带72→95 Complete，测试墓碑保留，原文件内容和元数据不变，/rc父目录元数据正常更新。详见holo-symlink-create-upgrade-20260925.md。

### 2026-09-25：链接跨带回收隔离验收

原回收走普通文件读取，单悬空链接可复现abandoned。修复将链接作为元数据迁移，不解析目标、不复制extent；保留时间/readonly/xattrs，目标卷分配UID并更新version，沿用路径互斥和源带格式化前检查。206项软件测试通过；新SR2501L8→SR2502L8实际回收、源带重新格式化、链接内容/元数据/新缓存/接管验收通过。尚未正式部署；详见holo-symlink-reclaim-20260925.md。
