# Organizer / Scheduler / Executor / Flow 第四轮实现检查

日期：2026-10-05。对照实施要求和第三轮检查报告审查最新源码。

## 总体判断

第三轮报告中的多项修复已落实：未完成任务的指针保留、依赖输入激活校验、回溯目标退出队列、树节点到实例映射，以及调度器和任务树的共同恢复快照。但实际 Agent 派发入口仍绕开部分新逻辑，跨请求树生命周期和事件身份仍有缺口，完整流程尚不能验收。

本轮仅做源码审查与编译检查，未修改执行代码，未运行单元测试或真实 Agent 回归，未重启服务。

## 本轮确认的修复

| 上次问题 | 本轮源码变化 | 判断 |
| --- | --- | --- |
| 用户继续导致 current 丢失 | 保留未完成 current，以 session_continuation handoff 请求 Organizer 决策；select 可恢复未完成实例 | 继续路径已补上；新任务替换见 F01 |
| Flow 原始树事件重复创建卡片 | 增加 nodeToWorkMap；tree_state 映射节点、active 和 active_path | 部分修复；progress 和旧版本映射见 F03 |
| 无真实执行入口 | 新增异步 Executor.next，主循环调用它发起模型请求；调用前捕获实例身份 | 部分完成，见 F05 |
| 依赖版本/字段缺失仍激活 | activate_task、activate_next、continue_decision 调用 validate_dependency_inputs；版本不符不再传实际字段值 | 已补基本门禁；声明和实际注入不一致见 F04 |
| 待执行回溯目标仍留队列 | queue.retain 同时剔除旧目标 ID 和失效下游 | 该边界已修复 |
| 调度器与树分别恢复不一致 | execution/commit 同一事件携带两份快照；恢复优先读这一个事件 | 两份状态的共同快照已补上；不等于文件副作用已具备事务恢复 |

## 待修复问题

### F01 / P1：替换旧任务的修复没有接入实际派发入口

位置：`src/agent_service.rs:2933–2963`；`src/work_scheduler.rs:371–377,605–606,1181–1191`。

新逻辑在 Scheduler.apply(action=work) 中将当前未完成实例标记失效。但 Agent 的 work 分支自行规范化 orders 后直接调用 enqueue 和 activate_next，没有调用 apply 的这一段。

实际路径：A 未完成，用户改变要求，Organizer 派发 B。enqueue 将 A 从 running 改成 ready，清空 current，并把 B 入队；A 既未失效，也没有重新排队。B 完成后 all_done 仍要求 A 完成，导致旧目标残留在收尾判断中。原有旧队列也没有明确的新请求归属。

新增测试 `session_continuation_resumes_unfinished_task_and_supersedes_on_new_work` 直接调用 Scheduler.apply，未覆盖 Agent 的真实分支。因此存在“底层测试覆盖修复，运行入口仍走旧行为”的情况；本轮未执行该测试。

要求：规范化 orders 后走统一的决策提交入口，明确继续、拆分、替换需求各自对旧任务和队列的处理。不要把所有新子任务都视为取消父任务，也不要让已替换任务参与新目标的 all_done。

### F02 / P1：已完成任务树阻止同一会话开展下一项工作

位置：`src/agent_service.rs:2849,2863–2866,2943–2955`；`src/flow_tree.rs:175–176,187–195,238–250`。

恢复时无条件将 previous_tree 赋给 task_tree。即使 Scheduler 已 finished / all_done，也只清理 Scheduler 的指针和 handoff，没有切换任务树。

旧复杂任务的根节点已完成后：

- 新复杂任务：ensure_request_goal 因树已存在直接返回；ensure_work_child 在旧根下面添加新未完成子节点；TaskTree.apply 因“不允许给已完成节点添加未完成子节点”而拒绝。
- 新简单回答：simple 分支不创建树，但旧树仍 enabled；默认 direct 节点不在旧树中，work_node_ready 校验不通过。
- Organizer 无法通过普通 flow_update 创建第二个根或改写已完成根：树要求恰好一个根，已完成目标不可变。

要求：新的用户任务具有独立的有效树/执行计划。结束的旧树归档为历史，保留展示；继续未完成工作才恢复该树，相关历史交付按需引用。

### F03 / P2：进度事件仍产生重复卡片，旧版本还可能覆盖当前映射

位置：`src/agent_service.rs:3291–3292`；`frontend/src/model.ts:941–943,1027–1033,1077–1097,1113–1123`。

树事件已使用映射，但 report_progress 发出的 worker/progress 只有 nodeId，没有 workId。前端 progress 分支也不查 nodeToWorkMap，而是按原始 nodeId 查找/创建节点。因此实际 Worker 汇报时仍会创建额外卡片，汇报可能挂在该卡而非执行实例详情上。

另一处问题：scheduler/state 遍历所有 frames，无论是否失效都覆盖 node ID → work ID 映射，没有按有效状态和执行版本选择。frames 是 BTreeMap，遍历按 ID 排序，不保证新版本最后。例如旧 work ID `task_b`、node ID `node_b`，回溯后新 ID `node_b_r2` 排在旧 ID 前，最后映射又指向失效的 task_b。随后的 tree_state 会把当前结果写到旧实例。

要求：所有 Worker 执行事件携带捕获的 work ID、revision 和 plan revision；兼容事件通过统一映射解析。映射只选择当前有效实例，按版本确定优先级；废弃实例保留自身输入输出，不可被当前树结果覆盖。

### F04 / P2：依赖字段通过校验，却可能根本没有进入 Worker 输入

位置：`src/work_organizer.rs:32–36`；`src/work_scheduler.rs:381–450,889–896`。

schema 允许只填 dependency_inputs，没有要求同时填 upstream_ids。validate_dependency_inputs 独立查找并校验其引用；但 worker_input 只遍历 upstream_ids 来生成交付。

例如 B 的 dependency_inputs 为 `[{work_id:"A", revision:1, fields:["app_url"]}]`，upstream_ids 省略。A 已完成且有 app_url，校验通过；B 激活后输入中却没有 A 的 app_url，Worker 只能再次寻找材料或反复确认。

要求：将两个字段规范化为一个一致的依赖定义，或在提交时检查二者一致。派发校验与上下文注入必须使用同一份已解析交付，不能各自寻找。

### F05 / P2：Executor.next 仍只负责模型 HTTP 请求

位置：`src/work_executor.rs:92–119`；`src/agent_service.rs:3191–3240,3439–3572,3759`。

本轮确实已经有实际调用的 next，不能再说“完全没有执行入口”。但它只等待 HTTP 响应头并返回 reqwest::Response；流响应解析、工具运行、局部状态变更和结果收尾仍由 Agent 大循环处理，再另调 advance 构造 Tick。

R02 要求 next 推进一个完整的模型/工具工作轮并返回执行结果。目前拆出了请求部分，尚未形成单进程执行器的完整边界。

要求：继续把响应消费、工具执行及局部结果整理收束进执行器。宿主事件和工具能力可通过 runtime 接口提供；Scheduler 接收具有调用开始身份的 Tick，决定继续或交接，不要求增加复杂状态。

## 检查结果及边界

- `cargo check`：通过，31 条 warning。
- 前端 `npm run build`（`tsc --noEmit && vite build`）：通过，Vite 构建 3.77 秒。
- 未运行新增单元测试或真实 Agent 场景；上述触发路径来自源码审查。
- 未重启服务，未确认现有运行实例已加载本轮修改。

共同 execution/commit 快照解决了两份状态分别恢复的问题；真实写入、服务启动等副作用在提交前发生，中断恢复仍需核对实际结果，不能将共同快照描述为工作区事务。

## 下一批重点

1. 先修 F01/F02：实际派发入口与同会话下一任务生命周期。
2. 修 F03/F04：执行事件身份和交付注入一致性。
3. 完成 Executor 的一轮执行边界。
4. 运行实际 Agent 场景：中断后继续、改变需求、完成复杂任务后问简单问题、回溯新版本汇报、仅声明依赖字段的派发。必须覆盖 Agent 真实入口，而不只是直接测试 Scheduler.apply。
