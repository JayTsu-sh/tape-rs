## 2026-09-27 20:23 CST — 最新：本轮全部实机验收通过，设备已收尾

活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`，源码/测试提交 **344e9b1**，之前合批测试提交fc4afcb。用户要求EOD不留兼容回退已落实：直接LOCATE EOD，不支持即返回SCSI错误，无BOP/SPACE补救；实际RC18挂载及互读通过。本轮新增原生FORMAT两个实机缺陷也修复：保留MODE SENSE只读分区字段、单位指数9/P0非零；显式定位BOP0。259passed/7ignored、fmt/check/Clippy -D warnings/兼容release全部通过。

**所有前述监督进程已结束，无测试运行中。** 最终`.143:/root/tape-rs-io-20260927/physical-io-final.complete`已生成；最终成功日志`physical-io-final-resume2-run.log`。仅RC18原生格式化成功，新UUID878f7b34-4910-4d09-9f1f-3e4725ede6d8；16组/3145文件/18256392448字节写入→正常卸载重载→Rust双SHA90.440/88.731s→私有LE双SHA70.849/19.982s全部通过。LE缓存label新UUID核验通过；第二次LE读没有新SCSI命令，属于缓存。Rust两轮均真实SG_IO。

最终只读核查：RC18槽7、RC17槽8、drive11EB4A80F1空（sg3，LE路径sg4），changer55L3A7802K19LL01 sg5；LE退出/private FUSE已卸载，PR generation18注册表空/无持有者，allow_dio保持0。没有操作.209，未推GitHub，未替换正式Holo部署。不得重跑已完成的检查点脚本（会格式化）；旧源清单RC0018L9.json仍旧UUID，新卷用native-final-manifest.json。

证据本地 `.scratch/ltfs-ha/probes/results/physical-rc18-20260927/`已刷新，最终状态native-final-state.json、最终二进制哈希perf-final-format-position-hashes.json；报告 `.scratch/ltfs-ha/physical-rc18-final-validation-20260927.md`，队列performance-fix-queue-20260927.md。

性能：16GiB总63.583s（LE62.942）；256×4KiB总25.820s（上轮Rust46.187、LE20.743）；连续小批总中位26.325s（LE约21.08）。大块写接近，微小文件/sync仍有差距，未独立归因，不宣称全部追平。下一步若继续：独立计时剩余sync约5s差距，检查最终FM0耗时及资格/发布屏障，保留安全边界；每文件实时位置检查不能直接删；跨两盘实机换带仍未覆盖，不盲目重新初始化曾卡住的RC17。

## 2026-09-27 — 最新：FORMAT 的 BOP0 修复，第二次检查点恢复

PID1570614已退出：MODE SELECT成功，但FORMAT返回05/3B/0C。现场READ POSITION为P1/block0；IBM §5.2.3要求BOP0。原mkltfs的REWIND不切换分区，已改显式LOCATE P0/block0。模拟器新增同样限制，回归测试先红（相同sense）后绿。全套259 passed/7 ignored，check、strictClippy、fmt、兼容build通过。UUID606a...是失败尝试，不能作为新卷UUID。

**原生格式化已完整成功**：FORMAT命令约24s，两分区标签/初始索引写完，成功UUID878f7b34-4910-4d09-9f1f-3e4725ede6d8，模式回读通过。新空卷mount21.325s，正在原基线write-groups（不是已完成全轮）。stream数据56.315/sync7.268/总63.583s，small数据12.982/sync12.839/总25.820s；LE对应总62.942/20.743s。

**本轮Rust全部通过**：全16组写入完成，IP checkpoint47.758s；卸载含checkpoint175.949s、重载31.309s；mount112.011s、3145文件双SHA90.440s/88.731s均通过。实际1MiB SG预留已观测、buffered1/speed0/DCE保持。正在正常卸载，下一阶段私有LE双SHA及UUID核验；尚不能宣称整个流程完成。

**当前PID1577397** 运行同一 `physical-io-final-resume-20260927.py`，新日志 `physical-io-final-resume2-run.log`、PID文件`physical-io-final-resume2.pid`；完成标志仍`physical-io-final.complete`。先查流程，禁止重复启动。新版bench SHA63f333eef1d72641df8e2c67b0bc5d5e40a8290c256530497414bf7e91afa5a8，CLI SHA f21ac754e88daf65007209cf2704f51cc288897d91460c1b99fddb64bdecf177。检查点脚本保留旧归档/测试源、在当前RC18已载带状态恢复。allow_dio0；RC17/.209不动。所有真实验收仍需等完整新轮成功，源改动未提交。

## 2026-09-27 — 最新：原生 MODE SELECT 缺陷修复，检查点恢复运行中

上一最终流程PID1552061已失败退出，**FORMAT尚未执行**。原生MODE SELECT硬编码只读字段0（最大分区数/格式识别）及P0=0被IBM拒绝05/26/00；单条sg_raw复现并按sense字段指针10→13→16逐项证明。单位指数也错误为0。现mkltfs先MODE SENSE读实际页，保留只读字段，P0=1GB向上取整、P1=FFFF、单位指数9，与参考LTFS一致。修正探针被接受并报告Recovered Error Rounded parameter，回读P0=128GB。未修改通用sense成功规则。新增实机页样本/截断边界/编码回归，258 passed/7 ignored，check、strict Clippy、fmt、兼容release通过。

**运行中PID1570614** `.143:/root/tape-rs-io-20260927/physical-io-final-resume-20260927.py`，日志 `physical-io-final-resume-run.log`。从失败检查点恢复：验证源/原归档、已完成LE三文件SHA证据、RC18仍载带，省略再次搬带/读取，然后原生格式化及后续全部写读/LE互读。不能重跑原脚本或恢复脚本，先查进程。最终完成标志仍 `physical-io-final.complete`。原失败日志 `physical-io-final-run.log` 和逐字段 `native-format-mode11-*.log/.bin` 保留。第一次生成的UUID8772...只是失败尝试，**不是新卷UUID**；以成功mkltfs输出及native-final-manifest.json为准。

最终二进制已更新（此前进程退出后才替换）：performance_compare-io-final SHA76105b52549bba200b081b5793285423fca14183829dfda0dbf65a59747577db；tape-rs-io-final SHA009f46705d95296fa80837a3c3d641e31a208fbbc08ae33c57538aeedad13e89。allow_dio0，RC17槽8不动，.209不动。EOD无兼容回退已通过实机LE三文件回读，源码修复仍未提交，继续监测全流程验收。

## 2026-09-27 — 最新：取消 EOD 兼容路径；最终实机轮已启动

用户明确要求不保留不支持 EOD 的兼容性。恢复扫描现在只执行 LOCATE16 DEST_TYPE=011、CP=1、IMMED=0，再读实际位置；删除 BOP 回退和补发 SPACE EOD，设备错误原样返回。新增成功路径（无 BOP/SPACE EOD）和不支持时报错且不写带测试。256 passed、7 ignored；check、strict Clippy、fmt、兼容 release build 全通过。原始证据 `software-eod-*.log`，规范 IBM GA32-0928-08 §5.2.7 Table53。

此前 sync/mount 全流程已通过，3273文件 Rust/LE各两轮SHA，`sync-mount-functional.complete`存在。LE诊断也正常结束、卸载归槽、退出：3次实测开FM IMMED1（3/5/6ms）、闭FM IMMED0（3249/3583/7433ms）。已从实际trace断言6个成功CDB交替，写 `le-fm-immed-evidence.json` 和 `le-fm-immed.confirmed`。

**直接 EOD 首次实机已通过**：挂载 generation24、126.05855s，真实读取 LE 三个诊断文件7.55115s、SHA全部通过；实际direct0/indirect14。无可证明额外提速。流程已进入仅RC18原生格式化，新UUID8772d068-bdfc-4c87-a837-695f442fb0ac（完成需继续查日志）。

**当前实机流程 PID1552061**，`.143:/root/tape-rs-io-20260927/physical-io-final-20260927.py`；日志 `physical-io-final-run.log`，完成标志 `physical-io-final.complete`。先校验源SHA/归档旧清单，然后真实读LE三个诊断文件，再仅RC18原生格式化、全基线写入、卸载重载Rust及LE各两轮SHA。已向用户说明格式化影响，既有授权有效。不得重复启动或盲目重跑，先查日志/进程。allow_dio=0、RC17槽8不动、.209不动。最终新UUID专用 `native-final-manifest.json`；原 `RC0018L9.json` 保留旧UUID。最终候选SHA：performance_compare-io-final=022175035dafd1a0a2fd5fea315eaa68de682ca3da6f691255545cce659d458d；tape-rs-io-final=fdc3c6533752dbb3b376206c1694167aead55d8c012f4df575e1afe7837eb015。替换了未运行的旧最终候选，远端--help通过。

活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`，HEAD fc4afcb。源改动尚待最终实机结果后提交，不push。后续继续监测最终轮并保存结果，不能将未完成硬件验证报告为通过。

## 2026-09-27 19:11 CST — sync 实机通过；正常挂载 363s→125s；最终候选待实机

用户要求所有发现的问题逐项修复，并实机验收。仍在活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`，不要检查旧工作树。HEAD新增 `fc4afcb`：合批阈值测试去除错误调度假设，确定性单测+完整套件+两次并行目标复验通过。

实机 `.143`：RC18 独占，drive serial11EB4A80F1 `/dev/sg3`，changer55L3A7802K19LL01 `/dev/sg5`，LE使用同LU `/dev/sg4`；allow_dio保持0，RC17槽8不动。普通sync候选追加96文件后3241文件两轮SHA均通过，正常卸载完成，`sync-position-native.complete`存在。日志 `sync-position-run.log` / `sync-position-measurements.jsonl`。后续监督进程PID1456235 `physical-sync-mount-followthrough-20260927.py` 自动继续：快速挂载追加32文件→重载读3273文件两轮→新LE读3273文件两轮→退出，完成标志 `sync-mount-functional.complete`。当前快速挂载首轮124.856s，history_checked=false/chain_depth0；旧同代挂载363.282s。**流程还没全结束，不得重启或格式化。**

后续候选源代码已完成软件验收：普通sync/顺序read使用实时位置核验；一致Full默认mount省去历史遍历但显式mount_with_budget/recover仍审计全部历史；每fd申请读回1MiB SG预留区；写文件复用缓冲；mkltfs header buffered=1/speed=0；DP/IP开FM IMMED，结束FM/最终屏障保持非IMMED。模拟器已区分IMMED持久化；掉电测试覆盖缓存全失和部分数据已落带。254passed/0failed/7ignored，check/Clippy -D warnings/fmt通过；兼容build成功；随后用 `CARGO_ZIGBUILD_ZIG_PATH=$PWD/.scratch/ltfs-ha/probes/zig-linker-compat.py` 去除无效的GNU ld优化参数，重新链接五个入口无告警、GLIBC最高2.34。下列最终候选哈希已更新并上传，远端--help通过。原始首次失败（历史审计计数、掉电边界、合批调度）日志保留，不能把失败轮报告为通过。

独立候选已上传但未替换运行中版本：
- performance_compare-sync-position SHA a9733a8dda122a43f431aa3b0efd5837cb172063eb6b1f99e72b275a0e9daa14
- performance_compare-mount-fast SHA 2e412818e44bc46c68d23bb478ee124106fe92ff97b33bebecaa61c828419aeb
- performance_compare-io-final SHA 9e5a58d34714675a171968e6f8bdafd6e14290d776709fa1d39e3f24882f78ca
- tape-rs-io-final SHA c7886b9d20a28f8974f018c9559f2443d34f041b0f3efdb628d1f4d2af5f87d7

下一步：等sync-mount流程成功，运行已准备/上传的 `physical-le-fm-trace-20260927.py`，在私有LE追加3×4KiB并采样真实FM CDB，正常卸载退出。查实开FM IMMED后写 `le-fm-immed.confirmed`（此标志尚不存在）；然后运行 `physical-io-final-20260927.py`。后者有前置完成门控，先用最终Rust库从真实磁带校验LE的3文件、核对UUID和全部源数据、留存旧清单，**仅RC18执行原生mkltfs --yes-destroy**，检查buffered mode=1/speed=0，跑原3145文件/约17GiB/16批普通sync，卸载重载Rust两轮SHA，LE两轮SHA。格式化先向用户说明影响；既有用户已授权必要格式化，无需再次询问，但保留脚本显式门槛。最终完成标志 `physical-io-final.complete`。两个脚本均未启动。源清单 `RC0018L9.json`一直保留旧UUID，最终新清单专用`native-final-manifest.json`，不要混用。测试源已保留全部旧/新文件，未删除。

证据本地 `.scratch/ltfs-ha/probes/results/physical-rc18-20260927/`（需刷新后续远程数据）；最终软件检查为其中`software-perf-final-*.log`。任务队列 `.scratch/ltfs-ha/performance-fix-queue-20260927.md`，首轮实机报告 `physical-rc18-position-validation-20260927.md`。代码尚未提交（除合批测试fc4afcb），等待逐项实机证据完成后做相关提交，不push。

## 2026-09-27 18:47 CST — 追加定位实机通过，普通 sync 修复验收运行中

活动工作树仍是 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`。RC18 上 3145 文件、约17GiB，Rust/LE 各两轮全 SHA 通过，正常卸载及 LE 退出。详见 `.scratch/ltfs-ha/physical-rc18-position-validation-20260927.md`。用户要求发现的所有问题逐项修复，队列见 `.scratch/ltfs-ha/performance-fix-queue-20260927.md`。

新一轮不格式化：`physical-sync-position-20260927.py` PID1442245 追加3×32独立小文件，再重载校验3241文件。远程日志 `sync-position-run.log`、测量 `sync-position-measurements.jsonl`，测试 binary `performance_compare-sync-position` SHA a9733a8dda122a43f431aa3b0efd5837cb172063eb6b1f99e72b275a0e9daa14。原 manifest 保持3145文件；新 manifest 为 `sync-position-write.json` 和 `sync-position-all.json`。完成前不得重复运行或覆盖介质。后续需要 LE 交叉读。allow_dio=0。装入后 MODE SENSE 当前 header byte3=0x10（buffered mode1），compression page DCE=1；可变掩码已另存。

# Claude / Codex handoff: LTFS interoperability

Last updated: 2026-09-27. Branch: `ltfs-recovery-ibm-interop`. Active worktree: `/work/jay/tape-rs-ltfs-recovery-ibm-interop`. GitHub remote: `origin` (`JayTsu-sh/tape-rs`). Follow `AGENTS.md` to verify the worktree before continuing; `/work/jay/tape-rs` on `feature/docs` contains early design, not the current implementation.

Next authorized queue: [remaining implementation](../.scratch/ltfs-ha/remaining-implementation.md), in order: documentation, intermittent directory outcome, LE metadata semantics, performance, formal rollout. The prior 0–5 queue is complete.

## Active I/O alignment work (2026-09-27; latest)

**18:20CST latest ACTIVE validation / source restored:** Controlled old native run stopped successfully AFTER small-group sync Full4 (data554.887s,sync15.101s), before any growth writes. Source/comparison/growth restored; no held source remains. Old native logs/manifest/binaries preserved in native-before-position-fix; no need to restore media from it. New supervisor1376117 runs physical-rc18-after-position-20260927.py (rc18-followthrough.log), reformatted RC18 again and is now in optimized native write-groups. Current candidate SHA3274c32a2377e902d33da0ec4d64f5d36311e27634fa586ea6fcc14b3342408f; benchmark SHAe10defb0c962f7cca1b2ac0f0f10be8456dce195eb673b27aa7189d60579ceb3. New full suite244passed/7ignored;fmt/check/strictClippy pass, compatible build passes with external Zig linker deprecation warning. New native small group data26.258s+sync19.929s (46.187s total, vs569.988s old);growth18.328+16.208s;16GiB64.228+10.627s. Current medium holds advancing native data: do not rerun formatting or old scripts. Full SHA/reload/LE cross-read still pending; completion file must be checked. User additionally requested write-performance research; background agent writes write-performance-research-20260927.md, no hardware actions delegated. Direct remains0,RC17slot8,.209untouched.

**18:04CST physical small-write regression / controlled boundary stop:** Native baseline stream16GiB passed write+sync (56.18s+10.50s). Small-file group is still running: traced every4KiB WRITE (~0.06s) preceded by redundant LOCATE (~1.9s). To stop safely after this group's ordinary sync without interrupting SG_IO, temporarily moved remote source/comparison/growth to /root/tape-rs-io-20260927/growth-source-held-at-batch-boundary. The existing write-groups process has not read the growth group yet; it will fail source-open validation before growth writes. Followthrough stops on failure, no automatic reformat after that. Restore the source directory only after this process exits, otherwise it will proceed to2500slowwrites. Evidence native-boundary-stop.json and native-small-write-sample.strace. Preserve initial native measurements before rerun.

Fix in progress locally: append checks live READ POSITION before LOCATE; exact same partition/block skips redundant positioning, LOLU/PERR or >32bit target keeps LOCATE, query errors prevent writes. Sim's nonstandard debug EOD bit0x04 removed because SSC defines it as LOLU. Added tests pass12le_sync; full suite/build running. Do not deploy until checks finish. Need restart from common empty RC18 after safe stop and archive baseline; then complete native SHA/read/reload and LE cross-read. Direct remains0.

**17:51CST physical validation ACTIVE:** User requested continuing functionality/performance with direct I/O disabled. RC18 only, .143, drive sg3/sg4 serial11EB4A80F1, changer sg5/sg6 serial55L3A7802K19LL01; allow_dio0. LE baseline completed:3145 files/~17GiB,16 ordinary DP sync groups, Full17; reload+two SHA reads pass. Those reads contain cache hits. Additional complete FUSE teardown/restart cold read passes3145 SHA in80.44s (includes on-demand mount); repeat19.63s adds no SCSI commands. Rust cross-read of all LE files also passes: mount277.52s,read134.95s,38035indirect/0direct READs; brief strace attached, so this cross-read is diagnostic, not formal timing. Native mount backtracks historical indexes; file reads issue per-extent LOCATE. No production optimization applied.

Active supervisor pid1320280 runs /root/tape-rs-io-20260927/physical-rc18-followthrough-20260927.py; log rc18-followthrough.log. It archives LE logs/manifest/cache in le-rc18-first-evidence, cold-reads LE, cross-reads with Rust, then reformats ONLY RC18 to common empty baseline, updates RC18 manifest UUID, runs native session baseline, then LE reads Rust data. Any failure stops; don't restart whole script over advanced media. First launch failed safely at sg_inq due LE exclusive changer fd; corrected to stop LE before SG queries and query LE while it owns devices. RC17 stays slot8; no cross-tape swap acceptance claimed. Completion file rc18-functional-performance.complete is required, not yet present.

Benchmark example added write-groups: one mount across same16 write/sync groups and in-session listings, end-of-session IP checkpoint measured separately; avoids repeated recovery per group. Latest performance_compare SHA730bfff03b16aa319ca7a441f42a26db92dbdb81e4918b358012eceed700cc36 staged on.143; tape-rs-candidate production code unchanged. Full tests240passed/7ignored,all-target check/strictClippy/fmt pass before final example checkpoint adjustment; final exampleClippy+compatiblebuild pass (external linker deprecation warning retained). Latest progress must be read from remote logs before further commands.

**17:20CST user direction: disable SG direct I/O.** Verified and explicitly set /sys/module/sg/parameters/allow_dio=0 on10.131.4.143 (already0 after previous cleanup). Updated local and remote physical LE/native runners plus remote run-le-rc18.py to keep0, including cleanup; LE skips the direct-only diagnostic while disabled. No test was restarted and no tape command issued for this change. Subsequent LE/Rust physical comparison must use allow_dio=0; do not re-enable for diagnosis without new user direction. Existing application SG_FLAG_DIRECT_IO requests are unchanged, but kernel direct I/O is disabled. Runtime setting verified; no boot configuration changed. Next remains same-semantics physical comparison onRC18 using indirect I/O. Evidence: probes/results/physical-sg-direct-disabled-20260927.json.

**16:59CST RC18 confirmed, new direct-I/O blocker (supersedes preceding empty-drive state):** User finished moving RC0018L9 into drive. Changer inventory, drive MAM barcode and successful native LTFS mount all agree: RC18, UUID33356bae-954a-4c1a-9310-54958eab43d1, 512KiB, compression=true, generation1, empty, DP/IP Complete/writable. Canonical drive sg3 (alternate sg4); changer sg5 (alternate sg6). Initial private LE restart used sg5 changer and auto-selected sg4 tape. Single-RC18 probe added; initial explicit leadm mount failed because LE is in auto-mount mode; fixed probe to rely on filesystem access. With allow_dio=1, LE auto-mount READ(6) failed SG_IO EINVAL before data writes. Wrapper restored allow_dio=0. Normal private FUSE unmount completed; LE exited and PR generation2 has no registrations/holder. Native drive-load succeeded (~6s), then straced ltfs-list with allow_dio=1 reproduced EINVAL on READ(6), CDB08 00 08 00 00 00, dxfer_len524288, SG_FLAG_DIRECT_IO, ~144us, zero SCSI/sense/host statuses. Restoring allow_dio=0 and reopening let native ltfs-list pass again with identical empty generation1; medium remains loaded. No benchmark success or physical direct-write success claimed. No LE/probe workers remain; allow_dio restored0. Evidence: probes/results/physical-rc18-reboot-direct-20260927.json. Next: isolate direct mapping size/alignment/HBA limits (sg_tablesize64 observed, cause not yet proven); do not assume the kernel always falls back on direct mapping failure. LE/Rust performance must use the same effective configuration. Existing two-media scripts still contain old device paths and must not be resumed. RC17 remains slot8.

**16:50CST after user drive reboot (supersedes all earlier active hardware state below):** User reported reboot and RC18 insertion. Old orphan LOAD and reservations are cleared; no LE/test/switch workers resumed. Device nodes changed: drive11EB4A80F1 is now sg3/sg4 (same LU), changer55L3A7802K19LL01 is sg5/sg6. Never reuse the old changer sg4 mapping or old scripted HCTL counters. Serial/WWID mapping verified after reboot. Inventory initially reported RC17 still in drive, then at16:49 and16:50 showed drive empty, RC18 in storage7 and RC17 in storage8; drive TUR independently reports NOT READY / Medium not present. No outstanding requests in /proc/scsi/sg/debug. MAM barcode read failed NOT READY; earlier read-only ltfs-list failed05/2c/00, so no current label UUID verified. No writes, formats or robot moves issued this turn. Waiting for user clarification whether manual loading is still underway, to avoid concurrent changer actions. Remote evidence: after-drive-reboot-inventory.txt, after-drive-reboot-ltfs-list.log, after-reboot-identity.json, after-reboot-latest.json under /root/tape-rs-io-20260927. Next: verify manual operation finished, then freshly identify/inventory devices and load only RC18 under existing authorization; verify its label before benchmarking. Management-login request is superseded by the manual reboot.

**16:25CST final host-side outcome:** UNLOAD failed after~326s with SCSI host_status0x0003 (timeout); original LE LOAD still active in kernel despite both device-only resets and LE termination. RC18 not loaded; no robot move attempted. All our LE/format/test/switch workers are stopped, private FUSE unmounted,allow_dio0. Need library management IP/login to perform drive hard reset/force eject; user asked asynchronously, unanswered. Evidence: probes/results/physical-forced-switch-20260927.json. Do not treat earlier switch supervisor as active or resume benchmark.


**Forced switch authorized,16:18CST (supersedes queued-switch state):** User explicitly requested forced interruption and RC18 swap. Stopped switch supervisor1170428 and format controller1104471/leadm1106453. SIGSTOP privateLE1085305 prevented post-reset format continuation; device-only resets with `sg_reset --device --no-esc` through verified same-LU sg5 then canonical sg3 returned0; privateLE was SIGKILLed and its dead FUSE mount normally unmounted. No bus/HBA reset. Old LOAD still appears active in/proc/scsi/sg/debug; TUR first reported reset UA, then NOT READY/becoming ready. Native `tape-rs-candidate drive-unload --device /dev/sg3` is currently outstanding (300s timeout); do not issue another LOAD/UNLOAD or robot move until its outcome is checked. Inventory confirmsRC17indrive/source8,RC18slot7. PR1438 stillLEkey0x400000000a83048f,type3 (no preemption performed). Logs: forced-reset{,-canonical}.log,forced-tur{,-after-ua}.log,forced-pr.log,forced-unload.log (exists only after command returns). No benchmark supervisor is running. Need management address/login if host-side unload cannot finish; asked user asynchronously.


**Latest steering,16:16CST:** User requested switching to RC0018L9. RC17 LOAD still active~2032s, so no forced abort/reset. Both waiting performance supervisors1107426/1129509 were verified childless and stopped with SIGTERM before any benchmark started. They must not be restarted automatically. Existing format-second.py/LE remains active. New switch supervisor1170428 (`/root/tape-rs-io-20260927/switch-to-RC0018L9.py`) waits for format-baseline.complete (RC17 normally formatted and returned to slot), verifies both RC tapes in slots, then moves/mounts RC18 on11EB4A80F1. Read switch-to-RC0018L9.log/.complete to determine actual outcome; no switch has yet been confirmed. allow_dio remains0. This supersedes the automatic LE→Rust sequence described below.


Continue [LE I/O alignment](../.scratch/ltfs-ha/le-io-alignment-20260927.md) before the historical queues. User authorized real I/O testing and formatting selected test tapes. Canonical real target is 10.131.4.143, drive11EB4A80F1 (/dev/sg3; sg5 is the same LU), changer55L3A7802K19LL01 (/dev/sg4), RC0018L9 + RC0017L9 only. Keep the running LE on10.131.7.209 untouched. No credentials in files.

Candidate ordinary DP sync / deferred IP checkpoint, aligned stream buffers, borrowed append API and SG_FLAG_DIRECT_IO are implemented; interrupted ioctl drains pending DMA and freezes the handle without replay. 235 all-feature tests pass,7 ignored;4 subsequently added error tests pass;strict Clippy/fmt/check pass. Holo DMA interruption trace proves WRITE was still active after EINTR and waited until completion. New Holo same-sync/direct-enabled LE and Rust runs each passed all SHA and6swaps. PF media are home/unassigned, final aligned LE contents retained and backed up (`aligned-le-final-*`); EE3nodesavailable/f3unchanged/9publicationsready, allow_dio restored0. Formal binaries remain af6bfad.

Real preparation is **in progress**: independent LE `/root/tape-rs-io-20260927/le-mount`, workdir `le-work`, admin17600. RC17 imported fromI/E3to storage8. RC18 formatted successfully (365s), UUID33356bae-954a-4c1a-9310-54958eab43d1,512KiB. `/root/tape-rs-io-20260927/format-second.py` is unloadingRC18 then formatting/unloadingRC17 and filling manifest UUIDs; read `format-second.log` before further device actions. `physical-le-runner-20260927.py` waits for `format-baseline.complete` then runs LE performance with allow_dio1, restoring original value in finally. Its pid/log files are in the same directory. Do not launch a duplicate test or format. Source dataset (~19.25GiB,3147files) ready; native candidate and probes staged. Real Rust read/write is not yet verified. A second supervisor `physical-native-runner-20260927.py` (pid1129509) now waits for LE success, archives its evidence, reformats only the RC test tapes sequentially, closes LE normally and checks PR before Rust. Read both supervisor logs before any manual action. As of15:58CST, RC17 LOAD has been active~955s with LE timeout7200000ms; the candidate LOAD timeout was extended to match (targeted test/Clippy/build passed, physical run pending). FUSE fsync data-only semantics and final candidate deployment remain incomplete.

## Current completed queue (2026-09-27; supersedes historical state below)

Items 1–5 complete. Formal source **af6bfad** is deployed on .71/.72/.74; .73 has matching FUSE/client. Active deployment `/home/rocky/tape-rs-rollout-20260927`, original data `/home/rocky/ltfsd-data`, ports7400/7401, original startup script now points at this deployment. [Rollout report](../.scratch/ltfs-ha/rollout-20260927.md) records hashes/backups. Final node1 term56/round499;58 identical catalog rows;TS1000L08 Full13 active,TS1001L08 Full120 preserved. Original9 files plus new8MiB/metadata passed fresh-cache and takeover verification. FUSE unmounted/cache empty. No GitHub push.

[HA performance evidence](../.scratch/ltfs-ha/performance-20260927.md):226 tests pass,7 ignored;fmt/check/strictClippy pass. Latest closed isolated data `/home/rocky/tape-rs-performance-20260927/test-data`,6993rows,node3 round436,SR1 Full94/SR2 Full18 in slots8/9;never resume older datasets. Backups `/home/rocky/holo-performance-20260927`.

[LE/direct Rust comparison](../.scratch/ltfs-ha/performance-compare-20260927.md):two runs each,3210files/allSHA plus6swaps per run,on .72 IBMlisa42299. PF2701L08/PF2702L08 slots1034/1035,unmounted/unassigned,finalLEdata retained;empty/native/LE backups `/home/rocky/holo-performance-compare-20260927`. LE normal sync is DP index persistence; Rust commit updates both DP/IP, so earlier “both Full commits” wording was incorrect. No command trace yet; proposed SCSI optimizations are not implemented. EE three nodes available/no tasks/f3 intact,nine publications ready. Preserve original LE SR2501L08/TS1000L8. Physical acceptance deferred.

## Working agreement

The user requested unattended work on 2026-09-27: proceed through defined tasks, choose and record recommended decisions, and verify each completed task in the actual environment. The available environment is Holo-VTL plus IBM EE/LE; physical tape firmware remains unverified. A failed or unavailable lab check keeps that task incomplete. Preserve the separate formal and isolated pools, and recheck current state before device actions.

## Historical implementation and deployment (superseded above)

The branch contains LTFS full/incremental indexes, recovery/checkpoint behavior, Raft service, client and FUSE operations (directories, rename, xattrs, symlink read/create), and symlink-preserving reclaim. Production source used for this deployment is `cda359d`; later documentation/evidence commits do not rebuild the executable.

The symlink reclaim fix **is now deployed** on the formal Holo three-node cluster:

- Directory: `/home/rocky/tape-rs-symlink-reclaim-prod-20260927`.
- `.71/.72/.74` ltfsd SHA256: `32bd5541c0123559b590e9dd94c935753a30dacdb660a3e8efcc2ee3e89f8092`.
- `.73` tape-fuse SHA256: `ef237d2ea43bfaf182f24d34a3d4a2a3bf9e509e8ba21ead321112e7c87c85d9`.
- `.73` ltfsctl in the new directory: `90f7d3caee4c16261779b5f5d3608e927dd899657144e3d1c02d4db4ab5ce933`. Other existing client paths were not replaced.
- Startup remains `bash /home/rocky/tape-rs-ltfs25-20260924/start.sh N`, pointing to the new ltfsd and preserving original ports, data directory, serials and settings.
- Each server's new directory contains `pre-upgrade.tgz` (0600): closed original data, old binary and startup script; `start.before.sh` is also retained. Never restore old Raft/catalog data over advancing live state.

Last verified state after completing tasks 0–5: node2, term51/round449, three matching catalogs with 53 rows. `TS1001L08` remains generation120, DP/IP `Complete`, writable, 7MiB available. `TS1000L08` remains generation2. Original 9 file lengths/hashes/mmap/barcodes passed on a fresh FUSE cache; all 53 catalog rows and pool summaries are exactly unchanged from before this drill. Sorted catalog SHA256: `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`.

Earlier report: [interruption after completed mkltfs](../.scratch/ltfs-ha/holo-reclaim-formatted-20260927.md), following [interruption before FORMAT](../.scratch/ltfs-ha/holo-reclaim-cut-20260927.md) and [formal deployment](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md). Latest software run: 218 passed, 7 ignored; full fmt, check and strict Clippy (`-D warnings`) pass after actual warning fixes with no new suppression. The compatible Zig build succeeds with an external deprecated linker optimization warning retained. See [cleanup validation](../.scratch/ltfs-ha/holo-cleanup-20260927.md). Those earlier interruption tasks added test-only barriers; the raw-FORMAT recovery below now includes a production fix. Holo resumed from an empty Full1 source while replicated state was still reclaiming; repeated formatting and completion succeeded, with cold-cache FUSE and complete link XML fidelity checks. Formal service and original files were verified; EE nodes available, no active tasks, baseline f3 hash intact, 9 publications ready. FUSE unmounted/cache empty; isolated processes and strace stopped.

The reclaim fix routes symlinks through metadata migration rather than reading their targets. Prior two-cartridge Holo reclaim and source-format validation is in [the isolated report](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md). Original media were not reformatted or reclaimed by this queue. The separate reclaimed-media LE roundtrip is now verified below.

## Historical isolated media and queue (superseded above)

Preserve pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and its two 512MiB cartridges. Latest **closed isolated data** is `/home/rocky/tape-rs-metadata-20260927/test-data` on .71/.72/.74, ports 7500/7501, node2/round295 at shutdown, 1143 rows. Do not resume either older symlink-reclaim or reclaim-cut directory: media have moved forward.

- `SR2501L8`: slot8, destination, Full generation52 after native metadata and LE roundtrip verification, six original links, LE file, 1100 benchmark files and three sync files.
- `SR2502L8`: slot9, Full generation3 with the deleted checkpoint test file’s superseded copy; the live catalog tombstone is on SR1.
- `RT2502L8`: slot7, separate history, untouched.
- `TS1001L08`: first TAPERS drive; formal pool `rc` still contains only TS1000L08/TS1001L08.
- Latest Holo backups: `/home/rocky/holo-sync-boundary-20260927`, before/after for both SR cartridges with verified hashes. The earlier reclaim-formatted node1 directory has `at-cut.tgz`; this is evidence, not the latest resumable state.
- Device mapping differs: TAPERS changer is .71 `/dev/sg9`, .72 `/dev/sg8`. Always verify VPD serial `IBMtaper2287_LL3` and the current holder before device commands.

Ordered unattended [tasks 0–5](../.scratch/ltfs-ha/autonomous-queue-20260927.md) are complete. Latest isolated candidate is `/home/rocky/tape-rs-sync-boundary-20260927`: ltfsd SHA256 `5d2ff8eb0b39debadb95739213cfdf315cb444786f9540420053a78554ecaaa0`, FUSE `62aa241430eb677fcbca9de807a631334816875e09694f0711ea66ea318996a2`. Both TAPERS drives configured. **Formal binaries above remain unchanged; these new changes are not formally rolled out or pushed to GitHub.** Physical firmware validation remains deferred.

- [Raw FORMAT recovery](../.scratch/ltfs-ha/holo-reclaim-raw-20260927.md): durable reformatting state before destructive FORMAT. Never mix old/new nodes with that state pending.
- [Namespace/checkpoint read fix](../.scratch/ltfs-ha/holo-namespace-20260927.md): typed error causes, retryable503 only for lost ownership before read output.
- [Reclaimed-media LE roundtrip](../.scratch/ltfs-ha/holo-reclaim-le-20260927.md): file/link/xattr/mtime fidelity passed. LE SR2501L08 now Full10 in LISA slot1033, unassigned; its history diverged before the benchmark. **Do not copy it over current SR1.** TS1000L8 remains restored Full99.
- [Directory benchmark](../.scratch/ltfs-ha/holo-directory-perf-20260927.md): optional link types with old-server fallback; 1000-file median843→18ms, HTTP1013→10; cold1100-file verification passed. One initial full-suite directory test returned Indeterminate during concurrent build; isolated and full retests passed, initial log retained.
- [Hard links and explicit sync](../.scratch/ltfs-ha/holo-sync-boundary-20260927.md): hard links explicitly unsupported (LE also rejects). Write-only `user.ltfs.sync` on the FUSE mount root flushes existing writers/staging and checkpoints the active write volume; failures/ambiguous responses never report success. Holo Full30 persisted across SIGKILL and node3 takeover, with cold-cache verification. Existing stronger fsync guarantee retained.

Detailed chronology: `.scratch/ltfs-ha/handoff-to-codex.md`; file semantics: `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Probe scripts have fixed paths and explicit gates: inspect before running. Older deployment status in appended chronology is superseded by this summary and the latest dated entry.

`.codegraph/` is absent in this worktree; do not initialize it. Save subsequent implementation, tests and handoff evidence on this branch. Push only when requested.

## Remaining queue progress (2026-09-27)

Items 1–2 complete: research status consolidated; [directory outcome investigation](../.scratch/ltfs-ha/directory-outcome-20260927.md) identified cached old-leader requests after fencing, corrected the persistence test, and preserved server diagnostics without automatic replay. 219 tests passed, 7 ignored; strict Clippy/fmt/check passed; Holo fault and cold FUSE verification passed. Next: LE permissions/time semantics, then performance and formal rollout.

Latest formal state: node1 term52/round454, original 53 catalog rows unchanged and 9 files verified. Formal binaries remain cda359d deployment. Latest closed isolated data is directory-outcome/test-data (SR1 Full41, SR2 Full3, 1139 rows); use it instead of sync-boundary data. Media backups: /home/rocky/holo-directory-outcome-20260927. All isolated processes/tracers stopped and FUSE unmounted. Physical acceptance remains deferred.

## Latest metadata queue checkpoint (2026-09-27)

Items1–3 complete; [native metadata report](../.scratch/ltfs-ha/metadata-20260927.md) supersedes prior permission/time limitations. Next item4 performance, then item5 formal rollout. Latest closed isolated data /home/rocky/tape-rs-metadata-20260927/test-data: node2 round295,1143 rows,SR1 Full52/SR2 Full3; media backups /home/rocky/holo-metadata-20260927. LE SR2501L08 Full52 unassigned, after verified roundtrip. Formal node1 term53/round459,53 rows unchanged and9files verified; formal binaries remain unchanged. Software221 full-suite passes plus1 subsequent timestamp-boundary pass,7 ignored; strictClippy/fmt/check pass. All isolated/FUSE processes stopped,cacheempty.
Latest metadata candidate ltfsd SHA256 `e5fe95d960ce606a401eaa4e107cb726bda3b0798a43d8cfa47a8f4a388eefe6`.
Latest metadata candidate tape-fuse SHA256 `631113b4315ba882cc045274e4f284782cf423a06753d9af8fa41f32610cdf85`.
