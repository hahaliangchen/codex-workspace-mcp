# 会话附件与 PPT 首批范围

本次沿用本机 DSH `46a7f68b0922371ce7144b668b90e377d8e799f4` 的附件分层：先保存字节并取得绑定接收会话的 receipt，提交消息时兑换引用；历史保存引用，视觉调用时再装配图片。来源为 `packages/client/file-upload/src/{types,http-route,protocol}.ts`、`packages/attachment/attachment/src/types.ts`、`packages/api/session-controller/src/commands.ts` 和 `docs/subsystems/attachment.zh.md`。

## 当前契约

- 宿主上传：`POST /api/session/uploadFileBinary?sessionId=...&name=...`，`Content-Type: application/octet-stream`，原始文件流。响应为 DSH `{ok:true,value:{receiptId,file:{attachmentId,name,bytes}}}`；失败为 `{ok:false,error:{code,message,details}}`。
- 提交给现有宿主 `/agent/tasks/{id}/messages` 或插件 `session('prompt', ...)`：保留 `prompt`，新增 `content: [{type:'file',receiptId}]` 和 `requestId`。也接收 DSH `{type:'text',text}`、`{type:'image',mediaType,data,name?}`。这里只兼容上述上传及消息内容契约，不表示所有 DSH Session Controller 路由已实现。
- 接纳事务校验会话、未消费凭证、格式与总量，成功后原子消费 receipt。拒绝提交可继续使用；同 `requestId`、同内容的重试返回已接收，不再启动第二轮；不同内容复用 ID 会报错。
- 历史内容为 `[{type:'text',text},{type:'image'|'file',attachment:ref}]`；图片引用带 `mediaType/width/height`。只发附件也是有效消息。
- 文件原件按 SHA-256 存在 `.codex-workspace-mcp/attachments/v1`，SQLite 保存会话归属、凭证、轮次及远程项目路径；不将 base64 或临时 Blob URL 写入对话历史。
- 限额：20 MiB/文件，20 个附件/消息，200 MiB/消息；图片最长边 8192，最多 6400 万像素。预览和视觉副本不替换原件。

`attachmentId` 是不透明引用，不是可访问的路径。读取需匹配已接纳的会话。主聊天的资源端点拒绝插件会话；PPT 原站用登录凭证与 connection_handle 读取；模块和 iframe 沿用插件 owner/package_scope 检查。新插件未声明附件白名单时默认不接收文件。

原站上传与模块/iframe 使用现有认证 JSON/RPC 桥传输 base64，返回相同 receipt 并复用同一宿主接纳、存储、历史和读取实现；这条桥接路径目前不是 DSH 的原始二进制流路由。原站支持上传进度和取消；模块/iframe 的停止可阻止接下来的消息提交，已经发出的桥接上传可能继续保存，但不会因此启动 Agent。

## AI PPT：SVG 版白名单

| 类型 | 用途 |
| --- | --- |
| PNG、JPG/JPEG、WEBP | 截图反馈、风格参考、图片素材 |
| PDF、DOCX、TXT、MD | 内容资料、需求说明、演讲稿 |

PPT 握手清单声明 `attachment_extensions: ['png','jpg','jpeg','webp','pdf','docx','txt','md']`。前端选择、拖拽、粘贴均限制该范围；宿主上传与消息接纳重验，PPT 后端项目导入再次校验格式及用户归属。SVG、GIF、PPTX、XLSX、CSV、ZIP 等本版不开放。完整 PPT 编辑器的文件能力以后单独开放。

原站创建/编辑页与模块/iframe 都支持选择、拖拽、粘贴图片、缩略图、移除、附件独立提交及历史恢复。主聊天使用相同消息与读取能力，不套用 PPT 专属白名单。未知格式可以在主聊天保存，但不会声称已解析。

## Agent 如何读取

- `read_attachment(attachment_id,char_offset?,max_chars?)` 解码 TXT/MD、PDF 文本、DOCX 正文和表格，返回精确字符范围和 `next_char_offset`；返回内容属于不可信资料，不能改变权限。
- 图片交给 `analyze_images`；模型输入实际包含图片字节，结果回到同一会话的工具交换中。视觉副本最长边 2048，并适配公共视觉层 400 万像素/8 MiB 的输入预算；原件仍保存。历史轮次中的图可在同一会话重新分析。
- `read_attachment(...,page_index:1)` 渲染 PDF 第 1 页，再交给视觉工具；扫描 PDF 提取不到文字时明确提示，不能当作空白内容。DOCX 当前不提取图片或还原版式。
- PPT 项目创建后将已接纳图片导入远程 `images/`，资料导入 `sources/`；`read_attachment` 返回 `remote_path`。SVG 从 `svg_output/` 使用 `../images/...` 引用素材，仍须渲染检查。原站项目由既有登录权限约束。

运行依赖：宿主 PATH 中的 `python3`（仅标准库）、`pdftotext` 和 `pdftoppm`。缺少依赖或解析失败通过工具错误明确返回，不会声称已经读过。

## 验证入口

- `cargo test attachments::tests`：会话/事务/重试、白名单、图片引用与原件、DOCX 表格、PDF 文本及页面、视觉副本预算。
- `cargo test image_analysis_is_a_native_tool_exchange_without_context_handoff`：接纳图片后走真实 Agent 工具循环，隔离模型服务验证图像字节与同会话返回；并检验跨任务与取消。
- `cd frontend && node --test tests/attachments.test.mjs`：主聊天框粘贴、拖拽、移除、附件独立提交与发送失败保留（默认复用邻近 ai-ppt-view 的 Playwright，可通过 `PLAYWRIGHT_MODULE` 指定）。
- AI PPT 前端：`npm run lint`、`npm run test:ppt-agent`、`npm run test:ppt-plugin`，覆盖原站及两种插件页面。
- AI PPT 后端：`go test ./service/ppt_runtime ./api/v1/system -run 'TestAttachment|TestAsyncTask|TestPpt' -count=1`。

浏览器回归使用隔离登录、接口与模型服务；它不代替真实模型对图片理解和生成审美的人工验收。附件暂存垃圾回收与配额管理、DOCX 图片提取、完整 PPTX 导入仍待后续。
