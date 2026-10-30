# Organizer / Scheduler / Executor / Flow 第五轮问题修补

日期：2026-10-05。对应 `organizer_flow_review_fifth_2026-10-05.md`。

## 修补结果

### F01：请求完成与工作包完成分开

- `all_done()` 仅表示已派发工作结束，不再用于跨轮恢复的清场判定。
- Scheduler 持久化明确的 `request_completed`。成功 `finish` 和无树简单任务的最终回答标记请求完成；`blocked` 保留可恢复状态。
- 恢复时，未完成树、尚待交接的执行和局部完成交付保留 Scheduler、版本、工作指针与 TaskTree。树请求同时要求根目标结束和请求完成标记，才建立新计划。
- 完成后的交付归档不再重新生成 handoff。正常完成后的连续简单请求也能建立全新 Scheduler。
- 旧快照缺少新标记时，仅在旧 Scheduler 已结束、所有包完成且树根明确完成的情况下认定整体结束；旧无树快照保守保留，交由 Organizer 处理。

### F02：替换决策同步废弃树节点

- `Scheduler.apply` 在副本上校验和提交，返回 `DecisionChanges`，明确列出废弃 work ID 和 node ID。
- Agent 的 `apply_organizer_decision` 对 Scheduler 与 TaskTree 一起提交；普通阻塞替换、跨消息替换和回溯共用这条路径。失败决策不修改两份原状态。
- 废弃树节点保留为 `deprecated`，旧结果进入历史；不再阻止父节点汇总。替换单个工作包不再清空整棵树。
- `need_split`、`upstream_problem` 保留原任务；其他临时修复可使用 `preserve_current=true`，修复交付后 `select` 原任务恢复。

### F03：依赖解析绑定一致的有效执行实例

- 规范化与交付校验共用有效实例解析器。node ID 按 revision、sequence 选取有效实例，废弃 frame 不参与选择。
- `dependency_inputs.work_id` 是精确执行 ID；`node_id` 是节点引用。失效的精确 work ID 不会重定向到其他版本。
- 入队时把节点引用固定成 work ID，规范化 upstream、校验字段和 Worker 输入使用同一份交付。显式 revision 仍需匹配实际交付。
- 更新 Organizer 工具参数，使仅声明 node ID 的依赖也能正常表达。

### R02：模型、工具与结果归档进入同一轮边界

- `WorkExecutor.next` 执行模型请求和响应解析，调用宿主工具适配器，再完成局部交付归档、TaskTree 完成同步和 Tick 接收。
- Agent 提供异步宿主工具适配器；原有工具权限、执行、notebook 和 Observer 逻辑在这个适配器内运行。Agent 不再在 `next` 结束后另行调用 `advance`。
- `next` 返回包含 `ExecutionTick` 的 `ExecutionRound`。执行前后检查 work ID、node ID、revision、plan revision；Tick 反映轮结束后的真实交接状态。
- 文本交付与工具交付共用轮末归档；工具阶段取消时，已记录的局部结果在取消收尾前保存。每轮输出 `execution/tick` 和一致的 `execution/commit`。

## 验证

- `cargo check --quiet`：通过，仍有未使用代码相关 warning。
- `cargo test --quiet -- --nocapture`：126 项全部通过。
- 前端 `npm run build`：通过，Vite 构建 3.71 秒。
- 本轮新增 8 项测试；完整 Agent 回归通过本地假模型 HTTP 服务运行 `run_task`，包含：A 完成后到达轮数上限再继续并引用 A；阻塞 A 被 B 替换后完成根目标；回溯 B 后按 node ID 使用 r2 字段交付；复杂任务结束后连续发起简单任务。
- 其他测试覆盖拆分/临时修复后恢复原节点、无树请求的明确完成标记、blocked 恢复、旧快照兼容、精确废弃 work ID 拒绝、revision/sequence 选择和决策失败不产生部分状态修改。原有顺序工具测试同时验证轮结束 Tick。

未调用真实模型，未重启运行中的服务。
