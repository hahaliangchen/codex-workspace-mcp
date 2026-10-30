# Observer / Organizer R01 独立复查

日期：2026-10-06。范围：上轮 A → B → A 意见回转遗漏，以及本次修补涉及的去重、历史顺序和恢复边界。

## 结论

R01 已修复。本轮源码检查和现有定向/全量回归未发现此修补范围内的新阻塞项。此前的变化意见来源绑定与最新事实版本选择回归继续通过。

## 核查内容

- `src/worker_work_state.rs:585`：同请求、实例和问题的替代链只比较最新来源建议的内容，不再用任意归档版本压掉新意见。A → A 不增加回执；A → B → A 创建第三版 unread，supersedes 指向 B，新来源独立保留。
- `src/worker_work_state.rs:593`：先核对已接收的 review/source 身份，再检查事件顺序；重复或较早来源不反向覆盖最新判断。
- `src/worker_work_state.rs:599`：相同提醒即使不生成新 advice，也保存 observations。恢复后仍能拒绝该来源重放，防止遗漏去重水位。
- `src/worker_work_state.rs:476`：有双方事件 seq 时按 seq 比较；缺少 seq 时按 turn、step、stage 比较，覆盖跨 turn 的 step 重置。
- `src/observer_service.rs:380`：实际建议读取路径透传 source_event_seq 和 turn，不仅是单元测试手工构造身份。
- 显式重新采用的副本不参与原实例的来源替代链；迟到原实例建议不会归档当前采用副本。

## 回归覆盖与结果

`returning_to_historical_advice_creates_a_new_unread_version` 覆盖 accepted/resolved/declined 后的 A → B → A，检查第三版送达、unread、独立来源、supersedes B 和历史处理保留。

`replay_and_out_of_order_advice_cannot_roll_back_the_latest_judgment_after_restore` 覆盖有/无 seq、相同提醒、review/source 重放、不同 ID 的旧事件、跨 turn、同复盘的其他问题以及旧快照兼容。

既有实际会话恢复、重新采用、最新事实写入/检索与 Observer 开启的 Agent 入口回归继续通过。前端重放保留 A/B/A 三份独立复盘和组织者回应，不改变执行节点及高亮。

| 本次独立运行 | 结果 |
| --- | --- |
| `cargo test -- --nocapture`，默认并行 | 152 passed，0 failed，14.43 秒 |
| 前端 `node --test tests/*.test.mjs` | 7 passed，0 failed |
| 前端 `npm run build` | TypeScript 与 Vite 构建通过 |

## 边界

旧快照若从未保存某来源的身份和可比较顺序，程序不能还原不存在的历史记录；本次兼容逻辑利用已存在的 advice 来源和 observations。

本结论针对修补正确性与自动回归。真实模型建议质量、实际长任务能否缩短路径，以及浏览器中的完整交互仍需另行验收。本轮未修改执行代码，未新增临时测试，未重启服务。
