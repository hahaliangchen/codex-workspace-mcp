# codex-workspace-mcp

> 当前执行、作用域、语言和插件边界，以[语言手册](docs/agent-program.md)及[通用 Agent 设计](docs/generic-agent-design.md)为准。旧设计执行路径已经移除；设计文档中的有日期测试记录只代表当时版本。
> 当前执行语言为版本 14：对象引用、块级作用域、词法作用域、函数值与闭包已按 JS 语义执行；可挂起的回调方法、完整 Promise 与事件循环尚未实现，计划见 [JS 运行时设计](docs/js-runtime-design.md)。版本 12 的运行记录及历史测试见[统一清理记录](docs/generic-agent-design.md#23-当前代码统一与退役路径清理2026-10-27)。

由 Rust 执行任务的本地通用 Agent，使用基于 DeepSeek Harness（DSH）的 React/Cordis 前端。当前执行采用固定 JS 基础语法子集的流程调用与返回、父作用域继承、按容量折叠上下文；支持按需串行子任务和受约束的并发委派。模型直接连接提供商的 Chat Completions 接口。

## 核心能力

- 版本 14 流程语言：普通函数、常见基础循环／表达式、块级作用域与对象引用由 Rust 确定性执行，工具使用固定库名和自动生成的声明表达；智能生成和业务判断通过显式 @virtual 虚函数调用，支持授权 fork/join、异常与回执恢复。异步目前仍是受限的宿主等待，并非完整 Promise 或 Node 环境，详见[语言手册与串行／并发教程](docs/agent-program.md)。可执行示例：[最小串行](skills/task-program/examples/serial.js)、[两 Agent 并发五页](skills/task-program/examples/parallel-five-pages.js)，配套 JSON 输入与源码同目录。
- 文件读取、文本搜索、版本校验写入与编辑。
- Go、Rust、Python、TypeScript/JavaScript 符号索引和职责记忆。
- 受限原生程序执行、项目依赖与脚本、进程日志、HTTP 探测及浏览器操作。
- 图片、文档附件、视觉分析、持久资产引用与上下文压缩。
- Rust 插件作用域、技能包、独立进程工具、插件页面与 PPT 会话。

内核按作用域树挂载工作区、模型设置、工具目录和任务服务。依赖缺失时拒绝启动；卸载按子节点优先与注册逆序清理。`GET /agent/plugins` 返回服务、依赖及生命周期事件。

### 一个程序、一套函数执行机制

主 Agent 创建一次主任务，提交唯一 main。普通函数、工具调用与显式 @virtual AI 工作由同一个 Runtime 推进；需要细分时补写当前函数，不手动维护步骤、串行任务数组或结束确认。用户授权并发后使用 fork/join，最多同时运行五个执行分支。公共材料通过实际参数和作用域变量交接，分支工作对话不复制回父／兄弟。

工具只有固定 library.method 名字和一份真实契约。规划时读取自动生成的头文件，源码直接调用库方法，虚函数中的原生步骤通过 invoke_library 传输相同方法名。底层分发标识仍用于回执和审计，不构成另一套公开编排 API。插件提供业务 Skill、生产流程和工具；宿主不内置业务步骤。

旧的 Organizer／Worker 交接、Observer 复盘执行器、模型选材压缩器、工具组加载状态及任务数组调度已经删除。前端不再展示无效的 Observer／压缩模型设置，也不轮询不存在的复盘。旧事件和请求正文仍能按原始引用查看。

实际模型请求及工具交换在执行前后持久化，Flow 展示这些事实。主 Agent 与协作分支共用进展超时策略：连接上限30秒，响应头及有效输出空闲上限180秒；持续有效输出不会因为旧的总时限被打断。取消立即传播，未确认的外部写入不重放。

## 日志策略

模型传输诊断由 `model_transport` 生成，通过 `proxy_log` 写入 `logs/model-requests-YYYY-MM-DD.jsonl`。

诊断按请求 ID 记录发送、响应头、首字节、解码和失败阶段，不写认证头或对话正文。字段与排查方法见[模型请求阶段日志](docs/model_request_diagnostics.md)。模型的完整请求和响应另存于任务历史与请求上下文中。

## 上下文策略

主任务定义、已确认计划及实际进度由宿主记录和注入，过程摘要不能改写目标。Runtime 保存函数帧、PC、变量和挂起效果；当前参数自动进入一次，其他可见变量按名称读取。函数返回释放局部工作上下文，不再引用的绑定和临时量会被回收。原始事件、不可变资产和项目文件不删除。

工作窗口先扣除固定规则和函数输入，在剩余容量80%处开始折叠旧的完整交换，尽力回落到60%。固定输入与最新交换受到保护；旧步骤保留摘要事实和可恢复引用。超大的材料用稳定资产引用分页处理，引用不能冒充已读正文。不存在另一套模型选材、全文摘要或历史投影执行器。

进程中断后消费已保存的回执，完成情况未知的外部操作明确报错，不自动重放。每轮请求正文保存到 agent_request_contexts，历史查阅继续通过同一个只读历史接口。

### 视觉与协作模型配置

设置中的专用模型只有视觉分析工具和协作子 Agent。可分别选择提供商、模型、推理档位及上下文容量，留空继承主 Agent；字段为 agent-generation.yml 中的 visual_tool、delegate_agent。上下文折叠由宿主完成，不调用另一个压缩模型。退役配置文件和未知旧字段不被当作能力执行，也不为清理代码而删除用户已有文件。

plugin_models 限制插件可使用的模型；plugin_generation 给对应插件提供默认生成／视觉档位。插件模型授权仍独立校验，不因函数继承或配置切换而扩大。

### 图像分析工具

视觉分析默认同步返回。调用方也可显式设置 background:true 创建后台作业，并通过 image_analysis_results 读取结果。后台作业只分析已保存图像，不继承主窗口或修改文件；取消及中断有明确回执。宿主提供通用能力，具体业务选择同步还是后台、检查哪些图像，由插件 Skill 与流程程序决定。检查完成不代表业务验收通过。

主 Agent 使用 `analyze_images({artifact_ids, question})` 对保存的图片进行视觉分析，最多两张，按参数顺序传原图。工具使用设置中的“视觉分析工具”路由（应用提供方的模型映射）；未配置的项跟随主 Agent。问题和必要背景由调用 AI 提供；工具请求只有这一条用户消息与原图，不继承主窗口或历史，也没有浏览器/递归工具权限。

返回的 `analysis` 是模型原始正文，按普通原生工具返回进入主窗口，并得到稳定资产引用。宿主不改写结论、插入额外用户消息、切换主模型或修改/压缩/释放上下文。`browser_screenshot` 负责截图，`view_image` 仍只负责选图；视觉检查由主 Agent 显式调用分析工具。模型原始请求与响应保存为 `actor=tool` 的诊断记录，不创建执行角色或子 Agent 窗口。取消、超时、截断和空返回作为工具错误处理。

### 资产引用与 AI 控制的加载状态

单 Agent 的文件/符号读取、搜索结果、页面资料、截图和执行输出会得到稳定的 `asset_id`。原始工具结果仍完整保存在历史中；正文快照按内容哈希保存在操作工作区的 `.codex-workspace-mcp/assets/`，图片继续使用 `.codex-workspace-mcp/visual/` 中的原文件。SQLite 的 `agent_context_assets` 保存来源、版本、路径、范围和读取引用。相同来源、参数及内容复用同一个引用，正文快照不会被新版本覆盖。

Agent 的库调用返回工具原始 JSON；资产索引另存 asset_reference 和 delivery_reference，不向业务返回值混入宿主字段。工作窗口中的材料可通过固定库方法管理：

- host.list_context_assets：查看当前窗口正文、片段、图片及引用，不重做读取。
- host.release_assets：主动将指定正文换成引用，保留原文历史。
- host.read_asset：读取保存版本或分页；图片引用不会重新截图。
- host.refresh_asset：明确读取本地文件当前版本，不重放命令、上传或截图。
- host.read_scope：访问当前仍可见的真实变量或字段，不从历史重新拼装输入。

按容量折叠由宿主确定性执行，没有 compress_context 模型或 asset_context 选材协议。函数参数保留一次，返回后释放局部窗口；原始事件和不可变资产继续可查。超容量正文分页交付，原有材料不因一次新读取失败而丢失。容量按保守估算并结合模型用量校准，不承诺与提供商 tokenizer 完全相同。


## 工作区工具

### 基础文件工具

下列原始名称用于外部 MCP 接口；内置 Agent 的流程源码仅使用 io.read/read_lines/write/list/search/edit/replace 等固定库名，不发布这些原始名作为另一套编排方法。

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

模型配置写入 profile 的 `cordis.patch.yml`，密钥存于 DSH 凭据文件；生成参数、视觉分析与协作模型覆盖配置存于 `agent-generation.yml`。任务请求会重新读取外部配置修改。

前端使用本地 Cordis 与 DSH UI renderer。来源与许可证见 [`frontend/src/dsh/SOURCE.md`](frontend/src/dsh/SOURCE.md) 和同目录的 `LICENSE`。

## 任务 API 与过程界面

`POST /agent/sessions` 创建空会话，`POST /agent/tasks/{task_id}/messages` 发送首条消息或续聊。HTTP/SSE API 提供任务列表、事件流、取消、模型请求详情、文件改动 Diff 与撤销。会话保存在工作区 `.codex-workspace-mcp/codex_state.db`。

任务事件使用 `turn/start`、`user/message`、`step/start`、`assistant/delta`、`assistant/message`、`tool/call`、`tool/result`、`step/end` 和 `turn/end`。Flow 节点对应实际请求与程序操作，协作分支按真实归属关联；历史事件查看保留来源。旧的 Worker 工作笔记接口、调度节点续跑入口和前端任务树／回溯实现已删除，已结束的会话直接发送新消息续聊。

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

历史数据访问已收敛到 `session_history/store.rs`：原始要求、引用校验、恢复、资产补回和界面历史读取共用该访问层，原始事件按编号升序返回。


## 技能插件包

技能包把提示词、工具和页面放在自己的目录中。宿主不根据用户关键词选技能；Agent 用 `list_skills` 查看名称和说明，再显式调用 `read_skill`。成功交付原始正文后才向后续模型请求提供专用工具。已读取的技能持续生效，直到显式 unload_skill 或插件生命周期变化。进入插件页面时，宿主在首轮请求前加载其 Skill 和工具；插件没有 MD 也可以正常启用。包停用或重载后，旧的激活版本失效，必须重新读取技能。

左侧主导航的“技能页面”无需已有对话或模型配置，支持输入本地目录安装，以及启用、停用、重载和移除包。移除只注销贡献，不删除包文件。配置保存在当前 DSH profile 下的 `skill-packages.json`；也可通过 `DSH_SKILL_PACKAGES_DIR` 指定启动扫描目录。同一路径只加载一次。包格式、独立程序的 JSON-RPC 协议和页面接口见 [技能插件包接口](docs/skill_packages.md)。本地独立程序工具和 HTML 页面无需重新编译宿主；内置源码位于 `src/builtin_plugins/` 和 `frontend/src/builtin_plugins/`，共享项目构建；外置即使下载到项目中也仍由安装器按外置身份加载。两者遵循相同声明、依赖、贡献注册与清理规则。

MCP `initialize` 返回 `Mcp-Session-Id`，后续 POST/GET/DELETE 应携带该头。每个客户端会话独立激活技能，默认工具列表不带代码 Flow 等包工具。`read_skill` 加载后，服务通过 GET SSE 发送 `notifications/tools/list_changed`，客户端应刷新 `tools/list`；`unload_skill`、重置和包生命周期变化也发送通知。新任务边界可调用 `reset_skill_activation`，结束时 DELETE `/mcp`。MCP 不提供用户轮次信号，服务不会根据消息猜测边界。未携带会话头的旧客户端仍可使用基础工具，但不能启用包工具。

聊天附件支持选择、拖拽和粘贴图片，并使用持久引用在多轮中读取资料及调用视觉分析。插件附件能力由插件声明，协议兼容范围、解析边界及测试见[会话附件](docs/attachments.md)。

Git并发文件交接已补齐：fork句柄明确工作区，join后由流程显式检查、合并，再读回验收。共享目录和Git仓库均有示例回归，详见[Git边界与十页实测](docs/generic-agent-design.md#24-git-并发产物的显式交接2026-10-27)。本次主／子Agent均使用Luna，10页已生成并自动合并；整轮仍因测试时限及导航问题未通过完整交付。
