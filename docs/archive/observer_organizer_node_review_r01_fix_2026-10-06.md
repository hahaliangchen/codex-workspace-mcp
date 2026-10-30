# Observer / Organizer：A → B → A 遗漏修补

日期：2026-10-06。对应 [修补复查 R01](/D:/enterpriseProject/codex-workspace-mcp/docs/observer_organizer_node_review_fix_recheck_2026-10-06.md)。

## 确认与修补

问题存在。先加入实际 Inbox 回归，修补前第三次 A 的 deliver 数量为 0，预期 1 的断言失败。

[ObserverInbox.insert](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:585) 原先对所有历史版本做内容匹配，现在只对照同请求、同实例、同问题的最新来源版本。新来源的 A → B → A 创建第三版 unread，保留第三次 review/source，supersedes 指向 B。历史正文、处理状态、理由、application 和 decision_id 保持原值。A → A 仍不重复送达。

Inbox 持久化轻量 observations，记录没有生成新 advice 的相同提醒。对同一问题先检查 review_id/source_event_id 是否已经接收，再比较来源顺序。优先使用 source_event_seq；缺失时使用 turn、step、stage，避免新一轮的 step 重置导致误丢弃。快照恢复后继续拒绝重放和较早来源；缺失 observations 的旧快照利用已有 advice 来源兼容恢复。若旧记录同时缺少可比较的顺序和来源身份，则不能推断其从未保存过的历史。

[ObserverSession.reviews](/D:/enterpriseProject/codex-workspace-mcp/src/observer_service.rs:380) 现在透传 source_event_seq 和 turn。去重以问题为范围，同一 review 的其他问题仍可送达。显式采纳到新计划的副本不参与原实例的来源替代链，迟到的原实例意见不会归档当前已采纳的决策。

## 回归与验证

- [状态回转回归](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:746)：accepted/resolved/declined 后 A → B → A，检查第三次送达、unread、新来源、supersedes B 和旧处理保留。
- [恢复与重放回归](/D:/enterpriseProject/codex-workspace-mcp/src/worker_work_state.rs:775)：有/无事件序号、相同提醒、旧 review/source 重放、不同 ID 的较早事件、跨 turn 的 step 重置、其他问题、旧快照兼容。
- 既有新交付回归使用独立 reminder/handoff 来源；恢复会话集成回归检查实际透传的 seq/turn；显式重新采纳回归检查迟到原实例意见不归档当前采纳副本。
- [前端事件回放](/D:/enterpriseProject/codex-workspace-mcp/frontend/tests/node-reviews.test.mjs:57)：A/B/A 三份复盘保留独立来源与 Organizer 回应，执行节点和高亮保持一致。

| 检查 | 结果 |
| --- | --- |
| 修补前 A → B → A 定向回归 | 断言失败，确认 R01 |
| `cargo test observer -- --nocapture` | 22 passed，0 failed |
| `cargo test -- --nocapture` | 152 passed，0 failed，14.67 秒 |
| `node --test tests/*.test.mjs` | 7 passed，0 failed |
| `npm run build` | TypeScript / Vite 通过，705 modules，4.08 秒 |

以上为自动回归与构建验证；没有重启现有服务或进行真实模型验收。
