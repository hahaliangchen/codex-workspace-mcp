# Organizer / Scheduler / Executor / Flow 第七轮修补

日期：2026-10-05。对应 `organizer_flow_review_seventh_2026-10-05.md` 的 F01、F02。

## F01：Observer 建议按请求隔离

观察快照、模型输入、异步建议和观察事件携带 `request_id`，使用当前目标的起始 turn，在同一目标续接和计划修订时保持不变。异步结果使用发起观察时捕获的身份，不读取返回时的新请求身份。

ObserverInbox 保存当前请求身份。替换目标时立即把旧建议标记为 archived 并持久化 inbox 状态；已接受、未读和其他旧建议保留历史 disposition。只有当前请求的建议可进入 pending、deliver、all_unread 和 decisions，旧建议不能被当前 Worker 或 Organizer 回执修改。

建议去重增加请求身份条件，同一 issue_key 或复用同一 node ID 不会跨请求合并。旧请求晚到的结果只更新旧归档或新增归档，不能成为当前待办。新请求正常产生的建议仍会送达。完成旧目标后开始新请求时也执行隔离；旧格式的 inbox 首次恢复时绑定原目标身份。

Organizer 和 Observer 提示明确：旧建议属于历史，替换目标时不要复制为新约束；跨请求经验从相关记忆中按需引用，回顾仍通过原有 memory 路径记录。

回归包括：

- 开启 Observer，经过实际 `run_task` 的脚本模型 HTTP 服务。先等待旧目标真实观察建议产生，保留未解决建议，再替换目标并复用 work/node ID；替换后的 Worker 不携带旧建议，下一次 Organizer 只收到新请求建议，旧建议仍在归档。
- 经过实际 `observe_worker` 的异步竞态：旧观察 HTTP 请求挂起期间替换 watch 快照，然后放行旧响应；两份结果保留各自请求身份，旧结果只归档，新结果可送达。
- Inbox 覆盖接受后续接、替换、旧回执拒绝、同键去重隔离、晚到旧建议、归档恢复及旧格式迁移。

## F02：等待测试任务退出后清理目录

`run_flow_script` 改用 graceful shutdown 并等待模拟 HTTP 服务退出，释放测试状态后再删除临时目录。Observer 开启场景通过测试作用域内的 TaskTracker 等待观察及回顾后台任务结束；观察任务超时停止时也等待 JoinHandle 退出。相关事件写入和数据库操作仍通过原有 await 路径完成。

目录删除最多尝试 10 次，仅 Windows 错误码 32/33（共享/锁冲突）有界重试，其他错误立即返回。业务断言仍保留，清理失败仍使测试失败。

新增 Windows 实际文件锁测试：用 share_mode(0) 锁住临时文件，延迟释放后删除成功；不存在路径的错误继续向上传递。未终止无关进程。

## 验证

| 检查 | 结果 |
| --- | --- |
| `cargo test -- --nocapture`，默认并行 | 135 项通过，0 失败；包含开启 Observer 的替换、晚到结果和 Windows 文件锁回归 |
| `cargo check --quiet` | 通过；仍有既有未使用代码 warning |
| `git diff --check -- src/agent_service.rs` | 通过 |

模拟 Agent 和 Observer 使用本地脚本模型服务，未使用真实模型验收，未重启现有服务。本轮修改不涉及前端代码。
