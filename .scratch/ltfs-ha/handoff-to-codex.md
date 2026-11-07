# 当前交接：本地工作树清理（2026-11-07）

当前授权范围是整理已有开发成果、审查合入和清理 worktree。清理完成前不开发新功能；清理后也不要自动恢复未完成的功能开发。

## 开发入口与待合内容

- 开发目录为 `/work/jay/tape-rs`，交付时使用 `main`。按 `AGENTS.md` 的接续步骤确认实际分支、提交和工作树状态。
- PR #39 已合入 GitHub main，合并提交 `591469170428c49ddfa4f93f9d7f969e78ac9aae`；其代码树与实带验证的 `f42c775292cdaee65221b1bc64aeeb0fb13e80d0` 相同。
- 原目录的大量差异是旧 WIP。额外 10 份源码和 4 份测试已复核，没有需要独立合入的正确修复；旧实现会削弱 main 的故障守卫、回执或测试覆盖。已直接采用 main，未归档旧源码。
- 已合入的旧 worktree 已删除。`integrate/historical-wip-20261106` 分支仍保留 `8caca7e2511a052c54745f90d787c7acb45f7896` 及其 9 个未合入提交，其 worktree 已移除。
- 这 9 个提交属于未完成原生 reclaim 的依赖链，只有软件、Sim/private FDB 证据；完整父流程、逐对象覆盖和实带验收尚未闭合。公共 FDB 原生 reclaim 入口继续拒绝执行，不能将这些提交视为已验收功能。
- 旧文档分支的 PDF 已在 main；旧 AGENTS/CONTEXT 已由现行管理及故障规范替代，不整体合回旧分支。
- 核对 Raft 初始化或成员替换方案时，读 [固定 raft-rs 0.7.0 源码研究](../../docs/research/raft-membership-bootstrap.md)。该文档区分源码事实与项目建议，不代表项目已实现成员变更。

## 真实环境与安全边界

现场操作前读取私有 `/root/jay/cicd/tape.md`，确认登录、实际设备、库存和当前服务状态。该文件含认证信息，保持本地，不写入仓库。

- 只允许 B605d 库参与已有授权实验；A/B6070 已释放给其他人，禁止操作。
- 可写实验介质为 `L00284L8`、`L00285L8`。保护 AA0000、8A0043、正式 8A0042/8A0047、AA0013 和清洁带。
- 最近已核实的三节点部署源码为 `f42c775292cdaee65221b1bc64aeeb0fb13e80d0`，普通 release 配置，无 GPFS、测试故障环境或 `RUST_MIN_STACK`。
- 部署目录 `/opt/tape-rs/v1-20261106-f42c775292cd`，`ltfsd` SHA256 为 `412eb49e1012f3140eb7f3aa87cc40e9353978640485b4d2322b9ad143a6e01b`。
- 独立 FDB 集群使用 45153；保留原 namespace、state、服务单元和端口。不得碰 45151/45152，不清除原任务门闩或设备预约来制造就绪。
- 已消费的实测 controller 不重跑。未知副作用保留原请求及检查点，按项目恢复流程核对；格式化、擦除沿用显式确认门槛。

## 验证证据与已知限制

本次工作树清理只整理源码和文档，没有部署或设备动作。

- PR #39：完整与增量审查问题已关闭，六项软件检查及相关真实 B 库 S3/FUSE rename 回归通过，FUSE 正常卸载，最终 12 类任务门闩为空；该轮没有 FORMAT/ERASE。
- READ/WRITE 后取消的精确故障边界由 SimLibrary 加隔离 FDB 验证；不把它描述为已在实带注入同一故障。
- 历史物理矩阵 24 项通过，各自绑定实际源码版本；不能称为最终候选统一重跑，完整原生 reclaim 未验收。
- 已有 owner 定向用例在默认 debug 小栈下溢出，基线 `6dbbd808` 同条件也复现。8 MiB 调试回归通过；正常 release 实带验证未扩大生产栈。
- 原始记录在本地 `/work/tmp/tape-wip-integration-evidence-20261106`、`/work/tmp/tape-wip-independent-b-20261106`，核心入口为前者 `PR39-MERGED-v1.json`、`PR39-MERGE-ACCEPTANCE-v1.json`。现场脚本和失败日志保留原位，不作为待合入产品源码。

查证历史或恢复现场上下文时，再读本地 `.scratch/ltfs-ha/local/handoff-history.md`。本机 `.git/info/exclude` 排除了现场证据、CodeGraph 索引和 EE 参考输入；它不隐藏 `src/` 或 `tests/` 的产品改动。
