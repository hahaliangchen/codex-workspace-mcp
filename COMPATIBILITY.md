# Codex Responses API 协议兼容性与踩坑实录

本文件详细记录了在逆向与适配 Codex 桌面端 Responses API（基于 `wire_api = "responses"` 协议）过程中所遭遇的各种技术壁垒、协议陷阱、以及代理层 `ai_proxy` 对应的终极转译与兼容技术方案。

---

## 协议背景
Codex 在较新版本中为了封锁底层大模型，强制推行了未公开、私有化的 `/v1/responses` 接口。该接口不仅在数据输入结构上做了大量定制，还引入了全新的 SSE 事件流格式。本项目的核心目标之一就是充当“无损协议转译桥梁”，在不破坏大模型指令原生态的前提下，打通第三方模型（如 DeepSeek、小米 Mimo）的顺畅接入。

---

## 踩坑实录与转译方案

### 1. 双层嵌套 Input 导致“提问丢失且白屏”
*   **问题现象**：Codex 传给代理的 `input` 上下文采用了极为罕见的双层嵌套数组格式（`[{"content": [{"text": "...", "type": "input_text"}]}]`）。原有的解析逻辑只期望单层平铺对象（`[{"text": "..."}]`），这导致用户的实际提问和长篇系统提示词全部解析失败并被意外丢弃，大模型由于收到空提问而无法产生任何回复。
*   **转译方案**：我们在 [src/ai_proxy.rs](file:///e:/项目/codex-workspace-mcp/src/ai_proxy.rs) 中重构了 `input` 提取层，增加了对内层 `content` 数组的递归深度提取，确保双层和单层结构 100% 兼容，使用户提问完整触达。

### 2. Missing `input_tokens` 导致“无限断线重连”
*   **问题现象**：大模型生成完毕后，Codex 在反序列化最后的 `response.completed` 事件时，强硬要求 `usage` 统计里必须包含 Anthropic 规范的 `input_tokens` 和 `output_tokens` 字段。而由于下游大模型（如小米、DeepSeek）通常只返回标准的 `prompt_tokens` 和 `completion_tokens`，导致 Codex 客户端直接抛出 `failed to parse ResponseCompleted: missing field input_tokens` 异常并高频断开重连。
*   **转译方案**：在 [src/format_translate.rs](file:///e:/项目/codex-workspace-mcp/src/format_translate.rs) 的 `ResponsesStreamConverter` 中重构了 `usage` 对象，以“双模式字段注入”的形式同时提供 `prompt_tokens` / `completion_tokens` (OpenAI 风格) 与 `input_tokens` / `output_tokens` (Anthropic 风格)，彻底根治反序列化中断。

### 3. WebSockets 与 HTTP REST 接口的“流事件名陷阱”
*   **问题现象**：虽然代理成功拦截了流数据并向下游进行了中转，但 Codex 前端聊天框始终呈现死寂和白屏状态。经深度抓包发现，OpenAI 针对 WebSockets 实时流和 HTTP REST responses 流采用了截然不同的事件命名。WebSocket 侧使用 `response.text.delta`，而 Codex 所基于的 HTTP REST `/v1/responses` 接口则强硬要求使用 **`response.output_text.delta`** 增量事件及 **`response.output_text.done`** 结束事件。
*   **转译方案**：我们在 `ResponsesStreamConverter` 中彻底修正了文本增量事件的名称为官方规范的 `response.output_text.delta` 并补齐了 `done` 帧，打通了前端文本实时渲染。

### 4. 多 System 消息导致“下游厂商 400 Bad Request”
*   **问题现象**：Codex 除了在 `instructions` 中注入提示词，还会在 `input` 数组的各个 `input_text` 部分塞入大量的系统环境与技能说明。如果直接将其作为多条 `system` 消息发给下游接口，很多对接口规范有严格限制的提供商（如 DeepSeek、Mimo）会直接抛出 `400` 错误并拒绝服务（大模型接口通常只允许有一条 `system` 消息且必须排在数组首位）。
*   **转译方案**：我们在 `ai_proxy.rs` 中设计了**系统级指令智能识别与头部唯一 System 消息合并算法**：
    *   智能检测从 `input` 中提取的文本是否以 `<permissions instructions>`、`<skills_instructions>`、`<app-context>` 等系统前缀开头。
    *   若是，划归为 `system` 属性并进行换行拼接；
    *   在最终 `messages` 列表的第 0 位组装出**唯一的一条头部 System 帧**，彻底屏蔽了任何多 System 报错校验。

### 5. Tools 命名空间展开扁平化与“非 Function 异型工具”的避错拦截机制
*   **问题现象**：
    1.  **核心 MCP 工具被隔离**：Codex 传给代理的真实本地 MCP 工具（如 GitKraken、读写文件等），全都被嵌套保存在了一个以 `"name": "mcp__codex_workspace_mcp"` 命名的 **命名空间工具对象** 的 `"tools"` 数组内部！由于它们被包裹在里面，且外层对象既没有 `type: "function"` 也没有 `function` 属性，直接发送会导致下游接口报 `"`function` is not set"` 的 400 参数异常，且大模型由于看不到里面的 MCP 工具而完全无法触发它们。
    2.  **异型工具引发 400 崩溃**：Codex 还会向 `tools` 中混入如 `type: "custom"` 的异型工具（如 `apply_patch`）。由于下游主流大模型接口（如 Mimo、DeepSeek）**在标准 API 层面只接受符合 "type": "function" 规范的工具定义**，一旦投递非 function 格式的工具，就会直接导致接口崩溃并抛出 `400 Param Incorrect` 校验报错。
*   **转译方案**：
    *   **命名空间扁平化提取**：我们在 `tools` 解析逻辑中加入了**命名空间递归扁平化机制**。一旦检测到某个工具项包含 `tools` 属性且为数组，就将其体内的全部真实 MCP 具体工具（如 `list_dir`，`read_file`）全部扁平化提取出来！
    *   **严格规范化与安全避错过滤**：对提取出的所有子工具和平铺 Function 工具，进行 OpenAI 嵌套重构，确保每一项都完美拥有嵌套的 `function` 对象。同时，为了彻底防范下游接口的崩溃，我们对不合规的异型工具（如 `type: "custom"`）进行了**安全避错截留**，确保发往下游大模型的工具队列 100% 具备 `type: "function"`。这在打通大模型对本地 MCP 接口调用感知力的同时，彻底治愈了 400 校验地雷！

### 6. 大模型工具调用的“流式转译发射（SSE）”
*   **问题现象**：即使大模型触发了工具调用，如果代理在流传输期间没有向 Codex 实时发射事件，Codex 客户端便无法在流期间感应并唤起本地 MCP 执行层，导致模型生成的工具代码最终被作为普通文本直接呈现在了气泡中。
*   **转译方案**：我们在 `ResponsesStreamConverter` 中引入了 `tool_status: HashMap<usize, ToolCallState>` 状态追踪器：
    *   当在流中捕获大模型的 `tool_calls` delta 时，以异步非阻塞形式，向客户端实时发射带有对应 `output_index` (设为 `idx + 1`) 的 **`response.output_item.added` (type: `"function_call"`)** 以及 **`response.function_call.arguments.delta`** 流式事件。
    *   在流结束时补齐并关闭所有仍在进行的工具调用项，发射 **`response.function_call.arguments.done`** 和 **`response.output_item.done`** 帧。
    实现了工具链在 SSE 事件流层面的实时打通。

### 7. 大模型 Resources 探活死循环与“虚设资源语义红牌阻断”
*   **问题现象**：当面对用户“有哪些 MCP 资源或服务可用”等探活提问时，下游大模型由于指令惯性，会发起 `List MCP resources` 和 `List MCP resource templates` 的系统调用。而由于本服务是一个纯工具型（Tools）MCP，我们在资源端默认返回空数组 `[]`。大模型接收到空结果后会产生疑惑，误以为服务加载异常，从而在对话中反复循环发起这两大资源的探测，陷入探活死循环。
*   **转译方案**：本着“以语义迎头棒喝，直接引导模型回归正轨”的巧思，我们没有返回冰冷的空 `[]`，而是向 `resources/list` 的数据中**注入了一个虚设的“语义红牌指示资源 (Virtual Notice Resource)”**：
    *   在它的 `description` 字段里，我们以最直接的英文指令告知模型：“NOTICE TO AI: This MCP server is 100% Tools-Only... Please DO NOT call list_mcp_resources or templates again. You should directly call tools like list_dir, read_file...”。
    *   模型在调用时，读取并理解这一系统语义级通知后，会瞬间醒悟，直接停止无意义的循环探测，从而将决策权交还并专注于调用平铺在眼前的实战 Tools。

---

### 8. Codex 前端白名单拦截与“瞒天过海（Dynamic Repack）”双向转译机制
*   **问题现象**：虽然代理层扁平化提取了子工具，但大模型调用 `"list_dir"` 或 `"read_file"` 时，Codex 客户端前端会粗暴拦截并持续返回 `"unsupported call"` 错误。经抓包探明，Codex 客户端前端存在极严格的“白名单校验”——在其全局工具表里，只认以 **`"mcp__codex_workspace_mcp"`** 命名的最外层命名空间外壳工具，任何平铺名称的直接流式调用都会在白名单匹配阶段被就地枪决。
*   **转译方案**：我们在流式中转层设计了极其高精度的“瞒天过海重组装（Dynamic Repack）”魔术桥梁：
    *   **大模型侧**：在 `tools` 列表中，我们把子工具加上命名空间前缀（例如 `"mcp__codex_workspace_mcp__list_dir"`）作为平铺 Function 推送，以确保大模型的生成精确度。
    *   **Codex 发射侧**：当在流中捕获大模型的子工具调用时，在向 Codex 发射事件时将工具名篡改回外壳名 `"mcp__codex_workspace_mcp"`（100% 绿灯放行！）。同时，在流结束时（done 帧），我们把大模型吐出的原始扁平参数，**在内存中智能重组装、套上一层包裹，重构为 Codex 命名空间期待的 `{ "name": "子工具名", "arguments": { 子工具参数 } }` 嵌套套娃结构**一次性发射。
    *   **本地 MCP 侧**：在 `3000` 端口的 `mcp.rs` 匹配前，加入智能前缀剥离与高度容错机制。无论 Codex 底层剥不剥离前缀，我们都能以 100% 的兼容度顺利分发并静默执行！

### 9. 重启后聊天历史丢失与“多角色历史（Role）动态无损透传”
*   **问题现象**：在会话重启或重新加载时，大模型在历史上说过的所有回复（`assistant`）突然大面积清空并神秘消失，大模型的历史记忆链条断裂。
*   **物理本质**：经对近 $400\,\text{KB}$ 的超长 `input` 日志进行字节分析，发现对话历史节点（`"type": "message"` 的元素）其外层天生带有 `"role"` 字段（包括 `"user"`、`"developer"` 和 `"assistant"`）。但旧版代理在遍历 `input` 数组文本时，硬编码强行把所有的文本角色改写为了 `"role": "user"`。大模型的历史发言全被“指鹿为马”伪造成了用户发言，直接摧毁了大模型的自我角色定位。
*   **转译方案**：我们在 `ai_proxy.rs` 的 `input` 文本解析宏中，加入了动态角色提取与多角色无损映射机制：
    *   智能检测并提取历史节点身上的 `"role"` 属性；
    *   将 `"developer"` 兼容映射为 downstream 所需的唯一 `"system"`；
    *   将 `"assistant"` 无损还原为大模型的 `"assistant"`；
    *   保证了重启重新加载后，大模型百分之百完美地继承前一轮对话中的自我心智和全部上下文发言轨迹！

---

## 全局技术架构探讨：第三方模型直连的五大协议天堑与代理层的“同声传译”价值

很多开发者会好奇，既然有类似 `cc-switch`（一键切换 AI 配置）的轻量级辅助工具，是否只需要修改一下 Codex 的模型底座 URL 或是 API Key 配置文件，就能让 Codex 客户端**直接**与小米 Mimo、DeepSeek 等第三方模型无缝对接，从而舍弃本项目的代理中转程序？

**答案是：绝对不可能直接对接！本项目的代理中转层（`codex-workspace-mcp.exe`）拥有不可替代的同声传译价值。**

`cc-switch` 仅仅是帮你重写了配置文件的物理“搬运工”。而如果你直接把大模型官方接口连接到 Codex，大模型和客户端会在第一秒钟就因为以下**五大协议天堑**而彻底瘫痪崩溃：

### 1. 天堑一：嵌套 Input 结构导致“提问丢失，界面白屏”
*   **物理本质**：Codex 传给大模型的上下文采用了罕见的双层嵌套数组格式（`[{"content": [{"text": "...", "type": "input_text"}]}]`）。
*   **直连灾难**：下游厂商（小米 Mimo、DeepSeek 等）的官方标准 API 根本无法识别这种嵌套结构，会直接将其丢弃。由于大模型收到的提问内容为空，前端聊天框将陷入永久死寂和白屏。

### 2. 天堑二：异型非 Function 工具引发“400 Bad Request（接口崩溃）”
*   **物理本质**：Codex 会在工具列表中混入非标准 Function 定义的工具（如 `type: "custom"` 的 `apply_patch`，以及 `type: "namespace"` 的本地 MCP 工具包裹外壳）。
*   **直连灾难**：下游厂商在标准 API 层面**只允许 "type": "function" 的工具通过**。一旦直接发给官方接口，接口在报文格式校验时会直接报错 `400 Param Incorrect` 并粗暴地拒绝提供服务。

### 3. 天堑三：多 System 消息引发“400 校验地雷”
*   **物理本质**：Codex 会在 `input` 中注入多条 permissions/skills 的系统前缀提醒。
*   **直连灾难**：许多严谨的第三方接口（如 DeepSeek）在 API 规范上强硬要求**有且仅能有一条 System 提示词且必须位于首位**，多条 `system` 角色消息直接引爆 400 校验拒绝。

### 4. 天堑四：缺少 `input_tokens` 导致“无限断线重连”
*   **物理本质**：这是由于大厂技术壁垒与 Anthropic / OpenAI 标准冲突所致（详见下文关于 `input_tokens` 的深度剖析）。
*   **直连灾难**：第三方模型官方返回的是标准 OpenAI 的 `prompt_tokens`。Codex 底层的反序列化解析器由于在 `ResponseCompleted` 事件里找不到 Anthropic 命名的 `input_tokens` 字段，会直接抛出反序列化异常，前端聊天框高频闪烁并陷入**无限断开重连的死循环**！

### 5. 天堑五：SSE 增量流事件名不符导致“气泡死寂”
*   **物理本质**：官方模型吐出的流事件名为 `response.text.delta`。而 Codex 客户端前端渲染引擎强硬要求必须是私有 responses 特有的 `response.output_text.delta` 增量事件名。
*   **直连灾难**：如果直接对接，Codex 前端渲染引擎根本认不出大模型吐出的数据流，聊天气泡中将不会显现任何字，陷入完全的死寂。

---

## 深度剖析：什么是 `input_tokens`，以及为什么它构成了大厂的“协议地雷”？

### 1. 基本定义与概念
**`input_tokens`**（输入 Token 数）是大模型使用与计费统计（Usage）中的基本度量指标。它指的是**大模型在接收到当前请求时，输入上下文总字符经过大模型专属分词器（Tokenizer）切分后，所占用的 Token 数量**。这部分包括：
*   全局唯一的 System 提示词（System Prompt）；
*   从 `input` 数组和 `messages` 历史中提取的上下文历史对话；
*   所有可用工具列表的完整结构体描述定义（Tools Definitions）；
*   用户的当前提问（User Query）。

### 2. 标准的冲突：OpenAI 风格 vs Anthropic 风格
在 AI 行业发展的过程中，逐渐分裂出了两大阵营的计量标准：
*   **OpenAI 规范**：将输入 Token 命名为 **`"prompt_tokens"`**，输出 Token 命名为 **`"completion_tokens"`**。下游绝大多数主流大模型（如小米 Mimo、DeepSeek、阿里千问等）都在其通用 OpenAI 兼容接口中完全沿用了这一命名。
*   **Anthropic 规范**：将输入 Token 命名为 **`"input_tokens"`**，输出 Token 命名为 **`"output_tokens"`**。
*   **大厂的协议壁垒**：Codex 客户端底层在设计私有的 Responses 流式 API 反序列化器时，强行采纳了 **Anthropic 的命名规范**。它规定大模型在生成结束返回最终 usage 统计时，**反序列化实体中必须严格包含 `input_tokens` 属性**！
*   一旦直接连接第三方大模型（只有 `prompt_tokens`），反序列化器就会抛出 `missing field input_tokens` 致命异常并当场断线重连！

### 3. 代理层的“双向同声传译”价值
我们设计的 Rust 中转代理（`codex-workspace-mcp.exe`）不仅是一个中转站，而是一个**智能网关转译器**：
*   **进站方向**：拦截 Codex 的奇特双层嵌套、异型 Tools、多 system 帧，在内存中用我们设计的局部宏和算法**实时翻译成大模型听得懂的标准 OpenAI 报文**发过去。
*   **出站方向（流式中转）**：把大模型吐出的标准流数据和 `tool_calls`，在内存中**实时截留、瞒天过海 Repack 包装、并翻译组装为包含双风格字段注入（同时返回 `prompt_tokens` 和 `input_tokens`）的 responses 私有事件流**发射回去，让 Codex 客户端 100% 顺畅、毫无阻碍地跑通！

**没有这个实时同声传译的 Rust 代理网关，网线插得再快，大模型和 Codex 之间听到的也只是乱码和噪音。这就是本服务无法被替代的终极架构价值。**

### 10. `response.completed` 中 output item 缺少 `id` 导致"重启后历史对话消失"

*   **问题现象**：Codex 正常使用时对话显示完全正常。但一旦重启，聊天界面里**AI 的所有历史回复全部消失**，只剩用户自己发出的消息。日志中没有报错，下游模型也正常响应新的提问。
*   **物理本质**：`ResponsesStreamConverter::emit_completion_events()` 在生成最终的 `response.completed` 事件时，其 `output` 数组里的每个 item 对象**缺少 `id` 字段**。而在同一次响应的流式 SSE 事件（`response.output_item.added`）中，这个 `id` 已经被正确生成并下发（如 `item_txt_{msg_id}`、`item_tool_{msg_id}_{idx}`），但在最终汇总环节重新用 `json!()` 宏构造 `output` 时，遗漏了这个字段。
    *   Codex 把 `response.completed.output` 里的 item 写入本地历史存储时，依赖每个 item 的 `id` 作为唯一索引键。
    *   `id` 为 `null` 的 item 被存入后，Codex 重启重建 UI 时按 `id` 查找找不到对应记录，直接跳过渲染，于是所有 AI 历史回复从界面蒸发。
*   **转译方案**：在 [src/format_translate.rs](file:///e:/项目/codex-workspace-mcp/src/format_translate.rs) 的 `emit_completion_events()` 函数中，为 `final_output_items` 的每个 item 补全 `id` 字段，**与流式 SSE 事件中的 `item_id` 保持严格一致**：
    *   文字回复 item：`"id": format!("item_txt_{}", self.msg_id)`
    *   工具调用 item：`"id": format!("item_tool_{}_0", self.msg_id)`

    这样 Codex 本地存储和 UI 渲染引擎就能在重启后按 id 精确还原全部历史对话，再不丢失。

---

### 11. 助手消息 content type 用 `"text"` 而非 `"output_text"` 导致历史消息无法渲染

*   **问题现象**：`response.completed` 的 `output` 里助手消息内容能正常存储，但重启 Codex 后历史会话中 AI 的回复气泡全部空白，只有用户消息可见。
*   **物理本质**：OpenAI Responses API 规范中，用户输入 content 的 type 是 `"input_text"`，而助手输出 content 的 type 是 `"output_text"`（不是 `"text"`）。我们此前在以下四个 SSE 事件里错误地使用了 `"text"`：
    *   `response.content_part.added` 的 `part.type`
    *   `response.content_part.done` 的 `part.type`
    *   `response.output_item.done` 的 `item.content[].type`
    *   `response.completed` 的 `output[].content[].type`
    
    Codex 的渲染器只认 `"output_text"` 类型的内容块，遇到 `"text"` 直接跳过不渲染，导致历史气泡空白。
*   **转译方案**：在 [src/format_translate.rs](file:///e:/项目/codex-workspace-mcp/src/format_translate.rs) 中将上述四处 `"text"` 统一修改为 `"output_text"`，与 Responses API 规范保持一致。

---

### 12. `response.completed.output` 工具调用用 Chat Completions 格式导致 "unsupported call"

*   **问题现象**：MCP 工具在流式 SSE 事件中看起来格式正确（`response.output_item.done` 里 `name` 已正确 repack 为 `mcp__codex_workspace_mcp`），但 Codex 执行时仍返回 `"unsupported call: mcp__codex_workspace_mcp"` 错误，无法调用任何 MCP 工具。
*   **物理本质**：`response.completed.output` 里的工具调用用的是 **Chat Completions 格式**（`type: "message"` + `tool_calls` 数组），而 Responses API 要求每个工具调用是**独立的顶层 `function_call` 类型条目**：
    ```
    // 错误（Chat Completions 格式）：
    {"type": "message", "role": "assistant", "tool_calls": [{"type": "function", ...}]}
    
    // 正确（Responses API 格式）：
    {"type": "function_call", "call_id": "...", "name": "mcp__codex_workspace_mcp", "arguments": "..."}
    ```
    Codex 读取 `response.completed` 时扫描 `output` 数组，只认 `"type": "function_call"` 的条目作为可执行工具调用。遇到 `"type": "message"` 的条目，直接返回 "unsupported call"。
*   **转译方案**：在 [src/format_translate.rs](file:///e:/项目/codex-workspace-mcp/src/format_translate.rs) 的 `emit_completion_events()` 中，将工具调用的最终输出格式从 Chat Completions 嵌套结构改为 Responses API 标准格式，每个工具调用作为独立顶层 `function_call` 条目推入 `final_output_items`，`id` 字段与流式 `response.output_item.done` 事件保持一致。

---

## 经验总结与后续维护建议
当 Codex 客户端版本后续再次升级导致通信异常时，可优先采取以下排查步骤：
1.  **拦截 `Codex raw tools` 日志**：观察 `ai_proxy.log` 中最新记录的 raw tools 是否有新增的特殊字段。
2.  **检查 `input` 结构体层次**：利用日志确认 Codex 传过来的 `input` 内部的 `content` 是否又发生了新的多层嵌套或属性重命名。
3.  **监控 SSE 异常帧**：通过调试观察 Codex 前端是否有解析报错，对照本文件中的 Token 结构或流事件名定义进行微调。
4.  **检查 `response.completed.output` 格式**：Responses API 与 Chat Completions 的输出结构差异很大，工具调用必须是顶层 `function_call` 条目，文字必须是 `output_text` content type。
