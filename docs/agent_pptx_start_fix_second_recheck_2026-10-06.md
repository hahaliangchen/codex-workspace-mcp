# PPT 启动修复第二次复核

日期：2026-10-06。

## 结论

上一轮指出的三个进程查询漏洞在当前源码中均已有对应修复；本轮检查范围内未发现新的明确阻断问题。四组既有回归测试共 55 项通过。

本轮没有修改生产代码、重启现有服务或向真实模型发布启动任务。测试包括临时测试项目的 npm 脚本与延迟启动服务，不代表 pptx-editor-engine 已由真实 Agent 自主启动成功。

## 修复核对

### 1. 无参数查询及跨节点复用

- get_project_process({}) 现在始终产生 workspace_list 观察，列表保留子项目。
- max_chars 只影响输出展示，不改变观察身份。
- WorkScheduler.observe 保存真实工具返回的样本，其他订单也能复用，不再依赖组织者填写可选 project_observation 才有查询缓存。
- 组织者显式派发重复观察时仍有契约拒绝；未声明时 Worker 查询也可复用有效样本。

验证：workspace_process_list_reuses_across_orders_but_never_short_circuits_a_wait，以及完整列表与日志限制相关测试通过。

边界：复用消除的是重复宿主查询，不保证模型不再提出重复请求；实际是否减少组织者/Worker往返仍需真实任务观察。

### 2. 等待就绪不再走旧状态缓存

- can_reuse_process_observation 明确排除 wait_seconds>0、ready_url/ready_port、process_id 及 force_refresh。
- Worker 工具分发和调度器复用入口均使用这个判断。
- 列表查询即使只传 wait_seconds，也会对记录实际执行 probe，使用进程已有的就绪端点。

验证：延迟 1.5 秒监听的临时 Node 服务先返回 ready=false，后续等待查询实际等待，并返回 ready=true。相关测试通过。

### 3. 单进程样本不会冒充完整列表

- 新增 workspace_list/project_list/single_process 覆盖类型。
- 启动、安装及指定 process_id 查询产生 single_process；范围包含进程 ID、operation 和脚本参数。
- 无参数及指定项目列表产生对应列表覆盖；只有范围匹配的有效样本才可复用。

验证：不同子项目、同脚本不同参数实例均保留；单进程和安装样本不能满足完整项目列表查询。相关测试通过。

## 测试结果

| 测试组 | 通过 |
| --- | ---: |
| project_process::tests | 7 |
| work_scheduler::tests | 13 |
| flow_tree::tests | 2 |
| agent_service::tests | 33 |
| 合计 | 55 |

命令为各组 cargo test，均使用 --test-threads=1；没有失败或忽略。组织者链路测试使用模拟模型回复，npm/延迟服务测试在当前本机执行。

## 后续验收

加载最新代码后，仍需让用户自己的 Agent 自然接到“启动 pptx-editor-engine 并打开页面”，观察真实模型的订单顺序、重复请求和最终页面身份。源码复核与回归通过不能代替这项验收。
