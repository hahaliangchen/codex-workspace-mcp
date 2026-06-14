# codex-workspace-mcp

给 Codex 和其他 Agent 用的本地工作区 MCP 服务，同时包含一个 AI Proxy，用来把 Codex / Responses / Chat / Anthropic 等请求适配到不同上游模型。

它的目标很直接：

- 让 Agent 少靠 shell，优先使用结构化文件、搜索、索引工具
- 让代码结构可导航，让项目经验可沉淀
- 让 Codex 可以接入 DeepSeek、Mimo、OpenAI 兼容中转站等不同 provider
- 在文本模型和多模态模型之间做轻量适配，而不是维护复杂状态

## 核心能力

### 工作区 MCP

- 文件工具：读文件、读行范围、写文件、替换行范围、列目录、搜索文本
- 代码索引：Go / Rust / Python / TS / JS 符号、注释、调用关系、建议阅读路径
- 项目记忆：记录每次改动、实现原因、测试结果和潜在风险
- Skills：按需列出和读取本地 Codex skills

### AI Proxy

- `/v1/responses`：Codex Responses API 入口，默认由本地 Agent Runtime 接管
- `/v1/chat/completions`：OpenAI Chat Completions 入口
- `/v1/messages`：Anthropic Messages 入口
- 模型路由：通过 `default_provider` 和 provider `model_map` 映射上游模型
- Agent Runtime：本地运行 ReAct 工具循环，直接调用 MCP 文件、搜索、索引、记忆等工具
- 过程输出：Codex 只看到普通 Responses 文本流，包括 `[agent]`、`[tool]` 过程信息
- 协议拆分：消息协议转换和工具调用控制分开处理，不再使用 raw Codex 透传、echo 伪装或 call_id 等待下一轮

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

DeepSeek、Mimo 这类文本模型不直接吃图片。代理会：

1. 只检查最新 user 消息中的真实图片
2. 调用配置的视觉 provider 解析图片
3. 把图片块替换成 `[图像分析报告]` 文本
4. 不把原始图片 base64/path 发给文本模型
5. 不维护长期 `image_key -> 原图` 映射

如果用户后续明确要求重新看图，`analyze_image` 只会尝试分析当前请求上下文中仍可见的原图。若上下文里已经没有原图，模型应提示用户重新上传。

这个设计原则是：图片是当前请求上下文资源，不是代理持久状态。

## 配置示例

参考 [ai_proxy_config.json.example](./ai_proxy_config.json.example)：

```json
{
  "default_provider": "deepseek-openai",
  "vision_provider": "gpt",
  "architecture_provider": "deepseek-openai",
  "architecture_model": "deepseek-v4-flash",
  "providers": {
    "deepseek-openai": {
      "url": "https://api.deepseek.com/v1",
      "api_key": "YOUR_KEY",
      "api_type": "openai",
      "supports_vision": false,
      "model_map": {
        "gpt-5-codex": "deepseek-chat"
      }
    },
    "gpt": {
      "url": "https://api.example.com/v1",
      "api_key": "YOUR_KEY",
      "api_type": "openai",
      "supports_vision": true,
      "model_map": {
        "gpt-4o-mini": "gpt-4o-mini"
      }
    }
  }
}
```

说明：

- `default_provider` 控制请求走哪个 provider
- `vision_provider` 控制图片解析使用哪个 provider
- `architecture_provider` / `architecture_model` 控制便宜模型分析代码业务逻辑并生成 architecture memory
- `supports_vision: false` 用来明确 DeepSeek、Mimo 等文本 provider 不支持视觉
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

### 代码索引

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

## 设计原则

- 优先使用结构化 MCP 工具，shell 只做 fallback
- 优先借当前上下文和上游原生能力，不急着维护代理状态
- 能文本化沉淀的结果就文本化，避免长期保存一次性资源
- 工具调用历史要尽量自愈，不能让异常历史拖垮后续请求
- 配置保持轻量，provider 能力通过 provider 本身声明

## 相关文档

- [AI_STATELESS_CONTEXT_DESIGN_LESSON.md](./AI_STATELESS_CONTEXT_DESIGN_LESSON.md)：一次多模态状态设计复盘，记录为什么要移除长期图片映射。
