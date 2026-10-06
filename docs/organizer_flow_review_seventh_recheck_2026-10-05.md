# 第七轮修补复查

日期：2026-10-05。范围：独立复查第七轮报告 F01、F02 的实现与回归覆盖。

## 结论

第七轮的两项问题均已修补，本次源码检查和回归未发现这两项仍未闭环的情况。此结论不代表新的 Observer / Organizer 节点审查计划已经完成。

## F01：旧 Observer 建议污染新请求

状态：已修复，并有开启 Observer 的实际 Agent 入口模拟验证。

- `src/worker_work_state.rs:463`：Inbox 保存当前 request_id，替换请求时归档旧建议；pending、deliver、all_unread、decisions 过滤请求身份和归档状态，旧回执被拒绝。
- `src/worker_work_state.rs:485`：建议合并增加请求身份条件。同一 issue_key、复用同一节点 ID 的两个请求不会混为一条建议。
- `src/agent_service.rs:904`：异步审查捕获不可变快照，模型输入、建议队列和观察事件均使用发起审查时的 request_id；返回时不会将旧结果改标为新请求。
- `src/agent_service.rs:3011`、`:3086`：恢复会话和提交请求替换时更新 Inbox 请求身份，持久化归档状态。
- 回归覆盖：未解决旧建议、新请求建议正常送达、接受后续接、旧回执拒绝、晚到旧结果、持久化恢复、旧格式迁移。

关键测试包括 `agent_flow_replacement_archives_unresolved_observer_advice_and_uses_new_request_advice` 和 `observer_inflight_review_keeps_original_request_identity_after_snapshot_replacement`。前者经过实际 run_task，并检查替换后 Organizer / Worker 输入；后者挂起旧 HTTP 审查，在快照替换后放行响应，确认旧结果仍归档。

## F02：Windows 测试目录删除时仍有文件占用

状态：已修复，本次默认并行运行未复现失败。

- `src/agent_service.rs:973`：观察任务超时取消后等待 JoinHandle 退出。
- `src/agent_service.rs:5218`：测试通过 TaskTracker 等待后台观察 / 回顾任务结束。
- `src/agent_service.rs:5239`：模拟 HTTP 服务 graceful shutdown 并等待退出，释放测试状态后清理目录。
- `src/agent_service.rs:5108`：删除目录仅对 Windows 32 / 33 共享或锁冲突有界重试，最多 10 次；其他错误立即返回，最终失败仍导致测试失败。
- 新增实际 Windows 文件锁回归，检查延迟释放锁后删除成功，以及不存在目录错误仍向上传递。

## 本次独立验证

| 命令 | 结果 |
| --- | --- |
| `cargo test agent_flow_ -- --nocapture` | 默认并行，7 / 7 通过 |
| `cargo test observer_request_tests -- --nocapture` | 2 / 2 通过 |
| `cargo test -- --nocapture` | 默认并行，135 / 135 通过，含异步审查竞态回归 |
| 再次 `cargo test agent_flow_ -- --nocapture` | 默认并行，7 / 7 通过 |

仍有 17 条未使用代码等编译 warning。回归包含本地脚本模型模拟，不等同于真实模型长任务验收；本次没有手工核对 Agent 浏览器画面，没有重启服务，没有修改执行代码。重复通过不能证明未来绝无文件占用，但原问题已有资源退出等待和有界错误处理。

Observer 改为审查 Organizer 方略、当前节点路径的角色调整，仍应按 `observer_organizer_node_review_plan_2026-10-05.md` 单独实现和验收。
