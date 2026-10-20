# codex-workspace-mcp

由 Rust 执行任务的本地 Agent，使用基于 DeepSeek Harness（DSH）的 React/Cordis 前端。主 Agent 可派发最多 5 个并发任务子 Agent，并使用独立的上下文压缩子 Agent；模型直接连接提供商的 Chat Completions 接口。

## 核心能力

- 文件读取、文本搜索、版本校验写入与编辑。
- Go、Rust、Python、TypeScript/JavaScript 符号索引和职责记忆。
- 受限原生程序执行、项目依赖与脚本、进程日志、HTTP 探测及浏览器操作。
- 图片、文档附件、视觉分析、持久资产引用与上下文压缩。
- Rust 插件作用域、技能包、独立进程工具、插件页面与 PPT 会话。

内核按作用域树挂载工作区、模型设置、工具目录和任务服务。依赖缺失时拒绝启动；卸载按子节点优先与注册逆序清理。`GET /agent/plugins` 返回服务、依赖及生命周期事件。

### 主 Agent、任务协作与上下文子 Agent

`/agent` 的新任务和续聊使用单一主 Agent、一份持久工作上下文。主 Agent 自己计划、调用业务工具和回答；执行路径不再经过 Organizer、Worker 交接或 Observer。Organizer／Worker 的执行、调度、任务树和交接实现已删除。Observer 的复盘实现与配置暂时保留，不接入当前执行循环；历史原始事件仍可查询。

主 Agent 可以按需调用 `spawn_agent` 派发独立任务，每个会话最多 5 个任务子 Agent 并发，压缩不占这些名额。子 Agent 使用独立工作窗口，继承当前模型、执行权限和插件/项目绑定，不能继续派生子 Agent。父窗口较小时整窗继承；较大时宿主投影为需求/计划/回执叙述与资产引用，正文按需读取，不额外调用模型整理。主 Agent 继续独立工作、接收报告并验证实际产物，以 `review_agent_result` 记录验收；未收齐或未验收时最终回复暂不交付。原始历史按实际事件顺序保存，聊天卡片固定在派发位置，每个卡片展开自身过程。详情见[任务协作与历史排序](docs/task_delegation.md)。

Git 仓库中的可写任务子 Agent 使用独立 worktree 和 `codex/delegate-*` 分支；主 Agent 查看 Diff、显式合并，再引用实际工具回执验收。会话状态保存在主工作区。运行中的子任务通过持久化租约区分其他宿主进程。

主 Agent 调用 `compress_context` 默认也先整理资产；显式 `deep=true` 时，启动干净的临时上下文子 Agent 调查缺失历史证据。两者复用同一套请求、工具调用、日志和容量检查循环。子 Agent 只可读本任务历史、读取冻结上下文、查看图片、保存阶段结果和提交压缩结果，不能执行业务操作或启动另一个子 Agent。

子 Agent 按时间读取本任务原始用户要求及后续修改，分段读取当前上下文全文，按需检索历史。它自己提取原始目的、当前目标、修改关系、约束、进度、未解决事项和下一步，并把已完成工具交换整理成有来源引用的文本。视觉结论要求它自己调用 `view_image`，且实际图片已进入后续模型请求。宿主只能检查图片交付记录，不能证明模型理解正确。

每次子 Agent 调用最多 64 次模型请求；上下文变大时可调用 `checkpoint_context` 原样保存阶段结论和后续阅读计划，换成自己的小窗口继续。该方法不会再启动子 Agent。达到轮数上限或压缩失败时，主 Agent 上下文不替换，部分工作仍保存。

自动容量整理优先处理全部资产：宿主提供当前对话与资产目录，AI 必须逐项说明每个已加载资产保留正文或仅留引用的原因，不完整的计划允许修正一次，仍不完整则不执行。当前正在编辑的代码/SVG、马上要检查的图片与未解决的证据按下一步需要保留；已完成页面、旧预览和已消费资料尽量一次释放。同一资产的相同正文只留一份，独立分页仍保留。资产整理后，历史对话仍有明显体积（估算 8192 token）则继续生成带来源引用的任务摘要，移出旧思考和完成的工具交换，不因刚释放一点空间就结束。自动摘要只提供 submit_context，原始用户要求已经完整交付，不再绕行历史读取；提交被拒绝时最多修正两次。原始历史及资产来源始终保留。

恢复旧会话时按上下文策略版本重新设置触发阈值，避免旧压缩失败留下的退避状态跳过新资产整理。新的失败退避只约束当前用户轮次，下一轮用户输入重新评估；成功整理留下的容量余量跨轮保留。迁移记录为 `context/policy_upgrade`，不改写对话和项目绑定。

资产整理后仍超过软阈值，才进行文字整理：原始用户要求和隐藏资产正文后的完整对话直接交付，无须逐段重复读取，自动调用最多 3 次模型请求、每次最多 4096 输出 tokens。手动 `compress_context(deep=true)` 仍可执行上面的完整历史调查流程。资产/文字整理分别记录 `context/asset_review_start`、`context/asset_review_end` 与 `context/compression_*` 事件，记录选择理由、前后容量与失败原因。

主 Agent、上下文整理和视觉分析共用进展超时策略：连接上限 30 秒，等待响应头及首个有效输出分别为 180 秒；开始输出后，每个正文、思考或工具参数增量都会刷新 180 秒空闲计时。持续输出不受原来的 180/300 秒总时限中断，SSE 心跳和重复空元数据不刷新计时；用户取消立即生效。普通 JSON 回包按响应体增长判断空闲，输出 tokens/响应大小限制仍有效。视觉工具使用流式请求，但分析结果仍作为完整工具结果交付，不混入主 Agent 聊天增量。

插件返回和大段写文件参数也纳入资产目录。PPT 后端持有的 `svg_output/*.svg` 仅保存远程资源链接、项目/路径和内容指纹，宿主不额外创建正文快照文件；原始工具交换仍完整保存在审计历史中。重读通过当前登录的插件连接，校验项目归属和内容指纹；链接内容变化或登录失效会明确报错，不把新版冒充旧稿。

主 Agent 和压缩子 Agent 的每次实际模型请求都生成唯一 Flow 节点，子 Agent 节点关联发起它的主 Agent 请求。请求全文在发送前保存；响应、状态和节点完成状态一起保存，成功保存后才执行模型返回的工具调用。上下文面板可查每次实际输入、完整输出和节点关系。

## 日志策略

模型传输诊断由 `model_transport` 生成，通过 `proxy_log` 写入 `logs/model-requests-YYYY-MM-DD.jsonl`。

诊断按请求 ID 记录发送、响应头、首字节、解码和失败阶段，不写认证头或对话正文。字段与排查方法见[模型请求阶段日志](docs/model_request_diagnostics.md)。模型的完整请求和响应另存于任务历史与请求上下文中。

## 上下文策略

原始对话、原生工具调用和返回始终完整保存在 `agent_task_events`。子 Agent 的压缩结果、阶段结果也作为新事件保存，原文不被宿主重写。实际模型输入保存在 `agent_request_contexts`，事件保留对应引用。

`agent_runtime_windows` 保存主 Agent 和临时子 Agent 的工作窗口；`agent_context_materials` 保存本次压缩冻结的阅读材料。这两类运行状态不自动作为普通对话注入。最终摘要与主 Agent 新窗口在同一事务中提交，下一轮原样恢复，包括目标和计划。历史中的旧状态快照仍可查询。

容量通知会说明正文是否真正交付。放不下的工具结果完整保存，返回精确历史引用；由 AI 决定分段读取或先压缩。宿主在每次主请求前监测输入，达到软阈值后执行一次只读压缩：资产正文全部以引用呈现，模型只接收核心对话、执行状态、原始用户要求和轻量资产索引，一次提交摘要与 `restore_assets`。宿主随后读回所选数据，自动保留当前技能规则，并校验项目权限、版本和最终容量后原子替换工作窗口。原始历史不改写，不重放任何业务工具；恢复失败会明确保留为未解决事项。若恢复技能规则或最终容量校验失败，保留原窗口。这些检查不能保证摘要的语义质量。

服务中断后，先恢复已保存的工具结果；没有结果的调用明确标记完成情况未知，不自动重复执行。旧会话首次续聊按原顺序导入带来源的历史文本，旧角色间未闭合的原生工具对不会阻断导入。旧会话全文若已超限仍会明确报错，需要另行整理迁移。

### 视觉与压缩配置

设置 → 视觉与压缩可分别指定提供商、模型、推理档位和最大上下文 tokens。每个留空项跟随当前主 Agent，未填写整个配置也沿用主 Agent；较旧客户端遗漏这些字段时保留已保存的覆盖项，显式清空则恢复继承。配置保存在 profile 的 `agent-generation.yml`，字段为 `visual_tool` 和 `context_agent`。压缩子 Agent 使用独立窗口预算，并预留 24,576 tokens；例如 500k 最大窗口对应 475,424 输入预算。结果提交仍按主窗口校验，源历史和资产交接规则不变。

模型目录中的提供商可配置 `contextRequestOptions`，以模型 ID 为键指定上下文任务的模板参数。例如 Qwen 的 `qwen3.8-27b: {chat_template_kwargs: {enable_thinking: false}}`。它仅用于资产/上下文整理，主 Agent 和视觉请求沿用自己的配置；不按模型名称猜测能力。当前只透传 `chat_template_kwargs`，不能借此覆盖模型、消息、工具或认证。

`agent-generation.yml` 中的 `plugin_generation` 可按插件配置生成默认值，例如 `ppt-master: {reasoning_effort: low, visual_reasoning_effort: low}`。它独立于 `plugin_models` 的模型授权列表，只影响对应插件。主任务的默认档位优先于旧会话保存的档位，显式请求仍可选择模型支持的其他档位；视觉档位覆盖全局视觉默认值。实际请求发送 `reasoning_effort`，模型支持的档位由模型目录配置，宿主不根据名称猜测。未配置的插件维持原默认值。

### 图像分析工具

PPT 页级检查使用后台视觉任务：`analyze_images({artifact_ids, question, background:true})`
持久化任务后返回 `accepted/job_id`，主 Agent 继续生成后面的页。PPT 渲染截图默认走后台；
其他图片理解保留原同步行为。后台仍使用“视觉分析工具”的模型配置，只接收问题和保存的像素，
不继承对话、不修改文件，也不是第二个执行 Agent。最多并发 2 项视觉任务，每会话最多 32 项待处理。

结果在下一次主请求前以 `tool_completion` 数据交付，并关联页码、保存版本、截图指纹。
宿主读取当前项目版本，将过期结果标为 `superseded`；无法核对则标为 `unknown`。
主 Agent 应读取最新 SVG 后修正，修改后重新检查。`completed` 表示结果已返回，不等于页面通过。
生成完成后可用 `image_analysis_results({wait_ms:30000})` 等待剩余任务；有未交付检查时，
宿主暂存无工具调用的收尾回复，先收齐结果再让主 Agent 处理。取消会传给后台任务；
进程重启后的孤立任务标为 `interrupted`，不会静默重跑或算作检查完成。

主 Agent 使用 `analyze_images({artifact_ids, question})` 对保存的图片进行视觉分析，最多两张，按参数顺序传原图。工具使用设置中的“视觉分析工具”路由（应用提供方的模型映射）；未配置的项跟随主 Agent。问题和必要背景由调用 AI 提供；工具请求只有这一条用户消息与原图，不继承主窗口或历史，也没有浏览器/递归工具权限。

返回的 `analysis` 是模型原始正文，按普通原生工具返回进入主窗口，并得到稳定资产引用。宿主不改写结论、插入额外用户消息、切换主模型或修改/压缩/释放上下文。`browser_screenshot` 负责截图，`view_image` 仍只负责选图；视觉检查由主 Agent 显式调用分析工具。模型原始请求与响应保存为 `actor=tool` 的诊断记录，不创建执行角色或子 Agent 窗口。取消、超时、截断和空返回作为工具错误处理。

### 资产引用与 AI 控制的加载状态

单 Agent 的文件/符号读取、搜索结果、页面资料、截图和执行输出会得到稳定的 `asset_id`。原始工具结果仍完整保存在历史中；正文快照按内容哈希保存在操作工作区的 `.codex-workspace-mcp/assets/`，图片继续使用 `.codex-workspace-mcp/visual/` 中的原文件。SQLite 的 `agent_context_assets` 保存来源、版本、路径、范围和读取引用。相同来源、参数及内容复用同一个引用，正文快照不会被新版本覆盖。

实际工具返回同时提供原始内容、`asset_reference` 和本次 `delivery_reference`。正文持续留在工作窗口中，宿主不按时间、访问次数或相关性淘汰。AI 可以使用：

- `list_context_assets`：查看本窗口已加载的正文、片段、图片引用及仅引用状态；不重新读取文件。
- `release_assets`：AI 明确指定资产后，把正文换成引用；原生工具调用关联及历史原文保留。
- `read_asset`：用引用读取当时的完整版本，或明确指定字符分页；图片读取选中已有截图，不重拍。
- `refresh_asset`：明确重读本地文件的当前版本/原范围，返回对应的新版本引用；不重新执行命令、上传或截图。

自动压缩与默认 `compress_context` 使用 `restore_assets: [{asset_id, char_offset?, max_chars?}]`，只选择下一步需要的数据，未选择的资产只留去重后的引用。压缩模型看不到正文或像素，也没有业务工具。当前激活技能的最新指令独立恢复，不依赖摘要是否选中；权限仍按原来的轮次和作用域校验。日志分别记录压缩总耗时与 `context/assets_restored` 读回结果和耗时。

显式 `compress_context({deep:true})` 的历史调查和子 Agent 阶段保存继续兼容 `asset_context`：`keep` 保持正文，`reference` 只留引用，`excerpt` 按 `event_seqs` 保留已读取片段；片段编号来自 `read_asset` 的 `delivery_reference`。`read_asset` 的分页范围针对保存的原始结果 JSON 文本；完整读取会恢复原始结构。

读取与释放/压缩/阶段保存不得在同一轮响应中混合调用。内容超限时，只拒绝本次新正文进入窗口并返回来源和资产引用；已经加载的资产保持原样，下一次主请求前统一检查压缩阈值。容量仍使用保守估算器，并非提供商精确 tokenizer。

旧持久窗口续聊时，会为仍在窗口内、能定位到原始工具事件的材料补充引用，不重新执行读取。保存版本与当前文件版本明确区分；需要当前版本由 AI 调用 `refresh_asset`。已加载文本/片段和稳定引用随窗口持久化，进程恢复后继续使用。


## 工作区工具

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

索引从定义注释提取短摘要，主 Agent 理解关键入口后可通过 `record_symbol_business_context` 补充功能职责、关键词和关注场景。支持“文件＋限定名”、符号 ID，以及 `scope: "file"` 的模块描述。写入时传读取结果里的 `expected_code_hash`，代码变化会拒绝该次写入。描述使用确定性的 FNV-1a 内容指纹绑定版本；索引检查仍以修改时间和大小发现变化，解析时更新内容指纹。保留人工/Agent 描述，代码变化后标记待复核；注释摘要由新注释更新。首次查询会升级索引文件跟踪版本并提取现有注释。

Observer 可在原有复盘请求中返回最多四条职责描述，服务仅接受本次成功读取过的定义/文件，并校验读取时的代码版本。不额外发起模型分析请求，也不阻塞主 Agent；未描述的代码仍能通过名字、路径、注释定位。

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

## 启动与配置

```sh
cd frontend
npm install
npm run build
cd ..
cargo run
```

打开 `http://127.0.0.1:3001/agent`，在 `/agent/settings` 配置提供商、密钥与主 Agent 模型。设置页和任务页都来自 `frontend/dist`；缺少构建产物时返回 503 并提示构建。旧的 `/agent/classic` 页面、内嵌网页运行时及 `/v1/responses`、`/v1/chat/completions`、`/v1/messages`、`/v1/models` 兼容入口已删除。

前端开发运行 `npm run dev`，页面位于 `http://127.0.0.1:5173/agent/`，默认代理到 `127.0.0.1:3001`；可用 `AGENT_BACKEND` 调整代理目标。

环境变量：

- `WORKSPACE_ROOT`：任务工作区，默认启动目录。
- `AGENT_BIND`：任务服务地址，默认 `127.0.0.1:3001`。
- `MCP_BIND`：MCP 地址，默认 `127.0.0.1:3000`，入口为 `/mcp`。
- `AGENT_WEB_DIR`：前端构建产物目录，默认 `frontend/dist`。
- `DSH_HOME`、`DSH_PROFILE`：配置目录与 profile，默认 `~/.dsh` 和 `web`。
- `AGENT_BROWSER_PATH`：可选的 Chrome、Edge 或 Chromium 可执行文件路径。

模型配置写入 profile 的 `cordis.patch.yml`，密钥存于 DSH 凭据文件；生成参数、视觉分析与压缩模型覆盖配置存于 `agent-generation.yml`，Observer 配置存于 `codex-workspace-observer.yml`。任务请求会重新读取外部配置修改。

前端使用本地 Cordis 与 DSH UI renderer。来源与许可证见 [`frontend/src/dsh/SOURCE.md`](frontend/src/dsh/SOURCE.md) 和同目录的 `LICENSE`。

## 任务 API 与过程界面

`POST /agent/sessions` 创建空会话，`POST /agent/tasks/{task_id}/messages` 发送首条消息或续聊。HTTP/SSE API 提供任务列表、事件流、取消、模型请求详情、文件改动 Diff 与撤销。会话保存在工作区 `.codex-workspace-mcp/codex_state.db`。

任务事件使用 `turn/start`、`user/message`、`step/start`、`assistant/delta`、`assistant/message`、`tool/call`、`tool/result`、`step/end` 和 `turn/end`。Flow 节点对应每次真实模型请求，上下文子 Agent 通过父节点关联；历史事件查看保留来源。旧的 Worker 工作笔记接口、调度节点续跑入口和前端任务树／回溯实现已删除，已结束的会话直接发送新消息续聊。

每轮文件改动由 SQLite 保存前后快照。Diff 与撤销检查当前文件版本，撤销失败会尝试回滚已应用的文件。源码读取保留原始换行，写入工具通过 `expected_code_hash` 或精确原文校验避免覆盖已变化的内容。

## 验证

```sh
cargo test
cd frontend
npm run build
node --test tests/*.test.mjs
```

浏览器与实际模型验收中的忽略测试需要对应环境，普通测试不要求配置真实模型。

## 按功能查看代码 Flow

会话顶部 **技能页面 → 代码 Flow** 按功能分级展示当前方法调用关系。根目录 [code-flow.json](code-flow.json) 声明职责、公共入口、方法绑定和测试入口；宿主只提取源码证据，不自动判断耦合或重复实现。人和 AI 的 `read_code_flow` 工具使用同一个图读取器。首版支持 Rust，并明确保留边界、歧义和未解析调用。详见 [代码 Flow 首版说明](docs/code_flow_v1.md)。

历史数据访问已收敛到 `session_history/store.rs`：压缩读取原始要求、引用校验、恢复、资产补回和界面历史读取共用该访问层，原始事件按编号升序返回。


## 技能插件包

技能包把提示词、工具和页面放在自己的目录中。宿主不根据用户关键词选技能；Agent 用 `list_skills` 查看名称和说明，再显式调用 `read_skill`。成功交付原始正文后才向后续模型请求提供专用工具。主 Agent 的启用状态按用户轮次隔离，`unload_skill` 可提前隐藏工具。包停用或重载后，旧的激活版本失效，必须重新读取技能。

左侧主导航的“技能页面”无需已有对话或模型配置，支持输入本地目录安装，以及启用、停用、重载和移除包。移除只注销贡献，不删除包文件。配置保存在当前 DSH profile 下的 `skill-packages.json`；也可通过 `DSH_SKILL_PACKAGES_DIR` 指定启动扫描目录。同一路径只加载一次。包格式、独立程序的 JSON-RPC 协议和页面接口见 [技能插件包接口](docs/skill_packages.md)。本地独立程序工具和 HTML 页面无需重新编译宿主；内置源码位于 `src/builtin_plugins/` 和 `frontend/src/builtin_plugins/`，共享项目构建；外置即使下载到项目中也仍由安装器按外置身份加载。两者遵循相同声明、依赖、贡献注册与清理规则。

MCP `initialize` 返回 `Mcp-Session-Id`，后续 POST/GET/DELETE 应携带该头。每个客户端会话独立激活技能，默认工具列表不带代码 Flow 等包工具。`read_skill` 加载后，服务通过 GET SSE 发送 `notifications/tools/list_changed`，客户端应刷新 `tools/list`；`unload_skill`、重置和包生命周期变化也发送通知。新任务边界可调用 `reset_skill_activation`，结束时 DELETE `/mcp`。MCP 不提供用户轮次信号，服务不会根据消息猜测边界。未携带会话头的旧客户端仍可使用基础工具，但不能启用包工具。

聊天附件支持选择、拖拽和粘贴图片，并使用持久引用在多轮中读取资料及调用视觉分析。AI PPT SVG 版只开放首批八种扩展名，协议兼容范围、解析边界及测试见[会话附件](docs/attachments.md)。
