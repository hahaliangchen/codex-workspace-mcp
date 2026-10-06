# 项目执行工具

Agent 和 MCP 都提供以下项目执行、进程管理和本机 HTTP 检查工具。当前项目脚本支持 **Node.js / npm**，宿主环境需要能从 PATH 找到 Node 和 npm。项目目录必须在所选工作区内，并包含 `package.json`。

| 工具 | 用途 | 主要参数 |
| --- | --- | --- |
| `install_dependencies` | 安装依赖，返回退出码与日志 | `project_path`、`mode`、`timeout_seconds` |
| `run_project_script` | 运行 package.json 中已存在的脚本 | `project_path`、`script`、`args`、`background` |
| `get_project_process` | 列进程、查询状态、增量读日志、探测就绪 | `process_id`、`after_seq`、`ready_url` / `ready_port` |
| `http_probe` | 独立检查本机 HTTP(S) 地址，可探测非 Agent 启动的服务 | `url`、`timeout_ms`、`reason` |
| `stop_project_process` | 停止受管理的进程及其子进程 | `process_id` |

`project_path` 默认 `.`。安装的 `mode=auto` 默认有 npm 锁文件就用 `npm ci`，否则用 `npm install`；也可以明确选择 `install` 或 `ci`。不能用这些工具安装单个包或执行任意命令字符串，pnpm/yarn 尚未接入。

Agent 的安装、运行、停止操作需要“完全访问”；状态和日志查询可在只读模式使用。用户明确禁止执行命令时，同样关闭安装、运行和停止工具。MCP 入口沿用受信任工作区工具的调用方式。

`get_project_process` 只报告 Agent 管理的进程、归属和日志。无受管进程不代表 URL 不可达。通用 URL、字体接口和页面连通性检查使用 `http_probe`：它仅连接 loopback 地址，禁用代理与自动重定向；任何 HTTP 响应（包括 404）都表示 `reachable=true` 并返回实际状态码，未收到响应则 `http_status=null` 并返回结构化 `error_kind`。返回的 `sampled_at` 是 UTC RFC3339 时间，`reuse_window_ms` 为 30000；在窗口内且服务状态未改变时复用已有结果，`reused=true` 表示没有再次发出网络请求。重新探测时在 `reason` 中说明状态可能变化或旧结果无法回答新问题的原因。

## 使用顺序

安装当前工作区依赖：

```json
{"project_path":".","mode":"auto","timeout_seconds":600}
```

通过 `run_project_script` 构建 frontend，等待结果：

```json
{"project_path":"frontend","script":"build","timeout_seconds":120}
```

启动开发服务器并检查 Agent 管理进程的监听归属和就绪状态（脚本和端口须以该项目实际配置为准）：

```json
{"project_path":"frontend","script":"dev","args":["--port","5173"],"background":true,"ready_url":"http://127.0.0.1:5173/","wait_seconds":15}
```

返回的 `process_id` 用于后续查询和停止。`running` 只表示进程还活着；`ready` 表示进程仍运行且指定本地 HTTP 地址返回成功状态，或指定 TCP 端口可连接。HTTP 不跟随重定向。未配置探测时 `readiness=not_configured`，不能宣称服务已就绪；探测等待结束仍未就绪则返回 `pending`，保留进程供查询。启动前检查指定端口是否已占用；就绪探测不是页面功能验收。

增量查询：

```json
{"process_id":"<启动返回的 process_id>","after_seq":0,"wait_seconds":5}
```

下一次把 `after_seq` 设为上次返回的 `next_seq`。`has_more` 表示本次长度限制下还有日志；`log_gap` 表示更早日志已被内存环形缓冲淘汰。省略游标时返回最近日志。停止使用同一 `process_id` 调用 `stop_project_process`，重复停止返回已退出状态。

## 执行与日志

宿主直接启动 `node npm-cli.js`，参数按数组传递，不拼接 PowerShell 命令，也不打开终端窗口。npm 运行 package.json 脚本和生命周期脚本时，仍会使用 npm 自己的脚本执行机制；脚本拥有宿主账户权限。

前台默认安装超时 600 秒、普通脚本 120 秒，最大 1800 秒。超时或所属 Agent 请求取消时终止进程组。后台执行成功返回后由宿主继续管理，同一工作区、目录、脚本和参数的重复后台请求复用运行中的进程。后台服务明确停止或宿主退出时结束，不因为 Worker 完成当前回答自动停止。

Windows 使用 Job Object 管理完整进程组，关闭作业时杀掉子进程；Unix 使用进程组。只接受宿主管理的 ID，不能停止任意系统 PID。宿主重启后，旧 ID 仅作为历史记录读取，不使用旧 PID 控制新进程。历史日志的 `cursor_reset=true` 提醒调用方改用 `after_seq=0` 重新读取历史日志，历史查询不提供实时就绪。

状态和日志保存在工作区 `.codex-workspace-mcp/project-processes/`。内存保留最近约 64,000 字符；每进程磁盘日志约 8 MiB 上限，达到上限后继续排空 stdout/stderr 并保留内存尾部，避免堵塞子进程。输出包含日志文件位置和截断/写入错误。宿主最多同时管理 16 个进程，内存最多保留 128 份进程记录；磁盘历史不会自动清理。无 ID 的列表只列本次宿主内存中的记录。

安装和运行脚本接入文件变更记录。后台脚本可能在返回后持续改文件，启动轮次会标记变更区间不完整，关闭完整一键撤销，避免遗漏后续写入。

## Flow 完成条件

组织者声明检查标识：`npm-install:.`、`npm:frontend:build`。带参数的启动标识例如 `npm-start:frontend:dev:["--port","5173"]`。前台退出码为 0，或后台脚本的 `running=true && ready=true`，才能产生成功检查记录；仅创建进程不计入完成。源码版本变动会使旧检查失效。尚未就绪的查询保留当前单元，失败则把真实日志返回给 Worker 处理。
