# 页面上传与流程修复第二次复查

日期：2026-10-06。对照首次复查遗漏，检查最新代码并运行四组现有回归测试。未修改执行代码，未重启服务，未追加实际 Agent 任务。

## 结论

上次的失败标记、成功结束关闭可见窗口、目标编辑器页码漏读均已修复；真实 PPTX 任务新增了指定文件上传和当前页面加载判定。但是文稿失败状态识别及上传事实的保存/跨节点交付仍存在问题。

## 已修复

- agent_service.rs:529：宿主识别 ok=false 和字符串 failed/timeout/invalid_condition/invalid_selector，结构化浏览器错误可以进入失败记录。
- browser_control.rs:23、agent_service.rs:1673：completed 时保留 visible 会话，新增 browser_close；无头会话仍清理。
- browser_control.rs:150：读取实际编辑器 #page-indicator，提供 document_loaded 与加载指示信息。
- browser_control.rs:174：等待超时返回 ok=false、wait_timeout；没有收到 change 返回 file_assigned_unconfirmed，不再声明上传确认成功。
- browser_control.rs:237、355：上传向 CDP 传普通 Windows 路径，支持去除扩展路径前缀及转换 UNC。
- work_scheduler.rs:887、1168：普通浏览器验收限定当前页面 epoch、最新交互之后的匹配；requires_pptx 要求 browser_document_path，上传路径一致、change 已观察、随后读取到非零幻灯片及页码。
- organizer_system.md:44 与 work_organizer.rs:57 已描述新的 PPTX 工作契约。

## 剩余问题

### P1：打开失败时可能把旧幻灯片判作新文稿加载成功

位置：src/browser_control.rs:150–153、169；src/work_scheduler.rs:903–908。

document_loaded 的条件是 slide_count>0、页码文字非空、没有可见 loading 控件。页码不解析实际数值，不排除“打开失败”，也没有核对当前显示文稿和上传文件的关联。

目标项目 src/demo-entry.ts:433–469 在开始上传时设置 aria-busy，失败时把页码设置为“打开失败”，finally 移除 aria-busy。该异常路径没有清空旧的 slide-list。当页面已经有旧文稿，而下一份文稿读取/解析早期失败时，旧列表可以仍然存在。这时非零旧幻灯片 + 非空“打开失败” + busy 已清除，会被当前 read/wait 算法归类为 loaded。若随后 browser_read 匹配已有标题/文本，指定文件上传成功的记录仍可以让 requires_pptx 通过。

建议：识别明确错误状态；校验有效当前页/总页数，不用“非空文字”代表页码有效。将显示的文稿加载结果关联到本次上传文件和加载尝试，旧文稿不能替新上传作验收。通用 DOM 只能提供推断时，应如实返回 unconfirmed；具体应用使用可观察的加载状态/文稿身份。增加“先加载 A，上传 B 失败，旧 A 仍显示”的回归。该发现来自源码检查，本次没有在真实编辑器触发失败上传。

### P2：上传事实仍放在当前节点的短操作列表，容易丢失并诱发重复上传

位置：src/work_scheduler.rs:896–902、1098、704–728。

browser_presentation_loaded 只在当前 WorkFrame.operations 中寻找上传；该数组最多保留 16 条。上传后较长的字体排查、等待、截图等会挤掉上传条目，随后即使真实文稿已打开，验收也会因“找不到上传”拒绝完成。

若 Flow 将“上传”与“等待/验证”拆为两个节点，上游的上传即使显式导出并通过 dependency_inputs 传给下游，当前判定也不会读取该交付。activate_task 只继承上游文件版本，新的 WorkFrame 没有前序上传操作。下游必须再次上传才可能满足当前硬门槛。这与各节点只消费上游产出、无需重做上游动作的目标冲突。

建议：将已确认上传保存为宿主维护的独立记录，包含文件身份、浏览器会话/页面、加载尝试与时间/代次；操作日志仍可截断。按显式上游交付允许下游消费该记录，不混入整个上游操作历史；导航、关闭或替换文件时使记录失效。回归覆盖上传后超过 16 条操作和上传→验证两个 Flow 节点。

## 测试结果与范围

| 现有测试组 | Cargo 报告通过数 |
| --- | ---: |
| browser_control::tests | 8 |
| work_scheduler::tests | 17 |
| agent_service::tests | 37 |
| flow_tree::tests | 3 |
| 合计 | 65 |

全部报告通过。live_editor_demo_shows_a_slide 未设置 PPTX_URL，提前返回，仍计入通过；真实 PPTX 解析/字体成功/图像验收尚未由这次测试确认。

completed_visible_browser_session_is_retained_until_explicit_close 使用无头浏览器，手动把 session.visible 改为 true，确认了保留/关闭分支和会话所有权；它没有实际启动可见窗口。因此可见窗口真实交付还需要端到端验收。

上传夹具仍是写入普通字节的 .pptx 文件，只验证浏览器文件分配和 change，不验证 PPTX 解析。新增调度器 PPTX 测试使用构造的工具结果，能确认状态门槛，但没有覆盖上述失败后旧列表及跨节点/日志截断场景。
