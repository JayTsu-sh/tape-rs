# 当前交接：继续原生 reclaim 开发（2026-11-07）

本地工作树清理已完成。用户随后明确要求“继续开发”，现恢复原生 reclaim 的剩余实现、故障验证与真实 B 库验收；沿用此前管理模型和设备安全边界。

## 开发入口与待合内容

- 开发目录为 `/work/jay/tape-rs`，交付时使用 `main`。按 `AGENTS.md` 的接续步骤确认实际分支、提交和工作树状态。
- PR #39 已合入 GitHub main，合并提交 `591469170428c49ddfa4f93f9d7f969e78ac9aae`；其代码树与实带验证的 `f42c775292cdaee65221b1bc64aeeb0fb13e80d0` 相同。
- 原目录的大量差异是旧 WIP。额外 10 份源码和 4 份测试已复核，没有需要独立合入的正确修复；旧实现会削弱 main 的故障守卫、回执或测试覆盖。已直接采用 main，未归档旧源码。
- 已合入的旧 worktree 已删除。现仍只使用本目录的 `main` 工作树；保留分支 `integrate/historical-wip-20261106` 的 9 个提交已无冲突接回本地，尚未向 GitHub main 发布。保留原分支 `8caca7e2511a052c54745f90d787c7acb45f7896` 作为现有开发来源，不新建代码归档。
- 这 9 个提交属于未完成原生 reclaim 的依赖链，只有软件、Sim/private FDB 证据；完整父流程、逐对象覆盖和实带验收尚未闭合。同一 FDB 读版本输入已绑定原源实读、目标实读/退休回执和 b/d/g 账本；查询不创建或修复记录，也不授予 I/O 权限。
- 当前新增元数据固定计划与准备任务：从实际源/目标捕获流式核对路径和物理文件数，固定新 UID、路径、目标卷头及原属性/目标 extent 指纹。FDB 的 `RA`/`RW`/`RT` 记录和现有节点卷占用在同一事务取得；重复准备、查询、显式 HA 领取与未开写关闭均保留原计划，缺失占用不自动修复。它只实现 Prepared/Closed，没有执行准入或设备写入；首次旧回执清理回归发现准备占用误挡历史验证，已拆开只读身份检查和写入准入，后续结果查本地证据。
- 该准备内核已本地提交 `9b3ac375322b52d35d72ad4a46371a9f09972cf3`，六项软件检查及两项隔离 FDB/Sim 回归全部通过，规范与需求审查均为零发现；随后随互斥版本部署，未向 GitHub 发布。
- 用户随后要求参考 EE 命令互斥，并明确“同一 library 内互斥，不同 library 可并行”。`767aea9c86df6d6c222913a988cb2b4067396d87` 已补库级持久占用及原任务索引，接 rescan、reclaim 父任务、unassign 与原子 tape set/offline；缺失或损坏时拒绝，不自动补锁。该片六项软件及八项隔离回归通过，两轴审查零发现。首次真实 B 库发现 S3 写入占用原因被误报；修复 `fa7a2d732fd6ce60d0eb68e5770d234aefdb7d0e` 在设备访问前检查原池占用，并保留实际准入重核。真实 RED/GREEN、六项软件及四项隔离回归、两轴审查均通过，限定现场路径与未覆盖项见 `docs/management-task-exclusion.md`。完整原生 reclaim 与未实现命令仍未开放，未向 GitHub 发布。
- 仍需补执行前持久化状态、逐 CDB 原任务守卫、未知提交恢复、关闭后的显式新尝试、完整属性与全源对象覆盖及真实 B 库验收。目标新代次须重新实读核验，不能仅用旧摘要代替；源 MD5 存在而正文副本未记 MD5 的情况仍保守拒绝。公共 FDB 原生 reclaim 入口继续拒绝执行，不能将当前准备内核视为已验收功能。
- 旧文档分支的 PDF 已在 main；旧 AGENTS/CONTEXT 已由现行管理及故障规范替代，不整体合回旧分支。
- 核对 Raft 初始化或成员替换方案时，读 [固定 raft-rs 0.7.0 源码研究](../../docs/research/raft-membership-bootstrap.md)。该文档区分源码事实与项目建议，不代表项目已实现成员变更。

## 真实环境与安全边界

现场操作前读取私有 `/root/jay/cicd/tape.md`，确认登录、实际设备、库存和当前服务状态。该文件含认证信息，保持本地，不写入仓库。

- 只允许 B605d 库参与已有授权实验；A/B6070 已释放给其他人，禁止操作。
- 可写实验介质为 `L00284L8`、`L00285L8`。保护 AA0000、8A0043、正式 8A0042/8A0047、AA0013 和清洁带。
- 最近已核实的三节点部署源码为 `fa7a2d732fd6ce60d0eb68e5770d234aefdb7d0e`，普通 release 配置，无 GPFS、测试故障环境或 `RUST_MIN_STACK`。
- 部署目录 `/opt/tape-rs/v1-20261107-fa7a2d732fd6`，`ltfsd` SHA256 为 `943e8ed8934ce72eaf0a52a23d36e3a0db5f90558b15ca77e63b8f8aa9e6059b`。
- 独立 FDB 集群使用 45153；保留原 namespace、state、服务单元和端口。不得碰 45151/45152，不清除原任务门闩或设备预约来制造就绪。
- 已消费的实测 controller 不重跑。未知副作用保留原请求及检查点，按项目恢复流程核对；格式化、擦除沿用显式确认门槛。
- 两轮占用父任务均已按原 handle 在零子任务状态安全关闭，原排队请求均终态。首次 `accept-exclusion-v1.py` 因占用原因被掩盖退出 1，证据 `EXCLUSION-V1-ORIGINAL-SETTLEMENT-v1.json`。修复后 `accept-exclusion-upload-v1.py` 的严格 PoolBusy、引用保护和原任务回执检查通过，但脚本误要求专用 rescan 终态重放为 200 而退出 1；现有路由/测试要求 202，原响应正文与原终态一致。只读补验 `settle-exclusion-upload-v1.py` 原进程退出 0，核实新旧正文、被拒写对象 404、原回执未变及 14 类占用为空。总证据 `EXCLUSION-UPLOAD-ACCEPTANCE-SETTLEMENT-v1.json`。L85 同池同卷、在线且非黑名单，最新修订号为 3。没有重新提交管理操作或 PUT，没有 FORMAT/ERASE；不把原失败进程改记为成功。后续不得重跑这些已消费脚本。

## 验证证据与已知限制

此前清理只整理源码和文档，没有部署或设备动作。恢复开发后的新结果须另行记录，不能沿用旧验收宣称新代码通过。

- PR #39：完整与增量审查问题已关闭，六项软件检查及相关真实 B 库 S3/FUSE rename 回归通过，FUSE 正常卸载，最终 12 类任务门闩为空；该轮没有 FORMAT/ERASE。
- READ/WRITE 后取消的精确故障边界由 SimLibrary 加隔离 FDB 验证；不把它描述为已在实带注入同一故障。
- 历史物理矩阵 24 项通过，各自绑定实际源码版本；不能称为最终候选统一重跑，完整原生 reclaim 未验收。
- 已有 owner 定向用例在默认 debug 小栈下溢出，基线 `6dbbd808` 同条件也复现。8 MiB 调试回归通过；正常 release 实带验证未扩大生产栈。
- 恢复开发的本地集成提交为 `a07768f`，保留了 PR #39 的 rename 原任务守卫。旧本地测试 FDB 35127 连续超时，原失败日志保留；新建独立本地测试实例后该回归通过，未重启或清理旧库，也未改真实集群。当前测试实例位置读本地 `/work/tmp/tape-reclaim-resume-20261107/PRIVATE-FDB-v1.json`，不要沿用旧测试路径。
- 新查询的源卷头篡改用例已实际先红后绿；初次测试代码编译错误与真实断言失败分别保留。最新源码、审查和检查结果查本地 `/work/tmp/tape-reclaim-resume-20261107`；后续互斥部署与部分实测不代表完整原生 reclaim 通过。
- 原始记录在本地 `/work/tmp/tape-wip-integration-evidence-20261106`、`/work/tmp/tape-wip-independent-b-20261106`，核心入口为前者 `PR39-MERGED-v1.json`、`PR39-MERGE-ACCEPTANCE-v1.json`。现场脚本和失败日志保留原位，不作为待合入产品源码。

查证历史或恢复现场上下文时，再读本地 `.scratch/ltfs-ha/local/handoff-history.md`。本机 `.git/info/exclude` 排除了现场证据、CodeGraph 索引和 EE 参考输入；它不隐藏 `src/` 或 `tests/` 的产品改动。
