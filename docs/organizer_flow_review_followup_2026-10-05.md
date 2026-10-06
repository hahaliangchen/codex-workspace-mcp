# Organizer / Flow 第二次实现核对

依据：[实施要求](organizer_flow_implementation_requirements_2026-10-05.md)。本文件对应首次审查之后的最新源码；上一份审查报告保留为历史，不代表本轮状态。

## 结论与验证范围

代码已经修正首次派发、树节点激活、废弃上游依赖过滤，并补上统一 Flow 投影、回溯目标版本及 Tick 身份字段。但回溯状态同步和展示身份仍未统一，执行器仍没有实际 next() 推进方法，完整流程尚不能验收。

本次只读代码并执行构建检查：Rust cargo check 成功（30 项 warning）；前端 tsc --noEmit && vite build 成功，Vite 3.71 秒。没有运行单元测试或真实 Agent 场景，没有修改执行代码，没有重启后端。

## 上次问题的变化

| 上次问题 | 本轮状态 |
| --- | --- |
| 所有新请求预设 Output/final_answer=true | 已移除该初始化路径；新的空调度器先请求 Organizer 派发 |
| TaskTree 激活使用 work_id 而非 node_id | 激活已改回 scheduler.node，正常树节点激活身份已修正 |
| 回溯无法重开完成的树节点 | 新增 revisit_node，可以重开目标；但废弃列表身份未映射，下游同步仍有阻断 |
| 废弃 Done 输出满足依赖 | activate/continue/worker_input 已排除废弃帧；旧目标版本仍需明确封存为历史 |
| Executor 只有结果包装 | 仍未完成。接入了工具过滤、许可检查、should_yield 和 make_tick，仍无执行推进 next |
| Flow 只选树投影 | 新增 unified_flow_plan，并转发 plan_revision/rewind_records；节点和边的身份映射仍未完成 |
| 回溯版本/权限/作废计划版本缺失 | 已消费 target_revision、核对写入/检查权限，作废标记与新计划版本一致 |
| 旧 Tick 无身份 | 新增身份字段，accept 校验 work_id 与 plan_revision；node/revision 校验及原子持久化仍需完善 |

## 当前主要问题

### 1. P1：回溯废弃列表是 work_id，TaskTree 按 node_id 查找

位置：`src/agent_service.rs:2930`、`src/flow_tree.rs:65`。

Scheduler.revisit 的 invalidated_tasks 收集 frames 的键，即 work_id，例如 B、C。agent_service 原样传给 tree.revisit_node，后者用 self.nodes.get_mut(inv_id) 查找树节点，而树保存的身份通常是 work_B、work_C。

查找失败被静默跳过。因此旧 B/C 在 Scheduler 中已废弃，在树中仍可为 completed/running/pending。尤其旧 C 还没完成时，父节点的完成条件仍等待 C，组织者可能无法汇总并结束新计划。

修复要求：提交回溯前，以 frames[id].order.node_id 映射树节点，并处理聚合子树与有效计划的关系。未知 ID 不能静默成功；废弃前结果应保留在历史。父节点只等待当前有效任务。

### 2. P1：回溯边界用“创建顺序或激活顺序”，会误废弃上游

位置：`src/work_scheduler.rs:866` 起。

代码新增 activated_history，但条件仍包括 frame.sequence > target_seq，两者采用 OR。sequence 是创建顺序，不是执行顺序。

反例：先创建 B，再创建 A；实际先执行 A，再执行 B。回溯到 B 时，A 的创建 sequence 大于 B，因此即使 A 已在 B 之前完成，也会被废弃。B 新版本仍可能保留 A 的依赖引用，从而形成失效输入。

修复要求：维护明确的当前有效计划顺序。用该顺序计算“目标之后”，已执行任务以实际有效执行关系确定，待执行任务以当前计划位置确定，不能再用创建顺序无条件并入。废弃历史任务不能作为默认回溯目标。

### 3. P2：Flow 展示和调试请求仍使用不同身份

位置：`src/work_scheduler.rs:206`、`src/agent_service.rs:2971`、`frontend/src/model.ts:980`、`:1056`、`:1091`、`frontend/src/components/RequestContextPanel.tsx:70`。

统一投影用 work_id 作为节点 id；worker/progress 和真实请求的 nodeId 仍用树 node_id。前端 flow/tree_state 又会按树身份建立节点。

例如 id=edit、node_id=work_edit：flow/plan 展示 edit，progress/tree_state 建立 work_edit；选中 edit 时 RequestContextPanel 仅精确匹配 nodeId，找不到记录在 work_edit 下的真实请求。回溯后多个 revision 的请求也可能合在同一树身份下。

此外 unified_flow_plan 排除了被工作实例代表的树节点，但原树连线仍直接加入，可能保留指向不存在树节点的端点。

修复要求：明确业务 node_id 与展示 work_id 的映射，事件和请求同时携带实例身份及版本；详情按 work_id/revision 过滤。所有边端点和 active_path 映射到实际画布节点。目标聚合节点单独保留，worker/progress 不再额外生成另一张执行卡片。

### 4. P2：执行器未承担执行推进

位置：`src/work_executor.rs:29` 起、`src/agent_service.rs:3671`。

执行器仍是静态代理和 make_tick：模型请求、响应、工具执行及循环都在 agent_service 内完成后再包装 Tick。没有 next(process,runtime)，也没有独立推进当前进程的入口。

修复要求：逐段提取真实的一轮模型/工具执行，返回带实例身份的结果；主循环只负责组织者交接、调用执行器和接收结果。不要只改方法名或新增一次转发。

### 5. P2：当前交付版本和字段协议仍不完整

位置：`src/work_scheduler.rs:740`、`:836`；`src/work_organizer.rs:6`。

- dependency_inputs 已支持 fields 筛选，但 schema 没有交付 revision，消费时也未校验该版本。
- revisit 排除下游时明确保留目标旧实例；旧 A@1 没有标为被新交付替代，依然可能作为正常上游使用。
- revisit 找不到有效目标时仍回退搜索全部历史帧，可重新选择已废弃的分支。
- exported_data 未在 Worker 返回 schema 和程序输出中形成完整链路。按字段选择 app_url 等业务值时，不能依赖这些值偶然写在 summary 中。

修复要求：区分交付历史与当前有效交付。依赖引用绑定执行版本，字段从有明确 schema 的 exported_data 中选择，缺失或失效时返回具体输入错误，历史重用必须明确。

## 其他未完成项

- scheduler/state 仍无请求/恢复意图限制；不同的新用户要求可能复用旧未完成工作包。需要明确新请求与恢复旧执行的边界。
- 调度和树状态仍分别提交事件，尚无恢复一致性的原子交接快照。
- changed_inputs 仍会因为改写约束、材料 ID 等重置无进展 continue 次数。revisit 的四次上限已有，但没有基于实际进展的判断。
- Tick 虽带 node_id/revision，accept 尚未校验这两个字段。
- Flow 的返工详情匹配有所收紧，但仍用命名前缀推断部分身份；应统一使用身份映射，并区分问题发现与已确认缺陷。

## 下一步建议

先修回溯列表映射和有效计划顺序，再统一 UI/请求身份。随后提取真实 Executor.next、补齐现行交付版本和恢复快照。最后用包含不同 work_id/node_id、不同创建/执行顺序的场景验证，不能仅靠 A/B/C 同名且按创建顺序执行的单独 Scheduler 用例证明集成正确。
