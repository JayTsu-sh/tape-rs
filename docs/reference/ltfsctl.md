# ltfsctl 完整参数参考

集群客户端：库、池、磁带、任务与文件操作。用法说明见[命令行参考手册](../cli-reference.md)。

本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。可选命令随构建特性变化，下面的帮助为本次构建内容。

构建产物 SHA256：`90798e1ea39188c816e0871292134deecbd0995e5ea3713ef57a5b284fe5b406`。共 83 个帮助入口。

## 目录

- [ltfsctl](#ltfsctl)
- [ltfsctl notify](#ltfsctl-notify)
- [ltfsctl notify inject](#ltfsctl-notify-inject)
- [ltfsctl notify create](#ltfsctl-notify-create)
- [ltfsctl notify list](#ltfsctl-notify-list)
- [ltfsctl notify show](#ltfsctl-notify-show)
- [ltfsctl notify read](#ltfsctl-notify-read)
- [ltfsctl notify watch](#ltfsctl-notify-watch)
- [ltfsctl notify ack](#ltfsctl-notify-ack)
- [ltfsctl notify delete](#ltfsctl-notify-delete)
- [ltfsctl fdb](#ltfsctl-fdb)
- [ltfsctl fdb rename](#ltfsctl-fdb-rename)
- [ltfsctl fdb rename prepare](#ltfsctl-fdb-rename-prepare)
- [ltfsctl fdb rename run](#ltfsctl-fdb-rename-run)
- [ltfsctl fdb rename status](#ltfsctl-fdb-rename-status)
- [ltfsctl fdb rename cancel](#ltfsctl-fdb-rename-cancel)
- [ltfsctl fdb fs](#ltfsctl-fdb-fs)
- [ltfsctl fdb fs attributes](#ltfsctl-fdb-fs-attributes)
- [ltfsctl fdb fs prepare](#ltfsctl-fdb-fs-prepare)
- [ltfsctl fdb fs run](#ltfsctl-fdb-fs-run)
- [ltfsctl fdb fs status](#ltfsctl-fdb-fs-status)
- [ltfsctl fdb fs cancel](#ltfsctl-fdb-fs-cancel)
- [ltfsctl fdb control](#ltfsctl-fdb-control)
- [ltfsctl fdb control run](#ltfsctl-fdb-control-run)
- [ltfsctl fdb control status](#ltfsctl-fdb-control-status)
- [ltfsctl fdb control cancel](#ltfsctl-fdb-control-cancel)
- [ltfsctl fdb control list](#ltfsctl-fdb-control-list)
- [ltfsctl fdb drive](#ltfsctl-fdb-drive)
- [ltfsctl fdb drive show](#ltfsctl-fdb-drive-show)
- [ltfsctl fdb drive set](#ltfsctl-fdb-drive-set)
- [ltfsctl fdb init](#ltfsctl-fdb-init)
- [ltfsctl fdb stat](#ltfsctl-fdb-stat)
- [ltfsctl fdb list](#ltfsctl-fdb-list)
- [ltfsctl fdb children](#ltfsctl-fdb-children)
- [ltfsctl fdb get](#ltfsctl-fdb-get)
- [ltfsctl fdb prepare](#ltfsctl-fdb-prepare)
- [ltfsctl fdb upload](#ltfsctl-fdb-upload)
- [ltfsctl fdb status](#ltfsctl-fdb-status)
- [ltfsctl put](#ltfsctl-put)
- [ltfsctl copy](#ltfsctl-copy)
- [ltfsctl get](#ltfsctl-get)
- [ltfsctl delete](#ltfsctl-delete)
- [ltfsctl stat](#ltfsctl-stat)
- [ltfsctl list](#ltfsctl-list)
- [ltfsctl cluster](#ltfsctl-cluster)
- [ltfsctl library](#ltfsctl-library)
- [ltfsctl library native-barrier](#ltfsctl-library-native-barrier)
- [ltfsctl library list](#ltfsctl-library-list)
- [ltfsctl library show](#ltfsctl-library-show)
- [ltfsctl library rescan](#ltfsctl-library-rescan)
- [ltfsctl drive](#ltfsctl-drive)
- [ltfsctl drive list](#ltfsctl-drive-list)
- [ltfsctl drive show](#ltfsctl-drive-show)
- [ltfsctl drive up](#ltfsctl-drive-up)
- [ltfsctl drive down](#ltfsctl-drive-down)
- [ltfsctl drive role](#ltfsctl-drive-role)
- [ltfsctl node](#ltfsctl-node)
- [ltfsctl node list](#ltfsctl-node-list)
- [ltfsctl node show](#ltfsctl-node-show)
- [ltfsctl task](#ltfsctl-task)
- [ltfsctl task list](#ltfsctl-task-list)
- [ltfsctl task show](#ltfsctl-task-show)
- [ltfsctl task cancel](#ltfsctl-task-cancel)
- [ltfsctl file-task](#ltfsctl-file-task)
- [ltfsctl admin](#ltfsctl-admin)
- [ltfsctl pool](#ltfsctl-pool)
- [ltfsctl pool create](#ltfsctl-pool-create)
- [ltfsctl pool list](#ltfsctl-pool-list)
- [ltfsctl pool show](#ltfsctl-pool-show)
- [ltfsctl pool set](#ltfsctl-pool-set)
- [ltfsctl pool delete](#ltfsctl-pool-delete)
- [ltfsctl tape](#ltfsctl-tape)
- [ltfsctl tape import](#ltfsctl-tape-import)
- [ltfsctl tape list](#ltfsctl-tape-list)
- [ltfsctl tape show](#ltfsctl-tape-show)
- [ltfsctl tape assign](#ltfsctl-tape-assign)
- [ltfsctl tape unassign](#ltfsctl-tape-unassign)
- [ltfsctl tape move](#ltfsctl-tape-move)
- [ltfsctl tape offline](#ltfsctl-tape-offline)
- [ltfsctl tape online](#ltfsctl-tape-online)
- [ltfsctl tape set](#ltfsctl-tape-set)
- [ltfsctl tape validate](#ltfsctl-tape-validate)
- [ltfsctl tape reclaim](#ltfsctl-tape-reclaim)

## ltfsctl

```text
ltfsd 客户端

Usage: ltfsctl [OPTIONS] --endpoint <ENDPOINTS> <COMMAND>

Commands:
  notify     外部系统通知：持久订阅、SSE 接收和显式确认
  fdb        FDB 服务：查询、校验下载和持久上传
  put        上传本地文件并等到落带（默认替换已有版本）
  copy       把已提交普通文件的内容校验后复制到另一库/池的空路径；后续改写不会自动同步
  get        从 FDB 查询并校验下载已提交文件；必须指定 --library 和 --pool UUID，不覆盖本地文件
  delete     删除文件并等墓碑落带（不立即回收介质空间）
  stat       查询 FDB 中已提交的路径；必须指定 --library 和 --pool UUID，不存在时失败
  list       分页列出 FDB 中已提交的文件；--dir 只列直接子项，必须指定 --library 和 --pool UUID
  cluster    各节点的自述
  library    带库管理状态
  drive      驱动器管理状态
  node       节点管理状态
  task       集群管理任务
  file-task  查询文件操作任务
  admin      调用指定带库的 HTTP 管理写入接口；设备任务返回 ID 后须查询终态
  pool       池管理
  tape       磁带归属
  help       Print this message or the help of the given subcommand(s)

Options:
  -e, --endpoint <ENDPOINTS>  节点的客户端接口地址，重复给出
  -l, --library <LIBRARY>     目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>           文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                  Print help
```

## ltfsctl notify

```text
外部系统通知：持久订阅、SSE 接收和显式确认

Usage: ltfsctl --endpoint <ENDPOINTS> notify [OPTIONS] <COMMAND>

Commands:
  inject  注入有明确标记的测试通知；不修改真实磁带状态、不访问设备
  create  创建持久订阅；重复执行同配置不会重置进度
  list    列出持久消费者
  show    查看实例标识、确认位置及积压量
  read    查询当前批次；不自动确认
  watch   SSE 持续接收并自动重连；输出不代表处理成功，须另行 ACK
  ack     处理并可靠保存完整批次后显式确认
  delete  删除订阅并明确放弃其未确认通知
  help    Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl notify inject

```text
注入有明确标记的测试通知；不修改真实磁带状态、不访问设备

Usage: ltfsctl notify inject [OPTIONS] --operation <OPERATION> --event-type <EVENT_TYPE> --data <DATA>

Options:
      --operation <OPERATION>    同一次注入重试必须保持此 UUID 和内容不变
      --event-type <EVENT_TYPE>
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
      --data <DATA>              事件data对象的JSON，最大8KiB
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                     Print help
```

## ltfsctl notify create

```text
创建持久订阅；重复执行同配置不会重置进度

Usage: ltfsctl notify create [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --from-start                回放保留的全部历史；默认从创建订阅时的位置开始
      --barcode <BARCODES>
  -l, --library <LIBRARY>         目标带库的真实 changer 序列号；省略时访问默认库
      --event-type <EVENT_TYPES>
  -p, --pool <POOL>               文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                      Print help
```

## ltfsctl notify list

```text
列出持久消费者

Usage: ltfsctl notify list [OPTIONS]

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl notify show

```text
查看实例标识、确认位置及积压量

Usage: ltfsctl notify show [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl notify read

```text
查询当前批次；不自动确认

Usage: ltfsctl notify read [OPTIONS] --generation <GENERATION> <ID>

Arguments:
  <ID>

Options:
      --generation <GENERATION>
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                     Print help
```

## ltfsctl notify watch

```text
SSE 持续接收并自动重连；输出不代表处理成功，须另行 ACK

Usage: ltfsctl notify watch [OPTIONS] --generation <GENERATION> <ID>

Arguments:
  <ID>

Options:
      --generation <GENERATION>
      --event-type <EVENT_TYPES>  指定包含原订阅类型的集合；允许扩大，不允许缩小
  -l, --library <LIBRARY>         目标带库的真实 changer 序列号；省略时访问默认库
      --all-event-types           扩大到全部事件类型
  -p, --pool <POOL>               文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                      Print help
```

## ltfsctl notify ack

```text
处理并可靠保存完整批次后显式确认

Usage: ltfsctl notify ack [OPTIONS] --generation <GENERATION> --delivery-id <DELIVERY_ID> --through <THROUGH> <ID>

Arguments:
  <ID>

Options:
      --generation <GENERATION>
      --delivery-id <DELIVERY_ID>
  -l, --library <LIBRARY>          目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>                文件命名空间的池名称或 UUID；库内有多个池时必填
      --through <THROUGH>
  -h, --help                       Print help
```

## ltfsctl notify delete

```text
删除订阅并明确放弃其未确认通知

Usage: ltfsctl notify delete [OPTIONS] --generation <GENERATION> --abandon-unacknowledged <ID>

Arguments:
  <ID>

Options:
      --generation <GENERATION>
      --abandon-unacknowledged
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                     Print help
```

## ltfsctl fdb

```text
FDB 服务：查询、校验下载和持久上传

Usage: ltfsctl --endpoint <ENDPOINTS> fdb [OPTIONS] <COMMAND>

Commands:
  rename    原双路径重命名；持久准备、运行、查询或取消
  fs        文件、目录和链接变更；先保存逻辑请求，再提交或查询原请求
  control   FDB 持久化管理任务：查询、重试及取消原请求
  drive     FDB 驱动器策略
  init      从三节点冻结屏障初始化全新空目录；需 fdb 特性，不导入旧目录、不操作磁带
  stat      查询库级命名空间的路径元数据（不读磁带）
  list      按 S3 ListObjectsV2 规则列举池内对象；完整对象键字典序
  children  库级目录的一页直接子项；内部诊断用途
  get       按显式池读取普通文件，校验后发布本地文件；不覆盖已有输出
  prepare   准备上传并保存原请求与正文目录；默认只创建，不写磁带、不预留路径
  upload    使用请求目录内的原正文上传；响应不确定时只查询原身份
  status    查询已保存请求的持久化结果
  help      Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb rename

```text
原双路径重命名；持久准备、运行、查询或取消

Usage: ltfsctl fdb rename [OPTIONS] <COMMAND>

Commands:
  prepare  保存两端原身份并只读准备，不登记任务或写带
  run      提交或恢复保存的原重命名，等待持久终态
  status   查询原重命名；null 表示尚未准备或登记
  cancel   取消原未写任务；已结束时返回首次终态
  help     Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb rename prepare

```text
保存两端原身份并只读准备，不登记任务或写带

Usage: ltfsctl fdb rename prepare [OPTIONS] --expected-version <EXPECTED_VERSION> --namespace <NAMESPACE> --request <REQUEST> <FROM> <TO>

Arguments:
  <FROM>
  <TO>

Options:
      --expected-version <EXPECTED_VERSION>                必须匹配的原源节点逻辑版本
      --expected-target-version <EXPECTED_TARGET_VERSION>  替换目标时必须指定其原逻辑版本；省略表示原目标不存在
  -l, --library <LIBRARY>                                  目标带库的真实 changer 序列号；省略时访问默认库
      --no-replace                                         明确要求目标不存在
  -p, --pool <POOL>                                        文件命名空间的池名称或 UUID；库内有多个池时必填
      --namespace <NAMESPACE>                              预期的 Active namespace，防止切换后误改另一目录
      --request <REQUEST>                                  原请求的保存位置；目标必须不存在，文件系统变更使用目录
  -h, --help                                               Print help
```

## ltfsctl fdb rename run

```text
提交或恢复保存的原重命名，等待持久终态

Usage: ltfsctl fdb rename run [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb rename status

```text
查询原重命名；null 表示尚未准备或登记

Usage: ltfsctl fdb rename status [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb rename cancel

```text
取消原未写任务；已结束时返回首次终态

Usage: ltfsctl fdb rename cancel [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb fs

```text
文件、目录和链接变更；先保存逻辑请求，再提交或查询原请求

Usage: ltfsctl fdb fs [OPTIONS] <COMMAND>

Commands:
  attributes  查询已提交节点的原属性、扩展属性及原生链接目标；不访问磁带
  prepare     先保存原意图目录再只读预检；尚不登记任务或写入磁带
  run         提交或重试保存的原请求，等待持久化终态
  status      查询原请求；null 表示尚未登记
  cancel      取消尚未写入的原请求；已结束时返回首次终态
  help        Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb fs attributes

```text
查询已提交节点的原属性、扩展属性及原生链接目标；不访问磁带

Usage: ltfsctl fdb fs attributes [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb fs prepare

```text
先保存原意图目录再只读预检；尚不登记任务或写入磁带

Usage: ltfsctl fdb fs prepare [OPTIONS] --change <CHANGE> --namespace <NAMESPACE> --request <REQUEST> <PATH>

Arguments:
  <PATH>

Options:
      --expected-version <EXPECTED_VERSION>  修改或删除时要求的原逻辑版本；创建时省略
      --change <CHANGE>                      类型化逻辑变更 JSON，例如 {"kind":"create-directory"}
  -l, --library <LIBRARY>                    目标带库的真实 changer 序列号；省略时访问默认库
      --namespace <NAMESPACE>                预期的 Active namespace，防止切换后误改另一目录
  -p, --pool <POOL>                          文件命名空间的池名称或 UUID；库内有多个池时必填
      --request <REQUEST>                    原请求的保存位置；目标必须不存在，文件系统变更使用目录
  -h, --help                                 Print help
```

## ltfsctl fdb fs run

```text
提交或重试保存的原请求，等待持久化终态

Usage: ltfsctl fdb fs run [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb fs status

```text
查询原请求；null 表示尚未登记

Usage: ltfsctl fdb fs status [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb fs cancel

```text
取消尚未写入的原请求；已结束时返回首次终态

Usage: ltfsctl fdb fs cancel [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb control

```text
FDB 持久化管理任务：查询、重试及取消原请求

Usage: ltfsctl fdb control [OPTIONS] <COMMAND>

Commands:
  run     提交或重试保存的原请求，等待持久化终态
  status  查询保存的原请求；null 表示尚未登记
  cancel  原子取消尚未执行的请求；已结束时返回原终态
  list
  help    Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb control run

```text
提交或重试保存的原请求，等待持久化终态

Usage: ltfsctl fdb control run [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb control status

```text
查询保存的原请求；null 表示尚未登记

Usage: ltfsctl fdb control status [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb control cancel

```text
原子取消尚未执行的请求；已结束时返回原终态

Usage: ltfsctl fdb control cancel [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb control list

```text
Usage: ltfsctl fdb control list [OPTIONS]

Options:
      --limit <LIMIT>      管理任务分页目前固定为 100 条 [default: 100]
      --after <AFTER>
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb drive

```text
FDB 驱动器策略

Usage: ltfsctl fdb drive [OPTIONS] <COMMAND>

Commands:
  show
  set   规范 JSON patch：enabled、role(m/r/g)
  help  Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb drive show

```text
Usage: ltfsctl fdb drive show [OPTIONS] <SERIAL>

Arguments:
  <SERIAL>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb drive set

```text
规范 JSON patch：enabled、role(m/r/g)

Usage: ltfsctl fdb drive set [OPTIONS] --patch <PATCH> --namespace <NAMESPACE> --request <REQUEST> <SERIAL>

Arguments:
  <SERIAL>

Options:
      --patch <PATCH>
  -l, --library <LIBRARY>      目标带库的真实 changer 序列号；省略时访问默认库
      --namespace <NAMESPACE>  预期的 Active namespace，防止切换后误改另一目录
  -p, --pool <POOL>            文件命名空间的池名称或 UUID；库内有多个池时必填
      --request <REQUEST>      原请求的保存位置；目标必须不存在，文件系统变更使用目录
  -h, --help                   Print help
```

## ltfsctl fdb init

```text
从三节点冻结屏障初始化全新空目录；需 fdb 特性，不导入旧目录、不操作磁带

Usage: ltfsctl fdb init [OPTIONS] --cluster-file <CLUSTER_FILE> --namespace <NAMESPACE>

Options:
      --cluster-file <CLUSTER_FILE>
  -l, --library <LIBRARY>            目标带库的真实 changer 序列号；省略时访问默认库
      --namespace <NAMESPACE>        稳定初始化身份；重复执行必须使用同一 UUID
  -p, --pool <POOL>                  文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                         Print help
```

## ltfsctl fdb stat

```text
查询库级命名空间的路径元数据（不读磁带）

Usage: ltfsctl fdb stat [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb list

```text
按 S3 ListObjectsV2 规则列举池内对象；完整对象键字典序

Usage: ltfsctl fdb list [OPTIONS]

Options:
      --prefix <PREFIX>                          [default: ""]
      --delimiter <DELIMITER>                    [default: ""]
  -l, --library <LIBRARY>                        目标带库的真实 changer 序列号；省略时访问默认库
      --max-keys <MAX_KEYS>                      [default: 1000]
  -p, --pool <POOL>                              文件命名空间的池名称或 UUID；库内有多个池时必填
      --continuation-token <CONTINUATION_TOKEN>
      --start-after <START_AFTER>
  -h, --help                                     Print help
```

## ltfsctl fdb children

```text
库级目录的一页直接子项；内部诊断用途

Usage: ltfsctl fdb children [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --limit <LIMIT>      [default: 100]
      --after <AFTER>
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb get

```text
按显式池读取普通文件，校验后发布本地文件；不覆盖已有输出

Usage: ltfsctl fdb get [OPTIONS] <PATH> <OUTPUT>

Arguments:
  <PATH>
  <OUTPUT>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb prepare

```text
准备上传并保存原请求与正文目录；默认只创建，不写磁带、不预留路径

Usage: ltfsctl fdb prepare [OPTIONS] --request <REQUEST> <FILE> <PATH>

Arguments:
  <FILE>
  <PATH>

Options:
      --request <REQUEST>                    保存原请求与正文的目录，目标必须不存在
      --expected-version <EXPECTED_VERSION>  覆盖时必须给出当前 FDB 文件版本
  -l, --library <LIBRARY>                    目标带库的真实 changer 序列号；省略时访问默认库
      --barcode <BARCODE>                    可选诊断约束；省略时由服务端从所选池自动选带
  -p, --pool <POOL>                          文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                                 Print help
```

## ltfsctl fdb upload

```text
使用请求目录内的原正文上传；响应不确定时只查询原身份

Usage: ltfsctl fdb upload [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl fdb status

```text
查询已保存请求的持久化结果

Usage: ltfsctl fdb status [OPTIONS] <REQUEST>

Arguments:
  <REQUEST>

Options:
      --details            查询原提交的完整属性与物理身份，无需当前路径或 owner
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl put

```text
上传本地文件并等到落带（默认替换已有版本）

Usage: ltfsctl --endpoint <ENDPOINTS> put [OPTIONS] <FILE> <PATH>

Arguments:
  <FILE>
  <PATH>

Options:
      --new                只创建：路径已有已提交版本时失败
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl copy

```text
把已提交普通文件的内容校验后复制到另一库/池的空路径；后续改写不会自动同步

Usage: ltfsctl --endpoint <ENDPOINTS> copy [OPTIONS] --to-library <TO_LIBRARY> --to-pool <TO_POOL> <SOURCE_PATH> <DESTINATION_PATH>

Arguments:
  <SOURCE_PATH>
  <DESTINATION_PATH>

Options:
      --to-library <TO_LIBRARY>
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
      --to-pool <TO_POOL>
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
      --spool-dir <SPOOL_DIR>    本地临时文件目录；需容纳整个源文件
  -h, --help                     Print help
```

## ltfsctl get

```text
从 FDB 查询并校验下载已提交文件；必须指定 --library 和 --pool UUID，不覆盖本地文件

Usage: ltfsctl --endpoint <ENDPOINTS> get [OPTIONS] <PATH> <OUTPUT>

Arguments:
  <PATH>
  <OUTPUT>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl delete

```text
删除文件并等墓碑落带（不立即回收介质空间）

Usage: ltfsctl --endpoint <ENDPOINTS> delete [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --if-version <IF_VERSION>  仅当当前文件仍是指定的 round.sequence 版本时删除
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                     Print help
```

## ltfsctl stat

```text
查询 FDB 中已提交的路径；必须指定 --library 和 --pool UUID，不存在时失败

Usage: ltfsctl --endpoint <ENDPOINTS> stat [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl list

```text
分页列出 FDB 中已提交的文件；--dir 只列直接子项，必须指定 --library 和 --pool UUID

Usage: ltfsctl --endpoint <ENDPOINTS> list [OPTIONS]

Options:
      --dir <DIR>
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl cluster

```text
各节点的自述

Usage: ltfsctl --endpoint <ENDPOINTS> cluster [OPTIONS]

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl library

```text
带库管理状态

Usage: ltfsctl --endpoint <ENDPOINTS> library [OPTIONS] <COMMAND>

Commands:
  native-barrier  从单一物理节点读取已冻结空目录屏障，不写FDB或磁带
  list            列出集群内所有带库
  show            查询一台带库；单库可省略，多库须指定此参数或 --library
  rescan          显式刷新当前库的设备库存
  help            Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl library native-barrier

```text
从单一物理节点读取已冻结空目录屏障，不写FDB或磁带

Usage: ltfsctl library native-barrier [OPTIONS] --challenge <CHALLENGE>

Options:
      --challenge <CHALLENGE>
  -l, --library <LIBRARY>      目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>            文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                   Print help
```

## ltfsctl library list

```text
列出集群内所有带库

Usage: ltfsctl library list [OPTIONS]

Options:
      --json
      --jsonpp
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
      --no-header
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl library show

```text
查询一台带库；单库可省略，多库须指定此参数或 --library

Usage: ltfsctl library show [OPTIONS] [SERIAL]

Arguments:
  [SERIAL]

Options:
  -a, --attribute <ATTRIBUTE>  只显示指定属性
      --json
  -l, --library <LIBRARY>      目标带库的真实 changer 序列号；省略时访问默认库
      --jsonpp
  -p, --pool <POOL>            文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                   Print help
```

## ltfsctl library rescan

```text
显式刷新当前库的设备库存

Usage: ltfsctl library rescan [OPTIONS]

Options:
      --request <REQUEST>  原请求文件；多库新扫描时为各库请求的目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive

```text
驱动器管理状态

Usage: ltfsctl --endpoint <ENDPOINTS> drive [OPTIONS] <COMMAND>

Commands:
  list
  show
  up
  down
  role
  help  Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive list

```text
Usage: ltfsctl drive list [OPTIONS]

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive show

```text
Usage: ltfsctl drive show [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive up

```text
Usage: ltfsctl drive up [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive down

```text
Usage: ltfsctl drive down [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl drive role

```text
Usage: ltfsctl drive role [OPTIONS] <ID> <ROLE>

Arguments:
  <ID>
  <ROLE>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl node

```text
节点管理状态

Usage: ltfsctl --endpoint <ENDPOINTS> node [OPTIONS] <COMMAND>

Commands:
  list
  show
  help  Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl node list

```text
Usage: ltfsctl node list [OPTIONS]

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl node show

```text
Usage: ltfsctl node show [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl task

```text
集群管理任务

Usage: ltfsctl --endpoint <ENDPOINTS> task [OPTIONS] <COMMAND>

Commands:
  list
  show
  cancel
  help    Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl task list

```text
Usage: ltfsctl task list [OPTIONS]

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl task show

```text
Usage: ltfsctl task show [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl task cancel

```text
Usage: ltfsctl task cancel [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl file-task

```text
查询文件操作任务

Usage: ltfsctl --endpoint <ENDPOINTS> file-task [OPTIONS] <ID>

Arguments:
  <ID>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl admin

```text
调用指定带库的 HTTP 管理写入接口；设备任务返回 ID 后须查询终态

Usage: ltfsctl --endpoint <ENDPOINTS> admin [OPTIONS] <METHOD> <ROUTE>

Arguments:
  <METHOD>  [possible values: post, patch, delete]
  <ROUTE>   `/admin/...` 路径；库由 --library 选择

Options:
      --body <BODY>        JSON 请求体；省略时发送空对象
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl pool

```text
池管理

Usage: ltfsctl --endpoint <ENDPOINTS> pool [OPTIONS] <COMMAND>

Commands:
  create  原子创建池及其根目录；不分配或格式化磁带
  list    列出当前带库的全部FDB池
  show
  set     提交异步策略修改；属性使用HTTP字段名，例如media_restriction L8
  delete  提交删除空池任务；目录或磁带归属非空时失败
  help    Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl pool create

```text
原子创建池及其根目录；不分配或格式化磁带

Usage: ltfsctl pool create [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
      --file-limit <FILE_LIMIT>  [default: 200000]
  -l, --library <LIBRARY>        目标带库的真实 changer 序列号；省略时访问默认库
      --request <REQUEST>        首次调用前保存请求身份；重试须用相同文件和参数
  -p, --pool <POOL>              文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                     Print help
```

## ltfsctl pool list

```text
列出当前带库的全部FDB池

Usage: ltfsctl pool list [OPTIONS]

Options:
      --json
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl pool show

```text
Usage: ltfsctl pool show [OPTIONS] <SELECTOR>

Arguments:
  <SELECTOR>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl pool set

```text
提交异步策略修改；属性使用HTTP字段名，例如media_restriction L8

Usage: ltfsctl pool set [OPTIONS] <SELECTOR> <ATTRIBUTE> <VALUE>

Arguments:
  <SELECTOR>
  <ATTRIBUTE>
  <VALUE>

Options:
      --request <REQUEST>
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl pool delete

```text
提交删除空池任务；目录或磁带归属非空时失败

Usage: ltfsctl pool delete [OPTIONS] <SELECTOR>

Arguments:
  <SELECTOR>

Options:
      --request <REQUEST>
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape

```text
磁带归属

Usage: ltfsctl --endpoint <ENDPOINTS> tape [OPTIONS] <COMMAND>

Commands:
  import    导入已有 LTFS 索引或刷新已分配卷的 FDB 目录；不格式化或擦除
  list      列出磁带状态和容量
  show      查询磁带详情
  assign    异步检查/初始化磁带并归入池；格式化须 --yes
  unassign  异步证明无引用后解除池归属，不擦除介质
  move      异步移动介质到 homeslot 或 ieslot
  offline   将已分配磁带设为离线；返回管理任务号
  online    实际装带验证后解除离线；返回管理任务号
  set       设置黑名单及离线属性；返回管理任务号
  validate  异步核验标签、LTFS 索引和结构；不读取全部文件正文
  reclaim   回收：FDB 原生模式尚未开放，会拒绝此命令。 目录模式需要两台驱动器，后台搬迁有效文件后重新格式化源带
  help      Print this message or the help of the given subcommand(s)

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape import

```text
导入已有 LTFS 索引或刷新已分配卷的 FDB 目录；不格式化或擦除

Usage: ltfsctl tape import [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  要导入的条码；使用 --request 恢复原请求时省略

Options:
      --request <REQUEST>  单盘请求文件或多盘请求目录；省略时在本机状态目录自动保存
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -P, --path <PATH>        首次导入的池内目录，默认 /IMPORT；已分配卷刷新沿用原路径映射
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -R, --merge-tapes        首次导入直接合并到目录，不追加条码；刷新不另建导入目录
  -h, --help               Print help
```

## ltfsctl tape list

```text
列出磁带状态和容量

Usage: ltfsctl tape list [OPTIONS]

Options:
      --json
      --jsonpp
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
      --no-header
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
      --unit <UNIT>        [possible values: decimal, binary]
  -h, --help               Print help
```

## ltfsctl tape show

```text
查询磁带详情

Usage: ltfsctl tape show [OPTIONS] <BARCODE>

Arguments:
  <BARCODE>

Options:
  -a, --attribute <ATTRIBUTE>  只显示指定属性
      --json
  -l, --library <LIBRARY>      目标带库的真实 changer 序列号；省略时访问默认库
      --jsonpp
  -p, --pool <POOL>            文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                   Print help
```

## ltfsctl tape assign

```text
异步检查/初始化磁带并归入池；格式化须 --yes

Usage: ltfsctl tape assign [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>  单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
      --yes                确认允许初始化/格式化指定介质
  -f, --force-format       允许格式化已有文件的介质；必须同时 --yes
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape unassign

```text
异步证明无引用后解除池归属，不擦除介质

Usage: ltfsctl tape unassign [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>  单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape move

```text
异步移动介质到 homeslot 或 ieslot

Usage: ltfsctl tape move [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>    单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>    目标带库的真实 changer 序列号；省略时访问默认库
  -L, --location <LOCATION>  目的位置；已分池卷出库前须 offline [possible values: homeslot, ieslot]
  -p, --pool <POOL>          文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                 Print help
```

## ltfsctl tape offline

```text
将已分配磁带设为离线；返回管理任务号

Usage: ltfsctl tape offline [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>      单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>      目标带库的真实 changer 序列号；省略时访问默认库
  -o, --offline-msg <MESSAGE>  离线说明，1–63 个英文字母、数字、空格、下划线或连字符；默认当天日期
  -p, --pool <POOL>            文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help                   Print help
```

## ltfsctl tape online

```text
实际装带验证后解除离线；返回管理任务号

Usage: ltfsctl tape online [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>  单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape set

```text
设置黑名单及离线属性；返回管理任务号

Usage: ltfsctl tape set [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>  单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
      --patch <PATCH>      JSON 属性：blacklisted、online=false、offline_message
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape validate

```text
异步核验标签、LTFS 索引和结构；不读取全部文件正文

Usage: ltfsctl tape validate [OPTIONS] [BARCODES]...

Arguments:
  [BARCODES]...  目标条码；恢复时省略并指定原请求文件

Options:
      --request <REQUEST>  单盘原请求文件，或多盘新请求目录
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```

## ltfsctl tape reclaim

```text
回收：FDB 原生模式尚未开放，会拒绝此命令。 目录模式需要两台驱动器，后台搬迁有效文件后重新格式化源带

Usage: ltfsctl tape reclaim [OPTIONS] <BARCODE>

Arguments:
  <BARCODE>

Options:
  -l, --library <LIBRARY>  目标带库的真实 changer 序列号；省略时访问默认库
  -p, --pool <POOL>        文件命名空间的池名称或 UUID；库内有多个池时必填
  -h, --help               Print help
```
