# Task tree and tool loading: real PPT verification

## Environment

- Backend checkout: `origin/codex/ppt-remote-svg`, `71710b7`.
- Frontend checkout: `origin/codex/ppt-host-agent`, `966c4eb`.
- Editor checkout: `origin/main`, `dbac4ed`.
- All three remotes were fetched before creating detached worktrees under `target/ppt-real-e2e-20261022`.
- The original backend's 886 staged changes were preserved. Original checkouts were not switched or reset.
- Local backend built successfully and started with an isolated SQLite database. Windows fontconfig CLI compatibility remains unresolved; this backend is not the service used for the real account test.
- Real test uses local frontend/editor at `http://127.0.0.1:16011`, local Agent at `http://127.0.0.1:13001`, and the existing PPT test backend at `http://192.168.100.114:18897/prod-api`.
- Account: `ppt_model_check`, supplied by the user. Credentials are not included in this report.
- New project: `826`. Agent task: `task_78f4e6485f7285ff5a0e0211179e1bcc`.
- Model: existing configured `gpt-6-luna`; global configuration unchanged.

## Observations And Repair

The real model successfully loaded the PPT skill, scoped original instructions, proposed the main task, and saved an eight-page outline. Task definition 151 and outline 417 were approved through the actual UI.

The run exposed contradictory frame state: `task_node.status` retained the pre-start `pending` snapshot while live `task_status.status` was `running`. The model repeatedly tried to start its own node and created redundant nested assignments before writing content.

`task_frames::notices` now refreshes the persisted node snapshot from the canonical task tree before each model request. This also repairs windows saved by previous builds. The system prompt clarifies that the current node is already executing and children should represent distinct units. The task-definition tool description now explains that an initial version must be omitted rather than set to zero.

Validation: all 12 task-frame tests passed, including a regression that saves a stale pending snapshot, restores it, and verifies the actual model request carries the running node and current briefing. A separate test Agent binary was built and restarted against the same workspace. Subsequent real request 86 carried the correct running state and the model proceeded to domain tools.

## Execution Conditions And Limits

The default generation configuration repeatedly read and copied pages and did not finish the deck autonomously. The isolated test profile was subsequently changed to `reasoning_effort: high` for the same configured model and provider; actual captured requests confirm that setting. The run remained in the same project and task. Explicit follow-up instructions and continuation after step limits were needed. This is an assisted integration test, not evidence of reliable unattended completion or a controlled cost comparison.

The run also reached the task depth guard after redundant nested planning. No database or task state was manually reset. Successful compactions included 198,091 to 78,502 estimated tokens and 199,521 to 103,558 estimated tokens. These observations establish that the compression path ran; they do not establish savings relative to the baseline.

Eight pages were generated through the actual Agent skill tools and saved by the PPT backend. Review found four corrections: make the repair branch return to the original paused switch task, fix the page 6 wording, move page 7 numbers clear of bars, and state the priority of running and recovering in complex environments on page 8. The model applied corrections through `apply_html`; revision 23 passed `check_layout` for all eight pages. Subsequent rendering and corrections reached saved revision 27 before the user requested a stop. Final acceptance and export were not completed.

## Acceptance Status

Paused at the user's request on 2026-10-23 (Asia/Shanghai). Agent status is `cancelled`, with the final `turn/end` at event 3808. Eight pages are saved at revision 27 (save result event 3519). No export tool was called and no exported PPTX was verified. The last turn only read page 3; the final two arrow corrections were not applied. Services remain available for inspection; generation has not resumed.

## 测试过程记录

1. 拉取三个项目的远程分支，在隔离检出中启动前端、编辑器和 Agent，保留原项目及后端暂存改动。
2. 使用用户提供的账号登录，创建项目 826，通过真实界面确认任务定义 151 和大纲 417，计划制作 8 页关于近期改动和设计理念的 PPT。
3. 默认配置下出现反复规划、启动自身和读取页面。定位到帧中旧 `pending` 快照与实时 `running` 状态冲突，修正快照更新和相关提示，在隔离 Agent 上验证；重复现象并未因此完全消失。
4. 隔离测试配置改为同一模型的 `reasoning_effort: high`，实际请求已确认携带该参数。测试中有人工补充指令、多次停止和继续，第 6 轮达到 100 步限制。这是有人工干预的集成测试，不能视为一次自主完成。
5. 生成并保存 8 页，经历布局检查、渲染和视觉复核，最新保存版本为 27。第 9 轮按用户要求停止，状态为 `cancelled`。
6. 停止后仅收集事件、阅读代码并刷新页面验证缩略图恢复，没有继续模型请求、修改 PPT 或导出。

## 核心发现：页面任务没有真正推进

本次运行不能用来证明“逐页面任务树仍然天然导致这些重复”，因为实际执行没有走逐页面任务树：

- 大纲确认后，事件 460–467 在根节点 `n0` 下创建了 8 个页面任务，但这些页面节点均未启动。
- 模型仍处于旧的准备方案分支，随后重复建立“制作全部 8 页”的泛任务，最终达到深度 4。
- 全部 163 次 `get_design`、`apply_html`、`render_slide` 调用，都属于同一节点 `n5a541e2c35b81628037f41d6a420d95d`。
- 仅有一次成功的 `return_task(done)`，对应前期方案提交。5 次 `split_task` 全部被 `TASK_DEPTH_LIMIT` 拒绝，没有通过折叠已完成工作来推进页面任务。

代码与事件共同指向调度缺口：`src/task_tree.rs` 的 `confirmed_ppt_pages` 固定在根下建页面节点；`src/single_agent/task_frames.rs` 的 `start_task` 只允许启动当前节点的 pending 直接子节点；`frame_status` 仅直接展示当前节点的子节点和兄弟节点。因此，确认大纲时仍停在旧分支，会使根下的页面任务不能从当前帧直接启动，也不在该帧的直接任务列表中。完整任务板仍可通过工具读取，不能说页面任务彻底不可见。

此外，`validate_plan_version` 校验计划版本，但没有强制页面写入发生在对应页面节点。宿主允许在泛任务帧里跨页制作，因此没有实现“当前页完成、返回父任务、进入下一页”的调度闭环。状态快照修复只解决了其中一处矛盾。

任务边界能帮助隔离上下文和推进状态，但节点内部仍可能重复读写；还需要宿主落实任务调度、完成条件及页面操作归属约束。本次先记录问题，未继续修改这些实现。

## 重复发生在哪里

停止时共记录 323 次工具调用，主要调用如下：

| 工具 | 调用次数 |
| --- | ---: |
| `get_design` | 91 |
| `apply_html` | 37 |
| `render_slide` | 35 |
| `plan_tasks` | 9 |
| `start_task` | 10 |
| `split_task` | 5 |
| `check_layout` | 5 |
| `analyze_images` | 15 |

### 重复规划与启动

- 事件 210/230 创建并启动准备方案任务；270/290 又在其下创建并启动近似目标；320 尝试启动当前节点自身，被拒绝。
- 页面节点已在 460–467 创建后，505/516、536/547、723/733 继续创建并启动“制作全部 8 页”“制作与检查 8 页”等重叠目标，而没有进入页面节点。
- 567、658、772、1286 等调用再次尝试启动自身，被拒绝；781、919 的进一步规划触发深度限制。失败的启动或规划没有产生实际任务推进。

### 相同页面快照重读

89 次成功且带页码的读取只对应 48 个不同的 `(page, data.revision, sourceHash)` 快照，即有 41 次同快照重读。其中包含失败恢复所需的读取，不能全部认定为无效操作。

- 第 3 页，设计版本 13、保存版本 14：事件 1926、1937、1965、1988、1997、2008、2038、2048，共 8 次读取同一快照。
- 第 3 页，设计版本 5、保存版本 6：事件 1314、1324、1333、1342、1371、1380、1389，共 7 次。
- 第 7 页：事件 2752、2776、2786 读取相同版本和内容，期间没有对应保存。

### 明确的重复业务写入

- 事件 879：成功将 1 页复制成 8 页。
- 事件 974：再次执行同一批 7 个复制命令，使页面数变成 15。
- 事件 1014：删除第 9–15 页，恢复为 8 页。

这是一处已确认的重复业务操作，不能仅解释为界面文字重复。

### 合理重试与重复验收的区别

37 次 `apply_html` 中，14 次失败、23 次成功。失败涉及字形左溢出 1px、不支持的 CSS、只读 metadata 修改、版本误用、遗漏隐藏节点等，部分重读和重写属于恢复过程。

- 事件 2972：把保存版本 23 当作设计版本 22 使用，被拒绝，重读后恢复。
- 事件 3284：修改只读 `data-name` 为 `branchArrow`，被拒绝。
- 事件 3503：遗漏隐藏节点，被拒绝。

`get_design.data.revision` / HTML 的 `data-design-revision` 与 `document.revision` 不是同一个版本字段，本次实际相差 1，工具协议对两者区分不够清楚，增加了误用机会。

35 次渲染分布为：保存版本 13 共 8 页，16 共 2 页，18 共 8 页，24 共 8 页，27 共 9 页（第 7 页多一次）。当前验收身份依赖整稿保存版本，任何一页保存都会使旧整稿版本的视觉证据过期，因此这些重渲染不能全部算作无意义重复，但会放大跨页反复修改的成本。

## 左侧为何没有渲染页

观察事实：停止时左侧显示骨架，中央 PPT 内容正常；停止后刷新一次，8 页缩略图恢复。证据为 `target/ppt-real-e2e-20261022/evidence/paused-test.jpg` 和 `paused-after-reload.jpg`。

前端存在与现象吻合的状态恢复缺陷：

1. `ai-ppt-view/src/store/PptWorkflowStore.ts:499` 附近，轮询 `tick` 的任意异常会执行 `this.slides = []`，同时清空其他展示状态。
2. 同文件 `project` 在识别到 PPTX deck 后直接返回，缩略图由编辑器提供；轮询下一次成功并不会重建 `slides`。
3. `ai-ppt-view/src/view/editPpt/DeckEditor.tsx:172` 的 `drawThumbs` 负责把缩略图发布到 store，通常由加载和编辑事件触发。单纯轮询恢复不会让已挂载的编辑器再次发布已有缩略图。
4. `ai-ppt-view/src/view/editPpt/index.tsx:242` 把缺失的 slide 当作“生成中”，于是列表可长期停在骨架状态。刷新重新加载编辑器后，缩略图重新发布，列表恢复。

这是高可信的原因路径，但没有捕获到本次最初触发清空的失败请求，具体请求仍待日志确认。最近浏览器警告只有字体 fallback，没有捕获到缩略图 worker 错误；现有证据不支持“PPT 页面未保存或未渲染”的结论。未修改前端源码。

## 视觉验收进度另有协议不匹配

“当前版本验收通过 0 页”不等于没有生成页面。模型通常一次提交两页给 `analyze_images`，返回按页的结构化 reviews；但 `src/ppt_task_state.rs:61` 附近的 `verdict` 要求 `source.len() == 1` 且 `reviews.len() == 1`，因此多页分析在进度聚合中被归为 `unknown`。

视觉评价本身也有不稳定之处：版本 27 的第 3 页被判 pass，但人工仍看到多余的向上箭头及不一致的返回箭头；第 2 页本次因缺页码被判 needs_fix，之前同内容曾被判 pass。不能据此宣布最终视觉验收完成。视觉作业最初记录的 freshness unknown 会在读取时重验，不能仅凭初始值判断失败原因。

## 证据与剩余限制

- `target/ppt-real-e2e-20261022/evidence/run-evidence.json`：工具计数、结束原因、保存和渲染版本、失败诊断、视觉结果。
- `target/ppt-real-e2e-20261022/evidence/repetition-analysis.json`：相同快照重读、逐轮调用和任务树操作明细。
- 两份证据均已在停止后采集，不包含账号密码。`contact.png` 仍为旧版本 18，不能作为版本 27 的最终验收图。
- 12 个 task-frame 测试已通过，但脚本化通过不能替代本次真实模型的端到端验收；原定 20 页真实任务树验收尚未执行。
- 全本地 Windows 后端的 fontconfig CLI 兼容性仍未解决，尚不能声称任意复杂环境可运行。
- 本次没有受控费用对照，不能宣称已经验证省钱效果。暂停后的下一步修复建议是先贯通页面任务调度，再处理缩略图恢复和批量视觉结果聚合；本报告不代表这些修复已经完成。

## 提交前检查（2026-10-23）

- `cargo test --offline`：226 通过、24 失败、6 跳过；任务帧、工具分组和脚本化 20 页任务树测试通过。全量失败涉及 Windows 文件占用、附件解析 Unicode 编码、缺少 PDF 解析所需程序、动态命令输出编码，以及 PPT 登录测试中的 `bindChat` mock 协议问题；尚未逐项修复，不能称为全量通过。
- 前端 TypeScript 类型检查通过；首次从仓库根运行前端测试为 32 通过、3 失败、1 跳过。其中两项 Vite 路径失败在切换到 `frontend` 目录后重跑通过，附件测试仍因默认路径缺少 Playwright 模块未运行成功。任务树 UI 浏览器测试未启用。
- 上述前端重跑包含任务流测试，共 5 项通过；Python 上下文统计 2 项、HTML 夹具检查 6 项通过。
- `git diff --check` 通过。测试没有恢复真实模型生成，也没有导出本次 PPT。
