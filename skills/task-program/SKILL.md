---
name: task-program
description: 将用户目标与业务 skill 表达为可执行的流程伪代码：唯一 main、显式 AI 任务、工具调用、确定性表达式和基础控制流。
---

# 用程序表达任务

语言版本 12。用普通 JS 基础语法表达完整流程；Rust 宿主执行普通计算，AI 只处理显式任务函数。业务要求、确认点、验收标准来自用户和业务 skill，不写进通用宿主。

## 从完整示例学习

这是一门面向 Agent 工作流的领域语言，借用 JS 的表面语法；执行规则以本 Skill 为准。下面两份完整程序随本 Skill 自动提供，不需要每个子任务重读手册。完整说明见 [语言手册](../../docs/agent-program.md)，包括提交方式、输入绑定、逐阶段数据流、当前函数补写、失败处理和语法差异。示例只是教学，不把 HTML、五页或两个 Agent 规定为其他业务的流程。

### 最小串行程序

源码：[serial.js](examples/serial.js)；输入：[serial.input.json](examples/serial.input.json)。execute_program 的 source 取源码全文，input 取 JSON 对象。普通函数调用顺序执行，不启动另一个 Agent。compose 返回正文，工具保存并读回，inspect 对实际正文检查，断言通过才交付。

```js
// 最小串行流程：设计 → 保存 → 读回 → 验收 → 交付。不启动子 Agent。
function main(request) {
  const draft = compose(request);
  const receipt = io.write({path: request.output, content: draft.content});
  const actual = io.read({path: request.output});
  const review = inspect(request, actual.content);
  assert(review.passed, "验收未通过，不能交付");
  return {path: request.output, receipt, review};
}

/** @virtual */
function compose(request) {
  // 按 request.topic 和 request.requirements 设计一个完整、独立的中文 HTML。
  // 当前执行 AI 直接完成设计，返回 {content:string}，不只返回计划。
}

/** @virtual */
function inspect(request, content) {
  // content 是已保存文件的真实读回正文；对照 request 检查内容与 HTML 结构。
  // 返回 {passed:boolean, issues:[string], evidence:[string]}。
  // 只做实际能完成的检查，未经浏览器渲染不能声称视觉验收通过。
}
```

### 授权两个 Agent 并发制作五页

源码：[parallel-five-pages.js](examples/parallel-five-pages.js)；输入：[parallel-five-pages.input.json](examples/parallel-five-pages.input.json)。request 含 source 路径、requirements 和五项 pages；每项有 path/title/focus。源文档真实读取一次后向两路传 shared，pages 切成3+2，先启动两路再 join。主 Agent 收齐结果后，按句柄的工作区信息检查并合并 Git 分支，再逐页读回、验收。makePages 中的循环在每个分支内串行执行，不派生更多 Agent。

```js
// 教学案例：用户已授权两个执行 Agent 并行制作五个独立 HTML。
function main(request) {
  assert(request.pages.length === 5, "此教学示例接收五页清单");
  const source = io.read({path: request.source});
  const shared = {
    source_path: request.source,
    source_content: source.content,
    requirements: request.requirements
  };

  // 先启动两路再等待；传相同公共材料与不同的独占页面清单。
  const a = fork(makePages, shared, request.pages.slice(0, 3));
  const b = fork(makePages, shared, request.pages.slice(3));
  const outcomes = join(a, b);

  // Git 分支的文件在独立工作树中；合并后才能从主工作区读回。
  integrate(a);
  integrate(b);

  // join 只收齐结果。主 Agent 仍要逐页读回并验收。
  const files = [];
  const reviews = [];
  for (const outcome of outcomes) {
    for (const file of outcome.files) {
      const actual = io.read({path: file.path});
      const review = inspectPage(shared, file, actual.content);
      assert(review.passed, "验收失败：" + file.path);
      files.push(file);
      reviews.push(review);
    }
  }
  assert(files.length === request.pages.length, "交付数量不符");
  return {files, reviews};
}

function makePages(shared, pages) {
  // 一个分支是一个 Agent；其中逐页串行执行，不再 fork。
  const files = [];
  for (const page of pages) {
    const draft = designPage(shared, page);
    const receipt = io.write({path: page.path, content: draft.content});
    files.push({path: page.path, title: page.title, focus: page.focus, receipt});
  }
  return {files};
}

/** @virtual */
function designPage(shared, page) {
  // shared 含真实源材料与公共设计要求，page 含本页标题、重点和输出路径。
  // 直接完成本页设计：内容忠实于源材料，主题不同于其他页，HTML/CSS 自包含。
  // 返回 {content:string}；不重新查找已传入的材料，不把相同职责改名转交。
}

/** @virtual */
function inspectPage(shared, page, content) {
  // 对照 shared.source_content、requirements 及 page 的标题/重点检查真实读回正文。
  // 检查主题覆盖、事实、完整 HTML、外部依赖和明显缺项。
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
  // 不把成功写入或数量正确视为内容验收，不虚构浏览器视觉检查。
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

此案例将失败上抛，未演示自动修复；真实业务需要的确认、修复或视觉渲染检查仍由业务 Skill 定义。不能以这些例子中的内容检查代替浏览器视觉检查或 PPT 导出。接到非 HTML 任务时按实际工具契约与产物要求改写函数职责。

## 已跑通的并发参考与换场景

[真实 20 页 HTML 流程](examples/parallel-twenty-html.js) 是主 Agent 与一个子 Agent 的完整案例：先读资料和由 AI 设计样式，再 fork 具体写页函数并由主 Agent 同时制作另一半；join 只收集结果，Git 模式需检查差异并合并，之后读回验收。2026-10-27 此具体流程真实完成 20 页、基础结构与 AI 内容检查；未进行浏览器视觉验收。示例正文仅作为按需查阅的案例，不要求每次加载或复制完整源码。

迁移的是分工、独立输出、具体函数、受限虚函数、join 与实际结果整合的思路，不是示例的业务内容。遇到其他任务，先依据用户要求与适用业务技能确定输入、生成工具及格式、实际验收和所需检查，再编排具体函数；不假设共享样式、页数、文件路径、Git 差异形式或本例的检查适用于新任务。只在授权并发时 fork，不把样例变为统一工作流。

## 固定写法

- 一个 main 覆盖用户要求的工作、验证和交付；顶层普通 function 定义辅助任务，helper(args) 调用。main 不能被调用或 fork。
- 数据为 JSON；支持 const/let/var、局部赋值及赋值表达式、++/--、+= -= *= /= %=、对象简写、简单数组解构、成员／下标读取、模板字符串。
- 控制流支持 if/else、传统 for、while、do/while、数组 for...of、无标签 break/continue、三元、assert、return、try/catch/throw。已知循环写进代码，不交给一个宽泛注释任务反复调度。
- + - * / %、比较、逻辑 && || ?? !、模板转换由宿主确定性计算，普通表达式绝不请求 AI。条件采用 C 风格真值：false、null 和数值 0 为假，其余 JSON 值为真（空字符串、数组、对象也为真）。if(items.length)、if(!result.error)、if(obj) 都合法。除字符串拼接外，算术和比较不做字符串／数字强制转换。短路和三元只执行选中的分支，副作用按从左到右的顺序执行一次。
- 数字运算类型明确：整数加减乘余数精确保留 JSON 64 位整数，溢出报错；除法为有限小数除法，除零报错。超过 2^53-1 的整数不能隐式转换成小数。+ / += 任一侧为字符串时，将另一侧 JSON 原始值（数字、布尔、null）转为文本后拼接；两个数字仍相加，数组／对象显式用 JSON.stringify。==/=== 都比较 JSON 值内容，不表示业务质量。
- 只使用 program_tool_headers 声明的固定 library.method(args)，如 io.read({path})、io.write({path,content})。参数、可选字段和返回信息直接看声明；不写 tools.*，不提供 libraries 别名。获授权的多 Agent 用 fork(helper,args)、join(handles)；子 Agent 不再派生 Agent。
- 本地数组 push 收集结果；只支持确定性方法 includes/indexOf/slice、字符串 startsWith/endsWith/trim/toLowerCase/toUpperCase/split、数组 join、JSON.stringify/parse。未知方法编译失败，不能假定会让 AI 解释。
- 入口函数（main 或当前补写函数）可声明 async，使用 await Promise.all([只读调用...])，或 await Promise.all(paths.map(path => io.read({path}))) 并发读取、列目录或搜索；最多 32 项，结果按输入顺序返回。仅允许 io.read、io.read_lines、io.list、io.search；AI 判断、写入和修复仍串行，不由此启动子 Agent。失败经既有异常路径处理，不自动重放。示例见 examples/async-page-review.js；串行基线见 examples/serial-page-review.js。
- 以上是受限异步语法，不是完整 Promise 或 JS 环境。async 只允许入口函数；await 只允许上述 Promise.all；map 箭头仅允许一个简单参数和一个只读调用，参数不能遮蔽已有局部变量。禁止一般 async helper、Promise 链、function*/yield、call()/ai() 包装、模块导入导出、TS 类型、闭包、其他箭头／匿名／嵌套函数、class/new/this、for-in、switch、finally、标签跳转、展开／剩余／默认参数、可选链、正则／BigInt、位运算、typeof/void/delete、对象成员写入和 JS 全局环境。

let/const 在函数内不做块级遮蔽，使用不同名字；var 可以重复声明，不做提升，首次无初值为 null。数据按 JSON 快照交接，不共享可变别名。字符串 length、索引、slice 和 indexOf 使用 Unicode 标量。模板只插入原始值，对象和数组使用 JSON.stringify。完整规范在 docs/agent-program.md。

## 工具库

program_tool_headers 是根据真实工具契约生成的 TS 声明，供阅读，不复制类型到源码。每个方法只有一个固定名字。io 是本地文件库，host 是其他宿主操作，插件在清单里声明自己的 library 名。声明中包含 JSON 对象参数、可选字段、整数／范围等约束及完整工具说明；返回类型无 schema 时为 unknown，不猜测其字段。

可用的方法全部发布，编译器使用同一份权限与插件归属过滤目录，使用本轮公布的目录，无工具组加载状态。只允许静态 namespace.method(args)，不支持动态方法名或模型起别名。方法最终经过原有工具分发和权限检查。

规划请求只发这些声明和编排工具，不重复发业务工具的 JSON schema。在注释任务中直接执行操作时，用原生 invoke_library({method:"io.read",args:{path:"..."}})，它的分支参数 schema 来自真实工具；同一个方法在源码中仍写 io.read({path:"..."})。invoke_library 只是传输工具，不是源码函数。需要补写当前任务时使用 define_task_body，不创建新 main。

## AI 工作必须显式调用

需要理解、生成、转换或验收时，显式声明 `@virtual` 虚函数，注释说明职责、输入和返回字段；调用它就是交给执行 AI 的任务。标记放在函数紧邻的前置 JSDoc 或函数体注释里。虚函数体只放注释，不写可执行语句，也不写占位 return；返回结构放在注释契约里，由执行 AI 完成实际操作后返回。普通函数中的注释只是说明，不会自动执行；普通代码里直接 return 完成数据只会返回该数据，不会替你制作或检查产物。空函数没有 @virtual 会在编译时拒绝。不要用 artifact !== expected 代替质量验收，也不要把未绑定名字当作描述性业务条件。使用实际证据让函数返回 passed，再在 if/assert 中消费它。

```js
function main(request) {
  let output = produce(request);
  let check = inspect(output, request);
  if (!check.passed) {
    output = repair(output, check, request);
    check = inspect(output, request);
  }
  assert(check.passed, "修复后验收仍未通过，停止交付");
  return deliver(output, request);
}
/** @virtual */
function produce(request) {
  // 按 request 制作实际产物，返回路径、版本和实际结果。
}
/** @virtual */
function inspect(output, request) {
  // 读取真实产物，对照 request 判断，返回 {passed:boolean,evidence:[...],issues:[...]}。
}
/** @virtual */
function repair(output, check, request) {
  // 依据 check 修正授权范围内的问题，返回实际产物。
}
/** @virtual */
function deliver(output, request) {
  // 完成所需检查和交付，如实返回结果和限制。
}
```

修复后必须再次 inspect；复检仍失败由 assert 停止，不能继续 deliver。此例只尝试一次修复，不循环重试。

规划具体任务时，应为验收发现的可处理问题留出处理分支：把失败证据、当前对象与必要要求交给一个职责明确的 AI 函数，由它判断能否在授权范围内修复并处理；调用方再重新读回实际结果并检查。无法处理或复检仍失败时如实停止。处理的是当前问题，不重启整条流程或重放已完成的操作。这个分支适用于实际任务允许修复的情况，不把任何特定业务的检查、格式或修法写进通用宿主。

已通过工具取得的实际值，如果是虚函数所需输入，应直接按参数传入，不因进入函数就重复读取；需要核实外部状态或修改后的结果时，再根据实际任务读回。职责约定可以是只检查、检查并处理、或只处理，不能默认每项检查都允许修改。同一虚函数可依次使用工具、观察回执、分析、处理和复检，不必把每个步骤拆成 AI 任务或先写子程序；复杂时才按需 define_task_body。

函数是否产生外部副作用以其职责和调用方分工为准：只负责生成并返回内容时，不应自行执行由调用方负责的保存操作；承担实际修改职责时才按授权和工具契约写入。工具可用不等于当前函数需要调用该工具，避免同一操作被执行两次。

函数契约若约定了返回值与输入之间可由代码确定的关系，调用方在使用返回值之前应直接核对该关系；不匹配不能当成当前对象的有效证据，应安排核实或返回明确失败。具体字段和关系由该任务的契约决定，通用运行时不要求统一字段，也不把文字检查误称为实际资源验证。

虚函数任务在当前 Agent 的局部作用域执行，可直接使用已公布工具；也可 define_task_body 补写当前函数及新 helper，继续分解工作；当前函数提供真实实现时去掉 @virtual，其内部尚需 AI 工作的新函数保留 @virtual，不新建第二个 main、不重复创建当前任务。形参与实参一律按位置绑定，普通调用、fork、补写函数都相同；例如 work(shared,part) 的并发调用是 fork(work,shared,part)，每个形参分别接收一个实参，不隐式打包对象；补写保持原参数个数，不会自动把多个参数打包成一个对象。program_task_input 给出函数签名和 bindings（各形参的实际局部值），bindings 不是参数内部的一层数据。若签名是 make(input)，bindings.input 为 {pages,...}，函数内直接用 input.pages；不能再套 input.input。有代码的普通 helper 在 Runtime 函数帧内执行，不创建 AI 任务节点；虚函数调用创建 AI 任务，其工具调用是步骤。main 返回结束，helper 返回恢复调用方。

当前调用为 process(shared, part) 时，可补写：

```js
function process(shared, part) {
  const source = io.read({path: part.source});
  const changed = transform(shared, part, source.content);
  const saved = io.write({path: part.output, content: changed.content});
  return {path: part.output, receipt: saved};
}
/** @virtual */
function transform(shared, part, content) {
  // 根据 shared、part 处理 content，返回 {content:string}。
}
```

## 规划与执行的职责

规划时，把用户目标和业务 skill 表达为一个 main 及有明确职责的辅助函数。进入 program_task_input 指定的虚函数后，当前 AI 就是该函数的执行者：直接完成所需的理解、生成、转换或验收，使用工具并返回约定的实际结果。“智能工作通过虚函数表达”的要求已经由当前调用满足，不要求执行者再声明一个虚函数来承接同一工作。

define_task_body 是按需补写具体实现的方式，不是执行前的必经步骤。可以把当前职责拆成实际子步骤，分别明确处理内容、输入与输出；不能只更换函数名，把相同输入和同一项尚未完成的职责继续转交另一虚函数。拆分应推进处理，而不是重复安排处理。

## 作用域与生命周期

当前函数的真实参数由 program_task_input.bindings 自动提供。普通函数代码引用调用位置可见的上层变量时，宿主取得该变量的进入时快照；当前参数遮蔽同名上层值，不能通过子函数赋值改写父变量。子 Agent 使用 fork 时的快照，之后父作用域变化不自动同步。未引用的上层变量不自动发送正文，scope_variables 提供名称、类型与大小，AI 可用 host.read_scope({name,path}) 按需读取；虚函数只有自然语言注释，不会从注释猜测代码变量引用。源码变量保留到函数返回，编译器临时量可回收。历史工具调用、回执不是自动重放的变量。

容量折叠先扣除固定指令和当前函数输入的开销，再按剩余工作空间的80%触发、向60%回落。自然增长的旧步骤可以折叠为可恢复引用；当前输入及最新交换保留。完整活跃输入超出硬上限时需分页或缩小单次处理范围，不把引用冒充已读取正文。

## 数据流与交付

共同依赖与各自职责分开传参。共享规则传完整对象，资产用真实稳定引用、路径、版本及必要正文；不以“沿用公共规范”代替数据。嵌套 helper 继续传递所需依赖，分支收到各自独占清单，不能只换标签却传相同完整工作。

正文、HTML 等长数据由任务函数返回或保存为资产，编排源码不展开巨型字符串。只读取缺失材料或确需新版的内容；已有输入、技能、工具和回执直接复用。

返回 JSON 要能被下一调用使用；检查任务保留真实工具证据。工具成功、accepted、函数返回或编译通过均不等于业务验收。按业务技能完成修复、异步结果收集和确认点，再交付。工具参数遵守真实契约，返回是原始 JSON，例如 io.read 的 result.content，不带 original_result 包装。

执行前检查完整 main、参数来源、分支所有权和返回契约。编译错误按行列修正；当前虚函数补写体未捕获错误时，在同一函数查看错误、回执与保存的变量，按真实状态继续处理，必要时补写仅包含剩余工作的函数体。新函数体从头执行，不能自动重放已完成或结果未知的外部操作。普通主流程未捕获错误仍会停止。任务返回一个 JSON 值，无 Markdown 围栏；无效 JSON 会在当前函数收到诊断并修正，文本用 JSON 字符串。无法完成返回 {"$task_error":{"message":"具体原因"}}。原权限和插件边界始终有效。

## 源码检查和错误修正

execute_program 和 define_task_body 都在任何实际操作前编译整份源码；失败会带行列、附近源码和错误原因返回当前 AI，当前任务及原有结果保留。根据诊断重写完整源码并在原作用域重新提交，不重建任务、不重放已完成操作、不把编译失败当作完成。需要先检查时可用 check_program；它复用同一编译与作用域规则，不安装、不执行、不调用业务工具，并非每次都必须增加的前置步骤。反复无法修正时如实报告阻碍。此修正机制只处理源码，不自动重试未知写入或改写业务目标。

库方法在源码和 invoke_library 中都返回工具原始 JSON，直接按声明读取字段；资产交付的 original_result 包装不属于库返回契约。host.read_asset 的快照结构是该方法自身的数据契约。编排使用 fork/join，不调用原生 spawn_agent/join_agents；不维护工具组加载状态。

工具返回值保持原 schema；资产编号和工具结果回执从 task_asset_index 读取。需要证据事件号时使用对应条目的 delivery_reference.event_seq，不从业务返回值猜 event_seq。

## 并发的工作区与整合

fork 返回 {agent_id, workspace} 控制句柄。workspace.mode 为 shared 或 git_worktree；workspace.path 是分支文件实际所在的目录。Git 可写项目中的分支有独立 worktree，workspace.integration_required=true；普通非Git目录共享文件，值为false。这个标记表达隔离方式，不是可轮询的合并进度。重启恢复同一个句柄，业务函数返回不添加这些字段。

join(a,b) 始终只等待并返回 [a的函数值,b的函数值]，即使只有一路也返回数组；它不复制文件、不合并、不验收。返回路径相对于执行该函数的工作区，不保证相对于主目录。主Agent使用仍保留的句柄，在 join 后先调用 host.inspect_agent_changes({agent_id})，检查差异符合分工，再以返回的 head 调用 host.merge_agent_result({agent_id,expected_head})；合并成功后才读主目录中的分支产物并验收。冲突或合并失败应停止或按实际错误处理，不能用手工复制伪装整合成功。普通共享目录跳过Git操作。

先派发所需分支，再在主Agent写入文件：Git分支从当前已提交版本创建，脏工作树会被拒绝。需要供子Agent读取的新文件须在分支前准备并明确传递；共享材料也可直接作为实参传入。公共样式等互不重叠的产物可以由主Agent制作，但分支中的检查不能把尚未合并的主目录文件当作已存在。
