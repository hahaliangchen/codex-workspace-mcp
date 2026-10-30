# 全量重新构建与重启

构建目录：`target/ppt-local-backend-20261023/rebuild-20261024-231610`。

全部基于当前本地源码和未提交改动构建，没有拉取、重置或覆盖用户源码。

- PPT 引擎：重新生成浏览器与 Node 两份 release WASM 包，使用现有离线依赖；按既有环境使用 `--no-opt` 跳过 wasm-opt。重新打包独立引擎至本次构建目录的 `engine-dist/`。
- 工具定义：由当前引擎重新生成 `ai-ppt-server/service/ppt_runtime/page_tools.json`，再构建 PPT 后端。
- PPT 前端：类型检查和生产构建通过，直接编入 `D:/enterpriseProject/pptx-editor-engine/src`。整合记录确认包含当前 `api/layout-check.ts`、`api/tools.ts`、`design/measure-dom.ts` 和 `page/page-tools.ts`，保存了源码 SHA256，确认构建期间这些输入未变化。重启的开发服务同样指向该引擎，启动日志显示 151 个引擎 JavaScript 模块及新 WASM 已编入。
- Agent：重新构建 Web 页面和 Rust 可执行程序。

运行服务：

| 服务 | 端口 | PID |
| --- | --- | --- |
| PPT 前端 | 6001 | 25628 |
| PPT 后端 | 18897 | 5184 |
| Agent | 3001，附带 MCP 3000 | 7620 |
| Node 渲染服务 | 8787 | 8264 |

PPT 后端与 Agent 都直接运行本次构建目录里的新可执行文件。原 `target/debug/codex-workspace-mcp.exe` 被其他进程占用，因此未强行覆盖或结束无关进程；运行路径已写入 `runtime.json`，重启脚本会读取对应路径。

后端健康、Agent 信息与页面、渲染健康、PPT 编辑入口均返回 HTTP 200，前端转发到后端的 `/dev-api/health` 也返回 HTTP 200 和 JSON。前端启动初期的两条代理连接拒绝发生于后端就绪之前，后续代理检查已通过。

没有运行真实 AI/PPT 生成测试，没有修改数据库连接或重置数据库。构建仍有原有编译、CSS 顺序、包体积及依赖弃用警告，未出现最终构建错误。

完整产物哈希、源码整合记录和进程状态见构建目录的 `manifest.json`。各组件构建及启动日志保存在同一目录。PPT 页面刷新后重新连接新 Agent/插件服务。
