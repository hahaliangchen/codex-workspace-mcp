# Organizer / Scheduler / Executor / Flow 第六轮实现检查

日期：2026-10-05。核对第五轮报告问题，并运行当前新增的定向回归。

## 结论

第五轮列出的三个问题已在实际 Agent 入口修复，相关模拟回归通过。Executor.next 现在包住模型调用、宿主工具适配器和结果归档，返回 ExecutionRound / Tick，已形成要求中的一轮执行边界。工具适配器以异步回调提供，不要求为了文件结构继续搬移全部工具实现。

仍有一项新请求切换边界需要补齐。尚未使用真实模型和浏览器验证完整产品流程，不能把本次结果称为全部验收完成。

## 已确认修复

- 恢复使用 request_completed 显式标记，区分请求完成与工作队列暂时耗尽；总目标未结束时保留树和交付。兼容旧快照的恢复规则也有定向覆盖。
- Scheduler.apply 返回 DecisionChanges，Agent 同一决策副本中同步 TaskTree.deprecate_nodes；废弃实例不再留下阻止汇总的未完成树节点。拆分与 upstream_problem 临时修复仍可恢复原任务。
- 统一 dependency_frame / declared_dependency_frame / normalize_dependencies；节点引用按有效版本解析，精确 work ID 不会被重定向至另一个实例。
- Executor.next 完成模型轮、执行工具适配器、归档与接收 Tick，Agent 在返回后才做下一次组织决策；模型调用开始时捕获完整身份。

## 剩余边界 / P2：未完成复杂任务中切换到简单新请求

位置：`src/agent_service.rs:2729–2751,2767–2806,2964–2980`。

restore_execution_session 正确保留尚未完成的旧树。新消息通过 session_continuation handoff 让 Organizer 决定继续、修改或分派新工作，但 apply_organizer_decision 没有明确的“当前用户请求替换旧目标”树生命周期处理。

触发例：旧树中拖拽功能还未做完，用户说“不用做这个了，先解释某个技术点”。Organizer 按简单回答协议派发 node_id=direct、completion=output、final_answer=true，且没有额外 flow_update。

代码将这个 order 判为 simple，跳过创建树节点；旧树却仍 enabled，接着 work_node_ready(direct) 不通过。错误发生在 Scheduler.apply 处理替换之前，旧工作尚未被废弃，新答案也无法执行。Organizer 可以通过额外 flow_update 创建子节点绕过校验，但这会把新问题继续挂到旧总目标；旧目标边界也可能进入 Worker 输入。

要求：在决策协议中明确继续当前目标、在当前目标下派子任务、替换为新用户目标。替换时归档旧树、保留灰色历史，并建立新有效目标；继续与拆分仍保留旧交付。无需增加执行状态枚举或模型配置。

建议增加实际 Agent 入口回归：旧复杂任务未完成 → 新简单请求，以及旧复杂任务未完成 → 新复杂目标。断言 Worker 收到新目标边界，不继承被取消的目标，旧任务不影响新请求结束。

## 本次运行的验证

| 命令 / 过滤器 | 结果 |
| --- | --- |
| cargo test agent_flow_ -- --nocapture | 3 项通过：轮数边界恢复后继续与结束后新任务、阻塞任务替换、回溯新版交付 |
| cargo test work_scheduler::tests:: -- --nocapture | 10 项通过：回溯、版本绑定、字段依赖、继续与空转约束等 |
| cargo test agent_decision_keeps_split -- --nocapture | 1 项通过：拆分/临时修复可恢复与决策原子性 |
| cargo test direct_request_needs_explicit -- --nocapture | 1 项通过：直接请求完成标记与阻塞恢复 |
| cargo test legacy_execution_snapshot -- --nocapture | 1 项通过：旧快照恢复边界 |
| 前端 npm run build（tsc --noEmit && vite build） | 通过，Vite 构建 3.66 秒 |

合计 16 项定向测试通过。Agent 流程测试使用本地脚本模型服务，确实经过 run_task，但没有验证真实模型的计划质量，也没有验证实际浏览器交互和灰色节点画面。

本轮未修改执行代码，未运行全部测试，未重启现有服务。
