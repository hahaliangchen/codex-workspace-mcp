---
name: task-program
description: 将用户目标与业务 skill 表达为可执行的流程伪代码：唯一 main、注释任务函数、工具库调用、串行和经授权的并发。用于本 Agent 的工具业务编排。
---

# 用程序表达任务

业务 skill 说明要完成什么及其流程、确认点和验收条件；本 skill 只教如何表达调用关系。宿主负责执行、保存和调用返回，不替业务决定步骤。纯对话回答不写程序。

## 写法

用简单 C 风格的函数语法表示流程，写一个完整的 `main(request)`。语法借用 TS 结构解析，不要求编写真实业务 JavaScript；业务语义交给 AI，宿主负责调用、返回、分支和迭代。它覆盖用户要求的工作、验证和交付，调用辅助函数；不能只准备资料就从 main 返回。源码交给 `execute_program({source,input})`，无需执行 JavaScript。

流程源码只表达调用、参数、依赖和返回。长正文或大型生成数据由任务函数产出为返回值或保存资产，再交给工具使用；不要把这些正文展开成流程源码里的巨型字符串，避免多层转义与代码／数据混杂。简短配置和普通参数仍可使用 JSON 字面量。

调用对象决定执行单元：AI 定义的函数调用是任务，宿主提供的工具库调用是步骤。任务函数可以组合子任务与工具步骤，不以有无注释判断是否为叶子。函数体有两种写法，可以组合：

- 已明确的操作：在函数内编排工具库或其他函数调用，注释说明其用途。
- 需要 AI 完成的工作：函数体只写注释。宿主把注释当任务说明，把实参按参数名绑定为输入，让当前 Agent 在局部任务作用域执行。执行 AI 可以直接调用工具，也可以补写当前函数体并调用更细的任务，返回后继续调用方。

注释说明本函数做什么、输入如何使用、要交回什么字段。输出应能直接作为下一调用的参数；实际产物宜返回路径／稳定引用和检查结果。工作步骤和规模来自用户与业务 skill，不从示例推定固定步骤、数量或分工比例。

空函数执行时，使用 `define_task_body({source})` 补写**当前函数**，保留 program_task_input 中的函数名和调用方所需返回契约。形参可直接使用命名输入的字段名，也可用一个形参接收完整命名输入对象；例如原输入为 {shared, item}，单个 input 形参通过 input.shared 和 input.item 访问。源码声明当前函数及需要的辅助函数，不声明另一个 main；宿主直接在当前任务作用域执行，不新增重复任务。已完成的操作用保存结果承接，只编排余下工作。辅助函数可以继续包含代码、说明注释、空任务以及工具步骤。

例如，当前调用是 process(shared, part)，可补写：

```ts
function process(shared, part) {
  // 当前任务继续执行，内部自定义方法调用形成子任务。
  const source = tools.read_file({path: part.source});
  const result = transform(shared, part, source.content);
  tools.write_file({path: part.output, content: result.content});
  return {path: part.output};
}
function transform(shared, part, content) {
  // 按 shared 与 part 处理 content，返回 {content:string}。
  // 需要时可继续补写本函数并调用其子任务。
}
```

## 参数和结果

把共同依赖与各自职责分开。准备函数返回共享材料及各份工作；后续需要这些材料的函数显式接收共享参数，嵌套调用也继续传递。可以传同一个 shared 对象或明确的所需子集，不能用“沿用公共规范”“参考前文”替代规范与资料本身。父环境继承提供已有能力，不替代函数的数据契约。

共享参数按需要携带已确认的要求、公共规则和资产。资产使用工具返回的真实稳定引用、路径、版本及必要正文，供被调用函数定位材料；不虚构引用，不为共享强行复制全部大正文。已有材料足够时直接使用；只有缺失具体内容或确需新版时才读取对应引用。各分支另外接收自己的工作清单和资源范围，不能只更换标签却传入相同的完整工作。

返回数据要能被下一调用使用：产物及其稳定引用、处理结果、尚未解决的问题和必要证据。汇总沿实际返回值和记录推进，不重新发现工作范围。明确变量从哪里产生、传给谁，不假定另一个局部函数的变量会自动生效。

## 编排已知的步骤

已知的先后依赖、逐项迭代和分支写在代码里；注释任务函数承接其中具体的判断或处理。不要把已经明确的多个阶段和整个循环塞进一个笼统的注释任务，留给内部工具循环反复决定下一步。相同处理作用于一个清单时，用 for...of 调用当前条目的函数，让调用方持有清单与迭代位置；条目返回后自然进入下一项。

工具库使用当前公布的真实方法与契约。已明确的读写可以直接写在辅助函数里，注释解释用途。可以用描述性条件和 assert 表达检查要求，或让检查函数返回布尔结论及证据；调用方据此继续或抛出具体问题。检查通过后使用其结果，不重新把整个检查阶段再做一遍。需要收集循环结果时，声明本地结果数组（const results = []），逐项 results.push(result)，再把 results 传给汇总函数。数组追加是宿主支持的结果收集语法，不执行任意 JS 方法；数组按 JSON 值传递，不共享别名。也可使用已有记录能力和稳定引用。可用 let 或简单局部赋值保存后续返回值；不要编造不存在的宿主工具。

下面只展示调用和数据流；函数名、输出字段和并发数量不是所有业务的固定流程：

```ts
function main(request) {
  const plan = prepare(request);
  const a = fork(process, plan.shared, plan.left);
  const b = fork(process, plan.shared, plan.right);
  const outputs = join(a, b);
  return verify(plan.shared, outputs);
}
function prepare(request) {
  // 按实际需求准备 shared 公共规则与真实资产引用。
  // 此例已获并发授权：返回职责不同的 left/right，含 source 和 output。
}
function process(shared, part) {
  // 使用公布的工具库方法读取、处理和保存。
  const source = tools.read_file({path: part.source});
  const changed = transform(shared, source.content, part);
  const saved = tools.write_file({path: part.output, content: changed.content});
  return {items: [{path: part.output, receipt: saved}]};
}
function transform(shared, content, part) {
  // 根据 shared 和 part 处理 content，只处理该分支职责。
  // 返回 {content: string}。
}
function verify(shared, outputs) {
  for (const output of outputs) {
    for (const item of output.items) {
      const check = inspect(shared, item);
      if (check.failed) { throw check; }
    }
  }
  return {outputs, verified: true};
}
function inspect(shared, item) {
  // 按 shared 规定的实际验收条件，检查当前 item 的真实产物。
  // 使用所需工具获得证据；返回 {failed: boolean, evidence, issues}。
  // 只处理当前条目，不重新枚举全部工作或代替调用方调度。
}
```

示例并发只适用于用户或业务 skill 已授权多个 Agent 的情况。串行直接调用函数。`fork(fn,shared,part)` 启动该函数对应的子 Agent，`join(a,b)` 等待返回；子 Agent 不再派生 Agent。任务函数默认由当前 Agent 执行，不是每个函数都新开 Agent。返回 main 之前完成业务要求的交付；示例的返回对象仅演示类型化交接，不代表任何业务验收标准。

## 数据和控制流

允许 JSON 字面量、参数／成员引用、`const/let` 接收返回值、简单局部赋值、函数调用、`if/else`、数组 `for...of`、`assert(condition, message)`、`return` 和 `try/catch/throw`。首次 `产物 = 制作(要求)` 可以建立局部值，后续 `产物 = 修正(产物, 要求)` 替换它；const 不可重新绑定，不支持成员写入、复合赋值或递增递减。

比较、逻辑、取反、描述性方法、三元表达式和模板插值交给语义任务解释，不按 JavaScript 运算。比如 `if (产物 !== 要求.期待产物)` 可以表示“实际产物不满足预期”，不是对象身份比较。函数里的注释说明判断意图，参与表达式的局部值按名字绑定，原任务要求与父环境继续可用。已返回的真布尔值直接推进，不再询问 AI；其他条件不会做 JS 真值转换。`assert` 失败沿已有异常通道传播，没有 catch 就停止。

语义任务可用正常工具读取、渲染或检查真实产物，返回 `{"value": true, "evidence": [...]}`（普通数据表达式的 value 可以是任何 JSON 值）。evidence 必须非空，指向已有输入、保存结果或本次真实工具观察；缺少证据先获取，无法判断就返回 `$task_error`，不能猜测通过。宿主检查返回格式、布尔类型及非空证据，保存原始回执后只将 value 交给后续流程；这不等于宿主认证业务验收。语义任务只解释当前表达式，不重新规划 main。若用 define_task_body 细化它，仍须保持 value/evidence 返回契约。

先绑定需要实际执行的工具／任务调用，再在语义表达式中引用其结果，避免在 `&&`、三元等描述里隐藏实际操作：`const actual = tools.read_file({path}); if (actual.content !== expected) { ... }`。`if (inspect(item).passed)` 这类直接调用仍按普通调用执行一次，再消费结果。数组和 for...of 由宿主推进，清单必须来自真实数组返回值；本地 `results.push(value)` 是确定性的结果收集。其他方法只描述返回值，不会运行 JS 或修改调用方变量。

工具通过 `tools.EXACT_NAME(args)` 调用；带特殊字符的名称可写 `tools["exact/name"](args)`。准确使用已公布的工具契约。程序里工具返回原始 JSON，例如 read_file 的正文为 result.content，不含聊天交付用的 original_result 包装。工具库不是 JavaScript／浏览器／Node 环境。

提交前像检查普通程序一样，沿 main 看一遍：用户目标是否完整覆盖，每个调用是否收到所需参数，每个返回字段是否有来源，串行依赖与并行所有权是否正确，验收是否读取实际产物。源码编译通过只说明表示合法，不保证业务逻辑正确。

注释任务完成时返回一个 JSON 值，与注释约定的输出匹配；不要 Markdown 围栏。普通说明可以作为 JSON 字符串返回。无法完成则返回 {"$task_error":{"message":"具体原因"}}，宿主会沿调用链传播；没有 catch 时终止，不自动重放外部操作。工具受理不代表异步工作完成，继续遵循业务工具契约与确认点。
