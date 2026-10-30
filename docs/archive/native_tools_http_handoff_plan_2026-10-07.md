# 原生工具替代 PowerShell 与 HTTP 观测跨节点交付实施计划

日期：2026-10-07

本计划处理最近审查的第 1、2 项。根据用户最新决定，第 1 项改为移除 PowerShell 执行入口，以 Rust 原生工具取得真实执行结果；第 2 项补齐 HTTP 观测的节点交付和上下文输入。此次不继续修补 PowerShell 的编码或输出解析器。

## 一、已确认的问题

1. `execute_run_command` 启动 `powershell.exe`，取得的是 shell 的退出码。原生子程序退出码为 9 后，再执行成功的输出语句，宿主仍得到退出码 0。日志混合错误 JSON 也无法被当前纯 JSON 解析识别，随后 `WorkScheduler::observe` 可能把它记为检查通过。
2. `http_probe_results` 是调度器缓存，没有自动出现在 Worker/Organizer 输入和节点的 `exported_data` 中。上游结果虽保留在 `operations`，默认的依赖交付过滤不传递这些过程日志；因此没有明确导出的 HTTP 事实会在节点切换后缺失。
3. 当前 HTTP 工具本身已通过临时回环服务检查：200、404、拒绝连接、超时、302 不跟随跳转。应复用它，重点补执行结果与交付机制。

代码入口：

| 模块 | 主要修改位置 |
| --- | --- |
| 原生执行 | 新增 `src/program_execution.rs`；复用 `src/project_process.rs` 的运行时定位与进程管理能力 |
| 工具接入 | `src/main.rs`、`src/mcp.rs`、`src/plugin_builtin.rs`、`src/agent_service.rs` |
| 执行事实与完成判定 | `src/work_scheduler.rs`、`src/work_executor.rs`、`src/worker_work_state.rs` |
| HTTP 观测 | `src/http_probe.rs`、`src/work_scheduler.rs` |
| 提示与交付契约 | `prompts/worker_system.md`、`prompts/worker_unit.md`、`prompts/organizer_system.md` |
| 展示与说明 | 前端工具卡片、上下文调试面板、`README.md`、`docs/project_process_tools.md` |

## 二、第 1 项：使用原生工具取代 PowerShell

### 2.1 工具使用原则

- HTTP 探测使用 `http_probe`；文件读写和搜索使用已有文件工具；浏览器操作使用已有浏览器工具。
- npm 安装、启动和脚本执行继续使用 `install_dependencies`、`run_project_script`、`get_project_process`、`stop_project_process`。
- 需要执行已安装的开发程序时，提供原生 `run_program`，以程序与参数数组调用 `tokio::process::Command`。
- 缺少能力时返回具体缺口，由组织者决定下一步；不得拼接 PowerShell，或通过 Node/Python 转发 PowerShell。

### 2.2 首版 `run_program` 接口

示例输入：

```json
{
  "program": "cargo",
  "args": ["check"],
  "project_path": ".",
  "timeout_seconds": 120
}
```

- `program` 使用宿主支持的程序名称，首版覆盖 `cargo`、`git`、`node`、`python`；宿主负责定位实际可执行文件，后续按具体需求补支持。
- `args` 是字符串数组，每个元素作为独立参数。没有 `command`、`shell`、`script` 字符串入口，也不提供 PowerShell 或其他通用 shell 程序选项。
- `project_path` 按现有工作区规则解析；超时和输出长度采用明确上限。
- 首版只做前台执行。后台项目启动沿用现有托管进程工具，避免再造一套进程注册表。
- 项目脚本若显式依赖 PowerShell，应报告能力限制，给出具体脚本名称；不能悄悄启动 PowerShell。npm 的脚本运行方式必须在文档中如实说明。

示例输出：

```json
{
  "program": "cargo",
  "args": ["check"],
  "outcome": "exited",
  "process_exit_code": 0,
  "process_success": true,
  "elapsed_ms": 1200,
  "stdout": "",
  "stderr": "Finished ...",
  "stdout_truncated": false,
  "stderr_truncated": false,
  "check_key": "program:cargo:<normalized-project>:<args-json>"
}
```

`outcome` 只需区分 `exited`、`spawn_failed`、`timed_out`。正常结束记录实际程序退出码；未成功启动或超时，退出码为 `null`，成功标志为 `false`。stderr 中有文字不自动判为失败，因为编译器和其他程序会向 stderr 写进度或警告。

`process_success` 只表示实际程序是否以 0 退出。普通 stdout 不自动作为业务结果，也不再从混合日志猜测 JSON 的成功与失败。HTTP 等业务判定由专用工具返回。输出按明确编码解码；解码失败应标明并保留原始材料，不得静默丢弃原始字节后声称正常。

### 2.3 完成判定与执行事实

- `check_key` 由统一函数生成，参数顺序和工作目录进入键值；组织者声明的检查与工具返回使用同一算法。
- `run_program` 只有命中当前节点声明的检查、实际程序退出码为 0、相关源码版本未改变，才记录检查通过。
- 启动失败、非零退出、超时均不能清掉已有检查错误，也不能触发节点完成。
- HTTP 检查继续使用 `check_passed`；托管服务启动继续检查进程存活、监听归属和就绪状态，不统一套成“退出码 0”。
- 将新工具纳入权限模式、用户禁止执行指令、文件变更收集、源码版本失效、实际操作记录与上下文调试。不能只注册模型工具却漏掉宿主执行链。
- 超时和取消使用原有进程组管理思路，避免只结束父进程而遗留子程序。

### 2.4 停用旧入口

- 删除生产路径中的 `execute_run_command` 及 PowerShell 启动代码。
- 删除工具目录、插件注册、模型工具列表和提示词里的 `run_command` 入口。旧工具调用返回明确的已停用说明及替代工具名称，不执行旧命令。
- 保留旧聊天事件、工具卡片和文件变更记录的历史展示。
- 旧的未完成订单如果声明了 shell 字符串检查，必须由组织者以新 revision 重建当前节点的执行契约；不能自动解释任意旧 shell 文本，也不能重跑已经完成的上游。
- 组织者与 Worker 的提示统一改为“专用工具优先，支持的原生程序其次，缺能力时交回具体缺口”。

## 三、第 2 项：把 HTTP 观测作为明确的节点产出

### 3.1 保存样本与来源

为每个 HTTP 样本保存稳定 ID 和来源：`sample_id`、`producer_work_id`、`node_id`、`revision`，以及现有 `url`、`sampled_at`、`elapsed_ms`、`reachable`、`http_status`、`error_kind`、`error_message`、`check_passed`。

每个 WorkFrame 保存该节点的 HTTP 观测，调度器复用索引引用这些样本。复用不能生成新的采样时间或把旧样本伪装成新请求。沿用现有 scheduler 快照持久化，不为此次改造增加新的状态机或另一个模型角色。

正文摘要保留在样本或记事本材料中；默认上下文仅交付状态结论。模型需要正文时再取对应材料。

### 3.2 自动交付，避免依赖 Worker 手写结论

- `close_if_satisfied` 和 `return_work` 自动将本节点 HTTP 样本写入 `exported_data.http_observations`。
- 导出失败样本与成功样本，不能只传递 200；明确表达“服务被连接拒绝”“已收到 HTTP 404”等不同事实。
- `operations` 继续用于过程审计，不能作为唯一的状态存储；即使操作日志超出 16 条被裁剪，HTTP 交付仍存在。
- 业务查询任务可能正常回答“接口返回 404”；工具执行成功、服务状态、当前节点检查是否通过应分别表达。

### 3.3 组织者绑定依赖，Worker 只收到当前所需事实

组织者输入增加精简 HTTP 观测目录：URL、生产节点、状态、采样时间及宿主计算的有效性。目录用于决定下一任务依赖，不携带整段响应正文。

沿用 `dependency_inputs`，允许按 URL 筛选 HTTP 交付，例如：

```json
{
  "work_id": "A",
  "fields": ["http_observations"],
  "http_urls": ["http://127.0.0.1:8080/api/fonts"]
}
```

- `resolve_dependency_deliveries` 返回所声明的样本与对应来源信息。
- `worker_input` 注入本节点已有观测、显式绑定的上游 HTTP 观测及有效性。不要向每个 Worker 灌入整个全局缓存。
- 无依赖、无当前需求的其他 URL 不进入 Worker 上下文。
- Observer 消费同一份宿主观测及有效性，避免它与 Worker 使用不同版本的状态结论。
- 这样 A 检查字体接口后，B 可以直接获得“字体接口已返回 200、何时检查、目前是否仍有效”，无需再次调用工具恢复材料。

### 3.4 宿主判断新旧与有效性

组装上下文时由宿主计算 `age_ms`、`fresh` 和必要的 `invalidation_reason`；`sampled_at` 保留 UTC 时间用于展示。不要要求模型自行做时间减法来决定是否可复用。

- 新样本成为同一 URL 的当前结论；旧样本保留为历史，不与当前样本并列成为无区别的有效结论。
- 超过复用期限、相关服务重新启动/停止/退出、宿主重启、生产节点被废弃时，不得继续把样本视为当前结论。
- 接入已有进程事件或新进程观测，确保已明确知道服务退出时，HTTP 200 缓存同步失效。不能只依赖“是否执行过一条命令”。
- 外部服务无法保证永远不变；时间窗口和失效信息应明确展示。Worker 有具体状态变化理由时可取得新样本。
- 若当前节点声明的是同一 URL 的纯 HTTP 2xx 检查，且绑定的样本仍有效，宿主可直接消费该样本满足这项检查。其他条件仍由原有节点完成规则判断，不能因为一个 200 就代替 PPT 加载、字体正文或浏览器画面检查。

## 四、实施顺序

1. 新增原生执行模块与 `run_program`；完成退出码、超时、取消和输出边界。
2. 注册新工具，并贯通权限、检查键、源码版本、修改记录和 Worker 进度记录。
3. 停用所有生产 `run_command` 入口，处理未完成旧订单的明确迁移；更新提示和工具说明。
4. 增加节点 HTTP 样本及自动导出，贯通组织者目录、依赖筛选、Worker 输入与 Observer 观测。
5. 补齐有效性计算和相关服务事件失效，再进行端到端验收。

每个步骤尽量独立提交，提交信息使用中文。此计划阶段不重启服务、不提交新的 Agent 用户任务。

## 五、验收条件

| 场景 | 必须观察到的行为 |
| --- | --- |
| 实际程序退出 9 | 工具返回退出码 9，声明的检查不通过，节点不能完成 |
| 参数包含空格、引号、中文 | 按独立参数正确传递，宿主没有拼接 shell 字符串 |
| 程序不存在、超时、用户取消 | 返回明确结果；不误判成功；结束受控子进程 |
| 用户禁止执行或权限不足 | 新原生程序工具与安装/脚本工具同样受到约束 |
| 旧 `run_command` 被模型强行调用 | 返回已停用及替代能力，不启动 PowerShell |
| A 探测字体 URL，B 消费结论 | B 的实际模型请求包含指定样本、来源、时间及有效性，零次额外恢复查询 |
| A 探测后又执行 20 个其他工具 | 指定 HTTP 产出不因操作日志裁剪而丢失 |
| 上游 HTTP 404/连接拒绝 | 下游获得真实状态及错误类型，不能当作 HTTP 0 或服务成功 |
| 上游绑定同 URL 的有效纯 HTTP 检查 | 宿主消费已绑定样本，零次新网络请求及模型取回调用 |
| 服务退出/停止/重启或宿主重启 | 旧 HTTP 成功样本失效，不与新失败结论同时标为有效 |
| 新样本取代旧样本、节点回溯 | 当前结论唯一；旧记录保留历史来源；废弃节点的样本不继续作为有效依赖 |
| 上游还有其他 URL 或大正文 | B 的请求只携带声明的必要观测，保持节点上下文独立 |

验收优先使用本机确定性样例与模型模拟，检查真正组装的请求上下文和宿主完成判定。再用“启动项目、检查字体接口、打开示例 PPT”的真实任务观察过程：执行事实由原生工具取得，HTTP 状态沿节点交付；遇到能力缺口给出具体阻塞，不能重新落回 PowerShell。
