# 任务/步骤迭代器真实 PPT 测试：登录前置阶段停止

日期：2026-10-23，Asia/Shanghai。

## 测试目标和停止条件

使用用户指定账号，在联通模板上生成 5 页《Agent 设计要点》。用户明确要求发现问题立即停止并复盘。本轮未授权失败后重试或修复后继续。

## 结果

**本轮在登录验证阶段发现测试后端请求超时，已停止。没有进入真实模型生成阶段，不能据此判断新版迭代器的真实模型效果。**

- 未完成账号登录，未选择模板，未创建 PPT 项目，未生成或导出 PPT。
- 新建的隔离 Agent 数据库中，任务、模型请求和工具调用均为 0，见 `target/ppt-task-iterator-real-20261023/evidence/stop-summary.json`。
- 发现问题后没有手动重试，没有切换其他后端，也没有修改业务源码。已停止本轮启动的前端和 Agent 进程，终止页面自动请求。

## 环境

- Agent：从当前工作区执行 `cargo build --offline --bin codex-workspace-mcp`，构建成功，包含任务/步骤迭代器和按主任务总结的记忆改动。
- Agent 使用独立工作区和配置目录：`target/ppt-task-iterator-real-20261023`，监听 `127.0.0.1:13001`；未恢复上次暂停的任务。
- 前端：已有 `codex/ppt-host-agent` 工作目录，Rspack 编译成功，监听 `127.0.0.1:16011`。
- 后端地址沿用上次真实测试环境：`http://192.168.100.114:18897/prod-api`。远端当前运行版本未能确认。
- 后端分支的本地源码为 `codex/ppt-remote-svg`；本轮没有部署或重启远端后端。
- 没有保存用户提供的账号密码到本报告或证据摘要。

## 已观察到的问题

打开前端后，页面先停在“正在验证登录状态...”。前端代理日志随后明确记录：

```text
Error occurred while proxying request 127.0.0.1:16011/user/getUserInfo
to http://192.168.100.114:18897/prod-api [ETIMEDOUT]
```

此后页面自身发出的 `/jwt/jsonInBlacklist` 和 `/auth/yalongjiang/status` 请求也记录了同一目标的 `ETIMEDOUT`。停止时页面显示“正在跳转统一身份认证...”，未进入账号密码登录表单。

这是实际观察到的前置连接故障，并非模型生成失败。现有证据只能确认请求超时，不能确定是服务未启动、网络路由、防火墙还是上游处理阻塞。

## 代码复盘

`ai-ppt-view/src/permission.tsx` 的认证守卫在已有 Token、角色尚未加载时调用 `getInfo()`；其 20 秒超时处理还会等待 `userStore.logOut()`。

`ai-ppt-view/src/store/UserStore.ts` 的 `logOut()` 又请求后端退出接口，之后才清理本地状态。这使原认证请求超时时，退出请求也可能延迟登录页恢复。页面后续实际进入统一身份认证检查，而该检查使用同一不可正常响应的后端。

这段代码路径可以解释阻塞为何延长；网络超时的底层原因仍未验证。本轮仅记录，没有修改或绕过认证流程。

## 证据

- `target/ppt-task-iterator-real-20261023/frontend.stderr.log`：代理目标与超时记录。
- `target/ppt-task-iterator-real-20261023/frontend.stdout.log`：前端构建结果。
- `target/ppt-task-iterator-real-20261023/agent.stdout.log`：最新 Agent 启动记录。
- `target/ppt-task-iterator-real-20261023/evidence/auth-blocked.png`：停止时的页面截图。
- `target/ppt-task-iterator-real-20261023/evidence/stop-summary.json`：隔离数据库中的零任务、零模型请求、零工具调用统计。

下次测试前需要先恢复该后端地址的可用性，或明确当前应使用的测试后端地址。本轮到此停止。

## 用户授权重试：23:23–23:24

用户随后要求“再试试”。重新启动同一份最新 Agent 和前端，使用新的隔离工作区 `target/ppt-task-iterator-real-20261023-retry1`，继续遵守发现问题立即停止的条件。

### 本轮结果

- 本次登录成功，页面显示 `admin`，可进入“PPT管理 → 创建PPT”。上次的登录接口超时在本次登录过程中未复现。
- 创建页显示以下错误：

```text
remote plugin HTTP error: HTTP status client error (404 Not Found) for url
(http://192.168.100.114:18897/prod-api/aiPpt/plugin/api/plugin/handshake)
```

- 模型选择器显示“暂无可用模型”，生成、附件和图片按钮均被禁用。
- 发现该错误后立即停止，没有继续选择模板、提交需求或重试接口。本轮启动的前端和 Agent 进程均已停止。
- 隔离数据库中的任务、模型请求和工具调用仍全部为 0，见本轮 `evidence/stop-summary.json`。5 页联通模板 PPT 尚未生成，真实模型效果仍未验收。

### 复盘

当前 Agent 的 `src/skill_packages/remote.rs` 在 `connect_user` 中调用通用协议 `plugin/handshake`；当前后端分支的 `router/system/ppt_plugin.go` 已声明 `POST aiPpt/plugin/api/plugin/:method`。而本次实际远端地址对该握手路径返回 404。

因此当前阻塞是**实际部署服务或网关路由与本地新协议未对齐**。不能把页面的“暂无可用模型”解释为模型提供商故障，因为尚未走到模型请求。404 也不能单独证明远端一定运行旧版本，仍需核查实际后端版本及该路径的转发规则。

下一步应核对 `192.168.100.114:18897` 的实际版本与路由，或确认应该使用的新后端地址；本轮没有部署、切回旧协议或修改业务代码。

本轮截图：`target/ppt-task-iterator-real-20261023-retry1/evidence/handshake-404.png`。其余日志在同一运行目录，账号密码未写入报告或证据摘要。

## 按用户要求准备本地后端

用户要求后端也在本地启动，并确认代码是否已更新。

- 对后端 `codex/ppt-remote-svg` 和前端 `codex/ppt-host-agent` 分别执行 `git fetch` 与 `git merge --ff-only`，均返回 `Already up to date`，提交分别为 `8fb747b`、`be8a1db`。之前两轮未重新 fetch，本轮已核实远端最新状态。
- 从上述后端工作目录执行 `go build` 成功，生成 `target/ppt-real-e2e-20261022/ai-ppt-server-local.exe`。
- 本地配置与前端启动脚本准备在 `target/ppt-local-backend-20261023`，前端 API 与 Agent 的插件服务地址均指向 `127.0.0.1:18897`，不再指向远程后端。
- 本地配置保留项目原有的 MySQL、MinIO 和鉴权设置；不代表数据库、对象存储也改为本地。全局自动表结构迁移在本地配置中关闭。
- **后端尚未启动。** 启动进程被工具自动审批拒绝，返回 `blocked by policy`，未给出更具体原因。采用保留原鉴权配置的启动方式后仍被拒绝，未继续尝试其他执行入口。
- 本轮没有启动真实模型测试。不能将配置准备和编译成功表述为本地服务已运行。

### 后续启动成功（23:34–23:37）

用户再次要求启动后，在当前权限配置下启动成功：

| 服务 | 本机端口 | 进程 |
| --- | --- | --- |
| Go PPT 后端 | 18897 | 36516 |
| 前端 | 16011 | 13896 |
| Agent | 13001 | 35028 |
| PPT 渲染服务 | 8787 | 9384 |

已实际检查四个端口均在监听，后端健康接口、前端创建页与渲染健康接口均返回 HTTP 200。未登录的本地插件握手请求返回 HTTP 401、`missing_token`，证明该路径不再是此前远端的 404；这不代表已验证带账号的完整握手。

后端使用项目原有数据库、Redis、MinIO 和鉴权配置。初始化期间存在本机 Docker 未运行导致的 OnlyOffice 插件目录同步警告，但后端最终启动并提供 HTTP 服务。

服务日志与进程信息保存在 `target/ppt-local-backend-20261023`。浏览器自动刷新被浏览器安全策略拒绝，未通过其他浏览器入口绕过；需要用户手动刷新现有页面。本轮仅启动与检查服务，没有发起 PPT 生成。

### 后续真实模型执行

用户随后要求操作测试，并明确选择 UniAI 模板。实际模型已创建项目 808 并保存五页大纲，因编辑器字体接口 HTTP 503 停止，未确认大纲、制页或导出。完整过程、迭代器验证范围和证据见 `docs/ppt-uniai-five-page-real-test-2026-10-23.md`。
