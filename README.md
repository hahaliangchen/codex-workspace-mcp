# codex-workspace-mcp

这是以 DeepSeek Harness（DSH）为参考、由 Rust 执行任务的本地 Agent。`/agent` 页面复用并改造 DSH 前端，通过本项目的任务 API 与 Rust 工具、记忆和代码索引协作。仓库名沿用早期名称；Agent 的运行不依赖 Codex。旧的协议兼容入口仍保留。

它的目标很直接：

- 让 Agent 少靠 shell，优先使用结构化文件、搜索、索引工具
- 让代码结构可导航，让项目经验可沉淀
- 让 Agent 可以接入 DeepSeek、Mimo、OpenAI 兼容中转站等不同 provider
- 在文本模型和多模态模型之间做轻量适配，而不是维护复杂状态

## 核心能力

### 工作区 MCP

- 文件工具：读文件、读行范围、写文件、替换行范围、列目录、搜索文本
- 项目执行工具：npm 安装依赖、运行项目脚本、查询进程和增量日志、停止进程组；见 [项目执行工具说明](docs/project_process_tools.md)
- 代码索引：Go / Rust / Python / TS / JS 符号、注释、调用关系、建议阅读路径
- 项目记忆：记录每次改动、实现原因、测试结果和潜在风险
- Skills：按需列出和读取本地 Codex skills

### AI Proxy

- `/v1/responses`：Codex Responses API 入口，默认由本地 Agent Runtime 接管
- `/v1/chat/completions`：OpenAI Chat Completions 入口
- `/v1/messages`：Anthropic Messages 入口
- 模型路由：设置页读写 DSH profile 中的提供商、主代理模型与可选的子代理模型覆盖配置
- Agent Runtime：本地运行 ReAct 工具循环，直接调用 MCP 文件、搜索、索引、记忆等工具
- 过程输出：Codex 只看到普通 Responses 文本流，包括 `[agent]`、`[tool]` 过程信息
- 协议拆分：消息协议转换和工具调用控制分开处理，不再使用 raw Codex 透传、echo 伪装或 call_id 等待下一轮

### Rust 插件内核

启动时，内核按作用域树挂载 `workspace`、`model-settings`、`tools`，并在 `tools` 下挂载文件工具、工作记忆、代码索引和 `agent-tasks`。插件声明依赖的服务；缺少依赖时拒绝启动。服务与事件监听归所属插件管理，卸载时按子节点优先、注册逆序撤销；仍有外部插件依赖时拒绝卸载。

任务执行实际读取插件工具目录来生成模型可调用工具，并调用对应插件注册的处理器。`GET /agent/plugins` 返回当前作用域树、服务、依赖和最近生命周期事件；设置页的「插件树」提供只读视图。Rust 侧当前是内置插件，任务事件的 SQLite 持久历史仍由任务服务负责；`/agent` 的浏览器端已接入 Cordis 核心和页面插槽，第三方动态插件尚未接入。

### 任务 Flow 与 Observer 全局观察

角色间传递的原始消息与宿主摘要分开保存：组织者通过 `current_result.worker_return` 收到 Worker 原文，Observer 的 `observer_return` 随同一份 `observer_messages` 进入 Worker 下一轮和组织者上下文。摘要可用于导航，原始消息不裁剪；没有建议的 Observer 观察也会传递。视觉请求无论成功、能力未知或服务失败，都保留模型、状态及失败原因；宿主报告能力事实，由角色决定下一步。

Flow 节点代表具体任务，Organizer 逐个派发有独立目标和返回条件的阶段，Worker 执行当前阶段并交付结果，同一个执行者可以完成多个节点。服务探测、打开页面、上传并等待加载、读取初始状态、截图与翻页核对、诊断等阶段分别记录；相邻任务显式引用前序结果并复用仍可用的浏览器会话和上传回执。每个节点展示目标、完成条件、约束、实际结果、工具调用及涉及文件。任务结束或服务重启后，工具调用、原始工具结果、Flow、进度和完整消息仍保留；仅清理内部运行快照、增量流及调试请求快照。

Observer 有独立的异步观察循环和全局进度视野，持续接收 Worker 的步骤汇报与工具活动。观察较慢时合并过期状态，优先评估最新方向；发现重复探索或信息足够时，给 Worker 发出简短的收尾建议。意见以 `observer/plan_review` / `observer/progress_review` 记录，及时返回的建议进入 Worker 下一次决策上下文。Worker 不等待观察者，意见也不会批准、阻断或接管操作。Worker 可自行调用 `consult_observer` 查询历史决策、项目记忆和本项目过往任务信息；这项主动咨询会等待答复。Observer 不检查语法、格式、编译结果或普通工具错误。Worker 的回答和状态先完成，Observer 随后独立复盘完整路径，说明可缩短之处和可复用发现的相关性；只有发现有复用价值的信息时才写入工作记忆。设置页可让 Observer 跟随当前 Worker 模型，或单独选择提供商与模型；设置保存在 DSH profile 旁的 `codex-workspace-observer.yml`。历史检索只访问本项目的 `agent_tasks` / `agent_task_events`，不读取 Codex 或 ChatGPT 会话，并采用有界关键词匹配，可能漏掉表述不同或较旧的相关会话。

日常 Observer 上下文只包含全局目标/状态/依赖概览、当前节点事实、相关 Worker 原始返回，以及上次成功观察之后的新活动。`activity_since_seq` 标明增量起点；失败的观察不推进该起点，当前视觉请求及能力缺失原因保留。旧操作不再同时放入活动摘要、操作列表和路径摘要。模型请求统一使用 60 秒超时，排队及截图准备不占该时限。

请求交付结束或执行失败后，Observer 单独调用一次模型，按时间顺序复盘完整工作路径，包括中段动作、原始 Worker 返回和视觉失败信息。它分析可缩短的步骤及更直接的路线，随后写入有复用价值的经验；日常观察不写复盘记忆。答案和任务状态先交付，复盘不重新派发 Worker。取消或中断只保存观察状态。

Observer 的复盘记忆区分已确认的项目事实与可复用工作路径，记录适用范围、来源任务和文件/符号线索；缺少来源的判断保留为待核对结论，不写成已确认事实。同一任务的复盘重复执行时更新同一条记忆。Worker 在后续相关任务开始时可做一次定向 `search_work_memory`，根据适用范围与来源决定先看哪里，再以当前代码核对；记忆只提供线索，不替代当前项目状态。

### Agent 系统提示词

`/agent` 的 Worker 角色提示词在 `prompts/worker_system.md`；Worker 和 Observer 共同使用 `prompts/shared_reasoning.md` 中的做事方法，它从 `user-reasoning-style`、`user-thinking-style` 的共通部分提炼。Rust 在 Worker 任务运行时追加工作区路径、操作系统、主任务/子任务角色，以及 Worker 自主规划和步骤汇报要求。Worker 角色提示词还吸收了 `cursor_system_prompt.md` 中适用于本项目的结构化工具优先原则。Cursor 身份、专属工具名、终端文件和代码引用格式不适用于本项目，没有复制进运行时提示词。

## 日志策略

AI Proxy 日志统一走 `proxy_log`，外部模块不再自己实现文件日志或数据库日志。统一入口负责决定写文件、写 SQLite，或两者都写。

- 文件日志：启动时创建 `logs/` 目录，并为本次进程启动生成一个 `YYYYMMDD_HHMM.log` 文件；同一分钟内重启发生重名时追加 `_2`、`_3` 后缀，避免覆盖或混写旧日志
- 数据库日志：结构化记录继续写入 SQLite，便于 `query_logs` 查询
- 输入诊断：Responses input 只向本次启动日志写入限长摘要；工具输出只记录 call_id/长度，避免完整历史在文件日志中重复膨胀
- 历史保留：SQLite 原始会话记录按 24 小时清理，单条内容超限会截断
- 异步写入：当前使用 `std::sync::mpsc` 后台线程落盘和入库，正常运行可避免请求线程阻塞；如果进程突然崩溃，队列尾部少量日志可能来不及写入

## 上下文策略

历史会话和模型上下文分开处理。

- 历史会话：默认按 workspace 作为上下文边界写入 SQLite，同一工作区内多个聊天可以共享项目历史；请求里的 Responses `conversation` / `previous_response_id` 等字段保留在原始历史内容里，不参与默认隔离
- 模型上下文：只取最近的干净消息片段，默认保留最近 24 个片段
- 异常工具链：tool_call / tool_output 不配对时，只在模型上下文层降级成短 assistant 文本，不修改原始历史
- 大段内容：tool output、tool arguments、普通文本超过阈值会截断或摘要
- 噪声过滤：代理 forwarding body、工具 schema、namespace/tools 大段 JSON 不进入模型上下文，只留日志或历史库

## Agent Runtime

`/v1/responses` 现在只有 agent 代理模式，不再通过配置开关选择旧桥。

代理流程是：

1. Codex 请求进入 `/v1/responses`
2. 本地 Agent Runtime 把上游模型当作 agent client 调用
3. 上游模型需要工作区信息时，调用 `codex_workspace_mcp__*` 工具
4. Runtime 在本地直接执行 MCP 工具，并把 `function_call_output` 继续喂回上游模型
5. 上游模型给出最终答案时，Runtime 转成普通 Responses SSE 文本流返回给 Codex

这条路径把两层协议拆开：Codex 只和本地 agent server 通信，上游模型只和 agent client 通信。工具调用控制属于 Runtime，不再伪装成 Codex 终端命令。

## 多模态策略

### Agent 任务的图片输入

在设置页的模型能力中点击「验证图片输入」，会先保存当前设置，再向选定模型发送随机颜色与圆点测试图。只有实际请求包含 PNG 内容且模型回答正确时，才记录为 `supported`。HTTP 成功但回答错误、网络失败或认证失败仍为 `unknown`；接口明确拒绝图片时记录为 `unsupported`。验证原始响应、错误与耗时保存在本地 `agent-generation.yml`，证据绑定端点和模型，修改路由后需重新验证。也可运行 `codex-workspace-mcp --probe-image-input` 验证当前主模型。

`browser_screenshot`、`view_image` 和图片材料使用任务所属的不可变图片记录。支持图片的 Worker/Observer 收到真实 `image_url` 内容块；后续回合保留请求标识、图片来源、页面版本与判断，方便正确引用。能力未知时保留截图并报告未确认范围；只有当前模型明确不支持且已配置经过验证的视觉路线时，才调用视觉服务。模型请求失败的原始原因进入角色上下文、最终交付和历史视图，图片不会被静默重试。

实际验收与复现方法见 [视觉输入实施记录](docs/agent_visual_input_implementation_2026-10-08.md)。业务截图和完整本机配置保存在忽略的验收目录中。

浏览器会话由显式的 `browser_open` 创建，并在创建时选择 `visible`。读取、等待、诊断、截图和交互工具不会隐式启动 Chrome；没有会话时返回 `browser_session_missing`，由 AI 决定下一步。页面事实保留实际 `display_mode`、会话及页面标识，已关闭的旧会话与新页面分别呈现。

### 兼容代理的图片转述

面向文本模型的兼容代理会：

1. 只检查最新 user 消息中的真实图片
2. 尝试用默认主模型解析图片；该模型需要支持图像输入
3. 把图片块替换成 `[图像分析报告]` 文本
4. 不把原始图片 base64/path 发给文本模型
5. 不维护长期 `image_key -> 原图` 映射

如果用户后续明确要求重新看图，`analyze_image` 只会尝试分析当前请求上下文中仍可见的原图。若上下文里已经没有原图，模型应提示用户重新上传。

这个设计原则是：图片是当前请求上下文资源，不是代理持久状态。

## 配置示例

启动服务后打开 `http://127.0.0.1:3001/agent/settings`，在「模型提供商」添加端点、模型 ID 与 API 密钥；可点击「获取可用模型」从端点选取模型。再到「子代理」选择主代理模型，并按需启用子代理。子代理默认继承主代理的提供商和模型；只有展开可选覆盖项时才需要单独指定。设置页使用 DSH 的 profile patch 与凭据布局：`$DSH_HOME/profiles/web/cordis.patch.yml` 和 `$DSH_HOME/.credentials.yaml`。Windows 默认的 `$DSH_HOME` 是 `%USERPROFILE%\.dsh`；可通过 `DSH_PROFILE` 选择其他 profile。无需 `AI_PROXY_CONFIG` 或 `ai_proxy_config.json`。

GPT-6 Luna 的思考等级和「⚡ 请求加速」可在 `/agent` 输入框的模型菜单或 React 设置弹窗调整。新任务会保存当时的提供商、思考等级和加速选项；已创建任务续聊时沿用原设置。加速选项向上游发送 `service_tier: fast`，实际是否加速以服务返回的档位为准；本地默认关闭。两项生成设置保存在 `$DSH_HOME/agent-generation.yml`，API 密钥仍保存在凭据文件中。

```yaml
- id: llm-pi-ai
  config:
    providers:
      main:
        apiKeyEnv: MAIN_API_KEY
        api: openai-completions
        baseURL: https://api.example.com/v1
        models:
          - id: model-id
- id: agent-default-model
  config:
    provider: main
    model: model-id
- id: subagent
  config:
    maxDepth: 1
    maxActiveSubagents: 1
- id: subagent-model-selection-settings
  config:
    enabled: false
    allowedModels:
      - provider: main
        model: model-id
```

说明：

- `agent-default-model` 指定主代理模型，普通兼容接口也使用此提供商。
- `subagent-model-selection-settings` 默认关闭；开启后主代理最多自主委派一次，子代理不能再委派。
- API 密钥保存在 DSH 的 `.credentials.yaml` 中，设置页不会回显密钥。图片和架构分析复用默认主模型路由。
- 当前 Rust 任务执行器只支持 `openai-completions` 作为主代理和子代理路由；DSH 的其他协议可保存到 profile，但尚不能执行任务。
- 此设置页使用 DSH 的文件结构和关键插件字段，但还未接入 Cordis 设置编辑器。保存已有 profile 时会规范化 YAML 排版与注释；包含 DSH 自定义 YAML 标签的 profile 暂不能由本页编辑。
- `/v1/responses` 一律启用本地 Agent Runtime；不再需要 `raw_codex` 或 `agent_mode` 开关

## 工具一览

### 基础文件工具

- `workspace_info`
- `list_dir`
- `read_file`
- `read_file_lines`
- `search_text`
- `write_file`
- `replace_range`
- `edit_file`

### 代码索引

职责描述已接入普通符号检索：每个结果的 `description` 包含 `text`、`qualified_name`、`status`（`current` / `stale` / `missing`）、`source`、`scope` 和 `code_hash`，有记录时还提供功能关键词、所属模块和关注场景。描述及关键词参与搜索排序，过期描述降低权重；不需要先调用独立的业务记忆搜索。

索引从定义注释提取短摘要，Worker 理解关键入口后可通过 `record_symbol_business_context` 补充功能职责、关键词和关注场景。支持“文件＋限定名”、符号 ID，以及 `scope: "file"` 的模块描述。写入时传读取结果里的 `expected_code_hash`，代码变化会拒绝该次写入。描述使用确定性的 FNV-1a 内容指纹绑定版本；索引检查仍以修改时间和大小发现变化，解析时更新内容指纹。保留人工/Agent 描述，代码变化后标记待复核；注释摘要由新注释更新。首次查询会升级索引文件跟踪版本并提取现有注释。

Observer 可在原有复盘请求中返回最多四条职责描述，服务仅接受本次成功读取过的定义/文件，并校验读取时的代码版本。不额外发起模型分析请求，也不阻塞 Worker；未描述的代码仍能通过名字、路径、注释定位。

写入示例（`code_hash` 来自实际读取结果，不自行生成）：

```json
{
  "file_path": "src/example.ts",
  "qualified_name": "ExampleParser.parsePicture",
  "business_role": "解析图片及裁剪参数，生成图片节点",
  "keywords": ["图片", "裁剪", "图片节点"],
  "expected_code_hash": "读取结果中的 code_hash"
}
```

代码定位按所需信息选择工具：已知文件或行号直接读取；已知文件和定义名直接 `read_*_symbol`；未知定义用 `search_*_symbols`；字符串、配置键、错误信息和导入语句直接 `search_text`。不需要先检查索引状态或列举整个项目。

- `search_*_symbols`：多关键词默认 `match_mode: "any"`，按相关度排序后分页；`all` 要求全部词命中，`phrase` 匹配整段子串。支持 `file_path`、`directory`、`kind` 过滤，最多处理 24 个不同词。返回 `score` 和 `matched_terms`。
- `list_*_symbols`：默认返回 40 个简短条目，`search_*_symbols` 默认 20 个，单页最多 100 个。使用 `page.next_offset` 继续读取；局部符号默认隐藏，可用 `include_locals: true` 开启，签名和注释预览需 `detailed: true`（分别最多 240/480 字符）；完整定义由读取符号工具获取。
- `read_*_symbol`：可传 `symbol_id`，也可直接传 `file_path` 和 `name`，例如 `{"file_path":"src/pptx-parser.ts","name":"PptxParser.parse"}`。同名歧义会返回候选 ID。`include_context: true` 才附加调用关系。
- 查询自动检查文件修改时间和大小，仅重解析变化文件，并清除已删除文件的符号。首次升级会重建一次以建立文件状态；此后无变化不重新解析。
- `index` 字段包含上次检查时间、改动/删除文件数、成功解析文件数及跳过/失败文件（最多显示 20 条，附总数与截断标记）。扫描遵循 ignore 规则并排除依赖/构建目录；单文件上限 2 MiB。零命中不能据此断言代码不存在。
- 调用边仍是启发式结果，不是编译器语义或完整跨语言调用图；路径别名、宏展开等仍有限制。保留旧 ID 格式，ID 中含行号，改动后应重新定位或使用文件＋名称。


- `list_go_symbols` / `search_go_symbols` / `read_go_symbol`
- `list_rust_symbols` / `search_rust_symbols` / `read_rust_symbol`
- `list_python_symbols` / `search_python_symbols` / `read_python_symbol`
- `list_ts_symbols` / `search_ts_symbols` / `read_ts_symbol`

### 工作记忆

- `record_work_memory`
- `list_work_memory`
- `search_work_memory`

### 代理辅助

- `query_logs`
- `analyze_image`
- `spawn_subagent`
- `list_skills`
- `read_skill`

## 推荐工作流

先查索引，再读符号，再改文件：

1. `search_*_symbols`
2. `read_*_symbol(include_context=true)`
3. `replace_range` 或 `write_file`
4. 自动刷新索引
5. `record_work_memory`

对于图片：

1. 当前轮图片先由视觉 provider 转成文本报告
2. 后续优先复用文本报告
3. 只有用户明确要求重新看图时才调用 `analyze_image`
4. 当前上下文没有原图时，让用户重新上传
## 启动

### 浏览器任务页面

服务可以在尚未配置模型时启动。打开 `/agent/settings` 配置提供商、密钥和主代理模型，然后返回 `/agent` 提交任务。配置直接写入 DSH profile，外部编辑会在下一次任务请求时重新读取。

模型目录支持手动填写 ID，也能从提供商的 `/models` 端点获取候选 ID。提供商设置可逐模型声明支持的思考等级、默认等级和是否支持请求加速；这些 Agent 专用能力配置保存在 DSH profile 同目录的 `agent-generation.yml`。模型端点通常不提供可靠的思考等级信息，因此新发现的模型默认不启用这些选项，需要按实际接口能力设置。已有任务继续使用创建时保存的模型和生成参数，新任务使用当前默认路由。

任务页是 `frontend/` 下的 Vite + React 项目，需先构建一次：

```powershell
cd frontend
npm install
npm run build
cd ..
cargo run
```

打开 `http://127.0.0.1:3001/agent`。Rust 从 `frontend/dist` 读取页面（可用环境变量 `AGENT_WEB_DIR` 指向其他目录）；未构建时回退到内嵌的旧页面，旧页面也可在 `/agent/classic` 打开。前端开发时运行 `npm run dev`（`http://127.0.0.1:5173/agent/`），接口请求代理到 `127.0.0.1:3001`，可用 `AGENT_BACKEND` 改写。

页面可以新建对话（先 `POST /agent/sessions` 建空会话，再发送第一条消息），查看流式模型回复和工具步骤，切换轨迹表格与时间轴，停止运行中的任务，并在刷新后打开历史任务。选择已结束的主任务后，可以直接发送下一条消息；新轮次沿用该任务的用户消息、模型回复和工具结果，轨迹追加到同一个任务 ID。运行中的任务和子代理轨迹不接受跟进消息。输入框支持 `/` 技能建议和 `@` 工作区文件引用；提交时，Rust 会将被引用文件的有界内容快照附进模型上下文，旧轮次继续使用当时的快照。

`frontend/src/dsh/` 从 DeepSeek Harness（commit `46a7f68`，MIT，见其中 `LICENSE` 与 `SOURCE.md`）复制了 Cordis 核心及其原许可证、`ui-slots` 和 `ui-renderer` 的浏览器端完整源码，以及 `ui-primitives`、`store`、`workspace-path`、主题样式、语言包、ui-chat / ui-tool / ui-conversation 的 CSS 模块和 `ReasoningRow`。Vite 从本地 Cordis 源码构建；TypeScript 使用固定版本 `@deepseek-ai/cordis@4.0.4` 的公开类型。`frontend/src/cordis/` 启动 Cordis 与原版 UI renderer，注册 Rust 任务 API 服务、页面 root，以及侧栏、聊天、轨迹、输入框子插槽。`frontend/src/components/` 是沿用 DSH 样式的 React 页面组件，数据经 HTTP/SSE 对接 Rust。DSH 的动态插件加载器、Remote RPC 和完整业务 UI 插件仍未移植。

设置页「子代理」开关默认关闭。启用后，主代理可自行调用一次 `spawn_subagent`，子代理默认使用与主代理相同的提供商和模型，也可选配单独的模型路由；子代理复用本项目文件工具、代码索引和记忆系统，不能继续派生。父子任务各自保存事件，页面可从委派结果打开子任务轨迹。取消父任务时也会取消正在运行的子任务。

设置页（`/agent/settings`）和旧任务页仍是 `web/` 下的内嵌页面，使用 DeepSeek Harness 的基础主题与配色文件；来源和许可证见 [`web/vendor/README.md`](./web/vendor/README.md)。它们的浏览器内置插件由 `web/browser-runtime.js` 管理，设置页的「插件树」同时显示浏览器和 Rust 内置插件。

### Rust Agent Tasks

AI Proxy（默认 `127.0.0.1:3001`）新增独立任务接口，不依赖 Codex 的 `/v1/responses` 入口：

- `POST /agent/tasks`：提交 `{ "prompt": "...", "model": "...", "max_steps": 100 }`，返回 `task_id`。
- `POST /agent/sessions`：创建空会话（状态 `draft`），返回 `task_id`；第一条消息经下面的 messages 接口发送，并成为会话标题。
- `POST /agent/tasks/{task_id}/messages`：向空会话或已结束的主任务发送 `{ "prompt": "...", "max_steps": 100 }`，继续同一任务的下一轮；并发重复提交返回 409。主任务每轮默认最多 100 步；子代理仍为 12 步。
- `GET /agent/tasks?limit=50`：读取最近任务，供会话列表和历史恢复使用。
- `GET /agent/tasks/{task_id}`：读取任务状态。
- `GET /agent/tasks/{task_id}/events`：读取持久化的完整事件历史。
- `GET /agent/tasks/{task_id}/stream?since=<seq>`：通过 SSE 订阅指定序号之后的事件；重连也识别 `Last-Event-ID`。事件载荷使用 dsh SessionEvent 的 `type/seq/time/data/surfaceOp` 结构。
- `DELETE /agent/tasks/{task_id}`：取消运行中的任务。
- `POST /agent/tasks/{task_id}/flow/nodes/{node_id}/interrupt`：中断当前活动节点并结束本轮，任务进入可继续的 `interrupted` 状态；`node_id` 使用 Worker 计划里的节点 ID 加轮次前缀（例如 `turn_2:inspect_shapes`）。

事件使用 dsh 的 `turn/start`、`user/message`、`step/start`、`assistant/delta`、`assistant/message`、`tool/call`、`tool/result`、`step/end` 和 `turn/end` 类型；Flow 从任务分配、交接和普通工具调用读取过程记录，旧版 `worker/progress` 历史仍兼容，委派时还记录 `subagent/start`、`subagent/end`。Observer 另记录计划/进度观察、历史咨询和任务结束复盘事件；复盘结论作为普通聊天消息显示并保存在会话历史中。Agent 顺序调用本地文件、索引和记忆工具，并提供工作区内的原生 `run_program`（仅 cargo、git、node、python，argv 参数数组，无 shell），以及独立的 loopback HTTP 探测。历史保存在工作区 `.codex-workspace-mcp/codex_state.db` 中。模型文字增量、最终回复、交接和普通工具调用均记录为事件；续聊从事件重建历史。Flow 图由 Worker 当前计划和执行轨迹生成。旧轮次中断的工具调用会补一条中断结果。

```powershell
cargo run
```

默认监听：

```text
http://127.0.0.1:3000/mcp
```

可以通过环境变量覆盖：

- `WORKSPACE_ROOT`
- `MCP_BIND`
- `AGENT_BROWSER_PATH`：可选，指定 Chrome、Edge 或 Chromium 可执行文件的路径。未设置时，Windows 检查用户及系统安装目录，macOS 检查 `/Applications` 和 `~/Applications`，Linux 检查常用命令及安装路径；各平台均检查 PATH。Unix 路径必须具有执行权限。显式路径无效时直接报告未找到，不自动换用其他浏览器。

## 设计原则

- 优先使用专用结构化工具；确需运行已安装的开发 CLI 时使用 allowlist 原生程序工具，不提供通用 shell fallback
- 优先借当前上下文和上游原生能力，不急着维护代理状态
- 能文本化沉淀的结果就文本化，避免长期保存一次性资源
- 工具调用历史要尽量自愈，不能让异常历史拖垮后续请求
- 配置保持轻量，provider 能力通过 provider 本身声明

## 相关文档

- [AI_STATELESS_CONTEXT_DESIGN_LESSON.md](./AI_STATELESS_CONTEXT_DESIGN_LESSON.md)：一次多模态状态设计复盘，记录为什么要移除长期图片映射。


### Agent 文本搜索与读取去重

`search_text` 默认按普通字符串搜索。正则需显式指定，例如：

```json
{"query":"loadPptx|loadVirtualDocument","regex":true,"paths":["src/index.ts"],"max_matches":20}
```

正则逐行匹配，沿用 `case_sensitive`；非法表达式返回错误。普通模式中的 `|` 不表示“或”，结果会提示使用正则模式。

Worker 每轮共用文件、行范围、四种语言符号读取的内容哈希与已读范围。重叠部分通过 `read_coverage.previously_read_ranges` 标明，默认只返回新增行。大块源码按完整行缩减，未返回的范围明确列在 `not_returned_ranges` 中，且不会算作已读；无需逐页穷举。文件内容变化自动失效，文件写入或外部命令后也会清理覆盖记录。需要原文用于修改，或原文已经离开当前上下文时，可指定 `force_read: true` 和具体 `reread_reason`。这些参数属于 Worker 工具层，直接 MCP 文件读取仍返回原始完整结果。零散 `new_lines` 不应当作完整函数替换。


### Worker 工作记录与 Observer 消息

普通说明、方法及完整结果、失败原因、能力限制和阶段交接就是工作记录，不再提供额外的 `report_progress` 工具。Worker 的当前调用通过原始聊天消息继续；Organizer、Observer 和历史查询读取同一份按顺序保存的记录。需要旧信息时调用 `read_session_history` 或按需读取已有材料，不额外生成一份进度文档。

视觉输入的实际交付结果在请求发出前作为原始消息保存，并保留在 Worker 后续上下文中。截图捕获与图像输入交付分别记录；模型能力未知、没有备用服务等原因完整可读，宿主不根据这些事实替 AI 决定是否需要再次查看。Observer 意见直接进入后续上下文，无须专门回执，也不审批 Worker 的行动。

### 每轮文件改动、Diff 与撤销

Agent 的 `write_file`、`replace_range`、`edit_file` 保存真实修改前后内容；`run_program` 对工作区文件进行操作前后的对比。同一文件的多次操作合并为本轮净变化，不根据调用参数或失败状态伪造统计。聊天和右侧面板消费 `workspace/changes`，使用原版 DSH `DiffBlock` 展示对比。详情支持文件切换、复制补丁、折叠、关闭按钮和 Escape。对应接口为 `GET /agent/tasks/{id}/changes/{turn}`、`GET /agent/tasks/{id}/changes/{turn}/files/{index}` 和 `POST /agent/tasks/{id}/changes/{turn}/undo`。

撤销需用户点击确认；运行中任务存在时拒绝撤销。操作前检查所有文件仍与记录的修改后版本一致，发现后续编辑时返回冲突，不强行覆盖；本轮新建的文件只删除该文件，本轮删除的文件恢复原内容。撤销同步更新界面与 Worker 工作记录。快照保存在工作区 SQLite 中；功能启用前的历史任务没有完整快照，无法补出准确 Diff 或撤销。

命令跟踪仅覆盖当前工作区，跳过 `.git`、Agent 状态目录、`node_modules`、`target`、`dist`、`build`、`.venv` 和 `__pycache__`，不跟随链接。单文件内容快照上限 2 MiB，每次扫描内容预算 32 MiB、文件数量上限 20,000。二进制文件不显示文本 Diff；内容快照不完整、范围无法安全跟踪或有外部编辑混入时禁用整轮撤销，并展示原因。大型文本超出精确行对比预算时明确标为粗略对比。撤销只恢复文件内容，不恢复命令的数据库、网络或其他外部副作用。

### 模型请求上下文调试

任务流顶栏的「调试：开/关」控制当前会话后续请求的记录，默认关闭，设置保存在工作区数据库中。开启后，节点抽屉显示请求列表；「全部请求」同时包含规划前的未分配节点请求及 Observer 复盘。归属是请求发起时的节点，不把本次响应中新规划的节点当成请求前已知节点。关闭后停止记录并隐藏查看入口，已有日志保留；关闭期间和功能上线前的请求无法补回。

Worker 和 Observer 在发送前记录最终 JSON 请求体，包含实际消息顺序、系统指令、原始交接、观察者意见和工具定义，不记录请求头、鉴权凭据或供应商 URL。正文未经日志模块二次裁剪，单独保存在 `.codex-workspace-mcp/codex_state.db` 的 `agent_request_contexts` 中；普通 SSE 仅通知日志 ID，不携带完整正文，也不将日志重新送入模型。日志写入失败不阻断任务，服务日志报告失败。

查看界面支持按轮次、步骤、角色和节点选择，展开原始消息、复制请求、加载更早请求，以及与上一条同角色请求比较新增/变更和移除消息。Worker 元数据明确原始历史窗口的移除、单条截短和当前限制，摘要替代历史不等于完整原文仍在上下文中。字符/字节量不伪装为 token 估算；仅在供应商返回时保留 usage。请求结果区记录 Worker 等待响应头、首个有效流输出、完整流响应及模型调用耗时，可与轨迹中的工具耗时区分。Observer 请求中断/超时、模型错误及正常完成有独立状态；进程被直接杀死时尚未结束的请求仍可能保留为请求中。

### 任务记事本与按需源码材料

任务记事本在 RAM 中保留材料索引，在当前工作区 SQLite 的 `agent_notebook_materials` 中保存源码材料，不依赖 Observer 或上下文调试开关。成功的文件、范围、符号读取在覆盖去重和输出裁剪之前自动保存；同内容、同文件版本、同范围复用材料 ID。文件写入后自动保存当前文件（自动快照上限 16 MiB），工具结果返回对应材料 ID。保存失败保留原工具结果并报告捕获错误，不假装材料已存。已有任务结论可继续使用，功能上线前的源码不会凭空补全。

新一轮从当前目标、相关结论、材料位置和上一动作的页面指针开始，不自动加载旧源码。Worker 决定具体操作后，只取该操作需要的最小范围；同一动作保留选中页面。近期原始工具批次用于理解新结果，已选择的正文只注入一份。任务树切换保存离开节点的页面选择，并恢复目标节点自己的选择；新子节点只通过 carry_material_ids 显式继承必要材料。完整材料始终在记事本中，返回父节点不必重新搜索文件。

`recall_work` 默认按 query 查询结论和材料索引；需要操作时可直接传 `material_ids`、`include_material:true` 与 `start_line/end_line`，无需先发一次索引请求。取回单次字符预算默认、最大均为 60000，没有三份数量限制，未返回项列在 `deferred`，完整行分页返回 `complete/next_start_line`。单次请求的源码合计上限为 100000 字符，当前动作保留页面最多 80000 字符，不应填满：用 `action_id` 标记具体操作（仅改编号不会清空材料），`source_material_ids` 缩小已有选择，或 `replace_context:true` 换成精确请求的页面。超预算新增页面不挤掉选中代码，工具返回未激活原因；完整材料仍在记事本。

核验使用当前文件内容哈希。文件变化后，完整文件快照视为旧版本；范围/符号材料若在当前文件中仍有唯一一致的代码片段则自动更新行号、继续复用。片段变化、多个匹配或文件消失时明确返回 changed/ambiguous/missing，不能当作当前源码，需定向刷新。关于改变片段的结论标记待核对，关于未改变片段的结论可保留。材料取回属于调查，不会重置实际执行停滞计数。

任务流调试抽屉提供「任务记事本」表，可查看结论、材料 ID、文件/符号、行范围与所属节点，点击材料会核验版本并按完整行读取。API 为 `GET /agent/tasks/{id}/notebook` 和 `GET /agent/tasks/{id}/notebook/materials/{material_id}`，后者支持 start_line/end_line。Observer 使用同一套结论与材料索引，历史对话仅提供少量结论，不反复注入整段旧轨迹；同节点的 sufficiency 建议保持一个意见 ID。长期项目记忆仍只保存复盘中有价值的事实与经验，不自动灌入整本源码材料。

### 写入工具的前置校验与返回值

结构化写入（`write_file`、`replace_range`、`edit_file`）统一限制在选定的工作区，绝对路径也必须位于该目录内；父级跳转及符号链接/junction 指向工作区外的路径会拒绝。完全访问允许工作区项目工具和 allowlist 原生程序执行；原生程序必须在工作区目录内运行，直接参数传递且不启动 shell。结构化文件工具仍保留路径边界。

- `write_file`：新建文件无需版本；覆盖已有文件必须传 `expected_code_hash`，从源码读取、记事本取回或上一次写入结果复制。
- `replace_range`：行号从 1 开始，首尾包含；必须提供 `expected_old_text` 或 `expected_code_hash`。未修改部分保持字节不变，替换区沿用换行格式，不裁掉额外末尾空行。
- `edit_file`：单文件一次接受 1–100 个 `{old_text,new_text}`；每个旧文本必须唯一匹配原始文件，范围不可重叠。全部校验后一次落盘；任一失败均不提交。插入时在 `new_text` 中保留原锚点，删除时 `new_text` 为空。可额外指定 `expected_code_hash` 检查整文件版本。LF 锚点支持匹配 CRLF 源码。
- 写入使用进程内统一锁、唯一临时文件和 `create_new`，保留原文件权限，提交前重新检查原内容；新文件通过不覆盖现有目标的方式发布。失败会清理本次临时文件。对外部程序的同时修改采用乐观检查，不提供跨进程锁保证。
- 返回 `changed`、`code_hash`、`bytes_written`、`index_refresh`，整文件工具还返回 `created`。`changed=false` 不重写文件、不刷新索引，也不计入成功修改次数。版本/旧文本冲突并非权限拒绝，只需取回相关材料后修正参数。
- 已初始化的索引只刷新被修改文件，其他文件留到后续正常索引查询检查；旧跟踪版本可能执行一次迁移扫描。写入及索引工作放到 blocking 线程；已开始的文件提交会完成并记录快照后再结束取消。索引失败明确返回，但不回滚已成功的写入，不应因此重复修改文件。兼容字段 `go_reindexed` 仅表示 Go 刷新成功，其他语言读取 `index_refresh`。

示例（两处编辑一次提交）：

```json
{
  "path": "src/editor.ts",
  "expected_code_hash": "复制已有读取结果中的 code_hash",
  "edits": [
    {"old_text": "const enabled = false;", "new_text": "const enabled = true;"},
    {"old_text": "function removeNode() {}", "new_text": "function removeNode() { deleteSelectedNode(); }"}
  ]
}
```

新工具已接入 Worker 工具列表、权限判断、Flow 动作统计、真实文件变更/撤销和任务记事本。

### 源码工作集、结论时间线与执行恢复

- 记事本完整保存已发现材料，原始工具返回保留在模型会话中，不再二次生成源码节选或重复注入工作集。Worker 可按材料 ID 与行范围主动读取需要的片段。`action_id` 标记具体动作而不隐式丢弃材料；`source_material_ids` 缩小已有选择，`recall_work replace_context=true` 更换页面。节点内汇报保留页面，任务树切换保存和恢复各节点的独立选择。100000 字符是单次请求源码的合计上限，保留页面最多 80000 字符，不是需要填满的目标；超预算新增页面明确标记未激活，不挤掉已有待修改代码。完整材料始终留在记事本，取回没有三份数量限制。工作集描述保存在任务事件中，并在使用前检查当前版本。调试请求显示当前动作、选中页面和未激活原因。
- 材料取回与工作集不再限制为 3 份；`recall_work` 根据字符预算返回显式指定的材料，未返回项放在 `deferred`，长内容继续使用分页。近期搜索结果与有效的进度工具参数也保留，避免重新搜索位置或模仿缺少必填字段的历史调用。
- 结论使用稳定的 `id` 与精确的 `topic`。`conflicts_with` 声明冲突；同一 topic 的不同声明也会进入待确认状态。确认后填写 `supersedes` 和 `verification`（当前源码材料 ID 与确认说明，或当前用户原文），运行时核对来源是否可用，再将被替代声明标记为历史。未经确认的冲突修改不能直接覆盖同一 ID 的已确认声明。普通无冲突结论尊重 Worker 的状态，不因未填写额外确认字段自动降成假设；来源变化或冲突的结论仍需要确认。
- `worker/finding_revision` 独立保存修订、发现冲突、确认、来源变化和替代事件，包含时间、轮次、步骤、文件版本和来源范围。旧会话尽量从原汇报事件补回记录时间，无法证明确认来源的旧声明单列待核对。正常上下文只带当前结论及待处理冲突；`recall_work include_history=true` 或 `/agent/tasks/{id}/notebook/history?before={seq}` 按需分页查询历史。前端任务记事本分别展示当前、冲突、历史和时间线。
- 修改汇报文字、计划或结论不再清零持续调查计数；材料取回属于辅助调查，`changed=false` 的写入不记作实际改动。连续两轮只有协调动作，或持续调查达到阈值时，运行时暂时收起 Observer 咨询并拒绝纯汇报批次；汇报可以与实际读取、修改或命令同批执行，用于保存新结论和清除已回答问题。汇报及结论改写仍不清零调查计数；不强迫只读任务写文件，也不猜测未读源码。

### 源码读取、分页与历史查询取回

- `read_file`、`read_file_lines`、`read_*_symbol` 的源码输出按完整行分页：默认 `max_chars=24000`，上限 60000，返回实际 `start_line/end_line`、`complete`、`next_start_line`。`complete` 指请求范围已返回；文件/定义总范围另见材料描述。源码内容保留 CRLF、LF、BOM 和末尾换行。
- 按行读取起点越过 EOF 会报错；终点越过 EOF 会截到实际末尾，并返回 `total_lines`、`requested_end_line`、`end_clamped_to_eof`。空文件通过 `read_file` 返回空内容，不虚构一行。
- 单行超过分页预算时显式返回 `partial_line=true`、`start_column/next_column`；下一次同时传 `start_line=next_start_line`、`start_column=next_column`。列偏移按 Unicode 字符计数，包含行分隔符；片段不是完整定义。记事本接口和调试按钮也支持长行续读。
- 底层读取使用元数据版本快照缓存（大小、修改时间、创建时间），最多 128 个文件和 64 MiB 内容/行索引，LRU 淘汰。单文件硬上限 16 MiB，同时检查实际读取字节数，避免文件增长绕过限制；源码读取放到 blocking 线程。元数据变化会重新读取；若外部程序修改内容并刻意保留相同元数据，需 `force_read=true` 绕过缓存。文件写入仍独立核验真实内容。
- 读取后保存记事本、同版本取回材料、每轮校验固定材料共用快照，避免反复读取相同文件。同版本取回只读材料描述，不重复加载 SQLite 中的大段原文；文件变化需要重新定位时才读取历史正文。记事本保存完整请求材料，显示页大小不改变保存范围。旧材料取回也从当前文件的原始字节范围返回，不再拼接换行。
- 符号读取检查索引中保存的文件内容 hash 与读取快照；不一致时定向刷新并重新定位一次，仍不一致则返回版本冲突，不用旧行号包装新源码。已初始化索引的已知文件/符号读取不再例行扫描全仓库。
- 重复源码页仅在完整材料已成功保存时省略正文；部分重叠仍是连续源码。修改某文件仅失效该文件的覆盖记录，普通命令会清空读取缓存/覆盖。覆盖标记表示工具返回范围，不表示模型已经理解。最近原文上下文另有 60000 字符总预算，省略时保留材料 ID 和取回提示。
- 目录、搜索、索引和记忆查询重新检查当前状态，不直接复用过去的结果；`repeated_query` 仅标记新结果与旧结果相同。工作记录保存最多 64 个查询索引，当前节点上下文带最近 4 个，用 `recall_work query` 搜索历史索引。
- `recall_work tool_call_ids` 可读取任务自己的原始工具结果；大结果用 `result_field`（字段名或 JSON pointer）与 `result_offset` 分页，读取返回的 `next_offset`。这些结果明确标记 `historical=true`。调试记事本显示历史查询索引。
- 超大工具结果不会硬切断 JSON：返回有效结构、明确截断标记、材料 ID 或工具调用 ID。原始结果保留在任务事件中，可按以上方式取回。


### 按功能定位代码（单个定义、文件、相关功能组）

任务的信息入口为**项目内会话历史 + 代码索引**。默认请求提供当前任务、前一步结果和精简 Flow；缺少具体历史信息时再查询，缺少代码位置时按职责描述定位。记忆系统保留为独立能力，不作为例行任务记录或启动时必查的入口。

256k 是上下文视野的最大范围，不是固定填充大小。新角色消息、失败状态和原始诊断完整传递，Worker 不固定丢弃两步以前的消息。旧任务保留在历史中，不自动给新任务附带旧任务摘要；当前信息不足或达到容量时，以当前任务和最新完整消息为基础，角色按需查询所缺的历史，不继续搬入一个满载的历史窗口。不同模型没有共用的分词器，发送前采用保守 token 估算并预留输出空间；源码页面仍使用独立缓存和按需选择。Observer 在任务结束后把过程得失与改进建议展示在聊天中，例行复盘不自动写入工作记忆。

Worker、Organizer、Observer 共用 `read_session_history`，直接读取当前工作区已有的会话事件，不新增历史数据库：

同一个 Worker 通过本任务原始工具调用和返回接续执行，工作包不再重复注入自己的 `actual_operations`。下一任务接收一次上游交接，包含已完成操作的返回结论、导出数据及失败和限制；原始返回字段合入同一记录。若前一任务已在 `upstream_outputs` 中交付，`previous_step` 只引用它，不再发送第二份结果。完整工具过程仍保存在会话历史，需要核查时按需读取。

结束复盘提供当前主任务的完整时间线，包括角色原始消息、工具调用与返回、失败恢复和此前 Observer 意见；跨轮续任务仍属于同一主任务。节点摘要只用于定位阶段。超过 256k 容量时明确标记未读过程，通过历史引用继续读取，分页保留 `after_seq` 和 `request_id`；不截短新消息，也不混入被替换主任务的迟到意见。API 的 `completed` 表示执行结束，`turn/end` 同时传递 Organizer 判断的 `goal_achieved` 和 `unresolved`；聊天区分别展示执行状态、目标达成情况及未确认事项。

- 默认读当前会话，可按 `query`、`role`、`node_id`、`turn` 筛选；记录保留原文、角色和采样时间，按事件顺序返回。
- `scope=project` 查找同项目的相关会话，每个会话返回一条最近匹配记录、简短需求和 `read_reference`。只查当前工作区数据库，不跨项目。
- 使用返回的 `task_id` 读取选定会话；`event_seq` 定位原始记录，`char_offset` 分页读取长内容。事件序号只在所属会话内有效。
- 会话内向前翻页用 `next_before_seq`；项目检索翻页用 `next_search`。每次正文最多 12000 字符，不将项目全部历史注入模型。历史观察不代表当前服务状态。

`search_code_map` 用任务描述（例如“节点拖拽”“图片渲染”）查询 Rust、TypeScript/JavaScript、Python、Go，返回功能组的职责说明和可直接读取的定义入口。分组来自保存的 `belongs_to_area`、架构记忆里的 `key_files`，或源文件边界；不根据调用链推断功能，也不根据函数名伪造说明。一个文件属于多个已匹配功能区时，不猜测它唯一属于哪一组。每组返回少量入口，候选和分组各自带分页信息，候选窗口最多每语言 100 个。

理解一个函数后，可用 `record_symbol_business_context` 保存职责、关键词和 `read_when`；`scope=file` 保存文件级说明，`belongs_to_area` 标识相关功能组。这些描述由 Worker 或 Observer 基于已读代码积累，不要求额外遍历项目。源码注释会自动成为初始描述。已有架构记忆的分组信息仍兼容读取；`record_architecture_memory` 保留供单独的记忆工作使用，不要求普通任务写入。没有描述时明确返回 `missing`；旧架构组说明返回 `saved_requires_confirmation`，并不声称当前实现已验证。

函数职责保存独立 `definition_hash`，未改动的函数可在同文件其他位置变化或行号移动后继续复用；文件级说明仍按整文件版本失效。`code_hash` 继续表示整文件版本，兼容写入前置条件。旧描述没有函数版本时，只有文件版本仍匹配才能建立函数版本，否则保留过期状态。

符号查询按文件/目录范围刷新；初始化仍建立该语言索引，无范围的连续查询共用 2 秒扫描窗口。显式索引刷新不受这个窗口限制，读取定义仍核对当前源码哈希。职责目录只加载候选文件，排名仅排序需要的前缀，同时保留准确匹配数量。中文任务词提供有限词表和中英文别名扩展；`all` / `phrase` 保持字面匹配。这不是通用中文语义模型。索引扫描/查询在阻塞线程执行，避免占用异步服务执行线程。

旧的目的、下一步和问题保存在记事本、汇报事件和过程界面中，不再重复注入后续 Worker 状态消息；上下文提供当前目标、结论、材料与 `recent_operations` 实际操作，由 Worker 判断下一步。改变动作编号不清空源码，只有明确的页面选择或替换才会调整注入内容。

新工具批次共享 100000 字符源码总额度中的剩余空间，至少保留 20000 字符的源码观察窗口：若新材料尚未进入保留工作集，仍先提供实际正文给 Worker 阅读；已保留页面不重复发送。旧批次未选中的正文转为可取回引用。保留页面和新读取窗口共享总额度，预算分别记录，超出观察窗口的片段明确标记省略/分页。保留工作集满时，不会把新工具结果在第一次被阅读之前全部裁成 ID。


### Focused editing reads and request projection

`read_ts_symbol` accepts `include_related_types` and `include_outline` (both opt-in).
Related reads return at most eight local interface/type/enum definitions and 12000
source characters across two reference levels. Parent/sibling outlines contain
positions and short signatures, not whole class bodies. Relative named/default
imports are supported; package aliases, namespace imports, re-exports and compiler
binding resolution are not inferred. Each returned type is saved in the task notebook and its original method result
remains in the Worker conversation. The host does not build a second source projection.

Worker requests retain original messages for the current invocation. Earlier task
returns arrive through one handoff; missing history and source material can be
read explicitly. The host does not select findings by task keywords.
The new request_sizes debug metadata separates message, system, work-state and
tool-schema character counts. These are character counts, not estimated tokens.

A source-budget rejection refers only to a model request, not notebook storage.
The combined source ceiling is 100000 characters. Retained pages are capped at
80000; fresh reads use the unused total capacity, reserving at least 20000.
The Worker extracts findings and selects only needed definitions/ranges. Read and
recall defaults remain unchanged; the larger ceiling is not a request to load all materials. An over-budget page remains stored; it does not evict existing edit pages.
Independent prerequisite reads/checks should share a tool batch. Verification
failures from missing dependencies/generated modules must be reported separately
from whether the implementation passed; missing imports do not prove correctness.

工具列表按用户权限和用户明确禁止的操作过滤，Worker 自行选择工具及调用顺序。
任务的 `checks`、`verification_due`、`completion` 和 `edit_targets` 用于描述目标及记录事实，
不会隐藏工具或阻止浏览器、读写文件与检查在同一任务中执行。任务返回后，宿主停止该次调用的后续操作。

### 可选递归任务树与节点上下文

复杂任务的 `plan.mode=tree` 有且只有一个根节点。节点包含 `parent_id`、`objective`、`done_when` 和 `constraints`；Worker 发现真实子问题后才扩展树。`node_result` 明确记录完成、受阻或跳过的实际结果；父节点不能在子问题尚未处理时完成。根节点明确完成后进入最终总结，不再继续查询。

请求只携带当前节点的相关结论、选中源码、祖先目标/约束和子问题的精简结果。其他分支保留在记事本和树快照中；`recall_work tree_node_ids` 可以按 ID 取回完整节点结果与材料指针。当前节点先筛选结论再应用条数限制，兄弟节点不会挤掉当前节点记录。工具结果按实际执行节点归属，自动返回父节点和无效汇报不会错误转移结果。

`flow/tree_state` 持久化结构、实际状态和源码页面指针，不存重复正文。后续同目标任务可显式 `resume_tree=true` 恢复；新任务不自动接管旧树。结束文字不自动将未完成节点标成完成。前端显示父子层次、当前路径、目标/完成条件、约束与返回结果；简单任务显示直接处理，调试模式仍可查看模型请求和记事本。Observer 独立提供意见与复盘，Organizer 根据实际返回决定节点派发与整体结束，Worker 执行当前任务并交付结果。

直接处理是暂时的组织选择。发现各自有目标与完成条件、结果又互相依赖的子问题时，Worker 应在下一次实际工作汇报中升级为任务树，简述 work_organization 的原因并同批创建树；无需单独分类、重启调查或补造已完成节点。功能名为“最小”或仅改一个文件不能替代依赖判断；调用次数、耗时和普通报错也不作为强制启用阈值。决策保存在工作记录和调试请求中，恢复提示同样允许真实子问题拆解。

### 角色消息与工具上下文

Organizer 读取按原始时间顺序传来的方法调用、分配回执、Worker 原始返回、Observer 消息和请求失败。Worker 保留本节点的完整调用参数与工具返回；不再把这些消息加工成另一份事实摘要或源码节选。Observer 日常观察是临时 AI 调用：每个 Worker 执行迭代结束后只触发一次，按原始顺序接收该迭代完整消息，不累计此前观察的输入和回复；缺少相关信息时主动读取历史。节点创建本身不调用模型，重复唤醒不产生新观察。结束复盘另行读取当前任务的完整过程，包括请求超时与恢复。独立只读 Organizer 查询可以同轮返回多个结果。

发给模型的历史描述保留方法名、参数、完整结果、失败状态和来源，不重复投递事件 JSON 外壳；原始业务结构化数据和 API 工具调用协议仍然保留。只有达到 256k 视野容量时才移出旧消息，角色按需回读原会话，不自动填满窗口。截图不自动触发附图或 Observer 截图；Worker 显式调用 `view_image` 才请求图像输入，能力不足的原始返回仍正常传递。
