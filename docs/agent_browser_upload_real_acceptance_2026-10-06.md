# 真实 Agent 上传 PPTX 验收

日期：2026-10-06。任务：`task_1a111a34c7e18dbf761fdf28a00`。工作区：`D:/enterpriseProject/pptx-editor-engine`。

## 结论

真实文件选择、change 事件、本次上传关联的文稿加载已成功，页面显示 `Slide 1 / 8`、8 张幻灯片。此前 `DOM.querySelector` 的失效节点错误没有复现。

**完整端到端验收未通过。** 首轮耗尽 30 步，续做因 Organizer 连续两次不合法 Flow 调度而失败。两轮均未调用 `browser_diagnostics`、`browser_screenshot` 或 `view_image`，所以不能宣称实际字体与渲染画面验收通过；跨上传/验证节点的回执消费也尚未实际执行到。

本轮编译并重启最新 Agent 宿主，随后由自定义 Agent 实际执行项目启动和浏览器上传。未修改生产代码或 PPT 项目业务源码。未再次运行单元测试。`npm run dev` 自带的 WASM 构建由 Agent 启动脚本执行。

## 实际过程

模型为 gpt-6-luna，reasoning=max，Observer 开启。下表中的步数是 Worker 循环预算；工具调用数不包含宿主拦截的 report_progress。

| 阶段 | 首轮步数 | 结果 |
| --- | --- | --- |
| discover | 1–6 | 找到根目录脚本及真实 PPTX，读取 5 份文件；包含两种终端路径查询与一轮纯汇报 |
| check_services | 7–16、18–20 | 节点累计 13 轮；多次查询受管进程及端点，出现临时命令语法错误；启动前最终确认连接拒绝，启动后确认 HTTP 200 |
| start_services | 17 | `run_project_script(script=dev, background=true)` 成功，前端 ready=true |
| confirm_services | 21 | 对已完成服务结果再做一次无工具探测的交付确认 |
| upload_pptx | 22–29 | visible=true 开页，真实上传一次，拿到 change 和加载周期；两次 yield 被文字匹配门禁拒绝，最终完成 |
| 首轮停止 | 30 | MAX_STEPS；独立 verify_pptx 尚未调度 |

首轮事件时间为 22:34:37 至 22:51:15，约 **16 分 39 秒**，36 次实际工具调用。包含 8 次 get_project_process、10 次 run_command、5 次 read_file、6 次 yield_work。服务确认部分存在明显重复；没有出现重复上传。

首轮可取到保存的实际请求体与工具返回，不需要根据 Worker 的事后总结推测。Worker 上下文 ID 498 对应执行 step 10：其 `actual_operations` 同时保留了两个不同进程采样时间 `1791297609198` 和 `1791297636519`，并保留 3000 与 8080 的连接失败输出。下一次进程采样时间又变成 `1791297651987`。因此“沿用同一旧时间戳”不是数据缺失，而是 Worker 对上下文的错误陈述。

失败信号也不够规整：PowerShell 的 `Invoke-WebRequest` 命令把异常捕获后输出了含 `error` 字段的 JSON，却以 shell exit code 0 返回，stderr 为空，错误文本还出现乱码。之后 Worker 又尝试当前 PowerShell 不支持的 `System.Net.Http.HttpClient` 类型、写错 Node HTTP 命令括号，最后才从有效 Node 请求取得明确的 `ECONNREFUSED`。拿到明确拒绝后它仍继续重复采样。

step 8–15 的 `execution/tick` 全部记为 `repeated_reads=0`、`should_yield_to_organizer=false`，即使检查目标和“服务不可达”的结论重复。调度器没有把相同服务结论识别为停滞；反复 `force_refresh` 生成新采样时间和操作记录，使这些轮次看起来仍有进展。这与上下文被压缩是不同的问题。

实际请求元数据也显示，Organizer 在首轮共发起 11 次请求（5 work、5 continue、1 select）；Worker 30 次，Observer 17 次（10 完成、7 中断）。Observer 在 step 8 检查的是“尚未取得服务证据”的任务分派，之后没有逐个 HTTP/进程工具结果进行实时复盘，因此没有在重复探测发生时及时让 Organizer 收束。Observer 的总累计处理时间约 152 秒，与主流程部分并行，不能简单加到墙钟时间上。

启动服务后仍需重新验证 8080，因为启动脚本的就绪探测只直接确认 3000 前端；这次重新检查字体端点是合理的。不过后续 `confirm_services` 节点只消费并封存已有 200/JSON 与进程归属，没有再调用检查工具，是一次可以合并掉的交接确认。

首轮具体证据：

- seq 599：上传实际文件，17,378,393 字节，files_length=1，change_event_received=true。
- 上传 ID：`upload_visual-38276-1791298087297-36`。
- seq 650：browser_read 报告 application_load_cycle、upload_load_status=loaded、同一上传 ID、8 张、Slide 1 / 8，无显式加载错误。
- seq 619、698：yield_work 被通用 browser_read 非空 expect_text 条件拒绝。
- step 27：当前上下文明确要求调用 browser_read(expect_text="Slide 1 / 8")，历史实际读取的 expect_text 仍为空。Worker 却直接 yield，声称非空匹配已经完成；宿主正确拒绝了该未经工具支持的声明。
- seq 726：实际带非空 expect_text 的读取成功。seq 746：上传节点完成。
- seq 793：达到步数上限。

## 续做：有人工明确引导，不能算自主首轮成功

首轮结束后，宿主按 max_steps 状态关闭了浏览器。追加指令明确说明旧会话已失效，要求重新可见上传，然后以显式 browser_upload_receipt 依赖做独立验证；复用已运行服务，不再调查项目。

续做约 **4 分 42 秒**，4 次工具调用：browser_open → browser_upload → browser_read → yield_work。

- 新上传 ID：`upload_visual-38276-1791298460207-54`。
- seq 854：新会话真实上传成功。
- seq 869：同一新上传周期加载成功，8 张、Slide 1 / 8。
- seq 890：上传节点完成。宿主将独立的 browser_upload_receipt 写入交付。
- step 5 / seq 907：Organizer 想把旧 verify_pptx 叶子节点标为 skipped；宿主拒绝：`Organizer may aggregate only completed children; leaf completion belongs to the host`。
- step 6 / seq 913：修正时把已有 verify_recovery 的 parent_id 从 goal 改为 verify_pptx；宿主拒绝：`reparenting an existing node is not allowed`。
- seq 916：一次定向修正后仍非法，任务以 failed 结束。没有执行独立验证。

两次失败结束均触发浏览器清理。服务仍可运行，但本次验收没有交付一个完成后保留的文稿窗口，不能沿用 Worker 先前的“窗口保持打开”作为最终状态。

## 必须修复的流程问题

### 1. 上传交付与加载验收的合同冲突

`src/work_scheduler.rs:1270–1276` 对所有 requires_pptx 节点同时要求非空文字匹配和完整文稿加载。Organizer 本次却为“只上传、交付回执”的节点设置 requires_pptx，同时禁止它做加载验收。源码存在跨节点回执支持，但缺少明确区分上传与验证完成条件的合同。

建议：为浏览器工作明确最小完成目标（上传交付 / 加载验证）。上传只要求目标文件、当前活动会话和本次 change 回执；验证要求显式绑定该回执、匹配加载周期、有效页码和幻灯片。字体和视觉验收按该节点声明的条件检查。不要依靠自然语言“本节点仅上传”覆盖通用硬门禁。

非空 expect_text 应只在任务明确要求文字匹配时成为门禁。结构化的本次加载事实已经满足文稿条件时，不应因为额外的页面文字参数再增加模型往返。空 expect_text 时的 matched=true 也不应容易被误读为已完成非空匹配。

### 2. Organizer 合法调度能力与 schema/反馈不一致

`src/work_organizer.rs:46` 提供 node_result.status=skipped，但 `src/agent_service.rs:2708` 只允许 Organizer 聚合已经完成的子节点。错误回报 field_path=decision，没有精确指出 leaf 的 node_result 不合法。

`src/work_organizer.rs:41–42` 对已有节点的 node_updates 暴露 parent_id，但 `src/flow_tree.rs:298` 禁止改变父节点。续做中第一次错误引出第二次错误，浪费约 79 秒 + 107 秒 Organizer 请求后任务失败。

这里并非完全没有灰色废弃能力：`TaskTree::deprecate_nodes` 会将分支标成 `deprecated`，但它在当前服务路径中只消费 Scheduler 返回的 `invalidated_node_ids`。Organizer 没有一个直接、受约束的操作来表达“这条尚未执行的旧叶子已被新计划取代”。因此 schema 暴露了 `skipped` 和 parent_id，却没有给这次计划调整一个匹配的合法调度动作。

建议：保留父节点不可随意改变、完成事实由宿主确认的约束。补上 Organizer 可用的显式废弃/替换操作，由宿主校验只能废弃未完成分支、保留灰色历史并选择有效新节点。根据节点身份限定 node_updates 字段；反馈精确到 `node_result.node_id` 或 `node_updates[index].parent_id`，列出原 parent_id 和可行动作。不能通过伪造叶子执行结果或重新挂树实现废弃。已有节点只修正允许字段，不要为补救一个状态问题改整个结构。

### 3. 单节点停滞与服务确认仍然过度

服务检查累计 13 轮，包含多次明确失败后的重查。get_project_process 没有受管进程时不能代替指定外部地址 HTTP 探测；工具合同应清楚区分“受管进程列表为空”和“端点已实测拒绝”。避免 Worker 反复期待空列表查询会产生新的就绪性结果。

本轮 step 27 的上下文保留了真实操作及明确修正要求，仍产生虚构已调用的声明，说明问题不全是材料丢失。应让宿主在结果中给出结构化的未满足条件及对应最小动作，并保留实际工具事实高于 Worker 汇报的规则。不能让 report_progress 重写结论算作执行进展。

首轮 Observer 给出了补做文字读取的有效建议，但 Worker 没有立即执行。Observer 复盘摘要更多重复节点交付，未充分指出服务检查空转与整体任务未完成。应以实际操作和状态跨度衡量推进，及时审查 Organizer 的边界与停滞处理。

### 4. 预算耗尽后完成节点无法推进

`src/agent_service.rs:3069` 最后一轮不再调用 Organizer（除非没有活动 order），`3250–3252` 移除工具。首轮第 29 步完成了上传，第 30 步只能让 Worker 汇报当前节点，不能决定验证/整体目标仍待完成。需要在预算结束时明确输出整体未完成、剩余节点与恢复策略，避免节点完成被误认成整个任务完成。

## 下一次验收要求

1. 上传与验证两个节点独立；上传节点不等待额外文本门禁，验证显式消费宿主回执。
2. 浏览器只创建一次、上传一次；后续节点不重新导航、不重传。
3. 同一上传周期 loaded + 有效页码/非零幻灯片数。
4. browser_diagnostics 中检查实际字体请求与失败资源；不能只用 /api/fonts 的 HTTP 200 或 document.fonts.status 代替。
5. browser_screenshot + 真正图像输入检查，返回绑定截图的结果。
6. Organizer 能合法废弃过期分支并继续；旧分支灰色保留。
7. 正常完成后可见窗口保留；发生失败时如实报告清理状态。

## 证据文件

- `.codex-workspace-mcp/ppt-upload-e2e-first-run-20261006.json`：首轮任务、事件与请求元数据。
- `.codex-workspace-mcp/ppt-upload-e2e-full-20261006.json`：两轮任务、事件与请求元数据。
- 原始上下文仍可在该任务的调试页面查看。没有在文档中复制完整系统提示、请求头或服务配置。

本轮结论只适用于实际走到的路径。没有截图，因此不判断文稿是否存在布局、图形或字体外观缺陷。
