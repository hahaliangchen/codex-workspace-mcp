# 页面上传与流程修复复查

日期：2026-10-06。对照 `agent_browser_upload_flow_fix_plan_2026-10-06.md`，进行源码检查及现有回归测试。此次未修改执行代码，未重启服务，未向实际 Agent 追加任务。

## 结论

上传跨 CDP 连接使用 nodeId 的根因已修复；组织者状态和依赖引用的反馈也已改善。但失败标记、可见窗口交付和文稿加载完成判定还有遗漏，不能认定完整场景已经通过。

## 已落实

- 上传中的 DOM.getDocument、DOM.querySelectorAll、DOM.describeNode、DOM.resolveNode、DOM.setFileInputFiles 和确认文件状态使用同一个 CdpConnection，命令编号递增。
- 检查文件存在、输入框类型、多输入框歧义，提供候选；失效节点最多两次尝试。
- 返回实际文件名、files.length、change 事件记录，明确区分 file_assigned 与 document_loaded:not_verified。
- browser_read 增加控件、文件输入、幻灯片计数；新增 browser_wait 和 browser_diagnostics。
- 新增 browser_open.visible，首次创建时选择可见窗口或无头模式，返回 display_mode。
- Organizer 可看到 work/node/revision/status、允许操作及可用交付字段；pending 直接派发，resume 仅允许 blocked/paused。
- 状态和依赖错误返回具体字段路径与可选交付，保留一次针对性纠正。

## 尚待修复

### P1：浏览器结构化失败没有接入宿主失败判定

位置：src/browser_control.rs:316；src/agent_service.rs:528、3677。

upload_failure 返回 `{ok:false,status:"failed",error_code,...}`，不带顶层 error。普通工具走 observer_output_failed，它只检查数字 status 非零、顶层 error 和 observer_interrupted。因而文件不存在、多输入框、选择器不存在等上传失败会被标记为 isError=false，scheduler.observe 接收 failed=false。browser_visibility_locked 和 browser_wait 的 invalid_condition 同样存在问题。

建议：统一工具结果失败契约，至少识别 ok=false；保留 error_code、stage、needed 原始结构。等待超时应明确为条件未满足，不能被当成验证通过。补宿主集成回归，不能只断言 browser_upload 的 error_code。

### P1：完成任务会立即关闭刚交付的可见浏览器

位置：src/agent_service.rs:1669；src/browser_control.rs:20。

finish 无条件调用 browser_control::cleanup；cleanup kill 浏览器并删除 profile。completed 路径同样调用 finish，因此即使上传成功并显示可见窗口，Agent 汇报完成后窗口也会关闭。此前无头会话的清理逻辑没有适配新展示能力。

建议：将执行结束与展示会话关闭分开。需要交付给用户的可见会话完成后保留，直到明确关闭、替换或宿主退出；无头验证会话仍可及时清理。保留会话身份及所有权。增加“完成后窗口仍存在”的生命周期测试。

### P2：requires_browser 仍然不能保障当前文稿已加载

位置：src/work_scheduler.rs:1115、1028；src/browser_control.rs:126。

当前门槛仅要求最近 16 条操作中存在一次成功匹配非空 expect_text 的 browser_read。欢迎页已有的“PPTX Editor Engine”也能满足条件，slide_count=0、未上传真实文件、后续导航或上传失败都没有参与判定。操作记录也没有保存读取对应的 page epoch，旧匹配不能保证对应当前页面。

建议：为需要打开真实文稿的工作声明具体验收条件，按当前页面身份/epoch、目标文稿标识、成功上传结果、非零幻灯片与加载状态判定；普通打开页面任务维持自身的较轻条件。导航或上传后使旧页面匹配失效。字体成功与画面正确性需独立验证，不能仅用非空文本或 document.fonts.status="loaded" 替代。

### P2：页码字段没有覆盖目标编辑器

位置：src/browser_control.rs:126；目标项目 index.html:53。

browser_read 查询 `.page-number,.slide-number,[aria-current="page"]`，实际编辑器使用 `#page-indicator.page-info`，因此当前项目 page_indicator 一直为空。建议覆盖实际控件或支持明确的观察条件，并增加目标页面夹具测试。

### 补充边界

- 上传观察到 change_event_received=false 时仍可以返回 ok=true、matched=true；这个结果最多证明文件被赋值，不能证明项目接收并启动解析。建议明确分类并提示条件未满足，后续通过加载状态确认；不要自动重复派发 change 导致重复解析。
- 文档要求的普通 Windows 外部路径转换尚未出现在上传路径中（仍直接用 workspace_path 返回路径）。不过本次中文/空格路径浏览器回归确实通过，不能据此认定它当前会导致 Chrome 上传失败。
- 可见模式只操作 Agent 自己的窗口，没有实现附着用户既有标签页；访问相同 URL 不代表同一页面状态。

## 本次测试

执行现有测试，均通过：

| 测试组 | Cargo 报告通过数 |
| --- | ---: |
| browser_control::tests | 6 |
| work_scheduler::tests | 15 |
| flow_tree::tests | 3 |
| agent_service::tests | 35 |
| 合计 | 59 |

browser_control::tests::live_editor_demo_shows_a_slide 未设置 PPTX_URL，函数提前返回；Cargo 仍将其计入通过。因此不能把 59 项全解释为完成了 59 项实际场景验证。其余浏览器夹具实际运行，确认中文路径、隐藏输入、change、多个输入、控制台/HTTP/字体失败观测可用。

当前上传夹具写入的是普通字节文件，文件名为 .pptx，不是可解析的真实 PPTX。尚未通过本次测试证明目标项目实际 PPTX 解析、字体成功、截图验收或可见窗口完成后交付正常。应先补上述宿主和生命周期遗漏，再进行完整真实任务验收。
