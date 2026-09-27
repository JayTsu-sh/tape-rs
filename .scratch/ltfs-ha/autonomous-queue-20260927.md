# 无人值守任务队列（2026-09-27）

告警处理约束：优先实际修复，不新增 `#[allow]` / `#[expect]` 或全局屏蔽以通过检查（用户2026-09-27补充）。

用户授权顺序：先完成历史格式差异与Clippy告警，再依次完成1–5；采用推荐决策，每项有软件及Holo/IBM LE现场验证，保存证据与最新关闭数据位置。物理设备验收因环境不可用暂缓，不作为本队列阻塞项。实现工作树和当前现场状态以docs/CLAUDE-HANDOFF.md为准。不得把早期设计中已被后续实现取代的“未实现”项重新列为任务。

- [x] 0. 全仓格式与Clippy清理：fmt/check/test/Clippy零告警；兼容构建、隔离现场回归；保护SCSI布局、命令、超时及行为。
- [x] 1. 原始FORMAT成功但LTFS标签/索引未完成时的回收恢复：先模拟复现，必要时修复，再Holo故障验收。
- [x] 2. namespace/takeover间歇HTTP500：稳定复现与根因定位、最小修复、接管并发与现场回归。
- [x] 3. 回收介质IBM LE往返：备份、LE读/回写、tape-rs读回，内容/链接/属性保真。
- [ ] 4. 大目录与性能：定义可复现实验规模及基线，测量请求/延迟/资源占用，按实际瓶颈优化并回归。
- [ ] 5. 功能扩展：先核对LE/标准支持边界，按推荐确定硬链接及user.ltfs.sync的具体语义；实现支持的范围并逐项现场验收，明确标准无法支持的限制。

初始基线：bf2b254，208通过7忽略；正式32bd5541…，原TS1001L08 Full120，隔离SR1空Full2/SR2数据Full4；最新关闭SR数据tape-rs-reclaim-formatted-20260927/test-data。

任务0已完成：208通过、7忽略；严格Clippy零告警、fmt/check通过；Holo隔离写入/属性/改名/链接/接管及正式9文件复验通过。详见holo-cleanup-20260927.md。最新关闭SR数据为tape-rs-cleanup-20260927/test-data，SR1 Full2/SR2 Full14。

任务1通过：见holo-reclaim-raw-20260927.md，211通过7忽略；FORMAT后标签前Holo接管完成。最新关闭数据reclaim-raw/test-data，SR1 Full5数据/SR2 Full2空；下一项2。

任务2通过：holo-namespace-20260927.md，213通过7忽略；真实检查点资格丢失HTTP503与新节点冷缓存读取通过。最新关闭namespace/test-data，SR1 Full7/SR2 Full3，下一项3。

任务3通过：holo-reclaim-le-20260927.md，LE Full7→8，内容/六种链接/xattr/mtime往返通过。最新关闭reclaim-le/test-data，SR1 Full8/SR2 Full3。下一项4。
