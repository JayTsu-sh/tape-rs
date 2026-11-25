# 物理带库交还与重新加载

FDB writer 可以按机械手序列号把一座受管带库交给外部应用。所有可能取得该库设备执行权的节点必须运行支持交还的版本。交还期间应通过稳定的节点地址或统一入口查询回执，跟随设备执行权的单库 VIP 可能已经撤下。

首次交还前，安排维护窗口，停止所有能访问该库设备的执行器，确认它们已退出且已提交的设备命令已结束。确认这两项后，在任一停止的节点上用其原 `--id`、`--listen`、`--peer`、`--data-dir`、`--libraries-config` 和 `--fdb-cluster-file` 参数运行 `ltfsd --enable-library-handoff-offline "$LIBRARY" --handoff-namespace "$NAMESPACE"`。命令检查节点目录锁和 Raft 端口，只升级指定库的 FDB 路由。停止进程和排空设备命令仍需现场核实。

启用后再启动升级后的节点。路由版本 2 保留原 namespace、路由身份和目录，并阻止旧版 FDB 执行器发起新的预检。活跃的混合版本集群不支持交还。旧版留下的交还记录可查询。若仍有未结清的旧版交还，离线启用会拒绝修改路由。加载后版本 2 继续保留。

已启用的带库会在 FDB 中记录普通接管尝试。仍有接管未结清时，交还会暂缓。如果接管进程退出或设备命令结果未知，可在确认所有执行器已停止、所有已提交的设备命令已结束后重运行同一离线命令，清理该库的接管记录。

交还检查机械手、完整库存、驱动器就绪状态和原执行轮次的持久预留。驱动器内的磁带原位保留。只有核对机械手和驱动器已释放本集群预留后，才可让外部应用接手。其他带库继续按各自的执行权运行。

## CLI

用明确的库序列号、当前 FDB namespace 和自己生成并保存的操作 UUID。以下示例假定已在环境中设好 `ENDPOINT`、`LIBRARY`、`NAMESPACE`、`RELEASE` 和 `RELOAD`。

```sh
ltfsctl -e "$ENDPOINT" -l "$LIBRARY" library release \
  --namespace "$NAMESPACE" --operation "$RELEASE"
ltfsctl -e "$ENDPOINT" -l "$LIBRARY" library handoff-status \
  --namespace "$NAMESPACE" --operation "$RELEASE"
```

收到 `released` 后，外部应用可以接手。外部应用停止操作并注销其设备注册后，再提交加载并查询原加载回执。

```sh
ltfsctl -e "$ENDPOINT" -l "$LIBRARY" library acquire \
  --namespace "$NAMESPACE" --operation "$RELOAD" --release "$RELEASE"
ltfsctl -e "$ENDPOINT" -l "$LIBRARY" library handoff-status \
  --namespace "$NAMESPACE" --operation "$RELOAD"
```

## Rust SDK 和 HTTP

`FdbClient::release_library`、`acquire_library` 和 `library_handoff_status` 使用同一回执。HTTP 使用 `PUT` 提交或重试，用 `GET` 查询。路径为 `/libraries/{serial}/admin/library/handoffs/{operation}`，带 `x-tapers-catalog-namespace` 请求头。交还的 JSON 请求体为 `{"action":"release"}`，加载为 `{"action":"acquire","release":"<交还操作 UUID>"}`。

| 回执或响应 | 含义 |
| --- | --- |
| HTTP 409 `busy` | 设备或管理任务尚忙，未受理交还。已知的阻塞操作 ID 在诊断中。 |
| `releasing` | 交还尚未取得成功回执。空预留观察不能证明其他节点的命令已经结束；结果未知时不得将设备交给外部应用。 |
| `released` | 设备已交还，集群继续保留目录，停止普通设备接管。 |
| `loading` | 正在重新取得设备并核对库存。 |
| `blocked` | 加载暂停，原因见 `detail`。`attempt_key` 为空时，排除原因后可以使用同一加载操作 UUID 重试。保留 `attempt_key` 表示原设备命令尚未结清，须现场核实。 |
| `online` | 已发布新库存，新的执行轮次已恢复服务。 |
| `failed` | 交还健康检查失败，原因见 `detail`。 |

请求超时后用原 UUID 查询。FDB 的库、池、文件、副本、物理位置引用及历史任务保留。库存重扫可以更新槽位观测。重新加载后执行正常卷访问时须核对实际 LTFS 身份，外部修改过的受管卷需要显式校验或导入。

设备交还经回读确认后，原节点把完成证明同步写入库数据目录的 `.fdb-release-<namespace>-<node>.json`，再尝试提交原 FDB 回执。FDB 暂时不可用时，执行器会按退避重试。原节点重启时会核对检查点并补交 FDB 回执，无需继续操作设备。设备结果未知或在检查点落盘前进程退出时，原操作保持 `releasing`。重新提交同一 UUID 返回已有状态。

对于没有完成检查点的交还，现场确认所有执行器和已提交命令均已退出后，在上述离线命令中追加 `--verify-release-operation "$RELEASE"`。该步骤用 sysfs 中的序列号和 FC 端口缩小设备范围，再核对原机械手和全部驱动器的身份及预留。只有全部设备已无本集群预留或注册时才补交原回执。设备身份无法确认、仍有本集群预留或核验失败时，原操作保持 `releasing`。
