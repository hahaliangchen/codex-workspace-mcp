# Agent 流程伪代码与 Rust 执行器

当前语言版本 **12**。使用常见 JS 基础语法表达流程，由 SWC 解析、Rust 执行持久化指令；没有 JS 虚拟机。**普通表达式由宿主确定性求值，智能判断必须写成显式任务函数。** 相同程序、相同输入和相同工具／任务返回值产生相同控制流；AI 任务及外部操作本身不因此变成确定性的。

这是面向 Agent 工作流的领域语言。可以借用 JS 的写法，但执行语义以本手册为准。业务 skill 提供需求和流程，语言负责表达函数、数据依赖和控制流，宿主负责执行；学习语言不替代加载当前业务的 skill。

## 顶层入口与工具导入

完整流程直接写在源码顶层，按顺序运行；不必声明 main，也不要求返回值。execute_program 的 input 在顶层以 input 访问，普通函数的返回值仍用于后续计算。通过 `import * as io from "host:io"` 或 `import {read as load} from "host:io"` 引入当前权限目录中公布的工具；支持命名导入和命名空间导入，别名仅为本地绑定，不扩大权限。工具仍以一个 JSON 对象传参并经过原权限检查。其他模块、默认导入和动态导入不支持。顶层运行完毕表示流程结束，不代表业务验收自动通过。

下面保留部分旧式函数入口示例作为历史参考；新流程应采用顶层入口。program_tool_headers 是精简的库和方法目录；完整契约由 read_tool_contracts 按需获取，不随每次请求重复注入。

## 阅读路线

1. 从下面两个完整示例理解一次工作如何开始、执行和交付。
2. 查阅“固定语言范围”“确定性的值规则”，避免照搬完整 JS／TS 的能力。
3. 需要调用业务工具时先看本轮 `program_tool_headers` 目录，再用 `read_tool_contracts` 获取相关签名，需要细分当前 AI 任务时读“当前函数的补写”。
4. 材料和并发问题查“函数作用域与工作空间预算”“已知循环与授权并发”，错误查“失败与恢复”。

宿主只提供简短的流程运行环境说明，不自动注入本手册、Skill 或完整示例。需要了解语言能力和差异时，可用 read_program_guide 按需查询；[task-program Skill](../skills/task-program/SKILL.md) 与详细手册保留供人查阅。示例源码和 JSON 输入分开保存，支持直接校验；它们是教学案例，不固定其他业务的页数、任务数量或步骤。

## 从最简单的串行执行开始

源码：[serial.js](../skills/task-program/examples/serial.js)；输入：[serial.input.json](../skills/task-program/examples/serial.input.json)。

这份程序在顶层顺序执行，不创建并发 Agent：

```text
顶层流程（input）
  → compose(input)           AI 设计，返回正文
  → io.write(...)             工具保存
  → io.read(...)              工具读回实际文件
  → inspect(request, content) AI 检查，返回 passed 和证据
  → assert(review.passed)     宿主消费检查结论，失败则停止
  → 流程结束
```

`compose` 和 `inspect` 带 `@virtual`，需要 AI 理解与处理；写文件、读文件及布尔判断有确定规则，直接由宿主执行。普通函数调用使用局部 Runtime 函数帧，不创建 AI 任务节点，也不启动另一个 Agent。此例用 HTML 说明串行数据流，只检查实际执行过的内容／结构，不声称已经完成浏览器视觉验收。

已有的真实结果通过参数交给需要它的函数，不因为进入虚函数就重读原材料。虚函数可在同一上下文连续观察、调用工具和处理；其职责是只检查还是包含修复，由任务确定。调用方对函数契约中可以确定的输入与输出关系做代码核对，不能把不匹配的返回结果当作有效验收。处理实际状态后需要复检时才重新读回；复杂处理可以按需补写当前函数体。

虚函数是流程中的一次 AI 能力调用，有明确输入、职责和返回值；不要把它当作省略整段流程实现的默认容器。能具体表达的读取、遍历、判断分支、写入和复查等步骤尽量由普通函数编排；需要 AI 理解、判断或生成的部分才交给职责明确的虚函数，调用方再用其返回值继续推进。划分应符合当前任务，虚函数内仍可按职责和授权逐步使用工具、处理与复查，无需为了形式强制拆分。

验收要覆盖当前任务的实际要求，禁止用表面特征冒充完整验收。基础文本、数量或存在性检查只能证明其实际检查到的事项，不能单独证明内容正确或资源可用。可确定的步骤尽量写成普通代码，需要理解与判断时调用职责明确的 AI 函数或适用工具；未检查或无法确认的事项如实报告。遇到语言限制或工具缺失时，不得删减必要检查后将结果当作全面通过，应该改用其他可行能力或说明未完成的原因。

开始时在已有主任务中，把文件原文作为 `execute_program.source`，把 JSON 作为 `execute_program.input`。没有主任务时先用当前工具契约创建一次；不为每条库调用再创建主任务。`check_program` 可以预检，提交本身也会先编译。

```json
{"source":"这里放 serial.js 的完整源码文本","input":{"topic":"通用 Agent 的职责边界","output":"agent-overview.html","requirements":"独立中文 HTML、内嵌 CSS、说明各角色职责"}}
```

这里的 `source` 是源码字符串，不能填文件路径让宿主猜。此 JSON 说明提交格式；实际使用文件原文。宿主将输入直接绑定到顶层变量 `input`；示例用 `const request = input`。

## 一个 AI 与并发只读操作

[文件读写普通函数](../skills/task-program/examples/file-functions.js)展示如何把已提供的内容写入文件并读回：`writeFile`、`readFile` 都是有实际函数体的普通函数，由宿主直接执行工具调用，不为读写单独请求 AI。每次写入后的读取按顺序进行。此示例串行处理各文件；宿主对单文件写入按路径协调，不同文件的独立写入可以并发。不过当前程序的 `Promise.all` 仍不支持写入或 AI 虚函数调用。

[串行页检查](../skills/task-program/examples/serial-page-review.js)使用普通函数依次读取页面，再把结果交给同一个 AI 上下文判断。[并发只读页检查](../skills/task-program/examples/async-page-review.js)改用 `await Promise.all(request.paths.map(path => io.read({path: path})))`：独立文件读取并发执行，结果仍按输入顺序交给同一个 AI 逐页判断，不启动子 Agent。

支持在顶层或 async 函数中对单个函数、工具调用或 JSON 值使用 `await`；宿主按已有的串行暂停与恢复机制等待结果，不产生 JavaScript Promise 对象。`await Promise.all` 接收只读调用的数组字面量，或接收数组的 `map`，其中箭头只有一个简单参数和一个只读调用，允许在箭头和调用上使用 async / await。允许的调用为 `io.read`、`io.read_lines`、`io.list`、`io.search`，每批最多 32 项；不允许并发写入、AI 调用或其他副作用。输入表达式不能有副作用。所有读取完成后返回按输入排列的真实结果；任一失败进入原有异常路径，宿主不自动重放。直接 await 的 Promise.resolve/reject 链以简单值为初始参数，不支持有副作用的表达式或属性访问；它支持 then/catch 的零或一个简单参数的表达式箭头，按顺序执行；一般 Promise 对象和其他 Promise 链仍不支持。

宿主为这些任务的模型请求共用一个并发队列，默认同时执行 3 个；启动进程时可用 `CODEX_AGENT_MODEL_CONCURRENCY` 设置正整数上限。等待名额的时间不算模型请求超时，取消任务会退出等待。此限制本身不会让虚函数并发执行：目前 `Promise.all` 尚不能接收 AI 虚函数调用。

## 完整示例：两个 Agent 并发制作五页

源码：[parallel-five-pages.js](../skills/task-program/examples/parallel-five-pages.js)；输入：[parallel-five-pages.input.json](../skills/task-program/examples/parallel-five-pages.input.json)。此例基于已跑通的两 Agent／五 HTML 流程整理，不复制测试中临时补写的长 HTML 正文。

| 阶段 | 输入 | 实际动作 | 输出／归属 |
| --- | --- | --- | --- |
| 公共材料 | 源文档路径、要求、五页清单 | 主 Agent 读源文件一次，组装 `shared` | 真实正文与统一要求 |
| 分工 | 同一个 `shared`、不同页面切片 | `fork(makePages, shared, 前三页)`，再 fork 后两页 | 两个句柄；分支文件路径互不重叠 |
| 制作 | 各分支自己的页面清单 | `makePages` 循环调用 `designPage`，再调用 `io.write` | 每个分支返回自己的文件清单与原始写入回执 |
| 汇合 | 两个句柄 | `join(a,b)` | 按句柄顺序返回两个分支的函数返回值 |
| 验收 | 公共材料、每页目标、真实读回正文 | 主 Agent 先按句柄整合 Git 分支，再逐页 `io.read`、`inspectPage`，检查 `passed` | 五项验收记录；任一失败则停止 |
| 交付 | 所有文件及检查记录 | 核对数量，`main` 返回 | `{files,reviews}` |

必须先 fork 两路再 join；`fork → join → fork → join` 会变成串行等待。`makePages(shared,pages)` 有两个形参，fork 就传两个实参，不隐式包成 `{shared,pages}`。两个分支共用材料的值快照，各自只收到自己的页面。主 Agent 读回的是实际文件，不把分支声称“已完成”当验收。

五页数量断言和 3+2 分工只属于此示例。其他请求根据自己的工作量与依赖组织。去掉 fork 后，直接顺序调用两次 `makePages(shared, part)` 就是在当前 Agent 中串行执行；函数任务与并发 Agent 是两个不同概念。

这份例子的 `assert` 在发现问题时如实停止，未演示自动修复。需要修复时，由业务 skill 决定修复范围和检查标准，在普通控制流中显式调用修复任务、再验收；不能无限重试，也不能捕获失败后直接返回“成功”。

## 真实运行参考：主 Agent 与一个分支制作二十页 HTML

完整源码：[parallel-twenty-html.js](../skills/task-program/examples/parallel-twenty-html.js)。这是 2026-10-27 在隔离 Git 工作区用 Terra 实际运行完成的流程；与当次提交的可执行语句一致，文件开头的说明注释已更新。它读取本项目的五份指定文档，AI 设计一份共享 CSS；主 Agent 负责页面 1–10 和 CSS，同时 fork 一个子 Agent 负责页面 11–20。两边各调用一次 AI 批量生成正文，普通函数负责模板、循环、写入和结构检查。join 后，主 Agent 检查子分支差异及 head，合并到主工作区，逐页读回并进行内容审查；失败即停止。此次 20 页存在且通过基础结构检查与 AI 内容审查，未做浏览器渲染或视觉验收。真实运行只说明当次输入、环境与模型下的结果，不保证以后模型返回或其他任务成功。

学习重点是控制流而不是 HTML 业务约束：可预先写清的分工、循环、工具调用和合并由普通函数实现；需要生成或语义判断才使用 `@virtual`；只在用户或适用技能授权时 fork，并在 join 后按实际 workspace 判断是否需要 Git 整合。分支路径必须独占，写入和检查必须针对真实产物；参数、返回结构及工具以当前发布的契约为准。

换场景时不要照抄页面数量、路径、输入文档、共享样式、模板、差异格式或验收标准。应根据实际要求和适用技能设计输入、输出、生成方式及验证；只有独立部分才适合并行，共享资源和依赖要显式协调。需要视觉验收时应执行相应检查，不能把本例的文本检查当成视觉验收。未知资料、权限或工具能力不能由这个示例补充或假定。

## 三类调用和执行者

| 写法 | 谁处理 | 何时完成 |
| --- | --- | --- |
| 普通 `helper(args)` | 宿主进入函数局部作用域并执行其代码 | 函数 `return` 后恢复调用方；注释不会自动执行 |
| `@virtual` 的 `helper(args)` | 当前 Agent 的 AI 读取职责注释与真实参数并实际工作 | 返回约定 JSON，或补写当前函数后继续执行 |
| `library.method({...})` | 已发布的真实工具 | 得到工具原始 JSON；业务验收仍由流程定义 |

`fork(helper, args...)` 才会建立另一个获授权执行 Agent；`join` 收集其返回。普通函数代码可引用调用位置可见的上层变量，按函数进入时的值读取，不能通过子函数赋值改写上层变量。这是本流程语言的调用作用域规则，不等同于 JavaScript 顶层函数的词法作用域。未使用的值不自动发送给 AI，AI 可根据变量索引按需读取；虚函数注释中的自然语言不自动推断变量引用。子 Agent 使用 fork 当时已存在的变量快照，之后父作用域的变化不自动同步。

## 当前函数的补写

进入 `designPage(shared, page)` 后，AI 已经是这项设计工作的执行者。可以直接使用公布的工具并返回 `{content:...}`，不必再规划一个同义虚函数。只有确实需要把工作拆成不同步骤时，才使用 `define_task_body`。

补写源码必须定义同名函数 `designPage`，保留两个形参（按位置绑定），去掉该函数的 `@virtual`，写出实现；新增的 AI 子任务依然使用带职责与返回契约的 `@virtual`。不创建第二个 `main`，不从头重做已完成的读写。详见后文“任务函数与工具步骤”。

## 失败与恢复

| 情况 | 意义 | 下一步 |
| --- | --- | --- |
| `PROGRAM_SOURCE_INVALID`、`executed:false` | 整份源码编译／校验未通过，拒绝源码未产生效果 | 当前 AI 按行列诊断重写完整源码，在同一作用域重新提交 |
| 当前虚函数补写的函数体出现未捕获错误 | 之前的操作可能已发生，原调用方仍在等待 | 宿主保留失败函数帧和回执，将错误交回同一 AI 函数；按实际状态修复、核查并返回原契约，或如实返回 `$task_error` |
| 普通主流程出现未捕获错误 | 执行中出现明确失败，之前已完成操作可能存在 | 当前实现停止并保留错误，不自动重放外部写入；流程可显式用 `try/catch` 处理 |
| 虚函数返回无效 JSON | 返回值无法交给调用方 | 同一 AI 函数收到诊断并继续，直到返回有效 JSON 或达到执行限制 |
| `@virtual` 无法完成 | 执行 AI 未取得要求的结果 | 返回 `{"$task_error":{"message":"具体原因"}}`，沿调用链上抛 |
| `join` 已返回／`main` 已返回 | 调度已交接／主程序已结束 | 仅当代码已完成业务要求的检查，才能称作业务完成 |

错误捕获使用 `try/catch`；`finally` 不在语言内。补写体失败后的 AI 可直接调用工具，也可重新 `define_task_body` 编排剩余工作；新函数体从第一条语句开始，不能当成从失败指令自动续跑。已保存的局部变量可通过 `host.read_scope` 读取；原有操作回执保留。跨进程缺少外部回执时，副作用是否发生可能未知，不能承诺外部操作恰好一次。主流程未捕获错误暂不自动进入 AI 恢复。

## 固定语言范围（版本 12）

| 部分 | 允许范围与含义 |
| --- | --- |
| 文件 | 顶层顺序执行，无需 main 或返回值；input 为输入。普通 function 定义辅助函数。补写源码只有当前函数及所需 helper，没有第二个 main |
| 数据 | JSON 字符串、有限数字、布尔、null、数组、静态键对象；成员／下标读取、对象简写、简单数组解构 |
| 局部值 | const/let/var、局部赋值、赋值表达式、局部 ++/-- 与 += -= *= /= %=。const 不重绑定。变量不共享可变别名 |
| 调用 | helper(args)、已发布的 library.method(args)（如 io.read）；编译时核对 helper 实参数量，不允许调用或 fork main |
| 控制流 | if/else、for(init;test;update)、while、do/while、数组 for...of、无标签 break/continue、三元表达式、return、assert、try/catch/throw |
| 表达式 | + - * / %、一元 + - ! typeof、== != === !== < <= > >=、&& || ??、模板字符串；宿主直接计算，不创建模型任务 |
| 收集及数据方法 | 本地数组 push；确定性的 includes/indexOf/slice/at/concat、字符串 lastIndexOf/startsWith/endsWith/trim/toLowerCase/toUpperCase/split/replace/replaceAll/match、正则 test、数组 join，以及 JSON.stringify/JSON.parse、Array.isArray、Object.keys/values |
| 路径和 URL 计算 | `path.join/resolve/relative/normalize/dirname/basename/extname`、`url.parse`；由 Rust 直接计算，不访问文件系统 |
| 授权并发 | fork(helper,args) 启动获授权的子 Agent，join(handles) 等待并按顺序返回；子 Agent 不再 fork |

常见语法归约为已有赋值、分支、跳转、调用和返回结构，不为每种写法新增调度协议。for 的 continue 进入 update 再检查条件；while 的 continue 进入条件；do/while 的 continue 进入末尾条件。break/continue 只退出其跨越的异常处理现场，保留外围 catch。

顶层和 async 函数可以 await 单个调用；`await Promise.all` 可以并发执行受限的只读文件操作。不支持一般 Promise 对象或完整 JS。其余仍禁止 function*/yield、call()/ai() 包装、非工具模块导入/export、TS 类型／断言、一般箭头／匿名／嵌套函数、闭包、class/new/this、for-in、switch、finally、标签跳转、展开／剩余／默认参数、可选链、BigInt、位运算、void/delete、对象成员写入，以及未公布的数据方法和 JS 全局环境。源码字符串里的这些词语只是数据，不按词语拦截。需要智能处理时声明注释任务；没有“计算失败后自动交给 AI”的路径。

## 确定性的值规则

- 数字保持 JSON 数字类型。十进制整数常量及整型加减乘余数保持精确的 JSON 64 位整数范围（有符号负数／无符号非负数），溢出报错。小数与除法使用有限 f64，5/2 为 2.5；除零、余数除零和非有限结果报错。整数转小数若超出 2^53-1 安全范围则报错，不能静默丢精度。
- + / += 任一侧为字符串时，对 JSON 原始值做文本拼接，包括数字、布尔、null；两个数字仍相加，数字转文本保留 JSON 整数精度。数组／对象需要显式 JSON.stringify。其他算术只接受数字，关系比较接受两个数字或两个字符串，字符串按 Unicode 标量顺序比较；这些算术、关系比较与相等判断不做字符串／数字／布尔隐式转换。条件真值规则单独规定。
- == 与 === 都是无类型强制转换的 JSON 值相等；!= 与 !== 是其取反。数组逐项、对象逐键比较内容，数字 1 与 1.0 相等；对象相等不是业务质量判断。
- if、循环条件、!、&&、||、三元与 assert 使用固定 C 风格真值：false、null、数值 0（含 0.0/-0.0）为假，其余 JSON 值为真。空字符串、空数组和空对象也为真，不冒充完整 JS 真值规则。&&/|| 短路并返回布尔值；?? 只在左值是 null 时执行右侧。缺少对象字段或越界下标返回 null，不提供 undefined。因此 if(items.length)、if(!result.error)、if(obj) 都可直接执行，不请求 AI 或因非布尔类型中断。
- 三元只执行选中分支；函数／工具调用可以出现在表达式中，按从左到右的确定顺序执行。对象字段、数组元素、调用实参及只读方法接收者在后续副作用前保存值快照；push 在实参求值后追加到当前绑定的本地数组。后缀更新返回旧值，前缀返回新值。独立表达式与 for 初始化／更新即使不使用返回值也必须求值并报告错误。
- 模板插值与数组 join 允许 JSON 原始值，null 转为 "null"；数组／对象需要显式 JSON.stringify。字符串 length、索引、slice 与 indexOf 的单位统一为 Unicode 标量，不是 UTF-16 单元。slice 使用整数下标并支持负数；JSON.stringify 使用稳定键序。
- 函数内的绑定不做块级遮蔽；let/const 名字须互不重复。var 是函数级可重复声明的可变值，不做 JS 提升；无初值的首次 let/var 初始化为 null，重复无初值 var 不重置旧值。for...of 使用进入循环时的数组快照。

数据方法只有文首列出的白名单：includes/indexOf 接受一个查找值（字符串接收者要求字符串实参；数组使用 JSON 值相等），slice 接受一个或两个整数下标；startsWith/endsWith/split 接受一个字符串；trim/toLowerCase/toUpperCase 无实参；join 接受零或一个字符串分隔符（默认逗号）；JSON.stringify/parse 恰好一个实参。类型或实参数量不匹配明确报错，不猜测方法含义。

`at` 接受一个整数，支持负索引，越界返回 null；字符串按 Unicode 标量索引。`concat` 接受一个参数：数组追加值或数组元素，字符串只接受字符串。

`typeof` 支持已声明的 JSON 值，null 和数组返回 `"object"`；未声明的名字仍在编译期拒绝。`Array.isArray` 判断 JSON 数组；`Object.keys/values` 接受 JSON 对象，按宿主稳定键序返回数组，不模拟 JS 整数键的特殊枚举顺序。

路径和 URL 由宿主 Rust 直接计算：如 `path.join("docs", "page.html")`、`path.resolve("/project", "docs", "page.html")` 和 `url.parse("../a.html", "https://example.com/docs/")`。`url.parse` 返回 `{href,origin,protocol,hostname,port,pathname,search,hash}`；`path.resolve` 至少要求一个绝对路径，`path.relative` 的两个参数必须均为绝对路径，避免隐式依赖服务进程目录。路径按运行平台的规则进行词法处理，不访问文件系统或解析符号链接；输入最多 64 KiB，输出最多 256 KiB。常见 POSIX 路径和 URL 行为与 Node 对照测试，Windows 复杂路径尚未做真实环境兼容性验证，不承诺完整 Node 行为。调用不启动外部进程或 AI 任务；文件读取与修改仍使用 `io` 工具。

`lastIndexOf` 接受一个字符串，返回 Unicode 标量下标或 -1。`replace` 和 `replaceAll` 接受两个字符串，分别替换首次或全部匹配；替换内容按字面文本处理，不展开 `$&` 或 `$1`，暂不接受正则或函数作为参数，结果最多 4 MiB。

支持正则字面量，例如 `/a(\\d+)/i.test(text)` 和 `text.match(/a(\\d+)/g)`。可用标志为 `g`、`i`、`m`、`s`、`u`；底层采用 Rust regex 的 Unicode 语义及语法，不支持回溯引用、环视或 JS 特有语法，非法模式和标志在编译时拒绝。`test` 无状态，即使有 `g` 也不维护 JS 的 `lastIndex`；`match` 不匹配返回 null，非 `g` 返回完整匹配与捕获组组成的 JSON 数组（未匹配组为 null，不含 JS 的 index/groups 属性），`g` 返回全部完整匹配的数组。正则以内部 JSON 描述保存，不能使用 `new RegExp` 或其他正则方法。模式最多 8192 字节，输入最多 2 MiB，结果最多 10000 项及 4 MiB 捕获文本；超出时报错。正则不替代结构化 HTML 解析。

## 工具声明与固定名字

模型请求发布 TS 风格的 program_tool_headers 索引，只列当前允许的库和方法，不是参数契约。使用 read_tool_contracts({methods:["io.read"]}) 或 read_tool_contracts({libraries:["io"]}) 按需取得真实参数、约束、返回类型和完整说明。虚函数可在同一上下文中调用 read_tool_contracts 和 invoke_library，宿主校验原始 schema 与权限；无需先编写流程。程序源码使用 import 引入工具，声明资料不是可执行源码。

内置文件库为 io.read/read_lines/write/list/search/edit/replace。其他宿主操作在 host 库中按原方法名声明。插件在 plugin.json 或远程握手的 plugin 中声明 library，例如 {"id":"ppt-master","library":"ppt"}；已登记的 ppt_master__get_design 对外只发布 ppt.get_design。未声明 library 的旧插件按 ID 将连字符替换为下划线，作为固定兼容名字。库名必须是有效、非保留的标识符，不能占用 io/host；不同插件不能占用同一个库名。方法来自实际注册工具，不按业务含义猜测。

头文件生成器和编译器使用同一份当前作用域目录：所有已启用且当前权限、插件归属、执行限制允许的操作均发布，不要求预先加载工具组。未启用／未授权的工具不发布也不能编译。主程序和当前函数补写都只使用宿主目录，模型不提供或继承别名表。宿主编译结果保留固定绑定；helper 和 fork 使用原编译结果，恢复不重新猜测工具目标。实际执行继续经过原工具分发、权限、插件归属和回执检查。

参数从真实 JSON schema 自动生成：整数、范围、格式、额外字段限制等通过注释保留，枚举和联合类型转成声明。返回类型来自 outputSchema（命令工具可用 output_schema）；没有返回 schema 时是 unknown，不虚构返回字段。递归／外部引用无法内联时保留引用注释并标 unknown，声明不代替原工具运行时契约。

规划请求只发送库声明，不重复发送这些操作的原始 JSON schema。注释任务需要直接执行工具时，发送原生 invoke_library 工具，其 method 是同一固定名字（例如 io.read），args 是原参数对象，分支 schema 来自原工具。宿主内部原工具名不作为第二个公开名字；历史结构化调用在模型请求中投影为固定方法名，原始审计记录保留。invoke_library 仅是兼容原生函数名字符限制的传输包装，不可写入流程源码，不授予新权限，也不供规划请求直接执行业务操作。

```js
function main(request) {
  const actual = io.read({path: request.source});
  if (!actual.error && actual.content.length) {
    return {content: actual.content, present: true};
  }
  return {present: false};
}
```

库不是可变 JS 对象：不允许把方法当函数值传递。整份源码先编译再执行，未知方法在任何效果发生前拒绝。源码参数仍是原 JSON 对象，不把位置参数自动转换成字段。

## 任务函数与工具步骤

业务 skill 和用户决定范围、依赖、确认点、验收与交付；通用 task-program skill 负责表达这些关系。无主任务时先 create_task，准备缺失的技能／工具契约，再 execute_program({source,input})。顶层流程覆盖完整工作、检查与交付。

普通 helper 在宿主的局部函数帧中执行，不创建 AI 任务节点；工具调用是实际操作步骤。显式 @virtual helper 创建独立 AI 任务上下文，把职责注释与命名实参交给执行 AI。虚函数标记放在紧邻函数的前置 JSDoc 或函数体注释中，体内可为空，或仅写 throw new Error("固定说明") 作为占位；不允许占位 return 或其他可执行语句；未标记的空函数拒绝编译。返回结构写入注释契约，实际返回来自执行 AI。普通代码中的注释没有执行效果。后者可以直接调用真实工具，也可 define_task_body 去掉当前函数的 @virtual 标记并补写实际实现，继续细分任务；不重新创建 main。注释应说明职责、输入与返回字段。所有调用一律按位置绑定实参，补写函数保持参数个数；不按名字猜测，也不把多参自动打包成单对象。bindings 中的值就是局部参数值，没有额外 input 包装；scope_variables 是其余可见上层变量的索引。

```js
function main(request) {
  let artifact = produce(request);
  let check = inspect(artifact, request);
  if (!check.passed) {
    artifact = repair(artifact, check, request);
    check = inspect(artifact, request);
  }
  assert(check.passed, "修复后验收仍未通过，停止交付");
  return deliver(artifact, request);
}
/** @virtual */
function produce(request) {
  // 按 request 制作实际产物，返回路径和版本。
}
/** @virtual */
function inspect(artifact, request) {
  // 读取或检查实际产物，对照 request 判断。
  // 返回 {passed:boolean, evidence:[实际工具观察], issues:[...] }。
}
/** @virtual */
function repair(artifact, check, request) {
  // 使用 check 的证据完成授权范围内的修正，返回实际产物。
}
/** @virtual */
function deliver(artifact, request) {
  // 完成 request 要求的检查与交付，返回真实结果和限制。
}
```

这个修复示例最多尝试一次修复。修复后必须再次 inspect；复检仍未通过就由 assert 停止，deliver 不会执行。

inspect 的判断由 AI 作出；!check.passed 由宿主消费布尔值。不再把 artifact !== expected 解释为“不满足预期”；它只比较 JSON 数据。证据保存在普通任务返回及原始工具回执中，没有运算符专用的 value/evidence 隐式协议。宿主检查调用与返回结构，不认证业务判断的真实性。

## 已知循环与授权并发

```js
function main(request) {
  const plan = prepare(request);
  const a = fork(work, plan.shared, plan.left);
  const b = fork(work, plan.shared, plan.right);
  const outputs = join(a, b);
  integrate(a);
  integrate(b);
  const checks = [];
  for (var i = 0; i < outputs.length; i++) {
    const check = inspect(plan.shared, outputs[i]);
    checks.push(check);
    assert(check.passed, `检查未通过：${i}`);
  }
  return {outputs, checks};
}
/** @virtual */
function prepare(request) {
  // 仅在已获并发授权时划分不同职责；返回 shared、left、right。
}
/** @virtual */
function work(shared, assignment) {
  // 使用完整共享要求及真实资产引用，只完成自己的 assignment，返回实际结果。
}
/** @virtual */
function inspect(shared, output) {
  // 检查这份实际 output，返回 {passed:boolean,evidence:[...]}。
}
function integrate(handle) {
  assert(handle.workspace, "缺少分支工作区信息");
  if (handle.workspace.integration_required) {
    const changes = host.inspect_agent_changes({agent_id: handle.agent_id});
    return host.merge_agent_result({agent_id: handle.agent_id, expected_head: changes.head});
  }
  return null;
}
```

共同材料、真实稳定资产引用和各分支职责显式传参；不以“沿用前文”替代数据。任务函数按 JSON 值交接，局部对话不复制给父／兄弟。主任务验收真实产物。工具受理、函数返回、语法编译通过都不等于业务通过；按技能处理异步结果和必要确认。

顶层执行完毕结束整项任务，helper 返回恢复原调用方；finish 只交接并推进，不额外请求模型确认完成。工具只能使用公布的确切名称及参数；工具返回原始 JSON，例如 result.content，没有 original_result 包装。工具仍经过原权限、插件隔离和取消机制，不能递归调用 execute_program/create_task/spawn_agent 等替代函数关系。

## 规划与执行的职责边界

规划阶段将用户目标与业务 skill 编排成顶层流程和辅助函数；进入虚函数后，当前 AI 负责实现该函数的职责，可以直接理解、生成、转换、验收或使用工具并返回实际结果。当前调用已经满足“智能工作显式交给虚函数”的表达要求，执行者不必再次包装同一职责。

define_task_body 用于需要具体编排时补写实现，并非执行前置条件。进一步拆分应产生职责明确、实际推进处理的子步骤；仅换函数名、传递相同输入并转交同一未完成工作，属于重复规划。保留合理的嵌套函数和动态子任务。

该边界由规划与执行指令表达。宿主继续负责调用、返回和记录，不根据函数注释猜测业务进展，不新增完成确认、业务状态或语义判重／嵌套深度限制。

## 持久化与兼容边界

源码整体编译通过后才开始执行。execute_program、define_task_body 的源码验证失败返回 PROGRAM_SOURCE_INVALID，包含行列、附近源码及修正说明，保留当前任务、输入和已有结果，由原执行 AI 重写完整源码后在原位置重新提交；被拒源码没有实际操作，不自动修补或重放操作。check_program 是可选预检，复用相同编译器、权限目录和参数绑定，在临时副本校验，不安装或执行程序，也不是强制的额外前置调用。运行时类型／算术错误仍沿 try/catch 传播，未捕获则停止。任务返回 JSON，无 Markdown 围栏；无法完成可返回 {"$task_error":{"message":"具体原因"}}，沿调用链上抛。

agent_runtime_windows.state.program 保存源码、指令、函数帧、PC、变量、catch 栈、指令预算和挂起 effect ID。每次效果先持久化位置，已有回执恢复时直接消费，不重复计算已完成的更新或重放操作。缺少外部回执时结果未知，不能承诺跨进程恰好一次。

版本 11 及更早的现场、源码和回执保留，但拒绝按版本 12 续跑；不把旧 AI 表达式回执解释成新规则。旧的隐式 AI 表达式枚举与执行／恢复分支已经删除；旧记录作为历史保存，不再提供旧语言执行器。源码上限 100,000 字节，最多 64 个函数、调用深度 64，每运行帧 100,000 条指令；预算耗尽强制失败，不能被 catch 隐藏为成功。

## 统一的调用边界（版本12）

内部函数调用保存 parameters 与 arguments 有序数组；普通 helper、虚函数、fork、补写及预检使用同一契约。program_task_input 只投影 signature、description、bindings，避免以 input 再包一层同名参数。补写参数可改名但数量必须一致，按位置接收原实参；改变为一个“整体输入对象”的隐式重载已删除。

源码调用固定库方法；执行 AI 的原生步骤用 invoke_library 传输相同方法名和 JSON 参数。两者都得到原工具 JSON，不混入资产交付 envelope 或 event_seq 字段；原始历史及资产元数据仍保存。host.read_asset 返回的快照 envelope 是其自身契约，不被当作传输包装剥除。规划请求不发布旧原生 spawn_agent/join_agents 和工具组加载入口，源代码使用 fork/join；底层调度只接收运行时挂起指令，不再接受模型自由派发自然语言任务或任务数组。

声明保留完整工具说明，不再只截取首句；规划端与执行端都能看到后续条件、确认要求和注意事项。插件原文中的历史工具标识只作说明，唯一可调用名字仍以固定声明为准，底层 dispatcher 与外部 MCP/服务接口不因此改名。

资产索引 task_asset_index 的每个条目通过 delivery_reference 指向最近一次真实交付（task_id、event_seq）。需要验收证据号时使用这一引用，业务结果不追加宿主字段；快照的原始记录和再次交付仍可分别追溯。

## 函数作用域与工作空间预算

编译调用的当前参数在 program_task_input.bindings 中；scope_variables 只列其余可见上层变量的名称、类型与大小。host.read_scope({name,path}) 按名称／字段路径只读访问完整的进入时快照，不重放文件或插件操作。需要的参数和固定输入超过容量时明确报告。工具权限与技能能力仍保持。

运行时在挂起效果的持久化边界保留仍处于函数作用域的源码变量，即使后续确定性代码没有引用；编译器临时量可按可达性回收。当前挂起效果持有自己的实际参数，恢复回执仍写回原目的变量；函数返回释放局部上下文，父／兄弟作用域不接收其工作全文。

步骤折叠使用 fixed + 80% × (input_budget − fixed) 作为触发点，fixed + 60% × (input_budget − fixed) 作为回落目标；fixed 按相同模型请求投影和 token 校准估算固定指令、当前参数等不可折叠开销。最新交换与显式输入保留，目标尽力达到；硬输入上限和超容量分页提示仍有效，不能保证任意大小的材料全部常驻。没有改变主任务定义、确认、权限或业务验收规则。

## 示例的可执行回归

当前手册的三段 JS 和 task-program Skill 的四段 JS 共七段，测试直接提取原文；两个完整示例还校对 Skill 内嵌正文与 examples/*.js 完全一致。当前函数补写片段通过真实 define_task_body 入口执行，其余程序直接提交原 main，不由测试重写流程。

| 示例 | 实际验证 |
| --- | --- |
| 固定工具名读文件 | 非空正文与空正文两条返回分支 |
| 制作、检查、修复与交付（手册／Skill 两份） | 初检通过；修复后通过；复检仍失败时停止且不调用 deliver |
| 公共材料、两路工作与循环验收 | 两个真实宿主分支、两份文件、汇合与逐份检查；验收拒绝停止 |
| 最小串行 HTML | 实际写入、读回、检查；拒绝时不交付；没有并发分支 |
| 两 Agent 五页 HTML | 源材料读取、3+2 分工、五个独立文件和五项读回检查；拒绝时停止 |
| process 当前函数补写 | 在原作用域安装文档中的函数体，读取源文件、调用 transform、保存新文件 |

上述15个受控场景使用真实 Rust 执行器、真实文件工具和宿主分支，模型返回由测试夹具提供，验证调度与参数契约。真实模型测试另外原样提交 serial.js 和 parallel-five-pages.js 及配套 JSON，只让实际配置的模型执行虚函数，核对真实文件、分支和检查结果；不向执行中的 Agent 补提示，不人工修产物。

```bash
# 七段示例、异常路径和补写；不需要真实模型凭证
cargo test single_agent::documented_examples:: -- --nocapture

# 实际配置模型的两个完整示例；消耗模型额度，使用仓库外隔离目录
cargo test single_agent::documented_examples::documented_file_examples_with_configured_model -- --exact --ignored --nocapture
```

实现与断言见 [documented_examples.rs](../src/single_agent/documented_examples.rs)。新增或修改 JS 示例会随这些测试重新编译、执行；检查返回 false、编译通过和工具写入成功不能互相替代。真实生成仍需按实际产物验收，不把受控夹具通过当作模型质量保证。

2026-10-27 实测：上述15个受控场景通过；Terra 原样串行示例约22.68秒、2次模型请求，原样两Agent五页示例约105.90秒、10次模型请求，全部文件和检查结果齐全。六个HTML在桌面与手机共12次浏览器检查通过。曾发现通用修复示例漏掉复检，已同步修正手册和Skill，并用“修复后仍失败”回归阻止误交付。完整回归291通过、0失败、4忽略。详见[本次示例执行证据](../target/documented-examples-real/documented-examples-real-02d22bf9738156fba25a493138223bf8/run-report.md)。这是原样提交示例程序的测试，不替代自然语言规划器生成流程或真实PPT导出的测试。

## Git 分支产物的整合

fork 返回控制句柄 {agent_id,workspace}，其中 workspace.mode 为 shared/git_worktree，包含实际 path 和 integration_required。Git 可写分支隔离在独立工作树，普通非Git分支共享目录。句柄反映实际宿主分配，重启后仍来自同一条持久化记录；不向业务返回值加包装。

join 仅返回按句柄顺序排列的函数结果数组，不改变文件位置。即使 join(child) 也用 results[0] 取该函数返回。integration_required=true 时，先用句柄 agent_id 调用 host.inspect_agent_changes，检查实际差异，再用其 head 调用 host.merge_agent_result；成功后才能在主工作区读回这些文件。shared 模式跳过合并。分支返回路径以分支工作区为基准，不能未合并就用同一相对路径读主目录。冲突、未知结果及失败不自动复制或重放。普通函数调用保持同一工作区，仍无需整合。

Git 派发要求从仓库根目录和干净的已提交基线启动。先 fork 所需分支，再执行主Agent自己的新写入，避免下一次 fork 被脏基线拒绝；共享输入可按实参交接。示例中的 integrate 是普通流程函数，调用已存在的检查／合并工具，不是新的宿主状态机或自动合并机制。

Git整合更新后的受控验证共18个场景：原15个场景加上两个真实Git并发示例及一个省略整合的失败反例。共享目录与Git目录使用同一份示例源码，区别来自真实句柄。完整回归294通过、0失败、5忽略；真实Luna十页的Git交接已完成，但全流程因300秒上限取消，另发现三页导航断链，不能将其记作完整交付通过。详见[当前Git实测边界](generic-agent-design.md#24-git-并发产物的显式交接2026-10-27)。
