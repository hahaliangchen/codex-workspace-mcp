---
name: task-program
description: 将用户目标与业务 skill 表达为可执行的流程伪代码：唯一 main、显式 AI 任务、工具调用、确定性表达式和基础控制流。
---

# 用程序表达任务

语言版本 8。用普通 JS 基础语法表达完整流程；Rust 宿主执行普通计算，AI 只处理显式任务函数。业务要求、确认点、验收标准来自用户和业务 skill，不写进通用宿主。

## 固定写法

- 一个 main 覆盖用户要求的工作、验证和交付；顶层普通 function 定义辅助任务，helper(args) 调用。main 不能被调用或 fork。
- 数据为 JSON；支持 const/let/var、局部赋值及赋值表达式、++/--、+= -= *= /= %=、对象简写、简单数组解构、成员／下标读取、模板字符串。
- 控制流支持 if/else、传统 for、while、do/while、数组 for...of、无标签 break/continue、三元、assert、return、try/catch/throw。已知循环写进代码，不交给一个宽泛注释任务反复调度。
- + - * / %、比较、逻辑 && || ?? !、模板转换由宿主确定性计算，普通表达式绝不请求 AI。条件采用 C 风格真值：false、null 和数值 0 为假，其余 JSON 值为真（空字符串、数组、对象也为真）。if(items.length)、if(!result.error)、if(obj) 都合法。算术和比较仍不做字符串／数字强制转换。短路和三元只执行选中的分支，副作用按从左到右的顺序执行一次。
- 数字运算类型明确：整数加减乘余数精确保留 JSON 64 位整数，溢出报错；除法为有限小数除法，除零报错。超过 2^53-1 的整数不能隐式转换成小数。+ 只连接两个字符串或相加两个数字。==/=== 都比较 JSON 值内容，不表示业务质量。
- 优先使用 program_tool_libraries 发布的 library.method(args)，如 io.read({path})、io.write({path,content})。工具原 JSON 参数和返回不变；tools.EXACT_NAME(args)／静态 tools["exact/name"](args) 保留作精确名称入口。获授权的多 Agent 用 fork(helper,args)、join(handles)；子 Agent 不再派生 Agent。
- 本地数组 push 收集结果；只支持确定性方法 includes/indexOf/slice、字符串 startsWith/endsWith/trim/toLowerCase/toUpperCase/split、数组 join、JSON.stringify/parse。未知方法编译失败，不能假定会让 AI 解释。
- 禁止 function*/yield、async/await/Promise、call()/ai() 包装、模块导入导出、TS 类型、闭包、箭头／匿名／嵌套函数、class/new/this、for-in、switch、finally、标签跳转、展开／剩余／默认参数、可选链、正则／BigInt、位运算、typeof/void/delete、对象成员写入和 JS 全局环境。

let/const 在函数内不做块级遮蔽，使用不同名字；var 可以重复声明，不做提升，首次无初值为 null。数据按 JSON 快照交接，不共享可变别名。字符串 length、索引、slice 和 indexOf 使用 Unicode 标量。模板只插入原始值，对象和数组使用 JSON.stringify。完整规范在 docs/agent-program.md。

## 工具库

io.read/read_lines/write/list/search/edit/replace 对应真实文件工具。插件库和方法从当前启用、可用的工具目录发布，不按业务含义猜测。必要时 execute_program/define_task_body 的 libraries 参数可建立新别名：{"ppt":{"get_design":"目录公布的精确工具名"}}，然后写 ppt.get_design(args)；这里只说明映射形状，实际值必须从目录取得。不能覆盖已发布方法或指向不存在／无权限的工具，不能通过库调用任务调度器。

库方法使用原工具的 JSON 对象参数，不自动把字符串或位置参数转换成字段。只支持静态 namespace.method，不把库当作 JS 对象或函数值。公共库映射随 main 保存、向 helper/补写/fork 继承；不需要在各分支重新发现或重填。方法最终走原来的工具分发、工具组加载、权限与插件归属检查。

## AI 工作必须显式调用

需要理解、生成、转换或验收时声明注释函数，说明职责、输入和返回字段；调用它就是任务。不要用 artifact !== expected 代替质量验收，也不要把未绑定名字当作描述性业务条件。使用实际证据让函数返回 passed，再在 if/assert 中消费它。

```js
function main(request) {
  let output = produce(request);
  const check = inspect(output, request);
  if (!check.passed) {
    output = repair(output, check, request);
  }
  return deliver(output, request);
}
function produce(request) {
  // 按 request 制作实际产物，返回路径、版本和实际结果。
}
function inspect(output, request) {
  // 读取真实产物，对照 request 判断，返回 {passed:boolean,evidence:[...],issues:[...]}。
}
function repair(output, check, request) {
  // 依据 check 修正授权范围内的问题，返回实际产物。
}
function deliver(output, request) {
  // 完成所需检查和交付，如实返回结果和限制。
}
```

注释任务在当前 Agent 的局部作用域执行，可直接使用已公布工具；也可 define_task_body 补写当前函数及新 helper，继续分解工作，不新建第二个 main、不重复创建当前任务。形参按原命名输入绑定，或用单个形参接收完整输入对象。有代码的 helper 同样是任务，其工具调用是步骤。main 返回结束，helper 返回恢复调用方。

当前调用为 process(shared, part) 时，可补写：

```js
function process(shared, part) {
  const source = io.read({path: part.source});
  const changed = transform(shared, part, source.content);
  const saved = io.write({path: part.output, content: changed.content});
  return {path: part.output, receipt: saved};
}
function transform(shared, part, content) {
  // 根据 shared、part 处理 content，返回 {content:string}。
}
```

## 数据流与交付

共同依赖与各自职责分开传参。共享规则传完整对象，资产用真实稳定引用、路径、版本及必要正文；不以“沿用公共规范”代替数据。嵌套 helper 继续传递所需依赖，分支收到各自独占清单，不能只换标签却传相同完整工作。

正文、HTML 等长数据由任务函数返回或保存为资产，编排源码不展开巨型字符串。只读取缺失材料或确需新版的内容；已有输入、技能、工具和回执直接复用。

返回 JSON 要能被下一调用使用；检查任务保留真实工具证据。工具成功、accepted、函数返回或编译通过均不等于业务验收。按业务技能完成修复、异步结果收集和确认点，再交付。工具参数遵守真实契约，返回是原始 JSON，例如 read_file 的 result.content，不带 original_result 包装。

执行前检查完整 main、参数来源、分支所有权和返回契约。编译错误按行列修正，运行时类型错误明确失败，不能转交 AI 猜测。任务返回一个 JSON 值，无 Markdown 围栏；文本用 JSON 字符串。无法完成返回 {"$task_error":{"message":"具体原因"}}，未捕获错误停止，不重放外部操作。原权限和插件边界始终有效。
