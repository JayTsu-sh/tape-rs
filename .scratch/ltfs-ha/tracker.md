# 本地 Markdown Tracker

本仓库本轮 Wayfinder 使用本地 Markdown tracker。map.md 为唯一地图，issues/ 下每个文件为一张子票据。

## Wayfinding operations

- 每张票据包含 Type、Labels、Status、Assignee、Parent 和 Blocked by；文件编号是稳定身份，面向用户引用时使用标题链接。
- Status 为 open、claimed 或 resolved。Assignee 为空的 open 票据尚未认领。所有 Blocked by 指向的票据 resolved 后才可工作，按编号选取最前的一张。
- 先将 Assignee 设为处理人且 Status 设为 claimed，再处理票据。无原生依赖关系，因此使用 Blocked by 编号表达阻塞。
- 研究票据由 research 子代理在独立 research/<name> 分支工作，研究文件从票据链接。
- 结论追加在票据的 Answer 下，Status 设为 resolved；地图 Decisions so far 只加标题链接和一句摘要。交互票据必须有真实用户回答才能解决。
- 每会话最多解决一张非研究票据；建图会话只创建并关联交互票据。
- frontier 查询：读取 issues/ 的元数据，筛选 open、Assignee 为空且所有阻塞项 resolved 的票据。不要把所有开放票据抄到地图。
