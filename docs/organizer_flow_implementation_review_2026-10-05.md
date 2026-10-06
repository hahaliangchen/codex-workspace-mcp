# Organizer / Flow 实现核对报告

日期：2026-10-05。核对依据：[实施要求](organizer_flow_implementation_requirements_2026-10-05.md)。

## 结论

本轮代码已经增加 select/revisit、节点与计划版本、下游作废记录、灰色节点和返工详情，并移除了普通节点完成后的自动推进路径。但实际执行入口、TaskTree 与 Scheduler 的身份和状态同步存在阻断问题，尚不能认定完整实现了组织者驱动的独立任务执行。

本次做了源码审查和编译构建，没有修改执行代码，没有运行单元测试或真实 Agent 回归，没有重启服务。

构建结果：`cargo check` 成功，30 项 warning；前端 `npm run build`（tsc --noEmit && vite build）成功，Vite 报告 3.69 秒。新执行器及调度器部分接口的未使用 warning 与源码观察一致：接口存在，但没有完整接入执行路径。

## 按 R01—R10 的实现程度

| 要求 | 评估 | 依据和缺口 |
| --- | --- | --- |
| R01 身份、执行版本、计划版本 | 部分实现 | WorkOrder/WorkFrame/RewindRecord 已增字段，scope 已含 revision；Tick 没有实例身份，旧目标输出与当前有效版本没有完整约束 |
| R02 单进程执行器 | 未达到要求 | 新增 work_executor.rs，但只包装判断和 make_tick，没有推进模型/工具工作的 next；大循环仍执行实际工作 |
| R03 完成交接与动态派发 | 部分实现 | 完成后不再自动 activate_next，支持 select；但新请求默认 output/final_answer=true，首次派发绕过 Organizer，未执行任务调整和取消协议也未完整实现 |
| R04 最小上下文 | 部分实现 | scope、材料恢复、当前结论已有基础；dependency_inputs 没有被消费，交付没有按字段和有效版本筛选 |
| R05 输入输出与实现检查 | 部分实现 | 写入/检查事实完成和 upstream_problem 已有；exported_data 没有完整协议，初始任务类型错误影响实际写入任务 |
| R06 回溯后作废下游 | 部分实现且树模式被阻断 | revisit 已标记 sequence 更大的任务并创建新版本；按创建顺序判定，不是计划执行顺序，TaskTree 仍禁止进入完成节点 |
| R07 灰色历史路径 | 部分实现 | 前端和 scheduler.flow_plan 已有灰色标记、历史节点及边；实际树模式仍使用不含这些信息的 task_tree.plan，不能保证显示一致 |
| R08 回溯详情与统计 | 部分实现 | 已有来源/目标记录和详情计数；用 startsWith 匹配节点会把 A 与 AB 等身份混淆，不能保证统计准确 |
| R09 持久化与恢复 | 部分实现 | 已读取最近 scheduler/state；无请求归属判断，独立事件未组成原子交接，没有实例身份校验和执行中副作用恢复闭环 |
| R10 停滞与 Observer 协作 | 部分实现 | 保留建议和停滞机制；修改约束文字、材料 ID 等仍会重置无进展 continue 次数，revisit 无对应收敛机制 |

## 需要优先修复的问题

### F01 / P1：新请求被程序直接当成“回答问题”

位置：`src/agent_service.rs:2838` 起，initial_order 创建路径。

当调度器没有任务或 all_done 时，代码创建：

```text
completion = Output
done_when = Direct response delivered
final_answer = true
edit_targets = []
checks = []
```

随后直接 enqueue/activate。needs_organizer 在运行帧且无 handoff 时为 false，组织者不会先看到该请求。

影响：实现功能、启动项目等请求也会先作为直接回答任务执行；结构化写工具在 Output 工作包中被过滤/拒绝。Worker 可能继续调查，或直接给文字答复就结束，无法保证用户要求的具体任务派发。

要求：新请求先交给 Organizer 分类和派发。直接回答必须由明确的任务判断产生；不要对全部新请求预设 final_answer=true。已有材料足够的实现请求直接派发实现工作包。

### F02 / P1：树节点身份与执行实例身份混用

位置：`src/agent_service.rs:3003`、`src/flow_tree.rs:214`。

组织者工作包通常 id=edit、node_id=work_edit，TaskTree 保存 work_edit。激活时却设置 active_flow_node=scheduler.id()，随后将 edit 当作 current_node_id 传给 TaskTree。

TaskTree 要求目标节点存在，因此这种普通命名组合会产生 `current_node_id must identify a task tree node`。错误发生在组织者决策已经赋给内存状态之后，无法形成可靠的交接闭环。

要求：明确 task node ID、work instance ID 和 Flow 展示 ID 的映射。调用 TaskTree 用真实树节点身份，调试及工具记录用执行实例身份；校验应在提交前完成。不能靠要求模型偶然将两种 ID 写成一样来修复。

### F03 / P1：Scheduler 的回溯没有同步到 TaskTree

位置：`src/agent_service.rs:2950`、`:2967`；`src/flow_tree.rs:215` 起。

revisit 创建 A@2 后，agent_service 仍直接让 TaskTree 进入原 node_id=A。已完成的 A 在 TaskTree 中仍为 completed，原逻辑禁止进入完成节点；旧 B/C/D 也没有同步作废，父节点聚合仍按旧树状态判断。

影响：实际复合任务使用树模式时，回溯可在决策应用阶段被拒绝；即使绕过进入限制，完成条件、灰色路径和新计划也仍不同步。

要求：回溯作为一次统一状态变更处理。Scheduler 的新实例、有效计划和废弃记录投影到树及 Flow；旧输出保留但不作为当前交付，总目标按有效任务判定。

### F04 / P1：被废弃的已完成交付仍可满足依赖

位置：`src/work_scheduler.rs:289`、`:319`、`:363`、`:683` 起。

activate_task/activate_next/continue 检查上游时主要判断 status==Done，没有排除 invalidated_by_plan_revision。worker_input 也没有排除失效上游。

例如 B 已完成，回溯到 A 后 B 被废弃但仍是 Done；新任务 E 如果引用旧 B 的 work ID，当前代码仍可能允许激活并把 B 输出放入上下文。

要求：依赖必须引用当前有效交付；失效任务不可满足依赖。校验指定 revision/plan 归属，消费 dependency_inputs 并投影所需字段。旧输出只显式作为历史材料读取。

### F05 / P2：Executor 只是执行完成后的结果包装

位置：`src/work_executor.rs:25` 起、`src/agent_service.rs:3702`。

新模块只有 should_yield/check_permits/filter_tools/make_tick。主循环只在工具执行之后调用 make_tick，再调用 scheduler.accept。

没有 next()，当前进程也没有通过执行器独立完成一次模型/工具步骤。Scheduler.apply/current_process 等新接口没有实际接入。

要求：把真实的当前进程推进逻辑提取到执行器；主循环负责调度交接，执行器负责单进程执行。不要只增加模块名或状态结构。

### F06 / P2：回溯边界仍按节点创建顺序判定

位置：`src/work_scheduler.rs:252`、`:758` 起。

sequence 来源为 frames.len()+1，revisit 使用 frame.sequence > target_seq 作废。这是创建顺序，select 却允许运行时改变执行顺序。

例：先创建 B，再创建 A，实际先选 A、后选 B。回溯到 A 时，B 的 sequence 小于 A，虽然属于执行顺序上的旧下游，却不会被废弃。

其他缺口：target_revision 虽出现在工具 schema 中，但调用 revisit 时未消费；目标查找未限定当前有效计划，可选到废弃历史任务；权限参数被忽略；作废标记写入旧 plan_revision，回溯记录却使用递增后的版本，UI 文案对应版本不一致。

要求：维护明确的有效计划顺序和回溯目标版本，在决策校验阶段确定全部废弃范围，再原子提交。

### F07 / P2：Flow 的两套投影没有合并

位置：`src/agent_service.rs:3030`、`:3038`；`frontend/src/model.ts:1044` 起。

启用 TaskTree 时后端选用 task_tree.plan，只有目标归属和原状态；没有 scheduler.flow_plan 提供的版本、废弃边和执行顺序。flow/plan 发出时也没有转发 plan_revision/rewind_records。

前端 scheduler/state 在 treeTurns 中跳过状态投影，flow/tree_state 又用旧树状态更新节点。灰色样式虽然已经写出，实际树模式不保证能使用，并可能出现树节点与工作实例两张卡片。

要求：生成包含目标层级、所有执行实例、版本及历史边的统一 Flow 投影。废弃状态优先，旧 tree_state 不得覆盖。刷新后恢复同样的路径。

## 恢复、上下文及统计的其他缺口

- 最近 scheduler/state 的读取没有 turn/请求恢复意图限制。新问题或改变任务的消息也可能复用旧未完成工作包，要明确继续旧任务与新请求的边界。
- Scheduler.accept 使用 self.current 写入 tick，ExecutionTick 不携带 node/work/revision/plan 身份，不能证明旧 tick 不会覆盖新实例。
- 原子交接还未实现：调度、回溯、Flow 和 scheduler/state 以多次事件提交，中途失败后可能恢复不同步状态。
- dependency_inputs 仅定义字段并初始化；Organizer schema、输入组装和上游消费没有形成使用链。exported_data 也未形成完整交付协议。
- 无进展 continue 的 changed_inputs 仍基于列表和文字变化；revisit 没有无进展收敛限制。
- FlowView 使用 rawId.startsWith(targetNode/sourceNode) 归属回溯记录，会把前缀相同的不同节点计在一起。应按身份映射精确匹配，并区分怀疑与确认的问题。

## 修复及验证建议

1. 先修 F01/F02/F03，确保新请求能由 Organizer 派发，树模式真实执行和回溯可进入。
2. 修 F04/F06，建立有效计划、输出版本与可靠作废边界。
3. 提取真实执行器，统一调度入口、Flow 投影和持久化。
4. 补齐字段交付、停滞收敛和精确统计。
5. 按要求文档提供真实 Agent 流程记录：正常 A→B→C；C 回溯 A 后灰色废弃 B/C/D，并重新安排 E/F；直接问答；中断恢复及旧事件边界。

现有 scheduler 的回溯单元测试主要验证独立调度器中 A/B/C 按创建顺序执行的情况，没有证明 TaskTree、agent_service、Flow 组合流程正确。本次没有执行这些测试，不能报告其通过。
