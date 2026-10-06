# PPT 项目启动试跑：工具与组织者修复计划

日期：2026-10-06。

目标：让 Agent 接到“启动 pptx-editor-engine 并打开页面”后，用合法计划推动实际启动；启动失败时识别失败阶段，不反复检查相同状态、不错误修复全局环境。

依据：[真实试跑报告](agent_pptx_start_trial_2026-10-06.md)与对应请求上下文。本文是实施计划，尚未修改生产代码、重启服务或执行下述回归。

## 一、已经确认的错误

| 问题 | 本次事实 | 原因与边界 |
| --- | --- | --- |
| npm CLI 启动失败 | 同一 Node 调用普通路径的 npm-cli.js 得到 10.8.2；调用带 `\\?\` 的路径则报 graceful-fs 不存在，实际依赖文件存在 | project_process.rs 的 node_npm 返回 canonical 路径，直接作为 Node 脚本参数；本次环境已确认路径兼容性问题 |
| 初次任务错误选择 replace | 连续三次被宿主拒绝，Worker 没有开始工作 | replace 仅允许在已有目标收到新消息的 session_continuation 交接中使用；工具 schema 始终允许这个选项，首次规划也能输出 |
| Flow 字段层级错误 | 连续三次把 current_node_id 放在 flow_update.plan 中 | flow_tree.rs 读取 flow_update.current_node_id；flow_update schema 只是宽泛 object，没有明确属性结构 |
| 改写完成节点 | 组织者遇到端口冲突后尝试改写已完成节点目标，被拒绝 | 完成节点不可变是正确保护；组织者没有正确使用新节点或 revisit |
| 重复核实进程 | 端口释放后仍连续分派状态确认，原任务 get_project_process 共六次 | 已有无运行实例的结果没有有效推动启动；节点改名、新建订单仍可绕过单个订单的停滞检测 |
| npm 故障误诊 | 组织者把宿主 npm CLI 加载失败解释为缺依赖，并安排全局修复 | 工具返回失败阶段不够明确；日志中的 MODULE_NOT_FOUND 不能单独证明项目依赖或全局安装损坏 |

说明：端口 3000 最初由本轮恢复的 MCP 宿主占用，是试跑环境冲突；启动工具识别外部进程并拒绝误认的行为正确。不能把端口冲突算作 PPT 项目缺陷。

此外，当前 organizer_input 已有 current_output、recent_outputs、work_index。不能直接断言“组织者完全没有收到交付”。实施时需要检查具体请求里交付字段的可消费程度及后续订单绑定，区分信息缺失、信息不清楚和决策重复。

## 二、第一步：修复外部程序调用路径（P1）

涉及：src/project_process.rs 的 node_npm、公共 npm 启动入口；必要时增加小型路径辅助函数。

1. 内部工作区权限检查、路径比较和注册表身份继续使用 canonical 路径。
2. 在外部程序调用边界转换 Node 脚本参数为兼容路径：
   - `\\?\C:\...` 转为 `C:\...`。
   - `\\?\UNC\server\share\...` 转为 `\\server\share\...`。
   - 不把特殊设备路径当普通磁盘路径；非 Windows 原样保留。
3. 覆盖 install_dependencies、run_project_script 共用的 npm 入口。核查 executable、current_dir 等边界参数，按实际兼容需求处理，避免全局替换所有 canonical 路径。
4. 路径无法安全转换或外部运行时不支持长路径时，返回明确环境阻塞，不转为“缺依赖”。
5. 保留项目进程的真实退出码和原始 stderr；添加失败阶段（如 runtime_bootstrap、project_script、readiness）及可核对的启动信息。若阶段不能确定，返回 unknown，而不是猜测。
6. npm 自身入口加载失败时优先诊断宿主运行时。不得仅凭 MODULE_NOT_FOUND 自动安装依赖、改 package.json 或修复全局 npm。

验收：当前 fnm 安装下，工具调用 npm CLI 不再出现已复现的 graceful-fs 假缺失；npm run dev 能进入项目脚本。项目脚本自身失败应如实返回。

## 三、第二步：让组织者只能提交当前阶段合法的契约（P1）

涉及：src/work_organizer.rs、src/agent_service.rs、src/flow_tree.rs、prompts/organizer_system.md。

### 3.1 需求生命周期和订单操作分开

- 向组织者提供程序计算的 allowed_request_actions。
- 没有 session_continuation 时，request_action 只允许 continue（或省略后由宿主使用 continue）；首次任务不是替换旧需求。
- 有新消息交接时，才提供 continue/subtask/replace，并声明 replace 必须 action=work。
- 更换同一个目标内的工作包仍是 continue。不得通过把所有 replace 静默改成 continue 来修复，否则可能吞掉用户真实的取消、替换意图。
- 整理提示词范围：当前 organizer_system.md 包含 “For a new complex goal, also use replace”，脱离前面的 session_continuation 限定容易被理解为首次复杂任务也要 replace。改为仅针对已有目标收到新消息的分支说明，并给首次任务独立示例。
- schema 按阶段收窄枚举，宿主再次校验；支持程度有限的模型/provider仍依靠宿主校验兜底。

### 3.2 明确 Flow JSON 结构

- 为现有 flow_update.plan.nodes、flow_update.node_updates、flow_update.current_node_id、node_result、resume_tree 定义属性、类型、必填字段和示例，不再仅用一段描述。
- 统一示例：current_node_id 与 plan 同级，位于 flow_update 内。
- 规划阶段选中的 ID 必须属于更新后存在的节点；select 使用订单 ID，revisit 使用目标节点 ID，反馈分别列出可用 ID。
- 如需兼容旧调用，只在字段唯一且无冲突时做确定性格式迁移，并记录迁移；不能默认忽略未知字段或猜测节点。
- 后续可让普通 action=work 继续使用宿主自动生成的 Flow 节点，模型只在真实需要分支、父子结构时提交树更新。简单回答仍有一个 Flow 节点。

### 3.3 保留完成节点保护，明确新增与回溯

- 当前节点输出正确、只是下一步操作：创建新节点，声明依赖。
- 已完成节点的输出确实有问题：使用 revisit、明确缺陷和目标；递增修订，旧下游灰色废弃。
- 不允许为了改变下一步而改写已完成节点的 objective/done_when/constraints。
- 在组织者输入列出不可变节点、可选回溯目标；错误直接指明冲突节点和合法操作。

### 3.4 契约失败只做有针对性的修正

当前只注入 last_decision_error.error，并允许三次失败。改为结构化反馈：error_code、field_path、rejected_value、allowed_values、expected_shape、relevant_node_ids。

- 验证及失败不改变已有 scheduler/tree 状态，保留当前 clone 后提交的原子性。
- 提供一次针对错误字段的纠正机会；同一错误指纹再次出现则报告契约阻塞，停止相同重试。
- 不让纠正任务重新调查项目代码或丢掉已完成交付。

验收：自然语言首次启动任务无需人工纠正 request_action 和 Flow 字段即可派发 Worker；格式错误反馈明确且不连续重复三次。

## 四、第三步：让实际交付驱动下一步，阻止跨节点空转（P1）

涉及：src/work_scheduler.rs、src/agent_service.rs、src/project_process.rs、src/work_organizer.rs。

1. 项目启动相关结果以结构化 exported_data 交付：project_path、script、ready_url、process_id，以及状态采样的范围、时间、运行实例和宿主环境版本。未知字段不能伪装成已确认。
2. 下一节点通过 dependency_inputs 绑定上一节点输出。启动节点只消费启动方式和必要当前条件，不重新执行整个配置调查。
3. 临时状态与静态结论分开：脚本配置受文件变更影响；进程状态受启动/停止/退出和宿主重启影响。TTL 和事件失效结合使用，不能无限复用 processes=[]。
4. 给组织者展示当前已满足的启动条件及其有效来源。旧端口冲突被新采样覆盖后，标记历史，不能继续作为当前阻塞。
5. 增加跨订单的重复操作记录，键由操作类型、项目、目标脚本/端点、输入版本构成，不依赖节点标题或订单 ID。
6. 相同输入版本已有有效状态交付时，拒绝再次分派等价“确认没有实例”订单，反馈已有交付引用；有进程事件、输入变化、状态过期或明确新观察目的时允许重新采样。
7. 已知脚本和端点时，直接 run_project_script；它本身已有进程去重与端口占用检查。不要把 get_project_process 当每次启动的固定前置节点。
8. 工具完成客观启动检查后封存该节点，转到打开页面节点。Worker 仍失败则交付具体失败阶段与日志，不用多轮“准备启动”。

这一步不增加复杂生命周期状态，也不硬编码所有任务的下一动作；组织者仍决定顺序，程序验证合法性、结果有效性和等价重复。

Observer 继续异步审查组织者：关注有效交付有没有被消费、重复订单、失败阶段误诊、是否偏离用户目标。它不审批每次工具调用；关键重复约束由程序执行，不能依靠 Observer 催促。

验收：同一份有效 processes=[] 不会在多个改名节点被反复查询；宿主重启后的旧结果失效，新的结果一旦可用就能推动启动。

## 五、启动任务的期望 Flow

```text
用户目标：启动 PPT 编辑器并打开页面
  A 确认现有启动方式（已有有效交付则复用）
    → B 启动并确认项目进程和目标端点
      → C 浏览器打开并确认确实是 PPT 编辑器
        → 返回真实 URL 与运行状态
```

- 安装依赖只在启动真实需要时插入独立节点，不作为所有任务默认步骤。
- 端口冲突：使用项目实际支持的替代端口，或明确报告阻塞；不停止无关进程。
- 只启动页面不自动扩展为编辑功能、示例 PPT 加载、截图审查或业务修复；若用户要求看画面，则另加有图像输入的视觉节点。
- 启动完成依然要求 managed process_running、listener_owned 和目标 HTTP readiness；浏览器确认承担页面身份检查。端口能连通、打开地址或进程创建都不能单独代表完成。

## 六、实施顺序与后续验证要求

顺序：路径兼容与失败阶段 → 组织者契约与纠错 → 交付复用及跨订单去重 → 自然任务试跑。

后续实施验收需要覆盖：

1. 路径转换的 DOS、UNC、普通路径及非 Windows 情况；验证 fnm 安装下实际 npm CLI 加载。
2. 首次/追加消息的 request_action 合法组合；合法的用户替换请求保持有效。
3. Flow 字段层级、非法 ID、完成节点保护、合法 revisit 及灰色下游历史。
4. 等价订单重复检测、有效交付复用；文件变化、进程退出、宿主重启和状态过期能解除去重。
5. 由用户 Agent 自然收到“启动 pptx-editor-engine 并打开页面”，不预先告诉模型如何修契约。记录人工干预，不能把人工直启当 Agent 成功。
6. 调试日志统计 Organizer/Worker/Observer 请求数、契约纠错次数、重复操作、各阶段耗时。不要用固定总调用数评价所有任务；重点是无相同无效重试、无有效条件下的反复核实。

实施及验收前不重启当前服务。本文未执行验证；真实启动中若出现 WASM 构建或项目依赖问题，另行根据项目脚本输出判断，不把 npm 路径修复等同于全部启动成功。
