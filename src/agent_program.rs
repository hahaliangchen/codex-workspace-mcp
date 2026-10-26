//! A fixed JavaScript-shaped subset is the flow language, not a JS/TS runtime.
//! SWC compiles it to serializable control flow; effects use the normal host tools.
mod compiler;
pub(crate) mod libraries;
pub(crate) mod declarations;
pub(crate) mod runtime;
#[cfg(test)]
mod tests;
mod values;

#[cfg(test)]
pub use compiler::{compile, compile_body, compile_entry};
pub use compiler::{compile_body_with_libraries, compile_with_libraries};
pub use runtime::{Action, Runtime, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const VERSION: u32 = 9;
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
    pub comment_only: bool,
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
    // Decode old snapshots for explicit version rejection; never newly compiled.
    Ai(Expr),
    // Decode-only legacy AI interpretation; version 9 rejects this effect.
    Semantic {
        expression: String,
        description: String,
        bindings: Expr,
        boolean: bool,
        direct: Option<Expr>,
    },
    Task {
        name: String,
        description: String,
        input: Expr,
    },
}

pub const SKILL: &str = include_str!("../skills/task-program/SKILL.md");

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"execute_program",
        "description":"Execute the complete version 9 flow program described by task_program_skill. One ordinary main covers the authorized work, verification and delivery, using helper(args) and the published library.method(args), such as io.read({path}), using the generated program_tool_headers declarations. Each tool has one fixed source name; tools.* and model-defined library aliases are rejected. Header types describe JSON inputs and returns only, never put TS types in executable source. Bindings are host-owned and cannot expand permissions. Every helper is a task with local scope and a JSON return; tools are actual steps. A comment-only helper explicitly requests AI work and can implement its current body with define_task_body. Ordinary arithmetic, comparisons, logic, templates and whitelisted data methods are deterministic host computations, never implicit AI tasks. Use const/let/var, local assignments/updates, common for/while/do-while/for-of loops, break/continue, if/else, ternaries, assert, return and try/catch/throw. Logical and conditional expressions short-circuit; types are explicit and false, null and numeric zero are false; all other JSON values are true, including empty strings and containers. Logical operators return booleans and short-circuit; ?? selects the right side only for null. Business judgment belongs in an explicit inspection helper returning a boolean field and real evidence, not in operator interpretation. Pass shared inputs and distinct branch assignments explicitly. Authorized fork(helper,args) starts a child Agent and join waits for ordered results; child Agents cannot fork. Main return ends the task; helper return resumes its caller. Reject function*, yield, async/await, call()/ai(), modules, TS types, closures/classes and unlisted JS runtime features. Long generated payloads belong in task returns or saved assets. Compile all source before effects, preserve frames/locals/position and use existing permission/plugin boundaries. Call alone after task creation and missing-contract preparation. Type/compile errors are explicit; no AI fallback. Existing receipts are consumed without replay; older language snapshots cannot execute. Compilation and function return do not prove business acceptance.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

pub fn body_tool() -> Value {
    json!({"type":"function","function":{"name":"define_task_body",
        "description":"Implement the current task function using its program_task_input description and named actual arguments. Submit source declaring that same function name; use current argument names or one parameter receiving the complete named-input object, with its calls to tools and/or additional helper functions. AI-defined helper calls create child tasks; tool-library calls are steps. Empty helpers require explanatory comments; they can themselves refine their implementation. No new main: this implements the current function and returns its value to the existing caller. Keep orchestration source separate from long generated payloads: obtain those as helper return values or asset references, not giant source literals. Pass shared materials explicitly. Use the same restricted syntax as task-program skill. Use only the fixed method names and JSON signatures in program_tool_headers; tools.* and model-defined libraries are rejected. Call alone; after compilation the host executes the body in this existing task scope. Do not recreate completed operations: implement only the remaining work with their saved results.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000}},"required":["source"],"additionalProperties":false}}})
}
