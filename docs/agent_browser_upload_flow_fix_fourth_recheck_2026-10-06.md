# 页面上传与流程修复第四次复查

日期：2026-10-06。检查第三次复查的两个问题，运行现有回归测试及提取源码的 JavaScript 模拟检查。未修改生产代码，未重启服务，未派发新的 Agent 任务。

## 结论

上轮页码格式不兼容、同页数/重开同文件被拒绝的问题，代码已修复，针对性模拟检查通过。本轮在所检查的修复范围内未发现新的明确阻塞缺陷。真实 Agent 上传 PPT 的端到端验收仍待进行，不能把模拟结果当成真实解析/字体/画面验证通过。

## 修复情况

- browser_control.rs:152、176、331：read、wait 和上传观察器均支持可选 Slide 前缀；work_scheduler.rs:191 的 Rust 回退也支持。`Slide 1 / 2` 不再被拒绝。
- browser_control.rs:321–370：移除了上传前后缩略图文本 signature 必须变化的条件。上传观察器按本次 change、观察到的 busy 周期、有效页码/幻灯片及无明确错误确认完成，因此无需新文稿文本与旧文稿不同。
- 原有独立 BrowserUploadEvidence、跨节点 browser_upload_receipt 和活动上传 ID 校验保留；上传事实不依赖最近 16 条 operations。
- 失败标记、完成后保留可见窗口、Windows 路径兼容仍保留。

## 针对性验证

直接提取 src/browser_control.rs 的 parsePage 和 upload_observer，在 Node 中执行。上传观察器使用模拟 DOM、文件 change 和 MutationObserver 回调，验证的是实际源码中的函数分支，不是运行真实浏览器或解析 PPT。

| 检查 | 结果 |
| --- | --- |
| 1 / 2 | current=1, total=2 |
| Slide 1 / 2 | current=1, total=2 |
| Slide 2 of 2 | current=2, total=2 |
| 打开失败 | 拒绝解析 |
| Slide 0 / 2、Slide 3 / 2 | 拒绝解析 |
| 同页数、相同页码的完成周期 | upload attempt 为 loaded |
| 重开同文件的完成周期 | upload attempt 为 loaded |
| 旧幻灯片存在，但页码为打开失败 | upload attempt 为 failed |

## 现有回归测试

| 测试组 | Cargo 报告通过数 |
| --- | ---: |
| browser_control::tests | 8 |
| work_scheduler::tests | 17 |
| agent_service::tests | 37 |
| flow_tree::tests | 3 |
| 合计 | 65 |

全部报告通过。live_editor_demo_shows_a_slide 未设置 PPTX_URL，提前返回，仍计入通过。现有可见窗口保留测试使用无头会话模拟 visible 标志；真实窗口交付未在本轮验证。

## 仍需完成的验收

1. 在真实 Agent 中上传目标项目的真实 PPTX，确认正常解析、实际页码、字体状态和截图，成功后展示窗口保持打开。
2. 增加长期保留的严格协议回归：带真实 upload_attempt_id 的加载关联、同页数/重开文稿、失败时旧文稿仍在、上传和验证两个 Flow 节点、超过 16 条操作后复用上传。

当前已有调度器 PPTX 测试构造的上传结果没有 upload_attempt_id，仍走 legacy-upload 的兼容分支。因此现有测试全绿并不代表最新严格关联协议的集成场景已全部覆盖。
