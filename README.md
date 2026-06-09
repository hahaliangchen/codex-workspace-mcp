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

- `/v1/responses`：Codex Responses API 入口
- `/v1/chat/completions`：OpenAI Chat Completions 入口
- `/v1/messages`：Anthropic Messages 入口
- 模型路由：通过 `default_provider` 和 provider `model_map` 映射上游模型
- raw Codex 透传：`raw_codex: true` 时不做格式转换，直接交给上游
- 工具调用修复：把 Codex Responses 的 function_call / function_call_output 转成标准 Chat Completions 工具调用链
- 历史清洗：遇到异常工具调用历史时，尽量转换或跳过，而不是把坏历史继续发给上游导致报错
- echo 伪指令：把代理工具调用伪装成 Codex 能显示的终端输出，避免模型不认识代理内部工具名

## 多模态策略

项目支持两种路径。

### raw provider

如果 provider 配置了：

```json
{
  "raw_codex": true
}
```

代理会跳过本地图片预处理，直接把 Codex 原始请求透传给上游。适合本身支持 Codex / 多模态格式的模型或中转站。

### 文本 provider

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

- `default_provider` 控制普通请求走哪个 provider
- `vision_provider` 控制图片解析使用哪个 provider
- `supports_vision: false` 用来明确 DeepSeek、Mimo 等文本 provider 不支持视觉
- `raw_codex: true` 的 provider 默认由上游自己处理多模态

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

