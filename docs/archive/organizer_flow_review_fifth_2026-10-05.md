# Organizer / Scheduler / Executor / Flow 第五轮实现检查

日期：2026-10-05。对照实施要求与第四轮检查报告审查当前源码。

## 本轮进展

第四轮的主要接线缺口已经修正：

- Agent 的 action=work 规范化 orders 后调用 Scheduler.apply，不再绕开决策处理。
- 已结束的前次执行可以建立新的 Scheduler 和任务树，避免给已完成根节点添加子节点；但结束判定仍有 F01 的问题。
- Worker report_progress 带 workId、nodeId、revision、planRevision；前端兼容旧事件时会查实例映射。
- 前端 scheduler/state 按有效状态、版本和 sequence 选择节点实例，避免失效旧实例因字典顺序覆盖新实例。
- dependency_inputs 参与 upstream_ids 规范化，校验与 Worker 注入共用 resolve_dependency_deliveries；只声明字段依赖时也能交付。
- Executor.next 已经执行模型请求、流消费与工具调用解析，返回 ModelRoundOutput；不再只返回 HTTP Response。

本轮没有修改执行代码；以下判断来自源码审查，未运行实际 Agent 回归。

## 仍需修复

### F01 / P1：局部工作包完成被当成整个请求完成，恢复时提前清场

位置：`src/agent_service.rs:2861–2875`；`src/work_scheduler.rs:725–726`。

完成判定是 previous_tree.root_finished OR scheduler.finished OR scheduler.all_done。all_done 只代表已创建工作包都完成且队列为空，不能证明用户总目标完成：Organizer 可能按一个子任务完成后再派下一个的方式工作，或尚未汇总根目标。

触发例：总目标需要 A、B，当前只派发 A；A 返回，尚未进行下一次 Organizer 决策时被中断或到达轮数上限。此时 all_done=true，任务树根仍未完成。用户说“继续”，恢复逻辑创建默认 Scheduler 和树，丢掉 A 的版本交付与原执行指针。即便 Organizer 根据 previous_unfinished_tree 再恢复树，也没有同步恢复 A 的 scheduler frame，后续引用 A 会遇到未知 upstream。

要求：区分“当前已派发工作完成”和“整项用户需求结束”。有未完成任务树或待交接时保留整体状态。只有明确结束的请求才归档并开启新的有效计划；简单无树任务也应有明确的请求结束标记。

### F02 / P1：废弃执行实例没有同步废弃任务树节点，可能无法汇总

位置：`src/work_scheduler.rs:1234–1252`；`src/agent_service.rs:2936–2939,2977–2992`；`src/flow_tree.rs:167–172,272–282`。

Scheduler.apply(work) 在非 need_split 的新派发中将当前未完成实例标记失效。Agent 只有 session_continuation + work 时重建树；普通阻塞/停滞交接中的替换没有同步该节点的树状态。切到 B 时，TaskTree 只把旧 A 从 running 改成 paused。

因此 A 在 Scheduler 中已废弃，不参与 all_done；在 TaskTree 中却仍是未完成子节点。B 完成后，根节点的 work_node_ready / aggregate_result_allowed 被旧 A 阻止，finish 又要求 root_finished，形成两套状态不一致的收尾障碍。

要求：决策提交应产出明确的废弃节点变更，并同步应用到 Scheduler 与 TaskTree。替换时旧节点灰色保留；拆分/临时修复时原任务需要恢复，不能误当作废弃。状态同步不能仅放在 revisit 或新消息分支。

### F03 / P2：依赖规范化仍可能把节点引用指向废弃旧版本

位置：`src/work_scheduler.rs:309–342,580–583`。

enqueue 建立 node_id → order.id 映射时遍历全部现有 frames，包括失效实例；没有按有效性与 revision 选择。BTreeMap 的 work ID 排序不保证新实例最后。

例如旧 task_b 的 node_id=node_b，回溯新实例为 node_b_r2。排序先经过 node_b_r2，再经过 task_b，节点映射最终指向失效的 task_b。新任务引用 upstream_ids=["node_b"] 时，会被规范化成 task_b；即使当前 node_b_r2 已完成，也因旧 upstream 失效而不能激活。

前端同类映射已修复，后端这个新增的规范化入口仍存在相同问题。明确使用最新 work ID 的路径不受这个节点别名分支影响。

要求：后端统一使用有效实例解析器，按 revision / sequence 选择；指定 work ID 与指定 node ID 应区分，不能让规范化和交付校验各自选到不同实例。

## 执行器剩余工作 / R02 部分完成

位置：`src/work_executor.rs:103–148`；`src/agent_service.rs:3442,3568,3722,3755`。

next 已接管完整模型响应取得和解析，属于实际进展。但项目工具执行、执行事实更新仍在 Agent 大循环，结束后另调 advance 接收 Tick。handle_yield_work 是 return_work 的代理，没有替代整个工具执行阶段。

因此 R02 的“一次 next 推进当前进程一轮模型/工具工作并返回执行 Tick”尚未完整实现。下一步应把工具执行与局部结果归档收束到同一轮执行边界；继续使用简单状态即可，不必扩展状态框架。

## 本次检查结果

- cargo check：通过，31 条 warning。
- 前端 npm run build（tsc --noEmit && vite build）：通过，Vite 构建 3.75 秒。
- 未运行单元测试或实际 Agent 流程；未重启服务。

建议实际回归覆盖：A 完成但尚未派发 B 时中断后继续；阻塞任务 A 被 B 替换后汇总；回溯新版本完成后按 node ID 引用交付；正常完成复杂任务后发起新简单任务。运行入口必须覆盖 Agent 与 TaskTree，不应只直接调用 Scheduler.apply 检查局部状态。
