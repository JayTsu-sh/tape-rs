# tape-library 完整参数参考

独占设备的单机逻辑库工具。用法说明见[命令行参考手册](../cli-reference.md)。

本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。可选命令随构建特性变化，下面的帮助为本次构建内容。

构建产物 SHA256：`fa97b2adcb5eaa04b8b872d4697c0b53afd21936cf67154d216692ae22feeee5`。共 39 个帮助入口。

## 目录

- [tape-library](#tape-library)
- [tape-library task](#tape-library-task)
- [tape-library task list](#tape-library-task-list)
- [tape-library task show](#tape-library-task-show)
- [tape-library task cancel](#tape-library-task-cancel)
- [tape-library task clearhistory](#tape-library-task-clearhistory)
- [tape-library status](#tape-library-status)
- [tape-library library](#tape-library-library)
- [tape-library library list](#tape-library-library-list)
- [tape-library library show](#tape-library-library-show)
- [tape-library library rescan](#tape-library-library-rescan)
- [tape-library pool](#tape-library-pool)
- [tape-library pool create](#tape-library-pool-create)
- [tape-library pool delete](#tape-library-pool-delete)
- [tape-library pool list](#tape-library-pool-list)
- [tape-library pool show](#tape-library-pool-show)
- [tape-library pool set](#tape-library-pool-set)
- [tape-library tape](#tape-library-tape)
- [tape-library tape list](#tape-library-tape-list)
- [tape-library tape show](#tape-library-tape-show)
- [tape-library tape offline](#tape-library-tape-offline)
- [tape-library tape online](#tape-library-tape-online)
- [tape-library tape move](#tape-library-tape-move)
- [tape-library tape validate](#tape-library-tape-validate)
- [tape-library tape objects](#tape-library-tape-objects)
- [tape-library tape restore](#tape-library-tape-restore)
- [tape-library drive](#tape-library-drive)
- [tape-library drive list](#tape-library-drive-list)
- [tape-library drive show](#tape-library-drive-show)
- [tape-library drive up](#tape-library-drive-up)
- [tape-library drive down](#tape-library-drive-down)
- [tape-library drive role](#tape-library-drive-role)
- [tape-library assign](#tape-library-assign)
- [tape-library unassign](#tape-library-unassign)
- [tape-library blacklist](#tape-library-blacklist)
- [tape-library clear-blacklist](#tape-library-clear-blacklist)
- [tape-library read](#tape-library-read)
- [tape-library list](#tape-library-list)
- [tape-library write](#tape-library-write)

## tape-library

```text
管理多卷逻辑库；显式 assignment 后才允许按路径自动装带

Usage: tape-library [OPTIONS] <COMMAND>

Commands:
  task             查询/取消本地持久任务；只需 --library-state，不打开设备
  status           查询库存、assignment、blacklist 和 drive 资源；不搬带
  library          查询单逻辑库与重新读取库存（不初始化机械手）
  pool             管理独立池实体；不格式化或写磁带
  tape             查询磁带、离线/上线；上线会装带验证 LTFS 索引
  drive            查询和启停本实例驱动调度资格（不是硬件断电）
  assign           将已知条码分配到逻辑池；不写介质
  unassign         取消已归槽卷的 assignment
  blacklist        禁止卷访问并保存原因
  clear-blacklist  显式清除 blacklist
  read             按逻辑路径读取文件到本地；会自动装带，结束后归槽
  list             列举逻辑卷中的文件；会自动装带，结束后归槽
  write            追加文件并完整提交；默认拒绝覆盖，不格式化磁带
  help             Print this message or the help of the given subcommand(s)

Options:
      --library-changer <LIBRARY_CHANGER>                逻辑库 changer 设备（必须与 LE/ltfsd 互斥使用）
      --library-changer-serial <LIBRARY_CHANGER_SERIAL>  预期 changer VPD 序列号
      --library-drive <LIBRARY_DRIVE>                    允许调度的 drive，格式 SERIAL=/dev/sgN，可重复
      --library-state <LIBRARY_STATE>                    资源状态 SQLite 路径；禁止复用 ltfsd catalog
      --library-node <LIBRARY_NODE>                      独占执行节点号 [default: 1]
      --library-round <LIBRARY_ROUND>                    运维分配的单调递增执行轮次（1..2^48），不得复用旧轮次
  -h, --help                                             Print help
```

## tape-library task

```text
查询/取消本地持久任务；只需 --library-state，不打开设备

Usage: tape-library task <COMMAND>

Commands:
  list
  show
  cancel
  clearhistory  只清理终态历史，保留运行中任务；不可恢复
  help          Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-library task list

```text
Usage: tape-library task list

Options:
  -h, --help  Print help
```

## tape-library task show

```text
Usage: tape-library task show <ID>

Arguments:
  <ID>

Options:
  -h, --help  Print help
```

## tape-library task cancel

```text
Usage: tape-library task cancel <ID>

Arguments:
  <ID>

Options:
  -h, --help  Print help
```

## tape-library task clearhistory

```text
只清理终态历史，保留运行中任务；不可恢复

Usage: tape-library task clearhistory [OPTIONS]

Options:
      --yes
  -h, --help  Print help
```

## tape-library status

```text
查询库存、assignment、blacklist 和 drive 资源；不搬带

Usage: tape-library status

Options:
  -h, --help  Print help
```

## tape-library library

```text
查询单逻辑库与重新读取库存（不初始化机械手）

Usage: tape-library library <COMMAND>

Commands:
  list
  show
  rescan
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-library library list

```text
Usage: tape-library library list

Options:
  -h, --help  Print help
```

## tape-library library show

```text
Usage: tape-library library show

Options:
  -h, --help  Print help
```

## tape-library library rescan

```text
Usage: tape-library library rescan

Options:
  -h, --help  Print help
```

## tape-library pool

```text
管理独立池实体；不格式化或写磁带

Usage: tape-library pool <COMMAND>

Commands:
  create
  delete
  list
  show
  set
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-library pool create

```text
Usage: tape-library pool create <NAME>

Arguments:
  <NAME>

Options:
  -h, --help  Print help
```

## tape-library pool delete

```text
Usage: tape-library pool delete <NAME>

Arguments:
  <NAME>

Options:
  -h, --help  Print help
```

## tape-library pool list

```text
Usage: tape-library pool list

Options:
  -h, --help  Print help
```

## tape-library pool show

```text
Usage: tape-library pool show <NAME>

Arguments:
  <NAME>

Options:
  -h, --help  Print help
```

## tape-library pool set

```text
Usage: tape-library pool set <NAME> <ATTRIBUTE> <VALUE>

Arguments:
  <NAME>
  <ATTRIBUTE>
  <VALUE>

Options:
  -h, --help  Print help
```

## tape-library tape

```text
查询磁带、离线/上线；上线会装带验证 LTFS 索引

Usage: tape-library tape <COMMAND>

Commands:
  list
  show
  offline
  online
  move      机械搬至 homeslot/ieslot；出 I/E 前必须先 offline
  validate  挂载检查，--full-read 逐文件读取并检查已有哈希
  objects   解释 EE 保存对象的原类型、路径和链接
  restore   从 EE 保存对象恢复普通文件/目录/链接到全新目录，不创建 HSM stub
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-library tape list

```text
Usage: tape-library tape list

Options:
  -h, --help  Print help
```

## tape-library tape show

```text
Usage: tape-library tape show <BARCODE>

Arguments:
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library tape offline

```text
Usage: tape-library tape offline <BARCODE> <MESSAGE>

Arguments:
  <BARCODE>
  <MESSAGE>

Options:
  -h, --help  Print help
```

## tape-library tape online

```text
Usage: tape-library tape online <BARCODE>

Arguments:
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library tape move

```text
机械搬至 homeslot/ieslot；出 I/E 前必须先 offline

Usage: tape-library tape move <BARCODE> <LOCATION>

Arguments:
  <BARCODE>
  <LOCATION>

Options:
  -h, --help  Print help
```

## tape-library tape validate

```text
挂载检查，--full-read 逐文件读取并检查已有哈希

Usage: tape-library tape validate [OPTIONS] <BARCODE>

Arguments:
  <BARCODE>

Options:
      --full-read
  -h, --help       Print help
```

## tape-library tape objects

```text
解释 EE 保存对象的原类型、路径和链接

Usage: tape-library tape objects <BARCODE>

Arguments:
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library tape restore

```text
从 EE 保存对象恢复普通文件/目录/链接到全新目录，不创建 HSM stub

Usage: tape-library tape restore <BARCODE> <DESTINATION>

Arguments:
  <BARCODE>
  <DESTINATION>

Options:
  -h, --help  Print help
```

## tape-library drive

```text
查询和启停本实例驱动调度资格（不是硬件断电）

Usage: tape-library drive <COMMAND>

Commands:
  list
  show
  up
  down
  role
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-library drive list

```text
Usage: tape-library drive list

Options:
  -h, --help  Print help
```

## tape-library drive show

```text
Usage: tape-library drive show <SERIAL>

Arguments:
  <SERIAL>

Options:
  -h, --help  Print help
```

## tape-library drive up

```text
Usage: tape-library drive up <SERIAL>

Arguments:
  <SERIAL>

Options:
  -h, --help  Print help
```

## tape-library drive down

```text
Usage: tape-library drive down <SERIAL>

Arguments:
  <SERIAL>

Options:
  -h, --help  Print help
```

## tape-library drive role

```text
Usage: tape-library drive role <SERIAL> <ROLE>

Arguments:
  <SERIAL>
  <ROLE>

Options:
  -h, --help  Print help
```

## tape-library assign

```text
将已知条码分配到逻辑池；不写介质

Usage: tape-library assign <POOL> <BARCODE>

Arguments:
  <POOL>
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library unassign

```text
取消已归槽卷的 assignment

Usage: tape-library unassign <BARCODE>

Arguments:
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library blacklist

```text
禁止卷访问并保存原因

Usage: tape-library blacklist <BARCODE> <REASON>

Arguments:
  <BARCODE>
  <REASON>

Options:
  -h, --help  Print help
```

## tape-library clear-blacklist

```text
显式清除 blacklist

Usage: tape-library clear-blacklist <BARCODE>

Arguments:
  <BARCODE>

Options:
  -h, --help  Print help
```

## tape-library read

```text
按逻辑路径读取文件到本地；会自动装带，结束后归槽

Usage: tape-library read <PATH> <OUTPUT>

Arguments:
  <PATH>
  <OUTPUT>

Options:
  -h, --help  Print help
```

## tape-library list

```text
列举逻辑卷中的文件；会自动装带，结束后归槽

Usage: tape-library list <PATH>

Arguments:
  <PATH>

Options:
  -h, --help  Print help
```

## tape-library write

```text
追加文件并完整提交；默认拒绝覆盖，不格式化磁带

Usage: tape-library write [OPTIONS] <PATH> <INPUT>

Arguments:
  <PATH>
  <INPUT>

Options:
      --overwrite
  -h, --help       Print help
```
