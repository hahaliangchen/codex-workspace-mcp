# Organizer / Scheduler / Executor / Flow 第三轮实现检查

日期：2026-10-05。对照 `organizer_flow_implementation_requirements_2026-10-05.md` 检查本轮源码。

## 结论

本轮修复了多项上次报告的问题，但尚不能按完整流程验收。仍有继续执行指针丢失、Flow 身份混用、依赖契约未阻止错误派发等问题。Executor 增加了结果结算入口，但还没有承担实际模型和工具执行。

本报告依据源码路径审查和编译检查；未运行单元测试或真实 Agent 回归，未重启服务。

## 本轮确认的修复

- `RewindRecord` 新增 `invalidated_node_ids`；Agent 将节点 ID 而非 work ID 交给 `TaskTree.revisit_node`。
- 回溯范围主要按 `activated_history` 与待执行队列确定，已去掉原先把创建顺序也作为截断条件的逻辑。
- 旧目标实例会被标记为失效；树节点原结果进入历史记录。
- `yield_work` / 交付加入 `exported_data`，下游输入支持指定版本和字段选择。
- Worker 请求记录增加 `workId`、revision、plan revision；请求调试面板能够匹配执行实例。
- `Scheduler.accept` 校验 Tick 的 work ID、node ID、执行版本与计划版本。
- 统一 Flow 投影将树边端点和父节点引用映射到执行实例 ID。

上述是源码层面的修复确认，不代表所有相关行为已经经过运行验收。

## 仍需修复

### F01 / P1：继续会话会清空未完成任务的当前指针

位置：`src/agent_service.rs:2833–2840`；`src/work_scheduler.rs:380–433`。

恢复 Scheduler 后，只要是第二轮及以后且 prompt 非空，就执行 `scheduler.current.clear()`，但保留原 frames 和 queue。原来的 running frame 在激活时已经移出 queue。

例如 A 执行中断，用户说“继续”：恢复后的 A 仍是 running，却没有 current，也不在 queue。`continue_current` 要求存在当前未完成帧；`select_task` 只选择排队任务。因此直接 continue/select 无法恢复 A，需要 Organizer 额外猜出应以相同 order 重新入队，容易产生错误决策和额外往返。

要求：区分继续未完成任务、修改需求和新增任务。需要 Organizer 评估新消息时，保留可恢复的执行实例身份，通过交接通知它；恢复后应有显式 resume 路径。新增请求也不能把旧任务隐含计入新任务的完成条件。

### F02 / P1：Flow 的统一投影仍被原始 tree_state 再次拆开

位置：`frontend/src/model.ts:1056–1076`；`src/agent_service.rs:3017–3022,3691`。

`flow/plan` 已按 work ID 建立执行节点；随后 `flow/tree_state` 又按 raw tree node ID 创建节点，并用原始 tree active / active_path 覆盖执行高亮。

当 work ID 为 `edit`、node ID 为 `work_edit` 时，先创建 `edit`，随后又创建 `work_edit`，同一任务出现两张卡。回溯产生多个执行版本时，单一树节点的当前状态与多张执行实例卡更容易混在一起。后续 scheduler/state 不会删除额外节点，而下一次 tree_state 还会再次覆盖高亮。

要求：树快照只更新归属与汇总元数据；执行卡统一按 work ID 定位。使用显式 node ID → 当前有效 work ID 映射，历史版本仍各自保留；active 与 active_path 同样经过映射。

### F03 / P2：Executor.advance 是结算入口，尚非实际 next 执行入口

位置：`src/work_executor.rs:85–107`；`src/agent_service.rs:3130–3161,3678–3686`。

新增 advance 会判定完成、附加交付、同步树、构造并接收 Tick，这是有效的结果处理提取。但实际模型请求、流响应和工具分发仍在 Agent 大循环内完成；advance 在工作结束后才调用。

R02 要求 `Executor.next(process,runtime)` 实际执行当前进程的一轮工作，Scheduler 接收结果并决定继续或交接。当前仍没有这样的入口，不能将 advance 等同于已经完成 R02。

要求：把一轮模型与工具调用逐段移入实际执行器入口，运行时依赖可通过参数提供。Tick 应携带调用开始时捕获的执行身份，再由 Scheduler 接收。保留简单 ready/running/done 状态即可。

### F04 / P2：指定交付版本或字段不满足，仍能激活 Worker

位置：`src/work_scheduler.rs:380–420,782–834`。

激活只检查 upstream frame 已完成且未失效，不验证 `dependency_inputs` 指定的 revision 与 fields。`worker_input` 发现版本不符时仅附加 error；缺字段时仅附加 missing_fields，仍向 Worker 派发任务，版本不符时甚至继续传入实际版本的字段值。

结果是输入不满足契约的问题再次交给 Worker 自行调查，可能引发重复确认或使用错误版本。字段选择已经实现，但派发前校验尚未闭环。

要求：激活前解析交付引用并检查版本与必要字段。失败时保持任务未激活，向 Organizer 返回具体输入缺口。若需要调查缺失事实，应由 Organizer 明确分派调查工作。

### F05 / P2：回溯到待执行目标时，旧目标留在队列

位置：`src/work_scheduler.rs:932–993,413–429,498–504`。

revisit 显式支持目标仍在 queue 的分支。旧目标被标记失效，但它不在 downstream invalidated 列表内，`queue.retain` 只剔除该列表中的下游 ID，留下旧目标 ID。

新版本执行结束后，旧目标仍占着队列：activate_next 跳过失效任务，all_done 又要求 queue 为空，导致无法正常结束并报告“上游结果未满足”。这是队列目标分支的边界问题，未声称常见的“回到已执行 A”路径同样触发。

要求：禁止回溯未执行目标，或正确支持该分支；若支持，原目标与失效下游都必须退出有效队列，历史 frame 保留展示。

## 持久化仍需补齐

`scheduler/state` 与 `flow/tree_state` 通过多个独立 emit 写入，恢复又分别读取两类事件的最后记录。它们没有共同的原子提交边界，进程在两次写入之间停止可能恢复不匹配的调度指针与树状态。增加统一提交 ID 仍需配合原子快照/事务及恢复规则，不能只依赖事件顺序。

## 本次检查结果

- `cargo check`：通过，30 条 warning。
- 前端 `npm run build`（`tsc --noEmit && vite build`）：通过，Vite 构建 3.58 秒。
- 未运行单元测试、真实 A → B → C / 回溯 / 中断恢复场景。
- 未重启现有服务；当前运行实例是否加载这些源码未确认。

## 建议修复顺序

1. 修复继续任务的指针和有效计划恢复。
2. 统一前端执行实例身份，消除重复节点和高亮覆盖。
3. 补充派发前输入契约校验、队列失效清理及原子持久化。
4. 提取真实 Executor.next，随后提供正常、回溯和恢复三类实际流程记录。
