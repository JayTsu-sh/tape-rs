# 周期磁带监控：SCSI 采样语义与保守故障归因

日期：2026-11-04。范围为 IBM LTO、TapeAlert、写保护、MAM 容量与 sense 分类的原始资料核对；本轮没有执行硬件命令、启动 EE 或修改 Rust。建议与标准/厂商事实在下文分开。

## 实现前必须采用的边界

**“不移动、不写带”不等于“观察完全无副作用”。LOG SENSE 可能消耗或重置诊断证据。** 只能由当前设备 owner 在既有串行 I/O 队列里采样；一次采到的错误必须先留存，后续零值、换 owner、重启或成功 rescan 不应自动抹掉。周期采样已装载卷能补充已知状态，不能发现所有尚未读取区域的损坏。

本报告主要资料：

- [IBM LTO SCSI Reference GA32-0928-08，2026-02-16](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf)，以下简称 **IBM-08**。适用于 IBM LTO5 及后续世代的参考文档；具体固件行为仍须实际核验。
- [IBM LTO SCSI Reference GA32-0928-05，2022-02-28](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference_GA32-0928-05%20%28EXTERNAL%29_0.pdf)，以下简称 **IBM-05**，用于交叉核对。
- [官方 LTFS 源码](https://github.com/LinearTapeFileSystem/ltfs/tree/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb)，本轮固定 commit `4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb`；下文称 **LTFS**。它是实现参照，不能替代目标驱动器规格或现场测试。
- sg3_utils 作者仓库的 [sg_logs(8)](https://github.com/doug-gilbert/sg3_utils/blob/main/doc/sg_logs.8) 与 [sg_read_attr(8)](https://github.com/doug-gilbert/sg3_utils/blob/main/doc/sg_read_attr.8)。

## LOG SENSE 0x2e 与读后清除

### 已核事实

1. **IBM-08 §6.4.3 p259（PDF288）仍规定：除另有说明，log counter 在 LOG SENSE 读取时 reset。** IBM-05同节p258也有此规则。因此不能笼统承诺 LOG SENSE 全部无副作用。[IBM-08 p259](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=288)、[IBM-05 p258](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference_GA32-0928-05%20%28EXTERNAL%29_0.pdf#page=288)。
2. 现代 TapeAlert 专节没有直接重申“全部 flag 读后清”；它警示产品/reset行为差异，并给出各 flag 的解除条件。IBM-08 §6.4.18.1–2 p320–322、表289：`R` 是介质完全移除，`L` 是load，`C` 是cleaner装载，`U` 是unload；另可能在原因消失后清除。**R不是Read。** 历史 IBM LTO 文档 §6.4.16.1 则明确说明2Eh读后清全部参数。两种资料不能合并成“所有现代固件都一定读清”，也不能据现代表格保证“绝不读清”。[IBM-08 p320](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=349)、[IBM旧版原厂文档，Lenovo分发](https://download.lenovo.com/servers/mig/systems/support/system_x_pdf/tape_scsi_reference.pdf)。
3. 官方LTFS `sg_get_tape_alert` 每次取2Eh后把结果OR到 `priv->tape_alert`；`sg_clear_tape_alert` 只清这份软件缓存。它支持“采样值与软件保留值分开”的设计，并不证明硬件所有清除机制。[LTFS sg_tape.c:3995](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/tape_drivers/linux/sg/sg_tape.c#L3995)。

### 编码及读取建议

IBM-08 §5.2.9 的LOG SENSE(10) opcode=`4Dh`，TapeAlert page=`2Eh`、subpage=0；PC一般取current=`01b`（该页IBM忽略PC并返回current），PPC/SP保持0，parameter pointer=0。页数据长度为`0140h`，参数编号1..64、每值1字节0/1。[IBM-08 §5.2.9、§6.4.18](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=349)。

**建议一次读取足量缓冲区。** 该IBM页加4字节header共324B，可用固定1024B容纳并以实际传输长度解析。sg_logs文档说明默认先取4B再按长度第二次读取；指定`--maxlen`才单次读取。对可能消费证据的页面，不采用探长后重读，不用`-a`顺带读取其它计数器页。监控不发送LOG SELECT、`--reset`或`--select`。[sg_logs NOTES及LOG SELECT说明](https://github.com/doug-gilbert/sg3_utils/blob/main/doc/sg_logs.8)。

解析至少验证实际长度≥4、page/subpage匹配、声明page length不越实际数据；逐descriptor核4字节header、parameter length及值边界，用parameter code定位，不假设顺序/固定偏移。未支持、截断、重复或异常长度均记录采样失败，不能解为全0。bit映射采用flag1→bit0、flag64→bit63，与LTFS一致。[LTFS同函数](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/tape_drivers/linux/sg/sg_tape.c#L3995)。

### 必须区分的 flag

下列名称/语义依据[IBM TS4300官方TapeAlert表](https://www.ibm.com/docs/en/ts4300-tape-library?topic=flags-tapealert-supported-by-drive)，分组是本项目建议：

| Flag（十进制） | 意义与保守归类 |
|---|---|
| 1、2 | 读/写性能警告，尚未丢数据；需要区分drive与tape，不能直接永久替换 |
| 3 | 不可恢复读/写/定位错误的总告警，需要4/5/6继续归因 |
| 4 | 明确介质故障；可作为介质错误证据 |
| 5、6 | 读失败、写或定位失败；可能drive或media。**单独出现不能确认磁带坏**；6还须排除9写保护 |
| 9、17 | 写保护、只读格式；是限制条件，不是不可恢复介质损坏 |
| 20、21 | drive清洁需求 |
| 25、32 | interface；30、31、38为drive硬件/预测故障；35/36/37为湿度/温度/电压 |
| 52、53 | 系统区写/读失败；52指**先前刚卸载的卷**，不可绑定到随后装入的另一卷 |
| 55、56 | load/unload问题，可能机械/drive，不能直接作为普通读写永久错误 |

只凭2Eh当前值无法识别该告警属于哪次操作或哪盘先前介质；应同时保存drive serial、当前/前一个volume UUID与条码、load会话及发生时间。设备级与环境级flags保存在drive/library范围，即使当前无介质也不挂到某Tape错误状态。上述绑定方法是建议，不是IBM公开持久化格式。

## MODE SENSE 写保护

MODE SENSE(10)返回8字节header，**byte3 bit7为WP**；MODE SENSE(6)则为byte2 bit7。使用current PC、既有受支持page，最小校验header与实际返回长度，再读WP，不能把page内部某个固定字节当header。[IBM-08 §6.6.2.1，表335/336，p368/PDF397](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=397)。

官方LTFS的LTO分支 `sg_get_parameters` 使用MODE SENSE current device configuration后读`buf[3]&0x80`并报告physical WP；enterprise分支另读专用medium sense字段，**不能把3592私有偏移照搬LTO**。WP0不意味着一定可写，Pool策略、LTFS锁、WORM、format兼容与已知错误仍分别限制写入。[LTFS sg_tape.c:4227](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/tape_drivers/linux/sg/sg_tape.c#L4227)。监控只MODE SENSE，不MODE SELECT。

## READ ATTRIBUTE 容量与归属

MAM `0000h`=remaining、`0001h`=maximum，值都是8B binary，**每单位1,048,576B（MiB），为指定partition的native未压缩容量**。READ ATTRIBUTE的PARTITION NUMBER选取目标分区；不是当前头所在分区。不能将index partition容量作为整个卷的data容量，也不能把压缩后应用字节数当作native free的反向计算。[IBM-05 §6.5.2.2.1 p351](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference_GA32-0928-05%20%28EXTERNAL%29_0.pdf#page=381)、[sg_read_attr的partition/cache与MiB示例](https://github.com/doug-gilbert/sg3_utils/blob/main/doc/sg_read_attr.8)。

建议只采已明确装载且身份已核的卷，分别请求已知LTFS data/index分区，沿既有MAM解析验证ID、binary format、长度8与big-endian；checked乘MiB，缺失或不合理的`remaining>maximum`保持unknown。0可是真实full，不能把命令失败映射为0；maximum0不足以单独诊断坏带。

READ ATTRIBUTE的CACHE=0在未装带时不要求返回上一卷缓存；CACHE=1允许上一mounted volume缓存，LTO7前总表现为允许缓存。IBM说明开始load会清缓存，但这不等于“任意无带读取都属于当前库存磁带”。监控不把空drive返回的缓存绑到槽位卷，亦不为探测而LOAD/MOVE。MAM故障、缓存不可用与属性未支持分别留诊断，不把容量未知变成健康。[IBM-08 §5.2.16 p108–109](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=137)、[sg_read_attr CACHE说明](https://github.com/doug-gilbert/sg3_utils/blob/main/doc/sg_read_attr.8)。

## sense 的最小白名单与排除项

必须保存 **SCSI status、host/driver status、sense原字节、response code/current或deferred、sense key、ASC/ASCQ、opcode、阶段、真实介质身份**。单看key3或错误中文字符串都过宽。下列分类来自[官方LTFS vendor_compat.c 的标准sense映射](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/tape_drivers/vendor_compat.c#L105)，并与[IBM-08 Annex B p483–491](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=512)核对：

| key / ASC / ASCQ | 含义、建议处理 |
|---|---|
| `03/11/00` | unrecovered read；actual read阶段可记永久读失败证据 |
| `03/0C/00` | write error；actual write阶段可记永久写失败证据 |
| `03/11/01`、`03/11/08` | LTFS另映射read retries exhausted/incomplete block read为READ_PERM；IBM本机白名单可先只含11/00，扩展须保留型号依据 |
| `01/11/00`、`01/0C/00` | 已恢复成功的read/write，不等于上两项永久错误 |
| `03/04/10`、`03/11/12` | MAM不可达/读错；需要独立CM诊断，不能声称用户payload已有不可恢复读错 |
| `03/30/*`、`03/31/00`、`03/3B/00` | 格式/不兼容/定位等；保留具体限制或require_validate，不泛化替换 |
| key4；如`04/44/00`、`04/41/00` | drive内部/数据通道硬件问题，不能直接归罪介质 |
| key2；如`02/04/01`、`02/3A/00` | becoming ready、no medium，资源/瞬态，不是坏带 |
| key5；如`05/24/00`、`05/2C/0B` | 非法CDB或没有reservation；实现/ownership错误 |
| key6；`28/00`、`29/00`、`2A/03..05` | 换带、reset、PR抢占/释放；重新核owner/identity，不能标坏带 |
| key7；`27/00`、`30/05`、`74/*` | 写保护、不兼容、密钥/安全原因，单列限制/crypto错误 |
| keyB；`47/00`、`47/03`、`4B/*` | transport/parity/CRC/timeout等，不能变成介质永久故障 |
| FM、ILI、EOM、`08/00/05` | FILEMARK、变长块长度语义、空间边界/EOD；须按原命令解释，不统一当故障 |

注意IBM厂商加密错误也可能使用key3（`EE/*`等）；**key3不是need_replace的充分条件**。SCSI RESERVATION CONFLICT是status而非介质sense；Linux超时、host_status/driver_status传输错误、没有有效sense均不可伪造key3。deferred write error可能在随后read/LOG SENSE等命令上报；不能按当前opcode直接变成读错误，必须关联原写入上下文，缺证据时只保留uncertain错误。

### 推荐首版触发条件（项目决策，不冒称 EE 内部算法）

- `need_replace`：同一真实卷的正常读取明确返回current `03/11/00`；或已可靠归属该卷的TapeAlert **4+5**。记录不可恢复读证据与原错误，禁止后台成功采样自动清除。
- `require_replace`：同一真实卷的实际写操作明确返回current `03/0C/00`；或TapeAlert **4+6且能证明属于写入失败**。仅6可能是定位/drive故障，不单独触发。失败前后的写入不确定性仍由现有journal处理，状态标记不能授权重发写带。
- 只有4、3、5、6、55/56或其它含混sense：保留media/drive diagnostic与原操作失败，必要时暂停自动选带并要求validate/人工隔离；不发明读写方向。52/53须关联前次卸载/本次load会话后再设计专门规则。
- `need_replace`与`require_replace`同时存在时建议展示更严格写限制，同时保留两个错误事实；改Pool策略、rescan、owner切换都不能消除历史证据。

以上白名单可以宁可漏掉部分厂商永久错误，也不把HBA/密钥/PR故障误判成坏带；新增规则须附原始sense和型号文档测试。周期本身不应发READ(6)来“制造可检测性”，也不读取全盘正文。

## 证据保存与尚未实测

本轮下载资料与提取文本位于 `/work/tmp/tape-monitor-scsi-research/`：`LTO-05.pdf/.txt`、`LTO-08.pdf/.txt`、`LTO-old-ibm.pdf`、固定LTFS源码及`ltfs-commit.json`。旧文档用于证实历史读后清描述，不能覆盖现代型号。没有运行取回的程序。

尚未实测：现场TD8/HH8具体固件的TapeAlert读后清、是否按initiator独立报告、告警重置时间、采样对流式I/O延迟的影响、MAM动态新鲜度及真实故障时sense映射。没有把不同initiator先后读出的0/1当作告警消失。采样间隔应是本项目调度选择，本报告不提供臆测的IBM固定周期。

交付前仅核本文UTF-8、链接目标和diff格式；未运行Rust检查，未接触真实硬件。
