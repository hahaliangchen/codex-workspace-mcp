# 模型请求阶段日志

主 Agent 和上下文子 Agent 的每次实际模型请求都会写入
`logs/model-requests-YYYY-MM-DD.jsonl`（日期和时间使用 UTC），并同步输出到宿主进程日志。
一行是一条 JSON 事件。文件追加写入，Unix 下新文件权限为 `0600`。

## 关联一次请求

`identity` 包含 `request_id`、`task_id`、`context_id`、`turn`、`step`、角色、提供商和模型。
`context_id` 对应数据库 `agent_request_contexts.id`，可在任务的请求上下文中查看原始输入。
最终的传输摘要同时保存到该请求的 `metadata.outcome.transport`。

宿主发送 `X-Client-Request-Id: <request_id>`。模型网关若记录该请求头，就可以按同一 ID
核对是否收到请求、转发给哪个上游，以及上游/下游何时断开。响应中的 `x-request-id`、
`request-id`、`x-correlation-id`、`traceparent`、`cf-ray` 也会保留，供网关日志查询。

例如，在项目根目录中实时查看：

```sh
tail -f logs/model-requests-$(date -u +%F).jsonl
```

## 阶段和判断

| 日志或失败阶段 | 能确定的事实 |
| --- | --- |
| `prepared` | 请求已组装并有持久化快照；记录字节数、正文哈希、角色数量、工具数量、末尾 8 条消息的结构，不记录内容 |
| `send_started` / `awaiting_headers` | 已进入 HTTP `send()`；尚未收到完整响应头。这个阶段包含 DNS、TCP、TLS、上传请求和等待响应，不能仅凭它断定网关已收到请求 |
| `response_headers` | 收到 HTTP 状态、协议版本、实际连接地址及选定响应头；`headers_ms` 是从诊断开始到响应头的耗时 |
| `http_error_body` | 已收到非成功 HTTP 状态，正在读取错误体或因该状态被拒绝；完整错误沿用原请求上下文的错误记录，诊断文件只记录错误体长度和哈希 |
| `first_byte` | 收到成功响应的首个非空字节块；记录首字节耗时、累计字节和块数 |
| `first_delta` | 解码到首个文本、思考或工具增量；不记录增量内容。普通 JSON 响应可能没有此事件 |
| `stream_read` | 等待响应体或读取响应体时失败；结合 HTTP 状态、已读字节数判断是否在传输途中断开 |
| `stream_decode` | 已拿到数据，在解析 SSE/JSON 时失败；记录解码器错误码 |
| `body_eof` | HTTP 响应体结束；仍需检查模型协议是否完整结束 |
| `response_validation` | HTTP 读取和解码完成，但模型缺少结束标志、输出截断或被过滤 |
| `event_persistence` | 把模型增量写入宿主历史时失败，应检查本机数据库/磁盘 |
| `waiting` | 每 15 秒记录当前阶段及最后收包时间，区分等待响应头与流中途停滞 |
| `finished` | 本次最终状态、失败阶段和分类错误链；成功时阶段为 `completed` |

`error_chain` 记录 reqwest 的 timeout/connect/request/body/decode 标志、IO 错误类型、
系统错误号和已识别的网络原因，例如 `incomplete_http_message`。未知来源写成
`unclassified`，不把任意错误字符串写入诊断文件，因为它可能包含响应正文或带凭证的 URL。

准备日志只记录代理环境变量的**名称**，不记录值；这不能证明某条请求实际经过了代理。
HTTP 200 也不能单独证明模型调用完成，必须一起检查最终状态及结束标记。
若失败在响应头之前，本机日志能缩小范围，但要确定是网关、上游还是中间代理主动断连，
仍需要对照网关日志或网络证据。

本次仅补充诊断，不自动重试，也不会为了诊断额外调用真实模型。
