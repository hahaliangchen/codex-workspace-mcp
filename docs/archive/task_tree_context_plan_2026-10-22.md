# 任务树驱动的上下文组织与压缩改造方案

日期：2026-10-22
状态：方案已确认，阶段 0 开始实施。代码基线为 `3125b1e`。本文描述目标协议，实际完成情况见文末实施记录。

实施优先级：正确性、复杂任务下的可恢复运行优先于缓存和费用优化。所有用户消息完整保存；本次不实现极长用户消息的截断。

## 1. 背景

现在的上下文是"一条线性工作窗口 + 达到阈值后整体压缩"：

- 主 Agent 只有一个 `Window`（`agent_runtime_windows`，scope=`main`）。宿主每轮估算一次，到 96k 就冻结整个窗口，交给只读上下文子代理写一份约 800 字的摘要，再按它选的 `restore_assets` 恢复资产（`src/single_agent.rs` 的 `auto_compact`/`organize_context`/`replace_context`）。
- Flow 图按"每次模型请求一个节点"生成（`request_context::record` 写 `flow/node_add`），记录的是调用过程，不是任务状态。
- 任务板（`main_task_anchor`）只有一层：目标、约束、完成条件，加一份扁平的计划项列表。

这种做法暴露了以下问题（详细分析见本会话的代码梳理）：

1. 压缩没有天然边界，只能在 token 到阈值时把"前面一大段对话"压成摘要。摘要质量靠模型临场发挥，非资产消息（用户原话、插话、命令输出、编辑回执）压缩后只剩摘要。
2. 有两套压缩协议（自动单次 / deep 多轮），默认语义相反，提示词已经与代码不一致。
3. 资产回执只增不减，压缩收益随任务变长而递减。
4. 预算写死 256k，中文估算偏高 2–3 倍；动态宿主状态插在 index 1，前缀缓存难以命中。
5. 进度靠每轮从对话里重新推断，没有显式状态。

2026-10-22 凌晨的 20 页 PPT 真实测试（05:17 截止，见 `task-board-final-summary-2026-10-22.md`）印证了这些问题：累计 13 次上下文整理，目标和大纲都保住了，但正文只写完 4/20 页。模型反复读取同一页、回查旧错误、提前结束。

## 2. 目标与不做的事

**目标**

- 用任务树组织上下文：节点是"有可验证结果的工作单元"，待办清单就是子节点，突发任务是挂在当前节点下的分支。
- 每个活动节点有自己的工作帧（frame），上下文只聚焦当前节点；节点完成后折叠成一条返回结果，回到父节点继续。
- 压缩的主力是"分支返回时折叠"，通用压缩只作为节点内的兜底。
- 状态显式：做完、在做、没做都记录在树上，不再依赖用户的第一句话或从对话里推断。

**不做的事**

- 不恢复 Organizer/Worker 双角色。规划和执行是同一个 Agent、同一份系统提示、同一套工具，只是切换工作帧。
- 宿主不判断相关性。节点需要哪些材料由 AI 在规划或开始节点时声明，宿主只按声明机械加载；其余内容以引用形式提供。
- 不改资产存储格式。内容寻址的 blob、`agent_context_assets` 和远程 SVG 句柄继续沿用。
- 不改根目标的确认机制。

## 3. 已确认的设计决定

| 决定项 | 结论 |
|---|---|
| 节点粒度 | 一个节点 = 一个有可验证结果的工作单元。节点内可以有多轮模型请求和工具调用，不为单个动作建节点 |
| 分支执行 | 串行分支由主 Agent 切换工作帧执行；互相独立的兄弟节点交给委派并行执行。两者共用同一套节点结构 |
| 用户确认范围 | 只有根目标和约束的实质变更需要确认（沿用现有任务板）。拆待办、插分支、调顺序由 AI 决定，在 Flow 图里实时可见，用户可随时插话纠正 |
| 返回状态 | `done` / `failed` / `blocked` / `needs_user` |
| 兜底压缩 | 保留。先拆分、后压缩：60% 提醒 AI 拆分，85% 由宿主强制做节点内压缩 |

## 4. 吸取 10-07 动态任务链试运行的教训

`dynamic_task_function_flow_plan_2026-10-07.md` 曾尝试"组织者派单 → Worker 执行 → 返回"的节点流程，试运行复盘（`agent_dynamic_flow_trial_review_2026-10-07.md`）列出的问题，本方案逐条规避：

| 当时的问题 | 本方案的做法 |
|---|---|
| Worker 纯汇报一轮就被强制交回组织者，任务被截断 | 节点只在 AI 显式调用 `return_task` 时结束。普通文字回复只结束本轮对话，节点保持进行中 |
| 交接投影按键截断，丢失结构化事实，导致重做已完成动作 | 返回内容由 AI 自己写，附带事实列表和证据引用；宿主不截断、不改写返回内容，原始结果始终可按引用读回 |
| 重派任务先废弃正在运行的节点，结果无法作为后续输入 | 栈纪律：父节点在子节点运行期间是"挂起"，不是废弃；子节点返回后父节点原样恢复 |
| 当前浏览器/上传状态跟随节点生命周期丢失 | 当前状态类信息（PPT 状态、委派状态等）是宿主全局状态，每次请求注入，不归属任何节点帧 |
| 组织协议参数过多（`organize_work` 一个工具多种模式） | 工具少而专：每个工具只做一件事，ID、节点状态和帧切换由宿主生成和执行 |
| 两个角色各有上下文，事实在角色之间传递时丢失 | 同一个 Agent，父帧持久保存，子帧结果以返回消息送回父帧 |

## 5. 术语

| 术语 | 含义 |
|---|---|
| 任务树 | 一个会话的全部节点，根节点对应任务板上已确认的主任务 |
| 节点 | 有可验证结果的工作单元，带目标、完成条件、声明的输入材料和状态 |
| 计划子节点 | AI 用 `plan_tasks` 拆出来的待办项，按顺序执行 |
| 分支 | 执行某节点时临时插入的子节点（如"开开关时发现锅坏了 → 修锅"），带插入原因 |
| 工作帧 | 某个活动节点的工作窗口，只包含该节点自己的对话和工具交换 |
| 折叠 | 节点返回时，丢弃它的工作帧（原文留在历史），只把返回结果交给父帧 |
| 简报 | 开始节点或分支时，AI 回顾以前节点后写下的参考信息 |
| 路径 | 从根节点到当前节点的祖先链 |

## 6. 目标数据结构

### 6.1 节点事件（事实来源）

节点状态以事件为准，沿用 `agent_task_events`，便于回退、回放和前端订阅：

| 事件 | 写入时机 | 主要字段 |
|---|---|---|
| `task/node_planned` | `plan_tasks` 新增节点 | `node_id, parent_id, ordinal, title, goal, done_when, inputs, executor` |
| `task/node_revised` | `plan_tasks` 修改、重排、取消未开始的节点 | `node_id, changes` |
| `task/node_started` | `start_task` / `branch_task` | `node_id, briefing, spawned_from?, frame_scope, turn` |
| `task/node_returned` | `return_task` / `split_task` 产生的已完成子节点 | `node_id, status, result, facts, outputs, evidence, event_range` |
| `task/node_reviewed` | 父节点验收委派结果 | `node_id, decision(accepted/needs_work), evidence_refs` |

宿主另维护一张由事件重建的缓存表，供查询和回退后重建：

```sql
CREATE TABLE agent_task_nodes(
  task_id TEXT NOT NULL, node_id TEXT NOT NULL,      -- 宿主生成：n1, n2 ...
  parent_id TEXT,                                     -- NULL 表示根节点
  kind TEXT NOT NULL,                                 -- root | planned | branch
  ordinal INTEGER NOT NULL,
  title TEXT NOT NULL, goal TEXT NOT NULL, done_when TEXT NOT NULL,
  inputs TEXT NOT NULL,                               -- JSON：声明的材料
  executor TEXT NOT NULL,                             -- inline | delegate
  agent_id TEXT,                                      -- executor=delegate 时对应 agent_delegates.id
  status TEXT NOT NULL,      -- pending | running | suspended | done | failed | blocked | needs_user | cancelled
  spawned_from TEXT,                                  -- 分支的插入原因
  result TEXT,                                        -- JSON：返回内容
  review TEXT,                                        -- 委派节点的验收结论
  first_seq INTEGER, last_seq INTEGER,                -- 节点帧产生的事件范围
  PRIMARY KEY(task_id, node_id));
```

- 根节点在任务定义生效时创建，`goal` 和约束直接引用 `task/definition`，不复制成第二份。
- 每条执行线最多有一个 `running` 的内联节点，它就是栈顶；活动节点直接查询这个节点，并校验其祖先均为 `suspended`。委派是独立执行线，不参与主执行线的唯一性判断。回退后从事件重建缓存表并校验此不变式。
- 没有计划的普通对话只有根节点和根帧，行为与现在的单 Agent 一致。

事件归属：`agent_task_events` 增加可空 `node_id`，旧记录为空时按根节点解释。助手消息、工具调用和结果、usage、资产注册等均记录当前任务节点；历史过滤和统计按节点归属，不以起止 seq 判断。`first_seq/last_seq` 仅用于时间线展示。委派事件同时保留执行者身份。

### 6.1.1 用户指令

- 用户消息原文立即进入栈顶帧，同时登记指令 ID、来源节点和原始 event_seq。
- 未定范围的指令在各执行线尾部可见，明确标注作用范围待判定。由主执行线调用 `scope_message` 声明范围，委派不得把待判定通知当成全局执行要求。
- 范围为当前节点、某个祖先节点的整棵子树或根节点。保存用户原话，包括“从第 11 页开始”等适用条件，不擅自扩写。
- 新帧在路径区加载适用指令；已运行帧以追加通知接收，以指令 ID 去重，不改写原有前缀。委派同样接收适用通知。
- 还有未定范围的用户指令时，拒绝 `return_task` 和 `split_task`。
- 指令定范围不需额外确认；改写已确认的根目标、约束仍沿用任务板确认流程。

### 6.2 声明的输入材料

```json
{
  "assets": ["asset-…"],                  // 开始节点时加载正文
  "nodes": ["n3"],                        // 引用这些节点的返回结果和产出
  "history": [1203, 1210],                // 需要直接参考的原始事件
  "pinned": ["asset-…"]                   // 挂在本节点、向所有后代继承的材料（设计规范、模板等）
}
```

技能说明（`read_skill` 的结果）在本轮激活期间自动视为 `pinned`，子帧继承父帧已激活的技能。

### 6.3 工作帧

工作帧取代现在的裸 `Window`，改为类型化结构，序列化成发给模型的消息只在一处完成：

```rust
struct Frame {
    node_id: NodeId,
    messages: Vec<FrameMessage>,              // 只含本节点自己的消息
    loaded: BTreeMap<AssetId, LoadState>,     // 本帧已加载的资产正文
    active_skills: BTreeSet<String>,
    skill_turn: Option<usize>,
    pressure: Pressure,                       // 阈值梯度状态，见第 9 节
    previous_request: Option<RequestId>,
}

enum FrameMessage {
    User { seq: i64, content: Value },                         // 用户消息或插话，不会被压缩掉
    Assistant { seq: i64, content: Value, tool_calls: Vec<ToolCall> },
    ToolResult { seq: i64, call_id: String, body: Body },      // Body::Inline | Body::Asset { id, state }
    ChildReturn { seq: i64, node_id: NodeId, status: ReturnStatus, result: String },
    FrameSummary { seq: i64, summary: Value },                 // 兜底压缩的结果
    Notice { kind: NoticeKind, content: Value },               // 委派完成、异步视觉结果等
}
```

- 帧存放在 `agent_runtime_windows`：根帧 scope 仍为 `main`（兼容旧会话），其他节点为 `node:{node_id}`。
- 帧只含本节点的消息，体积远小于现在的整窗，因此先沿用"整帧重写"的存储方式。如果写放大仍然明显，再改成按消息追加存储。
- `_asset_reference`、`_asset_state`、`source` 这类宿主元数据成为类型字段，不再混在发给模型的 JSON 里，也不用在 `request()` 里逐个剥掉。

### 6.4 返回内容

```json
{
  "status": "done | failed | blocked | needs_user",
  "result": "结论，几句话到一段话",
  "facts": [{"fact": "保险丝规格 10A", "evidence": [1288]}],
  "outputs": ["asset-…"],
  "evidence": [1288, 1302],
  "open_issues": "failed/blocked 时必填：卡在哪里、试过什么",
  "question": "needs_user 时必填：需要用户决定的具体问题"
}
```

宿主校验（机械检查，不判断语义）：

- `done` 至少要有一条证据：本节点的成功工具结果、直接子节点的完成返回，或自己/直接子节点的产出资产。委派子节点的返回和资产均须先验收为 `accepted`。禁止引用无关节点的证据。证据存在性不等于语义验证，完成条件仍由 AI 核对。
- 字段大小设上限，`result` 加 `facts` 约 4k token。超长时拒绝，要求精简，宿主不截断。
- 不能在还有子节点运行时返回。

## 7. 分支与返回协议

### 7.1 模型工具

| 工具 | 作用 | 说明 |
|---|---|---|
| `read_task_board` | 读任务树索引 | 现有工具扩展：返回根定义、路径，以及每个节点的标题、状态、一行结论、产出引用和事件范围 |
| `plan_tasks` | 给当前节点新增、修改、重排、取消待办 | 只能改当前节点的子节点；已开始的节点不能改；宿主生成 ID |
| `start_task` | 开始当前节点的一个计划子节点 | 参数：`node_id`、`briefing`，可补充 `inputs`。宿主挂起当前帧，为子节点装配新帧 |
| `branch_task` | 插入并立即开始一个突发子任务 | 参数：`title, goal, done_when, reason, briefing, inputs`。等于"新增分支 + 开始" |
| `return_task` | 结束当前节点 | 参数见 6.4。宿主折叠当前帧，恢复父帧，并把返回结果追加到父帧 |
| `split_task` | 把当前节点已完成的部分折叠成一个 `done` 子节点，剩余工作拆成计划子节点 | 用于第 9 节的软阈值，详见 9.2 |
| `scope_message` | 为用户原始指令指定作用范围 | 参数为指令 ID 和当前节点或祖先节点 ID；宿主保存原文并向适用执行线传播 |
| `compress_context` | 现有工具保留，作用范围改为当前帧 | 用于拆分无意义的情况，比如单份材料太大 |

`read_asset`、`release_assets`、`refresh_asset`、`read_session_history` 继续保留。`read_session_history` 增加 `node_id` 过滤和是否包含后代节点的选项，按事件节点归属读取原文。

### 7.2 状态机

```
pending ──start/branch──▶ running ──return──▶ done | failed | blocked | needs_user
   │                       │  ▲
   │ plan_tasks 取消        │  └── 子节点返回，父帧恢复
   ▼                       ▼
cancelled              suspended（有子节点在运行）
```

- 同一时刻每条执行线只有一个内联的 `running` 节点（栈顶），它的祖先都是 `suspended`。
- 委派执行的子节点可以多个同时 `running`，它们不影响父节点继续执行其他独立工作。

### 7.3 各返回状态的处理

| 状态 | 父帧收到后 | 例子 |
|---|---|---|
| `done` | 核对结果和证据，开始下一个子节点，或者自己返回 | 修好锅，回到"开开关"继续 |
| `failed` | 父节点不能照原计划继续，需要用 `plan_tasks` 改计划，或者自己返回 `failed`/`blocked` | 锅修不好，改用微波炉 |
| `blocked` | 和 `failed` 一样，由父节点决定绕过还是继续上报；带有 `open_issues` | 缺工具，暂时没法修 |
| `needs_user` | 当前节点结束并折叠，恢复父节点为栈顶，展示 `question` 并结束本轮。用户回复送到父帧，父节点重新规划；必要时新建节点引用旧事实和产出 | 改变上层计划，是否买新锅 |

当前节点范围内的澄清直接用普通文字提问，节点保持 `running`，回复送回同一个节点。根节点没有父节点时，`needs_user` 保持根帧可继续，用户回复后恢复根节点执行。

父帧收到返回后，由同一个 Agent 在父帧里跑一轮来决定下一步。这一轮的上下文很小（父节点自己的消息加上各子节点的返回），而且前缀稳定，成本低。"返回 done 后宿主直接开始下一个兄弟节点"作为后续可选优化，首版不做。

### 7.4 宿主强制的规则

1. 栈纪律：有内联子节点运行时，父帧不会收到模型请求；子节点返回前，父节点不能继续。
2. 深度上限 4（根为 0）。到达上限后拒绝 `branch_task`/`split_task`，只能走兜底压缩。
3. `done` 必须附证据（6.4）。
4. 普通文字回复只结束本轮，节点保持 `running`。用户下次发消息时继续在栈顶帧里工作。如果某个帧连续多轮既没有调用工具也没有返回，宿主在尾部提示"当前节点仍在进行中"，但不替模型结束节点。
5. 用户消息和插话一律送到栈顶帧，并记下节点 ID。是否需要改根目标由 AI 判断，改根目标仍须用户确认。

## 8. 上下文装配

每次请求由一个 `assemble(frame, tree, host_state)` 函数生成，取代现在 `request()` 里多处 `insert(1)` 的做法。顺序按"稳定在前、易变在后"排列：

| 位置 | 内容 | 变化频率 | 消息形式 |
|---|---|---|---|
| 1 | 系统提示（思想提示 + 节点协议） | 基本不变 | system |
| 2 | 项目笔记（AGENTS.md）、项目技能列表 | 很少变 | system |
| 3 | 根节点：已确认的目标、约束、完成条件、用户原话引用 | 根定义变更时 | system |
| 4 | 路径：从根到父节点，每层一行"目标 · 为什么分出来" | 帧内不变 | system |
| 5 | 本节点卡片：标题、目标、完成条件、插入原因、简报 | 帧内不变 | system |
| 6 | 已加载的资产正文：声明的输入和继承的 pinned 材料 | 读取或释放资产时 | user（`saved_asset`） |
| 7 | 帧内消息：对话、工具交换、子节点返回、帧摘要 | 只追加 | 原生角色 |
| 8 | 尾部宿主状态（见下文） | 每轮 | user（`host_state`，标注"不是用户请求"） |

尾部宿主状态包括：

- 当前节点所在层级的待办状态（✓ / ▶ / ○ / ✗），以及已完成兄弟节点的一行结论和产出引用；
- 当前作用域内的资产索引，每条一行、有数量上限，其余通过 `list_context_assets` 查询；
- PPT 状态、委派状态、视觉任务状态；
- 软阈值提醒（第 9 节）。

要点：

- 第 1–5 项在一个帧内逐字节不变，第 7 项只追加，所以帧内连续请求能命中前缀缓存。切换帧时缓存失效，这可以接受，因为切换次数远少于请求次数。
- 易变内容一律放在尾部，并用 user 消息承载。这样即使路由开了 `prefix_system_messages`（把所有 system 合并成第一条），易变内容也不会被合并到开头。
- 资产回执不再常驻窗口，改为尾部的有限索引，彻底解决"回执只增不减"。
- 预算按模型目录里的 `contextWindow` 减去输出预留计算。估算值用上一轮响应的 `usage.prompt_tokens` 校准（`model_stream` 已经记录了 usage），用"实际 / 估算"的比值修正下一轮估算。

## 9. 阈值梯度与兜底压缩

### 9.1 梯度

所有比例都以当前帧的预算为基准：

| 时机 | 谁决定 | 做什么 |
|---|---|---|
| 规划时 | AI | 按可验证结果切小节点（系统提示引导） |
| 软阈值约 60% | 宿主提醒，AI 决定 | 尾部追加一次提醒，建议用 `split_task` 折叠已完成部分、拆分剩余工作；不拆要说明理由。每越过一次阈值只提醒一次 |
| 硬阈值约 85% | 宿主强制 | 对当前帧做兜底压缩，见 9.3 |
| 超出容量 | 宿主 | 沿用现有规则：大结果不进入帧，原文保存在历史，返回 `MATERIAL_OVER_CAPACITY` |

### 9.2 `split_task`（事后拆分）

```json
{
  "title": "…",
  "completed": { "result": "…", "facts": [], "outputs": [], "evidence": [] },
  "tasks": [ { "title": "…", "goal": "…", "done_when": "…", "inputs": {} } ]
}
```

宿主依次执行：

1. 新建一个 `done` 子节点，代表已完成的部分。记录原节点 ID 和自上次拆分点到本次拆分点的来源区间，按“原节点 ID + 区间”定位事件，不改写历史归属；连续拆分的区间不重叠。证据校验和 `return_task` 相同。
2. 折叠 AI 与工具之间的旧工作消息，完整保留所有用户消息（原始顺序）、已完成子节点的 `ChildReturn`、本次拆分形成的 `ChildReturn`，以及本次 `split_task` 的原生调用和结果。声明输入和 pinned 材料不受影响。原文仍可按来源引用读回。
3. 按 `tasks` 新建计划子节点。
4. 当前帧继续运行，此时它相当于父节点，接下来由 AI 调用 `start_task` 开始第一个剩余子节点。

拆分的逻辑归属与历史物理归属分别记录；汇总 token 时每个原始 usage 事件只计一次，不同时计入原节点和合成子节点。

排查类的探索性工作同样适用，比如拆成"验证假设 A""验证假设 B"，每个子节点带回一个结论。

### 9.3 兜底压缩（单一协议）

只保留现在"自动单次"那一套，并且作用范围缩小到当前帧：

- 冻结的材料只有当前帧的消息，加上路径和节点卡片作为背景。不再收集全会话的用户消息，也不再把系统提示当成对话内容。
- 帧内的用户消息和插话原样保留，不进入摘要。
- 最近 K 轮工具交换（默认 2 轮）原样保留，只压更早的部分。
- 上下文子代理只有 `submit_context` 一个工具，一次提交摘要和 `restore_assets`；宿主恢复所选资产和本帧的 pinned 材料。
- 压缩视图保留失败工具结果里的诊断文本，带长度上限。这修复 2026-10-22 凌晨测试总结里记录的缺陷：诊断信息放在 `data` 字符串里，被压缩视图丢掉了。
- 推理模型强制工具调用的兼容问题，改为按模型目录配置（`context_request_options`）处理，不再只认 DeepSeek 官方域名。
- 失败时保留原帧，本轮内退避，下一轮用户输入时重试。这部分与现有逻辑相同。

删除的部分：deep 模式、`read_context_material`、`read_task_requirement`、`checkpoint_context`、`asset_context` 的 keep/reference/excerpt 三态、全量资产回执、`context_agent.md`、`asset_review.md`。

## 10. 委派（并行执行）

- `plan_tasks` 中的节点可以设 `executor: "delegate"`，`start_task` 时由 `collaboration_jobs` 启动委派（最多 5 个并发，沿用现有限制）。
- 委派的初始上下文就是该节点按第 8 节装配出来的帧：路径、节点卡片和声明的输入。它替代现在 `delegate_shared_context` 的整窗复制和 `projected_delegate_window` 的投影。
- 委派结束时也调用 `return_task`，返回格式统一。委派返回仍不等于验收：父节点用现有的 `review_agent_result`（代码委派先 `inspect_agent_changes`/`merge_agent_result`）验收后写 `task/node_reviewed`。
- 委派自己的帧内同样适用阈值梯度和兜底压缩。它压缩时的背景是自己的节点卡片，不再误用主任务的用户消息。

## 11. Flow 图

- 主体改为任务树：节点卡片显示标题、状态和请求数，悬停显示目标和完成条件。计划子节点用实线连接，分支用虚线连接，并标注插入原因。
- 每个任务节点可以展开，查看它内部的模型请求时间线（就是现在的请求节点），包括兜底压缩的请求。`flow/node_add` 增加 `task_node_id` 字段，前端按它把请求归到对应的任务节点下。按请求调试、查看请求上下文的能力保持不变。
- 委派节点在树中的位置不变，并标记执行者。
- 用户插话显示在当时的栈顶节点上。
- 前端从 `task/node_*` 事件重建树。旧会话没有这类事件，继续按现在的请求视图显示。

## 12. PPT 适配

- 大纲确认后（现有的 `confirm_outline` 用户动作），宿主按已确认的大纲项机械地为每一页创建一个计划子节点，挂在"生成整份 PPT"节点下。完成条件是：正文已写入，并且当前版本的视觉检查通过。
- 设计规范、模板、`ppt-master` 技能说明作为 pinned 材料挂在整份 PPT 节点上，每个页面节点自动继承，不用反复读取。
- 页面节点帧里的 `ppt_task_state` 只给出本页状态和整份文稿的摘要，不再每轮注入全量状态。
- 页面相互独立时可以交给委派并行，沿用现在"不同页分给不同委派、同一页不并发修改"的规则。
- 正式导出仍然要求全部计划页验收通过（`require_delivery` 不变）。

## 13. 迁移步骤

分六个阶段，每个阶段都能单独合入和验证。阶段 0 对现有系统也有直接收益，可以先做。分支安排和新旧对比方法见 13.2。

### 阶段 0：清理和修复（不改变行为）

1. 删除死代码：
   - `src/context_rebuild.rs`；
   - `context_window::prepare`、`HistoryView`、`session_history::process_view`；
   - `src/observer_service.rs`（运行时没有构造点），以及 `agent_service` 里的 `observer_json_response*`；同时清理或迁移 `visual_tests` 中依赖它们的测试；
   - `context_assets::apply_review` / `validate_complete_review`；
   - `prompts/asset_review.md`，以及 Observer 相关提示词；
   - `read_session_history` 角色枚举里的 organizer/observer/worker，`request_context` 的 `asset_reviewer` 分支。
2. 修复回退后资产溯源错位：
   - 事件 seq 改为单调递增，不复用（在 `agent_tasks` 记录高水位，`append_event_tx` 从它分配）；
   - 回退时删除 `history_reference.event_seq` 不小于起点的 `agent_context_assets` 行，以及对应的 `agent_context_materials` 行。
3. 预算改为按模型 `contextWindow` 计算，估算值用 `usage.prompt_tokens` 校准。
4. 把易变宿主状态（任务板、PPT 状态、各类状态尾注）移到尾部，改用 user 消息承载。补一个状态变化后前缀仍然稳定的测试。
5. 完整请求体记录增加开关和保留期限。现在 `recording` 恒为 true，并且没有清理。
6. 建立 13.1 的 20 页 HTML 模拟夹具、检查脚本和统计脚本，在现有机制上跑一次基线，记录第 15 节的指标。

### 阶段 1：任务树数据层（影子模式）

1. 实现 `task/node_*` 事件和 `agent_task_nodes` 缓存表，并支持回退后重建。
2. 实现 `plan_tasks`、`start_task`、`branch_task`、`return_task`、`split_task`，以及 `read_task_board` 的树索引。这一阶段**只记录状态**，上下文仍然使用现有的单窗口。
3. 前端 Flow 显示任务树，请求挂在任务节点下。
4. 用真实任务观察 AI 的规划和返回行为是否合理，例如节点粒度、证据是否充分、是否忘记返回，再进入阶段 2。

### 阶段 2：工作帧与上下文装配

1. 实现类型化的 `Frame`/`FrameMessage` 和 `assemble()`，替换 `request()` 的拼装逻辑。
2. `start_task`/`branch_task` 时挂起当前帧、装配新帧；`return_task` 时折叠当前帧、恢复父帧并追加 `ChildReturn`。
3. 实现资产作用域：声明的输入、继承的 pinned 材料、产出引用，以及尾部资产索引。
4. 回退快照从"只存 main 窗口"扩展为"存整条帧栈"。重启恢复（补齐中断的工具结果）改为在对应的帧里进行。
5. 旧会话的 `main` 窗口作为根帧继续使用，不做数据迁移。

### 阶段 3：阈值梯度与兜底压缩

1. 软阈值提醒和 `split_task` 接入。
2. 兜底压缩改为作用于单个帧（9.3），删除全局 `auto_compact`、deep 模式和多轮上下文工具。
3. 重写 `single_agent.md` 中关于上下文和压缩的段落，`context_review.md` 改为单帧版本，删除 `context_agent.md`。

### 阶段 4：委派统一

1. `executor: "delegate"` 接入 `collaboration_jobs`，用节点帧替代整窗复制和投影逻辑。
2. 委派改用 `return_task` 返回，验收流程保持不变。`delegate_agent.md` 同步修改。

### 阶段 5：PPT 适配与真实验收

1. 大纲确认后生成页面节点，接入 pinned 设计材料和按页的状态注入。
2. 用真实 PPT 跑一次 20 页任务，只验证 HTML 模拟覆盖不到的部分（见 13.1 末尾）。结果以实际运行记录为准。

### 13.1 贯穿各阶段的验收场景：20 页 HTML 模拟

上下文机制本身与插件无关。验收不启动 PPT 插件，改为让 Agent 按大纲生成 20 页 HTML，模拟 2026-10-22 凌晨那次 20 页任务的负载。这样可以排除 PPT 服务、登录、远程 SVG、部署等因素的干扰，在本地反复运行，并对新旧两套机制做对比。

**夹具**（建议放在 `tests/fixtures/html_20_pages/`）：

| 文件 | 内容 | 目的 |
|---|---|---|
| `outline.json` | 20 条大纲：`id`、标题、要点、版式类型 | 共享材料，每页都要用到 |
| `design_spec.md` | 配色、字体、版式规则、组件用法，篇幅约 5–8k 字 | 共享材料，体积足以造成反复读取的成本 |
| `shared.css` | 公共样式，**故意留一个缺陷**：第 7 页所用版式的某个 class 缺失 | 制造突发任务 |
| `check_pages` 脚本（Node 或 Python） | 逐页检查：文件存在、能正常解析、标题与大纲一致、要点齐全、只使用 `shared.css` 中定义的 class、体积在合理范围内；插话生效后再检查页脚 | 每页的可验证结果，也是 `done` 的证据来源 |

**任务要求**：按大纲生成 `page01.html` 到 `page20.html`。每页写完后运行检查脚本，检查通过才算这一页完成。有浏览器工具时可以额外截图，但不作为必需步骤。

**埋设的事件**：

1. **突发任务**：第 7 页的检查会因 `shared.css` 的缺陷而失败。期望 Agent 在第 7 页节点下开分支修复 `shared.css`，修好后返回，再继续第 7 页。
2. **中途插话**：第 10 页完成后，用户插话："从第 11 页开始，页脚加上页码和'内部资料'字样"。期望第 11–20 页带页脚，第 1–10 页不改。
3. **上下文压力**：把模型窗口配置为 64k（依赖阶段 0 的"按模型窗口计算预算"），使 20 页以内能够触发 60% 和 85% 两级阈值。

**两个版本**：

- **脚本化假模型**：沿用 `single_agent.rs` 测试里的脚本化模型服务，按固定顺序发出工具调用，并输出足够大的 HTML 正文来制造压力。结果确定，用于在 CI 中检查协议和装配，包括帧装配、返回折叠、`split_task`、两级阈值、前缀稳定，以及在第 12 页中途回退。分支和插话在脚本里强制发生。
- **真模型**：人工验收，按第 15 节记录指标。是否主动开分支、是否在软阈值时拆分，属于观察项，不作为硬性通过条件。

**运行时机**：

| 时机 | 目的 |
|---|---|
| 阶段 0 完成后、阶段 1 之前 | 在现有线性窗口机制上跑**基线**，模型、窗口配置和夹具都与之后相同，预期复现反复重读的问题 |
| 阶段 1 完成后 | 影子模式下观察规划质量：是否生成 20 个页面节点、节点粒度、返回是否带证据。此时上下文仍是旧机制 |
| 阶段 3 完成后 | 完整新机制，与基线对比第 15 节的指标 |
| 阶段 4 完成后 | 变体：分成 4 个委派，每个负责 5 页，验证并行节点和验收流程 |

**判定原则**：

1. **基线必须复现原问题。** 旧机制的基线应当出现 2026-10-22 凌晨 20 页任务中的现象，至少满足其中一条：
   - 共享材料或同一页被反复读取；
   - 每页输入 token 随页序上升；
   - 没能完成全部 20 页。

   如果基线顺利完成且没有出现这些现象，说明模拟太简单，新旧对比没有意义。这时要先调高夹具难度，再进入阶段 1，可以加长 `design_spec.md`、调小窗口配置、增加每页的要点和检查项。每次调整后重跑基线，直到能复现为止。把最终的夹具版本和窗口配置记录下来，之后所有对比都沿用这一份。
2. **新机制还要满足绝对条件。** 只比基线好不算通过。阶段 3 完成后，必须同时满足 15.2 的全部通过条件。

**覆盖不到的部分**（留给阶段 5 的真实 PPT 运行）：远程 SVG 的版本校验、`ppt_task_state` 注入、大纲确认后自动生成页面节点、异步视觉任务。

### 13.2 分支与对比策略

| 阶段 | 在哪里开发 | 何时合入 master |
|---|---|---|
| 阶段 0 | master，或一个尽快合回的短期分支 | 先合入，然后在 master 上跑 HTML 基线 |
| 阶段 1 | 新分支，例如 `feat/task-tree-context` | 影子模式只新增记录、不改变上下文行为，验证后可以先合回 master，缩短新分支的存活时间 |
| 阶段 2–4 | 新分支 | 满足 15.2 的通过条件后合入 |
| 阶段 5 | 合入后的 master | 在 master 上做真实 PPT 运行 |

1. **阶段 0 先合 master**：阶段 0 的改动对旧机制同样有用，基线也必须在它的基础上测量。否则对比时分不清改善来自阶段 0 还是来自任务树。先合入的另一个好处是，新分支只包含任务树相关的改动，差异更干净。
2. **对比靠运行，不靠比代码**：两个构建跑同一个 20 页 HTML 场景，条件保持一致（同一个模型、同样的 64k 窗口配置、同一套夹具、检查脚本和统计脚本），然后比较第 15 节的指标。
3. **独立目录和数据库**：事件库、资产和工作帧都存放在工作区的 `.codex-workspace-mcp/` 下（`codex_state.db`、`assets/`），新分支还会新增表和事件类型。两个构建如果共用一个工作区，数据会互相污染。因此：
   - 新分支放在另一个 clone 或 worktree 里；
   - 两个构建各用一份 HTML 测试工作区副本和各自的服务端口，不共用数据库。
4. **定期同步 master**：master 更新频繁（10-13 到 10-22 有 12 个提交），而 `single_agent.rs` 正是这次重写的核心文件。以下两种情况，都要把 master 合进新分支，并重跑 CI 中的脚本化场景：
   - 每个阶段开始前；
   - master 上出现涉及 `single_agent.rs`、`context_assets.rs`、`request_context.rs` 或前端 Flow 的提交时。

## 14. 测试

### 14.1 现有测试的处理

`src/single_agent.rs`：

| 测试 | 处理 |
|---|---|
| `native_page_capture_and_disconnected_analysis_keep_explicit_identity`、`git_delegate_file_tool_writes_only_its_worktree`、`delegated_tools_preserve_permission_plugin_scope_and_forbid_recursion`、两个 `transport_diagnostics_*`、`plugin_conversation_cannot_escape_via_tools_skills_history_or_followup`、`unavailable_request_log_prevents_network_call`、`restart_marks_pending_request_interrupted_without_erasing_input`、`context_view_delivers_pixels_from_prior_turn_but_excludes_other_tasks`、`image_analysis_is_a_native_tool_exchange_without_context_handoff`、`background_vision_allows_later_generation_and_withholds_unverified_final_answers`、`remote_svg_is_a_service_reference_without_a_local_body_and_checks_content_version` | 保留。阶段 2 后按帧结构调整构造代码 |
| `skill_tools_load_as_native_results_and_do_not_leak_to_next_user_turn`、`restart_recovers_saved_tool_result_without_reexecuting` | 改写：技能在帧栈内继承；中断恢复在对应帧内完成 |
| `delegate_shared_context_forks_loaded_material_and_confirmations`、`large_delegate_parent_is_projected_to_references`、`large_parent_request_selects_projection_and_records_reference_mode` | 改写为"委派帧 = 路径 + 节点卡片 + 声明输入" |
| `task_delegates_run_concurrently_and_main_must_review_before_final_delivery` | 改写为委派节点版本，验收要求不变 |
| `incremental_requests_keep_the_system_and_history_prefix_stable` | 加强：任务板、PPT 状态变化时，帧内前缀仍逐字节不变；覆盖开启 `prefix_system_messages` 的情况 |
| `context_policy_upgrade_resets_legacy_backoff_but_preserves_successful_thresholds`、`automatic_threshold_leaves_headroom_and_caps_long_windows` | 改写为阈值梯度测试（按模型窗口计算，60% 提醒只出现一次，85% 触发兜底） |
| `automatic_compression_runs_before_large_main_request_and_survives_restore`、`oversized_summary_is_rejected_and_main_window_is_unchanged`、`context_subagent_text_reply_returns_tool_error_without_host_message`、`single_pass_compaction_restores_selected_data_and_skill_rules_without_replaying_tools`、`explicit_compress_context_uses_one_summary_and_reference_restoration` | 改写为单帧兜底压缩版本 |
| `compression_query_uses_only_the_latest_frozen_human_requirement`、`repeated_compaction_restores_frozen_requirements_without_a_human_in_the_window`、`defined_generic_task_survives_three_compactions_without_reinjecting_transcript` | 改写为"根定义和路径在多次折叠、压缩后保持；帧内用户消息原样保留" |
| `fixed_main_requirement_and_saved_outline_are_injected_outside_compaction` | 保留思路：根定义和 PPT 大纲在帧外注入 |
| `review_preserves_nested_async_completion_evidence_without_source_bodies` | 改写，并补充"`data` 字符串里的诊断文本被保留"的用例 |
| `multi_round_context_subagent_preserves_plan_and_records_every_request`、`page_coverage_detects_missing_middle_and_unicode_offsets`、`asset_plan_preserves_omitted_pixels_and_user_requirements_and_rejects_partial_invalid_edits`、`thorough_asset_review_covers_all_bodies_and_deduplicates_only_identical_presentations`、`replay_saved_window_asset_review_with_real_model`（`#[ignore]`） | 删除：对应的 deep 模式、分页读取和资产审查功能已移除 |

其他模块：

| 位置 | 处理 |
|---|---|
| `context_rebuild.rs` 的 `observer_rebuild_rejects_future_reference_and_cannot_execute_business_tools`；`context_window.rs` 的 `below_capacity_is_unchanged`、`overflow_never_crops_by_message_role` | 随死代码删除 |
| `context_window.rs` 的 `image_references_and_wire_images_have_same_budget` | 保留，并补充估算校准测试 |
| `rewind.rs` 的 `rewind_restores_the_window_and_drops_later_records` | 扩展：恢复整条帧栈和任务树；seq 不复用；回退区间内的资产行被清理 |
| `interjections.rs` 的 `interjections_require_a_running_task_and_are_delivered_once` | 扩展：插话送到栈顶帧，并记录节点 ID |
| `main_task_anchor.rs` 的三个测试 | 保留，并补充"根定义变更后在树上标注旧计划" |
| `collaboration_jobs.rs` 的 `five_slots_exclude_compression_and_enforce_parent_cancellation_scope` | 改写：压缩改为按帧进行，委派槽位规则不变；该文件其他测试保留 |
| `ppt_task_state.rs` 的测试 | 保留，并补充按页注入的视图测试 |
| `request_context.rs` 的两个测试 | 保留，补充 `task_node_id` 字段 |
| 前端 `agent-flow-layout.test.mjs`、`single-agent-flow.test.mjs`、`delegation-history.test.mjs` | 改写为两层 Flow：任务树，请求挂在任务节点下 |
| 前端 `task-flow-history.test.mjs`、`node-reviews.test.mjs` | 只用于渲染旧的 Organizer/Observer 记录。如果保留旧会话的显示就保留，否则随 Observer 一起删除 |

### 14.2 新增测试

1. **基本流程**：`plan_tasks` → `start_task` → `return_task(done)`。父帧收到 `ChildReturn`，节点状态和事件范围正确，子帧被丢弃，历史完整。
2. **分支**：在运行中的节点上 `branch_task`。父节点变为 `suspended`；分支帧包含"根 → 父 → 分支"的路径、插入原因和简报。分支返回后，父帧逐字节恢复，并追加了返回消息。
3. **四种返回状态**：`needs_user` 结束本轮并展示问题，用户回复后送到父帧；`failed`/`blocked` 由父帧改计划或继续上报。
4. **栈纪律**：子节点运行时父帧不收到请求；有子节点运行时不能返回；超过深度上限时拒绝分支。
5. **证据校验**：`done` 缺证据被拒绝；引用其他节点的事件被拒绝；超长返回被拒绝，宿主不截断。
6. **资产作用域**：新帧只加载声明的输入和 pinned 材料；产出在父帧以引用出现；`read_asset` 只影响当前帧；尾部资产索引有上限。
7. **`split_task`**：已完成部分变成 `done` 子节点，事件范围正确；当前帧被清空；剩余工作变成计划子节点。
8. **阈值梯度**：60% 只提醒一次；85% 触发兜底压缩，帧内用户消息和最近 K 轮原样保留；压缩失败时退避，原帧不变。
9. **前缀缓存**：同一帧内连续请求，前缀直到最后一条帧消息都逐字节一致，即使待办状态或 PPT 状态发生变化。
10. **回退与重启**：回退跨越多个帧时，帧栈、任务树和资产都恢复一致；重启时在分支中途补齐中断的工具结果，帧栈也能正确恢复。
11. **委派节点**：初始帧由节点装配；委派用 `return_task` 返回；验收前不能作为最终交付。
12. **无计划对话**：只有根帧，行为与现在的单 Agent 一致（回归测试）。
13. **PPT**：确认大纲后生成页面节点；页面帧包含继承的设计规范和本页状态。
14. **用户指令传播**：第 10 页后的页脚指令传到第 11–20 页，前 10 页不变；有未定范围指令时返回和拆分被拒；已有帧追加通知且去重，原前缀保持不变；用户原话完整保留。
15. **恢复与归属**：多层分支下唯一活动节点及祖先状态正确；普通提问和 `needs_user` 返回后的回复分别到达正确帧；并发委派事件按节点归属，委派未验收资产不能作为父节点完成证据。
16. **连续拆分**：用户消息、已有返回、新返回和本次工具调用配对齐全；两次拆分来源区间不重叠，不重复统计 usage。

## 15. 风险与验收指标

| 风险 | 应对 |
|---|---|
| 模型不遵守节点协议：忘记返回、拆得过细、分支过多 | 系统提示引导，宿主强制规则（7.4），尾部提醒；阶段 1 先用影子模式观察 |
| 每个节点多一轮父帧请求，增加耗时和成本 | 父帧小、前缀稳定；实测后再决定是否做"直接开始下一个兄弟节点"的优化 |
| 折叠后细节丢失 | `done` 必须带证据和事实；按事件 `node_id` 及拆分来源读回原文 |
| 切换帧时缓存失效 | 切换次数远少于请求次数；用指标验证 |
| 根目标变更后旧计划失效 | 在树上标注变更版本，由 AI 用 `plan_tasks` 调整 |

### 15.1 指标

从阶段 0 的基线开始记录。所有指标都从事件库计算，建议写一个只读统计脚本，输入是任务 ID：

| 指标 | 数据来源 |
|---|---|
| 每页（每个完成节点）消耗的输入 token，以及它是否随页序增长 | `context/usage`（估算）、`context/model_usage` 和请求 outcome.usage（实际）；阶段 1 起按事件 `node_id` 归属到节点 |
| 前缀缓存命中率 | provider 响应中的 usage 缓存字段（如有） |
| 软阈值提醒次数、`split_task` 次数、兜底压缩次数及失败次数 | 宿主提醒事件、`task/node_returned`、`context/compression_*` |
| 共享材料（大纲、设计规范）被重复读取的次数 | 对相应资产或文件的 `read_asset` / `read_file` 工具调用。这是 2026-10-22 凌晨 20 页测试的主要失败模式 |
| 同一页被重复读取、重复检查的次数 | 对 `pageNN.html` 的读取和检查调用 |
| 不带证据的返回被拒次数；每个节点的平均请求数 | `return_task` 的工具结果；请求所属的任务节点 |
| 完成页数、检查通过页数 | 检查脚本的最终输出 |

### 15.2 20 页 HTML 模拟的通过条件

阶段 3 完成后，真模型版本应满足以下条件：

1. 20 页全部生成，最终检查全部通过，本轮没有因超出容量而失败。
2. 第 1–10 页没有页脚，第 11–20 页带页码和"内部资料"页脚。
3. `shared.css` 的缺陷被修复，修复记录挂在第 7 页节点下（作为分支或节点内的工作），并带有检查通过的证据。
4. 每页输入 token 不随页序持续增长：最后 5 页的平均值不超过前 5 页平均值的 1.5 倍。基线预计会明显超过这个比例。
5. 共享材料的重复读取次数明显低于基线。挂为 pinned 材料后，后续页面不需要再读取。
6. 每个页面节点的 `done` 都带有检查通过的证据。

脚本化假模型版本应在 CI 中稳定通过 14.2 的相关测试，并额外覆盖：第 7 页分支的帧装配和恢复、插话被送到栈顶帧、第 12 页中途回退后帧栈和任务树一致。

阶段 5 的真实 PPT 运行只要求 HTML 模拟覆盖不到的部分正常工作，结果以实际运行记录为准。

## 16. 实施记录

2026-10-22：任务树、独立工作帧、单帧压缩、委派节点、PPT 页面适配和前端 Flow 已实现；真实模型新机制验收和真实 PPT 验收尚未完成。以下记录描述实际交付，前面的分阶段迁移表保留为原设计。

### 16.1 实现内容

- `src/task_tree.rs`：事件驱动的节点投影、根节点 `n0`、计划/启动/分支/返回/拆分/指令定域工具。宿主强制唯一运行内联节点、祖先挂起、深度上限 4、完成证据和委派验收。完成节点保存稳定的返回事件引用，根定义或 PPT 计划变化后拒绝旧节点继续完成，重新规划才能采用新版本。
- `src/single_agent/task_frames.rs`：子帧按路径、节点卡片、声明输入和 pinned 材料装配；父帧挂起后按原消息恢复并追加 `ChildReturn`。节点变化、工具调用/结果及帧保存放在同一事务中，失败操作回滚。已有单窗口会话继续作为根帧。
- 用户原话完整保存，包括极长消息。未定范围的指令在各运行帧可见；`scope_message` 指定当前节点或祖先子树后，后续帧继承，已有帧在下一轮请求前追加通知并去重。存在未定范围指令时拒绝返回和拆分。普通澄清留在当前节点；`return_task(needs_user)` 折叠后把用户回复交给父帧。
- 事件持久化 `node_id`，工具、助手消息、usage 和资产按节点归属。历史支持节点/后代筛选；拆分保留原归属，另记不重叠来源区间。统计脚本按节点及拆分来源汇总，每条实际 usage 只计一次。
- 60% 软提醒、85% 单帧兜底压缩。压缩和拆分保留用户原话、声明输入、pinned 材料和子节点返回；压缩额外保留最近两轮原生工具交换，拆分保留本次调用与结果。没有可压缩的旧工作消息时跳过自动摘要，实际容量检查仍生效。
- 委派按节点装配帧，用 `return_task` 返回。父节点完成前必须验收委派结果；未验收报告正文不注入父帧。中断委派保留可恢复记录并标记阻塞。
- `rewind.rs` 快照覆盖整条帧栈，回退后重建节点和指令。事件 seq 使用持久化高水位且不复用，同时清理回退区间资产和压缩材料。
- 确认 PPT 大纲后幂等生成版本化页面节点；换计划取消旧的未启动页面节点。页面帧注入本页要求、共享设计及本页视觉状态，其余计划内容用引用替代；旧计划的写入和完成被拒绝。
- 前端默认显示跨轮持久化任务树，保留请求历史视图、节点内请求筛选和比较，区分挂起、待用户决定及取消状态。
- 预算接入模型目录 `contextWindow` 并用实际 prompt usage 校准；易变宿主状态放在尾部 user 消息。请求正文记录独立开关，默认保留 7 天（可配 1–365 天），清理后保留索引和审计身份。
- 修复验收暴露的 Windows 兼容问题：CRLF/BOM 技能前言解析和 Git worktree 参数中的 Windows 扩展路径。

### 16.2 验证结果

| 验证 | 结果及记录 |
|---|---|
| 最新 Rust 定向测试 | 72 通过、0 失败、1 忽略；`target/context-acceptance/task-tree-focused-tests.log` |
| 最近一次 Rust 全量测试 | 215 通过、30 失败、6 忽略；30 个失败项均已出现在阶段 0 的失败清单中，涉及 Windows 目录占用、浏览器/Python 环境及旧 Observer 测试等。全量并未通过；日志为 `task-tree-full-tests.log` 和 `stage0-full-tests.log` |
| 脚本化任务树 20 页验收 | `target/context-acceptance/scripted-tree-ac462f00bf118c6b1adcae49b3068c03`：20/20 页通过，22 个节点完成且带证据，第 7 页修复分支归属正确，第 11–20 页页脚正确；共享材料重复读取 0 次；34 次压缩、0 次失败；后五页/前五页估算输入比 1.006。假模型不返回实际 usage，不能用于声明真实 token 收益 |
| 协议专项 | 覆盖指令传播与未定域拦截、分支恢复、`needs_user` 路由、连续拆分保留和来源去重、并发委派归属及验收、回退与重启、计划版本失效、容量保护及前缀稳定 |
| 前端 | TypeScript 检查和 Vite 生产构建通过；任务树浏览器测试单独运行通过，并检查 1440px/390px 截图。隔离预览通过真实后端加载 22 个节点，无页面脚本错误。非浏览器测试 34 项通过；另有既存附件测试默认依赖导入路径缺失 |
| Python | 夹具检查器 6 项、统计脚本 2 项测试通过 |

已获用户授权并在改造运行时之前完成一次真实模型旧机制基线，记录位于 `target/context-acceptance/real-linear-67479c111ba6823c7a5ee9cd4a910747`：20/20 页通过，页脚正确，1 次压缩、0 次失败；56 个有 usage 的请求，实际输入 1,489,160 token，其中缓存 374,272 token；后五页/前五页估算输入比 1.243。该基线没有充分复现原来的异常增长或反复读取，不能据此断言任务树节省成本或已经满足第 15 节真实验收条件。新旧运行必须使用独立工作区和事件库。

### 16.3 实施差异和待验收项

- 工作帧沿用现有可序列化 `Window` 和原生 JSON 消息，并增加帧元数据，没有另建类型化 `FrameMessage` 枚举。
- 本次直接接入运行时，没有保留影子模式开关，也没有进行分阶段合并或创建提交。
- 运行时压缩只提供单次摘要协议；旧 deep/Observer 辅助代码和部分旧测试仍留在源码中，尚未按阶段 0/3 的清理表全部删除。
- 20 页脚本验收覆盖分支和插话；拆分、回退及并发委派另有协议专项测试，尚未把所有变体都合成一次 20 页运行。
- 真实模型任务树 20 页场景尚未执行，真实 PPT 的远程 SVG 版本、异步视觉和完整交付链路仍待端到端实测。现有通过结果不替代这些验收，也不表示任何复杂环境都已经得到验证。

本地复跑：

```powershell
cargo test --offline -- single_agent::tests single_agent::task_frames::tests main_task_anchor::tests ppt_task_state::tests context_acceptance::scripted rewind::tests context_window::tests model_settings::tests request_context::tests session_history::store::tests
python -B tests/fixtures/html_20_pages/test_check_pages.py
python -B scripts/test_context_acceptance_stats.py
```

新机制真实模型入口（使用隔离的 `target/context-acceptance/real-tree-*` 工作区，消费配置服务的模型 token；本次未运行）：

```powershell
cargo test --offline context_acceptance::real_model_twenty_page_task_tree_acceptance -- --ignored --nocapture
```

`--offline` 只控制 Cargo 的依赖下载，不禁止测试程序调用模型服务。每次验收保存 `run-manifest.json`、`metrics.json`、`final-check.json` 和独立事件库；结果以生成的文件和实际运行记录为准。

### 16.4 工具分类与按需加载

2026-10-22：内置工具改为按当前任务帧加载能力组。分类直接控制发送给模型的工具定义，不只是展示标签。上下文容量仍由所选模型的 `contextWindow` 决定；本次没有把生产容量改成脚本验收使用的 64k。

核心常驻工具负责节点控制、任务板、历史与资产恢复、技能发现以及工具组控制。其余内置工具分为 12 组：

| 组 ID | 能力 |
|---|---|
| `files` | 目录、文件读取/查找/写入/编辑和文件资产刷新 |
| `code_index` | 工作区代码地图查询 |
| `code_go` / `code_rust` / `code_ts` / `code_python` | 各语言索引状态、符号查询和读取，分别加载 |
| `execution` | 程序和项目脚本、依赖、进程以及 HTTP 探测 |
| `browser` | 网页打开、读取、交互、诊断和截图 |
| `memory` | 工作记忆、架构和符号业务信息、项目规则提议 |
| `attachments` | 上传附件读取 |
| `vision` | 已保存图像、视觉分析和结果收集 |
| `collaboration` | 委派、等待、检查、合并、验收和取消 |

- `list_tool_groups` 只返回当前权限/插件范围内的组说明、工具名称和加载状态，不返回各工具的完整 schema。`load_tool_group` / `unload_tool_group` 每次单独调用；变更在下一轮工具列表生效。同轮加载并使用不会执行被隐藏的工具。
- `Window.loaded_tool_groups` 持久化在每个帧里。计划/启动/分支/委派可用 `inputs.tool_groups` 声明初始组；省略时只有核心工具。子节点独立选择，返回恢复父帧原有组。拆分、压缩、回退和重启保留对应帧的组状态。
- 加载不增加权限、不跨插件范围、不允许递归委派。没有加载的调用返回 `TOOL_GROUP_NOT_LOADED`。加载前计入新 schema 检查容量，超限时保留原状态；卸载允许在上下文仍偏大时进行。原生工具调用/结果和组状态在同一事务保存，委派事件按委派节点归属。
- 技能沿用 `read_skill` 的现有按需加载协议。用户原话完整保存，不因工具分组截断。

本地使用现有估算器测得：原来 73 个全量工具定义约 30,785 token；增加三个组控制工具后，全量 76 个约 31,593 token；新帧核心 20 个约 5,300 token；加载 `files` + `execution` 后 34 个约 12,122 token。这是当前目录及权限下的 schema 估算，不是模型账单或成本节省实测。

验证记录：

- Rust 定向组合测试 75 通过、0 失败、1 忽略，日志 `target/context-acceptance/tool-groups-final-tests.log`。覆盖加载/卸载实际请求、未加载调用拦截、同轮调用限制、权限/插件/容量、子帧隔离与父帧恢复、拆分/压缩/回退/重启，以及两个并发委派独立动态加载和事件归属。组合运行曾触发委派测试原有 20 秒总时限；测试增加进度诊断并把总时限改为 60 秒，最新组合运行约 19 秒完成。
- 20 页脚本验收记录 `target/context-acceptance/scripted-tree-5af716cb1401ef287140b37c21aef214`：20/20 页通过，第 7 页修复分支及第 11–20 页页脚正确；共享材料重复读取 0 次；1 次压缩、0 次失败；后五页/前五页估算输入比约 1.011。第 16.2 节的 34 次压缩是加入工具分组前的历史结果。
- 本次没有新增真实模型调用；真实模型是否正确选择工具组以及实际 token/费用变化仍待实测。全量测试的既有失败记录见第 16.2 节，不能把本次定向通过视为全量通过。

复跑工具分组相关组合测试：

```powershell
cargo test --offline -- single_agent::tests single_agent::task_frames::tests single_agent::tool_groups::tests main_task_anchor::tests ppt_task_state::tests context_acceptance::scripted rewind::tests context_window::tests model_settings::tests request_context::tests session_history::store::tests
```
