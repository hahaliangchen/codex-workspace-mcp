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

SKILL.md 的 YAML frontmatter 必须有相同的 `name: example` 和适用描述 `description`。工具名必须唯一，不得覆盖其他已安装插件或宿主工具。清单 `inject` 是后端服务依赖；每个插件发布 `plugin:{id}` 服务，可声明 `inject: ["plugin:other"]`。缺少依赖不能激活，被依赖的插件不能在使用者仍运行时停用。

`tools` 是完整声明。实现可由项目的原生贡献适配器提供，或通过 `command_tools` 的独立程序适配器提供。两者都经过相同清单校验、注册和清理；通用加载器没有 code-flow 特例。原生适配器提供工具 schema、handler 和权限等级，外置包不能借用项目的原生实现。

`renderer: react` 表示 Cordis 页面模块：内置模块的 entry 为项目注册键（例如 `code-flow.architecture`），外置 entry 为包内已构建 JS/MJS 模块。`renderer: html` 是通用 HTML 页面适配器，内置外置均可使用。资源路径不得越出包或页面目录。

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

插件专用会话每轮模型请求显式说明当前轮次与工具可用状态。用户确认方案、追加修改等新轮次也须重新 `read_skill`，旧历史中的技能正文不能充当本轮激活。主 Agent 提前调用已安装但尚未激活的工具时返回 `SKILL_NOT_ACTIVE`，提示独立读取对应技能后再调用；权限拒绝、插件作用域拒绝和只读上下文子 Agent 限制分别报告，不把它们统称为子 Agent 递归错误。未激活的调用不会到达远程插件。

- `GET /agent/skill-packages`：共同目录，包含来源 origin、依赖、作用域、工具和页面声明。
- `POST /agent/skill-packages`，`{"path":"/absolute/package/path"}`：安装外置包，保存配置。
- `POST /agent/skill-packages/{id}/enabled`，`{"enabled":false}`：共同启停接口，清理所属贡献。
- `POST /agent/skill-packages/{id}/reload`：验证后重载，校验失败保留原贡献。
- `DELETE /agent/skill-packages/{id}`：注销外置安装记录，不删除文件；项目内置成员用停用操作。
- `POST /agent/skill-packages/{id}/tools/{name}`：所属页面调用工具。
- `GET /agent/skill-packages/{id}/pages/{page}/{resource}`：外置模块、HTML 与资源。

Agent 自行通过说明判断技能是否适用，read_skill 成功交付正文后才提供专用工具；主 Agent 按用户轮次隔离。重载产生新作用域，旧激活失效。MCP 按 Mcp-Session-Id 隔离，通过 GET SSE 发送工具列表变化通知，unload_skill 隐藏一个技能，reset_skill_activation 清空会话技能。MCP 没有用户轮次信号，调用者显式重置。两小时无请求的会话失效。

本实现参考本机 DSH 的 client/modules、ui-renderer、slots.register，以及 Cordis 的 Service、inject、plugin、effect 生命周期。协议与 Rust/前端适配器共享上述思想；DSH npm 包的完整配置和远程服务 wire 协议不是本插件清单。

## 由插件页面执行的工具

插件可以把一部分工具交给自己的页面执行：工具定义写 `"executor": "page"`（不写或写 `"service"` 表示由插件服务执行）。宿主不知道这些工具的具体内容，只负责转发：

- Agent 调用这类工具时，宿主把调用交给发起这个任务的页面，等页面交回结果。页面通过插件会话的两个方法与宿主交互：
  - `tool_calls`：`{task_id, page_id, wait_ms}`，领取待执行的调用，最多挂起 25 秒。
  - `tool_result`：`{task_id, page_id, call_id, result}`，交回结果。
  iframe 和 Cordis 页面都可以通过 `pluginHost.session` 使用这两个方法。
- 发送 `prompt` 时带上 `page_id` 的页面，负责这个任务的页面工具；同一任务只认最后一个发送 prompt 的页面。其他页面也可以轮询，但领不到调用。任务还没有负责的页面时（例如宿主刚重启），由第一个来轮询的页面负责。
- 调用已经排队、但负责的页面 45 秒没有来轮询，或者页面领走调用后 180 秒没有交回结果，工具就返回 `PAGE_CLOSED`，宿主结束这个任务：关闭页面即视为放弃任务。
- 页面交回的结果格式为 `{ok, content: [{type: "text", text} | {type: "image", mimeType, data}]}`。图片存成任务的视觉素材，结果中给出 `artifact_id`；JSON 文本原样作为 `data` 传给 Agent。
- 远程插件的页面工具不会被注入 `project_id`，也不会发送到插件服务；如果绕过 Agent 直接调用，宿主会拒绝。

插件增加或修改页面工具，只需改插件自己的清单和页面，宿主不需要改动。实现见 `src/page_tools.rs`。

## 远程页面和插件会话

### 统一模型目录

宿主设置「模型与提供商 → 插件可用模型」维护授权，保存在 generation 配置的 `plugin_models` 中，格式为 `{"ppt-master":[{"provider":"qwen-local","model":"qwen3.8-27b"}]}`。未出现的插件默认可用全部模型；显式空数组禁止该插件调用模型。设置只允许引用已登记且 Agent 支持的模型，旧版客户端未提交该字段时保留已有授权。

供应商可开启 `prefix_system_messages` 适配只接受开头系统消息的服务：请求时按原有顺序合并系统提示到前缀，普通消息和工具对保持顺序，持久化历史不变。该适配保存在宿主 generation 配置的 `provider_compatibility`，不向 DSH 的供应商清单增加协议外字段。

插件查询目录已按授权过滤。新会话优先采用允许的全局默认模型，否则取授权列表中首个可用模型；旧会话的原模型被撤销后，下一轮自动选用允许的默认模型。前端显式提交未授权模型会被拒绝，不能通过一般任务接口绕过。视觉、压缩和 Observer 的专用模型同样受限制，原专用模型不在授权范围时跟随当前允许的主模型。运行时使用独立的配置投影，不修改全局路由或其他插件的设置。

模型来源为 DSH profile 的 `llm-pi-ai.providers.*.models`，能力配置由宿主管理。
主 Agent、视觉分析、上下文压缩和 Observer 都引用目录中的 `(provider, model)`。
专用任务只保存模型引用及推理档位、任务上下文预算；不另存 URL、密钥或另一套模型定义。
设置页从目录选择专用模型，保存时拒绝缺失引用；删除被引用的模型前须重新选择对应任务模型。
运行时再次解析并检查引用，防止直接编辑配置后将未登记的模型传给专用任务。

- 宿主页面：`GET /agent/models`。
- Rust 插件：声明依赖 `model-catalog`，注入 `ModelCatalog` 后调用 `list().await`。
- Cordis 页面：`ctx.pluginHost.models()`，等价于 `session('models')`。
- iframe 页面：`skillPackage.models()`，通过同一个归属与作用域检查。
- 独立 PPT 页面：认证后的 `/agent/ppt/session`，传 `connection_handle` 和 `method: "models"`，不要求先创建 PPT 会话。

响应包含 `version`、`models` 和 `defaults`。模型按提供商和 ID 唯一标识，包含显示名、协议、当前 Agent 是否支持该协议、已声明的上下文/输出上限、视觉与推理能力。未知能力/上限保留 `unknown` 或 `null`；在目录中并不表示全部功能已通过实测。`verification` 只描述对应图片探测证据，不代表完整模型验证。
`defaults.visual_tool` 等是模型引用；字段为空时跟随当前主 Agent。不会向插件返回供应商地址、凭据、凭据环境变量或探测响应正文。每次查询读取当前配置，目录修改不需要各插件分别维护列表。本接口提供只读查询，不授予插件修改全局模型设置的权限。

这沿用 DSH 的提供商/模型标识和声明式目录；以上 HTTP/页面桥接接口是本项目适配层，不宣称等同于 DSH 全部 LLM wire API。

`POST /agent/skill-packages/remote` 接收 `{url, token}`，其中 url 为 PPT 服务的
`.../aiPpt/plugin`。使用 DSH Connection 包络进行 `ppt/handshake`、`ppt/confirm`；
确认 ready 并取得技能正文后才挂载贡献。远程包仍是 external，`remote:true`；
不创建本地包目录。配置仅在宿主保存，目录接口、模块和 iframe 不返回认证凭证。
重载会重新握手；连接过期、服务重启后可重载恢复。离线服务不会阻塞宿主启动。
当前远程适配器实现的是 PPT Remote 契约，不声称可直接运行任意 DSH 服务。

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
会话读取及新一轮权限提升被拒绝。`cargo test remote_handshake_registers` 验证
Remote 包络、确认、工具映射、页面声明、凭证隔离与卸载。
`node --test tests/plugin-host.test.mjs tests/plugin-pages.test.mjs` 验证 Cordis 服务
隔离及页面生命周期。PPT 前端的 `npm run test:ppt-plugin` 在 Chromium 中验证
模块和 iframe 的持续聊天、选页、只读呈现及工具详情不展示。

`cargo test skill_ -- --nocapture` 验证共同后端协议、来源、依赖、权限、激活与清理。`node --test tests/plugin-pages.test.mjs tests/code-flow.test.mjs` 验证实际 Cordis 的前端依赖、页面归属、卸载和图布局。构建 Rust 后，`python3 scripts/skill_packages_smoke.py` 在临时 profile/工作区验证安装、RPC、SSE、重载和持久化，不调用模型。MCP 接口参考 [Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports) 与 [Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)。

## 原站登录接入

AI PPT 原站通过 `POST /agent/ppt/{connect,session,service,tool}` 接入，当前 JWT 放在
Authorization。宿主使用 `PPT_PLUGIN_URL` 或已注册 PPT 服务地址，不能由浏览器指定
任意远程地址。握手验证 `user_id`，每次页面请求通过 ppt/identity 再验证凭证。
首次登录可在内存注册需要用户会话绑定的 PPT 插件，无需把用户 JWT 保存为全局配置。
通用工具入口不能执行这类插件的业务工具。

用户会话、PPT project_id 与已接收的异步任务归属存入 SQLite，用户 JWT 仅存内存。
后台 Agent 使用所属会话的连接执行工具，验证目标项目及任务归属。异步初始化在
原始任务完成后绑定实际 project_id；进程重启后先由原站重新认证，不使用共享凭证。
原站部署只公开 `/agent/ppt/*`，宿主通用管理及开发工具接口继续留在本机。
前端配置与代理示例见 ai-ppt-view/docs/ppt-host-agent.md。

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
