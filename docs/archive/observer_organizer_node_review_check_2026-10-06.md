# Observer / Organizer 新方案实现检查

日期：2026-10-06。依据：`observer_organizer_node_review_plan_2026-10-05.md`。本报告检查“观察者关注组织者与当前执行节点”的新实现，与第七轮请求隔离修补复查不同。

## 结论

核心执行链已经实现，不是只调整名称和提示词：观察输入包含组织者决策和当前节点，派发/交付记录持久化，组织者处理建议，Worker 默认不增加回执轮，结束汇总不追加模型调用。

但不能判为完整验收通过。本次独立复现两项 P2 问题：变化后的交付建议被旧处理状态吞掉；最新项目事实绑定了同文件的新旧哈希，导致当前记忆检索排除它。

## F01 / P2：同实例的实质新建议继承旧处理状态，组织者收不到

位置：`src/worker_work_state.rs:474,556–570`；`src/observer_service.rs:244`。

`ObserverInbox.insert` 按 request_id、identity 和 issue_key 合并，但不区分建议内容的版本。匹配旧项时仅更新 summary、suggestions、latest_step，保留旧 disposition、review_id、source_event_id、stage 和 application。

组织者正常输入只取 `unhandled()`，它过滤为 disposition=unread。因此，已接受的派发建议在交付阶段出现实质新变化时，会更新正文却仍保持 accepted，既不进入组织者待处理输入，也没有正确关联到新的交付复盘。旧项若已 resolved/declined，新内容甚至不会更新。

不显式提供 issue_key 时，normalize 默认使用 recommendation_0 等位置键，也容易让同节点两个阶段的不同第一条建议碰撞。

### 独立复现

临时测试 `audit_observer_changed_handoff_recommendation_must_reach_organizer` 调用实际 Inbox 和 `respond_for_decision`：

1. 同一实例 w@1 的 assignment_review 建议“复用已封存输入”，组织者通过实际约束字段接受。
2. 实例完成后，handoff_review 使用同一 route issue_key，提出“交付暴露上游问题，考虑先回溯”。
3. 新建议正文写进 advice_1，但 disposition 仍是 accepted，review_id 仍是 assignment_review，stage 仍是 assignment。
4. `unhandled()` 返回空；“新交付建议必须到达组织者”的断言失败。

这不是要求对同样的提醒重复回执，而是保证不同事实带来的新调整能进入下一次正常决策。

### 修复要求

- 对实质未变化的提醒去重，保留处理结果。
- 对新事实导致的建议变化建立新建议版本或新的待处理项，绑定新的 review/source/stage；旧处理记录作为历史保留。
- assignment/progress/handoff 之间不能只凭相同 issue_key 就继承旧回执。相同内容可合并，内容变化需显式区分。
- 增加 accepted、resolved、declined 后出现新交付问题的回归，并检查组织者输入和 Flow 来源关联。
- 不新增 Worker 回执轮，不让 Observer 直接调度任务。

## F02 / P2：同文件的新旧哈希一起绑定，最新事实也被判为过期

位置：`src/agent_service.rs:962,1058–1067`；`src/memory.rs:349–360`。

`save_observer_lessons` 从读取轨迹收集多个 known_read_targets，按 source_refs 的文件路径把所有匹配目标绑定到事实记忆。没有为该事实选择明确的源码版本。同一个文件在修改前后被读取，会同时带入旧 hash 和新 hash。

`observer_sources_current` 要求绑定的每个 hash 都与当前文件匹配。一个文件不能同时匹配两个不同版本，所以刚基于最新源码存下来的事实也被 search_work_memory 排除。

### 独立复现

临时测试 `audit_observer_latest_fact_after_two_source_versions_remains_searchable` 调用实际保存与检索路径：

1. 当前 source.rs 内容为 new source。
2. 读取轨迹按时间包含 source.rs 的 old source 和 new source 两个 hash。
3. 观察者提交 confirmed 事实“latest mapping is present”，来源 source.rs。
4. `save_observer_lessons` 返回 memoryRecorded=true，list_work_memory 可找到记录。
5. search_work_memory 查询 latest mapping 返回零条；“最新事实必须仍可检索”的断言失败。

现有记忆版本测试手工给每条事实绑定单一 hash，未覆盖实际保存函数收集同文件多个版本的情况。

### 修复要求

- 事实来源绑定具体读取目标、事件和源码版本，不能按文件名收集所有历史 hash。
- 若事实明确基于最新读取，绑定该次读取的版本；旧读取和旧事实留历史。
- 若无法判断事实对应哪个版本，保留 uncertain，不直接投影成当前 confirmed 结论。
- 多文件事实可以各自绑定相应版本；不要通过取消 hash 校验掩盖冲突。
- 增加实际 save_observer_lessons → list/search 的回归：读取旧文件、修改、读取新文件、保存新事实，确认新事实可查、旧事实不作为当前结论。

## 已核查的主链路

| 能力 | 本次检查 |
| --- | --- |
| 角色和视野 | observer_system.md 面向 Organizer；observation/pack 发送当前决策、目标、选定输入与浓缩路径，常规预算 12,000 字符 |
| 持久边界 | assignment/handoff 与 execution/commit 使用 SQLite 事务；通知容量 1，仅负责唤醒，事实保存在数据库 |
| 异步和结束 | 单会话模型许可；限时请求；finish 取消观察并标记未评估事项，汇总已有记录，无新增完整历史模型复盘 |
| Worker 独立 | 默认 Worker 提示词和报告 schema 移除建议回执要求；可选历史咨询保留 |
| 组织者处理 | 同一次正常决策返回建议处理与实际字段应用；仍需修复 F01 的新建议送达问题 |
| 身份隔离 | 请求、实例、revision、plan_revision 绑定；旧版本归档、显式重新采用与迟到来源通知已有定向回归 |
| Flow / 调试 | 按实例关联复盘和处理记录；已有事件重放通过。本次未手工操作浏览器确认全部面板 |
| 记忆 | 事实与经验分开记录、有去重和源码有效性校验；仍需修复 F02 的实际写入版本选择 |

## 本次独立验证

| 验证 | 结果 |
| --- | --- |
| 原有 `cargo test -- --nocapture`，默认并行 | 147 passed，0 failed，14.37 秒 |
| 前端 `node --test tests/*.test.mjs` | 5 passed，0 failed |
| 前端 `npm run build` | TypeScript 与 Vite 通过，705 modules，Vite 4.01 秒 |
| 临时 `cargo test audit_observer_ -- --nocapture` | 2 failed，分别确认 F01、F02；非编译失败 |

临时复现测试已经撤下，两个源文件的 SHA256 均恢复为复现前值，没有留下执行代码修改或失败测试。复現只使用临时工作区和实际函数，未调用真实模型。原有测试通过与新增边界复现失败是两组不同结果，不能合并宣称全面通过。

本次未重启现有服务。真实模型的建议质量、是否缩短工作路径和长期调用成本，仍需修复上述问题后用实际任务验收。
