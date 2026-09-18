# 文件接口契约候选：原生接口与 FUSE 子集的共同语义

关联：[票据 05](issues/05-file-contract.md) · [提交契约 04](issues/04-durability.md) · [恢复与数据承诺 02](issues/02-availability-contract.md#answer) · [卷提交状态机](commit-state-machine.md) · [容量账本](capacity-reservations.md) · [任务与调度](ee-task-scheduling.md) · [资源与状态模型](ee-resource-model.md)

状态：依自动接受授权采用的首版候选，票据 05 保持 claimed。只定义语义与错误类别，不定义 Rust 类型签名、Python 绑定形态、FUSE 库选型或 errno 之外的编码。依赖 04 尚未收敛的部分在文末单列，不提前解除。

## 范围与入口

首版支持：查询（stat/状态）、枚举、读取、创建、顺序写入、重命名、删除、目录创建/删除、标准扩展属性读写。不支持：writable mmap、硬链接、原地随机覆盖、已完成文件追加、多节点同时写、跨卷原子操作。

两个入口共享同一语义模型，差别只在表达方式：

| 概念 | 原生接口（Rust LIB，Python 为其绑定） | FUSE |
| --- | --- | --- |
| 写入单元 | 显式上传任务：`put(path, source, size?, mode)` 返回任务 ID（含接管轮次前缀，见 38） | 以写方式打开的文件句柄隐式对应一个上传任务，close 结束任务 |
| 严格持久化 | `commit(task)` / `wait(task)` 返回归档结果 | `fsync`/`fdatasync` 等待覆盖当前快照的卷提交 |
| 取消 | `cancel(task)` | 无显式取消；close 前中断只结束进程侧写入，任务仍按“未完成”处理（见取消节） |
| 状态查询 | `stat(path)` 返回文件状态与版本；`status(task)` 返回任务状态 | `stat` + 扩展属性 `user.tape.state`、`user.tape.version` 只读 |
| 目录枚举 | `list(dir)` 含每项状态 | readdir 返回条目；状态经 xattr 查询 |

Python Client API 不新增语义；它暴露原生接口的同一组操作、状态和错误类别。

## 文件状态

每个路径对应一个当前版本，版本由提交时的索引代次与文件身份决定。状态取值：

| 状态 | 含义 | 可读 | 可作为写目标 |
| --- | --- | --- | --- |
| in_progress | 有活动上传任务/写句柄 | 否（其他句柄） | 否 |
| ready | 已完整接收，尚未被可靠索引提交覆盖 | 否 | 否 |
| committed | 当前版本已可靠提交并本地发布 | 是 | 仅替换 |
| partial | 上传异常结束，仅前缀已可靠提交；按 04 保留、不自动删除 | 仅原生显式读取前缀 | 仅替换 |
| unknown | 提交结果不明或视图不可信（恢复中） | 否 | 否 |

`ready` 与 `committed` 的区别对客户端可见：在 `ready` 状态返回给读者的不是“文件不存在”，而是“尚未提交”。目录枚举列出全部状态的条目，不隐藏 in_progress/ready/partial。

## 创建与替换

- 默认 `create_only`：路径已存在（任何状态）即拒绝，错误 `AlreadyExists`。FUSE 对应 `O_CREAT|O_EXCL`；普通 `O_CREAT` 无 `O_TRUNC` 打开已存在文件视为追加意图，首版拒绝（见偏移写）。
- `replace`：创建新版本；旧版本在新版本 committed 之前仍是当前可读版本，切换在新版本所在索引提交发布时原子发生。FUSE 对应 `O_TRUNC`。
- 替换失败（异常结束）时旧版本保持当前，新版本按 partial 保留且不成为当前版本；`stat` 同时报告当前版本与 partial 版本存在。
- 替换不回收旧版本占用的磁带空间；由对账/回收任务处理（37/38）。

## 写入与偏移

- 只接受从当前逻辑末尾开始的顺序写入。原生接口按流提交；FUSE `write` 的 offset 必须等于当前长度，否则 `EINVAL`（原生 `NotSequential`）。允许零长度写。
- 大小已知的原生上传在开始时按 04 预留整文件预算；未知大小按滚动额度。预算确定不足在准入或滚动申请时拒绝：原生 `InsufficientCapacity`，FUSE `ENOSPC`；校准等待超时：原生 `CalibrationTimeout`，FUSE `EAGAIN`。
- 数据流式写带，不要求先整文件落到 SSD；`write` 返回只表示接收进有界缓冲，缓冲满时背压阻塞（04）。
- 已完成（ready/committed）文件不允许再次以写方式打开追加：原生 `AppendNotSupported`，FUSE `EOPNOTSUPP`。需要修改用替换。

## 完成、fsync 与归档成功

- 原生：源数据传完并 `complete` 后任务进入 ready；`commit(task)` 请求把该文件纳入下一次卷提交并等待结果，`wait(task)` 只等待不催促。返回 `Archived{version}` 才是归档成功；`Indeterminate` 表示结果未定；其他为明确失败。
- FUSE：`fsync(fd)` 对打开写句柄触发覆盖“截至此刻有效长度”的卷提交并等待；返回 0 表示该快照已归档持久化，不表示文件已完整。`close` 只把文件标为 ready，返回 0 不表示归档成功；需要确认的应用必须在 close 前 `fsync`。`fsync(dirfd)` 提交该目录下待提交的命名空间变化。
- `fsync` 返回错误只表示这次确认失败或结果未定，不表示数据未写；应用通过 `user.tape.state` 或原生 `stat` 核对。FUSE 无法区分“结果未定”与“失败”的情形统一返回 `EIO`，细分靠 xattr。
- 同卷多个 ready 文件可共享一次提交（04）；单个 `commit` 不保证其他文件同时提交。

## 读取

- 读取只服务 committed 的当前版本。in_progress/ready 返回 `NotYetCommitted`（FUSE `EBUSY`）；unknown/恢复中返回 `RecoveryInProgress`（FUSE `EBUSY`）；partial 只有原生 `read_partial(path)` 显式读取已提交前缀，FUSE 不可读（`EBUSY`）。
- 读取任务按 38 的 H 优先级派发；随机 `pread` 允许，但性能按顺序读优化，不承诺随机读延迟。
- 读句柄不绑定执行实例：换 Leader 后，已提交文件的后续读取在恢复完成后可继续，调用方可能观察到一次 `RecoveryInProgress`。

- **元数据读的一致性（2026-09-17，来自 41）**：`stat`、`list` 由当前 Leader 用内存里的已提交视图回答，接受有界陈旧，界限为多数派确认周期：刚被取代、尚未察觉的旧 Leader 可能在这段时间内返回稍旧的目录。归档是否成功不受影响：它以卷提交为准，旧 Leader 无法提交（设备拒绝）。需要强一致的调用方用原生接口的 `wait`/`status(task)`，它们以提交事实为依据。按推荐记录，用户可推翻。

## 重命名与删除

- rename/unlink/mkdir/rmdir 是命名空间变化：受理后立即在本地视图生效，持久化随下一次卷提交；需要确认时用 `fsync(dirfd)` 或原生 `commit_namespace(dir)`。
- 对 in_progress/ready 文件 rename 或 unlink：拒绝 `EBUSY`（原生 `InProgress`）；须先取消或等待提交。
- unlink 已 committed 文件：当前有打开读句柄时允许，读句柄继续读到 close；数据在磁带上直到回收。
- rename 跨目录允许，跨卷不允许（`EXDEV`）。目录删除要求目录在当前视图为空，包括无 partial 条目。

## 取消与会话

- 原生 `cancel(task)`：停止后续接收，不回滚；已可靠提交的前缀按 partial 保留；返回时报告已提交前缀长度或“无已提交数据”。取消与完成竞争时以服务端先受理者为准，另一方得到 `AlreadyCompleted`/`AlreadyCancelled`。
- FUSE：进程被中断只影响进程；未 close 的句柄在会话到期（04 会话规则）后由服务端按“未完成”结束，等同取消。
- 会话过期后旧句柄的写与 fsync 返回 `ESTALE`（原生 `SessionExpired`）；已提交状态不受影响。
- 网络超时不代表取消成功；原生调用方在超时后必须查询 `status(task)`。

## 接管与句柄

| 情形 | 写句柄/上传任务 | 读句柄 | 客户端应做 |
| --- | --- | --- | --- |
| Leader 更换，客户端节点存活 | 全部 aborted；下一次 write/fsync 返回 `ESTALE`；原生 `status` 返回 `TerminatedRound` | 恢复完成后可继续 | 按下节核验后重传 |
| 客户端节点同时故障 | 同上，客户端重启后按任务清单核验 | — | 同上 |
| 同节点执行实例原地重启 | 同 Leader 更换（新执行实例，见 39） | 同上 | 同上 |
| 仅 FUSE 适配器重启 | 句柄失效 `ESTALE`；后端任务仍按会话规则处理 | 句柄失效 | 重开 |

不承诺内核 fd 跨节点迁移或旧句柄无感延续。

## 结果未定与安全重试

应用持有未确认任务清单与源数据（02）。收到 `Indeterminate`/`TerminatedRound`/`ESTALE` 后，在新 Leader 恢复完成后按以下规则核验，不盲目重传：

1. `stat(path)` 为 committed 且版本/长度与本次上传一致（可用应用自设的标准 xattr 辅助比对）→ 视为已完成，不重传。项目不保证能把该状态对应到某次请求，判断责任在应用。
2. committed 但版本/长度不一致（例如是旧版本）→ 用 `replace` 重传。
3. partial 或不存在 → 用 `replace`（partial）或 `create_only`（不存在）重传；重传从 0 开始，不续传前缀。
4. unknown/`RecoveryInProgress` → 等待，不重传。

首版不提供按任务 ID 的持久结果查询、自动去重或服务端幂等键；`create_only` 语义是唯一由服务端提供的重放保护。

## 错误类别与 errno 映射

| 原生错误类别 | FUSE errno | 含义 |
| --- | --- | --- |
| AlreadyExists | EEXIST | create_only 冲突 |
| NotFound | ENOENT | 当前视图确认不存在（视图可信时才返回） |
| InProgress / NotYetCommitted | EBUSY | 目标有活动任务或尚未提交 |
| RecoveryInProgress | EBUSY | 视图不可信或卷恢复中；不是不存在 |
| InsufficientCapacity | ENOSPC | 可信预算确定不足 |
| CalibrationTimeout | EAGAIN | 校准等待到期 |
| NotSequential | EINVAL | 偏移写 |
| AppendNotSupported / NotSupported | EOPNOTSUPP | 首版不支持的操作 |
| SessionExpired / TerminatedRound | ESTALE | 会话或执行实例已终止 |
| Indeterminate | EIO | 结果未定；细分靠 xattr |
| IoFailure | EIO | 明确失败 |
| CrossVolume | EXDEV | 跨卷 rename |

FUSE 不得把 RecoveryInProgress 或 unknown 翻译成 ENOENT。

## 依赖 04 尚未收敛的项

- 会话期限、校准等待上限、缓冲额度：数值留空。
- partial 前缀的具体可读范围判定依赖 D02 尾部恢复协议。
- `fsync` 快照与 time/size 触发的交错细节依赖 commit-state-machine 的 D03/PC。
- 恢复期间“可信当前视图”的判定依赖 RV01—RV05。

## 候选验证 FC01—FC10

- FC01：create_only 冲突返回 AlreadyExists/EEXIST，不创建新版本。
- FC02：replace 失败后旧版本仍可读，新版本以 partial 存在且不可作为普通读目标。
- FC03：非顺序偏移写返回 NotSequential/EINVAL，不写入。
- FC04：close 后 stat 为 ready，读取返回 NotYetCommitted；fsync 返回 0 后 stat 为 committed 且可读。
- FC05：fsync 期间换 Leader，调用返回 ESTALE/Indeterminate，随后 stat 给出 committed/partial/不存在之一，不出现“文件不存在但磁带已有数据被隐藏”。
- FC06：unlink 有读句柄的 committed 文件，读到 close 成功，之后 stat 为 NotFound。
- FC07：rename in_progress 文件返回 EBUSY。
- FC08：容量确定不足在准入返回 ENOSPC，未消耗任何预算。
- FC09：恢复期间读取返回 EBUSY 而非 ENOENT；恢复完成后返回数据。
- FC10：cancel 与 complete 竞争，只有一方成功，另一方得到对应 Already* 错误，预算不重复释放。

均未实现/执行。
