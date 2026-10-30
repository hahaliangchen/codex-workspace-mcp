# Observer / Organizer 修补复查

日期：2026-10-06。对应上次检查报告 F01、F02，独立检查当前源码和回归。

## 结论

- F02 最新事实绑定新旧哈希导致检索消失：已修复，实际读取、保存、list/search 回归通过。
- F01 新交付建议继承旧处理状态：原先复现的 A → B 路径已修复，但建议去重仍有 A → B → A 的遗漏，不能判为完全闭环。

## 已修复的主要路径

### F01：变化建议有独立版本与来源

`src/worker_work_state.rs:566` 已比较规范化的正文、建议、判断依据和目标。变化时创建 unread 新项，绑定新 review/source/stage，旧项归档且保留原处理和 application，使用 supersedes 关系关联。

accepted/resolved/declined 后出现新交付问题的新增回归通过。相同提醒不会仅因阶段切换就再要求回执。默认 issue_key 改为调整内容与目标的哈希，避免位置键碰撞；前端事件重放检查新旧处理挂到不同复盘。

### F02：事实绑定明确的读取版本

`src/agent_service.rs:961–1014` 为已观察读取附加 seq、事件 ID、任务与 hash；来源按规范化精确路径匹配，普通引用选有可靠事件顺序的最新读取，显式 source_reads 可指定版本。

不同事实分别保存，避免把旧事实和新事实的清单混合。顺序不明确、未观察来源、选择器冲突等降为 uncertain。原有 hash 有效性检查没有取消。

`observer_latest_fact_after_source_edit_uses_one_version_and_remains_searchable` 经实际 Workspace.read_file、Agent 事件轨迹和 save_observer_lessons 验证：旧版留在 list，search 只返回新版，重复保存去重，多文件依赖变化后排除过期事实。

## R01 / P2：历史内容匹配会吞掉新的状态回转

位置：`src/worker_work_state.rs:570–578`，关键提前返回在 576 行。

matching 包含该请求、实例、问题的所有历史版本。只要新内容与任何历史版本相同，就提前 return。这里没有限制为该问题最新、未被替代的版本。

因此，意见随新事实从 A 变成 B，再回到 A 时，会命中已归档的第一次 A，第三份复盘无法登记。最新有效意见仍是 B，组织者看不到新的回转判断。历史曾出现过某个结论，不等于它与当前最新结论重复。

### 独立复现

临时测试 `audit_observer_changed_advice_returning_to_historical_content_is_not_swallowed` 调用实际 Inbox：

1. 同一实例、同一 route 问题，review_1：A，“输入够用，开始实施”；送达并 resolved。
2. review_2：B，“发现来源映射缺口，先查缺口”；创建第二版并归档第一版，送达并 resolved。
3. 缺口处理后，review_3：A，“输入够用，开始实施”；有新的事件 ID、step 和复盘 ID。
4. insert 提前返回。counter 仍为 2，review_3 没有保存，unhandled 为 0。

预期新意见进入正常组织决策的断言失败。临时测试没有调用真实模型，也没有新增 Worker 回执轮。

### 修复要求

- 内容去重对照该问题最新的有效版本，不能用任意归档版本压掉新变化。
- A → A 保持不重复通知；A → B → A 必须登记第三版，新的来源不应继续挂在第一版。
- 通过 review/source 身份和事件顺序区分旧事件重放与真正的新判断，防止旧结果回放覆盖当前版本。
- 保留历史处理、版本链和来源关联；新增回归检查第三版 unread、supersedes 指向 B，以及组织者可见。

## 本次独立验证

| 检查 | 结果 |
| --- | --- |
| 原有 `cargo test -- --nocapture`，默认并行 | 150 passed，0 failed，14.42 秒 |
| 前端 `node --test tests/*.test.mjs` | 6 passed，0 failed |
| 前端 `npm run build` | TypeScript 与 Vite 通过，705 modules，3.78 秒 |
| 临时 A → B → A 定向测试 | 1 failed，复现 R01；非编译失败 |

临时测试已经撤下，worker_work_state.rs 的 SHA256 恢复为复查前值，没有修改执行代码。未重启现有服务，未进行真实模型或浏览器手工验收。
