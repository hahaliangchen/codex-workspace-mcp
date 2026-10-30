# 固定插件协议：内置与外置

插件都通过同一份清单声明技能、工具、页面与依赖，经同一个注册器挂载到作用域。贡献归插件所有，卸载按作用域清理工具、页面、服务与进程。内置不绕过清单检查、工具权限或技能激活规则。

内置和外置是**项目成员身份**，不是渲染方式或目录推断。内置由项目的显式 roster 注册，随前后端共同构建；外置由安装器或扫描器发现，即使下载到项目目录中也仍是外置。清单不能自报 `origin: builtin`。内置可以停用；移除外置只注销安装记录，不删除文件。

## 项目内置目录

```text
src/builtin_plugins/
  mod.rs                       # 后端项目成员 roster
  services.rs                  # 宿主基础服务插件，遵循 Plugin 生命周期
  code_flow/
    mod.rs                     # 声明原生工具实现、schema 与权限
    plugin.json                # 固定协议清单
    SKILL.md
    tool.json
    backend.rs
frontend/src/builtin_plugins/
  index.ts                     # 前端项目成员 roster
  shell.ts                     # 基础界面插件，使用 Cordis 插槽
  code-flow/
    index.ts                   # Cordis name / inject / apply
    page.tsx
    graph.ts
    style.module.css
```

内置页面共用项目的 React、Cordis、组件库和构建工程。宿主的技能页面只显示贡献目录，不知道 code-flow 的实现或磁盘路径。后端也不在通用加载器中判断某个插件的工具名。

## 清单与实现适配

`src/plugin_protocol.rs` 定义固定清单，`frontend/src/plugins/protocol.ts` 定义前端注册契约。示例外置包：

```json
{
  "version": 1,
  "id": "example",
  "library": "example",
  "title": "示例插件",
  "skill": "SKILL.md",
  "inject": [],
  "tools": ["example_inspect"],
  "command_tools": [{
    "name": "example_inspect",
    "description": "检查工作区数据",
    "input_schema": {"type": "object", "properties": {}},
    "program": "python3",
    "args": ["backend.py"],
    "timeout_seconds": 60
  }],
  "pages": [{"id": "view", "title": "示例页面", "entry": "page/plugin.mjs", "renderer": "react"}]
}
```

`skill` 为可选字段；未声明、文件不存在或为空时，插件仍可正常挂载、打开页面和调用声明工具，不生成替代业务提示词。提供非空 SKILL.md 时，其 YAML frontmatter 必须有相同的 `name: example` 和适用描述 `description`。工具名必须唯一，不得覆盖其他已安装插件或宿主工具。清单 `inject` 是后端服务依赖；每个插件发布 `plugin:{id}` 服务，可声明 `inject: ["plugin:other"]`。缺少依赖不能激活，被依赖的插件不能在使用者仍运行时停用。

`library` 声明该插件的固定程序库名（例如 ppt-master 声明 `"library":"ppt"`）。未填写的旧包按 ID 将连字符替换为下划线。库名必须是非保留标识符，不能使用 io/host，不能与其他已安装插件重名。该字段也支持远程 plugin/handshake 的 plugin 对象。工具方法从已登记名称移除该插件的双下划线前缀；模型不再自行传 libraries 别名。

宿主从 inputSchema 自动生成 program_tool_headers 声明，供模型阅读参数和约束，程序源码仍是无类型 JS 子集。原生／远程工具可提供 outputSchema，命令工具可提供 output_schema；没有返回契约时声明 unknown。规划请求只发声明，虚函数任务用 invoke_library 传输同一个固定方法名及原参数，执行仍进入原 handler 和权限检查。编译调用和原生传输都交回原工具 JSON；资产交付 envelope 和事件序号不是库返回字段。原生 spawn/join 和工具组加载不是模型的另一套编排入口。详见[执行语言](agent-program.md#工具声明与固定名字)。

`tools` 是完整声明。实现可由项目的原生贡献适配器提供，或通过 `command_tools` 的独立程序适配器提供。两者都经过相同清单校验、注册和清理；通用加载器没有 code-flow 特例。原生适配器提供工具 schema、handler 和权限等级，外置包不能借用项目的原生实现。

`renderer: react` 表示 Cordis 页面模块：内置模块的 entry 为项目注册键（例如 `code-flow.architecture`），外置 entry 为包内已构建 JS/MJS 模块。`renderer: html` 是通用 HTML 页面适配器，内置外置均可使用。资源路径不得越出包或页面目录。

## 插件页面的业务指令

只有明确归属于插件的会话（进入插件页面后发起的聊天）自动启用自己的工具和指令。
宿主在第一次模型请求之前完成激活，将该插件非空 MD 正文作为 `plugin_instructions`
系统消息；后续轮次及子步骤按同一插件身份重新提供，独立于可压缩聊天历史。
没有 MD 时不注入该系统消息，页面和声明工具照常可用，不要求模型调用 read_skill。
模型不能在插件页面里卸载自己的技能；离开页面不向主聊天注入指令，停用插件则撤销能力。
普通主聊天不会因为安装或启用了某插件就自动加载它，仍保留按需 read_skill/unload_skill。

## 前端 Cordis 贡献

前端模块均导出 `name`、`inject`、`apply`；name 必须等于清单 id。加载器负责挂载，页面由模块通过服务注册。内置代码可以直接导入共用 React；外置模块也可以使用宿主注入的 React 和插件工具 hook，不需要携带另一份 React：

```javascript
export default {
  name: 'example',
  inject: ['pluginPages', 'pluginUi'],
  apply(ctx, { owner }) {
    const { React, usePluginTools } = ctx.pluginUi;
    function Page({ owner }) {
      const tools = usePluginTools(owner);
      const [result, setResult] = React.useState('');
      return React.createElement('button', {
        onClick: async () => setResult(JSON.stringify(await tools.call('example_inspect', {})))
      }, result || '读取');
    }
    ctx.pluginPages.register(owner, 'view', 'page/plugin.mjs', Page);
  }
};
```

`pluginPages.register` 校验清单归属与当前作用域，通过调用者的 `ctx.effect` 注册清理。贡献随所属 Cordis fiber 卸载，不依赖 React 页面组件自行注销。模块可用普通 `inject` 依赖其他前端服务；依赖未就绪时 Cordis 保留 pending 状态。外置模块不通过项目 glob 加入内置 roster，下载目录不改变身份。

打开页面不激活 Agent 技能。`usePluginTools(owner)` 将调用绑定到所属包，后端再次检查工具归属。HTML 页使用只允许脚本的 iframe 和 `/agent/skill-page-sdk.js`，通过 `skillPackage.workspace()`、`skillPackage.callTool(name,args)` 调用同一归属接口。

## 独立程序 JSON-RPC

每次工具调用启动程序，工作目录为包目录；固定 argv 不经过 shell，不插入模型参数。stdin 收到一行请求后关闭，stdout 只能输出一个 JSON-RPC 响应，日志写入 stderr。

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"example_inspect","arguments":{},"workspace_root":"/path/to/workspace"}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{"items":[]}}
```

失败可返回 error 或非零退出码。超时为 1–600 秒，输入上限 1 MiB，stdout/stderr 各 256 KiB。超时、任务取消、插件卸载终止程序及子进程。进程没有文件系统沙箱，因此 Agent 仅在 full_access 且允许执行时获得这些工具；读取技能不提升权限。

## 生命周期与技能激活

插件专用会话每轮模型请求说明当前工具可用状态。成功 `read_skill` 后，技能在同一会话的步骤和后续用户轮次中持续启用，确认方案或追加修改无需重读。显式 `unload_skill`、插件停用或重新加载产生的包作用域变化会使原激活失效；重新启用必须读取新包说明。只有历史正文而没有有效激活记录，不会获得工具。未激活调用返回 `SKILL_NOT_ACTIVE`，不会到达远程插件；权限、插件会话隔离和只读限制仍独立校验。

- `GET /agent/skill-packages`：共同目录，包含来源 origin、依赖、作用域、工具和页面声明。
- `POST /agent/skill-packages`，`{"path":"/absolute/package/path"}`：安装外置包，保存配置。
- `POST /agent/skill-packages/{id}/enabled`，`{"enabled":false}`：共同启停接口，清理所属贡献。
- `POST /agent/skill-packages/{id}/reload`：验证后重载，校验失败保留原贡献。
- `DELETE /agent/skill-packages/{id}`：注销外置安装记录，不删除文件；项目内置成员用停用操作。
- `POST /agent/skill-packages/{id}/tools/{name}`：所属页面调用工具。
- `GET /agent/skill-packages/{id}/pages/{page}/{resource}`：外置模块、HTML 与资源。

Agent 自行通过说明判断技能是否适用，read_skill 成功交付正文后才提供专用工具；主 Agent 在同一会话中持续保存有效激活，显式卸载后失效。重载产生新作用域，旧激活失效。MCP 按 Mcp-Session-Id 隔离，通过 GET SSE 发送工具列表变化通知，unload_skill 隐藏一个技能，reset_skill_activation 清空会话技能。MCP 没有用户轮次信号，调用者显式重置。两小时无请求的会话失效。

本实现参考本机 DSH 的 client/modules、ui-renderer、slots.register，以及 Cordis 的 Service、inject、plugin、effect 生命周期。协议与 Rust/前端适配器共享上述思想；DSH npm 包的完整配置和远程服务 wire 协议不是本插件清单。

## 由插件页面执行的工具

插件可以把一部分工具交给自己的页面执行：工具定义写 `"executor": "page"`（不写或写 `"service"` 表示由插件服务执行）。宿主不知道这些工具的具体内容，只负责转发：

- Agent 调用这类工具时，宿主把调用交给发起这个任务的页面，等页面交回结果。页面通过插件会话的两个方法与宿主交互：
  - `tool_calls`：`{task_id, page_id, wait_ms}`，领取待执行的调用，最多挂起 25 秒。
  - `tool_result`：`{task_id, page_id, call_id, result}`，交回结果。
  iframe 和 Cordis 页面都可以通过 `pluginHost.session` 使用这两个方法。
- 发送 `prompt` 时带上 `page_id` 的页面，负责这个任务的页面工具；同一任务只认最后一个发送 prompt 的页面。其他页面也可以轮询，但领不到调用。任务还没有负责的页面时（例如宿主刚重启），由第一个来轮询的页面负责。
- 调用已经排队、但负责的页面 45 秒没有来轮询，或者页面领走调用后 180 秒没有交回结果，工具就返回 `PAGE_CLOSED`，宿主结束这个任务：关闭页面即视为放弃任务。
- 页面交回的结果格式为 `{ok, content: [{type: "text", text} | {type: "image", mimeType, data, meta?}]}`。图片存成任务的视觉素材（`meta` 存为素材元数据 `image_meta`，例如 `{slide: 3}` 表示这张图是第 3 页），结果中给出 `artifact_id` 和 `images:[{artifact_id, mime_type, meta}]`；JSON 文本原样作为 `data` 传给 Agent。
- 远程插件的页面工具不会被注入 `project_id`，也不会发送到插件服务；如果绕过 Agent 直接调用，宿主会拒绝。
- 页面结果里有图片、且远程插件在 `capabilities` 中声明了 `visual_review` 时，宿主调用插件 `plugin/reviewBrief`，参数为 `{context, tool, arguments, document, images, plan}`：`tool` 是去掉宿主前缀的工具名，`document` 是页面结果里的 `document` 原样，`images` 是各图的 `meta`（按顺序），`plan` 是任务看板上的计划 `{confirmed, version, plan}`。插件返回 `{instructions, subjects}`（每张图一个 subject；只返回单个 `subject` 时用于所有图；不适用时返回空对象），宿主把 `instructions` 和该图的 subject 存进各图的视觉素材元数据 `review`。获取失败时图片照常保存，工具结果附 `review_instructions_unavailable` 说明原因。

插件增加或修改页面工具，只需改插件自己的清单和页面，宿主不需要改动。实现见 `src/page_tools.rs`。

### 图片审查说明与分析证据

- `analyze_images` 发现所选图片带有 `review` 时，把各图不同的 `instructions` 原样拼接（相同的只出现一次），再按图片顺序附上 `Image N:` 加该图的 `subject`，作为视觉请求的 system 消息；调用方的问题仍是 user 消息的第一段，每张图前插入 `Image N` 文字。没有 `review` 的图片维持原来的单条 user 消息。宿主不理解说明内容。
- 一次 `analyze_images` 最多 9 张图。1–2 张照原样发送；3 张及以上拼成一张编号大图（最多 3 列，每格上方黑条写白色编号，留灰色间隔；图片保持原尺寸，整张超过 4 百万像素或 8 MiB 时等比例缩小），user 消息说明「Images 1-N are tiled on this sheet…」。这张大图另存为视觉素材（`source: analysis_sheet`），就是视觉模型实际收到的那张。
- 每次分析完成后，宿主保存 `{analysis_id, analysis, images:[{artifact_id, source:{tool, arguments, document, meta}}], sheet}`，结果返回 `analysis_id`。
- 远程插件工具的参数顶层出现 `analysis_id`（字符串）或 `analysis_ids`（数组）时，宿主按 ID 读取本任务（及其根会话）保存的分析，放进 `plugin/call` 的 `context.analyses`；参数本身原样转发。ID 不属于本任务时调用直接失败。插件据此读取视觉模型的原文，不依赖调用方转述。实现见 `src/image_analysis.rs`、`src/remote_sessions.rs`。

## 远程页面和插件会话

### 统一模型目录

宿主设置「模型与提供商 → 插件可用模型」维护授权，保存在 generation 配置的 `plugin_models` 中，格式为 `{"ppt-master":[{"provider":"qwen-local","model":"qwen3.8-27b"}]}`。未出现的插件默认可用全部模型；显式空数组禁止该插件调用模型。设置只允许引用已登记且 Agent 支持的模型，旧版客户端未提交该字段时保留已有授权。

供应商可开启 `prefix_system_messages` 适配只接受开头系统消息的服务：请求时按原有顺序合并系统提示到前缀，普通消息和工具对保持顺序，持久化历史不变。该适配保存在宿主 generation 配置的 `provider_compatibility`，不向 DSH 的供应商清单增加协议外字段。

插件查询目录已按授权过滤。新会话优先采用允许的全局默认模型，否则取授权列表中首个可用模型；旧会话的原模型被撤销后，下一轮自动选用允许的默认模型。前端显式提交未授权模型会被拒绝，不能通过一般任务接口绕过。视觉和协作执行 Agent 的专用模型同样受限制，原专用模型不在授权范围时跟随当前允许的主模型。运行时使用独立的配置投影，不修改全局路由或其他插件的设置。

模型来源为 DSH profile 的 `llm-pi-ai.providers.*.models`，能力配置由宿主管理。
主 Agent、视觉分析和协作执行 Agent 都引用目录中的 `(provider, model)`。上下文折叠由宿主确定性完成，不另设压缩模型。
专用任务只保存模型引用及推理档位、任务上下文预算；不另存 URL、密钥或另一套模型定义。
设置页从目录选择专用模型，保存时拒绝缺失引用；删除被引用的模型前须重新选择对应任务模型。
运行时再次解析并检查引用，防止直接编辑配置后将未登记的模型传给专用任务。

- 宿主页面：`GET /agent/models`。
- Rust 插件：声明依赖 `model-catalog`，注入 `ModelCatalog` 后调用 `list().await`。
- Cordis 页面：`ctx.pluginHost.models()`，等价于 `session('models')`。
- iframe 页面：`skillPackage.models()`，通过同一个归属与作用域检查。
- 独立插件页面：认证后的 `/agent/plugins/{id}/session`，传 `connection_handle` 和 `method: "models"`，不要求先创建业务文档。

响应包含 `version`、`models` 和 `defaults`。模型按提供商和 ID 唯一标识，包含显示名、协议、当前 Agent 是否支持该协议、已声明的上下文/输出上限、视觉与推理能力。未知能力/上限保留 `unknown` 或 `null`；在目录中并不表示全部功能已通过实测。`verification` 只描述对应图片探测证据，不代表完整模型验证。
`defaults.visual_tool` 等是模型引用；字段为空时跟随当前主 Agent。不会向插件返回供应商地址、凭据、凭据环境变量或探测响应正文。每次查询读取当前配置，目录修改不需要各插件分别维护列表。本接口提供只读查询，不授予插件修改全局模型设置的权限。

这沿用 DSH 的提供商/模型标识和声明式目录；以上 HTTP/页面桥接接口是本项目适配层，不宣称等同于 DSH 全部 LLM wire API。

`POST /agent/skill-packages/remote` 接收 `{url, token}`，url 是插件服务基址。
使用 Connection 请求/响应包络调用 `plugin/handshake`、`plugin/confirm`；
握手返回 `version:1`、`state:awaiting_confirmation`、`connection_id`、不透明的
`subject` 和插件清单（`id/title/tools/pages` 及可选 `skill/attachment_extensions/capabilities`）。
确认 ready 后，通过 `/resource?path=<skill>` 取得技能正文并挂载贡献。
认证头为 `Authorization: Bearer ...` 和 `X-Plugin-Connection`。
这些是本项目的通用远程插件契约，参考 DSH 的分层和 Connection 包络，
不声称能直接运行任意 DSH 服务。配置仅在宿主保存，页面及目录不返回认证凭证。
重载会重新握手；离线服务不会阻塞宿主启动。

宿主只注册清单实际声明的工具，不补充任何业务工具或参数。
工具通过 `plugin/call` 接收 `{tool, arguments, context}`，其中 arguments 原样转发；
context 的 `conversation_id` 是根聊天，`execution_id` 是当前执行节点。
返回 `{value: <工具结果>}`。旧的计划确认（`plan_proposal`）已随任务板删除，宿主不再处理该字段；
需要用户确认的场景将以虚函数循环中的确认工具提供，形式待定。
可选 `capabilities` 中声明 `attachments` 才转送附件；插件返回 `path/src`，宿主不拼业务 URI。
声明 `visual_review` 才会收到 `plugin/reviewBrief`（见“图片审查说明与分析证据”）。
`plugin/service`、`plugin/resource` 的业务方法、标识、资源归属均由插件检查。

远程清单可同时声明 `react/module.js` 和 `html/page`。模块注入的 `pluginHost`
按 Cordis isolation 绑定所属插件；iframe 的消息桥转到同一 facade。
iframe 只允许脚本，代理响应 CSP 禁止直接网络请求和表单提交；页面只得到工具
结果和资源内容。模块共享宿主 JavaScript 环境，属于可信代码扩展，不是脚本安全沙箱。

`pluginHost.session(method,args)` 支持 open、create、prompt、events、status、cancel。
`pluginHost.service(method,args)` 支持 project、task、events、resource。
HTML SDK 对应 `skillPackage.session` / `skillPackage.service`。
前者调用 `/agent/skill-packages/{id}/session`，后者调用同路径的 `/service`。
宿主校验当前包作用域和会话归属，桥接不能访问主聊天或其他插件会话。

页面会话的 plugin_id 保存在 SQLite `plugin_sessions`，由宿主创建，客户端不能
修改。每次 Agent 启动/恢复都读取此归属，同时过滤工具 schema、校验执行并限制
技能加载。PPT 会话不提供宿主文件、终端、浏览器或其他插件工具；历史读取限本
会话。上下文整理子 Agent 继承同一范围，提升 permission_mode 不会解除范围。
模型的工具调用越界会返回错误；提示词同时要求拒绝 PPT 以外的工作，宿主不按
用户关键词分类。此处保证的是执行权限边界，不声称模型绝不会输出无关文字。

PPT 工具加上 `ppt_master__` 命名空间，避免与宿主 write_file/read_file 冲突。
render_slide 的服务端 PNG 由宿主读取并注册为当前任务的视觉素材，交给现有视觉
模型适配器，不开放本地任意图片读取。异步模式及任务状态继续由 PPT 服务控制。

## 验证（包含远程页面）

`cargo test plugin_conversation_cannot_escape` 验证伪造写文件、终端、跨技能、跨
会话读取及新一轮权限提升被拒绝。`cargo test remote_` 验证
Remote 包络、确认、工具映射、页面声明、凭证隔离与卸载。
`node --test tests/plugin-host.test.mjs tests/plugin-pages.test.mjs` 验证 Cordis 服务
隔离及页面生命周期。PPT 前端的 `npm run test:ppt-plugin` 在 Chromium 中验证
模块和 iframe 的持续聊天、选页、只读呈现及工具详情不展示。

`cargo test skill_ -- --nocapture` 验证共同后端协议、来源、依赖、权限、激活与清理。`node --test tests/plugin-pages.test.mjs tests/code-flow.test.mjs` 验证实际 Cordis 的前端依赖、页面归属、卸载和图布局。构建 Rust 后，`python3 scripts/skill_packages_smoke.py` 在临时 profile/工作区验证安装、RPC、SSE、重载和持久化，不调用模型。MCP 接口参考 [Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports) 与 [Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)。

## 独立插件页面登录接入

页面通过 `POST /agent/plugins/{id}/{connect,session,service,upload,attachment}`
接入，当前用户凭证放在 Authorization。连接地址必须已由管理员注册，页面不能指定任意地址。
宿主使用通用 `plugin/identity` 每次验证 subject；用户凭证仅存内存，不替换管理员配置。
SQLite 的 `remote_plugin_sessions` 只保存聊天、插件、subject 与服务地址。
业务文档、模板、异步任务关联只存在插件自己的存储。

`plugin/session` 的 `resolve/bind/open/prompt/status` 由插件处理业务状态；
宿主响应将状态放在 `plugin_state` 中，宿主不解释其中字段。
子执行节点携带同一根 conversation_id。重启后须重新认证。
旧的插件聊天只有通过插件自己的 open 归属验证后，才能建立新的通用身份绑定。
宿主不解析聊天里的“确认”字样来推进流程。

PPT 专有的旧协议兼容与项目绑定已迁到 AI PPT 插件仓库，Agent 没有对应适配器。
部署须同步更新插件后端、插件前端和 Agent；旧 `/agent/ppt/*` 路由已删除。

## 会话附件

插件清单可声明 `attachment_extensions`，按文件扩展名列出允许的输入；缺省不接收附件。宿主按插件所有者在上传与消息接纳时校验，插件业务服务仍须检查实际格式与用户权限。模块及 iframe 通过注入的 `pluginHost.session('upload', ...)`、`session('attachment', ...)` 和既有 `prompt` 方法接入，与主聊天共享宿主存储和消息引用。具体 DSH 兼容范围、PPT SVG 版白名单和测试见[会话附件](attachments.md)。

### 上下文自动整理

宿主在主 Agent 每次模型请求前检查估算输入 token 数。默认软阈值为
`min(96000, 输入预算 × 75%)`，达到后自动运行现有只读 context agent；
专用任务模型沿用模型列表中的配置和插件允许范围。上下文模型阅读原始需求，
决定摘要和资产保留方式，宿主不按消息角色、大小或年龄删改原始记录。
摘要与工作窗口在同一事务内保存，原始历史、文件和资产引用保留。
失败保留原窗口并延后重试；超过硬容量仍禁止发送。整理后的窗口若仍偏大，
会留出后续增长量再重试，避免连续整理阻塞业务。
界面接收 `context/compression_start`、`context/compression_end`、
`context/compression_failed` 进度事件；事件记录前后估算量便于核对效果。

### 请求前缀与 KV 缓存

主 Agent 的逐轮上下文用量由宿主记录和监测，不拼进系统提示前缀；
MAIN 运行角色提示也不写入变化的用户轮次编号。相同技能和项目内的增量请求
保留固定系统前缀与既有历史，避免仅用量变化就让长历史的前缀缓存失效。
只读 context agent 用于自身 checkpoint 的用量提示置于请求尾部，明确标记
`runtime_context_capacity`，不进入持久化用户历史，也不提升为前缀系统消息。
