//! A fixed JavaScript-shaped subset is the flow language, not a JS/TS runtime.
//! SWC compiles it to serializable control flow; effects use the normal host tools.
mod compiler;
pub(crate) mod declarations;
pub(crate) mod libraries;
pub(crate) mod runtime;
#[cfg(test)]
mod tests;
mod values;

#[cfg(test)]
pub use compiler::{compile, compile_body, compile_entry};
pub use compiler::{compile_body_with_scope, compile_with_libraries};
pub use runtime::{Action, Runtime, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const VERSION: u32 = 12;
// JavaScript has one number type; JSON tool schemas often require integer
// representations. Preserve integral values at the deterministic boundary.
fn number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= 0.0 && value < (u64::MAX as f64) {
        json!(value as u64)
    } else if value.fract() == 0.0 && value < 0.0 && value >= (i64::MIN as f64) {
        json!(value as i64)
    } else {
        json!(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    pub version: u32,
    pub source: String,
    pub functions: BTreeMap<String, Function>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Function {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub virtual_task: bool,
    #[serde(default)]
    pub captures: std::collections::BTreeSet<String>,
    pub params: Vec<String>,
    pub code: Vec<Instruction>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instruction {
    pub at: Location,
    pub op: Op,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    Set(String, Expr),
    Effect(Option<String>, Effect),
    Append {
        target: String,
        items: Vec<Expr>,
        destination: String,
    },
    Return(Expr),
    Branch(Expr, usize),
    Jump(usize),
    ForNext {
        items: String,
        index: String,
        item: String,
        end: usize,
    },
    Catch {
        target: usize,
        name: String,
    },
    EndCatch,
    Throw(Expr),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Expr {
    Value(Value),
    Variable(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Member(Box<Expr>, Box<Expr>),
    // Pure expressions are deterministic. Calls are lowered to durable effects.
    Binary(String, Box<Expr>, Box<Expr>),
    Unary(String, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Stringify(Box<Expr>),
    StringMethod(Box<Expr>, String, Vec<Expr>),
    Template(Vec<Expr>),
    ParseJson(Box<Expr>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    Tool(String, Vec<Expr>),
    Call(String, Vec<Expr>),
    Fork(String, Vec<Expr>),
    Join(Vec<Expr>),
    Task {
        name: String,
        description: String,
        input: Expr,
    },
}

pub const SKILL: &str = include_str!("../skills/task-program/SKILL.md");

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"execute_program",
        "description":"Execute the complete version 12 flow program described by task_program_skill. One ordinary main covers the authorized work, verification and delivery, using helper(args) and the published library.method(args), such as io.read({path}), using the generated program_tool_headers declarations. Each tool has one fixed source name; tools.* and model-defined library aliases are rejected. Header types describe JSON inputs and returns only, never put TS types in executable source. Bindings are host-owned and cannot expand permissions. Ordinary helpers execute in local Runtime frames and return JSON values without creating AI tasks; @virtual calls create AI task scopes and tools are actual steps. An AI task must be explicitly marked @virtual in its adjacent JSDoc or body comments, with its responsibility and return contract. Its body contains no executable statements or placeholder return; the executing AI produces the actual result, or implements this same function using define_task_body without @virtual. Ordinary functions execute their code and comments do not perform work. Ordinary arithmetic, comparisons, logic, templates and whitelisted data methods are deterministic host computations, never implicit AI tasks. For + and +=, a string operand selects concatenation of JSON primitives, including numbers, booleans and null; two numeric operands still add. Containers require JSON.stringify. Use const/let/var, local assignments/updates, common for/while/do-while/for-of loops, break/continue, if/else, ternaries, assert, return and try/catch/throw. Logical and conditional expressions short-circuit; types are explicit and false, null and numeric zero are false; all other JSON values are true, including empty strings and containers. Logical operators return booleans and short-circuit; ?? selects the right side only for null. Business judgment belongs in an explicit inspection helper returning a boolean field and real evidence, not in operator interpretation. Ordinary helpers can read caller-visible variables at entry; fork snapshots the parent scope. Pass distinct branch assignments explicitly; unused scope values are indexed for on-demand AI access. Authorized fork(helper,args) returns an Agent handle with agent_id and workspace (mode, path, integration_required); join waits for an array of ordered function results, never merges files. If handle.workspace.integration_required is true, inspect its Git changes and explicitly merge the inspected head with the advertised host tools before parent-workspace reads. Non-Git shared workspaces require no Git merge. Child Agents cannot fork. Main return ends the task; helper return resumes its caller. Reject function*, yield, async/await, call()/ai(), modules, TS types, closures/classes and unlisted JS runtime features. Long generated payloads belong in task returns or saved assets. Compile all source before effects, preserve frames/locals/position and use existing permission/plugin boundaries. Call alone after task creation and missing-contract preparation. Compilation errors are returned to this same AI with source diagnostics: rewrite complete source and resubmit here, keeping the task and prior results; no effects ran from the rejected source. Runtime type errors stay explicit; no implicit operator interpretation. Existing receipts are consumed without replay; older language snapshots cannot execute. Compilation and function return do not prove business acceptance.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

/// Optional preflight using exactly the install compiler and current scope.
pub fn check_tool() -> Value {
    json!({"type":"function","function":{"name":"check_program",
        "description":"Check complete flow source without executing tools or changing the current task/program. Uses the same compiler, fixed libraries, permissions, input binding and @virtual rules as execute_program or define_task_body. In a virtual task, check the concrete current function implementation (no main); otherwise check the complete main program. Optional preflight, not a mandatory extra step: submission always compiles too. On a compile error, use its diagnostics to rewrite the complete source and resubmit in this same scope; do not recreate tasks, repeat earlier operations or claim completion. Passing a check does not execute or prove business acceptance.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

pub fn body_tool() -> Value {
    json!({"type":"function","function":{"name":"define_task_body",
        "description":"Implement the current task function using the exact signature, description and local parameter values in program_task_input.bindings. Submit source declaring that same function name; keep its parameter count; actual arguments bind by position exactly as an ordinary function call, with no automatic packing or input envelope, with its calls to tools and/or additional helper functions. Ordinary helper calls execute in local Runtime frames; @virtual calls create AI tasks and tool-library calls are steps. AI helper declarations require @virtual plus explanatory comments and an empty body; no placeholder return. Remove @virtual from this current function when providing its concrete implementation; nested AI helper declarations retain @virtual. No new main: this implements the current function and returns its value to the existing caller. Keep orchestration source separate from long generated payloads: obtain those as helper return values or asset references, not giant source literals. Pass shared materials explicitly. Use the same restricted syntax as task-program skill. Use only the fixed method names and JSON signatures in program_tool_headers; tools.* and model-defined libraries are rejected. Call alone; after compilation the host executes the body in this existing task scope. Do not recreate completed operations: implement only the remaining work with their saved results. If compilation fails, rewrite and resubmit the complete current function source in this same scope using the returned line/column diagnostics. check_program is optional preflight; this submission always validates before effects.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000}},"required":["source"],"additionalProperties":false}}})
}
