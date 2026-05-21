# codex-workspace-mcp

一个给 Codex 和其他 Agent 用的本地工作区 MCP 服务。

它不是单纯的文件读写包装，而是把项目整理成三层能力：

- 文件工具：读、写、搜、列目录
- 代码索引：Go / TS / JS 的符号、注释、直接调用、建议继续阅读的符号
- 项目记忆：把每次改了什么、为什么改、还剩什么风险记下来

目标很直接：让 Agent 少靠 shell，少靠全文搜索，少靠猜。

## 现在能做什么

- 读取和修改工作区里的文件
- 搜索文本
- 列目录
- 维护 Go 符号索引
- 维护 TS / JS 符号索引
- 自动识别直接调用、caller、callee、suggested reads
- 记录工作记忆，按工作区保存改动总结

## 为什么要做这个

传统 shell 在 Windows 上很吵：

- 编码麻烦
- 路径转义麻烦
- 搜索结果不稳定
- 读文件要绕很多层命令

这个服务想做的是：

```text
Agent -> MCP -> Rust -> 文件 / 索引 / 记忆 -> 结构化结果
```

让 AI 拿到的是坐标，不是大段噪音。

## 设计思路

### 1. 共享一个服务，多项目复用

`workspace_root` 由调用方显式传入，服务不靠“启动目录幻想”。

### 2. 工具自解释

Go / TS / JS 的 `list_*_symbols` 和 `search_*_symbols` 会在索引缺失时自动建索引。

### 3. 轻量而不是重型

这里做的是“类 AST”代码地图，不是完整编译器语义分析。

保留最有用的信息：

- 符号在哪里
- 符号上方写了什么
- 谁直接调用了它
- 它直接调用了谁
- 下一步最值得继续读哪个符号

### 4. 写后自动刷新

对 Go / TS / JS 文件的写操作完成后，会自动刷新对应索引。

### 5. memory 记录项目隐性规则

适合记这种东西：

- 包名命名规则
- 接口路径和目录映射规则
- 某个项目的约定写法
- 反复踩坑的地方

## 工具一览

### 基础文件工具

- `workspace_info`
- `list_dir`
- `read_file`
- `read_file_lines`
- `search_text`
- `write_file`
- `replace_range`

### Go 索引

- `go_index_status`
- `index_go_workspace`
- `list_go_symbols`
- `search_go_symbols`
- `read_go_symbol`

### TS / JS 索引

- `ts_index_status`
- `index_ts_workspace`
- `list_ts_symbols`
- `search_ts_symbols`
- `read_ts_symbol`

### 工作记忆

- `record_work_memory`
- `list_work_memory`
- `search_work_memory`

## 推荐用法

先查索引，再读符号，再改文件。

例如：

1. `search_go_symbols`
2. `read_go_symbol(include_context=true)`
3. `replace_range`
4. 自动刷新索引
5. `record_work_memory`

TS / JS 也是同样流程。

## 启动

```powershell
cargo run
```

默认监听：

```text
http://127.0.0.1:3000/mcp
```

可以通过环境变量覆盖：

- `WORKSPACE_ROOT`
- `MCP_BIND`

## 适合什么项目

- Go 后端项目
- TS / JS 前端项目
- 需要频繁跨文件理解和修改的代码库
- 本地 Agent 工作区

## 不适合什么

- 追求完整类型推导和编译器级语义的场景
- 超大规模多人并发数据库系统
- 需要远程多租户权限管理的生产后台

## 现在的状态

这个服务还在持续进化，但核心方向已经很明确：

- 让 Agent 少走 shell
- 让代码结构可导航
- 让项目经验能沉淀

## 一句话

这是一个给 AI 用的本地工作区底座。

