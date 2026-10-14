# 跨带读检查点失去资格的 HTTP500（2026-09-27）

基于941f974。历史 namespace_conflicts_across_tapes_and_takeover 失败日志明确为读旧带前的写带检查点失去预留，却返回read_failed/HTTP500。原用例修改前30次均通过，不能靠概率复跑证明修复。新增确定性集成测试在单驱动器跨带读取的检查点WRITE注入预留抢占UA，1.10秒复现相同500；另加介质错误对照。

根因：检查点错误分支统一返回Failed；CommitFailed又把底层错误转成String，使资格分类丢失。修复保留Box<TapeError> source链，ownership_lost递归分类；检查点已经关闭服务、上报Lost，资格错误返回Busy/HTTP503，让尚未向客户端发送数据的读取换节点重试。介质错误仍为500，不重放部分输出。日志文本与提交阶段保持，Rust CommitFailed构造字段由reason:String变为source:Box<TapeError>，外部直接构造/匹配字段的调用者需适配；无持久格式、协议或依赖变更。source属性用于错误链，不是告警屏蔽。

软件：213通过7忽略，fmt/check/严格Clippy零告警。两个新测试分别验证自动换届读回与介质错误不隐藏；原namespace用例修复后30次通过。兼容构建成功，外部Zig废弃优化参数提示保留。

Holo隔离目录 /home/rocky/tape-rs-namespace-20260927，ltfsd SHA256 1040995424f01e4f7060cf617806f48179af51598bd2b8f0a55db39b53049442。从reclaim-raw关闭数据复制；仅本隔离启动脚本配置一台TAPERS驱动器。SR2502L8新增24B测试文件并保持载入，原数据在SR2501L8，确保读取seed必须经过换带检查点。node3 round147处理读取，strace确认第二次库存读取完成时SIGSTOP；node2 round152隔离设备并服务，再恢复旧节点。旧HTTP实际503/drive_busy，保留执行资格丢失、新旧PR键信息。新缓存FUSE六链接/内容/mmap/seed/条码复验通过。

删除测试文件后18目录记录（原17条除代数外不变，加一条墓碑）。正常停止后SR1 Full7（15个文件元素、六链接），SR2 Full3（一个已被目录墓碑取代的旧副本）；槽8/9。关闭数据namespace/test-data、node2 round152；下次恢复双驱动器配置再做维护。前后介质备份 /home/rocky/holo-namespace-20260927 已逐文件SHA验证。暂停过程中sudo监督进程也停止，清理时发现已退出子进程为zombie；校验父子PID/命令后SIGCONT监督进程完成回收，探针补入同样清理步骤。首次删除误用/sr/checkpoint-current被拒不存在，改用实际目录路径/checkpoint-current成功，没有删除其他数据。

正式服务恢复node1 term47/round429，53行目录及池摘要完全不变，TS1001L08 Full120/TS1000L08 gen2，原9文件复验通过；正式二进制仍32bd5541…。EE三节点available、无任务、f3哈希不变、九publication ready。隔离进程与strace停止，FUSE卸载缓存清空。无物理设备验收。接续任务3：回收介质IBM LE往返。

可复核材料：probes/namespace-checkpoint-20260927.py、namespace-read-20260927.py 及 results/namespace-20260927-*。生产源码只改错误链与返回分类，测试故障注入仅在测试transport。
