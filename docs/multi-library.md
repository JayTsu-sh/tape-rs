# ltfsd 多带库与统一 HTTP 入口

多库模式在一个 ltfsd 进程中运行一个 Raft 节点、一个控制日志和一组集群成员。每个带库各有设备 owner、执行线程、PR 隔离、ReadIndex 执行许可、文件暂存区和目录。两个库可以由同一个节点持有，也可以由不同节点持有；单个库失去完整设备通路时只撤销该库的资格。

管理状态与目录更新都由同一 Raft 日志复制。SQLite 是各节点的持久副本及目录投影，不需要另设集中数据库。库间允许相同池名、池 UUID、条码和文件路径；这些资源通过库标识区分。

## 以真实序列号配置带库

三个节点提供相同的 `--libraries-config /path/libraries.json`；该参数与 `--changer-serial`、`--drive-serial` 互斥。例如当前实验室的两个物理库：

```json
{
  "default_library": "55L3A7802K19LL01",
  "libraries": [
    {
      "changer_serial": "55L3A7802K19LL01",
      "drive_serials": ["11EB4A80F1"]
    },
    {
      "changer_serial": "55L3A78011W9LL01",
      "drive_serials": ["117018605B", "117018606F"]
    }
  ]
}
```

带库的唯一标识是 `changer_serial`，使用 1–64 个字母、数字、`-` 或 `_`；不接受自定义 `id` 或 l8/l9 别名。`default_library` 也必须填写实际 changer 序列号，设备序列号禁止在库间重复。设备由 VPD 序列号识别，不把动态的 `/dev/sgN` 作为身份。配置的驱动须属于对应库；执行器沿用原设备发现与 PR 流程，库存映射使用 changer 返回的驱动 DVCID。本次现场在接管前逐库核实了真实 DVCID 归属。同主机的设备序列号锁仍禁止不同进程操作同一设备。

未提供多库参数时仍运行单库模式。多库默认库的目录位于根目录 `directory.db` / `spool/`，其他库使用 `libraries/<serial>/`。Raft 日志作用域、控制快照键及目录路径均使用同一个真实序列号；`raft.db` 始终只有一份。此前用 l8/l9 标识的多库配置、目录和快照不做兼容迁移，不得直接更改旧目录中的 manifest 或覆盖旧状态。

多库配置规范化后保存在数据目录的 `libraries.json`。后续启动校验默认库、库集合及设备对应关系，禁止隐式更换。新增库可在**全部 Raft 节点正常停机且日志追齐**后逐节点显式执行 `ltfsd --add-libraries-offline --libraries-config <新配置>`（同时提供该节点原有的 `--id`、`--listen`、`--peer`、`--data-dir`）；三个节点必须使用相同的新配置和程序版本，并核对命令打印的变更前 Raft 提交索引一致。此命令只更新该节点的配置记录，不打开设备或启动服务。旧库及驱动序列号必须原样保留，新库数据目录须为空；旧快照中的库继续保留，新库从空控制状态和空目录开始。记录文件 `libraries-addition.json` 允许节点接收变更前的控制/目录快照，并支持连续多次配置变更。确认三个节点均完成变更后，正常启动服务并核对每个库的真实设备身份与 owner。在线增删库及移除旧库仍不支持，不能直接编辑持久化 manifest 冒充迁移。启用多库后必须用多库参数启动，不能用旧单库进程解释新的日志或快照。升级须停止并升级全部成员，不支持混合版本运行。

`--expand-libraries-offline` 是同时支持新增库和在旧库追加驱动器的正式参数，`--add-libraries-offline` 保留为同义别名。新增驱动必须使用新的真实 VPD 序列号，旧驱动不能从列表删除或改名；记录同时记下新增驱动集合。原控制快照可继续使用，因为库身份不变。执行前须只读确认新驱动确属该库、DVCID 与预期一致且没有其他服务占用；启动后继续核对 owner 与设备状态。

更换一台旧驱动时，把新配置中该库的旧序列号换成新序列号，然后在全体节点停机后逐节点运行 `ltfsd --replace-drive-offline OLD=NEW --libraries-config <新配置>`，同时带上原有节点和数据目录参数。该命令要求库集合及默认库不变、同一库只更换这一台驱动，并拒绝本地存在未提交的 Raft 日志；不会访问设备。回放旧快照和日志时，旧驱动的 `enabled`、角色、节点组等管理策略迁到新序列号，旧库存观测失效，实际库存须在新 owner 就绪后显式 `library rescan`。操作前须核实旧驱动没有载带、新驱动确属该库且 DVCID/真实 VPD 序列号相符，并确认 EE 或其他进程未占用；三个节点变更前提交索引必须一致。没有替换硬件时不能把此软件路径算作现场验收。

退役一台驱动但保留带库时，从新配置中删去该驱动序列号，在全部节点正常停机且日志追齐后逐节点运行 `ltfsd --remove-drive-offline <旧驱动序列号> --libraries-config <新配置>`，并提供原节点、peer 和数据目录参数。每次只移除一台，库内至少保留一台驱动；命令拒绝未提交日志及同时增删其他设备，不访问设备，也不删除磁带目录或文件。旧驱动策略在快照和日志回放时丢弃，指向它的排队/运行管理任务标为 `indeterminate`，含它的库存失效；启动后应显式 `library rescan`。执行前核对驱动已空、无未完成机械任务、实体带仍可由剩余驱动读取，并确认三个节点提交索引一致。移除的序列号不允许在之后的配置变更中复用；若要恢复该实体驱动，需要设计单独的恢复流程。此命令不是带库退役，也不迁移文件或磁带。

B 库在三物理节点隔离候选中完成一次逻辑驱动移除，并仅用保留驱动读回 `AA0013L8` 的既有文件；退役驱动仍在库内，候选被设备策略禁止访问它。[验收范围与证据](multi-library-drive-removal-physical-20261022.md)。

离线配置变更要求本节点已有完整的 `raft.db` 和 HardState；缺失这些文件时命令会拒绝执行，不能把空节点当作追齐的成员。

## 固定 VIP

继续使用原 `--http-vip`、`--http-vip-interface`、`--client-listen`、`--client-port`。VIP 入口独立获得同一 Raft 集群的多数派许可，只要求入口网络资格，不依赖某个带库的 FC 通路或是否已有可写磁带。各节点 HTTP 端口须能相互访问。

客户端始终访问同一个地址，例如 `http://10.131.7.219:17821`。入口把数据请求转到目标库当前 owner，把管理写请求转到控制 Leader；管理查询可由入口的已应用状态回答。用户选择库和资源，不需要选择节点。接管期间仍可能返回明确暂态 503。

每个请求最多转发一次；owner/Leader 在转发途中变化时不会继续跨节点转发。连接建立失败是暂态；请求开始发送后连接中断返回 504 `indeterminate`，不会自动重发写请求。调用方应查询文件或管理任务状态确认结果。原 VIP 的有限地址寿命和严格撤址确认协议继续适用，超时本身不授权强制抢占 VIP。

## HTTP 路径

| 路径 | 作用 |
| --- | --- |
| `GET /admin/libraries`、`GET /cluster` | 列出带库序列号（id 与 changer_serial 同值）、各库 owner、就绪状态；同时给出入口与 VIP 信息 |
| `GET /readyz` | 当前节点是否拥有有效的统一入口许可 |
| `/libraries/<serial>/admin/...` | 该库的管理接口，沿用原接口语义 |
| `/libraries/<serial>/files/<path>` | 该库的上传、读取、删除 |
| `/libraries/<serial>/stat/<path>`、`/libraries/<serial>/list` | 该库的文件查询 |
| `/libraries/<serial>/tasks/<task>` | 该库的文件任务 |
| `GET /admin/libraries/<serial>` | 单库详情 |
| `/admin/libraries/<serial>/tapes/...`、`/admin/libraries/<serial>/pools/...` | 对应库内管理路径的别名 |
| `POST /admin/libraries/<serial>/rescan` | 对应库的显式库存刷新 |

原 `/files`、`/admin/tapes`、`/admin/pools` 等路径映射到默认库，保留原单库入口。非默认库的客户端应始终保留 `/libraries/<serial>` 前缀。容量、状态、清洗带识别和管理任务均在库内隔离。

快照包含全部分库控制状态；目录以 1 MiB 分片打包到一个 SQLite 快照文件。各库目录覆盖同一个全局日志索引，接收方验证库集合、分片序号和目录索引后安装，再按日志补齐。目录尚未同步或领先于本地控制日志时禁止获得执行许可。

## 验证记录

纯软件集群测试覆盖：同节点持有两个库、分散持有、经固定入口转发读写、相同条码/池名/UUID/路径隔离、单库失去通路不影响另一库、目录快照打包恢复、旧快照默认库映射和 VIP 占用保留、转发写入断连返回结果未定。

历史版本的真实硬件验收记录保存在 `.scratch/ltfs-ha/probes/results/`；模拟器结果不代表物理验收通过。

2026-10-21 已完成双物理库与固定 VIP 数据面、真实节点目录快照及旧 VIP 迁移保护验收，详见[现场验收记录](multi-library-physical-acceptance-20261021.md)。HTTP 转发启用现有 Hyper 的 client 特性，锁文件新增其 want/try-lock 两个传递依赖；不需要额外代理进程。


## FUSE 与命令行选库

两个客户端都支持 `--library <真实 changer 序列号>`，所有文件、目录、任务和管理请求自动携带 `/libraries/<serial>` 前缀；省略时使用默认库。未知序列号返回 404，不回退到其他库。入口提示不会改变客户端已经选定的库。

```bash
mkdir -p /mnt/tape/55L3A78011W9LL01 /mnt/tape/55L3A7802K19LL01
# 以下两个挂载进程分别在前台运行。
tape-fuse -e 10.131.7.219:17821 --library 55L3A78011W9LL01 \
  --cache-dir /var/tmp/tape-cache/55L3A78011W9LL01 /mnt/tape/55L3A78011W9LL01
tape-fuse -e 10.131.7.219:17821 --library 55L3A7802K19LL01 \
  --cache-dir /var/tmp/tape-cache/55L3A7802K19LL01 /mnt/tape/55L3A7802K19LL01

ltfsctl -e 10.131.7.219:17821 --library 55L3A78011W9LL01 put ./archive.bin /archive.bin
curl -i --upload-file ./archive.bin \
  'http://10.131.7.219:17821/libraries/55L3A7802K19LL01/files/archive.bin?wait=1'
```

FUSE `close` 仍只确认暂存；应用须在关闭文件前调用 `fsync` 确认落带。`--library` 只用于远程集群模式，与直接打开设备的 `--library-changer` 互斥。挂载点名称可以自行选择，但目标始终由参数中的真实序列号决定。

序列号唯一身份、两个真实内核 FUSE 挂载及双物理库读写已经完成[本轮现场验收](library-serial-physical-acceptance-20261021.md)。前一份多库报告中的 l8/l9 是历史版本事实，不代表当前接口仍支持别名。

`ltfsctl` 的管理子命令使用同一固定入口和 `--library` 选择库。`library rescan`、`drive up/down/role/assign`、`node assign`、`nodegroup create/delete`、`pool set/delete`、`tape offline/online/blacklist/clear-blacklist/validate/reconcile/objects`、`task cancel/clearhistory --yes` 调用对应 HTTP 管理路由；设备操作返回任务号，须再用 `task show <ID>` 查询终态。`tape reconcile` 从磁带索引更新集群目录，不执行 GPFS HSM 对账；`tape objects` 查询 EE 保存对象，不是普通文件列表。`admin` 子命令仍可显式调用尚未列为具名命令的管理路由。三节点隔离候选已验证 B 库的 rescan、保留驱动 up/down/role、空池属性、上述磁带策略/校验/对账/对象查询与旧文件读回；其余命令和正式服务部署范围见[差距进展](multi-library-gap-progress-20261022.md)。

驱动 `up/down/role/assign` 只接受当前 owner 轮次经 `library rescan` 观测到的真实驱动序列号；旧库存和未列出的序列号都会拒绝，避免替换驱动后把策略写给不存在的设备。

`ltfsctl copy` 已在 [同节点 owner 的 A→B 试验](multi-library-cross-copy-physical-20261022.md)与[分散 owner 的 A→B 试验](multi-library-split-owner-copy-physical-20261022.md)中将 A 库的普通文件显式复制到 B 库，并在独立冷挂后从 B 磁带读回。源和目标仍是分库目录中的两个文件；当前没有全局副本集合、自动同步或物理迁池事务。
