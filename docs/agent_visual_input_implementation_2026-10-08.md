# Agent 视觉输入实施与验收记录

日期：2026-10-08。对应 [视觉输入接通计划](agent_visual_input_next_plan_2026-10-08.md)。

## 实现

- 新增真实图片能力探测：随机生成 2 行 3 列颜色块和黑色圆点，检查最终请求中的 PNG 字节，要求模型按位置回答颜色和点数。HTTP 200 本身不代表图片可用。完整响应、错误、耗时、图片哈希与路由指纹作为本地证据保存。
- React 设置弹窗和兼容设置页提供「验证图片输入」，CLI 提供 `--probe-image-input`。只有本机探测通过才能保存支持状态；前端提交不能伪造证据。端点、协议或模型变化会使证据失效，保存期间检查配置修订号以防覆盖并发编辑。目前探测支持 OpenAI Chat Completions 协议。
- 复用既有任务图片分发，保留任务、节点、版本、页面/会话、采集时间和原图/缩放哈希。Worker 后续请求带上已发生的图片分发记录，解决看图后无法正确引用 `request_trace_id` 的问题。
- 有新图时带入同一任务中最近成功发送的图片作为比较上下文，保持两图上限。没有新图时不重复发送；最近失败的图片只允许显式重新选择。任务提示说明如何用 `view_image` 复用已有图片，避免为补齐图片输入反复截图。
- HTTP 检查标识统一清理 `http-probe:` 后的空白及 URL 片段，避免模型声明的同一个 URL 因格式空白被拒绝。
- Worker、Observer 和视觉服务保留 HTTP 错误原文。Worker 的实际图片请求失败交给 Organizer 的普通决策路径；已尝试图片不会自动重复发送，模型可显式调用 `view_image` 重新选择。单次运行收到明确图片拒绝后，更新该运行的能力状态。
- 视觉服务仅用于明确不支持图片的路线；未确认能力不触发回退。保留服务来源、原始响应、所看图片和验证失败原因，不把服务结果表述成角色直接看图。
- 新增 `visual/model_failed` 历史展示和 Observer 工作路径事件。工具、Flow、视觉判断和图片在结束清理及重新加载后继续可用；临时执行和调试快照按既有规则清理。

## 实际模型验证

当前保存的 `gpt-proxy / gpt-6-luna` 路线通过随机测试：HTTP 200，耗时 2,941 ms，六个位置的颜色及点数全部正确。最终请求含一张真实 PNG，图片内容编码前后相同。能力由 `unknown` 更新为 `supported`，视觉回退未启用。

原始报告在本机 `.codex-workspace-mcp/image-input-probe-20261008.json`。探测只更新能力证据，不改凭据及模型路由。密钥、完整模型配置及业务图片未加入代码库。

## 真实 PPT 验收

使用用户授权的本机 8 页示例，实际调用配置中的模型、Organizer、Worker 和 Observer。验收构建读既有 PPT 项目源码及 WASM，输出到当前仓库忽略目录；验收使用独立数据库和示例副本。为控制验收耗时，单次验收采用 `low` 思考等级，日常配置不变。

早期运行真实发现以下问题，报告如实保留失败：

1. 模拟测试的一秒 Organizer 超时不适合真实接口；仅显式真实验收环境变量改用 60 秒预算，生产超时保持原值。
2. 既有 PPT 构建缺少 WASM 所需 glue 导出，页面保持空工作区；重新构建时显式保留导出。
3. PPT 页面依赖本机字体后端；未启动时加载失败并出现阻塞弹窗。实际验收需先启动已有字体服务。
4. 原验收把清理后的总事件数与含临时快照的总数比较；改为逐条核对所有持久历史事件及图片哈希。

### 最终完整运行

实际耗时 163,322 ms，任务 `completed`，验收断言通过。六个任务均独立返回，16 次工具调用包含 6 次 `yield_work`，只打开一次浏览器、上传一次文稿、点击一次下一页、捕获两张截图。

| 阶段 | 实际证据 |
| --- | --- |
| 服务探测 | 单次 `http_probe`，HTTP 200，保留采样事实 |
| 可见页面 | 单次 `browser_open`，复用同一 session/page |
| 上传及加载 | 单次 `browser_upload`，`browser_wait` 确认加载完成 |
| 初始页码 | `browser_read` 显示 Slide 1 / 8，共 8 页 |
| 图片及翻页 | Worker 两次真实图片请求分别包含 1 图、2 图；第二次带入此前图片，正式 `visual_check_result` 引用两图。下一页画面显示 Slide 2 / 8 |
| 浏览器诊断 | WASM 和字体 API 200；favicon 404；字体及请求覆盖限制进入最终正文 |

Observer 两次请求实际包含 1 图、2 图；结束完整复盘成功，`memoryRecorded=true`。清理后 HTTP 历史接口恢复 377 条持久事件，逐条内容一致；两张图片重新读取后的哈希与原图一致。

另用真实 Observer 对已保存的两图再次检查，验收通过，新增截图为 0。实际请求的两张 PNG 哈希与分发记录一致，返回有 `check_id` 的两图事实。原始回应和输入哈希保存在同目录的 `observer_recheck.json`。

本机证据目录为 `.codex-workspace-mcp/visual-input-e2e-visual-19556-1791458042633-0/`，保存 `report.json`、`restoration.json` 和原始图片。报告中的实际 HTTP 图片哈希来自发送前最终模型请求，未把 base64 图片复制进历史事件。

视觉结果是带限制的 `issue`：第一页中央区域明显留白，无法仅凭截图判断原因；第二页未见明显整体裁切，小字和小图细节无法可靠确认。字体状态 loaded，但 FontFace 列表为空；资源观察有覆盖限制。实际验收没有执行编辑交互，未声称验证拖拽、删除或导出。

## 回归结果

- Rust 视觉相关用例：21 项通过，真实接口用例默认忽略，按上述授权单独执行。
- 调度器用例：41 项通过（其中 3 项也包含在视觉筛选中）。
- 配置与证据保存用例：4 项通过。
- Observer 观察、历史咨询及结束复盘用例：1 项通过。
- 真实六阶段 PPT 用例及真实 Observer 已保存图片复查用例：各 1 项通过。
- 前端视觉及六阶段历史用例：6 项通过。
- TypeScript 类型检查、Vite 生产构建和兼容设置页模块语法检查通过。

图片拒绝运行回归核实：最终 HTTP 确有图片，接口返回原始拒绝，实际图片请求仅一次；Organizer 和最终正文均保留该原因，任务正常收尾。

## 复现

先启动服务并在设置页验证当前模型，或运行：

```powershell
cargo run -- --probe-image-input
node scripts/visual_acceptance_ppt_build.cjs
```

启动已有 PPT 字体后端（仅本机 `127.0.0.1:8080`），然后设置：

```powershell
$env:CODEX_REAL_MODEL_ACCEPTANCE = '1'
$env:PPT_DEMO_DIST = Join-Path (Get-Location) '.codex-workspace-mcp/visual-input-ppt-dist'
$env:PPT_SAMPLE_PATH = '本机授权的示例 PPTX 绝对路径'
cargo test real_ppt_visual_input_six_stage_acceptance -- --ignored --nocapture
```

复查已经保存的两图：

```powershell
$env:VISUAL_ACCEPTANCE_REPORT = '上述验收目录中的 report.json 绝对路径'
cargo test real_visual_observer_six_stage_reuse_recheck -- --ignored --nocapture
```

CLI 验证将证据写入磁盘；已运行的服务需重启后加载。设置页验证会直接更新服务配置。

实际模型验收会发送该文稿截图，需获得文稿内容外发授权。每次运行输出独立目录，其中 `report.json` 含实际 HTTP 图片哈希、角色判断、最终正文及事件；`restoration.json` 记录持久事件逐条比对和图片恢复。报告和业务图片保留在本机忽略目录。
