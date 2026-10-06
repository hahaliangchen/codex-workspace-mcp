# Agent 页面截图与视觉检查实施结果

日期：2026-10-06。对应 [实施计划](agent_visual_inspection_plan_2026-10-06.md)。

已实现六批中的代码链路和回归测试。真实模型已能辨认 PPT 页面未加载、删除前后的图形变化，但真实业务验收没有全部通过，具体失败和限制见下文。不能将自动测试通过或截图成功表述为 PPT 渲染验收通过。

## 1. 已实现的行为

| 部分 | 实现 |
| --- | --- |
| 浏览器 | 每个 task 独立 Chrome/Edge 会话、临时 profile 与页面锁；宿主绑定请求/节点身份；清理本任务资源；Windows 普通路径与 canonical 路径统一 |
| 截图 | viewport 默认、full_page 显式；每次不可变 PNG 和 artifact_id；SQLite 保存执行身份、来源、页面/viewport、capture_seq、时间、尺寸、字节、SHA256 与已知源码版本 |
| 读取 | view_image 只接受宿主 ID；校验任务/请求/实例，跨节点须显式有效上游导出；重新验证路径、格式、尺寸、hash，文件被覆盖即失效 |
| Worker | 本次截图默认选入下一次正常请求；图片在最终装配时编码为真实 image_url 内容块，普通 tool 回执保留；正常响应后只保存引用/判断，无额外意见回执轮 |
| MCP/协议 | MCP 图片工具返回 text + image 内容块；Anthropic image 与 Responses input_image 转换保留图片块；任务视觉请求跳过旧全局 visible-image 回退和静默预处理 |
| 能力/降级 | provider/model 明确 supported / unsupported / unknown；unknown 默认不发送图片；unsupported 仅在显式配置可用视觉路由时调用一次视觉服务，并记录其独立来源/耗时；失败为 unavailable |
| 预算 | 每轮最多 2 张；每张输入最多 8 MiB、400 万像素；过大材料缩放至 1600 边界内，记录输入 hash/尺寸与原图关系；原图保留，不能从缩放细节缺失推断元素不存在 |
| 交付 | visual_goal/requires_visual 必须使用独立 output 节点；显式 edit_targets 才可修复；pass/issue 须引用已成功接收图片的请求、同实例/执行版本、原样 checked_goal 和有效图片事实；uncertain/unavailable 交付为 blocked |
| 失效 | 文件修改或已观察到的页面版本变化使旧视觉结论失效；在同一模型响应里先操作页面、再使用操作前判断交付，也会被版本校验拒绝 |
| Observer | 优先复用相关图片，包括修改前后两张；已有当前图时不因“前图较旧”丢掉对比材料；缺图/全部过期时最多一次只读截图；不导航/点击/上传/写代码；沿用单会话许可、12 秒总预算和取消 |
| 建议/记忆 | 视觉材料、判断随原始 review/Inbox 实例保留；晚到结果不更新新请求；截图观察、交互记录、原因推测与源码事实区分，图片不自动成为长期源码知识 |
| Flow/调试 | 工具缩略图与原图链接；节点按请求/版本展示材料、Worker/Observer 判断与实际路由；显式消费上游图时保留原所有者；SSE 只传元数据；完整请求调试 opt-in，UI 默认折叠 base64 |

主要文件：`src/visual_artifacts.rs`、`src/browser_control.rs`、`src/agent_service.rs`、`src/work_executor.rs`、`src/work_scheduler.rs`、`src/work_organizer.rs`、`src/observer_service.rs`、`src/task_notebook.rs`、`src/ai_proxy.rs`、协议转换模块、角色提示，以及前端 `visual.ts` / `VisualMaterials.tsx` / SettingsModal / FlowView / ToolRow / RequestContextPanel。

没有增加审批角色或视觉审批状态。Observer 关闭时 Worker 的执行链保持独立。先前 A → B → A 建议版本回归仍通过。

## 2. 自动验证

| 检查 | 结果 |
| --- | --- |
| `cargo test -- --nocapture` | 161 passed，0 failed，2 ignored（两项真实网络验收需显式运行）；最终完整回归 14.44 秒 |
| `node --test tests/*.test.mjs`（frontend） | 11 passed，0 failed |
| `npm run build`（frontend） | TypeScript 与 Vite 构建成功；707 modules，3.81 秒 |
| `cargo build` | 非测试目标构建成功；32.73 秒，仍有现存 dead-code warnings |
| `git diff --check` | 无 whitespace 错误；仓库已有 CRLF 提示 |

新增验证使用真实 PNG、实际本地 HTTP、真实浏览器与 Worker/Observer 运行链，而不是检查字符串中是否包含路径：

- 请求中的图片可解码，hash/尺寸与对应材料一致；MCP 返回实际图片块。
- 无图片输入的 pass 被拒绝；未发送的图片/事实、过期执行版本、错误请求身份被拒绝。
- unknown 路由不发图；显式 fallback 多一次服务调用，主角色不接收图片、不冒充直接看图；服务失败有界降级。
- 两任务同时打开不同页面，截图不串；请求替换后旧页面不可用于新的只读观察。
- 恢复图片 ID 后重新装配图片；大图受控缩放、原图保留；材料修改或腐损可检测。
- Observer 复用前后 2 张图时 0 次新截图，缺图时仅 1 次；不新增 Worker 收件回执轮。
- 页面交互使已保存的视觉结论失效；write_check 不可兼做视觉验收。
- 前端历史请求/节点隔离、上游消费展示、未知能力提示与 base64 折叠。

脚本模型返回的事实用于验证协议和归属，**不计为模型视觉理解验收**。现有 `live_editor_demo_shows_a_slide` 在未提供 PPTX_URL 时直接返回，不将其测试名称/通过状态当成真实 PPT 成功证据。

## 3. 真实模型与 PPT 场景

实际路由：`gpt-proxy / gpt-6-luna`。现有配置声明为 unknown；仅在隔离验收工作区做一次显式有界图片能力探测，随后本次验收复用结果。没有修改全局模型配置，没有凭模型名称判断能力。真实网络调用均无自动重试。

浏览器操作由验收驱动执行，判断使用真实角色模型。自主 Worker 的完整调度/图片传输由上面的运行链自动测试验证；本节不宣称完成了全产品自主端到端任务。

### 加载示例并判断画面

使用现有 `D:/enterpriseProject/pptx-editor-engine` 的 demo、真实 bundle/WASM，点击“载入示例”，读取状态并截图。

结果：画面始终保留空文档欢迎页，幻灯片数量为 0。真实 Worker 指出“没有渲染幻灯片”，返回 issue，描述了空白区域、欢迎内容与侧边栏状态；不是用 DOM matched=true 得出 pass。

为排查启动依赖，临时启动了该项目已有的字体后端，确认 145 个字体 family；重新检查后仍未显示幻灯片。临时服务已停止。没有修改这个外部项目的源码，也没有将加载失败误报为业务成功。后续示例成功渲染仍需要该 PPT 项目的启动/渲染链排查。

### 删除视觉节点并确认变化

通过 `scripts/visual_acceptance_harness.mjs` 在隔离页面导入现有 `ai-ppt-view` 的真实 `SlideEditor.tsx` / Moveable，加载 SVG 幻灯片示例；任务内原生鼠标选择粉色矩形，`browser_press_key(Delete)` 删除，然后捕获两张不可变截图。

状态证据：目标节点数量 1 → 0，编辑 onChange 已触发，SVG 中蓝色圆形仍为 1，标题仍存在。真实 Worker 和 Observer 均描述了“前图有粉色矩形、后图该区域为空，标题和蓝色圆形保留”。

这是现有编辑器组件的真实删除验收，不是完整登录产品、文档持久化或导出回读验收；这些能力没有据截图推断通过。

### 来源与失败保留

模型格式并非总能满足契约：曾改写 checked_goal、漏掉 artifact_ids；宿主拒绝对应 pass，Observer 结果转为 uncertain。另有 Observer 在 12 秒预算内未返回，记录 uncertain，无额外循环。

为减少普通格式遗漏，宿主现在提供带真实 request_trace_id / checked_goal / artifact_ids 的结果结构模板；仍保留严格校验，不为“让验收通过”自动补造成功判断。

保留的可核对报告：

- [首轮能力探测与真实两角色看图](../.codex-workspace-mcp/visual-acceptance-visual-39480-1791259702571-0/report.json)：加载 issue；两角色均识别删除变化；Worker 改写 goal 被拒绝。此轮 Observer 在当时校验规则下返回完整来源判断，后续加强了 expected_visible_result/事实校验。
- [严格校验下的 Worker 删除交付](../.codex-workspace-mcp/visual-acceptance-visual-41524-1791259889593-0/report.json)：加载 issue、删除 pass 均通过宿主绑定校验；Observer 12 秒超时。
- [最终 PPT 场景记录](../.codex-workspace-mcp/visual-acceptance-visual-35724-1791260051259-0/report.json)：加载仍 issue；删除的可见变化和 SVG 状态正确，但该次 Worker 改写 goal、Observer 漏掉 artifact_ids，交付校验未通过。
- [复用图片的 Observer 有界复查](../.codex-workspace-mcp/visual-acceptance-visual-35724-1791260051259-0/observer_recheck.json)：2 张图片、63,504 原始字节、0 新截图；12,018 ms，uncertain 超时。

每个 report 邻接的 `.codex-workspace-mcp/visual/` 保存原始图片和 SHA256 对应文件；独立验收数据库也保留在该工作区，不污染日常任务列表。

## 4. 调用与预算统计

| 一次完整场景运行 | Worker | Observer | 截图/图片 | 耗时/结论 |
| --- | --- | --- | --- | --- |
| 首轮探测 | 2 | 1 | 3 次 Worker 截图；Worker 1+2 张；Observer 复用 2 张、新截图 0 | Worker 4,479 / 13,569 ms；Observer 10,285 ms |
| 严格校验复查 | 2 | 1 | 同上 | Worker 4,315 / 9,736 ms；Observer 12,002 ms 超时 |
| 最终场景 | 2 | 1 | 同上 | Worker 9,507 / 3,796 ms；Observer 9,072 ms，回执缺字段 |
| 最终 Observer 结构模板复查 | 0 | 1 | 复用 2 张；新截图 0 | 12,018 ms 超时 |

所有完整场景的 PNG 原始字节一致：加载 34,838 B，删除前 32,087 B，删除后 31,417 B；每次 Worker 共 98,342 B，Observer 两张共 63,504 B。图片均在像素/字节预算内，使用原图。

整个诊断过程共 10 次真实模型请求尝试：Worker 6、Observer 4；其中一次独立 harness 路径检查在 HTTP 调用前失败，没有模型请求。视觉服务降级调用 0；自动重复调用 0；额外 Worker 意见回执轮 0。为了修正提示、检查环境和确认未通过的契约，人工驱动重新运行了场景两次，并增加一次 Observer 复查；这些不能算作“没有重复成本”。超时的请求是发送尝试，不能表述为已收到模型评估。

## 5. 启用与边界

在设置中的模型能力选项为实际 provider/model 声明图片输入 supported 后，Worker / Observer 才会直接附图。当前全局配置仍为 unknown，所以默认生产任务会明确展示 unknown_capability，不会偷偷发图或自动切模型。unsupported 的角色需要用户显式启用并选择已声明 supported 的视觉服务路由，才能降级。

生产路径不自动探测 unknown 模型。本次网络探测是隔离验收行为，不将临时结论写入用户的全局配置。

外部 MCP 沿用宿主 workspace 对应的 MCP 会话命名空间；内置 Agent 才有宿主 task/request/work 实例绑定。不要用模型自报 task_id 扩大读取权限。

页面锁和导航/DOM 签名可识别已知交互和捕获期间的状态冲突；不能冻结第三方页面所有异步 Canvas/WebGL 绘制。截图只证明捕获时的像素，仍须用当前截图和实际交互/节点状态验证具体目标。

本次未宣称全页局部定位、截图裁剪、导出回读、完整 PPT 产品登录流程、任意模型视觉质量或失败示例渲染都已通过。首版主链路、失败可见性和有界行为已实现。
