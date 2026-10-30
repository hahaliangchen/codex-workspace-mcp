# Organizer / Scheduler / Executor / Flow 第七轮实现检查

日期：2026-10-05。重点检查第六轮报告的目标切换边界，并重跑定向回归。

## 已修复

- Organizer 的 schema、提示词和程序校验均增加 request_action=continue/subtask/replace，明确用户目标生命周期。
- 未完成目标切换为简单问题时，replace 创建新 Scheduler 和树，普通 direct/output/final_answer 可以执行，无需额外 flow_update。
- 切换为新复杂目标时，新根目标使用当前用户需求，不再沿用取消目标的边界。
- 旧调度状态、树、Worker 状态归档，退出新依赖图；旧实例和路径以灰色历史保留，新目标允许重复使用原 work ID。
- 替换时清理 Worker 状态、源码工作集、读取覆盖和局部消息；实际 Agent 入口的模拟测试检查了新请求不携带旧目标事实。
- 新消息发生在子任务已完成但整体未结束时，也进入请求级生命周期决策。

第六轮的切换问题已修复，当前核心闭环和这两种替换路径通过定向模拟验证。

## 剩余问题

### F01 / P2：Observer 建议没有随请求替换隔离

位置：`src/agent_service.rs:2904–2925,3018–3041,3052–3063`；`src/worker_work_state.rs:466–513`。

replace 清理 WorkState、SourceWorkingSet 和局部历史，但没有归档或筛选 observer_inbox，也没有给正在产生的观察结果加请求版本门禁。

ObserverInbox.start_turn 只删除 resolved/declined 项。pending 返回未解决和已接受建议，不按请求身份筛选。下一轮 Organizer 输入仍使用 observer_inbox.pending；insert 也只按 issue_key 或 category/node_id 合并，没有请求身份。

因此旧拖拽目标中的建议（例如继续查来源 ID、停止调查后立即编辑）在替换为新导出任务后，仍会作为当前待处理建议进入组织者上下文。这个事实来自输入构造路径审查；本轮没有执行开启 Observer 的替换回归，也不声称已观测到真实模型因此做错。

当前 run_flow_script 使用的 flow_test_state 配置 observer_enabled=false，所以新增的“无旧上下文”测试不能覆盖此残留。

要求：建议与观察快照携带请求身份/版本。替换时将旧目标建议归档，仅当前请求建议进入 pending；旧请求晚到结果只归档。跨请求可复用的经验另存为记忆，不能自动作为当前待办。增加 Observer 开启且有未解决旧建议的目标替换回归。

### F02 / P3：Windows 临时目录清理存在偶发文件占用失败

位置：`src/agent_service.rs:5075–5076`。

首次默认并行运行 cargo test agent_flow_ 时，4 项通过，1 项在 run_flow_script 的 remove_dir_all(root).unwrap 失败，Windows 错误码 32（文件正在使用）。失败发生在辅助函数清理阶段，不是报告的业务断言失败。

使用 --test-threads=1 重跑，5 项全部通过。串行重跑通过并不证明文件句柄生命周期问题已修复；也不能确定占用一定来自测试并行，仍需核查异步数据库操作、服务任务等句柄的结束时机。

要求：等待测试创建的异步任务和相关数据库操作结束，再清理临时目录；必要时对 Windows 文件占用使用有界重试，保留其他清理错误。不要忽略业务断言，也不要终止无关进程。

## 本次验证

| 定向检查 | 结果 |
| --- | --- |
| cargo test agent_flow_ -- --nocapture | 首次 4 通过、1 清理失败 |
| cargo test agent_flow_ -- --nocapture --test-threads=1 | 5 通过 |
| cargo test work_scheduler::tests:: -- --nocapture | 10 通过 |
| cargo test request_lifecycle_replacement -- --nocapture | 1 通过 |
| cargo test agent_decision_keeps_split -- --nocapture | 1 通过 |
| cargo test direct_request_needs_explicit -- --nocapture | 1 通过 |
| cargo test legacy_execution_snapshot -- --nocapture | 1 通过 |
| 前端 npm run build（tsc --noEmit && vite build） | 通过，Vite 构建 3.60 秒 |

重跑后合计 19 项定向测试通过。模拟 Agent 场景经过实际 run_task；没有使用真实模型、开启 Observer 验证目标替换，或核对实际浏览器画面。

本轮未修改执行代码、未运行全部测试、未重启服务。
