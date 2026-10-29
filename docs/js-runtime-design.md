# JS 运行时设计（语言版本 14）

状态：M0–M8 的实现、示例迁移和约定回归已完成。运行时保留少量旧快照类型和串行动作适配，以便恢复或明确拒绝已退役的 fork/join 现场；异步只读调用在宿主合批。已知 JS 子集边界和并发验证见 [promise-virtual-tasks.md](promise-virtual-tasks.md)。

## 1. 目标与不变的机制

目标：流程源码按真实 JS 语义运行，包括块级作用域、闭包、函数值、async/await 与 Promise；模型按习惯写出的 JS 能直接编译执行。运行时继续由本项目自己实现，下面这些底层机制保持不变：

- 整份源码先由 SWC 解析并编译，通过后才执行；编译错误带行列，任何效果发生前拒绝。
- 外部效果（工具、虚函数、fork/join）只发生在指令边界；发出前整个运行时存盘；按回执恢复，不重放外部操作。
- 工具和 AI 的参数、结果走 JSON 边界。
- `@virtual` 空函数是 AI loop 的钩子：它只规定签名和职责，函数体由 AI loop 实现；调用它就起一次 AI loop，结果回到调用点；`define_task_body` 可补写当前函数；`$task_error` 变成异常。`@virtual` 与同步、异步无关：和普通函数一样，由是否声明 `async` 决定（见第 5、7 节）。
- 父作用域继承：AI 能读到调用链上的变量快照（`scope_variables`、`read_scope`）。
- 权限、插件归属、取消、模型并发队列不变。fork/join 保留到 M6，之后由 async 虚函数取代（第 15 节）。

借 QuickJS 的是设计思路（作用域分析、每轮迭代的新绑定、async 帧、Promise 任务顺序），不搬代码。解释器按最简单的方式实现（例如变量按名沿环境链查找）：流程的耗时在 AI 和工具上，不追求解释速度。

本轮不做：class、this、自定义 `new`、原型与属性描述符、getter/setter、生成器、Symbol 与迭代器协议、Proxy/Reflect、Map/Set（以后作为内置补上）、UTF-16 字符串（仍按 Unicode 标量）。正则换成 ECMAScript 语义的 `regress` crate 是独立的一项，可随时做。

## 2. 现状与差距

| 方面 | 版本 13 | 版本 14 |
|---|---|---|
| 变量 | 每帧一个平铺的 `vm_locals`；没有块级作用域，同一函数里重复 `for (let i…)` 或两个 `catch (error)` 编译失败 | 堆上的词法环境链：块级作用域、var 提升、TDZ |
| 普通函数看到的外层变量 | 调用时按名从调用方各帧拷贝（`Function.captures`，相当于动态作用域） | 词法闭包：只看定义处可见的变量，修改彼此可见 |
| 函数值 | 没有；回调在编译期内联进调用者，只能是单个表达式 | 函数表达式、箭头函数、嵌套声明都是值（`Closure`） |
| 带回调的内置方法 | 编译器生成指令（`callback_method`、内联归并排序） | 用 JS 写的预置库，由同一编译器编译 |
| 异步 | `await` 是串行等待；`Runtime.pending` 只能挂一个效果；`promise.rs` 未接入 | 协程、Promise、任务队列；多个效果可同时挂起 |

验收样本：`scratch/five-page-flow-experiment/acceptance-flow.model.js` 是模型自然写出的 676 行流程。按当前编译器规则，它在同一函数内重复 `let i`、写两个 `catch (error)`，无法编译。

## 3. 值与堆（heap.rs）

```rust
enum VmValue { Undefined, Null, Boolean(bool), Number(JsNumber), String(String), Reference(HeapId) }

enum HeapNode {
    Object(Vec<(String, VmValue)>),
    Array(Vec<VmValue>),
    Environment { parent: Option<HeapId>, bindings: BTreeMap<String, VmValue>, uninitialized: BTreeSet<String> },
    Closure { function: FunctionId, environment: HeapId },
    Native(NativeFn),   // 工具、内置函数、Promise 的 resolve/reject
    Promise { state: PromiseState, reactions: Vec<Reaction>, handled: bool },
}
```

- 去掉 `VmValue::Promise`：Promise 是普通堆对象，参与身份比较和垃圾回收。
- `uninitialized` 记录还在 TDZ 中的名字：声明执行前读取它，抛 `ReferenceError`。
- 垃圾回收的根：所有协程的帧（环境和临时量）、任务队列、挂起效果的 Promise、模块环境。

## 4. 作用域

### 4.1 编译期解析

编译器维护作用域栈：块 → 函数 → 外层函数 → 模块 → 继承的快照名 → 内置全局。每个标识符在编译期解析到某一层，找不到就报 unknown variable，保持执行前拒绝。给 `const` 赋值、同一块里重复声明同名 `let/const`，都在编译期报错。

- `let`、`const` 属于所在块；`var` 属于所在函数并提升，函数入口初始化为 undefined；函数声明在所在块或函数入口提升并立即建闭包。
- 运行时按名沿环境链查找。名字一定存在由编译期保证，运行时只多一个 TDZ 检查。
- 编译器临时量（`$tempN`）不进环境，留在帧里；沿用现在的可达性回收。

### 4.2 指令

| 指令 | 作用 |
|---|---|
| `EnterScope { lexical, functions }` | 新建子环境；`lexical` 中的名字置为未初始化；`functions` 中的函数声明立即建闭包 |
| `LeaveScope` | 回到父环境 |
| `CopyScope` | 用于 `for (let …)`：复制当前环境（父环境不变），上一轮创建的闭包保留旧的那份 |
| `Declare(name, expr)` | 初始化绑定，解除 TDZ |
| `Assign(name, expr)` | 沿环境链赋值 |

函数入口由运行时建函数环境（参数、var、函数体顶层的 let/const 和函数声明）。`break`、`continue` 跨出块时，编译器按跨过的层数补 `LeaveScope`，与现在补 `EndCatch` 的做法相同。`catch` 处理器记下进入 try 时的环境，抛出时先恢复到该环境，再为 catch 参数建一个块。

`for (let i = 0; i < n; i++)` 编译后：

```text
        EnterScope [i]; Declare i = 0; CopyScope
loop:   Branch !(i < n) → exit
        …循环体…
next:   CopyScope; Assign i = i + 1; Jump loop
exit:   LeaveScope
```

`for (const x of list)` 每轮 `EnterScope [x]` 开始、`LeaveScope` 结束。

### 4.3 与版本 13 的差异

- 同一函数里可以重复 `for (let i …)`、写多个 `catch (error)`，内层可以遮蔽外层。
- 普通函数不能再直接使用调用方的局部变量，只能用定义处可见的变量（真实 JS）。模块顶层变量对所有顶层函数可见，现有流程大多不受影响。
- AI 能看到的作用域不受这条影响，见第 8 节。

## 5. 函数值与调用

- 每个函数（顶层声明、嵌套声明、函数表达式、箭头函数）编译成独立的代码单元，有唯一 id。顶层声明用函数名作 id（fork 和虚函数仍按名找），其他用 `外层名/名字@行:列`。`Function` 增加 `is_async`、`is_arrow`、`internal`（预置库），去掉 `captures`。
- `Expr::Function(id)` 求值时用当前环境建 `Closure`。
- 表达式只做纯计算；所有调用都是指令：`Call { dest, callee, args }`、`CallMethod { dest, receiver, name, args }`、`Construct { dest, class, args }`（只支持内置构造：Error 系列、RegExp、Promise）。现在的编译器已经把调用提到临时量，这里只是把 `StringMethod`、`NodeBuiltin` 也改成调用指令，因为编译期不知道 `x.map` 的 `x` 是数组还是带函数属性的对象。

运行时按被调用的值分派：

| 被调用的值 | 行为 |
|---|---|
| 普通函数的闭包 | 在同一协程压一帧，返回值写回 `dest` |
| async 函数的闭包 | 新建协程并立即运行到第一个 `await`，`dest` 得到它的 Promise |
| 同步 `@virtual` 函数的闭包 | 发出 AI 任务效果并等待，AI loop 返回后 `dest` 得到返回值（和现在一样） |
| `async` 的 `@virtual` 函数的闭包 | 发出 AI 任务效果，`dest` 立即得到 pending Promise，调用方继续执行 |
| 从 `host:<库>` 导入的工具 | 发出工具效果并等待，`dest` 得到工具结果（和现在一样） |
| 从 `host:<库>/promises` 导入的工具 | 发出工具效果，`dest` 立即得到 pending Promise |
| `fork`、`join` | 发出对应效果并等待（和现在一样；M7 删除，见第 15 节） |
| 内置 Native（`Boolean`、`String`、`Math.max` 等） | 在 Rust 里同步算完 |
| 其他值 | `TypeError: x is not a function` |

同步还是异步完全由声明决定，和普通函数相同：同步调用等待并直接得到结果，对结果再 `await` 也照样得到它；异步调用返回 Promise。返回 Promise 的两种调用从 M5 起可用（第 7 节）。

- 参数个数：普通函数按 JS 规则，少传为 undefined，多传忽略；`@virtual` 函数和 fork 目标仍严格按位置核对（这是给 AI 的契约），直接调用时编译期检查，运行时再查一次。
- 方法调用：数组、字符串、数字、正则的内置方法走原生方法表或预置库；对象属性是函数时直接调用（没有 `this`）；命名空间导入的 `io.read` 在编译期核对方法名，沿用现有报错。
- `fork` 的第一个参数仍必须是顶层函数名，因为子 Agent 按名运行同一份编译结果。
- `this`、`arguments`、自定义 `new` 在编译期拒绝。
- 上限：用户顶层函数仍最多 64 个；全部代码单元（含箭头函数）另设上限 1024；调用深度仍为 64。

## 6. 预置库（prelude.js）

规则：**Rust 原生内置永远不回调 JS。** 凡是需要调用回调的内置方法，都用 JS 写在 `src/agent_program/prelude.js`，由同一编译器编译成 `internal` 函数。这样回调就是普通调用：回调里调工具或虚函数，挂起、存盘、恢复都和普通代码一样。QuickJS 在这一块是在 C 循环里调回调，不能挂起，所以这里不学它。

预置库按进程编译一次，运行时按 id 链接。Program 记录预置库源码的哈希；续跑时哈希与当前构建不一致就拒绝，与语言版本采用同一策略，快照里不必重复保存预置库代码。

首批内容：

- `Array.prototype` 的 map、filter、forEach、find、findIndex、findLast、findLastIndex、some、every、reduce、reduceRight、flatMap，以及带比较函数的 sort、toSorted（稳定归并排序）
- `Array.from(items, mapFn)`
- `String.prototype.replace`、`replaceAll` 的函数替换
- `Promise.all`、`allSettled`、`race`、`any`，`Promise.prototype.catch`、`finally`（建立在原生 `then` 之上）

预置库函数把接收者作为第一个参数，不需要 `this`。`$$` 开头的是只对预置库开放的原生内部函数，用户源码里出现 `$$` 开头的名字直接编译报错：

```js
function map(array, callback) {
  const length = $$length(array);
  const result = $$newArray(length);
  for (let i = 0; i < length; i++) {
    if ($$has(array, i)) result[i] = callback(array[i], i, array);
  }
  return result;
}
```

随之删除编译期特例：`callback_method`、`array_callback`、`merge_sort`、`promise_callback`、`awaited_promise_chain`，以及 `Effect::ParallelReads`、`Effect::ParallelReadMap`。

## 7. async/await 与 Promise

### 7.1 协程

```rust
struct Coroutine { frames: Vec<Frame>, completion: Option<HeapId>, origin: Vec<HeapId> }
struct Frame { function: FunctionId, pc: usize, env: HeapId, temps: Temps, handlers: Vec<Handler>, destination: Option<Temp> }
```

- 模块顶层就是一个 async 协程，支持顶层 await；旧式 `function main(input)` 入口照常可用。
- 调用 async 函数：新建协程，同步运行到第一个 `await`。`completion` 是交给调用方的 Promise：函数返回时兑现，异常离开协程时拒绝。
- `await v`：把 `v` 转成 Promise，把“恢复本协程”登记为它的反应，协程挂起。`await` 只能出现在 async 函数自己的帧里（解析器保证），所以因 `await` 挂起的协程只有一帧。
- 同步宿主调用（同步虚函数、`host:<库>` 工具、fork、join）等待时，保留当前协程的整个调用栈，和现在一样。按 JS 同步调用的语义，等待期间不运行其他任务；已经在后台进行的异步调用照常进行，它们的结果等这次调用返回、当前同步代码执行完后再处理。
- `origin` 记录创建它的那串用户帧的环境，只用于第 8 节的 AI 可见作用域。

### 7.2 Promise 与任务队列（promise.rs）

- Promise 的状态和反应存在堆上。`promise.rs` 保留 FIFO 任务队列，以及“结算时把反应排进队列、重复结算不再排队”的规则（现有测试保留）。
- 反应有两种：恢复某个协程；调用某个回调，并用其结果结算派生的 Promise（`then`）。用 Promise 结算 Promise 时按规范跟随它的结果。
- 原生只实现 `then`、`Promise.resolve`、`Promise.reject`、`new Promise(executor)`；组合子写在预置库里。
- 调度循环：运行当前协程，然后依次取任务队列里的任务，直到空闲；空闲时把新发出的效果交给宿主。

### 7.3 同步与异步的宿主调用

- 同步调用（同步虚函数、`host:<库>` 工具、fork、join）：运行时登记一条等待中的效果并停在这条指令，结果回来后写入 `dest` 继续执行，与现在相同。同一时刻最多一条。
- 异步调用（`async` 虚函数、`host:<库>/promises` 工具）：运行时登记挂起效果 `{action, promise}`，调用立即得到 pending Promise，代码继续执行，遇到 `await` 才等待。
- 多个异步效果可以同时挂起：`inspect` 声明为 `async` 时，`await Promise.all(pages.map(p => inspect(p)))` 一次发出全部 AI 调用。
- 结束规则：顶层执行完、任务队列为空、没有挂起效果时程序结束（同 Node）。结束时仍未被处理的 rejection 让程序失败，报原错误；先发起、后 await 的写法中途失败时不提前报错，在 await 处抛出。顶层仍在 await，但已经没有任务和挂起效果时，报“等待一个永远不会完成的 Promise”并失败。
- 上限：同时挂起的效果最多 64 个；指令预算从每个运行时 10 万条提高到 100 万条（预置库让同样的工作多出一些指令）。

### 7.4 写法

`@virtual` 函数是否 `async` 由作者按需要声明。工具默认同步；需要并发时导入异步版本，同 Node 的 `fs` 与 `fs/promises`：

```js
import { read, write } from "host:io";
import { read as readAsync } from "host:io/promises";

/** @virtual 按 request 设计一页 HTML，返回 {content:string} */
function compose(request) {}

/** @virtual 检查一页 HTML，返回 {passed:boolean, evidence:string[]} */
async function inspect(html) {}

const draft = compose(input);                        // 同步：AI 返回后才继续
write({ path: input.output, content: draft.content });
const pages = await Promise.all(input.paths.map(path => readAsync({ path })));
const reviews = await Promise.all(pages.map(page => inspect(page.content)));  // 多个 AI loop 同时进行
```

- 现在的写法（同步调用，写不写 `await` 都可以）在版本 14 中继续有效。
- 防止对异步调用漏写 `await`：编译期拒绝直接对未 await 的异步调用取成员、调用方法（`then`/`catch`/`finally` 除外）、参与运算、作为条件或 assert 实参、放进模板字符串，例如 `readAsync(x).content`、`inspect(x).passed`、`if (inspect(x))`。识别范围是未被局部名遮蔽的顶层 async 函数和 `host:<库>/promises` 工具；局部 async 函数值只由运行时检查。运行时读取 Promise 上不存在的属性时，报“这是 Promise，可能漏了 await”。这是唯一一处有意偏离 JS（JS 在这里返回 undefined）。

## 8. AI 可见作用域（父作用域继承）

JS 代码按词法作用域执行；AI 看到的仍是**调用链**上的变量快照，保持现有行为（包括测试 `virtual_task_can_see_live_bindings_from_ordinary_caller_frames`）：

- 发起虚函数调用时，从当前协程栈顶往下逐帧收集，每帧沿自己的环境链取变量，内层遮蔽外层；跳过 `internal` 帧、临时量和仍在 TDZ 中的名字。
- 跨协程：收集完当前协程后接着走它的 `origin`。所以在 async 函数里调用虚函数时，AI 也能看到调用这个 async 函数的那一层的变量。
- 快照导出为 JSON：`scope_variables` 只列名字、类型和大小，`read_scope` 按需读取；函数、Promise 等非 JSON 值显示为不可读的占位。
- `define_task_body` 补写体和 fork 子程序：继承的快照作为最外层环境，像参数一样是本地副本，修改不回传；顶层函数声明按代码重新建闭包。
- 补写体的异常未被捕获时，在异常离开协程前保存当时的调用链快照，供 `FUNCTION_BODY_FAILED` 恢复使用，替代现在“保留失败帧”的做法。

## 9. 宿主接口（runtime.rs ↔ program_flow.rs）

```rust
pub enum State { Actions(Vec<Action>), Returned(Value), Failed(Value) }
// Runtime.pending: BTreeMap<ActionId, Action>，取代 Option<Action>
fn resume(&mut self, id: &str, value: Value, failed: bool) -> Result<()>;
```

- `next()` 运行到停下（遇到同步效果，或空闲），按发出顺序返回所有尚无回执的效果，每条标明同步还是异步；`resume` 按 id 交回结果：同步效果写回 `dest` 继续执行，异步效果结算对应的 Promise。
- `recover()` 按事件序号依次消费所有已记录的回执，恢复顺序与实际完成顺序一致。
- `single_agent.rs`、`task_frames.rs` 中 `pending.as_ref()` 加 `a.id == call_id` 的判断，改为 `pending.get(call_id)`。
- 虚函数统一成一种动作 `Call { name, args }`，去掉 `ActionKind::Task` 和 `scoped_calls`；去掉 `ActionKind::ParallelReads`。

**第一阶段派发（M5，宿主串行）**：等待中的只读工具（read、read_lines、search、list）合成一次 `parallel_read_batch`，每批最多 32 项，并按项分别成功或失败（现在一项失败整批失败）。其他效果按发出顺序逐个派发；虚函数仍走现在的子任务窗口。语言语义已经是并发的，宿主暂时逐个执行：结果正确，只是虚函数还不并行。

**第二阶段（M6，虚函数真正并行）**：一个父程序同时有多个子任务窗口，各自运行 AI loop，完成后把结果记为对应调用的回执；父程序收到任意一个回执就推进；模型并发队列（默认 3 个）限流；取消和重启恢复覆盖所有在运行的子窗口。这一步涉及 `single_agent.rs` 主循环和 `task_frames.rs` 的 create/finish，需要单独出宿主设计；运行时接口不再变化。

## 10. 存盘与版本

- `Runtime` 继续整体序列化：程序、预置库哈希、堆、协程、任务队列、挂起效果、预算。
- `VERSION = 14`；版本 13 及更早的现场拒绝续跑，沿用现有策略。

## 11. 测试

- 每个里程碑先写失败用例再实现。
- 每个用例都做一次“存盘 → 反序列化 → 继续”，确认环境、闭包、协程跨存盘仍然正确。
- 从 QuickJS 的 `tests/test_closure.js`、`test_loop.js`、`test_language.js`、`test_builtin.js` 挑闭包、作用域、循环、Promise、数组回调相关的用例，配一个简单的断言头来跑（QuickJS 是 MIT 许可，移植的用例保留其版权声明）。
- 验收：`acceptance-flow.model.js` 不改一字通过编译，并在夹具提供的回执下跑完。
- 现有 `agent_program::tests` 和 `documented_examples` 按新写法更新；最后用真实模型原样运行 serial.js 和 parallel-five-pages.js。

## 12. 里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M0 | 修 `tests.rs` 中两处已删除的 `values::method` 调用，让 v13 测试能编译，跑一遍记录基线 | 基线结果 |
| M1 | 环境链、块级作用域、var 提升、TDZ、每轮迭代绑定、catch 作用域；顶层函数按词法看模块变量；去掉 `captures`；AI 快照改为按调用链收集 | 重复 `let i`、两个 `catch (error)`、遮蔽、TDZ、调用链快照用例 |
| M2 | 函数值与闭包、嵌套函数、`Call`/`CallMethod`/`Construct`、Native 值（工具绑定、内置函数） | 计数器闭包、循环中捕获 `i`、函数存入数组再调用 |
| M3 | 预置库；删除编译期内联回调 | 多语句回调、嵌套回调、回调里调用工具并跨存盘恢复；五页流程编译通过并跑完 |
| M4 | 解构（声明、参数、catch）、默认参数、剩余与展开、switch、finally、带标签的 break/continue、for-in、`f?.()`；只改编译器 | 对应用例，例如 `Object.entries(x).map(([k, v]) => …)` |
| M5 | 协程、await、Promise、任务队列；`async` 虚函数和 `host:<库>/promises` 工具返回 Promise；多个挂起效果；宿主串行派发 | Promise 执行顺序、未处理 rejection、Promise.all 读文件并发、同步调用行为不变 |
| M6 | 虚函数真正并发（宿主） | promise-virtual-tasks.md 中“回家后的验证”清单；用 async 版五页示例确认多个 AI loop 同时进行 |
| M7 | 删除 fork/join、子 Agent 与 Git 工作树整合；两个并行示例改写为 async 虚函数（第 15 节） | 改写后的示例并行跑通；没有代码再引用 fork/join |
| M8 | 手册、提示词、Skill、示例更新到版本 14；真实模型复测 | 两个原样示例与五页验收流程 |

## 13. 改动文件清单

| 文件 | 改动 | 里程碑 |
|---|---|---|
| `src/agent_program.rs` | Function、Op、Expr、Effect 类型；`VERSION = 14` | M1–M5 |
| `src/agent_program/compiler.rs` | 作用域分析与解析；函数代码单元；调用指令；删除内联回调与 Promise 特例；M4 语法 | M1–M4 |
| `src/agent_program/heap.rs` | Environment 增加 `uninitialized`；Native、Promise 节点；去掉 `VmValue::Promise` | M1、M2、M5 |
| `src/agent_program/vm_eval.rs` | 变量走环境链与临时量；`Expr::Function` | M1、M2 |
| `src/agent_program/runtime.rs` | 帧（env、temps）、调用分派、协程、调度循环、挂起效果表、按 id 恢复、调用链快照 | M1–M5 |
| `src/agent_program/promise.rs` | 堆上 Promise 与任务队列；then、resolve、跟随 | M5 |
| `src/agent_program/js_methods.rs` | 方法分派到预置库；移出带回调的方法；`$$` 内部函数 | M3 |
| `src/agent_program/prelude.js`、`prelude.rs`（新增） | 预置库源码、编译与链接 | M3、M5 |
| `src/agent_program/tests.rs` | M0 修复；各里程碑新用例 | 全部 |
| `src/single_agent/program_flow.rs` | 多个挂起效果、按 id 恢复、只读合批、动作统一；删除 Fork/Join 派发与恢复 | M5、M7 |
| `src/single_agent.rs` | 按 id 查挂起效果；`parallel_read_batch` 按项返回成败；删除 spawn_agent、join_agents、子 Agent 工具和 `run_delegate` 等 | M5、M7 |
| `src/single_agent/task_frames.rs` | `create_function_task` 按 id 查挂起效果；并发子窗口；删除 `delegate_context` | M5–M7 |
| `src/single_agent/function_scope.rs` | 快照来自新的调用链收集 | M1 |
| `src/collaboration_jobs.rs`、`src/delegate_git.rs` | 子 Agent 与 Git 工作树：删除；文件写入协调移到 `workspace_changes.rs` | M7 |
| `src/agent_program/libraries.rs`、`src/workspace_changes.rs` | 去掉 spawn_agent/join_agents 保留名与 `host.inspect_agent_changes`、`host.merge_agent_result`；接收 `resource_key`、`claim_resource` | M7 |
| `docs/agent-program.md`、`docs/promise-virtual-tasks.md`、`prompts/flow_capabilities.md`、`prompts/flow_environment.md`、`skills/task-program/SKILL.md` 与 `examples/*.js`、`src/single_agent/documented_examples.rs` | 版本 14 的写法与语义（各里程碑同步修改受影响的段落）；M7 删除 fork/join 相关章节并改写并行示例 | M1–M8 |

## 14. 已确认的决定（2026-10-29）

1. 同步与异步由函数声明决定：`@virtual` 只表示函数体由 AI loop 实现；不写 `async` 是同步调用，直接得到返回值；写了 `async` 返回 Promise，可以并行。
2. 工具默认同步，保持现在的写法；需要并发时从 `host:<库>/promises` 导入异步版本。
3. 普通函数改为词法作用域，不再直接读调用方的局部变量；AI 仍看调用链快照。
4. 普通函数的参数个数按 JS 放宽；虚函数和 fork 目标仍严格。
5. 里程碑顺序：先 M1–M3（模型写的代码能跑），再 M4 语法，再 M5 异步。
6. M6 完成后，用 async 虚函数取代 fork/join，删除子 Agent 与 Git 工作树整合（第 15 节）。

## 15. 用 async 虚函数取代 fork/join（M7）

### 15.1 为什么可以取代

fork/join 要解决的是“几份 AI 工作同时进行”。async 虚函数加 `Promise.all` 能做到同样的事，而且更灵活：粒度随意，不必事先分成几路；结果直接返回；失败走 `await` 和 `try/catch`；不需要句柄、join 和分支整合。每个 async 虚函数调用本来就是独立的 AI loop，有自己的上下文。

正常 JS 写法里，并发任务各写各的文件；目录页、汇总这类共享文件，在 `Promise.all` 拿齐结果后统一写一次。流程代码本身不会制造对同一文件的并发写入。

### 15.2 前提与时机

- M6 完成、并用五页示例确认多个 AI loop 确实同时进行之后，才删除 fork/join。M6 之前 fork 是唯一的并行方式。
- 并发不再需要单独的“多 Agent 授权”：同时进行的 AI loop 数由模型并发队列限制（默认 3 个，`CODEX_AGENT_MODEL_CONCURRENCY` 可调）。
- 虚函数的补写体里还能再调用 async 虚函数，嵌套深度受现有临时任务层数上限（64）约束。fork 时代“子 Agent 不能再 fork”的限制随之取消。

### 15.3 同一文件的并发写入

- 文件基线按任务保存，同一任务内所有 AI loop 共用一份；宿主只拦截任务之外的改动。
- 两个并发 AI loop 改同一文件时：整文件 `write` 后写的覆盖先写的，不报冲突；`edit` 基于最新内容应用，原文对不上时报普通的修改错误。
- 约定：虚函数的职责描述写明只改自己的文件，或只返回内容、由父流程写入。
- 兜底在 M6 决定：整文件写入前，如果发现该文件已被同任务的其他 AI loop 改过，按冲突停下。

### 15.4 示例改写

`parallel-five-pages.js` 改写为：

```js
// 五页同时设计，写入后逐页读回，再同时验收；没有子 Agent，也没有分支合并。
import * as io from "host:io";
const request = input;

const source = io.read({path: request.source});
const shared = {source_path: request.source, source_content: source.content, requirements: request.requirements};

const designs = await Promise.all(request.pages.map(page => designPage(shared, page)));
for (let i = 0; i < request.pages.length; i++) {
  io.write({path: request.pages[i].path, content: designs[i].content});
}

const reviews = await Promise.all(request.pages.map(async page => {
  const actual = io.read({path: page.path});
  return inspectPage(shared, page, actual.content);
}));
for (const review of reviews) {
  assert(review.passed, "验收失败：" + review.path);
}

/** @virtual */
async function designPage(shared, page) {
  // shared 含真实源材料与公共设计要求，page 含本页标题、重点和输出路径。
  // 只设计本页并返回 {content:string}；不写文件，由调用方写入。
}

/** @virtual */
async function inspectPage(shared, page, content) {
  // 对照 shared 和 page 检查真实读回的正文。
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
}
```

`parallel-twenty-html.js` 同样改为按页 `Promise.all`：先同步生成共享 CSS，再并发设计各页，由主流程统一写入和验收。

### 15.5 删除与保留

删除（M7 动手前按 M6 的宿主实现再核对一遍）：

- 语言：`fork`、`join`，即 compiler.rs、runtime.rs 中的 Fork/Join 效果，以及 compiler.rs、libraries.rs 中 spawn_agent/join_agents 的保留名单。
- 宿主的程序路径：program_flow.rs 的 Fork/Join 派发与恢复；single_agent.rs 中 spawn_agent、join_agents 的处理和对挂起 Fork/Join 的特判。
- 子 Agent 机制：collaboration_jobs.rs 的提交、等待、join、交付、审阅、检查、合并、取消，以及公开给模型的 wait_agents、review_agent_result、inspect_agent_changes、merge_agent_result、cancel_agent；delegate_git.rs（Git 工作树）；single_agent.rs 的 `run_delegate`、`has_delegates`、`deliver_delegates`；task_frames.rs 的 `delegate_context`；库方法 `host.inspect_agent_changes`、`host.merge_agent_result`。
- 文档与示例：docs/agent-program.md 中“两个 Agent 并发制作五页”“已知循环与授权并发”“Git 分支产物的整合”等章节；SKILL.md 和提示词里的 fork/join 说明；documented_examples.rs 中 fork 相关用例改为 async 版本。

保留：

- 文件写入协调：collaboration_jobs.rs 中的 `resource_key`、`claim_resource` 移到 workspace_changes.rs，M6 的并发 AI loop 也要用。
- 任务取消：`cancel_all` 改为取消正在运行的子任务窗口。
- 前端的子 Agent 展示保留，用于查看旧记录；新运行不再产生子 Agent。
- 如果 M6 的并发子窗口复用了 `run_delegate` 的执行方式，保留被复用的部分。

## 16. 进度与下一步（2026-10-29）

### 16.1 已完成

| 里程碑 | 实际改动 |
|---|---|
| M0 | `tests.rs` 两处 `values::method` 改为 `js_methods::call_method`，测试目标恢复编译 |
| M1 | 堆环境带 TDZ（`heap.rs`）；`EnterScope`/`LeaveScope`/`CopyScope`/`Declare` 指令；编译器作用域栈取代平铺变量表；var 提升、catch 作用域、`for (let …)` 每轮新绑定；去掉调用点拷贝变量（`captures`）；AI 可见快照改为沿调用链各帧的环境链收集；版本号 14 |
| 旧测试 | JSON 边界按 JSON.stringify 处理 undefined 和函数（对象中省略、数组中为 null，位置实参为 null）；路径/URL 报错不再重复前缀；9 个测试的断言改为 JS 语义 |
| M2 | 嵌套函数声明（提升）、函数表达式、箭头、对象方法简写、具名函数表达式自引用；`Expr::Function` 建闭包，`Expr::Native` 表示内置函数和工具值；`Effect::Dynamic`、`Effect::Method` 在运行时按被调值分派（`Runtime::invoke`）；顶层函数作为闭包绑定在模块环境；普通函数实参按 JS 规则，@virtual 和 fork 目标仍严格；内联回调的参数改为块级作用域绑定，可嵌套回调和闭包 |
| M3 | 编译并链接 JS 预置库，以普通可恢复函数帧执行数组回调、比较函数及字符串替换函数；记录预置库哈希并检查恢复兼容性；删除编译器回调内联；五页流程在夹具回执下运行完成 |

测试现状：`cargo test agent_program::` 106 个全过；全量 351 过、18 失败、11 忽略。18 个失败均为改动前就有的环境类测试（attachments 6、browser_control 4、collaboration_jobs 2，agent_service、interjections、memory_panel、plugin_sessions、request_context、skill_packages 各 1）。`single_agent::tests::independent_file_writes_overlap_while_same_file_writes_queue` 是计时测试，全量并发时偶尔失败，单独运行通过。

### 16.2 实现要点（接手时先看）

- 函数 id：顶层函数用函数名；嵌套函数为 `外层id/名字@行:列`（匿名为 `<anonymous>`）。运行时在 `new_with_scope` 中把顶层函数作为闭包绑定到模块环境；嵌套函数声明在所在块或函数开头用 `Declare` 初始化。
- 变量：编译器临时量 `$tempN` 存在帧的 `vm_locals`；源码变量在帧的 `env` 环境链上，按名查找（`vm_eval::Locals`）。
- 调用：编译期能确定的（顶层函数名、导入的工具、`lib.method`、fork/join）仍走 `Effect::Call`/`Effect::Tool`；其余函数值调用走 `Effect::Dynamic`，`obj.name()` 走 `Effect::Method`（先查函数属性，再查内置方法）。未知方法名在运行时抛 TypeError。
- 回调：`map`/`filter` 等由 JS 预置库执行，旧的编译期内联回调已删除；预置库调用和回调使用可存盘的普通函数帧。

### 16.3 M3 实现内容与验收

1. 新增 `src/agent_program/prelude.js` 与 `prelude.rs`：用 JS 写 map、filter、forEach、find、findIndex、findLast、findLastIndex、some、every、reduce、reduceRight、flatMap、带比较函数的 sort/toSorted、`Array.from(items, mapFn)`、replace/replaceAll 的函数替换；接收者作为第一个参数，`$$` 开头的原生内部函数只对预置库开放。
2. 预置库按进程编译一次，Program 记录预置库源码哈希，续跑时哈希不符就拒绝。预置库函数标记为 internal，AI 可见快照跳过它们的帧。
3. `Effect::Method` 遇到数组接收者的这些方法名时调用预置库函数；删除编译期内联回调的代码。
4. 验收：多语句回调、把函数变量作为回调（如 `.filter(Boolean)`）、回调里调用工具并跨存盘恢复；`fixtures/model-five-page-acceptance.js` 在夹具回执下跑完。

### 16.4 下一步：M4

已实现声明、普通函数参数和 catch 的解构与默认值、解构剩余项、字面量展开、switch、标签跳转、简单绑定的 for-in 和可选函数调用。函数剩余参数、调用参数展开及 finally 已接入，并覆盖跨存盘恢复。非简单参数在独立的参数作用域中初始化，再进入函数体作用域；默认值中的闭包保留参数环境。M4 主体已实现，继续核对边界与快照兼容性后推进 M5 的 Promise 调度。

### 16.5 M5–M8 进度

普通 async 函数、Promise、await、FIFO 回调、通用组合子和异步工具已接入；并发只读请求合批并逐项结算。普通 thenable 同化和提取 Promise 方法值仍不支持；同步工具调用按原顺序执行；少量旧快照变体暂保留兼容。

async 虚函数使用独立窗口、调用时作用域快照和按调用 ID 的持久化回执；父流程收到一个结果即可继续，其他调用可仍在运行。模型请求共享三个并发槽位；中断时结果未知的调用不会自动重放。并发窗口对同一文件的整文件写入发生冲突时保留待写副本。

fork/join 不再允许出现在新源码中，旧子 Agent 和 Git 工作树执行模块已移除；历史记录不删除。五页、二十页示例已改为 async 虚函数，均有真实宿主和受控模型回执的端到端测试。五页示例在隔离目录使用配置的真实模型原样运行：一次十次虚函数请求完成、五页存在且无缺失本地链接；后续三次五页均生成但第 3 页的 AI 检查未通过，程序按断言停止；发现的问题包括内容缺项、日期判断和示例语法与源材料不符。一次检查包含浏览器诊断，但尚无完整视觉验收。补充源文档版本语法约束后，最终一轮真实模型运行完成：11 次模型请求，五页读回验收全部通过，独立检查没有缺失本地链接。串行示例也已完成真实模型复测；自动回归 425 项通过、10 项忽略（包括需真实模型的测试）。
