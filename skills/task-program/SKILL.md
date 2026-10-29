---
name: task-program
description: 将用户目标与业务 skill 表达为可执行的流程伪代码：顶层执行、显式 AI 任务、工具调用、确定性表达式和基础控制流。
---

# 用程序表达任务

语言版本 14。用普通 JS 基础语法表达完整流程；Rust 宿主执行普通计算，AI 只处理显式任务函数。业务要求、确认点、验收标准来自用户和业务 skill，不写进通用宿主。

## 从完整示例学习

流程就是普通的 JavaScript：宿主实现其中的一个子集，工具是从 `host:<library>` 导入的库方法，AI 工作是 `@virtual` 函数。下面两份完整程序供查阅；宿主不会在每次规划时自动注入本 Skill 或示例。完整说明见 [语言手册](../../docs/agent-program.md)，包括运行方式、逐阶段数据流、当前函数补写、失败处理和尚未支持的 JS 功能。示例只是教学，不把 HTML、五页或两个 Agent 规定为其他业务的流程。

### 最小串行程序

源码：[serial.js](examples/serial.js)。把程序放在回复的一个 ```js 代码块里，宿主收到后直接运行；用户请求中的数据直接写进代码。普通函数调用顺序执行。compose 返回正文，工具保存并读回，inspect 对实际正文检查，断言通过才交付。

```js
import * as io from "host:io";
// 最小串行流程：设计 → 保存 → 读回 → 验收 → 交付。
const request = {
  topic: "通用 Agent 的职责边界",
  output: "agent-overview.html",
  requirements: "制作一个独立中文 HTML，说明用户、AI、通用宿主、插件和工具的职责。内嵌 CSS，可在桌面与手机阅读；准确区分流程结束与业务验收。"
};

const draft = compose(request);
const receipt = io.write({path: request.output, content: draft.content});
const actual = io.read({path: request.output});
const review = inspect(request, actual.content);
assert(review.passed, "验收未通过，不能交付");
// 读写和验收完成后，流程自然结束。

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

### 并发制作并验收五页

源码：[parallel-five-pages.js](examples/parallel-five-pages.js)。读取真实源材料后，每页通过 async 虚函数并发设计；父流程统一写入，再对实际读回的内容并发验收。失败进入 `await` 的异常路径。

```js
// 五页并发设计、统一写入，再并发检查实际读回的内容；未通过的页修正一轮后复检。
import * as io from "host:io";
const request = {
  source: "docs/agent-design-guide.md",
  requirements: "制作五个内容不同、风格协调的独立中文 HTML。忠实于源材料的当前设计，准确区分已废弃的机制和现行设计；内嵌 CSS、无外部资源依赖，兼顾桌面与手机阅读。完成后对实际读回的各页进行验收。",
  pages: [
    {path: "agent-01-overview.html", title: "设计出发点", focus: "朴素循环为何最稳、上下文有限带来的问题，以及用函数作用域代替压缩"},
    {path: "agent-02-program.html", title: "流程与虚函数", focus: "流程代码就是 JavaScript、虚函数是唯一的 AI 入口、第一层与叶子的两层边界"},
    {path: "agent-03-scope.html", title: "作用域与上下文", focus: "参数显式传入、返回值是跨作用域的唯一通道、上下文超限时抛错而不压缩"},
    {path: "agent-04-libraries.html", title: "工具库与并发", focus: "工具即库并通过 import 引入、叶子的工具范围与运行时查找、Promise 取代子 Agent"},
    {path: "agent-05-acceptance.html", title: "错误与交付", focus: "错误统一走异常、副作用边界、已废弃机制，以及真实证据与完成条件"}
  ]
};

const source = io.read({path: request.source});
const shared = {
  source_path: request.source,
  source_content: source.content,
  requirements: request.requirements
};

const designs = await Promise.all(request.pages.map(page => designPage(shared, page)));
for (let i = 0; i < request.pages.length; i++) {
  io.write({path: request.pages[i].path, content: designs[i].content});
}
const review = async page => {
  const actual = io.read({path: page.path});
  return await inspectPage(shared, page, actual.content);
};
const reviews = await Promise.all(request.pages.map(review));
// 未通过的页按验收意见修正一轮，写入后重新读回复检。
const revised = await Promise.all(request.pages.map(async (page, i) => {
  if (reviews[i].passed) return reviews[i];
  const current = io.read({path: page.path});
  const fixed = await revisePage(shared, page, current.content, reviews[i].issues);
  io.write({path: page.path, content: fixed.content});
  return await review(page);
}));
for (let i = 0; i < request.pages.length; i++) {
  assert(revised[i].passed, "修正后验收仍未通过：" + request.pages[i].path);
}

/** @virtual */
async function designPage(shared, page) {
  // shared 含实际源材料与统一要求，page 含标题、重点和输出路径。
  // 设计本页，逐项覆盖 page.focus 中的要求并依据实际源材料说明；
  // 区分现行机制和历史记录，不擅自推断源材料的版本或日期。
  // 主题不同于其他页，HTML/CSS 自包含。
  // 返回 {content:string}；不写文件，不重复读取已传入的材料。
}

/** @virtual */
async function inspectPage(shared, page, content) {
  // 对照实际源材料和本页要求检查真实读回正文。
  // 只验收本页；五页之间是否不同、是否协调不在本函数职责内，不因此判为不通过。
  // 只有与源材料不符、遗漏本页要求等实质问题判为不通过；措辞上的建议可写进 issues，但不影响 passed。
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
  // 不虚构未执行的浏览器或视觉检查。
}

/** @virtual */
async function revisePage(shared, page, content, issues) {
  // content 是本页已保存的真实正文，issues 是验收指出的问题。
  // 依据 shared 中的源材料逐条修正这些问题，保留本页其余正确内容和风格。
  // 返回 {content:string}；不写文件。
}
```

[二十页示例](examples/parallel-twenty-html.js)先设计共享样式，再并发生成每页正文，统一写入、读回和验收。示例中的页数、格式和检查标准仅适用于该业务；未执行浏览器或视觉检查时不能声称已完成。

## 固定写法

- 顶层语句按顺序覆盖用户要求的工作、验证和交付，像 JS 模块一样从上到下执行，没有入口函数，也不需要返回值。程序需要的数据写在代码里，或用导入的工具读取。function 定义辅助任务，helper(args) 调用。
- 内部值保存在 VM 堆中，对象和数组在普通函数调用间保留引用；宿主工具和 AI 参数／结果使用 JSON。支持 const/let/var、局部赋值及赋值表达式、对象／下标赋值及复合赋值、更新、++/--、+= -= *= /= %=、对象简写、简单数组解构、成员／下标读取、模板字符串。
- 控制流支持 if/else、传统 for、while、do/while、数组 for...of、简单绑定的 for...in、switch、带标签 break/continue、三元、assert、return、try/catch/finally/throw。已知循环写进代码，不交给一个宽泛注释任务反复调度。
- + - * / %、比较、逻辑 && || ?? !、模板转换由宿主确定性计算，普通表达式绝不请求 AI。条件中空字符串、false、null、undefined、0 和 NaN 为假，数组和对象为真。&& 和 || 返回所选操作数；短路和三元只执行选中的分支，副作用按从左到右的顺序执行一次。基本数字、布尔与字符串支持部分转换，对象到原始值的转换仍有限制。
- 数字采用 IEEE-754 双精度，超过安全整数范围会像 JS Number 一样舍入；除零产生非有限数，但非有限数不能直接越过宿主 JSON 边界。=== 对对象比较引用身份；基本原始值的 == 支持部分类型转换。复杂对象转换和完整 JS 数值语义仍未覆盖。
- 从 `host:<library>` 导入同步工具；从 `host:<library>/promises` 导入异步工具。所有调用仍经过权限与工具契约检查。
- 本地数组 push 收集结果；支持确定性方法 includes/indexOf/slice/at/concat、字符串 lastIndexOf/startsWith/endsWith/trim/toLowerCase/toUpperCase/split/replace/replaceAll/match、正则 test、数组 join、JSON.stringify/parse、Array.isArray、Object.keys/values、typeof。Object.keys/values 仅接受对象，整数键按索引排序，其余键按插入顺序；宿主 JSON 输入的键顺序可能已经变化。replace/replaceAll 接受字符串或正则查找（replaceAll 的正则须带 g），替换文本按 JS 展开 $&、$1、$<name> 等，替换函数见下条；未知方法编译失败，不能假定会让 AI 解释。
- 声明、普通函数参数和 catch 支持解构及默认值；对象与数组字面量支持展开，可使用可选函数调用。支持函数剩余参数与调用参数展开。非简单参数在独立参数作用域初始化，默认值与其中的闭包不读取函数体局部绑定。
- 函数和闭包可保存、传递和调用。数组 map 支持 async 回调；其他数组回调目前要求同步。普通函数按 JS 处理实参数量；虚函数严格核对参数。
- 路径与 URL 可直接计算：`path.join/resolve/relative/normalize/dirname/basename/extname` 和 `url.parse(value, base)`；结果用于普通流程，不调用 AI。resolve 至少一个绝对路径、relative 的两个参数必须绝对；url.parse 返回 href/origin/protocol/hostname/port/pathname/search/hash。它们由宿主 Rust 实现，处理路径不访问文件系统；文件访问仍使用 io。
- 支持正则字面量及 `/pattern/i.test(text)`、`text.match(/pattern/g)`；只有 g/i/m/s/u 标志，Rust regex 语法不支持环视和回溯引用。test 无状态；match 未命中返回 null，非 g 返回完整匹配和捕获组数组，g 返回完整匹配数组。模式最多 8192 字节，输入最多 2 MiB，结果最多 10000 项、捕获文本最多 4 MiB；超出时报错。不是完整 JavaScript RegExp，支持 new RegExp 和 replace/replaceAll；不支持 lastIndex。
- async 函数返回 Promise，await 挂起并在结果到达后恢复。`Promise.all`、`allSettled`、`race`、`any`、`then`、`catch`、`finally` 可组合结果；只读异步工具最多 32 项一批。async 虚函数可并发，模型请求共享并发槽位。Promise 按 JS 微任务顺序执行，用 Promise 结算另一个 Promise 时同样多等两轮。对顶层 async 函数或 /promises 工具的调用不 await 就直接取属性、参与运算、作为条件、传给 assert 或放进模板字符串，会在编译时报错；需要先保存 Promise 或 await。
- 这是受限 JS 环境：普通 thenable 同化和提取 Promise 方法值尚不支持；禁止 function*/yield、call()/ai() 包装、通用模块、TypeScript 类型、class/this 和 BigInt。

let/const 属于所在的块，内层块可以重复声明同名变量并遮蔽外层，声明前读取会抛出 ReferenceError；var 属于所在函数并提升，首次无初值为 undefined。函数只看自己的变量和顶层变量，调用方的局部变量作为参数传入。普通函数中的对象别名共享修改；AI 作用域和宿主工具以 JSON 交接。字符串 length、索引、slice 和 indexOf 使用 Unicode 标量。模板只插入原始值，对象和数组使用 JSON.stringify。完整规范在 docs/agent-program.md。

## 工具库

program_tool_headers 按库列出可用方法，是 TS 风格的发现目录，其中 unknown 不代表实际参数或返回契约。需要准确参数、约束和工具说明时调用 read_tool_contracts，例如 read_tool_contracts({methods:["io.read","io.write"]})，或 read_tool_contracts({libraries:["io"]}) 读取整个库。返回的真实契约是声明资料，不复制类型到源码。io 是本地文件库，host 是其他宿主操作，插件有自己的库名；只公布本作用域可用工具。导入和读取契约都不扩大权限。

在虚函数中执行实际操作时，同样可按需读取契约，用 invoke_library({method:"io.read",args:{path:"..."}}) 直接调用可用工具，不必先写流程程序。宿主按真实 schema 校验参数并执行权限检查。需要补写当前任务时可先调用 read_program_guide 获取流程语言说明，再使用 define_task_body；源码中通过 import 使用工具。
## AI 工作必须显式调用

需要理解、生成、转换或验收时，显式声明 `@virtual` 虚函数，注释说明职责、输入和返回字段；调用它就是交给执行 AI 的任务。标记放在函数紧邻的前置 JSDoc 或函数体注释里。虚函数体可为空，或仅写 throw new Error("固定说明") 作为占位；不写其他可执行语句或占位 return；返回结构放在注释契约里，由执行 AI 完成实际操作后返回。普通函数中的注释只是说明，不会自动执行；普通代码里直接 return 完成数据只会返回该数据，不会替你制作或检查产物。空函数没有 @virtual 会在编译时拒绝。不要用 artifact !== expected 代替质量验收，也不要把未绑定名字当作描述性业务条件。使用实际证据让函数返回 passed，再在 if/assert 中消费它。

遵循常见的软件设计原则划分函数：保持单一职责与高内聚，针对接口契约编程，将变化原因不同的职责解耦，并通过组合形成流程。复杂或当前无法直接实现的功能可以声明为虚函数，定义调用方需要的能力、输入、职责和返回值；可以按需要声明多个虚函数，每个尽量保持单一、明确的功能，不把多个不同职责堆在同一个虚函数里。普通函数负责组织调用、控制流程、传递数据和处理结果；能够具体实现的步骤也可直接用代码表达。按实际任务选择适当的抽象粒度，避免为了形式拆分函数。

**禁止用表面特征冒充完整验收。** 按当前任务的实际要求设计检查：普通函数明确编排可确定的读取、计算、循环、分支、工具调用和必要的复查；内容理解、无法由现有代码可靠判断的问题交给职责清楚的虚函数和适用工具。基础字符串、数量或存在性检查可以作为有用的局部证据，但不能据此声称内容正确、资源可用或整个任务通过。每项结论必须与实际检查及其证据相匹配；未检查或无法可靠确认的要求应如实报告，不能计入完整通过。编译限制或缺少工具时，不得通过删掉必要检查、降低标准或虚构结果使流程看起来成功；应使用其他可行能力，或明确返回未完成及原因。不要为此把某一种业务的检查规则写死为通用标准。

```js
const request = {output: "report.html", requirements: "按用户要求制作并验收报告"};
let output = produce(request);
let check = inspect(output, request);
if (!check.passed) {
  output = repair(output, check, request);
  check = inspect(output, request);
}
assert(check.passed, "修复后验收仍未通过，停止交付");
deliver(output, request);
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

虚函数任务在当前 Agent 的局部作用域执行，可直接使用已公布工具；也可 define_task_body 补写当前函数及新 helper，继续分解工作；当前函数提供真实实现时去掉 @virtual，其内部尚需 AI 工作的新函数保留 @virtual，不重复安排当前任务。形参与实参按位置绑定；例如 work(shared,part) 的异步调用是 work(shared,part)，每个形参分别接收一个实参，不隐式打包对象；补写保持原参数个数，不会自动把多个参数打包成一个对象。program_task_input 给出函数签名和 bindings（各形参的实际局部值），bindings 不是参数内部的一层数据。若签名是 make(input)，bindings.input 为 {pages,...}，函数内直接用 input.pages；不能再套 input.input。有代码的普通 helper 在 Runtime 函数帧内执行，不创建 AI 任务节点；虚函数调用创建 AI 任务，其工具调用是步骤。顶层运行完毕结束，helper 返回恢复调用方。

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

规划时，把用户目标和业务 skill 表达为顶层流程及有明确职责的辅助函数。进入 program_task_input 指定的虚函数后，当前 AI 就是该函数的执行者：直接完成所需的理解、生成、转换或验收，使用工具并返回约定的实际结果。“智能工作通过虚函数表达”的要求已经由当前调用满足，不要求执行者再声明一个虚函数来承接同一工作。

define_task_body 是按需补写具体实现的方式，不是执行前的必经步骤。可以把当前职责拆成实际子步骤，分别明确处理内容、输入与输出；不能只更换函数名，把相同输入和同一项尚未完成的职责继续转交另一虚函数。拆分应推进处理，而不是重复安排处理。

## 作用域与生命周期

当前函数的真实参数由 program_task_input.bindings 自动提供。普通函数代码按词法作用域只能引用自己的变量和顶层变量，调用方的局部变量需要作为参数传入。异步虚函数使用调用时的作用域快照，之后父作用域变化不自动同步。AI 函数可以看到调用链上各层可见变量的索引：未引用的变量不自动发送正文，scope_variables 提供名称、类型与大小，AI 可用 host.read_scope({name,path}) 按需读取；虚函数只有自然语言注释，不会从注释猜测代码变量引用。源码变量保留到函数返回，编译器临时量可回收。历史工具调用、回执不是自动重放的变量。

容量折叠先扣除固定指令和当前函数输入的开销，再按剩余工作空间的80%触发、向60%回落。自然增长的旧步骤可以折叠为可恢复引用；当前输入及最新交换保留。完整活跃输入超出硬上限时需分页或缩小单次处理范围，不把引用冒充已读取正文。

## 数据流与交付

共同依赖与各自职责分开传参。共享规则传完整对象，资产用真实稳定引用、路径、版本及必要正文；不以“沿用公共规范”代替数据。嵌套 helper 继续传递所需依赖，分支收到各自独占清单，不能只换标签却传相同完整工作。

正文、HTML 等长数据由任务函数返回或保存为资产，编排源码不展开巨型字符串。只读取缺失材料或确需新版的内容；已有输入、技能、工具和回执直接复用。

返回 JSON 要能被下一调用使用；检查任务保留真实工具证据。工具成功、accepted、函数返回或编译通过均不等于业务验收。按业务技能完成修复、异步结果收集和确认点，再交付。工具参数遵守真实契约，返回是原始 JSON，例如 io.read 的 result.content，不带 original_result 包装。

执行前检查完整顶层流程、参数来源、分支所有权和返回契约。编译错误按行列修正；当前虚函数补写体未捕获错误时，在同一函数查看错误、回执与保存的变量，按真实状态继续处理，必要时补写仅包含剩余工作的函数体。新函数体从头执行，不能自动重放已完成或结果未知的外部操作。普通主流程未捕获错误仍会停止。任务返回一个 JSON 值，无 Markdown 围栏；无效 JSON 会在当前函数收到诊断并修正，文本用 JSON 字符串。无法完成返回 {"$task_error":{"message":"具体原因"}}。原权限和插件边界始终有效。

## 源码检查和错误修正

回复中的程序和 define_task_body 的源码都在任何实际操作前整份编译；失败时什么都不执行，诊断带行列、附近源码和错误原因自动返回当前 AI，当前任务及原有结果保留。根据诊断重写完整源码，用同样方式在原作用域重新提交，不重建任务、不重放已完成操作、不把编译失败当作完成。反复无法修正时如实报告阻碍。此修正机制只处理源码，不自动重试未知写入或改写业务目标。

库方法在源码和 invoke_library 中都返回工具原始 JSON，直接按声明读取字段；资产交付的 original_result 包装不属于库返回契约。host.read_asset 的快照结构是该方法自身的数据契约。使用 async 虚函数和 Promise 编排并发；不维护工具组加载状态。

工具返回值保持原 schema；资产编号和工具结果回执从 task_asset_index 读取。需要证据事件号时使用对应条目的 delivery_reference.event_seq，不从业务返回值猜 event_seq。

## 并发与工作区

并发虚函数在同一工作区运行，调用时固定参数和可见作用域。独立任务应分别处理各自的文件，或返回内容交给父流程统一写入。并发函数对同一文件的整文件写入发生冲突时，宿主保留现有文件和待写副本并停止，不自动合并或重放。
