# Observer / Organizer 检查问题修补

日期：2026-10-06。对应 [独立检查报告](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_organizer_node_review_check_2026-10-06.md) 的 F01、F02。

后续复查确认 F01 仍有 A → B → A 遗漏；已进一步修补，见 [R01 修补与验证](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_organizer_node_review_r01_fix_2026-10-06.md)。最新全量结果为后端 152 项、前端 7 项通过。

两个 P2 的根因与当前实现一致，已修补并保留回归。原来的 147 项测试通过只能证明原有覆盖范围，不能否定这两个新增边界。

## F01：变化的交付建议被旧处理状态吞掉

修补：[ObserverInbox.insert](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:566)、[建议规范化](/D:/enterpriseProject/codex-workspace-mcp/src/observer_service.rs:226)。

- 同请求、同实例、同问题的建议先比较规范化正文、调整内容、判断依据和目标。仅换 review/source/stage 的相同提醒保持原处理，不产生回执要求。
- 实质内容变化时创建新 advice，初始状态 unread，绑定新的 review_id/source_event_id/stage。supersedes_advice_id / superseded_by_advice_id 连接两个版本。
- 旧建议归档，原正文、disposition、application、decision_id 和来源不被覆盖。新项进入正常的 deliver → unhandled → Organizer 输入路径。
- 已 resolved/declined 的有来源记录不再被 start_turn 删除；同样覆盖它们之后出现的新问题。
- 默认 issue_key 改为调整内容和目标的稳定哈希，避免仅凭 recommendation_0 等位置碰撞。
- 已被新建议替代的旧交付意见不会再次作为迟到交付通知进入当前计划。

回归：[changed_handoff_advice_after_handling_reaches_organizer_with_new_provenance](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:684) 分别覆盖 accepted、resolved、declined：先由实际 respond_for_decision 处理派发意见，再送入相同提醒和变化的交付问题。断言相同提醒不重新送达，新问题进入 unhandled，旧处理保持完整，新项不继承 application，持久状态恢复后仍待处理。

[前端事件重放](/D:/enterpriseProject/codex-workspace-mcp/frontend/tests/node-reviews.test.mjs:33) 验证旧响应仍挂在 assignment review，新响应只挂在 handoff review，source_event_id 正确，并且不会新增执行节点或改变高亮。

## F02：最新事实绑定同文件新旧哈希后无法检索

修补：[读取来源选择](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:961)、[save_observer_lessons](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:1017)、[Observer 契约](/D:/enterpriseProject/codex-workspace-mcp/prompts/observer_system.md)。

- known_read_targets 包含原始读取事件 seq、事件引用、任务和 code_hash，并按事件顺序排序；不依赖传入数组的先后位置。
- source_reads 可以精确选择读取目标、事件和源码版本。普通文件 source_refs 明确定义为该文件最新的已观察读取；若多个版本没有可靠顺序、选择器冲突或引用不存在的读取，则降为 uncertain。
- 来源按规范化后的精确文件路径匹配，避免按文件名或路径子串收集所有历史 hash。多文件事实分别选择每个文件的一次读取；有未观察来源时不确认整个事实。
- 不同项目事实分别存储。即使同一报告包含基于旧版和新版的两个事实，也不再把它们合成一个永远无法匹配的来源清单。
- 事实正文保留源码版本，来源清单保留具体读取事件。相同事实与源码版本继续去重；旧版事实留在 list 历史。
- 原有 observer_sources_current 的全部 hash 校验保持。当前 search 只返回与当前文件一致的事实，没有通过放宽校验掩盖冲突。

回归：[observer_latest_fact_after_source_edit_uses_one_version_and_remains_searchable](/D:/enterpriseProject/codex-workspace-mcp/src/agent_service.rs:5231) 使用实际 Workspace.read_file、Agent 事件读取、save_observer_lessons 和 list/search：

1. 读取旧 source.rs 并保存旧事实。
2. 修改文件，读取新版本，保存最新事实。
3. list 保留两版，search 只返回最新事实；新清单仅含一个新 hash 及对应事件。
4. 重复保存不增记录；同报告的显式旧版事实和新版事实分别绑定，不混合。
5. 多文件事实分别绑定两份来源，其中任意文件改变后，该事实从当前搜索排除。
6. 无读取顺序、未知 hash、混入未观察的选择器、同文件冲突版本和部分未读取来源，都保持 uncertain，不存为 confirmed 记忆。

## 验证

| 检查 | 结果 |
| --- | --- |
| `cargo test observer` | 20 passed，0 failed；包含新增的建议变化、默认键和实际事实保存边界 |
| 全量 `cargo test` | 150 passed，0 failed，14.68 秒 |
| 最后补充未读取来源断言后的事实保存定向测试 | 通过；该修改仅加强事实确认条件 |
| `node --test tests/*.test.mjs` | 6 passed，0 failed |
| 前端 `npm run build` | TypeScript 与 Vite 通过，705 modules，3.85 秒 |
| 定向 `git diff --check` | 通过 |

本次没有增加 Worker 回执轮或 Observer 调度动作。没有调用真实模型，也没有重启现有宿主。本次 Flow 来源验证为事件重放，未新增浏览器手工验收；真实模型建议质量及长期调用成本仍未验证。
