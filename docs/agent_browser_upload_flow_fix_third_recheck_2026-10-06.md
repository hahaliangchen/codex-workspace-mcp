# 页面上传与流程修复第三次复查

日期：2026-10-06。检查第二次复查后的最新实现，运行现有相关测试，并直接执行源码中的页码解析函数。未修改生产代码，未重启服务，未派发真实 Agent 任务。

## 结论

上次指出的旧文稿误判与上传事实易丢失，已分别加入错误/上传尝试关联和独立上传交付记录。但新加载判定不兼容目标编辑器的真实页码格式，正常加载会被阻塞；内容变化的判断也会错误拒绝同页数文稿。需要修正这两处再做真实任务验收。

## 已落实的改动

- BrowserUploadEvidence 和 active_browser_uploads 保存上传事实，不依赖最近 16 条 operations；browser_current_read 也独立保留。
- 上传节点通过宿主附加 exported_data.browser_upload_receipt，下游可在 dependency_inputs 声明并消费它。宿主核对原执行实例、修订、交付记录和活动上传 ID；导航、关闭或替换上传会失效。
- browser_read/browser_wait 识别明确的打开/解析错误，不再将“打开失败”当成有效页码。
- 上传工具设置 upload_attempt_id，观察 change、busy 和完成状态；requires_pptx 核对同一次应用加载尝试，通用 DOM 推断不能替代新宿主上传的加载确认。
- 原有失败标记、成功后保留可见窗口、Windows 文件路径转换仍然保留。

## 待修复问题

### P1：有效页码格式与目标编辑器不一致，正常打开也无法完成

位置：src/browser_control.rs:152、176、321；src/work_scheduler.rs:191。

浏览器读取/等待和上传 MutationObserver 的页码解析均要求整段文字为纯数字 `1 / 2` 或 `1 of 2`。目标项目 src/demo-entry.ts:331 实际设置的是 `Slide 1 / 2`。Rust 的页码回退解析也直接把 `/` 左侧解析为数字，不接受 Slide 前缀。

直接提取 src/browser_control.rs 中的 parsePage，使用 Node 执行，结果：

| 输入 | 实际解析结果 |
| --- | --- |
| 1 / 2 | current=1, total=2 |
| Slide 1 / 2 | null |
| 打开失败 | null |

因此，真实编辑器即使解析成功，upload_attempt 仍不能变为 loaded，browser_wait(document_loaded=true) 可能超时，requires_pptx 不能完成。只修改 read 的正则也不够，上传观察器内部和 wait 有重复实现。

建议：统一浏览器端页码解析，支持目标页面真实格式（例如可选 Slide 前缀）并保持对失败文字、零页、越界页数的拒绝；优先使用结构化当前页/总页数。Rust 回退采用一致规则。加入目标格式回归，不能只使用简化的 `1 / 1` 夹具。

### P2：以列表文本变化确认新文稿，会阻塞同页数文稿和重开同一文件

位置：src/browser_control.rs:321。

上传观察器的 signature 只包含每个 slide-item 的 innerText/textContent，以及 #page-indicator 文字。完成时要求 signature 与上传前不同，否则状态为 unconfirmed，并立即 disconnect。

目标项目 src/demo-entry.ts:358–377 的 slide-item 内部是图片缩略图，没有正文文字；页码和幻灯片名在 aria-label/img.alt 中，也没有进入 signature。两份不同的 N 页文稿，均停留第一页时，signature 可以完全相同；重新打开同一份文稿更会相同。因此即使 busy 完整经历且成功加载，新代码仍拒绝确认，并停止继续观察。

建议：使用本次上传关联的明确加载完成事件/文稿身份或应用状态，不能把“内容必须与上一份不同”作为成功条件。相同文稿、相同页数都应允许成功。若只能观察 DOM，需要可靠的刷新代次/节点替换事实并明确其局限，不能只比较空文本和页码。

## 测试结果和遗漏

| 测试组 | Cargo 报告通过数 |
| --- | ---: |
| browser_control::tests | 8 |
| work_scheduler::tests | 17 |
| agent_service::tests | 37 |
| flow_tree::tests | 3 |
| 合计 | 65 |

均通过。live_editor_demo_shows_a_slide 未设置 PPTX_URL，提前返回；真实 PPTX 加载/字体成功/画面检查尚未验证。

当前新增生产逻辑没有相应的新回归覆盖：跨上传/验证节点、超过 16 条操作仍复用上传、失败后保留旧文稿、同页数不同文稿、重开同文件及实际 `Slide 1 / N` 页码。已有调度器上传测试构造的结果不含 upload_attempt_id，因此走 legacy-upload 分支，load_attempt_required=false，不能证明最新的严格关联逻辑有效。

下一轮应先修页码和完成事件关联，再补上述测试，最后让实际 Agent 完成真实 PPT 上传任务；单纯已有测试全绿不足以覆盖这次加载协议变化。
