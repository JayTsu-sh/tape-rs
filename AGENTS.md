# AGENTS.md

本文件适用于整个仓库。目标是让 Codex 和其他 AI Coding Agents 以最小、清晰、可验证的改动维护 `tape-rs`，同时保护真实磁带与机械设备。

## 接续入口：先确认工作树

当前开发接续分支为 `ltfs-recovery-ibm-interop`，本机工作树为 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`；`/work/jay/tape-rs` 的 `feature/docs` 保存早期设计。用户明确指定其他分支时，以用户指定为准。

每次新会话，以及回答“当前进展”或接到“继续”时，先执行 `git worktree list --porcelain`，再在目标工作树执行 `git status --short`、`git branch --show-current` 和 `git log -1 --oneline`。读取目标工作树的 `AGENTS.md` 和 `.scratch/ltfs-ha/handoff-to-codex.md`，按交接记录的最新条目核对实现、测试及部署状态；确认目录、分支、提交与记录一致后再继续。

路径变化时按分支在工作树清单中定位；分支和记录冲突或目标缺失时先查各分支提交及交接文件，仍无法确定再澄清。最新时间戳或当前 cwd 单独都不足以决定接续分支。

后续读取、编辑和验证显式使用已确认的工作树目录。保留其他工作树的未提交内容；无需为接续而 checkout、reset 或复制旧设计覆盖实现。迁移接续分支时同步更新新旧入口及交接记录。

## 工作原则

- 无人值守约定（2026-09-27）：在已定义功能和已授权实验室范围内逐片自主推进，决策采用推荐方案并记录依据；每项任务完成软件检查后还须现场验证通过。现场不可用或验证失败时保留未完成状态并记录原因，Holo 结果与物理带库结果分开报告。

- 先把请求转成可检查的成功标准，再开始修改。
- 选择能完整解决问题的最小方案；不添加需求外的功能、抽象、配置项或依赖。
- 每一行 diff 都应能追溯到用户请求。保留用户已有改动，只清理由本次改动产生的无用代码。
- 遇到会实质改变行为或范围的歧义时，说明已知事实与取舍并请求澄清；其余小缺口采用最保守的合理假设并明确说明。
- 多步骤任务给出简短计划，并让每一步都有明确的验证条件。

## 开始工作

交接状态与原 Claude 本地记忆里的工作约定见 `.scratch/ltfs-ha/handoff-to-codex.md`，开始前先读。


1. 运行 `git status --short`，识别并保护已有改动。
2. 在已确认的目标工作树检查 `.codegraph/`；存在时先运行 `codegraph status .`，索引过期则运行 `codegraph sync .`，优先用 `codegraph explore "<问题或符号>"` 获取源码与调用路径。不存在时直接使用 `rg` 或读取文件，是否建索引由用户决定。
3. 涉及 SCSI/SG_IO、CDB、机械手、磁带读写、LTFS、超时或远程硬件流程时，修改或执行命令前完整阅读 `CLAUDE.md` 中对应章节；它是硬件细节和远程开发流程的项目参考。
4. 检查相关符号的调用方、错误路径和现有测试，确定改动边界与验证方式后再编辑。

## 项目边界与结构

- 这是 Linux-only 的 Rust 2024 CLI，通过 Linux `SG_IO` ioctl 控制 IBM TS4300，不依赖 LTFS 挂载来完成底层 SCSI 操作。
- `src/scsi/sg_io.rs` 只承载 C ABI 结构和 ioctl 绑定；`src/scsi/cdb.rs` 的 CDB 构造函数保持纯函数、固定长度、无副作用。
- `src/scsi/device.rs` 负责设备句柄、数据方向、ioctl 执行、状态码及 sense 解析；上层不得绕开它重复实现 SG_IO。
- `src/changer/` 与 `src/tape/` 是设备领域封装，`src/ltfs/` 处理 LTFS 标签、索引、MAM 和卷操作，`src/catalog/` 维护 SQLite 目录，`src/cli/` 负责用户交互，`src/main.rs` 只做参数定义和分派。
- 用户可见的帮助、状态和错误上下文保持中文；已有 SCSI 标准术语、命令名和字段名可保留英文。
- 可失败函数沿用 `crate::error::Result<T>` / `TapeError`；用户里程碑用 `info!`，逐 CDB 细节用 `debug!`，CLI 结果输出沿用 `println!`。

## SCSI 与硬件不变量

- `SG_IO` 请求号必须保持字面量 `0x2285`，绑定方式保持 `ioctl_readwrite_bad!`。不得改用会重新编码 ioctl 号的宏。
- 保持 `SgIoHdr` 的 `#[repr(C)]`、字段布局、指针有效期和传输方向一致；修改 `unsafe` 边界时必须逐项验证长度、空指针和内核返回值。
- CDB 的 opcode、位域、端序、长度上限和 transfer length 语义必须以对应 T10 标准（SPC-5、SSC-5、SMC-3）核对。响应解析先验证缓冲区长度，再切片或转换整数。
- `MediumChanger` 在读取状态或移动介质前必须先加载 element address map；槽位、I/E、机械手和驱动地址分别从 map 计算，不能混用地址空间。
- 机械操作的长超时是硬件约束。没有真实设备测量证据时不缩短现有超时。
- 磁带 `READ(6)` / `WRITE(6)` 当前使用变长块模式；引入固定块模式时，明确区分 transfer length 的“块数”与“字节数”。

## 设备安全

- 部署到远程主机、访问 `/dev/sg*` 或执行真实硬件命令，仅在用户明确要求时进行；先确认目标主机、设备节点、介质和预期结果。
- 将写带、文件标记、load/unload、move/exchange/import/export、initialize、erase、format、mkltfs、LTFS 写入以及 MAM 写入视为有状态操作。执行前说明影响并确认当前库存或介质状态；格式化和擦除必须保留现有显式确认门槛。
- 自动化验证默认只运行不接触硬件的测试。新增硬件测试时使用 `#[ignore]` 或显式环境变量门控，并写明所需设备状态。
- 不凭猜测处理 SCSI sense 或设备状态；保留原始 status、sense key、ASC/ASCQ 和足够的设备上下文以便诊断。

## 实现与测试

- 修复 bug 时，优先先写能稳定复现问题的测试，再完成最小修复。重构应保持重构前后的测试结果一致。
- CDB 编码、sense/响应解析、地址换算、XML、MAM 和 catalog 逻辑优先写纯单元测试，覆盖正常值、边界值、截断输入和错误路径。
- 匹配邻近代码风格。不得为顺手清理而重排 import、重写注释或格式化无关文件。
- 添加生产依赖或改变持久化格式、CLI 接口、默认设备、SCSI 行为前，先说明必要性和兼容性影响。

## 验证

按改动范围执行，并保留结果：

1. Rust 格式：`cargo fmt --all -- --check`。仓库存在历史格式差异时，只格式化本次触及的代码，并在交付中明确说明剩余基线差异。
2. 单元与文档测试：`cargo test`。
3. 编译检查：`cargo check --all-targets`。
4. 涉及逻辑、API、`unsafe` 或底层协议时：`cargo clippy --all-targets --all-features`，区分新增告警与既有告警。
5. 目标工作树已有 `.codegraph/` 时，修改完成后运行 `codegraph sync .`；用 `codegraph affected <changed-files...>` 或相关符号的 `codegraph explore` 检查受影响调用路径和测试。

无法运行某项验证时，不以推测代替结果；说明原因、已完成的替代检查和建议的下一步。真实硬件验证与纯软件验证分开报告。

## Code Review Rules

- 标记任何可能改变目标设备、介质内容、机械位置或持久化 catalog 的隐式副作用；安全路径是显式命令、显式目标和显式确认。
- 标记 ioctl 号、C ABI 布局、CDB 位域/端序、响应边界、sense 判定、传输残差和超时的无证据改动；安全路径是标准依据、边界测试和 Linux/硬件验证。
- 标记绕过 address map 的固定 element 地址、槽位基数混淆或 changer/drive 设备节点混用。
- 标记真实设备测试进入默认测试套件，或日志/错误丢失诊断所需的 SCSI 状态。
- 格式问题交给格式检查；Review 重点关注正确性、数据安全、协议兼容性和请求范围。

## 交付要求

完成时简洁说明：做了什么、如何验证、是否触及真实硬件，以及仍存在的风险或未完成项。不要把未运行的检查描述为通过。
