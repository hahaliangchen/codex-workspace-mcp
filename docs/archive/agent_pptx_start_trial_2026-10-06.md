# 真实 Agent 启动 PPT 编辑器试跑

日期：2026-10-06。目标项目：`D:/enterpriseProject/pptx-editor-engine`。

## 结果

已给用户自己的 Agent 下达任务并实际执行，**PPT 页面未启动成功**。当前两次试跑均已停止，避免继续无效调用或基于错误诊断修改全局 npm。没有把 Agent 页面当成 PPT 页面，没有直接代替 Agent 启动 PPT 项目，也没有修改业务源码。

此前视觉链路回归通过，只说明当时复核范围内的修复有效；本次真实任务暴露了另外的规划契约、状态重复核实和项目进程工具问题。

## 试跑与干预

1. 本机 3001 没有运行中的 Agent 服务，先构建当前宿主并恢复服务，工作区绑定 PPT 项目；开启请求上下文调试。
2. 原始自然任务要求按现有方式启动 PPT 页面。首次组织者连续三次输出 request_action=replace；首次任务没有 session_continuation handoff，调度器拒绝，Worker 未运行。
3. 人工指出沿用当前目标后，组织者又连续三次把 current_node_id 放在 flow_update.plan 内，而宿主要求 flow_update.current_node_id，任务再次失败。
4. 人工纠正字段层级后，Worker 读取 package.json、rspack.config.js、README，确认 `npm run dev` 和前端 3000 端口。
5. 此时 3000 被本轮恢复的 Agent MCP 辅助接口占用。启动工具正确识别它是 workspace host，拒绝误认。该冲突属于试跑环境问题，不能归责于 PPT 项目。组织者尝试调查替代端口，并出现修改已完成节点目标的错误。
6. 为保留 PPT 默认端口，将本轮创建的 Agent 宿主 MCP 接口移至 3002，Agent 页面仍在 3001；通知原任务旧冲突已变为历史。期间未启动 PPT 进程。
7. 原任务又多次安排“核实没有运行实例”；get_project_process 共调用 6 次，仍未进入有效启动。结束这段空转。
8. 创建第二次有明确已知启动条件的任务，要求直接使用 run_project_script 执行根目录 dev。该任务第一步实际调用了启动工具，但 npm 在项目脚本执行前失败，退出码 7。
9. 组织者将错误解释为全局 npm 缺依赖，并派发修复。实际依赖存在；Agent 只执行了两次只读 run_command 检查后，本轮停止，未让错误修复继续。

因此，此次不是“Agent 自主启动成功”的验收。人工纠正、环境调整与提供明确命令均计入试跑过程。

## 确认的工具缺陷：[P1] npm-cli.js 的 Windows 规范化路径破坏模块解析

位置：`src/project_process.rs:370`。

Node/npm 发现函数返回 `cli.canonicalize()`，Windows 得到带 `\\?\` 前缀的 npm-cli.js 路径，并传入 Node 执行。当前 Node v22.5.1 / npm 10.8.2 下，这导致 npm CLI 解析自身依赖失败：

```text
Error: Cannot find module 'graceful-fs'
Require stack: \\?\C:\...\node_modules\npm\lib\cli\entry.js
```

本轮做了同一 Node、同一 npm 文件的只读对比：

| 调用 | 结果 |
| --- | --- |
| node 普通路径/npm-cli.js --version | 10.8.2，成功 |
| node 带 \\?\ 前缀路径/npm-cli.js --version | graceful-fs MODULE_NOT_FOUND |
| 正常路径 npm/node_modules/graceful-fs/package.json | 存在 |

**根因不是已证明的依赖缺失；当前环境下是宿主传参路径兼容性问题。** 不应通过 npm install、重装全局 npm 或修改项目 package.json 处理。

修复要求：内部权限与工作区判断仍使用 canonical 路径；在外部程序启动边界转换为 Node/npm 可用的普通 Windows DOS/UNC 路径，覆盖 fnm 符号链接安装和实际本机 npm CLI 运行回归，不仅检查进程是否创建。随后重新测试 install_dependencies / run_project_script 的实际依赖解析。

## 规划与状态问题

- 首次规划的 request_action 选择与宿主许可不匹配；错误重试三次没有纠正。应按当前生命周期收窄可选契约，并提供明确修复反馈。
- flow_update 是宽泛 object，节点和 current_node_id 的结构只靠描述；模型的两种不同层级写法分别被忽略或拒绝。应使用明确 JSON schema，并让错误指出字段路径、合法节点 ID 与期望结构。
- 已完成节点目标不可改是正确约束；组织者仍尝试改写，应收窄修改方式并提供新节点/revisit 的明确输入。
- 端口释放后的状态核实被重复分派，已有“processes=[]”交付未能迅速驱动启动。应区分需要重新采样的状态和已可消费的当前交付，防止组织者自己创建重复步骤。
- 模型使用 gpt-6-luna、reasoning_effort=max；部分组织者请求明显较慢。请求耗时需和规划重试、重复节点成本分开记录，不能把全部延迟归到工具或上下文容量。

## 证据与运行状态

- 原始任务：`task_1a11053de3918dbe363b9a2aacc`，共 17 次工具调用，其中 get_project_process 6 次、run_project_script 1 次；7 条组织者契约错误。最后为 cancelled。
- 明确命令任务：`task_1a11063f6dc18dbe4595588bc10`，工具调用 3 次：run_project_script 1 次、只读 run_command 2 次。最后为 cancelled。
- [请求耗时、工具调用和错误摘要](agent_pptx_start_trial_evidence_2026-10-06.json)。完整请求仍在两个任务的上下文调试中。
- 当前恢复的 Agent 宿主 PID 为 29712，界面/API 在 3001，辅助 MCP 在 3002，工作区为 PPT 项目。3000 未被本轮成功启动的 PPT 服务占用。

本报告只记录试跑和修复要求，没有修改宿主执行代码。未执行新的测试套件；npm --version 对比属于本次启动故障诊断。
