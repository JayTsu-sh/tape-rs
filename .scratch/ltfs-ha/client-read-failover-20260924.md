# 缓存旧 Leader 后读取失败：诊断与修复

## 根因与行为

已在三节点模拟器复现历史 `read_failed`：Client先成功读取并缓存Leader，另一个模拟initiator抢占驱动器预留；下一次get_to从旧执行者收到HTTP500，原始SCSI状态为status0x02、sense06/2A/03。旧read_any将设备错误一律转换成Failed字符串，丢失了“必须结束当前执行轮次”的分类。

修复在executor，不扩大Client对HTTP500的重试范围：

- 读取、读取库存、装卸读带和换回写带的设备错误使用现有ownership_lost分类器，保留原始状态和上下文。
- 所有权丢失终止本轮、关闭FileService、发送ExecEvent::Lost；返回Busy，由既有HTTP映射生成503。Client原有清除Leader缓存和换节点逻辑即可工作。
- 读失败后若已丢失资格，立即返回，不再执行卸带或恢复写带。换回过程中卸带失败也不再被忽略。
- HTTP在完整读入私有spool后才发送成功内容，因此此503重试不会把半份数据交给应用。已经输出部分HTTP内容后的网络错误仍返回Io，调用方必须丢弃输出后从头开始。
- 普通介质错误继续HTTP500，保留诊断；没有重试READ/WRITE/SPACE等位置相关SCSI命令，没有重新注册或抢回预留。无持久化格式或外部HTTP接口变更。

## 验证

- `client_read_retries_after_executor_reports_preemption`先失败再通过：真实模拟PR抢占，Client流式读自动切换，输出仅一份22字节完整内容，新的执行节点恢复服务。
- 原rename接管测试移除“重建Client并只访问新Leader”的绕过，保留原Client缓存读取改名结果。
- 错误分类单测覆盖抢占、复位、预留冲突与普通medium error，原始status/sense保留。
- Client保护测试：200内容截断不对第二节点重放，不向输出拼接内容；普通500 read_failed不盲目重试。
- `cargo test`197通过、1忽略，all-targets check与all-features clippy完成；无新增警告种类，仍保留历史警告。全仓fmt历史差异未清理，仅格式化新增片段；git diff --check通过。
- 日志 `/tmp/read-preempt-red2.log`、`/tmp/read-preempt-green.log`、`/tmp/read-failover-final-{tests,check,clippy}.log`、`/tmp/read-failover-fmt.log`。

最初仅恢复历史rename测试未稳定触发旧Leader的窗口；因此缩小到显式模拟PR抢占。探针初稿向独立SimTransport注入故障（不影响执行者句柄），随后改为实际PR抢占；又修正抢占键的过大轮次，避免测试本身阻止下一任合法执行者接管。这些试验不作为通过证据。

## 现场与后续

本轮没有远程操作、部署、Holo/真实硬件访问。原集群仍为上一轮rename发布版本；本轮未重新核对其在线状态。此修复需要更新ltfsd，现有Client可以复用，不要求同步修改客户端重试策略。

下一步在隔离Holo副本进行网络隔离旧Leader、设备预留转移后的真实读回验收，再统一部署服务端修复。不能将本轮模拟验证称为Holo故障实测。其他既有边界（跨带rename、可写xattr/符号链接）不变，代码仍未提交。
