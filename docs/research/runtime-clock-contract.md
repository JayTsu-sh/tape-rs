# 运行许可时钟与暂停恢复契约

票据：34。范围：Linux/Rust/KVM/QEMU 官方时钟语义，以及既有 RQ、首次启用和 K→G 的时间适配条件。结论是设计候选与环境前提，不是实现或实验认证；未连接 PVE、EE 节点或设备，未运行代码、模型或故障实验。

## 结论

建议本项目 Linux 许可时间统一采用 `clock_gettime(CLOCK_BOOTTIME)`，经检查的时间表示/算术，并绑定当前运行实例与时间域。它优于默认 Rust `Instant` 的明确之处是计入 Linux 系统挂起；它不自动解决虚拟机暂停、迁移、内存/磁盘回滚，也不是跨节点租约。首版将活跃许可跨这些未资格化生命周期事件的延续排除在支持范围之外，保留受控停权、恢复检查和新许可建立路径；不是禁止用户执行运维操作。

## 主源事实

| 对象 | 文档保证与不能推出的保证 |
| --- | --- |
| Linux `CLOCK_MONOTONIC` | 非墙钟跳变时钟，不计系统挂起；频率仍可受调整，连续读数可以相等。`CLOCK_BOOTTIME` 在此基础上计入系统挂起时间。两者不是进程 CPU 时间。[Linux 时钟接口][clock] |
| Rust `Instant` | 官方明确不保证 steady，也不规定各平台/版本如何计系统挂起；当前 UNIX 实现使用 MONOTONIC，但实现可变。文档承认虚拟化/OS/硬件缺陷可能破坏单调性；饱和减法可能掩盖逆序，checked 操作只检测相应算术/顺序问题。[Rust Instant][instant] |
| 定时器 | `CLOCK_BOOTTIME_ALARM` 定时器可唤醒挂起系统，普通 BOOTTIME 不提供该唤醒保证；读时钟本身不是唤醒机器。不得把定时器事件已注册当作回调及时执行证明。[Linux timerfd][timer] |
| 时间命名空间 | MONOTONIC 与 BOOTTIME 可具有不同 namespace offset，影响读时钟与定时 API。新命名空间首次进程进入后，不能再通过该 offsets 文件修改；这不等于跨 namespace 可直接比较时间。文档明确该设施服务于容器迁移/检查点恢复。[Linux time namespaces][timens] |
| KVM x86 时钟 | GET/SET_CLOCK 用于迁移等场景的时钟单调性；SET_CLOCK 的 REALTIME 标志可按宿主经过时间补偿。说明有显式虚拟化恢复责任，不是“任意 guest BOOTTIME 自动覆盖所有停顿”的通用承诺。[Linux v6.12 KVM API §4.29–30][kvm] |
| 迁移与整机快照 | KVM 文档讨论迁移期间中断无法交付、guest 时间可能需追赶及频率补偿。QEMU 整机快照包含 CPU、RAM、设备状态和可写磁盘；loadvm 恢复整机状态，stop/cont 与 guest suspend/wakeup 是不同管理动作。[KVM timekeeping §4.4][migration]、[QEMU VM snapshots][snapshots]、[QEMU Monitor][monitor] |

## 项目时间适配候选（推论，不冒充内核提供的业务协议）

1. 一个许可上下文内，原请求 `t0`、期限 `E=t0+T`、P 后 W 采样、C 后及每次派发的 `now` 使用同一 BOOTTIME 时间域。时间值携带运行实例、时间域与对应许可/请求关联；不序列化成本次运行之外仍有效的许可证。审计时间戳可以保存，但不得据其恢复执行资格。
2. 检查调用失败、表示合法性、单位转换、加减溢出及具有明确先后关系的样本倒退。允许相等读数；不得以饱和为零、回绕或夹成“最近合法时间”掩盖失信。错误立即拒绝当前动作并使适用共同时间能力受限；恢复后建立新上下文/轮次，不将旧期限换算续用。并发样本不能仅按返回/处理顺序判断倒退。
3. W 必须是成功发布对应 P 之后才发起的新采样，不能缓存或提前取值；适配器须在支持环境中给出该采样对应于调用期间且与 P 实际发布顺序一致的保证。既有 W 的严格期限条件不变；确认 C 迟到可以仅携带历史证明，当前读者仍检查本轮实际期限。`SeqCst` 描述原子操作的内存排序，不独自证明时钟相对真实时间的推进、采样精度或物理设备交付顺序。[Rust Ordering][ordering]
4. 定时器仅提示检查，不是权限来源；遗漏/迟到回调不能延期。每个读者依当前描述/阶段读取当前时间并复核 token 与共同故障域。BOOTTIME 只让恢复运行后的检查有机会看到已过期；不能撤回已交付外部请求，也不能封闭最后检查之后进程暂停再交付的竞态。
5. 单调性不等于真实时间速率误差有界。期限首先是同一受支持本地时间域的证据时效政策；若规格进一步承诺真实秒数上界，仍须明确时钟精度、速率和恢复补偿能力，数值留空。本报告不据 BOOTTIME 宣布取得跨节点排他性或固定 RTO。

## 暂停、恢复及支持条件

| 场景 | 对本项目的具体处理/验证条件 |
| --- | --- |
| 线程未调度、进程停止而 Linux 正常运行 | 系统时钟并非该进程 CPU 计时；恢复后重新采样可发现期限已过。不能依赖被停止的 timer 处理线程，不能把暂停前通过的检查当作恢复后交付许可。 |
| Linux 正常 suspend/resume | BOOTTIME 的挂起计时语义适用。恢复后检查已过期则拒绝；没有发生失权且所有既有非时间条件仍成立的正例不必因 timer 迟到永远拒绝。设备/路径恢复另依既有资格与隔离条件。此结论不包括任意整机快照回滚。 |
| VM pause/resume 或 live migration | 不等同于 guest Linux 正常 suspend。首版不承诺活跃许可自动延续；正式支持需环境排除该交错，或另行证明生命周期约束、暂停补偿和恢复前门控。这里并不声称所有 KVM 配置一定冻结或倒退，只是不从现有通用文档推出本项目所需的完整能力。 |
| daemon 重启 | 即使同一 BOOTTIME 数轴仍在前进，也建立新运行实例；旧 P/W/C、许可及在途刷新不得继承。可信控制恢复、现实隔离与新 K/G 仍分别完成。 |
| VM/容器检查点恢复或 namespace 更换 | 恢复前保持设备副作用受外部约束；新时间域/运行上下文重建许可。RAM 恢复可同时恢复旧实例标签、STOP、epoch 和描述，磁盘回滚另触发 SG；仅重新读 boot ID、随机标签或双 guest 时钟不是可靠识别证明。 |

VM 恢复的出口不能偷换成“guest 恢复后总会及时收到通知”。未被可靠观察/约束的恢复可能让旧线程直接继续运行，因此环境生命周期约束必须在旧代码再次可能产生副作用之前成立。用户仍可暂停、迁移或恢复；在尚未资格化的部署中，应通过已有受控维护先终止/约束旧权并收束外部效果，再执行操作，恢复后走可信历史检查与新许可流程。具体管理入口、检测覆盖和外部约束适配尚未证明，不能宣称实验室已支持。

反例：guest 的两个时间源与全部内存一起冻结/恢复，它们的差值与旧标签都可以保持一致；本地比较无法区分“短暂正常间隔”与“外界已经经过很久”。这是观察不可区分的项目推论，不是对每一种 hypervisor 行为的断言。引入更多 guest 时钟不能替代独立环境保证。

## 下一验证契约（未执行）

- 正例：同一可信时间域中完成 P/W/C，当前期限与全部门控成立时允许动作；挂起/迟调度后未过期且资格仍有效的合法路径可工作。
- 反例：P→W 间暂停跨期；C 迟于当前期限；timer 不运行；读数失败/真正倒退/溢出；时间域混用；恢复旧内存与双时钟共同冻结。分别验证拒绝或历史证据行为，不靠永久拒绝过关。
- 环境资格：明确 guest/host 内核、虚拟化时钟与生命周期组合，证明不能未经收束重新执行旧许可；本地检测只作补充，不冒充全覆盖。源端获知到失效发布、最后门控到物理交付仍保留独立证明义务。

[clock]: https://man7.org/linux/man-pages/man2/clock_gettime.2.html
[instant]: https://doc.rust-lang.org/std/time/struct.Instant.html
[timer]: https://man7.org/linux/man-pages/man2/timerfd_create.2.html
[timens]: https://man7.org/linux/man-pages/man7/time_namespaces.7.html
[kvm]: https://www.kernel.org/doc/html/v6.12/virt/kvm/api.html#kvm-get-clock
[migration]: https://cdn.kernel.org/doc/html/latest/virt/kvm/x86/timekeeping.html#migration
[snapshots]: https://www.qemu.org/docs/master/system/images.html#vm-snapshots
[monitor]: https://www.qemu.org/docs/master/system/monitor.html
[ordering]: https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html
