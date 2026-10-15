# 按功能查看代码 Flow

首版将历史数据访问收敛到 `session_history/store.rs`，并提供人和 AI 共用的代码结构视图。代码图不进入模型的系统提示或聊天历史快照；只有 AI 调用普通工具时，才以工具结果交付。

## 历史读取边界

- 内部 `query_events`：精确事件访问和检索共同经过的参数化查询执行点。
- `load_events`：按调用者提供的会话、编号范围、事件类型、turn 和工具调用 ID 读取原始记录，结果统一按 `seq` 升序。指定 newest 时先选择最新 N 条，再升序返回。
- `read_event`：通过 `load_events` 读取一条原始记录，按字符分页；保留数据库 JSON 原文，包括空白，不重新序列化。冻结上界是调用者明确提供的访问边界。
- `requirement_snapshot`：通过 `load_events` 返回历史上界和原始用户要求的升序引用。
- `read_session_history`：保留既有检索与响应协议；数据库检索集中于同一个 store，格式组装仍由历史模块完成。

压缩只负责材料冻结、允许读取的引用、阅读范围记录和交付校验。主循环、压缩、恢复、资产恢复和界面读取不再各自实现原始消息查询。当前上下文窗口、冻结材料、资产快照是不同数据，不强行当作原始聊天历史。写入时分配事件编号和清理技术快照的 SQL 不属于消息读取。

## 入口

- 界面：左侧主导航“技能页面”中的“代码 Flow”；会话顶部也有入口。代码工作区可以指定另一个本机项目。
- AI 工具：先 `read_skill({"name":"code-flow"})`，成功交付正文后才提供 `read_code_flow`。
- HTTP：`POST /agent/skill-packages/code-flow/tools/read_code_flow`。

三个入口使用相同的 `code_flow::read`，返回同一结构，没有额外模型调用，也不运行测试。

```json
{"workspace_root":"/path/to/project"}
```

先读取功能目录，再指定 `feature_id` 查看一个功能的节点与调用关系；指定 `node_id` 返回该方法的当前完整源码、直接调用邻域和相关边界，明确标注 `scope=node_neighborhood`，避免重复交付整个功能图。界面节点详情也可以聚焦该节点。

```json
{"workspace_root":"/path/to/project","feature_id":"history","node_id":"records"}
```

## 功能声明与客观关系分开

根目录 `code-flow.json` 是人或 AI 明确维护的架构声明，版本 1。首批功能为历史读取、压缩、资产、视觉。以 parent 形成分级目录；每个节点用 file、name、module（文件内模块路径）、impl_type 绑定源码。节点 ID 不依赖行号。

每个节点声明 label、role（entry/unit/shared）、responsibility、inputs、outputs，以及可选 tests（Rust 测试完整路径）。角色和职责是声明，宿主不根据名称、关键字或连线数量推断“独立”或“重复”。可以用已有文件编辑工具修改声明。

宿主复用 Rust 索引并刷新源码，提取实际调用表达式。图中 edges 带调用位置、原始代码行与文件版本；公共方法只保留一个节点。循环仅在布局计算时收敛，原始节点与边保持完整。

功能之外的调用在 boundary_calls 中保留，外部调用者在 external_callers 中保留。界面节点详情与导出的 JSON 都能查看这些记录。未知接收者、歧义、宏和动态派发明确标记为未解析，不能把图视为完整编译器调用图。

源码改变后刷新图，失去定位的声明显示 missing，重复定位显示 ambiguous，不沿用旧边。返回源码时再次通过已有符号读取器核对文件版本。

## 从节点测试

节点 tests 绑定现有测试源码，返回 `cargo test <完整路径> -- --exact` 的 argv 和源码引用。人可复制命令；AI 用已有执行工具运行。这里只声明测试入口，绝不伪造通过状态。测试缺失明确显示 missing。

首版支持 Rust；尚未实现 TS/Python/Go 调用图、运行时覆盖、自动架构评判、自动执行测试和测试结果持久展示。对应索引已有，但不能将它们误报为此版本已支持。
