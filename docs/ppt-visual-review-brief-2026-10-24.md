# PPT 视觉审查：插件出题、两份结论、按页复查

日期：2026-10-24。起因：项目 816 的检测阶段 59 轮、约 11 分钟、7.1M tokens。审查问题由 Agent 自己写，审查模型只看得到这段问题和图片：把大纲里的「可选讨论邀请」写成必查项；两次漏判（重复的「一」、目录编号 01/二/三）；每改一页就整套重审 5 页。

## 改了什么

**宿主（通用，不含 PPT 业务）**
- 页面工具返回图片、且插件声明 `visual_review` 能力时，宿主调用插件 `plugin/reviewBrief`，带上工具名、参数、页面 `document` 和任务看板上的计划；返回的 `{instructions, subject}` 存进这些图片的视觉素材元数据。`src/single_agent.rs`（`page_tool_result`）、`src/plugin_io.rs`、`src/remote_sessions.rs`。
- `analyze_images` 把图片带的说明原样作为 system 消息：相同的 `instructions` 只出现一次，再按顺序附 `Image N:` 与该图的 `subject`；Agent 的问题仍是 user 消息，图片前标 `Image N`。没有说明的图片维持原样。`src/image_analysis.rs`。
- 每次分析保存为 `analysis_id`（原文 + 每张图的渲染来源），结果返回 `analysis_id`。远程插件工具参数顶层有 `analysis_id/analysis_ids` 时，宿主把对应分析放进调用上下文 `context.analyses`；ID 不属于本任务直接失败。

**PPT 插件（ai-ppt-server）**
- 握手能力加 `visual_review`；新增 `plugin/reviewBrief`。`instructions` 原样读取 `skill:references/visual-review-brief.md`：先转录、再列缺陷、再按大纲分级（阻塞/可选）、最后结论，并输出 json 结论块；Agent 说「已改好」「之前通过」也照样四步检查。`subject` 用已确认计划生成：文稿标题、整体要求、该页大纲条目；未确认时明确说没有确认过的条目。
- `record_visual_review` 改为 `revision + analysis_ids + reviews[{page, assessment, findings, reason?}]`。分析原文由宿主经 `CallWithEvidence` 交给工具，不在模型参数里。每页必须有同一版本、已保存的渲染图和审查结论块；Agent 与审查模型结论不同必须写 `reason`。两份结论、分析 ID、图片 ID 和该页内容哈希一起保存。
- 验收按页内容判断：`slide_hash` 覆盖该页 slide XML、关系文件和引用的包内部件（不含备注）。编辑器只回写改过的部件，所以只改第 2 页时，其余页的验收照常计入。进度新增 `disagreements`。早期没有哈希的记录只在原版本计入。
- Skill 与生产流程说明：审查说明随图片自动送达，Agent 的问题只补充关注点；只复查改过的页；记录时传 `analysis_ids`、不同意时写理由。

## 验证

- 宿主：`remote_sessions::tests::rendered_images_carry_plugin_review_instructions_into_vision_and_back_as_evidence` 走真实执行循环（模型调用页面工具 → 假页面返回 PNG → 插件给说明 → `analyze_images` → 插件记录工具），断言插件收到的 `reviewBrief` 请求体、视觉请求的 system 消息原文、记录调用 `context.analyses` 里的原文与渲染来源、伪造 ID 被拒。`image_analysis::tests::plugin_review_instructions_are_sent_verbatim_as_the_system_message` 断言两张图的线上请求体与分析存取。无能力的插件不会被请求说明。
- 插件：页面哈希（按幻灯片列表顺序、只受本页部件影响、备注不影响、无列表时按编号）；审查说明（四步顺序、规则、使用已确认条目、不泄漏别页要求）；记录（只改第 2 页其余仍计入、旧版本和旧图片被拒、两份结论与理由、缺分析/缺结论块/别页图片被拒、结论块取最后一个）；Skill 文本；RPC 层握手能力、`reviewBrief` 往返、带/不带宿主分析的记录。
- 全量：宿主 230 项通过，22 项失败均在未改动的模块（观察者、附件、浏览器、委托、记忆面板等），抽查原因为 Windows 文件占用（测试删临时目录时 SQLite 仍打开）和 GBK 编码；插件 `ppt_runtime`、`api/v1/system` 除 `TestSVGValidationAndResourceConfinement`（Windows 创建符号链接需要权限）外全部通过。

## 已知限制

记录时要求渲染图来自当前保存版本：先渲染第 2 页、再改第 5 页、最后才记录第 2 页时，需要重新渲染第 2 页。

## 应用

1. 后端：用 `target/ppt-real-e2e-20261022/ai-ppt-server-visual-review.exe` 替换正在运行的 `ai-ppt-server-layout-advisory.exe`（同一配置与资源目录）。
2. Agent：停掉 3001 上的进程后 `cargo build` 并重启。
3. 刷新 PPT 编辑页，重新握手以加载新能力、新的 `record_visual_review` 和 skill。

## 第二轮（同日）：事实把关、整套一次视觉检查

**引擎（pptx-editor-engine，页面里运行）**
- 修复嵌套段落重复：`measure-dom.ts` 只取最外层段落块，`<p data-paragraph><span data-paragraph>一</span></p>` 量出一段「一」。真实 Chromium 回归测试在旧代码上复现为两段。
- `apply_html` 变更摘要带文字改前改后：`{action:"update", fields, text:{before:"CONTENTS", after:"DISCUSSION"}}`；新增节点带 `after`，删除节点带 `before`。
- 同一文本框里出现重复段落时给出版面提示（不阻止应用），说明重复的文字和次数；`check_layout` 新增 `repeated_text`，整套检查也能发现。
- `render_slide` 支持 `slides`，一次最多 9 页，每页一张图并带 `meta:{slide}`。重新导出插件后端的 `page_tools.json`。

**宿主**
- `analyze_images` 单独上限 9 张（对话里直接附图仍是 2 张）。3 张及以上拼成一张编号大图（九宫格）：最多 3 列、每格上方黑条写白色编号；图片保持原尺寸，超过 4 百万像素才等比例缩小，5 页文稿是一张 3×2、每页 1024 像素宽。大图另存为视觉素材，可直接看到视觉模型收到了什么。
- 页面图片的 `meta` 存进素材，`plugin/reviewBrief` 带上各图 `meta`，插件按图返回 `subjects`；每张图的审查说明是它自己那一页的大纲条目。

**插件**
- 审查说明只查画面能看到的问题（文字重复/缺失、并列元素不一致、裁切溢出重叠、方块字乱码、对比度、对齐越界、图标图片缺失、残留示例字），论点和措辞不算画面问题；说明拼图的编号方式。
- 记录验收时按 `meta.slide` 判断图片是哪一页。
- Skill：视觉检查只在全部页面保存后做一次：一次 `render_slide`（所有页）+ 一次 `analyze_images`（编号大图）；起草时不逐页渲染和检查；修完只复查改过的页。

**前端（ai-ppt-view）**
- 「视觉检查已返回」按每张图的 `meta.slide` 计页，一次渲染多页也能对上。

**验证**
- 引擎：新增 7 项（变更摘要、重复段落提示、`repeated_text`、多页渲染与参数错误、真实浏览器嵌套段落）及受影响的 51 项通过；全量 513 项通过，失败的 4 个文件与本轮无关：字体相关 2 项（Windows 字体工作进行中）、`design-client` 缺少 Playwright 自带浏览器、`render-server` 并发排队测试在全量并行时超过 5 秒（单独运行 3 次均通过）。
- 宿主：新增拼图测试（解码实际发送的 PNG，核对 3×2 布局、每格像素、黑条与编号、保存的拼图与发送内容一致、10 张被拒）；端到端测试改为一次渲染两页，断言 `reviewBrief` 收到两张图的 `meta`、视觉 system 消息含两页各自的大纲条目、插件证据里每张图的 `meta`。全量 231 项通过，22 项失败与上一轮完全相同（Windows 环境）。
- 插件：每图 subject、`meta` 定位页、无 meta 单页兼容、审查说明画面规则、Skill 一次性检查规则、RPC 层 `reviewBrief` 往返；`ppt_runtime`/`api/v1/system` 除符号链接权限测试外全部通过。
- 前端：多页渲染计数测试；对新引擎类型检查通过。

## 应用（含第二轮）

1. 后端：`target/ppt-real-e2e-20261022/ai-ppt-server-visual-review.exe`（23:04 重新编译，内嵌新的 `page_tools.json`）。
2. 前端：引擎改了，需要用 `target/ppt-local-backend-20261023/build-ai-ppt.cjs` 重新构建（它先对新引擎做类型检查，再输出到 `frontend-build`）。
3. Agent：停掉 3001 后 `cargo build` 并重启。
4. 刷新 PPT 编辑页重新握手。

未重启任何服务，未重新构建前端，未提交代码。
