# Agent 流程伪代码与 Rust 执行器

当前语言版本 **9**。使用常见 JS 基础语法表达流程，由 SWC 解析、Rust 执行持久化指令；没有 JS 虚拟机。**普通表达式由宿主确定性求值，智能判断必须写成显式任务函数。** 相同程序、相同输入和相同工具／任务返回值产生相同控制流；AI 任务及外部操作本身不因此变成确定性的。

## 固定语言范围（版本 9）

| 部分 | 允许范围与含义 |
| --- | --- |
| 文件 | 顶层普通 function；唯一 main 接受零或一个输入。辅助函数显式传参。补写源码只有当前函数及所需 helper，没有第二个 main |
| 数据 | JSON 字符串、有限数字、布尔、null、数组、静态键对象；成员／下标读取、对象简写、简单数组解构 |
| 局部值 | const/let/var、局部赋值、赋值表达式、局部 ++/-- 与 += -= *= /= %=。const 不重绑定。变量不共享可变别名 |
| 调用 | helper(args)、已发布的 library.method(args)（如 io.read）；编译时核对 helper 实参数量，不允许调用或 fork main |
| 控制流 | if/else、for(init;test;update)、while、do/while、数组 for...of、无标签 break/continue、三元表达式、return、assert、try/catch/throw |
| 表达式 | + - * / %、一元 + - !、== != === !== < <= > >=、&& || ??、模板字符串；宿主直接计算，不创建模型任务 |
| 收集及数据方法 | 本地数组 push；确定性的 includes/indexOf/slice、字符串 startsWith/endsWith/trim/toLowerCase/toUpperCase/split、数组 join，以及 JSON.stringify/JSON.parse |
| 授权并发 | fork(helper,args) 启动获授权的子 Agent，join(handles) 等待并按顺序返回；子 Agent 不再 fork |

常见语法归约为已有赋值、分支、跳转、调用和返回结构，不为每种写法新增调度协议。for 的 continue 进入 update 再检查条件；while 的 continue 进入条件；do/while 的 continue 进入末尾条件。break/continue 只退出其跨越的异常处理现场，保留外围 catch。

仍禁止 function*/yield、async/await/Promise、call()/ai() 包装、import/export、TS 类型／断言、箭头／匿名／嵌套函数、闭包、class/new/this、for-in、switch、finally、标签跳转、展开／剩余／默认参数、可选链、正则／BigInt、位运算、typeof/void/delete、对象成员写入，以及未公布的数据方法和 JS 全局环境。源码字符串里的这些词语只是数据，不按词语拦截。需要智能处理时声明注释任务；没有“计算失败后自动交给 AI”的路径。

## 确定性的值规则

- 数字保持 JSON 数字类型。十进制整数常量及整型加减乘余数保持精确的 JSON 64 位整数范围（有符号负数／无符号非负数），溢出报错。小数与除法使用有限 f64，5/2 为 2.5；除零、余数除零和非有限结果报错。整数转小数若超出 2^53-1 安全范围则报错，不能静默丢精度。
- + 只接受两个数字或两个字符串；其他算术只接受数字。关系比较接受两个数字或两个字符串。字符串按 Unicode 标量顺序比较。算术、关系比较与相等判断不做字符串／数字／布尔的隐式转换；条件的真值转换单独按下一条规定。
- == 与 === 都是无类型强制转换的 JSON 值相等；!= 与 !== 是其取反。数组逐项、对象逐键比较内容，数字 1 与 1.0 相等；对象相等不是业务质量判断。
- if、循环条件、!、&&、||、三元与 assert 使用固定 C 风格真值：false、null、数值 0（含 0.0/-0.0）为假，其余 JSON 值为真。空字符串、空数组和空对象也为真，不冒充完整 JS 真值规则。&&/|| 短路并返回布尔值；?? 只在左值是 null 时执行右侧。缺少对象字段或越界下标返回 null，不提供 undefined。因此 if(items.length)、if(!result.error)、if(obj) 都可直接执行，不请求 AI 或因非布尔类型中断。
- 三元只执行选中分支；函数／工具调用可以出现在表达式中，按从左到右的确定顺序执行。对象字段、数组元素、调用实参及只读方法接收者在后续副作用前保存值快照；push 在实参求值后追加到当前绑定的本地数组。后缀更新返回旧值，前缀返回新值。独立表达式与 for 初始化／更新即使不使用返回值也必须求值并报告错误。
- 模板插值与数组 join 允许 JSON 原始值，null 转为 "null"；数组／对象需要显式 JSON.stringify。字符串 length、索引、slice 与 indexOf 的单位统一为 Unicode 标量，不是 UTF-16 单元。slice 使用整数下标并支持负数；JSON.stringify 使用稳定键序。
- 函数内的绑定不做块级遮蔽；let/const 名字须互不重复。var 是函数级可重复声明的可变值，不做 JS 提升；无初值的首次 let/var 初始化为 null，重复无初值 var 不重置旧值。for...of 使用进入循环时的数组快照。

数据方法只有文首列出的白名单：includes/indexOf 接受一个查找值（字符串接收者要求字符串实参；数组使用 JSON 值相等），slice 接受一个或两个整数下标；startsWith/endsWith/split 接受一个字符串；trim/toLowerCase/toUpperCase 无实参；join 接受零或一个字符串分隔符（默认逗号）；JSON.stringify/parse 恰好一个实参。类型或实参数量不匹配明确报错，不猜测方法含义。

## 工具声明与固定名字

模型请求发布 program_tool_headers：每个库一份自动生成的 TS 风格声明，包含固定方法名、JSON 参数类型、可选字段、返回类型及注释。程序仍使用不带类型的 JS 子集，声明不是可执行源码。只接收 namespace.method(args)，拒绝 tools.*、动态方法下标与 execute_program/define_task_body 的 libraries 别名参数。

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

业务 skill 和用户决定范围、依赖、确认点、验收与交付；通用 task-program skill 负责表达这些关系。无主任务时先 create_task，准备缺失的技能／工具契约，再 execute_program({source,input})。main 覆盖完整工作、检查与交付。

AI 定义的 helper 调用均为有独立局部上下文的任务；工具调用是实际操作步骤。有代码的 helper 由宿主执行，只有注释的 helper 把注释与命名实参交给执行 AI。后者可以直接调用真实工具，也可 define_task_body 补写当前函数，继续细分任务；不重新创建 main。注释应说明职责、输入与返回字段。

```js
function main(request) {
  let artifact = produce(request);
  const check = inspect(artifact, request);
  if (!check.passed) {
    artifact = repair(artifact, check, request);
  }
  return deliver(artifact, request);
}
function produce(request) {
  // 按 request 制作实际产物，返回路径和版本。
}
function inspect(artifact, request) {
  // 读取或检查实际产物，对照 request 判断。
  // 返回 {passed:boolean, evidence:[实际工具观察], issues:[...] }。
}
function repair(artifact, check, request) {
  // 使用 check 的证据完成授权范围内的修正，返回实际产物。
}
function deliver(artifact, request) {
  // 完成 request 要求的检查与交付，返回真实结果和限制。
}
```

inspect 的判断由 AI 作出；!check.passed 由宿主消费布尔值。不再把 artifact !== expected 解释为“不满足预期”；它只比较 JSON 数据。证据保存在普通任务返回及原始工具回执中，没有运算符专用的 value/evidence 隐式协议。宿主检查调用与返回结构，不认证业务判断的真实性。

## 已知循环与授权并发

```js
function main(request) {
  const plan = prepare(request);
  const a = fork(work, plan.shared, plan.left);
  const b = fork(work, plan.shared, plan.right);
  const outputs = join(a, b);
  const checks = [];
  for (var i = 0; i < outputs.length; i++) {
    const check = inspect(plan.shared, outputs[i]);
    checks.push(check);
    assert(check.passed, `检查未通过：${i}`);
  }
  return {outputs, checks};
}
function prepare(request) {
  // 仅在已获并发授权时划分不同职责；返回 shared、left、right。
}
function work(shared, assignment) {
  // 使用完整共享要求及真实资产引用，只完成自己的 assignment，返回实际结果。
}
function inspect(shared, output) {
  // 检查这份实际 output，返回 {passed:boolean,evidence:[...]}。
}
```

共同材料、真实稳定资产引用和各分支职责显式传参；不以“沿用前文”替代数据。任务函数按 JSON 值交接，局部对话不复制给父／兄弟。主任务验收真实产物。工具受理、函数返回、语法编译通过都不等于业务通过；按技能处理异步结果和必要确认。

main 返回结束整项任务，helper 返回恢复原调用方；finish 只交接并推进，不额外请求模型确认完成。工具只能使用公布的确切名称及参数；工具返回原始 JSON，例如 result.content，没有 original_result 包装。工具仍经过原权限、插件隔离和取消机制，不能递归调用 execute_program/create_task/spawn_agent 等替代函数关系。

## 持久化与兼容边界

源码整体编译通过后才开始执行。编译错误提供行列；类型／算术错误沿 try/catch 传播，未捕获则停止。任务返回 JSON，无 Markdown 围栏；无法完成可返回 {"$task_error":{"message":"具体原因"}}，沿调用链上抛。

agent_runtime_windows.state.program 保存源码、指令、函数帧、PC、变量、catch 栈、指令预算和挂起 effect ID。每次效果先持久化位置，已有回执恢复时直接消费，不重复计算已完成的更新或重放操作。缺少外部回执时结果未知，不能承诺跨进程恰好一次。

版本 8 及更早的现场、源码和回执保留，但拒绝按版本 9 续跑；不把旧 AI 表达式回执解释成新规则。旧 Semantic/Ai 枚举仅用于读取旧快照和明确拒绝，不生成或执行隐式 AI 表达式。源码上限 100,000 字节，最多 64 个函数、调用深度 64，每运行帧 100,000 条指令；预算耗尽强制失败，不能被 catch 隐藏为成功。
