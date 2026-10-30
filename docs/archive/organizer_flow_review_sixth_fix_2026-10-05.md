# Organizer / Scheduler / Executor / Flow 第六轮边界修补

日期：2026-10-05。对应 `organizer_flow_review_sixth_2026-10-05.md` 的新请求切换边界。

## 行为修正

Organizer 决策新增 `request_action`：

- `continue`：保留当前用户目标和已完成交付；`action=work` 可以替换当前工作包，但不替换根目标。
- `subtask`：在当前目标下派工作，并保留原未完成工作包，供后续恢复。
- `replace`：取消旧用户目标，归档旧执行，按最新用户请求建立新有效计划。只允许在新消息的 `session_continuation` 交接中搭配 `action=work` 使用。

新消息接续一个未完成目标时，宿主要求 Organizer 明确给出 `request_action`，避免把新复杂目标默默挂到旧根节点。即使最后一个已派发工作包已经完成，但整项目标仍未结束，也会产生请求级交接；继续时不废弃该包的交付或待执行队列。

`replace` 在同一决策副本中完成旧请求归档、Scheduler 重建和新树建立。简单回答直接使用无树的 output/final_answer 协议，不需要创建旧目标下的子节点；新复杂任务建立自己的根目标。无效决策不会修改原请求，`resume_tree` 也不能把被取消的旧树带回有效计划。

旧树、Scheduler 交付和 Worker 工作记录保存在 `archived_requests`，与当前 frames、队列及依赖解析隔离。新请求可复用旧 work ID。归档列表保持平坦，并在后续正常完成和新消息恢复时保留；不会生成层层嵌套的旧快照。

新 Worker 使用新的目标边界、工作记录和源材料选择；消息窗口、读取缓存同步重置。Notebook 仍保存历史材料，但复用 node ID 时只自动提供本次请求的材料位置；显式选取的文件或材料可以按需要再次使用。

Flow 将归档节点和边使用独立命名空间标为 deprecated。`flow/request_archived` 同时把旧请求所在轮次的节点及关联边标灰；此前已正常结束的其他请求不受影响。当前新节点不会因复用 work ID 被旧灰色实例覆盖。

## 回归与验证

- 实际 `run_task` 入口：未完成复杂目标 → 简单技术问题，不传额外 flow_update；新答案完成，旧根目标、待执行包及旧结论只存在于归档中。
- 实际 `run_task` 入口：未完成复杂目标 → 新复杂目标；复用旧 work ID，Worker 收到新根目标，汇总成功，后续 Organizer 上下文不再提供被取消的树。
- 决策级覆盖：替换仅发生于新消息交接、遗漏请求级选择被拒绝、失败替换保持两份原状态不变、子任务保留原目标、禁止自动恢复被取消树。
- Notebook 覆盖：同一 node ID 在新请求中不会自动引入旧材料；明确选择的文件仍可使用。
- 前端事件归并覆盖：旧请求标灰、其他已完成请求保留、复用 ID 的新节点保持有效、归档根不成为新目标父节点或阻止新目标完成。

| 验证 | 结果 |
| --- | --- |
| `cargo check --quiet` | 通过，仍有未使用代码相关 warning |
| `cargo test --quiet -- --nocapture` | 130 项通过 |
| `node --test tests/request-replacement.test.mjs`（frontend） | 2 项通过 |
| `npm run build`（frontend） | 通过，Vite 构建 3.69 秒 |

Agent 回归使用本地脚本模型 HTTP 服务。未使用真实模型或浏览器验收产品交互，未重启现有服务。
