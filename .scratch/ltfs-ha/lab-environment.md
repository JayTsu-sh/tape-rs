# EE 对照实验室环境

关联：[地图](map.md) · [EE 架构参考](issues/14-ibm-ee-architecture.md) · [部署与隔离验证](issues/09-fencing-deployment.md)

来源：用户提供的环境说明，以及随后经授权对 PVE 和 VM 514 的只读查询。用户报告当前环境已支持 IBM Storage Archive EE 实验室文件迁移与召回；本次未复跑该工作流，不表示本项目已部署或 HA 已通过验收。用户明确允许后续带库调研利用此环境；涉及状态变化的实验仍需单独界定目标和授权。

## 已提供组件

| 组件 | 地址 | 用户说明的角色与版本 |
| --- | --- | --- |
| PVE | 10.131.9.20 | 虚拟化宿主，PVE 9.1.6 |
| Holo-VTL | 10.131.9.70 | 虚拟带库、机械手、LTO-8 驱动器 |
| EE1 | 10.131.9.71 | EE 主控节点、GPFS 节点 |
| EE2 | 10.131.9.72 | EE 数据节点、GPFS 节点 |
| 测试客户端 | 10.131.9.73 | 带库及文件客户端测试 |
| EE3 | 10.131.9.74 | VM 514；用户提供 8 vCPU / 12 GiB / 100 GiB，已由 PVE 配置核对；EE 具体角色仍待核验 |

EE 实际安装版本、Holo-VTL 版本/后端、驱动器数量、介质清单、连接协议、多路径及各节点可见设备集合仍未知，不从名称或相邻 IP 推定。PVE 的 root SSH 入口已获本轮授权并实际使用；EE3 内部 SSH 访问方式尚未提供。指南中的版本不等于现场安装版本。凭据不保存在本文或地图中。

## 已核验的 EE3 基线

本轮经 PVE `10.131.9.20` 的配置查询和 QEMU Guest Agent 查询获取以下结果；未通过客体代理执行来宾 shell。

| 项目 | 实际观察 | 证据来源 |
| --- | --- | --- |
| PVE | 主机名 devops；pve-manager 9.1.6；内核 6.17.2-1-pve | hostname、pveversion |
| 虚拟机 | VM 514，名称 ee3，running | qm status/config |
| CPU/内存 | 1 socket × 8 cores；12288 MiB；balloon=0 | VM 配置，不代表独占物理资源 |
| 系统盘 | scsi0，100G，pool_1t；virtio-scsi-single | VM 配置，不代表另有磁带直通 |
| 客体系统 | ee3.ee.lab；Rocky Linux 9.4 (Blue Onyx)；内核 5.14.0-427.20.1.el9_4.x86_64 | get-host-name、get-osinfo |
| eth0 / vmbr0 | 10.131.9.74/20 | Guest Agent 网络信息与 VM 网卡 MAC 对应 |
| eth1 / vmbr1 | 10.10.1.74/24 | 同上 |
| eth2 / vmbr2 | 10.10.2.74/24 | 同上 |
| 客体代理报告的文件系统 | /boot/efi 为 vfat；/boot 与 / 为 xfs | get-fsinfo；不据此断言 GPFS 未安装或绝无其他挂载 |

网络角色未核验，不把三块网卡直接命名为管理、GPFS 或带库专网。其对应 bridge 已核对，不代表各网络的路由、隔离、带宽或共享故障域已验证。

**实际访问限制。** 尝试使用 `qm guest exec 514` 查询操作系统、软件包及服务时，代理明确返回 `Command guest-exec has been disabled: the command is not allowed`。未启用该功能、修改策略或尝试绕过；随后仅使用已允许的 get-osinfo/get-fsinfo/get-host-name/network-get-interfaces 查询。因此 EE/GPFS 包版本、运行服务、磁带路径及介质可见性仍未核验，不能将“无法查询”当成“没有安装”。

本轮使用的只读入口是 PVE 的 hostname/pveversion、qm status/config（输出字段白名单）及上述代理信息查询。未改 VM/EE 配置，未启动迁移/召回，未挂载/卸载文件系统，也未向磁带或机械手发命令。后续深入 EE 软件及带库映射时，应使用另行明确的来宾只读 SSH 入口或用户提供的配置/诊断材料，不以修改代理安全策略作为默认步骤。

## 对项目地图的影响

- 增加可供对照的 EE 工作流环境，后续可研究迁移/召回的操作边界、调度及可见结果；不照搬 GPFS、共享目录权威、DMAPI 或 HSM 架构。
- 不将 EE1/EE2/EE3 自动认定为本项目三个 Raft 成员，也不将测试客户端自动改为第三个控制节点。角色复用、资源隔离和软件部署均需后续明确。
- 不将迁移/召回可用直接视为三节点设备共同可达、旧执行者可隔离、陈旧管理请求已收束、LTFS 增量链或断电持久化已验证。
- Holo-VTL 可提供哪些故障注入、持久化与设备行为须先核对。其结果按虚拟实验室证据记录，不直接替代物理磁带/驱动器的持久化或全路径隔离证明。
- 所列虚拟机是否共享同一 PVE 宿主、网络、VTL 后端和存储仍待核验；不能因存在三个节点名就宣称有三个独立故障域。
- 仓库 CLAUDE.md 另记载 10.128.54.118 的历史 TS4300/FC 环境。本次不假定两者相同、相通或新环境已替代旧环境，不复用其中设备路径和远程命令。

## 后续只读调研的入口与边界

[SQLite 控制库基线](control-store-sqlite.md#打开恢复与物理假设)另要求核验实际链接版本、配置、本地文件锁及 guest 文件系统到虚拟磁盘/宿主缓存的同步链路。已有 XFS、系统盘大小和 PVE 版本信息不证明掉电可靠性；本轮未创建控制库、测锁、改变缓存或执行宿主/来宾故障注入。
[许可时钟契约](runtime-clock.md)已明确：现有 PVE/EE3 配置查询不证明 guest BOOTTIME 在 VM 暂停、迁移、快照恢复中的许可连续性。后续资格化须单独记录 host/guest、clocksource、namespace、生命周期约束和旧副作用封闭证据；本轮未运行时钟测试或更改 VM 策略。

用户已补齐 EE3 地址与资源，并授权经 PVE 查看其环境；本轮只读观察见上节。后续调研优先复用这套实验室，按具体问题选择最小访问范围，不要求将密码或私钥写入研究资料。不因拥有 PVE 管理入口就推定可修改所有虚拟机或接管现有 EE 设备。

获得明确范围后，优先核对以下现有资料与软件可见信息，不先操作设备：

1. 组件版本、主机/虚拟机归属、已配置网络与存储映射；区分用户说明、配置声明和实际观察。
2. EE/GPFS 角色配置、已有迁移/召回日志及任务状态；只取验证需要的字段，不收集无关文件内容或凭据。
3. 已有设备映射、库存/卷列表及共享访问关系；缓存清单不当作实时物理状态，未经核验不执行所谓“只读”的 SCSI 探测。
4. 是否存在可独占的测试设备组/介质、可支持的隔离设施及故障注入入口；这里只调查能力，不执行预留抢占、断链、重启、暂停 VM 或存储回滚。

任何新的迁移/召回试验、写带、挂载/卸载、设备抢占、服务或 VM 操作另行明确目标、影响及授权；不得让 tape-rs 与正在工作的 EE 实例并发争用同一设备组。

现场配置核对与后续 09 的隔离验证分别记录。本资料补充不解除 09 对 18 的依赖，也不将未完成的协议或现场能力标为已验证。

## Holo-VTL 的可信度与 EE 交叉验证（用户提示，2026-09-17）

用户指出 Holo-VTL 模拟器本身不一定完全正确；但 IBM EE 在该实验室上已能正常迁移/召回，因此 EE 的行为是校验 Holo-VTL 忠实度的参照。实机验证按三层组织，每层的结论只在其层内有效：

1. **本仓库 SimTransport**：只证明逻辑一致性，不证明设备语义。
2. **Holo-VTL**：先用 EE 校准。方法：a) 让 EE/LE 在 VTL 上写一盘磁带，用本项目 `LtfsVolume::mount` 只读恢复，核对末索引、回指链、VCI 布局与 EE 的 `eeadm tape list`/`file state` 一致；b) 用本项目在 VTL 上 mkltfs+commit 一盘磁带，再由 EE `tape import` 只读导入，核对文件与索引可被 EE 接受；c) 对反向 SPACE、READ POSITION、EOD 报告、WRITE FILEMARKS(0) 屏障这类本项目恢复协议依赖的命令，对比 EE 在同一 VTL 上留下的痕迹（`ltfs.log`、`ltfsee_trc.log`）与本项目的命令日志。VTL 与 EE 不一致时以 EE 观察为准并记录 VTL 偏差，不据此修改协议。
3. **物理 TS4300/LTO-8**（10.128.54.118）：掉电与屏障持久化只有这里能证明。

任何一层通过都不替代上一层；接管验收（08）的 TA02/TA03/TA10 至少要在第 2 层跑通并经 EE 校准后才有意义。

## 已核验的 SSH 入口（2026-09-17，用户提供 PVE root 口令后只读核验）

- 入口链：本机 → PVE `root@10.131.9.20`（口令，不记录）→ 五台实验室 VM `rocky@<ip>`，使用 PVE 上 `/root/.ssh/id_ed25519`（`root@devops`）免密；`rocky` 拥有免口令 sudo。直接以 root 登录 VM 被拒绝（cloud-init 用户为 `rocky`）。
- 五台 VM 均为 Rocky Linux 9.4；ee1/ee2 的 cloud-init 公钥来自 VM 101（ubuntu-24c）root，但 PVE 的 ed25519 私钥同样可登录（应是在客体内另外授权过）。
- VM 配置事实：holo-vtl（510）挂 2T 数据盘（pool_50t，虚拟磁带存放）；ee1/ee2（511/512）各挂两块 100G 数据盘（serial ee1-data1/2、ee2-data1/2）和一块共享 10G 盘 `ee-tiebreaker`（GPFS 仲裁盘）；ee3（514）只有系统盘；tsclient（513）只有系统盘。三网卡：vmbr0=10.131.9.x/20，vmbr1=10.10.1.x/24，vmbr2=10.10.2.x/24。
- `qm guest cmd network-get-interfaces` 本轮无输出（代理策略），IP 以 cloud-init `ipconfig0` 为准。

后续在 VM 内的任何非只读操作（安装 tape-rs、装载磁带、写带）仍须逐项说明目标与影响；不与运行中的 EE 争用同一设备组。

## 实验室只读采集结果（2026-09-17）

| 主机 | 观察 |
| --- | --- |
| holo-vtl | 服务 `holo-control-plane.service` + `tcmu-runner.service`（LIO 用户态直通）；`/opt/holo`；虚拟磁带存放于 `/dev/sdb` 2T 挂载在 `/var/lib/holo/storage-pools/pool1`（已用 15G） |
| ee1 | gpfs 5.2.3-6、ltfsle 2.4.8.3；`eeadm node list`：3 个节点 available，ee1 为活动控制节点，每节点 1 台驱动器，节点组 G0，库名 LISA4300；`drive list`：IBMlisa41D6C（节点 1）、IBMlisa42299（节点 2）、IBMlisa4260A（节点 3，已装载 EE8005L8）；`tape list`：EE8000L08–EE8005L08 六盘 unassigned，EE8005L8 在池 EETEST（100 GB，appendable，位于驱动器）；`pool list`：仅 EETEST |
| ee3 | 装有 gpfs 与 `/opt/ibm/ltfsee`、`ltfsle`，是 EE 集群第 3 节点；`lsscsi` 可见 changer（3573-TL）与 3 台 ULT3580-TD8 |
| tsclient | 可见 changer（3573-TL，序列号 IBMlisa41D6C_LL3）与 2 台驱动器；无 cargo/rustc |

**设备型号与真机一致**：Holo-VTL 呈现的是 IBM 3573-TL 换带器与 ULT3580-TD8（LTO-8）驱动器，与 10.128.54.118 上的物理 TS4300 相同型号，CLAUDE.md 中的 CDB/地址映射经验可直接复用；固件字符串 D711/HB81 为 VTL 标称，语义须经 EE 校准。

**驱动器共享（重要）**：INQUIRY VPD 0x80 显示 tsclient 的两台驱动器序列号为 IBMlisa41D6C 与 IBMlisa42299，与 EE 节点 1、2 的驱动器相同；ee3 也能看到这两台。即 Holo-VTL 的 3 台虚拟驱动器对所有主机可见并共享，EE 把它们分配给了三个节点。结论：在 tsclient 上直接对这些驱动器写带会与 EE 争用同一设备组，违反本地图"不与运行中的 EE 实例并发争用"的约束。可行路径（需用户裁定）：a) 在 Holo-VTL 上为 tape-rs 单独建一个逻辑库/驱动器集与磁带；b) 测试窗口内 `eeadm cluster stop` 停 EE 后独占使用；c) 只做只读校准（用本项目 mount 只读恢复 EE 写过的 EE8005L8，不写带，仍需 EE 未装载该带时进行）。当前未执行任何写操作。

## 路径 1 已执行：tape-rs 专属逻辑库 TAPERS（2026-09-17）

用户选定"在 Holo-VTL 上为 tape-rs 单独建库"。执行记录：

- 执行前：EE `eeadm task list` 无活动任务；用 sqlite backup API 备份 `holo.db` 为 `/var/lib/holo/holo.db.before-tapers-20260917033120`。
- 经控制平面 HTTP API（80 端口，当前为免登录模式，API key 为空）创建：库 `tapers`（IBM TS4300 / TS2280 配置，驱动器起址 256、槽位 10 起址 1024、I/E 2 起址 768）、驱动器 `tapers-drv-01/02`、磁带 `TR8000L08`–`TR8003L08`（各 20 GiB，池 pool1）。target 自动发布：`library-tapers`、`drive-tapers-drv-01`、`drive-tapers-drv-02`，均 ready。
- 隔离：三个新 target 用 targetcli 加 ACL 仅允许 tsclient（`iqn.1994-05.com.redhat:6a32276ca48`），并设 `generate_node_acls=0`，已 saveconfig。注意控制平面重启后可能重置该属性，`configure-ee-holo-acls` 只覆盖 EE 的三个 changer target，如需持久化应把 tapers 加入该脚本。
- tsclient 经 10.10.1.70 发现并登录三个新 target。设备：changer `/dev/sg5`（序列号 `IBMtaper2287_LL3`，产品串 "TS4300"）、驱动器 `/dev/sg6`（`IBMtaper2287`）、`/dev/sg7`（`IBMtaper3D5E`）。tsclient 原有到 EE 驱动器 01/02 与 `library-lisa4300` 的会话未动；所有脚本以序列号守卫选设备，不符即中止。
- 执行后核对：EE 六个 target 仍 ready，`eeadm node list` 三节点 available，驱动器状态不变。
- tape-rs release 二进制（本机 WSL 构建，最高依赖 GLIBC_2.34，与 Rocky 9.4 兼容）部署在 tsclient `/home/rocky/tape-rs/`。

### 首轮端到端结果

`inventory` 正确（1 机械手 · 2 驱动器 · 10 槽 · 2 I/E，条码与驱动器序列号正确）。装载 TR8000L08 → mkltfs → 写入 3 MiB → commit gen 2 → ltfs-list → ltfs-read，sha256 一致。期间暴露并修复了 tape-rs 的两个健壮性缺口，并确认 Holo 的两处设备语义偏差：

| 现象 | 归属 | 处理 |
| --- | --- | --- |
| FORMAT MEDIUM 后下一条命令得到 UA 06/2A/01；装载后连续多个 UA 06/28/00 | 标准 SCSI 行为（物理 LTO 同样会报 UA）。Holo 把 2A/01 报给了发出 MODE SELECT 的同一 nexus，略偏离 SPC，但无害 | tape-rs：`retry_unit_attention` 仅用于位置无关命令（TUR/REWIND/LOCATE/READ POSITION/LOAD/MODE SELECT/FORMAT/MAM/换带器），位置相关命令不重试；模拟器同构产生 UA |
| READ(6) 读到文件标记：sense `70 00 80 00 …`，仅 FM 位；VALID=0、INFORMATION=0、ASC/ASCQ=00/00，且 **residual=0**（sg 报告传满缓冲区） | **Holo 偏差**。物理 LTO：`F0 00 80`，INFORMATION=请求长度，ASC/ASCQ=00/01，传输 0 字节。EE 能工作是因为 IBM LTFS 只看 FM 位 | tape-rs：`SenseInfo` 解析 FM/EOM/ILI 位与 INFORMATION，`read_block` 以标志为准；模拟器加 `SimQuirks::holo_vtl()` 回归。Holo 待修 |
| 短记录读（ILI）：ILI 位 + INFORMATION=200，收到 824 字节 | 正确 | — |
| 在 BOP 处 SPACE(-1 filemark)：ILLEGAL REQUEST 24/00 | **Holo 偏差**。规范为 CHECK CONDITION / NO SENSE + EOM 位 + ASC/ASCQ 00/04，INFORMATION=未完成计数 | tape-rs：`back_fm` 把该错误按到达 BOP 处理。Holo 待修 |

### Holo 源码位置

Holo 为开源项目 github.com/Holo-VTL/Holo（Rust 数据面 + Go 控制面 + React 控制台）。实验室运行的是 v1.0.4 加本地补丁；补丁源码树在开发机 `10.131.9.10:/work/jay/env/Holo`（HEAD 2296e3b，10 个文件 952 行未提交改动，另有 `infra/tcmu/patches/`），构建产物在 `/work/cargo-target/release/tcmu_handler`，部署为 holo-vtl 上的 `/opt/holo/bin/holo-tcmu-handler`（旁边保留了历次 `.before-*` 备份）。`/tmp/holo-review.eVxGay` 是上游干净检出，**不能**用它重编部署，否则会丢失 EE 依赖的本地补丁。

### Holo 偏差修复记录（2026-09-17，用户指示"是 Holo 的偏差就第一时间修复"）

源码树：`10.131.9.10:/work/jay/env/Holo`。改动前的他人本地改动已导出为 `/work/jay/env/Holo.local-changes.before-claude-20260917035617.patch`，改动后的全量 diff 为 `/work/jay/env/Holo.local-changes.after-claude-ssc.patch`，两者之差即本次改动。

| 文件 | 改动 |
| --- | --- |
| `data-plane/src/iscsi/cdb_server.rs` | `consume_current_filemark` 改为返回 VALID + FILEMARK、INFORMATION=请求长度、ASC/ASCQ 00/01（原先只有 FM 位） |
| `data-plane/src/scsi_tape/command_chain.rs` | `space()`：反向到 BOP 时位置归 0 并返回新错误 `BeginningOfPartition`；正向 SPACE filemarks 用尽时位置停在 EOD 并返回 `NotFound`（→ BLANK CHECK 00/05）。原先两者都是 `OutOfRange` → ILLEGAL REQUEST 且位置不动 |
| `data-plane/src/scsi_tape/error.rs`、`sense.rs` | 新增 `TapeError::BeginningOfPartition` 与 `SenseContext::BeginningOfPartition`（NO SENSE + EOM + 00/04） |
| `data-plane/src/iscsi/cdb_server_tests.rs`、`scsi_tape/space_mode_tests.rs` | 文件标记读取断言 VALID/INFORMATION/00-01；SPACE 越界用例从"映射为 ILLEGAL REQUEST"改为 SSC 行为。数据面 `cargo test`：280 + 7 项通过 |
| `infra/tcmu/patches/tcmu-runner-1.5.4-sequential-read-len.patch`、`infra/tcmu/README.md` | READ_LEN 条件从 VALID+ILI 扩为 VALID+(ILI/FILEMARK/EOM)，`residual <= requested`。**仅改了补丁文本，未重编未部署** |

部署（holo-vtl）：新二进制 sha256 前缀 `8cce2956556afc5a` 放为 `/opt/holo/bin/holo-tcmu-handler`，原文件备份为 `holo-tcmu-handler.before-ssc-filemark-space`（`e009866834afebdf`）。只重启了 tapers 两个驱动器的 handler（读出旧进程的环境、参数、用户 `holo`、cwd 后用新二进制原样拉起，日志在 `/var/log/holo/handler-<pub>.claude.log`）；EE 的六个 handler 与 tcmu-runner、控制面均未重启，仍运行旧代码，待下次控制面重启后才会用上新二进制。控制面登记的 handler PID 与实际不符（手工重启所致），unpublish 时它会去杀不存在的旧 PID，需要知情。

`sg_raw` 验证（tsclient → tapers-drv-01）：文件标记读取 `f0 00 80 00 00 04 00 …  00 01`；BOP 反向 SPACE `70 00 40 … 00 04` 且位置 0；正向越过 EOD `f0 00 08 … 00 05` 且位置在 EOD。tape-rs 端到端校验和一致；EE `node list`/`drive list` 无变化。

**未完成**：文件标记读取时 sg 层仍报告传满缓冲区（residual 未回报）。需要用更新后的补丁重编 libtcmu 并重启 tcmu-runner，这会中断全部 target（含 EE，当前 drv-03 装着 EE8005L8）。holo-vtl 上的 libtcmu 是 9 月 15 日本地补丁构建（`/usr/lib64/libtcmu.so.2.2`，旁有 `.before-holo-readlen*` 备份）。等用户安排窗口。tape-rs 不依赖 residual（按 FM/ILI 位与 INFORMATION 判定），不受影响。

### 实机故障场景与第四处 Holo 偏差：VCR 不随写入变化（2026-09-17）

`examples/hw_tapers.rs`（`cargo build --release --example hw_tapers`）是实机场景执行器：需要 `TAPE_RS_HW_DRIVE` 与 `TAPE_RS_HW_SERIAL`，先核对驱动器 VPD 0x80 序列号，不符即拒绝；每个场景都会对驱动器内磁带 mkltfs。做成 example 而非 `#[test]`，因为 libtest 依赖 GLIBC_2.39 的 `pidfd_spawnp`，在 Rocky 9.4（glibc 2.34）上无法启动。

| 场景 | 内容 | 结果 |
| --- | --- | --- |
| hw01 | 新卷：DP/IP 均 T0、可写、VCI 快速路径命中、零反向步进 | PASS |
| hw02 | 两次提交：gen 3、回指链深度 2、快速路径、3 MiB+ 与 700 KB 文件内容逐字节一致 | PASS |
| hw03 | 数据落带未提交：T1、只读、已提交文件可读、VCI 不被采信 | PASS |
| hw04 | 开头文件标记 + 索引前缀、无结束标记：T2、视图 gen 2、只读 | PASS |
| hw05 | 结构完整但自指针错误的索引：T3、反向 3 步找到真索引、只读 | PASS |
| hw06 | IP 写回旧代：`ip_debt`、视图取 DP gen 3、可写；继续提交后 IP 补齐 | PASS |

首轮 hw03/hw05 失败，暴露 **Holo 的 VCR（MAM 0009h）只在装载时 +1，写入不变**，VCI 因此永远"新鲜"。风险：DP 索引已写完但 VCI 未及更新时，快速路径会把旧索引当末索引，漏掉已可靠提交的新索引。两头修复：

- tape-rs：VCI 提示命中后还要求"该索引的结束文件标记紧接 EOD"，否则不采信、退回标准反向搜索。模拟器新增 `vcr_never_changes` 怪癖与回归用例 `constant_vcr_does_not_hide_newer_committed_index`（屏障失败 + 恒定 VCR 下仍恢复出 gen 3）。
- Holo：`cdb_server.rs` 新增 `bump_volume_change_reference_after_mutation`，对成功的 WRITE(6/16)、WRITE FILEMARKS(6/16)、ERASE、FORMAT 递增 VCR，并在 WRITE FILEMARKS 时立即持久化计数器（防止 handler 崩溃后 VCR 回退到某份旧 VCI 携带的值）。数据面测试 281 + 7 项通过。

部署：新 handler sha256 前缀 `e851ddde9de9f366`，`/opt/holo/bin/holo-tcmu-handler` 再次替换，上一版备份为 `holo-tcmu-handler.before-vcr-bump`（更早的原版仍在 `.before-ssc-filemark-space`）；同样只重启 tapers 两个驱动器 handler（先卸带、后装带）。EE 节点与驱动器状态不变。Holo 全量 diff 已更新到 `/work/jay/env/Holo.local-changes.after-claude-ssc.patch`。

### EE 校准：tape-rs 读 IBM 写的带（2026-09-17）

对象：EE 集群的 `EE8005L8` 目录级克隆为 `TR8003L08`（TAPERS 槽 4），由 IBM LTFS 2.4.8.3（EE 下，creator 含 "Sync via adminchannel"）写成。只对克隆操作，EE 侧只读（`eeadm file state`、对 resident 文件取 md5，不触发召回）。回退：卸带后删除 `cartridges/tapers/tr8003l08` 目录即可。

| 项 | 结论 |
| --- | --- |
| 卷标与索引 | label 2.4.0、blocksize 524288、compression=true；DP 末索引 gen 3 @35..36，回指 b@24；IP 末索引 gen 2 @5..6。两条尾部均 T0，回指链校验通过，追加位置核验通过 |
| IP 落后 DP | **IBM 自己就让 IP 落后一代**。D02 把它记为 `ip_debt` 而非错误，与 IBM 行为一致 |
| 文件内容 | 两个 `.LTFSEE_DATA/<id>` 文件（8 MiB、4 MiB）：索引 xattr `ltfs.hash.md5sum`、tape-rs 读回、ee1 上 GPFS 原文件三方 md5 相同 |
| EE 的卷内布局 | 数据在 `.LTFSEE_DATA/<id>`，用户路径 `gpfs/<fs>/...` 是 `<symlink>`（`../../.LTFSEE_DATA/<id>`，length 0，带 `ltfs.vendor.IBM.prefixLength`）。数据文件 xattr：`ltfs.hash.md5sum`、`ibm.ltfsee.objgen`、`ibm.ltfsee.gpfs.objtype`、`ibm.ltfsee.gpfs.path` |
| 索引元素 | 出现 `volumelockstate`、`extendedattributes/xattr/key/value`、`symlink`，其余与 tape-rs 已有集合相同 |
| VCI（MAM 080Ch） | IBM 的字节布局与 tape-rs 编码一致（Table 17/18），唯一差别是 **IBM 把 VCR 零扩展成 8 字节**（`08 00…00 33`），而 MAM 0009h 是 4 字节。每分区各指自己的末索引（IP gen2@5，DP gen3@35）|

由此对 tape-rs 的修改（均有模拟器或单元测试，实机已验）：

- 索引模型新增 `Xattr`（原样保留文本，`type="base64"` 记标志）、`FileNode.symlink`、目录 xattr、`volume_lock_state`，解析后原样写回；自闭合元素（`<value/>`）现在也会解析。
- **无损回写保护**：解析器记录不认识的元素名（`LtfsIndex::unknown_elements`）。非空则只读挂载，`restricted_reason` 列出元素名。原因：tape-rs 每次提交整份重写索引，不认识的元素会被丢掉；校准前 tape-rs 若往 EE 的带追加，会抹掉全部 xattr 和符号链接。
- 读符号链接时跟随一层到卷内目标；绝对路径或越出卷根的目标拒绝。`LtfsVolume::add_symlink` 可写出同样布局。
- 内容哈希（LTFS 2.5.1 附录 F.3，`ltfs.hash.<type>`，同一文件可带多种）：`append_file` 按实际写入内容计算，调用方不能覆盖。`HashPolicy` 默认只写 `sha256sum`，`md5sum` 可选（给 IBM LTFS 校验时打开）。`verify_file` / CLI `ltfs-verify [--name] [--xattrs]` 读出比对，优先 sha256，其次 md5。
- 取舍依据（本机 release、512 KiB 块、单线程、CPU 有 SHA-NI）：sha256 1248 MiB/s，md5 403，sha512 402，md5+sha256 同时算 292。两个同时算低于 LTO-8 原生速率（约 343 MiB/s），所以不默认双写。没有 SHA 指令的 CPU 上 sha256 会慢得多，部署时需复测；该数字未在 tsclient 上测。
- VCR 比较改为忽略前导零（`vcr_matches`），写 VCI 时零扩展到 8 字节（`vcr_for_vci`）。单元测试 `decodes_vci_written_by_ibm_ltfs` 断言 tape-rs 对同一内容的编码与 IBM 写出的字节逐字节相同。修复前 tape-rs 永远认不出 IBM 的 VCI，IBM 大概率也不认 tape-rs 的 4 字节 VCR（后半句未实测）。

### 第五处 Holo 偏差：装载即改 VCR（2026-09-17）

`scsi_tape/state.rs::mount` 每次装载把 `volume_change_reference` 与 `load_count` 一起 +1。真实驱动器的 VCR 只随介质内容变化。后果：LTFS §10.3 的快速路径在 Holo 上跨装载必然失效，对 IBM LE 也一样（克隆带上 VCI 记 0x33，装一次就成了 0x34）。

- 修复：删掉装载时的 VCR 递增，保留 `load_count`；新增回归用例 `test_volume_change_reference_survives_reload_without_writes`。数据面测试 282 + 7 项通过。
- 部署：新 handler sha256 前缀 `0ba202cc9da607bd`，上一版备份为 `holo-tcmu-handler.before-vcr-load`；只重启 tapers 两个驱动器 handler（先卸带）。Holo 全量 diff 更新到 `/work/jay/env/Holo.local-changes.after-claude-vcr-load.patch`。
- 验证：两次卸带装带后 MAM 0009h 不变，`usage.counters` 的 `load_count` 照常增加。把克隆带的计数恢复成原带的 51 后，**tape-rs 用 IBM 写的 VCI 在两个分区都命中快速路径**（DP gen3@35、IP gen2@5），随后 `ltfs-verify` 两个数据文件 md5 一致。
- 观察（未定位）：handler 运行期间直接改 `usage.counters` 不生效，同一盒带经换带器卸下再装回后 handler 仍沿用内存里的计数；重启 handler 后才重新读文件。只影响我这种绕过驱动器的改法。
- 探测注意：Holo 的 080Ch 固定填充到 256 字节，READ ATTRIBUTE 的分配长度小于整条属性时 Holo 返回只有 4 字节头、长度 0 的应答（SPC 要求返回截断数据并报告完整长度），sg_raw 显示的其余字节是上一条命令的残留。后者属于待部署的 tcmu-runner 残留长度补丁范围。tape-rs 用的缓冲区足够大，不受影响；未修。

### 反向校准：IBM LTFS 读写 tape-rs 写的带（2026-09-17，用户指示"反向校准也要做"）

工具：tsclient 上从源码构建开源参考实现 LinearTapeFileSystem/ltfs 的 tag `v2.4.8.3-10521`，与 EE 集群 LE 的 creator 版本号完全相同，装在 `/opt/ltfs-ref`（不进系统路径，运行需 `LD_LIBRARY_PATH=/opt/ltfs-ref/lib:/opt/ltfs-ref/lib64`，后端 `sg`）。为构建新增了 `/etc/yum.repos.d/claude-rocky-9.4-crb.repo`（默认 disabled）并装了 gcc、automake、libtool、fuse-devel、libxml2-devel、libuuid-devel、libicu-devel、icu、net-snmp-devel。源码在 `/home/rocky/ltfs-ref`（含子模块 uthash）。只碰 TAPERS 的 `TR8001L08`（槽 2）与驱动器 `/dev/sg6`，未接触 EE 设备。

首轮 IBM 完全读不了 tape-rs 写的带。逐条暴露的 tape-rs 格式偏差，全部已修并有回归用例 `on_tape_layout_matches_what_ibm_ltfs_requires`：

| IBM 报错 | 原因 | 修复 |
| --- | --- | --- |
| LTFS17034E invalid time | 时间戳没有小数；规范要求 `YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ`，9 位必填 | `label::ltfs_time_now()` 统一生成 |
| LTFS11184E format time mismatch | 两份卷标各取一次当前时间；规范要求除 `location` 外逐字段相同 | mkltfs 只取一次 `formattime` |
| LTFS17037E / 11253E No index found | Index Construct 应是"FM + 索引 + FM"（§5.2.3），tape-rs 的 mkltfs 与 IP 回写缺开头 FM，索引落在块 4 | 新分区索引在块 5（`FIRST_INDEX_BLOCK`），IP 每次从块 4 重写 FM + 索引 + FM。IBM 自己的带也是 IP 索引在 5..6 |
| LTFS17106E UID 0 is reserved | `ensure_dir` 自动建的目录 `fileuid` 为 0 | 从 `highestfileuid` 取号 |
| 无报错，但 IBM 挂载时自行补写 IP（gen+1） | IP 索引回指针指向上一代 DP 索引；一致卷的定义是 IP 末索引回指 DP 上最后一个完整索引 | IP 回指针改指刚写好的同代 DP 索引（mkltfs 同） |

同时修了恢复协议里两条与 IBM 行为不符的规则：

- "IP 代数高于 DP"原先一律判冲突。IBM 在只改元数据或补写 IP 后会留下这种状态，它是一致卷。现在 `RecoveryReport::ip_ahead()`：IP 更新且回指针正好指向 DP 末索引时视图取 IP、允许追加；回指针不符才判冲突。用例 `ip_ahead_with_back_pointer_to_dp_last_is_consistent`。
- 追加放行条件：提交会从内容区首块整段重写 IP，所以视图里有文件数据位于 IP（IBM 放置策略）时必须只读（用例 `volume_with_file_data_on_index_partition_mounts_read_only`）；没有 IP 数据且 DP 尾部完整时，IP 自身状态不再阻止追加（IP 阶段失败后 IP 只剩开头 FM 的情形）。

修完后的实机结果（`ts_rev_full.sh`、`ts_vci_accept.sh`）：

| 步骤 | 结果 |
| --- | --- |
| tape-rs mkltfs + 写 2 个文件 → IBM `ltfsck` | Volume is consistent，Gen 3 (a,5)->(b,18)，不再补写 IP |
| IBM `ltfsck -l` | 列出 tape-rs 写的全部 4 个回滚点与回指链 |
| IBM 挂载读 | 两个文件 sha256 与源文件一致 |
| IBM 追加 1 个文件 + 1 个目录后卸载 → tape-rs | gen 4，回指链 4 代全通，可写，`from_ibm.bin` sha256 一致，tape-rs 自己写的文件 `ltfs-verify` 一致 |
| tape-rs 在其上再追加 → IBM `ltfsck` | gen 5，consistent |
| IBM 对 tape-rs 写的 VCI | 接受：快速挂载、无告警、rc=0；不写任何内容就卸载后带保持原样，tape-rs 仍命中快速路径 |
| tape-rs 对 IBM 写的 VCI | 两个分区都命中快速路径 |

扩展属性双向验证（2026-09-17，用户指示；tsclient 装了 `attr`，脚本 `ts_xattr.sh`）：

| 方向 | 内容 | 结果 |
| --- | --- | --- |
| tape-rs 写 → IBM 读 | `ltfs-write --md5 --xattr origin=… --xattr 'note=中文 & <xml> "q"'`；IBM 挂载后 `getfattr` | `user.ltfs.hash.sha256sum`、`user.ltfs.hash.md5sum` 与本地文件实算值相同；`user.origin`、`user.note`（含中文、`&`、`<>`、引号）逐字符一致 |
| IBM 写 → tape-rs 读 | IBM 挂载下 `setfattr`：文本、二进制 `0x0001feff80`、`user.ltfs.hash.md5sum`、目录属性、给 tape-rs 写的文件加属性 | tape-rs 读到全部属性，二进制值为 `AAH+/4A=`（base64 标志正确）；`ltfs-verify` 用 IBM 写的 md5 校验 `from_ibm.bin` 一致 |
| 无损回写 | tape-rs 再追加一个文件（整份重写索引）→ IBM 再挂载 | IBM 写的文本、二进制（hex 读回 `0x0001feff80`）、目录属性、后加到 tape-rs 文件上的属性全部原样保留；`ltfsck` consistent |

键名约定：索引里存的是不带 `user.` 前缀的名字（tape-rs 的 `--xattr origin=…` 对应 IBM 侧的 `user.origin`），与 EE 写的 `ibm.ltfsee.*` 一致。CLI 为此新增 `ltfs-write --xattr KEY=VALUE`（可重复）与 `--md5`。

实机场景 hw01—hw06 在新布局下重跑全过（hw06 的脚本原先写死 IP 索引在块 4，已改）。

### 第六处 Holo 偏差：080Ch 被当成 256 字节定长属性（2026-09-17）

现象：IBM 每次挂载都报 `LTFS12058W Cannot get VCI data: unexpected ID 0x0001` 并走全量检查。原因：Holo 的 MAM 模型用一张静态表给每个属性一个定长默认值，WRITE ATTRIBUTE 时 `normalize_attribute_value` 一律补齐到该长度；080Ch 是变长属性，表里写了 256。IBM 用恰好 79 字节（4+5+0x46）读，Holo 的"放不下就整条不返回"规则使应答为空，IBM 读到的是缓冲区残留；且 IBM 要求属性长度等于 0x46。EE 集群自己的 LE 同样受影响（每次慢速挂载）。

- 修复（`cdb_drive.rs`）：读 080Ch 时按值自描述长度（1+VCR 长度+8+8+2+ACSI 长度）裁剪，已存盘的旧数据同样生效；该分区没写过 VCI 时属性不存在，返回 ILLEGAL REQUEST 24/00（IBM 视为预期情况）。Holo 原有的"只返回完整条目"行为由 `test_drive_read_attribute_avoids_partial_entries_when_allocation_truncates` 保护（注释提到 Dbackup），未改动。新增 `test_volume_coherency_attribute_keeps_written_length`。数据面测试 283 + 7 项通过。
- 部署：handler sha256 前缀 `bbe5fa358c8fe1d2`，上一版备份为 `holo-tcmu-handler.before-vci-length`；只重启 tapers 两个驱动器 handler（先卸带）。全量 diff：`/work/jay/env/Holo.local-changes.after-claude-vci-length.patch`。
- EE 的 handler 仍是旧进程，下次由 control-plane 重新拉起时才会用到新二进制。

### 持久预留抢占实验留下的环境改动（2026-09-17）

为 [09 的设备层隔离证据](fencing-lab-evidence.md)，临时在 tsclient 建过 open-iscsi iface `claudeb`、在 Holo 给 `drive-tapers-drv-01` 加过发起方 `iqn.2026-09.lab.claude:tsclient-path-b` 的 ACL（未 saveconfig）。实验后均已删除，驱动器上的注册与预留已清除（PR generation 停在 0x19）。对 EE 设备只从 ee1 发过 PERSISTENT RESERVE IN 与 INQUIRY 两类只读查询。

同日第二次以相同方式临时建立双路径，用 tape-rs 自己的 `pr-fence`/`pr-release` 重做抢占实验（脚本 `ts_fence2.sh`），并对 TAPERS 换带器做过一次预留与清除。实验后 iface、ACL、驱动器与换带器上的注册和预留均已清除。

### 三节点环境：借用 EE 的三台节点（2026-09-17，用户提议并确认 EE 节点无业务负载）

- Holo 侧给 ee1、ee2、ee3 的发起方名（`iqn.1994-05.com.redhat:447bb2401795`、`…:39c09bf29d64`、`…:3bd2d2127b`）在 `library-tapers`、`drive-tapers-drv-01/02` 上加了 ACL，已 saveconfig。三台节点经 10.10.1.70 登录这三个目标，`node.startup=manual`（重启后不会自动登录）。tape-rs 二进制与设备映射在各节点 `/home/rocky/tape-rs/`（`devmap` 记录本节点上 TAPERS 的 sg 设备名，三台各不相同）。tsclient 仍是第四个发起方。
- **与 IBM LE 共存的观察**：LE 空闲时不碰 TAPERS 的设备（100 秒内无注册、无预留、无进程打开）；EE 重启或 `library rescan` 时是否会碰，未验证，不在 EE 上主动触发。LE 以 O_EXCL 打开它自己的 sg 设备，阻塞式 open 会在内核 `sg_open/open_wait` 里无限等待，tape-rs 的 `inventory` 因此挂死过两次。已修：`ScsiDevice::open` 改为非阻塞打开（被占用立即 EBUSY），`inventory` 只对换带器报告的本库驱动器读 MAM，其余驱动器只做 INQUIRY。
- 在 EE 节点上只做进程级故障（暂停、杀进程、断开本系统端口）。对 EE 的 VM 做停止/重置会影响 GPFS 与 EE 这套对照环境；需要 VM 级掉电场景时另行克隆 tsclient。

三节点隔离接力演练（脚本 `pve_drill.sh`、`ee_node.sh`，每个节点对两个驱动器加换带器整体隔离）：

| 步骤 | 结果 |
| --- | --- |
| 节点 1 取得整个逻辑库并写入；节点 2 接管；节点 3 接管 | 每次接管三个设备都回读确认；被接管者的写入先得 2A/05、后得 RESERVATION CONFLICT，什么也没落带；卷上只有三个合法持有者各自提交的文件，回指链完整、可写 |
| 节点 3 写 1 GiB 途中被 SIGSTOP 暂停，节点 1 以新轮次接管，随后节点 3 恢复运行 | 节点 3 恢复后的第一条命令即被拒绝，进程退出。节点 1 挂载该卷：已提交的 5 个文件完好，DP 尾部为 T1 UnindexedData（节点 3 暂停前已写入的数据块），按 D02 只读挂载 |
| 节点 1 对它被抢占后从未碰过的设备重新隔离 | 首版失败：第一条 PR 命令先收到历史遗留的 2A/05，而重试逻辑已不再越过它。已修：PR 命令自己越过全部 UNIT ATTENTION，按读到的现状行事 |

第二行是一个需要产品层面决定的后果，见 map 的 Not yet specified。

接管与自动收尾演练（脚本 `pve_drill4.sh`，2026-09-17）：节点 3 接管后写 1 GiB 途中被 SIGSTOP；节点 1 执行 `ltfs-takeover --salvage`：抢占并回读确认 → 恢复协议判定 T1（末索引结束于块 10，EOD 253）→ 打捞 242 块并在 EOD 追加 gen 3 索引 → 卷可写；打捞文件 126,877,696 字节，与 ee3 上源文件同长度前缀 sha256 相同；节点 3 恢复运行后第一条命令得 2A/05；节点 1 继续追加正常；释放后 tsclient 上的 IBM `ltfsck` 判定一致，`ltfsck -l` 列出 258 → 254 → 9 → 5 的完整回指链。EE 三节点全程 available。

### ltfsd 在三台 EE 节点上的部署（2026-09-17）

二进制 `/home/rocky/tape-rs/ltfsd`，控制脚本 `/home/rocky/tape-rs/ltfsd_ctl.sh`（start/kill/pause/resume/status/log/wipe），数据目录 `/home/rocky/ltfsd-data`（`raft.db`、`status.json`），日志 `/home/rocky/ltfsd.log`，节点间通信走管理网 10.131.9.x 的 7400 端口（节点上没有 firewalld）。演练后进程已全部停止，TAPERS 三个设备上的预留已清除。演练结果见 [ltfsd 骨架记录](ltfsd-skeleton.md)。

TAPERS 新增两盘 256 MiB 的小带 `TS1000L08`（槽位 5）、`TS1001L08`（槽位 6），经 Holo 控制面 API 创建，用于写满换带的演练。演练后两盘各有 5 个 20 MiB 文件并带池标记 `small`，都在槽位里；要重用需重新格式化。

第七处 Holo 偏差与修复（2026-09-18）见[池化切片计划](pooling-slice-plan.md#p4-实机读取已补齐2026-09-18修复第七处-holo-偏差)。handler 二进制换为 `holo-tcmu-handler.ssc-freshload`，上一版备份 `holo-tcmu-handler.before-freshload`，全量 diff `Holo.local-changes.after-claude-freshload.patch`；只重启了 tapers 的三个 handler。实验室操作磁带的脚本一律要在换带前先 `tape-rs drive-unload`。

### Holo 在 lisa4300 库里建带/删带会停用 ee2/ee3 的换带器发布（2026-09-22）

**现象**：2026-09-18 05:56 用 `POST /v1/cartridges` 在 lisa4300 建 EE8006L8 的同一秒，Holo 控制平面删掉了 iSCSI 目标 `library-lisa4300-ee2` 和 `-ee3` 及其 backstore；ee2/ee3 的发起方从此每 2 秒重登失败（`conn error (1020)`），EE 节点 2/3 的 LE 报 `LTFSI1096E Library failes to get inventory`。EE 的 `node list`/`drive list` 直到需要用到那两个节点的换带器时（`tape assign` → 格式化任务 → GLESL093E）才显示异常（驱动器 `missing`）。

**原因**（源码 `control-plane/internal/api/resources_handler.go`）：`ensureLibraryAutoPublications` 在建驱动器、建带、删带、加槽、删库、建链时运行，把该库下所有 ready 状态、IQN 不在"自动生成集合"（库的规范 IQN + 各驱动器 IQN）里的发布全部 `Unpublish`。ee2/ee3 的换带器发布是 9 月 15 日以 actor `ee3-expansion` 手工 POST 的，属于会被清掉的那一类。`unpublishDependentPublications`（删带/擦带/导出时）另按 cartridgeId 匹配。控制台的"目标发布"页只读，看不出这一层。

**恢复步骤**（已验证，约 5 分钟）：

1. 在 holo-vtl 上按原字段重新发布两份（被停用的旧记录留在 `GET /v1/targets/publications` 里，state=disabled，IQN 可复用）：
   `curl -X POST http://127.0.0.1/v1/targets/publications -H 'Content-Type: application/json' -d '{"poolId":"pool1","libraryId":"lisa4300","driveId":"lisa4300-drv-01","cartridgeId":"EE8000L08","targetIqn":"iqn.2026-04.cloud.backupnext.holo:library-lisa4300-ee2","deviceRole":"changer","deviceProfile":"ibm-3573-tl","driveProfile":"ibm-ult3580-td8","actor":"..."}'`（ee3 同理）。
2. `sudo /usr/local/sbin/configure-ee-holo-acls`（建 ACL、关 generate_node_acls、saveconfig）。发起方几秒内自动重登。
3. LE 进程缓存了旧的换带器设备，iSCSI 恢复后仍报 LTFSI1096E，须逐节点：ee1 上 `eeadm node down N` → 该节点 `systemctl restart ltfsle.service` → `eeadm node up N`。做完 `drive list` 三台 ok。装着带的驱动器（当时节点 3 装着 EE8005L8）重启 LE 后磁带回槽位。

**约定**：以后在 lisa4300 里做任何 Holo 资源变更（建带、删带、擦带）前，先确认 EE 没有任务，做完立即按上面 1—3 恢复；或者在 TAPERS 库里建带（TAPERS 的发布都是自动生成的，不受影响）。EE8006L8 因此留在 lisa4300（unassigned），未删。
