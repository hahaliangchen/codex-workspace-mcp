# Observer / Organizer 节点复盘实施报告

日期：2026-10-05。依据 [实施计划](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_organizer_node_review_plan_2026-10-05.md)。

2026-10-06 更新：独立检查发现 F01（变化建议继承旧处理）和 F02（事实绑定混合源码版本），已修补。最新状态及 150 项后端、6 项前端回归见 [修补报告](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_organizer_node_review_fix_2026-10-06.md)。下文保留 10 月 5 日的实施和记录基线。

核心执行链已实现：Observer 围绕当前 Organizer 决策和节点交付观察，Organizer 在正常决策中处理建议，Worker 收到实际落实后的工作包。边界记录持久化，结束时停止观察并汇总已有结果。下列测试及流程记录使用本地脚本模型；真实模型的判断质量和成本收益尚未验收。

## 已完成

| 项目 | 实现与代码位置 | 验证结果 |
| --- | --- | --- |
| 当前决策与节点输入 | [observer_service.rs](/D:/enterpriseProject/codex-workspace-mcp/src/observer_service.rs:63)、[observer_system.md](/D:/enterpriseProject/codex-workspace-mcp/prompts/observer_system.md)。保留当前目标、决策、完成条件、交付摘要；按优先级装入实际活动、选定上游、材料索引、最多 6 条路径摘要和 3 条相关记忆。常规预算 12,000 字符，异常长的受保护目标明确标记 overflow，不截掉目标。 | 输入预算测试及真实构造的 HTTP 请求断言通过；节点复盘不发送完整 Worker 对话或源码。 |
| 持久边界和幂等 | [observer_service.rs](/D:/enterpriseProject/codex-workspace-mcp/src/observer_service.rs:158)、[agent_service.rs](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:3611)。assignment/handoff 与 execution/commit、Scheduler、树共享 SQLite 事务；独立的 sealed 提交 ID 保存执行后的事实。review_id 和 source_event_seq 指向原提交。 | 快速 A/B/C 的 6 项边界、重复提交以及原始提交版本关联通过。 |
| 有界异步观察 | ObserverSession 使用容量 1 的通知通道，SQLite 为边界事实源；progress 只保留最新异常快照，按内容和 5 秒间隔去重。每会话一个模型许可，节点观察和可选 consult 共用，请求限时 12 秒。 | 恢复待评估交付时，持有咨询许可不会启动第二个请求；释放后仅评估一次。重复调查场景出现异常观察。 |
| 结束、取消、失败 | [ObserverSession.finish](/D:/enterpriseProject/codex-workspace-mcp/src/observer_service.rs)。取消在途请求，保留 timeout/cancelled/unassessed/failed，合并未开始的 assignment；结束汇总仅计算已有节点复盘和耗时，无新模型调用。异常 Worker 返回也取消观察并尽力写入中断状态。 | 真实 12 秒 HTTP 超时与取消测试通过；结束清理小于 3 秒，HTTP 请求计数仍为 1。 |
| Organizer 实际落实建议 | [worker_work_state.rs](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:494)、[work_organizer.rs](/D:/enterpriseProject/codex-workspace-mcp/src/work_organizer.rs)、[organizer_system.md](/D:/enterpriseProject/codex-workspace-mcp/prompts/organizer_system.md)。accepted/adjusted 必须匹配实际 resulting order 字段、revisit 或最终 summary，记录 application 和 decision_id；仅“已读”不算落实。 | 空回执校验及原子性测试通过；正常路径、调查纠偏、回溯有实际约束/决策响应。 |
| Worker 独立执行 | [agent_service.rs](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:1846)、[worker_unit.md](/D:/enterpriseProject/codex-workspace-mcp/prompts/worker_unit.md)。移除静态和动态默认提示词中的建议回执要求，以及默认报告 schema 的 observer_responses；保留旧会话处理兼容和可选历史咨询。 | 正常路径 Observer 开/关均为 3 次 Worker 模型调用；实际发送的 Worker 系统消息不含 observer_responses。简单回答为 1 次 Worker，无查询工具和回执轮。 |
| 完整实例版本隔离 | request_id/work_id/node_id/revision/plan_revision 绑定输入、review、advice 和调试请求。replace 更新观察目标和记忆缓存，旧建议归档。旧计划只有显式重新采用才生成派生建议，原 identity 不变。 | Observer 开启的目标替换、在途旧请求晚到、同 ID 新版本、显式重新采用测试通过。 |
| 迟到交付对下游的影响 | [late_delivery_notifications](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:480)。仍有效的封存上游交付迟到时，向当前计划提供独立来源通知；保留旧意见身份，由 Organizer 显式采用并落实调整或 revisit。不会自动重新打开节点。 | 版本仍有效才通知；已废弃实例、增加 revision 和替换请求均不通知；重复来源不产生多条意见。 |
| 经验与事实 | [save_observer_lessons](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:961)、[memory.rs](/D:/enterpriseProject/codex-workspace-mcp/src/memory.rs:343)、[database.rs](/D:/enterpriseProject/codex-workspace-mcp/src/database.rs:37)。事实和路径经验分开存，完全相同记录幂等。事实仅接受执行中确认的 known_read_targets；源码 hash 变化后旧事实留历史，当前搜索排除。符号描述使用已有材料。 | 同任务多个经验、重复经验、源码新旧版本历史保留与有效搜索测试通过。 |
| Flow 节点详情 | [model.ts](/D:/enterpriseProject/codex-workspace-mcp/frontend/src/model.ts:578)、[FlowView.tsx](/D:/enterpriseProject/codex-workspace-mcp/frontend/src/components/FlowView.tsx:560)。按完整身份展示派发理由、交付复盘、结论、处理、经验、来源和版本。未评估、合并、超时等有明确标签；普通复盘不生成任务节点。 | 事件重放覆盖晚到结果、ID 复用、不改变高亮和建议关联；浏览器检查了正常节点和执行完成但复盘未完成的页面。 |
| 上下文调试 | [RequestContextPanel.tsx](/D:/enterpriseProject/codex-workspace-mcp/frontend/src/components/RequestContextPanel.tsx:43)。按当前/归档 request/work/revision/plan 筛选；Observer 请求记录 review_id、source_event_id 和原始响应，仍遵循已有调试开关。 | TypeScript 构建通过；归档 ID 的 request_/turn_ 前缀处理已补齐。 |

## 实际执行记录与调用计数

这些记录由实际 `run_task` 入口执行 Organizer、Worker、Observer，通过本地 HTTP 脚本模型收发请求；JSON 保留完整事件和模型请求体。它们验证程序组织、上下文、版本隔离和调用数，不代表真实模型主动提出了高质量建议。测试中的 HTTP 协调用来稳定捕捉已到达建议，生产执行没有等待 Observer 的门禁。

| 场景 / 记录 | Worker 模型 | Worker 工具 | Organizer | Observer | 已完成观察 | 完成观察耗时合计 | 建议处理 | 因观察新增 Worker 回执轮 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| [正常 A → B → C](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/observer_normal_a_b_c.json) | 3 | 2 | 4 | 6 | 6 | 24 ms | 2 | 0 |
| [重复调查纠偏](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/observer_repeated_investigation.json) | 3 | 4 | 4 | 6 | 6 | 96 ms | 1 | 0 |
| [目标替换 / 同 ID 重用](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/cancelled_drag_goal.json) | 2 | 2 | 3 | 3 | 3 | 51 ms | 0 | 0 |
| [回溯与重新采用](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/use_repaired_b.json) | 3 | 1 | 4 | 6 | 6 | 122 ms | 1 | 0 |
| [直接知识回答](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/explain_closures_directly.json) | 1 | 0 | 1 | 0 | 0 | 0 ms | 0 | 0 |
| [Observer 请求失败](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/observer_unavailable.json) | 1 | 0 | 2 | 2 | 0 | 0 ms | 0 | 0 |

计数来自最后一次完整测试运行。工具计数包含 yield_work 等 Worker 工具，不等同于项目查询或写入次数。观察耗时为 completed review 的 elapsed_ms 合计，属于本地脚本 HTTP 的测量值，不含未完成请求，也不是任务总耗时。各场景固定 Worker 调用数由断言检查；正常路径另与关闭 Observer 的相同脚本比较，均为 3 次。

正常 B 的输入包含 A 已封存摘要，Worker 根据 Organizer 落实的约束推进。重复调查的 3 个相同查询在一个真实 Worker 回合内执行，随后 Organizer 用已知结论安排 synthesis，未增加读取意见的空转回合。目标替换保留旧灰色历史，并检查新 Worker/Organizer/Observer 请求不携带旧目标。回溯使用新的封存版本，旧实例保留历史，旧计划建议通过显式重新采用关联新决策。

直接回答派发与交付合并为一个观察事项；此次执行结束过快，未启动模型，交付记录显示 unassessed。系统不为补齐判断而延迟回答或追加结束后模型调用。

## 验证结果

- `cargo check` 通过；全量 `cargo test`：147 passed，0 failed，耗时 14.45 秒。最后将 progress 去重限定到同一实例、保留当前请求仍有效的旧计划路径摘要后，追加 `cargo test observer`：17 passed，0 failed。
- `node --test tests/*.test.mjs`：5 passed，0 failed。
- 前端 `npm run build`：TypeScript 检查和 Vite 生产构建通过，705 modules。
- 浏览器使用实际导出的事件驱动真实 FlowView：正常 B 节点能查看两项复盘、原始来源和落实到 C 的约束；直接回答显示“执行已结束，观察未完成”，没有“已检查无问题”的替代文案。
- 定向 `git diff --check` 通过。临时预览页、备份和本次启动的预览服务已清理。

![正常节点的复盘与建议落实](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/flow_normal.jpg)

![执行完成但观察未完成](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_node_review_records_2026-10-05/flow_unassessed.jpg)

## 部分验收及未验证项

| 项目 | 状态与边界 |
| --- | --- |
| 真实模型意见质量、路线缩短和成本收益 | 未验证。已有程序调用计数和输入隔离证据；尚不能据此宣称真实模型会减少项目调查或节省总成本。 |
| 崩溃后的服务恢复 | 持久记录、重复提交，以及从 reviewing 恢复同请求的一次评估已验证；未通过杀掉整个生产宿主并重启的方式验收。已结束/取消记录不会自行恢复观察。 |
| 普通编译失败后同实例修复 | 原有 Worker/Executor 检查机制和提示词保持，普通检查失败未增加独立 Observer 触发；本次未新增真实项目编译失败→修改→重编译的完整 Observer 开启记录。 |
| Flow 调试请求面板 | 身份筛选与类型构建已验证，正常/未评估节点详情已做浏览器检查；调试开关、远程上下文分页和全部归档版本面板尚未逐项浏览器验收。 |
| 经验进入真实下一任务 | 相关检索、去重、旧源码事实排除有自动测试；真实模型按需采用经验的效果未验证。 |
| 运行中宿主 | 修改在源码和前端构建产物中；本次未重启用户原有宿主服务。 |

本次代码实施已完成，剩余项是以上明确列出的集成和模型质量验收。没有将模拟测试和页面展示当作真实模型效果的证明。
