# 核心引擎重构：多语言自愈与 Git 式寻址网关实现计划

为了消除当前在 `expert_surgery.rs` 中对 Rust (`cargo check` 和 `syn`) 的硬编码耦合，并将专家模型的输出映射到多语言架构，本计划将在底层实现无状态、多语言兼容的编译检查与代码寻址网关。

## Open Questions

> [!WARNING]
> 1. **TS Verification Command**: What should be the default command for verifying TypeScript files if `tsc` is not globally available? Should we default to `npx tsc --noEmit`?
> 2. **Retry Loop Limitation**: In Phase 3, you mentioned adding `for retry in 1..=MAX_RETRIES` inside `run_expert_code_surgery`. Currently, the retry loop is handled by the **outer Flash ReAct loop** via the `agent_runtime`. Do we want an inner retry loop in the surgery itself, or should we keep it completely stateless and let the outer Flash loop handle the retry by returning the AST diff errors?

## Proposed Changes

---

### Phase 1: 多语言包拓展与 AST 树比对 (TS/Rust)

为 `ts_index` 和 `rust_index` 引入语言专属的物理寻址和 AST 盲比机制。

#### [NEW] src/ts_index/diff.rs
- 实现 `relocate_ts_span(current_disk_content: &str, search_text: &str) -> Option<(usize, usize)>`：利用 `swc` 解析器校准漂移位。
- 实现 `diff_ts_symbols_ast(base_code: &str, attempted_code: &str) -> Vec<String>`：基于 `swc_ecma_visit` 在内存中对比两棵 AST 树。

#### [MODIFY] src/ts_index.rs
- 暴露 `diff` 模块。

#### [NEW] src/rust_index/diff.rs
- 实现 `relocate_rust_span`：复用 `conflict_resolver::find_unique_normalized_match` 的同时增加 syn 层级的保障。
- 实现 `diff_rust_symbols_ast`：利用 `syn::parse_file` 比较 Signature 和语句块。

#### [MODIFY] src/rust_index.rs
- 暴露 `diff` 模块。

---

### Phase 2: 独立策略封装与跨语言质检

将合并逻辑与回滚控制流分离成独立的纯函数。

#### [MODIFY] src/expert_surgery.rs
- **引入独立合并流**：新增 `async fn try_three_way_merge_and_resolve(...) -> anyhow::Result<String>`。根据扩展名动态分发到 `ts_index::diff` 或 `rust_index::diff`。
- **重构质检网闸**：替换原有的 `cargo check` 硬编码，新增 `async fn write_and_verify_by_language(...) -> Result<VerificationStatus, String>`。实现 100% 物理回滚。

---

### Phase 3: 自愈收敛循环

整合高内聚方法。

#### [MODIFY] src/expert_surgery.rs
- 修改 `run_expert_code_surgery`：通过注入 `write_and_verify_by_language` 来驱动无状态的跨语言修复。如果发生 AST 不匹配，向上抛出结构化 Error。

## Verification Plan

### Automated Tests
- 为 `ts_index::diff` 和 `rust_index::diff` 添加单独的单元测试，验证内存中的 AST 比对能准确捕获结构损坏。
- 执行 `cargo test` 确保解耦并未破坏现有的 `conflict_resolver` 链路。

### Manual Verification
- 故意对一个 TypeScript 文件下达错误的替换补丁，观察系统是否能在微秒级物理回滚，并正确通过 `tsc --noEmit` 或 `SWC` 提炼出报错。
