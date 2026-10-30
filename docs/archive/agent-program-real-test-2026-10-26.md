# TS 执行层真实测试：函数任务与5页闭环验证通过

日期：2026-10-26。模型：当前已配置的 `gpt-6-luna`。本地独立工作区运行真实宿主 `single_agent::run_task` 和真实模型 API；不是浏览器交互测试。没有调用 PPT 插件，也没有把业务规则写进通用宿主。

## 请求与停止规则

只发送一条请求：

> 请依据 docs/generic-agent-design.md 制作20个独立的中文HTML页面介绍这套通用Agent，每页保存为一个HTML文件。请用两个执行Agent并行制作，各负责10页；主Agent准备公共材料、汇总检查并交付全部文件。

不预置 TS 程序、页面大纲或分工明细，不中途指导。遇到工具错误或实际执行异常结束该轮。小问题及 TS 解析／转换问题可以修复后在新工作区重跑；其他涉及执行结构的问题停止并报告。每次输入文档保持原样。

## 三轮结果

| 轮次 | 用时 | 结果 | 处理 |
| --- | ---: | --- | --- |
| 1 | 68.44 秒 | 编译拒绝字符串 `indexOf`；尚未执行制作程序 | 补纯字符串方法白名单及基础原始值比较，保存原失败源码作为回归样例 |
| 2 | 33.95 秒 | TS 整数 `2` 被转为 JSON `2.0`，`list_dir.max_depth: usize` 拒绝 | 修正整数的 JSON 表示，包括算术结果；加整数工具参数测试 |
| 3 | 34.23 秒 | 程序只读资料便返回；宿主状态 completed，实际 0/20 文件、0 个子 Agent | 判定真实业务测试失败，停止调度结构修改和后续真实重试 |

前两轮由监控发现错误并取消，数据库状态为 cancelled；第三轮宿主自然结束为 completed，但外部测试检查实际文件数和两个成功执行分支，明确判为失败。三轮均未进入两分支生成阶段，不能声称验证了 20 页并行性能、视觉质量或真实上下文折叠。

## 第三轮的确切问题

模型提交的程序完整如下：

```ts
function* main(input) {
 const source = yield call(tools.read_file, {path:"docs/generic-agent-design.md", max_chars:60000});
 return source;
}
```

程序本身解析、工具执行和返回都成功，但 main 只写了读取资料，漏掉了用户要求的并行制作、检查与交付。宿主执行 main 到 return 后结束任务，符合正常函数语义。

此前把这次失败归因为“程序返回与任务结束共用出口”的判断已更正。用户明确要求：整个任务只有一个 main，辅助函数返回后继续该 main，主函数返回就结束。应让模型像编写真实流程代码一样生成覆盖完整需求的主程序，保留正常返回语义；不应在 main 返回后再调用模型补下一段，也不添加业务页数检查到通用宿主。

源码还使用了不存在的 `max_chars` 参数；工具当前忽略额外字段，所以该参数本轮没有报错，不代表工具契约使用正确。原始证据及上述三轮失败统计保持不变。后续已收紧为无运算符编排，并修改主提示词和工具说明以生成完整的唯一 main；新复测记录另列。

## 已完成的实现与验证范围

实现了受限 TS → SWC AST → 可序列化指令 → Rust 执行器；包括局部函数、显式参数、变量、分支、数组循环、call/fork/join、模型求值 ai、try/catch 和未捕获错误中断。效果通过原有工具路径执行，保留权限和插件隔离。挂起位置及变量保存在工作窗口；有原始回执时恢复不会重放写入。

模拟测试已经覆盖两个分支、类型化返回、宿主等待、无额外模型调度轮次、拒绝写入后不执行后续操作及回执恢复。但模拟模型按预设提交完整主程序，因此它没有暴露真实模型只提交准备片段的问题。单元／集成回归通过不能替代这次真实失败。

实现目前应视为原型，不应当成已经跑通业务的稳定执行路径。没有部署到远程，没有修改正在运行的服务，没有提交或推送。

## 本地证据

三轮完整证据及汇总在 [本地测试目录](../../target/context-acceptance/ts-program-20261026/summary.json)：每轮包含 `program-1.ts`、`run-manifest.json`、`probe-report.json`、`summary.json`、原始事件数据库及请求日志。目录被 Git 忽略，不随源码提交。

- [第一轮](../../target/context-acceptance/ts-program-20261026/attempt-1/summary.json)
- [第二轮](../../target/context-acceptance/ts-program-20261026/attempt-2/summary.json)
- [第三轮](../../target/context-acceptance/ts-program-20261026/attempt-3/summary.json)

最终完整回归：**271 通过、0 失败、7 忽略**，用时 24.26 秒；调试构建成功。日志：[回归](../../target/context-acceptance/ts-program-20261026/final-regression.log)、[构建](../../target/context-acceptance/ts-program-20261026/final-build.log)。未在生产源码中保留临时真实测试钩子。

## 唯一 main 复测（2026-10-26）

用户进一步澄清：按编写真实流程代码的方式生成伪代码，整个任务只有一个 main，步骤写辅助函数，保持正常调用与返回。此次统一了主／子提示词、任务启动提示、任务板和工具说明；公开 execute_program 只进入 main，内部 fork 进入已声明的辅助函数。未修改 main 返回结束任务、辅助函数返回继续调用方的语义。

仍使用同一条“两执行 Agent、20 个独立 HTML”请求，没有中途补话、人工分工或代写输出。夹具提供当前设计文档与仓库顶层 Markdown 文档，供已有引用可读；没有预置大纲或程序。因此这轮功能复测不用于和之前单文档夹具比较速度。

第一次复测生成的 main 已有规划、两路制作、检查和交付，但在程序中直接调用 native spawn_agent/join_agents，被编译器拒绝。检查发现主提示和任务板还同时建议旧的原生调度接口，已统一为程序内 call/fork/join，并明确程序工具返回原始 JSON、没有会话交付用的 original_result 包装。

第二次复测编译成功，实际执行了两次 fork 和一次 join：

- 主程序包含总纲规划、两个 worker、汇总检查及逐文件检查，没有再只读资料就返回。
- 模型构造的 left.pages 和 right.pages 都引用 shared.pages，即完整的 20 页；两路生成 ai() 输入均只有 pages/shared，没有传各自页段。两份生成配置的 SHA-256 完全相同。
- 第一个分支实际写了 01.html—20.html 全部 20 个文件，第二个分支还在生成。存在重复劳动和同名覆盖风险，未完成“每个 Agent 各负责 10 页”的分工。
- 发现该任务数据流错误后，按停止约定用 SIGTERM 终止了自己启动的真实测试进程，退出码 143。没有改生成程序、补传分组或继续跑到成功；没有启动后续真实重试。快照中的任务／分支 status 仍为 running，表示进程终止前的历史状态，不能解读为仍有执行进程或已完成。
- 没有完成父任务验收、逐文件检查或浏览器视觉检查，20 个已写文件只是部分测试产物，不能作为并行测试通过的依据。

这次剩余错误是生成程序的分工参数不正确。宿主按程序传参与执行；没有通过加入页数、分支数量、重复输入等业务硬判断去修补。完整主函数生成已有改进，但端到端任务仍未验收通过。

最终本地回归：**276 通过、0 失败、7 忽略**，用时 23.66 秒。新增测试验证唯一 main 以及辅助函数返回后继续主流程；模拟双 Agent、权限和回执恢复仍通过。调试构建成功。未部署、未提交或推送。

证据：[两轮汇总](../../target/context-acceptance/ts-program-20261026/single-main/summary.json)、[第二轮外部停止记录](../../target/context-acceptance/ts-program-20261026/single-main/attempt-2/external-stop-report.json)、[实际主程序](../../target/context-acceptance/ts-program-20261026/single-main/attempt-2/program-1.ts)、[回归日志](../../target/context-acceptance/ts-program-20261026/single-main/regression.log)。原始事件数据库、模型请求、夹具、完整程序和部分生成文件均随各轮本地证据保存，Git 忽略。


## 普通函数、注释任务与工具库（版本 3）

按用户进一步明确的写法，增加专门的 [task-program skill](../../skills/task-program/SKILL.md)，规划请求固定加载；普通辅助函数可以编排工具库调用，也可以调用只有注释的任务函数。后者把注释作为任务说明、实参按参数名作为输入，在局部作用域使用正常工具循环完成，返回 JSON 后恢复调用方。执行任务体时不再要求重写 main，也不暴露任务／并行调度工具。main 返回仍然结束整个任务，没有新增完成闸门。

编译器支持普通调用、嵌套调用和结果引用，仍拒绝运算符与普通 JS 方法。新回归覆盖混合调用、注释提取与参数绑定、双 Agent 内部任务实际写入、类型化返回、局部窗口释放、错误传播和挂起任务回执恢复。最终 **280 通过、0 失败、7 忽略**，用时 25.74 秒。技能格式校验通过。

随后在全新工作区只发原来的初始请求，没有补话、预写分工或修改模型生成的程序、HTML：

| 项目 | 实际结果 |
| --- | --- |
| 主程序 | 一个 main：prepare → 两次 fork(make_pages) → join → verify_and_deliver |
| 分工 | 两份不同的十页清单进入实际参数 |
| 制作 | 两个执行 Agent 均 completed，各写 10 个文件；20 个路径互不重复 |
| 工具执行 | 没有解析或工具错误；join 只调用一次 |
| 独立浏览器检查 | Chromium 打开全部 20 个文件，1440×1000 与 390×844 两种视口；无页面脚本错误、横向溢出或检测到的文字／页脚重叠。抽看首尾页截图，内容可读。该检查在 Agent 之外执行，不向其提供反馈 |
| 上下文 | 实际触发步骤折叠；最后请求仍保留完整验收函数说明、命名实参、20 个文件清单及实际工具结果，未观察到主目标丢失 |
| 未通过项 | 验收被写成一个宽泛的注释任务。模型在其局部工具循环里重复列目录、搜索同一个 HTML 结构、读取第一个文件和自己的历史，没有稳定推进逐文件验收与返回 |
| 停止 | 按“重复多了立即停止、结构问题不继续改”的约定，对本轮测试进程发送 SIGTERM；约 363 秒时停止，退出码 143。未继续调结构或重跑 |

因此本轮只能确认：通用表示、注释任务、双分支制作、返回与等待已实际工作，文件可独立打开。**端到端测试仍未通过，主流程没有完成最终交付。** 不能把 20 个文件或两个 completed 分支等同于整项任务验收。

重复调用来自模型新发出的工具调用，不是宿主重放同一条挂起指令。完整源码中的 verify_and_deliver 只有一段笼统注释，没有将各文件检查表达成明确迭代与返回数据。把它细化为可见的循环、逐件检查与汇总，符合用户所说的“函数里编排工具库”的方向；本轮遵守停止边界，没有再加入业务硬判断或临时修改模型程序。折叠与重复在时间上有关，但当前证据不足以单独归因于压缩丢失任务；最后的实际请求中任务输入和最新返回仍存在。

证据：[真实程序](../../target/context-acceptance/ts-program-20261026/comment-tasks-v3/attempt-1/program-1.ts)、[统计与调用记录](../../target/context-acceptance/ts-program-20261026/comment-tasks-v3/attempt-1/summary.json)、[停止记录](../../target/context-acceptance/ts-program-20261026/comment-tasks-v3/attempt-1/external-stop-report.json)、[浏览器报告](../../target/context-acceptance/ts-program-20261026/comment-tasks-v3/attempt-1/inspection/browser-check.json)、[回归](../../target/context-acceptance/ts-program-20261026/comment-tasks-v3/regression.log)。该目录保存一致性的原始事件／请求数据库、技能、输入文档、源码与 20 个 HTML。停止前数据库中的主任务 running 是历史快照，测试进程已终止。

真实测试使用临时编译的测试入口，源码中已移除该入口；没有重启服务、部署、提交或推送。


## 通用传参与迭代规范复测

用户要求只改通用编排指导，禁止把举例用的具体业务写进提示词。本轮只调整 task-program skill 与主提示中的编排说明，未修改 Rust 执行语义、增加业务工具或业务判断。运行提示词、skill 和内置业务 skill 中未发现烧水／热菜／修锅场景；这些例子只在既有 Rust 测试夹具中保留，不注入运行请求。

补充了三项通用要求：共同依赖作为显式参数传给需要它的函数，分支另外接收职责；资产采用真实稳定引用、路径及版本，完整公共规则不能用一句指代代替；已知迭代和调用顺序写进程序，注释任务承担具体条目的判断与处理。示例仍是通用函数与文件工具，不含测试任务的业务、规模或分工规则。

同一初始请求、新工作区，不预写大纲或程序、不向执行中的 Agent 补话，进行了两轮真实测试：

| 轮次 | 用时 | 实际结果 |
| --- | ---: | --- |
| 1 | 42.8 秒 | 模型提交的源码未闭合，编译在产生效果前以 Expected("}", "<eof>") 拒绝；0 文件、0 子 Agent，自动停止。未改解析器接受错误源码 |
| 2 | 288.7 秒 | 共享资产和规则传参正确；两个 Agent 各写10个互不重复的 HTML 并返回。验收仍是整个阶段的注释函数，第三次在同一验收作用域列相同目录时自动取消 |

第二轮的实际调用为 fork(make_pages, shared, assignment)。两份 shared 内容一致，source 中包含真实已登记的 asset_id、源路径和 code_hash，html_rules 是完整公共规则对象；两份 assignment 分别明确不同编号范围。没有再以一句“沿用公共规范”代替共享对象，制作分支也没有重复读取源文档。但 inspect_delivery(shared, results) 仍只有整体检查的注释，没有生成逐项迭代。该轮说明传参规范有改善，不能证明模型已稳定遵循程序粒度要求。

独立 Chromium 检查全部20个文件，两种视口均没有检测到页面脚本错误、横向溢出或文字／页脚重叠；未把此反馈交给 Agent。验收中只有少量文件被读取，其余检查没有完成，**端到端仍未通过**。停止来自测试监控，不是新加的生产状态门；主任务实际保存为 cancelled，两个制作分支为 completed。

现有回归 **280 通过、0 失败、8 忽略**，25.19 秒；其中多出的一个忽略项是临时真实测试入口，入口已从源码移除。skill 格式校验通过。没有再修改宿主结构，没有重启服务、部署、提交或推送。

证据：[首轮报告](../../target/context-acceptance/ts-program-20261026/explicit-flow/attempt-1/probe-report.json)、[第二轮统计](../../target/context-acceptance/ts-program-20261026/explicit-flow/attempt-2/summary.json)、[第二轮完整程序](../../target/context-acceptance/ts-program-20261026/explicit-flow/attempt-2/program-86.ts)、[浏览器检查](../../target/context-acceptance/ts-program-20261026/explicit-flow/attempt-2/inspection/browser-check.json)、[回归](../../target/context-acceptance/ts-program-20261026/explicit-flow/regression.log)。两轮均保存原始事件和模型请求数据库，未修改生成源码与产物。


## 函数任务统一与5文件快速闭环（版本4）

用户澄清：空函数的注释是给执行 AI 的说明，不表示这个任务只能是叶子；调用 AI 定义的方法就是任务，调用宿主工具就是步骤。本轮修正了此前实现中的两处偏差：有代码的函数也进入任务作用域；空函数执行 AI 可以补写当前函数体并继续调用子任务。采用 define_task_body 在当前作用域安装实现，不新建第二个 main、不重复创建当前任务。沿用原有任务、步骤、finish 与持久化机制，未加入具体业务流程。

函数实现可以按命名实参绑定形参，或用单个形参接收完整命名输入对象；函数名和调用方的返回契约保持不变。新增本地数组 push 指令收集循环结果，仍拒绝运算符与任意 JS 方法。工具错误保留原始错误并提供稳定异常消息，跨任务传回父函数 catch。临时任务深度上限统一为函数资源边界64层，相应提高父链防环界限；未新增业务完成状态。

先通过模拟测试验证四层函数任务、空函数补写后包含代码和注释子任务、参数／字符串返回、错误上抛、父函数继续及局部窗口释放，再使用当前 gpt-6-luna 进行真实测试。初始请求仅将20个HTML缩为5个，保留两个执行Agent；不提供大纲、程序或分工，不中途补话。

| 轮次 | 用时 | 结果及处理 |
| --- | ---: | --- |
| 1 | 120.6秒 | 一分支写出2页，另一分支把长正文塞入TS源码后转义损坏，编译拒绝并停止。补充通用的编排源码与长数据分离规范；未修补模型生成源码 |
| 2 | 48.2秒 | 模型开始用逐项 render_page 子任务制作，但调用本地数组 push 收集结果被既有子集拒绝，停止。补上确定性的本地数组追加指令及回归测试；没有放开运算符 |
| 3 | 236.0秒 | 5页均已生成并被主任务顺序读回，进入独立验收子任务；单个 input 形参被过严的同名参数校验拒绝，停止。明确支持整体命名输入对象并补测 |
| 4 | **147.75秒** | 两个分支分别写入3页和2页；主任务顺序读取全部5页，进入 reviewPages 子任务，补写后正常返回，主任务 completed，测试通过 |

最后一轮的实际验收实现：verifyAndSummarize(input) 声明5个实际路径，用 for...of 调用 read_file，reads.push 收集正文，然后调用 reviewPages(input.shared, input.outputs, reads)。后者以已提供的五份实际读取结果形成检查结论并返回，原调用链继续结束。既包含有代码体的函数任务，也包含由注释任务进一步补写的嵌套任务，没有继续重复列目录或查历史。

实际统计：5次文件写入、5个互不重复的HTML；主任务对5个文件各读取一次；2个执行Agent均 completed；0个工具错误、0次 read_session_history；join一次。过程中触发上下文折叠后仍正常交付。独立 Chromium 打开5份文件，1440×1000与390×844两种视口，未检测到脚本错误、横向溢出或文字／页脚重叠；抽看两张桌面截图可读。该浏览器检查在 Agent 之外执行，未反馈给执行中的模型；Agent 自己也如实标明未进行视觉实测。

完整回归：**285通过、0失败、8忽略**，25.26秒；其中1个忽略项是临时真实测试入口，入口已从源码移除。技能格式校验、调试构建及差异空白检查通过。此前失败记录完整保留，没有改写生成的程序或HTML。该结果只验证了本轮5文件流程，20页与PPT插件未重新测试；常驻服务未重启，未部署、提交或推送。

证据：[最终统计](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/attempt-4/summary.json)、[实际main](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/attempt-4/execute_program-100.ts)、[验收函数补写](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/attempt-4/define_task_body-275.ts)、[验收子任务补写](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/attempt-4/define_task_body-327.ts)、[浏览器报告](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/attempt-4/inspection/browser-check.json)、[回归](../../target/context-acceptance/ts-program-20261026/nested-tasks-v4/regression.log)。各轮均保存输入、源码和原始事件／请求数据库。
