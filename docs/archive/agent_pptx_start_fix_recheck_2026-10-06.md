# PPT 启动与组织者修复复核

日期：2026-10-06。范围：agent_pptx_start_fix_plan_2026-10-06.md 中的启动路径、组织者契约、进程状态复用。

## 结论

启动路径及首次组织者契约的问题已在源码中修复；本轮四组既有测试共 52 项通过。进程观察复用尚有三个缺口，不能认为上次重复查询问题已完整解决。

本轮只审查代码、运行既有测试并写本报告；没有修改生产代码、重启服务或向真实 Agent 发布新任务。因此不等于 PPT 项目已能自主启动的实跑验收。以下残留问题为源码执行路径分析，未通过真实模型任务重现。

## 已修复并核查

- 外部程序边界对 Node/npm 脚本参数和 cwd 转换普通 DOS/UNC 路径，内部 canonical 路径仍保留。当前本机可发现 fnm Node；npm CLI 实际执行及测试项目脚本回归通过。
- 首次/普通阶段的 request_action 枚举被收窄到 continue；宿主再次校验；追加消息阶段仍可选择 subtask/replace。旧提示词的“新复杂目标也使用 replace”已删除，并增加首次任务示例。
- flow_update 有明确属性及 current_node_id 层级；宿主对放错位置、未知属性进行拒绝。
- 完成节点保护保留；提供不可变节点、select 订单 ID 和 revisit 节点目标。
- 契约错误改为结构化字段反馈，给一次纠正机会；重复错误停止，不再照旧连续三次。
- 新增进程观察的输入版本、时间、宿主身份、进程事件代数及有效性判断，并把观察放入交付与组织者上下文。
- 新增已声明 project_observation 的跨订单重复拒绝，以及 Worker 部分状态查询的缓存复用。

## 残留问题

### 1. [P1] 上次实际反复调用的无参数查询没有被去重覆盖

位置：src/project_process.rs:125、1553；src/agent_service.rs:2713。

上次六次 get_project_process 调用中，五次参数为 `{}`，一次为 `{max_chars:4096}`，均没有 project_path/script/ready_url/ready_port。

当前 has_observation_scope 对这些参数返回 false；查询不会生成 process_observation，WorkScheduler.observe 因而不能保存可复用样本。另一个跨订单检查只在组织者主动填写 orders[].project_observation 时执行，该字段可省略。组织者不填它、Worker继续无参数查询的原路径仍合法。

影响：修复依赖模型主动改用新声明方式，尚不能阻止原来不断改名订单、重复查询的行为。

修复要求：为无参数工作区列表查询提供明确的 workspace 覆盖范围与可复用交付；从真实工具操作记录构成跨订单重复检查，不能只依靠可选声明。保留工作区列表与特定项目列表的不同含义，不把工作区列表直接转换成根项目列表。

需要补充的回归：两次跨订单 get_project_process({})，第二次应复用或受到重复限制；max_chars 改变不应被当作新状态观察。另验证两个子项目仍能完整列出。

### 2. [P2] 显式等待就绪的请求也可能被旧缓存短路

位置：src/agent_service.rs:3438–3453；src/project_process.rs:54、87。

缓存入口只排除 process_id 和 force_refresh，没有排除 wait_seconds>0。观察范围不包含等待意图；样本有效期为 30 秒，进程事件只在启动/退出等时更新，服务从未就绪变为就绪本身不会使旧样本失效。

执行路径：run_project_script 返回正在运行但 ready=false 的样本；随后 Worker 以相同 project_path/script/ready_url 调用 get_project_process(wait_seconds=60)，宿主可直接返回之前的 ready=false，而不执行 probe，也不等待。通过 process_id 查询或明确 force_refresh 可避开，但普通带作用域查询不应悄悄丢掉等待语义。

影响：真实服务可能已经就绪，Worker仍消费未就绪状态，重新调查或错误报告启动阻塞。

修复要求：显式等待必须进入真实探测；区分状态快照复用和就绪检查。等待中或 ready=false 的快照不能满足一个新请求的主动就绪验证。

需要补充的回归：服务延迟就绪，首次返回 pending；后续带 wait_seconds 的查询实际等待并得到 ready=true。

### 3. [P2] 单进程样本可以被当作完整进程列表复用

位置：src/project_process.rs:54、1383、1400；src/agent_service.rs:3450。

run_project_script/install_dependencies 的 process_observation.processes 只有本次操作的一个进程。其 scope 没有覆盖类型、进程 ID、operation 或“完整列表”标记。缓存命中后直接将这个 processes 数组当作列表查询结果返回。

具体路径：同一项目已有运行中的 dev；随后独立节点执行 install_dependencies(project_path='.')。安装交付的 scope 为项目 '.'、script=null，数组只有安装进程。紧接着 get_project_process(project_path='.') 的 scope 相同，若交付仍有效，可能只返回已退出的安装进程，漏掉仍运行的 dev。启动去重本身仍有保护，但组织者收到的列表已经不完整。

修复要求：标明 workspace_list/project_list/single_process 等样本覆盖范围。单进程结果只能回答该进程的问题，不能满足完整列表查询；安装和脚本操作的身份不可混淆。

需要补充的回归：同项目先启动服务再完成一个安装/其他进程操作，项目列表必须仍包含服务；两个同脚本不同参数的实例也不能被单实例样本遮蔽。

## 本轮验证

| 既有测试组 | 通过 |
| --- | ---: |
| project_process::tests | 5 |
| agent_service::tests | 33 |
| work_scheduler::tests | 12 |
| flow_tree::tests | 2 |
| 合计 | 52 |

无失败；编译有既有 dead_code 警告。组织者测试使用模拟模型回答，验证程序契约及上下文发送，不证明真实模型一定作出正确决策。

下一步：补齐上述查询覆盖与缓存语义，再对自然启动任务试跑，记录实际调用和人工干预。不要仅凭既有测试全绿认定原来的六次空转路径已被覆盖。
