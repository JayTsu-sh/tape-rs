# 跨节点接管与 LTFS 持久化的事实依据

研究日期：2026-09-14。范围：为 Wayfinder 决策提供证据，不替代部署验证或选择架构。仅阅读官方资料；未访问真实磁带、远程主机或设备节点。

## 1. 所有权、仲裁与隔离

**已核实：**Pacemaker 将 fencing 定义为：即使节点不响应集群命令，也能使其无法继续运行资源。断电或切断资源访问是实现方式；仅失去心跳不能证明旧节点已停止。仲裁判断谁可行动，隔离负责阻止旧所有者继续行动，两者不能互相替代。[Pacemaker 3.0：Fencing，§8.1–8.3](https://clusterlabs.org/projects/pacemaker/doc/3.0/Pacemaker_Explained/html/fencing.html)

两个各有一票的节点按普通多数票需要两票；Corosync 的 `two_node: 1` 将 quorum 人为设为一，并默认启用首次启动等待所有节点的规则。这能允许单节点继续服务，但不能单独证明另一节点已失去写入能力。[Corosync 官方 votequorum 手册](https://github.com/corosync/corosync/blob/main/man/votequorum.5)

**推论：**本机 `flock`、owner 文件、过期 lease 都不能约束仍可发出 SG_IO 的另一主机。后续决策必须交代独立仲裁／确定性胜者机制，以及成功接管前可验证的隔离证据。使用第三方见证也不免除 fencing；见证的故障域需单独列明。

IBM 的历史 LTO SCSI 手册提供 PR 的 Exclusive Access、抢占及中止语义，并按驱动代际列出支持范围；这证明设备级排他机制存在，不能证明本仓库的 LTO-8 和 TS4300 机械手支持同一策略。[IBM LTO SCSI Reference，Persistent Reserve Out](https://publibfi.boulder.ibm.com/epubs/pdf/a3204509.pdf)

**待核实：**每个 drive/changer LUN 的 PR 类型、所有 initiator 路径、掉电后保留行为、抢占后在途命令结果、旧节点能否重新注册。仅注册 key 不等于获得独占权。若选择 PR fencing，必须覆盖实际可能修改磁带位置、介质和库存的访问路径。

## 2. LTFS 提交边界不能由 MAM 推定

**已核实：**LTFS 2.5.1 §4.1.4 的 consistent state 要求两分区完整及正确回指；规范性 Annex C.4 的 `ltfs.sync` 只要求数据与索引落带后能够无损恢复，不要求当时已 consistent。§10.3 的 VCI 可选，使用时须按规定编码和顺序更新；VCR 从介质读取，不能自增充当应用事务号。§9.2.19 的 advisory locking 是协作式写保护，不能阻止其他应用修改。依据：[SNIA LTFS 2.5.1，§4、§9.2.19、§10、Annex C](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)。

**推论：**之前“P0/P1 index 和 MAM 全部更新才算提交”不能称为规范的唯一完成条件。产品可以选择更严格的承诺，但必须区分“可恢复的持久化”“卷已一致”“快速挂载所需提示有效”。不得将 MAM 提示失效直接判为数据丢失，也不得仅凭旧 catalog 或节点日志判定写带成功。恢复状态机需核对卷身份、索引有效性、分区位置和提交证据，并对每个中断点作故障注入；具体算法仍待决策。

## 3. 同步 staging 的承诺有故障边界

**已核实：**以 DRBD Protocol C 为例，同步复制要求本地和远端磁盘写入确认后才报告写完成；文档承诺单节点丢失不丢数据，同时明确所有存储副本毁坏仍会丢失。[LINBIT DRBD 9 用户指南：Replication modes](https://linbit.com/drbd-user-guide/drbd-guide-9_0-en/)

**推论：**此处是机制证据，尚未选用 DRBD。“同步”不能只指发送成功或远端内存收到。应用的确认点需要同时覆盖数据、命名元数据和恢复日志的持久化顺序，且要确认断网降级时是否继续接受写入。两个进程共享同一电源／存储控制器不构成独立故障域。跨节点零丢失与磁带介质独立可恢复是不同承诺；必须分别定义 staging ACK 和 tape commit ACK。

## 4. FUSE 进程恢复与跨节点恢复

**已核实：**常规 FUSE connection 属于本机内核与 daemon；连接 abort 后，在途和新请求返回错误。[Linux FUSE Overview，§1.1、§1.5](https://www.kernel.org/doc/html/latest/filesystems/fuse/fuse.html)

Linux 已合入 `FUSE_NOTIFY_RESEND`：由另一个 daemon 保留原连接 fd，新 daemon 接管并重发在途请求。源码明确要求请求幂等或防止非幂等操作重复执行。这是有额外实现条件的机制，不能泛称“重启 daemon 即可恢复”。[Linux 官方提交：FUSE resend](https://github.com/torvalds/linux/commit/760eac73f9f69aa28fcb3050b4946c2dcc656d12)

**推论：**保留原 fd 的机制不能把已失效节点的内核挂载和文件描述符搬到另一节点。若 FUSE 客户端所在节点仍存活而仅远端 LTFS 服务失败，则可研究保持本地挂载、让 adapter 重连新主；若应用随节点一起故障，则需应用重启和任务恢复。两者必须分开验收。Rust SDK 也需要稳定操作标识、结果查询与去重，解决“服务完成但成功响应丢失”的重试歧义。

## 5. RTO 是待测指标

**已核实：**IBM 给出的 LTO-8 指标包括约 15 秒 nominal load-to-ready、约 59 秒平均定位／回卷；它们不是所有磁带与驱动状态下的上限。[IBM：LTO performance specifications](https://www.ibm.com/docs/en/ts4500-tape-library?topic=performance-lto-specifications)

**推论：**60 秒控制面接管和 5 分钟恢复服务可作为目标，尚无数据证明。计时需明确是否包含故障检测、隔离、设备命令退出、日志恢复、索引验证、机械定位及客户端重试；目录可用、首次磁带数据返回、恢复写入应分别记录。需要长扫描或驱动异常恢复时可能超过目标，不能通过缩短 SCSI 超时证明达标。

## 部署前仍须得到的事实

- 两节点实际 FC/SAN 可达性、稳定设备身份、drive/changer 控制路径与独立 fencing 设施。
- staging／journal／catalog 的复制或共享存储配置、持久化能力、故障域和降级写策略。
- 目标内核、FUSE 库及恢复机制支持；应用部署位置、句柄失效和重试要求。
- 干净卷、提交中断、VCI 失效、断网脑裂、旧主恢复等场景下的接管时间与数据证据。

本报告只消除规范与机制层面的事实疑问；硬件能力、产品提交契约及可承诺的 RTO 留给后续决策票据。
