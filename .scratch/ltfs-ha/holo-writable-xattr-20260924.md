# 可写 xattr：本机内核与 Holo/LE 双向验收

2026-09-24，ltfs-recovery-ibm-interop。已完成隔离测试，发现并修复空 Base64 属性的 IBM XML 兼容缺陷。原集群已恢复，尚未将可写 xattr 发布到原集群。

## 结果

- 本机真实 FUSE 调用通过：二进制/NUL/0xff、空值、64 KiB、CREATE 冲突 EEXIST、REPLACE 缺失 ENODATA、替换、删除及重复删除、目录 get/list、内部属性 EPERM、脏 writer 同步后继续写入保留属性。
- Holo tape-fuse → LE：文件及目录二进制属性、空值、已删除属性均读回正确。
- LE 修改 → tape-fuse：替换二进制值、创建文件/目录属性、删除文件/目录属性后，LE 写出 2.4.0 gen58；隔离集群恢复并经全新 FUSE 挂载读回通过，空值仍存在，删除未复活。
- 原生文件内容 SHA256、mtime、UID、extent 在三阶段一致。XML 对比 gen55（失败版）、gen57（修复版）和 LE gen58；实际数据读取另行核对。
- 本轮无断电或 SIGKILL，不声称新增提交中断场景已实测。

## 发现的缺陷与修复

首次 LE 挂载 gen55 返回 LTFSI5030E Unexpected xattr size。最小离线样本只有一个零长度文件和一个空 Base64 属性；现场 IBM 参考 libltfs 的 xml_schema_from_file 对 `<value type="base64"></value>` 返回 -5030，仅改成 `<value type="base64"/>` 即返回 0。源码 xml_reader_libltfs.c 的非空元素分支对解码后长度 0 报错，自闭合元素分支直接接受零长度。

src/ltfs/index.rs 的公共 write_xattrs 对空字符串输出 Event::Empty，保留 type 标记；全量和增量共同调用该序列化路径。修复前新增回归失败，修复后成功并验证 Rust 解析往返。没有删除空值规避问题，也没有改变二进制属性的编码语义。

修复版重新挂载原 gen55 测试带，通过同值 REPLACE 提交 gen56，正常停机写 Full gen57。LE 随后实际挂载、读取、修改成功，并写出2.4.0 gen58。避免只靠 Rust 自己的解析器往返判断 IBM 兼容性，是本次回归的关键。

## 隔离、版本与收尾

原三节点正常全停，原 TS1001L08 从 drive0 归槽6。测试仅归属 RT2502L8，独立目录 /home/rocky/tape-rs-xattr-20260924/test-data、端口7500/7501；LE使用 TS1000L8、IBMlisa41D6C、/ltfs。介质复制前双方均卸载且 LE unassigned；保留副本不得归属原集群（与原 TS1000L08 同 UUID）。

Holo /home/rocky/holo-xattr-20260924 保存 rt-before、le-before、native-full（失败gen55）、native-fixed（gen57）、le-result（gen58）。测试结束 RT2502L8 在槽7，TS1000L8在LE槽1032且unassigned；测试FUSE和节点全部停止，副本保留。

隔离最终 ltfsd SHA256：f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d；tape-fuse：fac4a6ae370f978c39b5b15bf24985f03899c2a63954f5505ca657238fdcad86。FUSE使用既有 zigbuild glibc2.34 构建方式；首次普通构建因GLIBC_2.39依赖无法启动，没有写入。

原集群恢复 node1/term25/round207，TS1001L08仍gen58 Complete。三节点实际exe均 /home/rocky/tape-rs-read-prod-20260924/ltfsd，SHA e1a8c4fde9bffdb9fcee1108d4cd4c5bbbaa537b974d13d3838b02e3d302b410。原9文件长度及SHA256逐一一致。EE3节点available、无活动任务，/gpfs/tapers-del/f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e；9个Holo publication ready。

探针过程记录：LE挂载点门控最初误查/ltfs/TS1000L8，已改为验证实际/ltfs；首次只move未mount返回EBUSY，之后显式mount才得到上述真实XML缺陷。离线解析器最初缺XML声明，随后完整文件因无设备label.blocksize而不能直接离线解析，最小零长度文件消除了此依赖。恢复原集群入口需bash调用，首次直接执行无执行权限失败，最终实际进程与读回验证后才判定恢复。上述准备错误均不作为通过证据。

## 软件验证与证据

cargo test --all-features：201通过/0失败/6门控跳过；独立 kernel_file_operations --ignored 通过。cargo check --all-targets --all-features通过；clippy完成且无新增告警；git diff --check通过。全仓fmt仍历史基线差异，仅整理本轮触及片段。测试后仅有格式和文档整理，没有新语义改动。

日志 /tmp/xattr-{empty-red,empty-green,kernel-final,interop-final-tests,interop-final-check,interop-final-clippy,interop-final-fmt}.log。仓库证据 probes/results/writable-xattr-* 包含现场失败、修复后LE成功、FUSE读回、三个原生XML、离线最小失败/成功XML与解析器结果、原集群恢复状态和EE状态。可重复探针 probes/writable-xattr.py 有目标门控；xattr-xml-oracle.py 仅用于最小零长度文件离线样本，不访问设备。

下一步统一部署已验收版本并在原集群专用路径做可写属性烟测；跨带属性修改仍EXDEV，根目录LE虚拟控制属性和完整LE能力对齐仍未完成。所有工作树改动未提交。
