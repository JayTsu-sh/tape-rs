# tape-rs 完整参数参考

直接 SCSI/LTFS 设备工具。用法说明见[命令行参考手册](../cli-reference.md)。

本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。可选命令随构建特性变化，下面的帮助为本次构建内容。

构建产物 SHA256：`6dd951953a80508a708165f7670379770861b4c74626ef843215709fc804afff`。共 39 个帮助入口。

## 目录

- [tape-rs](#tape-rs)
- [tape-rs inquiry](#tape-rs-inquiry)
- [tape-rs inventory](#tape-rs-inventory)
- [tape-rs move](#tape-rs-move)
- [tape-rs load](#tape-rs-load)
- [tape-rs unload](#tape-rs-unload)
- [tape-rs rewind](#tape-rs-rewind)
- [tape-rs position](#tape-rs-position)
- [tape-rs write](#tape-rs-write)
- [tape-rs read](#tape-rs-read)
- [tape-rs status](#tape-rs-status)
- [tape-rs init](#tape-rs-init)
- [tape-rs exchange](#tape-rs-exchange)
- [tape-rs prevent-removal](#tape-rs-prevent-removal)
- [tape-rs import](#tape-rs-import)
- [tape-rs export](#tape-rs-export)
- [tape-rs drive-load](#tape-rs-drive-load)
- [tape-rs drive-unload](#tape-rs-drive-unload)
- [tape-rs space](#tape-rs-space)
- [tape-rs locate](#tape-rs-locate)
- [tape-rs erase](#tape-rs-erase)
- [tape-rs format](#tape-rs-format)
- [tape-rs log-sense](#tape-rs-log-sense)
- [tape-rs report-density](#tape-rs-report-density)
- [tape-rs diagnostic](#tape-rs-diagnostic)
- [tape-rs mkltfs](#tape-rs-mkltfs)
- [tape-rs ltfs-list](#tape-rs-ltfs-list)
- [tape-rs ltfs-read](#tape-rs-ltfs-read)
- [tape-rs pr-status](#tape-rs-pr-status)
- [tape-rs pr-fence](#tape-rs-pr-fence)
- [tape-rs ltfs-takeover](#tape-rs-ltfs-takeover)
- [tape-rs pr-release](#tape-rs-pr-release)
- [tape-rs ltfs-verify](#tape-rs-ltfs-verify)
- [tape-rs ltfs-write](#tape-rs-ltfs-write)
- [tape-rs catalog](#tape-rs-catalog)
- [tape-rs catalog sync](#tape-rs-catalog-sync)
- [tape-rs catalog list](#tape-rs-catalog-list)
- [tape-rs catalog find](#tape-rs-catalog-find)
- [tape-rs catalog show](#tape-rs-catalog-show)

## tape-rs

```text
IBM TS4300 Tape Library SCSI Control Tool

Usage: tape-rs <COMMAND>

Commands:
  inquiry          查询设备信息（INQUIRY）：自动扫描 /dev/sg*，按 LU NAA 合并控制路径
  inventory        显示带库库存状态
  move             移动磁带
  load             装载磁带到驱动器
  unload           从驱动器卸载磁带
  rewind           磁带倒带
  position         读取磁带位置
  write            写入文件到磁带
  read             从磁带读取文件
  status           检查磁带机状态
  init             初始化换带器 element 状态（重新扫描全库）
  exchange         三方交换介质（A→B，原 B→C）
  prevent-removal  锁定/解锁介质移除
  import           通过 I/E 口导入磁带到存储槽
  export           从存储槽导出磁带到 I/E 口
  drive-load       磁带机内部装载（LOAD）
  drive-unload     磁带机内部弹出（UNLOAD）
  space            SPACE：跳过 block / filemark / 到 EOD
  locate           LOCATE：快速定位到指定 block
  erase            ERASE：抹带
  format           FORMAT MEDIUM：格式化介质
  log-sense        LOG SENSE：读取 log page（默认读 TapeAlert 0x2E）
  report-density   REPORT DENSITY SUPPORT：显示支持的 density
  diagnostic       SEND DIAGNOSTIC：触发自检并读取结果
  mkltfs           mkltfs：把磁带初始化成 LTFS 格式（破坏性操作）
  ltfs-list        ltfs-list：列出 LTFS 卷的所有文件
  ltfs-read        ltfs-read：从 LTFS 卷读取文件到本地
  pr-status        pr-status：读设备的持久预留注册表与持有者（只读）
  pr-fence         pr-fence：设备层隔离。注册本轮键、抢占现有持有者并回读确认
  ltfs-takeover    ltfs-takeover：新执行者接管一个驱动器里的卷。隔离并回读确认 → 挂载（恢复协议）→ 尾部不完整（写到一半的数据或索引）时自动收尾，使卷重新可写
  pr-release       pr-release：释放预留并注销本轮键（计划移交）
  ltfs-verify      ltfs-verify：读出文件并与索引里的哈希扩展属性比对
  ltfs-write       ltfs-write：把本地文件追加写入 LTFS 卷
  catalog          跨盘 catalog（SQLite）：sync / list / find / show
  help             Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-rs inquiry

```text
查询设备信息（INQUIRY）：自动扫描 /dev/sg*，按 LU NAA 合并控制路径

Usage: tape-rs inquiry [OPTIONS]

Options:
      --json  以 JSON 格式输出（便于程序解析）
  -h, --help  Print help
```

## tape-rs inventory

```text
显示带库库存状态

Usage: tape-rs inventory [OPTIONS]

Options:
  -d, --device <DEVICE>    换带器设备路径 (如 /dev/sg2) [default: /dev/sg2]
      --catalog <CATALOG>  catalog 路径（覆盖 $TAPE_RS_CATALOG / XDG 默认）； 没有 catalog 或查不到时不影响 inventory，仅跳过缓存容量。
      --no-drive-scan      跳过扫描 /dev/sg* 自动读 drive 实时容量。跳过后仅回落到 catalog。
  -h, --help               Print help
```

## tape-rs move

```text
移动磁带

Usage: tape-rs move [OPTIONS] --from-slot <FROM_SLOT>

Options:
  -d, --device <DEVICE>        换带器设备路径 [default: /dev/sg2]
      --from-slot <FROM_SLOT>  源 slot 编号 (1-based)
      --to-slot <TO_SLOT>      目标 slot 编号 (1-based) 或 drive 编号
      --to-drive <TO_DRIVE>    目标 drive 编号 (0-based)
  -h, --help                   Print help
```

## tape-rs load

```text
装载磁带到驱动器

Usage: tape-rs load [OPTIONS] --slot <SLOT>

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
      --slot <SLOT>      源 slot 编号 (1-based)
      --drive <DRIVE>    目标 drive 编号 (0-based) [default: 0]
  -h, --help             Print help
```

## tape-rs unload

```text
从驱动器卸载磁带

Usage: tape-rs unload [OPTIONS] --slot <SLOT>

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
      --drive <DRIVE>    驱动器编号 (0-based) [default: 0]
      --slot <SLOT>      目标 slot 编号 (1-based)
  -h, --help             Print help
```

## tape-rs rewind

```text
磁带倒带

Usage: tape-rs rewind [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机 sg 设备路径 (如 /dev/sg1) [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs position

```text
读取磁带位置

Usage: tape-rs position [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机 sg 设备路径 [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs write

```text
写入文件到磁带

Usage: tape-rs write [OPTIONS] --file <FILE>

Options:
  -d, --device <DEVICE>          磁带机 sg 设备路径 [default: /dev/sg1]
  -f, --file <FILE>              要写入的文件路径
      --block-size <BLOCK_SIZE>  块大小（字节，默认 512KB） [default: 524288]
  -h, --help                     Print help
```

## tape-rs read

```text
从磁带读取文件

Usage: tape-rs read [OPTIONS] --output <OUTPUT>

Options:
  -d, --device <DEVICE>          磁带机 sg 设备路径 [default: /dev/sg1]
  -o, --output <OUTPUT>          输出文件路径
      --block-size <BLOCK_SIZE>  块大小（字节，默认 512KB） [default: 524288]
      --max-size <MAX_SIZE>      最大读取字节数，0 表示不设上限（由 filemark / BLANK CHECK 终止） [default: 0]
  -h, --help                     Print help
```

## tape-rs status

```text
检查磁带机状态

Usage: tape-rs status [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机 sg 设备路径 [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs init

```text
初始化换带器 element 状态（重新扫描全库）

Usage: tape-rs init [OPTIONS]

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
  -h, --help             Print help
```

## tape-rs exchange

```text
三方交换介质（A→B，原 B→C）

Usage: tape-rs exchange [OPTIONS] --source <SOURCE> --dest1 <DEST1> --dest2 <DEST2>

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
      --source <SOURCE>  源绝对地址（十六进制或十进制）
      --dest1 <DEST1>    目标1绝对地址
      --dest2 <DEST2>    目标2绝对地址
  -h, --help             Print help
```

## tape-rs prevent-removal

```text
锁定/解锁介质移除

Usage: tape-rs prevent-removal [OPTIONS] --prevent <PREVENT>

Options:
  -d, --device <DEVICE>    换带器或磁带机设备路径 [default: /dev/sg2]
      --prevent <PREVENT>  true=禁止, false=允许 [possible values: true, false]
  -h, --help               Print help
```

## tape-rs import

```text
通过 I/E 口导入磁带到存储槽

Usage: tape-rs import [OPTIONS] --ie <IE> --slot <SLOT>

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
      --ie <IE>          I/E 偏移（0-based）
      --slot <SLOT>      目标 slot (1-based)
  -h, --help             Print help
```

## tape-rs export

```text
从存储槽导出磁带到 I/E 口

Usage: tape-rs export [OPTIONS] --slot <SLOT> --ie <IE>

Options:
  -d, --device <DEVICE>  换带器设备路径 [default: /dev/sg2]
      --slot <SLOT>      源 slot (1-based)
      --ie <IE>          I/E 偏移（0-based）
  -h, --help             Print help
```

## tape-rs drive-load

```text
磁带机内部装载（LOAD）

Usage: tape-rs drive-load [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs drive-unload

```text
磁带机内部弹出（UNLOAD）

Usage: tape-rs drive-unload [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs space

```text
SPACE：跳过 block / filemark / 到 EOD

Usage: tape-rs space [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --mode <MODE>      模式：block | filemark | eod [default: filemark]
      --count <COUNT>    数量（filemark / block 模式下生效，可为负数表示反向） [default: 1]
  -h, --help             Print help
```

## tape-rs locate

```text
LOCATE：快速定位到指定 block

Usage: tape-rs locate [OPTIONS] --block <BLOCK>

Options:
  -d, --device <DEVICE>        磁带机设备路径 [default: /dev/sg1]
      --partition <PARTITION>  目标 partition [default: 0]
      --block <BLOCK>          目标 block 号
      --change-partition       是否切换 partition
  -h, --help                   Print help
```

## tape-rs erase

```text
ERASE：抹带

Usage: tape-rs erase [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --long             整带长 erase（非常耗时）
  -h, --help             Print help
```

## tape-rs format

```text
FORMAT MEDIUM：格式化介质

Usage: tape-rs format [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --mode <MODE>      format 类型：0=默认（单 partition），1=按默认 partition 参数 [default: 0]
      --verify           格式化后校验
      --yes-destroy      确认会擦除磁带；不带此 flag 时拒绝执行
  -h, --help             Print help
```

## tape-rs log-sense

```text
LOG SENSE：读取 log page（默认读 TapeAlert 0x2E）

Usage: tape-rs log-sense [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --page <PAGE>      Log page code（十六进制，如 2E） [default: 2E]
      --raw              以 hex 打印原始数据
  -h, --help             Print help
```

## tape-rs report-density

```text
REPORT DENSITY SUPPORT：显示支持的 density

Usage: tape-rs report-density [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --media-only       仅返回当前介质支持的 density
  -h, --help             Print help
```

## tape-rs diagnostic

```text
SEND DIAGNOSTIC：触发自检并读取结果

Usage: tape-rs diagnostic [OPTIONS]

Options:
  -d, --device <DEVICE>  磁带机设备路径 [default: /dev/sg1]
      --foreground       前台自检（阻塞至完成）
  -h, --help             Print help
```

## tape-rs mkltfs

```text
mkltfs：把磁带初始化成 LTFS 格式（破坏性操作）

Usage: tape-rs mkltfs [OPTIONS] --volume-id <VOLUME_ID>

Options:
  -d, --device <DEVICE>          [default: /dev/sg1]
      --volume-id <VOLUME_ID>    Volume Identifier（6 字符以内，通常 = barcode）
      --owner <OWNER>            Owner Identifier（14 字符以内） [default: ""]
      --block-size <BLOCK_SIZE>  块大小（字节，默认 512KB） [default: 524288]
      --compression              LTFS label 里的 compression 标记
      --yes-destroy              必须显式加这个参数才会真正执行（避免误操作）
      --quick                    快速模式：跳过 MODE SELECT + FORMAT MEDIUM，只重写 labels/index/MAM。 仅适用于已经是 LTFS 2-partition 格式的磁带；裸带必须先跑普通 mkltfs。
  -h, --help                     Print help
```

## tape-rs ltfs-list

```text
ltfs-list：列出 LTFS 卷的所有文件

Usage: tape-rs ltfs-list [OPTIONS]

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs ltfs-read

```text
ltfs-read：从 LTFS 卷读取文件到本地

Usage: tape-rs ltfs-read [OPTIONS] --name <NAME> --output <OUTPUT>

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
      --name <NAME>      磁带上的文件路径（'/' 分隔，不以 '/' 开头）
  -o, --output <OUTPUT>  输出本地文件路径
  -h, --help             Print help
```

## tape-rs pr-status

```text
pr-status：读设备的持久预留注册表与持有者（只读）

Usage: tape-rs pr-status [OPTIONS]

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
  -h, --help             Print help
```

## tape-rs pr-fence

```text
pr-fence：设备层隔离。注册本轮键、抢占现有持有者并回读确认

Usage: tape-rs pr-fence [OPTIONS] --node <NODE> --round <ROUND>

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
      --node <NODE>      节点号
      --round <ROUND>    执行轮次
  -h, --help             Print help
```

## tape-rs ltfs-takeover

```text
ltfs-takeover：新执行者接管一个驱动器里的卷。隔离并回读确认 → 挂载（恢复协议）→ 尾部不完整（写到一半的数据或索引）时自动收尾，使卷重新可写

Usage: tape-rs ltfs-takeover [OPTIONS] --node <NODE> --round <ROUND>

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
      --node <NODE>
      --round <ROUND>
      --salvage          把未索引的数据打捞到 _ltfs_lostandfound/（默认放弃，只记录块范围）
      --report-only      只报告，不收尾
  -h, --help             Print help
```

## tape-rs pr-release

```text
pr-release：释放预留并注销本轮键（计划移交）

Usage: tape-rs pr-release [OPTIONS] --node <NODE> --round <ROUND>

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
      --node <NODE>
      --round <ROUND>
  -h, --help             Print help
```

## tape-rs ltfs-verify

```text
ltfs-verify：读出文件并与索引里的哈希扩展属性比对

Usage: tape-rs ltfs-verify [OPTIONS]

Options:
  -d, --device <DEVICE>  [default: /dev/sg1]
      --name <NAME>      磁带上的文件路径；不给则校验全部文件
      --xattrs           同时打印每个文件的扩展属性
  -h, --help             Print help
```

## tape-rs ltfs-write

```text
ltfs-write：把本地文件追加写入 LTFS 卷

Usage: tape-rs ltfs-write [OPTIONS] --file <FILE> --name <NAME>

Options:
  -d, --device <DEVICE>     [default: /dev/sg1]
  -f, --file <FILE>         本地文件路径
      --name <NAME>         磁带上的目标路径（'/' 分隔）
      --xattr <KEY=VALUE>   附带的文本扩展属性 key=value，可重复。key 是索引里的原始名字 （IBM LTFS 在 Linux 上会显示成 user.<key>）
      --md5                 同时记录 ltfs.hash.md5sum（默认只记 sha256sum）
      --guard <NODE:ROUND>  提交前核对预留持有者是不是 (节点号, 轮次) 对应的键，形如 1:7
  -h, --help                Print help
```

## tape-rs catalog

```text
跨盘 catalog（SQLite）：sync / list / find / show

Usage: tape-rs catalog <COMMAND>

Commands:
  sync  把当前 sg 设备上 mount 的 LTFS 卷灌进 catalog。
  list  列出 catalog 里所有卷。
  find  按路径模糊搜索文件（支持 SQL LIKE 通配符 `%`、`_`）。
  show  列出某卷的所有文件（uuid 或 barcode 均可）。
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## tape-rs catalog sync

```text
把当前 sg 设备上 mount 的 LTFS 卷灌进 catalog。

Usage: tape-rs catalog sync [OPTIONS]

Options:
  -d, --device <DEVICE>    [default: /dev/sg1]
      --catalog <CATALOG>  覆盖 catalog 文件路径（优先级：该参数 > $TAPE_RS_CATALOG > XDG 默认）。
      --barcode <BARCODE>  手工指定 barcode，跳过 MAM 0x0806 读取。
  -h, --help               Print help
```

## tape-rs catalog list

```text
列出 catalog 里所有卷。

Usage: tape-rs catalog list [OPTIONS]

Options:
      --catalog <CATALOG>
  -h, --help               Print help
```

## tape-rs catalog find

```text
按路径模糊搜索文件（支持 SQL LIKE 通配符 `%`、`_`）。

Usage: tape-rs catalog find [OPTIONS] <PATTERN>

Arguments:
  <PATTERN>  搜索串；不含 `%`/`_` 时按 "包含" 匹配。

Options:
      --catalog <CATALOG>
      --limit <LIMIT>      结果上限 [default: 200]
  -h, --help               Print help
```

## tape-rs catalog show

```text
列出某卷的所有文件（uuid 或 barcode 均可）。

Usage: tape-rs catalog show [OPTIONS] <KEY>

Arguments:
  <KEY>  卷的 UUID 或 barcode

Options:
      --catalog <CATALOG>
  -h, --help               Print help
```
