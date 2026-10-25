# UniAI 五页《Agent 设计要点》真实测试

日期：2026-10-23（Asia/Shanghai）。本轮实际模型执行时间约为 23:42–23:47。

## 结论

**实际模型已创建项目并保存五页大纲；编辑器打开后字体接口返回 HTTP 503，本轮到此停止。** 没有确认大纲、没有制作五页正文、没有完成视觉验收或导出。

这次已走通本地前端 → 本地 Agent → 本地 Go PPT 后端及真实模型请求，不再使用之前返回握手 404 的远程后端。

## 任务与环境

- 用户确认模板：中国联通 UniAI 品牌模板，`china_unicom_uniai`。
- 内容：恰好 5 页《Agent 设计要点》，面向研发团队。
- 失败策略：发现问题立即停止，不自动重试或修复。
- 项目：808。
- Agent 会话：`task_8f4abfe4d6a870baca6c3c3b62f1d8e3`。
- 模型：现有配置的 `gpt-6-luna`，`reasoning_effort: high`。
- 后端：`codex/ppt-remote-svg`，`8fb747b`；本机端口 18897。
- 前端：`codex/ppt-host-agent`，`be8a1db`；本机端口 16011。
- Agent：包含任务/步骤迭代器及按主任务总结记忆的当前工作区构建；本机端口 13001。
- 渲染服务：本机端口 8787。
- 数据库、Redis、MinIO 沿用项目既有配置；没有将这些基础服务也改成本地。

## 实际过程

1. 页面可选择模型，模板服务正常返回数据。
2. 操作者先在页面选中“数字大脑 · 联通红”；后来提交记录中出现“中国联通 UniAI 品牌模板”。因无法即时确认模板变化来源，操作者停止第一轮进行核对。本轮证据不足以将此认定为前端自动替换模板的缺陷。
3. 用户明确“就选 uniai 模板”后，通过真实页面重新选中“中国联通 UniAI 品牌模板”，核对选中状态，再向同一任务发送继续指令。
4. 首轮 `init_project` 只调用一次，异步初始化成功创建项目 808，初始模板自带 4 页、`deck_revision=1`。这是模板原稿，不是已生成的五页成品。
5. 第二轮接收原初始化结果并复用项目，没有重复创建项目或主任务。
6. 模型调用 `commit_design_spec` 保存了五页大纲，计划版本 112，页面自动进入项目 808 的编辑器。
7. 编辑器显示 `Font backend returned HTTP 503`。发现后没有点击“确认大纲并生成”，停止后续制作。此时模型轮次已结束、正在等待大纲确认，没有继续模型调用。

## 首个明确故障

后端已有日志在 23:46:50 记录：

```text
GET /prod-api/aiPpt/fonts/api/fonts -> HTTP 503
{"error":"fontconfig is unavailable; install fontconfig and fonts-noto-cjk, fonts-crosextra-carlito, fonts-crosextra-caladea, fonts-liberation"}
```

`service/ppt_runtime/fonts.go` 使用外部 `fc-list` / `fc-match` 获取字体目录和字体文件；命令查找或执行失败时统一返回 `fontconfig is unavailable`。当前 shell 中也无法通过 `Get-Command` 找到这两个命令。

因此已确定的直接阻塞是**本地后端的字体运行环境不可用**，进而导致编辑器加载失败。已有工具文件位于之前准备的 fontconfig 目录，并不代表当前后端进程可以找到并正常执行它们；本轮没有进一步执行这些程序、安装依赖或修复后重试，具体 DLL 依赖和字体安装情况尚未逐项验证。

## 已验证的迭代器行为与边界

- 总计 9 次模型请求，包括首轮停止时被取消的 1 次请求。
- 模型主动调用 `create_task` 1 次、`create_step` 2 次，形成 1 个主任务、3 个步骤；步骤数量少于请求数量。
- 跨轮恢复继续原任务和当前步骤，接收已有异步初始化结果；`init_project` 总共只调用 1 次。
- `commit_design_spec` 调用 1 次，五页大纲真实落地并显示为待确认。
- 宿主会话状态 `completed` 表示当前轮结束，**不表示 PPT 主任务或最终成品完成**。任务仍在等待计划确认。
- 没有制作新正文页，未调用页面写入、视觉验收或导出；没有验证临时任务分支、压力折叠或下一主任务的记忆索引。

工具调用统计：

| 工具 | 次数 |
| --- | ---: |
| create_task | 1 |
| create_step | 2 |
| list_tool_groups | 1 |
| ppt_master__read_files | 1 |
| ppt_master__project | 1 |
| ppt_master__init_project | 1 |
| ppt_master__commit_design_spec | 1 |

## 保留状态与证据

本轮没有改动业务代码，也没有删除项目。前端、Agent、后端和渲染服务保留运行，模型不再执行，计划版本 112 未确认。失败监测辅助脚本已停止。

- `target/ppt-local-backend-20261023/evidence/font-backend-503.png`：字体错误及待确认大纲截图。
- `target/ppt-local-backend-20261023/evidence/real-test-summary.json`：模型请求、工具计数、结束状态与项目标识。
- `target/ppt-local-backend-20261023/evidence/test-events.json`：按事件顺序保留的模型和工具记录。
- `target/ppt-local-backend-20261023/backend.stdout.log`：字体接口 HTTP 503 及错误响应。
- `target/ppt-local-backend-20261023/agent.stdout.log`：真实模型请求和首轮取消记录。

下一次继续之前，需要先让本地后端的字体目录接口正常返回、编辑器能够加载，再从项目 808 的待确认大纲继续。本轮未自动开展这些修复。

2026-10-24 更新：按用户要求拉取并构建指定的 pptx-editor-engine，前端与渲染服务已切换到 `76fdb64`；字体 503 仍存在，未恢复生成。详见 [引擎更新与集成记录](ppt-engine-integration-update-2026-10-24.md)。
