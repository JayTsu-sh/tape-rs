# HTTP 固定 VIP

`ltfsd` 内置同子网 IPv4 VIP，可让客户端固定访问一个地址。没有 Keepalived、VRRP、外部脚本或新增 Rust 依赖；地址管理使用 Linux rtnetlink，邻居缓存更新使用原生 ARP 公告。

当前访问约定：管理 HTTP 与 [S3 HTTP](s3.md) 共用 VIP、使用不同端口；FUSE 与 Rust Client/lib 直连各节点物理地址。S3 监听随同一 VIP 漂移，不单独取得另一份地址占用。

## 配置

所有可能成为设备 owner 的节点配置同一个专用 VIP；接口名称可以不同。示例地址仅用于说明，部署时替换为实际子网内已预留、未被其他设备使用的地址。

```bash
# 在原有 ltfsd 启动参数上增加：
--client-listen 0.0.0.0:7401 \
--client-port 7401 \
--http-vip 192.0.2.100 \
--http-vip-interface eth0
```

VIP 必须与指定以太网接口已有的 IPv4 地址同子网，不得使用主地址、网络地址或广播地址。接口必须已启用且有链路；名称最多 13 字节，为内核地址标签 `接口:t` 留出空间。所有节点的 HTTP 端口保持一致，`--client-port` 应与实际监听端口一致。

启用 VIP 时要求 `CAP_NET_ADMIN` 和 `CAP_NET_RAW`，以及 `--execution-permit-ms >= 4000`（默认 6000）。若使用 systemd 权限限制，需要将这两项 capability 加入现有允许集合；网络命名空间、地址族、系统调用和设备权限仍按原服务要求配置。VIP 接口须在进程所在网络命名空间内。程序在启动设备线程前校验配置、权限并取得本机 VIP 进程锁。

VIP 互斥依赖已提交的占用/撤址确认记录，不通过比较各节点许可时限推断旧地址已经消失。许可到期仍会停止本地服务并撤址，但不会自动释放 Raft 中的占用记录。

HTTP 必须监听 `0.0.0.0:端口`，这样备用节点即使尚无 VIP 也能启动，且可通过本机地址检查状态。不需要打开 `ip_nonlocal_bind`。应使用现有可信内网和防火墙规则限制 HTTP 访问范围。

```bash
ltfsctl -e 192.0.2.100:7401 get /archive.bin ./archive.bin
curl --fail http://192.0.2.100:7401/readyz
curl http://192.0.2.100:7401/cluster
```

不配置 VIP 时维持原有监听和多端点客户端行为。VIP 只增加固定数据入口；管理命令仍由控制 Leader 受理，若 VIP owner 是 follower，管理请求沿用 503 中的 Leader 地址提示，现有客户端继续按原规则处理。

## 归属与切换

VIP 跟随设备 owner。控制 Leader 具备目标设备组的有效访问资格时兼任 owner，即使原 owner 健康，也会按正常接管协议迁移；只有 Leader 不具备该资格时，才保留或选择其他合格节点作为 owner。资格由设备组探测、连续成功上报、报告新鲜度及故障冷却共同决定，不能仅凭安装了 FC 卡判定。只有设备全组隔离、恢复及就绪记录已完成，且本地多数派服务许可有效时，才允许添加地址；没有可写磁带不妨碍保留管理/查询入口。`GET /readyz` 使用同一资格判定，返回 200 或 503，正文包含 `ready`、`id`、`owner`、`round`、`shutting_down`。它表示 owner 资格，不保证池还有空间或 VIP 网络配置操作已经成功；后者失败会记录中文日志。

独立网络线程约每 200ms 核对资格。失去有效 Serving 许可时主动撤下本机 VIP；计划停机、owner 分配改变或网络操作失败时，还须完成以下显式交接。网卡链路也参与执行候选探测。恢复资格的 follower 不抢占健康 owner；恢复资格的控制 Leader 会接管。控制选举和设备接管异步完成，迁移期间两者可能暂时不同；切换需要撤址、隔离和卷恢复，会产生服务中断。

1. Raft 为每个 VIP 保存节点、owner 轮次和唯一授权号。预约仅允许当前设备 owner，并须匹配最后一次授权号；新节点在预约已提交前不能配置地址。
2. 旧节点通过同一个网络线程依次完成删除请求、内核 ACK 和全地址列表回读。只有确认该 VIP 已不存在，才提交绑定旧节点、旧轮次、旧授权号的释放确认。
3. 释放确认经多数派提交后，才允许新 owner 预约。授权号在释放后仍保留，释放前排队的旧申请和迟到确认不能重建或清除后续授权。
4. 新授权申请与设备隔离、磁带恢复并行；只有新授权已提交且本地 Serving 许可有效时才挂 VIP、发送 ARP。确认已齐全后不再额外固定等待八秒。

**宁可暂停 VIP，也不因超时推断旧 VIP 已退出。** 旧 owner 失联、暂停、崩溃或失去多数派时，未确认的占用记录不会因设备 owner 变更、长时间等待、PR 隔离成功或 ARP 不应答而清除。新 owner 可以完成带库接管，但固定 VIP 保持暂停；允许通过节点自身地址检查 `/readyz` 或本地 `status.json` 中的 `vips` 记录，查看正在等待哪个节点及哪次授权。记录表示地址预约权，不表示该节点的内核当前一定仍配置着地址。

地址以 `/32` 和 `NOPREFIXROUTE` 添加，不修改主地址、网关或现有子网路由。内核地址寿命最多两秒，正常内核可在进程暂停/强杀后回收地址；这只是本地残留清理保障，不能代替已提交的释放确认。地址增删只匹配本功能的接口、标签及地址，发现其他用途的同 IP 时拒绝修改，也拒绝确认释放。网络线程发送释放后即封存该授权号，共享快照尚未更新或迟到许可到达时也不会重用。

旧节点恢复后，必须先取得本机 VIP 进程锁、清理地址并回读为空，才能释放旧占用。计划停机期间 Raft 继续运行，等待设备安全收尾和 VIP 释放确认提交；宽限耗尽仍未确认时保留占用记录。当前没有自动 BMC/交换机隔离、远程强制撤址或强制释放入口；旧节点永久不可恢复时，VIP 继续暂停，不能靠清理 Raft 数据或手工删除占用记录恢复自动漂移。

此策略针对本程序管理的 VIP、稳定的节点身份与可信控制网络；不会隔离同一主机上外部管理员或其他程序手工配置的相同 IP。设备安全仍依赖原多数派许可和 PR fencing。授权共识、设备恢复、内核调度及邻居缓存收敛都会影响恢复时间，不承诺固定 RTO 或零连接中断。

切换会中断已有 TCP 连接。202 仍只表示暂存；请求正文可能已到达但响应丢失时，必须在新 owner 上查询路径和哈希，不能因为 VIP 恢复就盲目重放写入。控制管理请求的路由和所有原有提交语义不变。

首版只支持同一二层网络内的 IPv4 入口；不支持跨子网、IPv6、BGP、自动调整交换机策略或 NIC 重建。VIP 必须从 DHCP 和其他静态配置中排除，不提供完整的 ARP 地址冲突检测。交换机需允许该 VIP 在各 owner 的 MAC 之间移动并接受 ARP 公告。网卡重建或 MAC 改变时应重新核对配置后重启本节点。

## 升级边界

本版新增 `vip_acquire` / `vip_release` 控制日志命令，以及快照中的 `vips` 占用和授权号字段。旧快照可以解码为空表，但不能据此认定旧 VIP 已退出。首次从此前寿命窗口版本升级时，须整组停止旧 VIP 实例，并确认各节点地址均已撤下，再统一部署本版；不支持与旧 VIP 版本混跑或带占用记录回退。共识数据目录必须保留，不能用空目录重新启动集群来绕过未确认的旧占用。

## 验证

默认 `cargo test` 不操作真实网卡。就绪条件、轮次/许可变化、地址生命周期和报文编码用纯软件测试覆盖；状态机测试验证占用记录不因 owner 切换或等待而清除、释放精确匹配授权号、快照保留未确认记录。真实 Raft 循环配合模拟设备验证 follower owner 分区后新 owner 仍被旧 VIP 占用阻止，旧节点恢复并提交确认后才允许预约，且不能从新节点伪造旧节点确认。

以下显式测试只允许在独立网络命名空间中运行，使用专用 veth 和文档地址，不接触磁带。需要本机允许创建 user/network namespace；`ip` 仅用于搭建测试网络，不是运行时依赖。

```bash
export TAPE_RS_VIP_HOST_NETNS=$(stat -Lc %i /proc/self/ns/net)
unshare --user --map-root-user --net bash <<'NETNS'
set -eu
ip link add viptest0 type veth peer name vippeer0
ip addr add 192.0.2.1/24 dev viptest0
ip addr add 192.0.2.2/24 dev vippeer0
ip link set viptest0 up
ip link set vippeer0 up
ip link set lo up
export TAPE_RS_VIP_NETNS_TEST=1
cargo test --lib daemon::vip::tests::kernel_address_lifecycle -- --ignored --exact --nocapture
cargo test --lib daemon::vip::tests::kernel_process_expiry -- --ignored --exact --nocapture
NETNS
```

隔离测试覆盖内核实际添加/刷新/删除和到期、拒绝覆盖其他标签、通过 VIP 访问 HTTP、veth 对端实际接收 ARP、新授权与慢/快恢复并行且 ready 后及时发布，以及子进程计划停机状态、暂停/恢复和强制退出后的地址回收。测试记录 ready、VIP 发布及旧地址消失的实际时间；内核测试对控制日志提交使用内存适配器，真实 Raft 转交和持久化由独立集群及快照测试覆盖。它不等于客户交换机/网关验收，也不代替真实带库上的整套 HA 故障矩阵。

协议依据：[Linux rtnetlink 地址及寿命接口](https://man7.org/linux/man-pages/man7/rtnetlink.7.html)、[RFC 5227 §2.3 ARP Announcement](https://www.rfc-editor.org/rfc/rfc5227.html#section-2.3)。这里只使用公告报文格式，不声称实现完整 RFC 5227 地址冲突检测。
