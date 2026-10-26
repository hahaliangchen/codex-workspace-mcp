# Agent 流程伪代码与 Rust 执行器

当前实现语法版本为 **5**。使用普通函数形状的伪代码表达流程，SWC 解析 AST，Rust 将其编译成持久化指令并推进；不执行任意 JavaScript。真实测试结果和历史失败见[测试报告](agent-program-real-test-2026-10-26.md)。

**最新设计边界：伪代码用于表示流程，不是编写真正的业务代码。** 简单 C 风格的函数、数组、循环、分支和返回用于组织工作；描述性的比较、判断和断言由执行 AI 依据实际证据理解，宿主消费结论并推进流程，不为每种表达逐项补运算符或扩展成 C／TS 虚拟机。完整原则见[通用设计第 1.1 节](generic-agent-design.md#11-伪代码表示流程不是编写可执行业务代码)。版本 5 将描述性表达式编译成持久化的语义任务：AI 接收表达式、命名输入和函数注释，必要时通过工具取得证据，宿主接收明确值后继续流程。TS 仍用于结构解析，不执行业务运算。

## 业务要求与表示规范

用户及业务 skill 决定做什么、先后依赖、确认点、验收和交付。通用 [task-program skill](../skills/task-program/SKILL.md) 教模型如何表达这些要求：唯一 main、辅助函数、工具库、参数和返回、串行及经授权的并发。该 skill 编入宿主，在规划请求中作为独立系统消息固定注入，不靠模型反复查找；执行注释函数时不再注入编程指令，而是注入当前函数的执行契约。没有 PPT 等具体业务分支。

没有主任务时，先单独调用 create_task 建立目标及背景。发现缺失的业务 skill、工具确切名称和参数后，通过 execute_program({source,input}) 提交完整程序。main 覆盖用户要求的工作、验收和交付，不能仅完成准备就返回。主入口接受零或一个 JSON 参数。

## 两种辅助函数可以组合

明确的操作直接编排工具调用，需要 AI 判断与处理的工作由注释任务表达：

```ts
function main(request) {
  const source = tools.read_file({path: request.source});
  const result = process(source.content, request);
  return inspect(result, request);
}
function process(content, request) {
  // 工具是程序库，准确采用公布的方法名与字段。
  const changed = transform(content, request);
  const receipt = tools.write_file({path: request.output, content: changed.content});
  return {path: request.output, receipt};
}
function transform(content, request) {
  // 根据 request 的要求处理 content。
  // 返回 {content:string}，供调用方保存；不要另写主程序。
}
function inspect(result, request) {
  // 读取 result.path 的实际产物，按 request 验收。
  // 完成授权范围内的修复后，返回交付说明与检查证据。
}
```

- AI 定义的函数调用均为任务：有执行语句时，宿主在该任务作用域执行已编译的函数体，其内部工具调用记录为步骤；注释说明职责。
- 函数体只有注释：编译成局部任务调用。注释作为 description，实际参数按参数名绑定为 input；当前 Agent 使用已有正常工具循环完成工作，交回 JSON 值，再恢复调用方。
- 空函数没有说明会报错；main 不能只是注释任务。
- 同一程序可以嵌套调用，调用结果可以成为其他调用的实参、成员访问或布尔条件。编译器固定其执行顺序。

共同依赖与分支职责显式分开传参，共享参数携带所需规则及真实资产引用；“沿用公共规范”不是数据。已知的迭代与依赖应由函数、for...of 和返回数据表达，注释任务承接具体判断与处理，不承担整个已知循环的临场调度。

注释任务不另开 Agent、不写第二份 main。它可使用 define_task_body 补写当前函数体，保持当前名称和返回契约；形参可按原命名输入绑定，或用单个形参接收完整命名输入对象；内部继续调用自定义函数任务和工具步骤。补写在原任务执行，不重复创建当前任务，辅助函数也可以继续补写。定义源码只能包含当前函数及所需辅助函数，不能声明 main。它继承父环境的技能、工具和材料，拥有自己的过程上下文；结束时保存原始事件和产物引用，释放局部工作窗口，调用方继续使用自己的上下文与返回值。局部模型调用不会重新调度父程序。

工具返回原始 JSON，例如 read_file 的正文为 result.content，没有聊天交付层的 original_result 包装。tools["exact/name"] 支持带特殊字符的工具名。所有工具仍经过原权限和插件归属检查；可用但隐藏的工具组可自动加载，不扩大授权。异步受理不等于完成，仍须遵守业务工具契约。

## 串行与并发

```ts
function main(request) {
  const plan = prepare(request);
  const a = fork(work, plan.left);
  const b = fork(work, plan.right);
  const results = join(a, b);
  return deliver(results, request);
}
function prepare(request) {
  // 读取共享材料，按已授权的并发范围划分工作。
  // 返回 left/right，各含自身工作清单和所需共享资料。
}
function work(assignment) {
  // 完成 assignment 指定的工作并保存产物。
  // 返回产物路径及实际检查结果，不能扩展到另一分支。
}
function deliver(results, request) {
  // 收齐实际产物，按原始需求验收、整合并返回交付说明。
}
```

普通 helper(args) 在当前 Agent 执行。fork(helper,args) 启动子 Agent 返回句柄；join 等待全部指定句柄，按传入顺序返回类型化结果。只有用户或适用 skill 授权多 Agent 时使用 fork；子 Agent 不再派生 Agent。

分工必须进入实际参数。给相同的完整清单分别贴“左／右”标签，不算划分工作。共享材料和各分支独占的职责分开传递。父程序负责实际验收；返回值、编译成功、任务状态都不是业务完成证明。

main 返回结束整个任务；helper 返回恢复调用方。finish 只是确定性地交接，不再调用模型询问“是否完成”。原生委派保留历史 review 接口；编译函数的验收由父程序安排，不额外插入模型确认。

## 语法与边界

允许顶层普通函数、类型标注、JSON 字面量、参数及成员引用、const/let 结果绑定、简单局部赋值、简单数组解构、函数调用、if/else、assert(condition, message)、数组 for...of、return、try/catch/throw。for...of 的真实数组、迭代位置以及本地 results.push(value) 结果收集由宿主推进。首次未声明名字的简单赋值建立可重绑定局部值；后续赋值保存新返回值。const 不可重新绑定，不支持对象成员写入、复合赋值或递增递减。

比较、逻辑、取反、其他值运算、三元表达式、模板插值和普通数据方法按原文交给 AI 解释，编译器不逐项实现其 JS 含义。比如产物 !== 预期可以表示“不满足预期”；有关要求来自当前任务、命名输入及函数注释。描述性未知名字可以表示业务概念，不会被宿主当成 JS 全局变量查找。表达式仅携带实际引用的局部值；父环境仍按原作用域与容量规则继承。已有布尔值直接推进；非布尔条件交给语义任务，不做隐式真值转换。assert 消费明确布尔结论，失败抛出指定错误；未捕获即停止后续操作。

语义任务通过已有 create_task／finish 通道执行，拥有工具、技能及独立局部上下文。需要实际产物时先读取或检查，再返回 {"value": true, "evidence": [...]}。一般数据表达式可返回任意 JSON value；条件必须返回真布尔值。宿主验证 value 字段、非空 evidence 数组和条件类型，原始完整回执持久化；只将 value 交回调用方。格式无效、证据数组缺失或任务失败都会沿 catch 传播。非空 evidence 是结构约束，不能证明其内容真实或业务验收通过；具体判断仍由 AI、实际工具证据和业务契约负责。

先把真实工具／helper 调用结果绑定为变量，再在比较或逻辑表达式里引用；编译器拒绝在这些不透明表达式里隐藏真实调用，避免宿主猜测短路与业务条件。if(inspect(item).passed) 仍执行一次普通调用，再消费明确结果。语义任务细化函数体时同样保持 value/evidence 契约。数据方法只描述新返回值，不执行 JS、不修改调用方变量；results.push 只在正常调用位置进行确定性收集。

禁止展开、闭包、Promise、类、动态导入、DOM/Node/eval、finally 和真实 JS 执行。源码仍须是 SWC 可解析的函数结构；不接收任意自然语言或完整 C 源码。纯字符串里的 HTML 或代码属于不透明数据，不按词语拦截。

变量不做块级遮蔽，函数不能捕获另一函数的局部变量，共享值显式传参。预留 tools/call/fork/join/ai/assert/JSON 和 $temp 前缀。工具不能递归调用 execute_program/create_task/spawn_agent/join_agents/return_task 来替代函数关系。源码上限 100,000 字节，最多 64 个函数，任务调用深度 64，单运行帧指令预算 100,000。

兼容历史 function*、yield call/fork/join 和 ai({instruction,input,format})；新规范使用普通调用和注释任务。ai 仅产出一次无工具模型值，不能代替需读写或渲染证据的工作。旧版本程序保留原文，但明确拒绝不兼容恢复。

## 状态、错误和恢复

agent_runtime_windows.state.program 保存源码、编译指令、函数帧、PC、变量、catch 栈及挂起 effect ID。注释任务和语义表达式通过现有临时任务及 finish 迭代器保存、切换和恢复上下文，任务 inputs.program_task 保存名称、说明与命名实参；不新建一套任务状态机。内部描述只能由当前挂起的 Call／Task／Semantic effect 产生，模型不能通过伪造 create_task 参数注入执行模式。

任务体返回一个符合注释契约的 JSON 值，不带 Markdown 围栏；文本也须返回 JSON 字符串。无效 JSON 是调用失败，不能当成功说明接着跑。无法完成时返回 {"$task_error":{"message":"具体原因"}}。宿主沿调用帧上抛，try/catch 可处理，未捕获就停止后续操作并保存位置及栈。实际错误不会自动重试外部写入。

工具 call/result（含语义任务的完整 value/evidence 回执）、program/ai_result、program/error 和函数任务节点是审计事实。每次 effect 先保存挂起位置；已有回执时恢复消费回执，不重放操作。没有回执的外部写入可能已经执行，恢复只能报告未知或中断，不能承诺跨进程恰好执行一次。
