//! A fixed JavaScript-shaped subset is the flow language, not a JS/TS runtime.
//! SWC compiles it to serializable control flow; effects use the normal host tools.
mod builtins;
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
    // Preserve the serialized variant name for existing program snapshots;
    // the built-ins are now implemented directly in Rust.
    NodeBuiltin(String, Vec<Expr>),
    Template(Vec<Expr>),
    ParseJson(Box<Expr>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    Tool(String, Vec<Expr>),
    ParallelReads(Vec<(String, Vec<Expr>)>),
    ParallelReadMap {
        name: String,
        items: Expr,
        parameter: String,
        args: Vec<Expr>,
    },
    Call(String, Vec<Expr>),
    Fork(String, Vec<Expr>),
    Join(Vec<Expr>),
    Task {
        name: String,
        description: String,
        input: Expr,
    },
}

#[cfg(test)]
pub const SKILL: &str = include_str!("../skills/task-program/SKILL.md");
pub const ENVIRONMENT: &str = include_str!("../prompts/flow_environment.md");
pub const CAPABILITIES: &str = include_str!("../prompts/flow_capabilities.md");

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"execute_program",
        "description":"Compile and execute the complete top-level flow for the current task. Pass JS source and optional input; the flow ends when its statements finish. Ordinary functions and tools execute in the host; @virtual functions invoke AI. Import tools from host:<library>. Compilation errors return diagnostics before effects; correct and resubmit in the same task. Call alone. Successful execution does not imply business acceptance.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

/// Optional preflight using exactly the install compiler and current scope.
pub fn check_tool() -> Value {
    json!({"type":"function","function":{"name":"check_program",
        "description":"Check complete flow source without executing tools or changing the current task/program. Uses the same compiler, fixed libraries, permissions, input binding and @virtual rules as execute_program or define_task_body. In a virtual task, check the concrete current function implementation (no main); otherwise check the complete top-level program. Optional preflight, not a mandatory extra step: submission always compiles too. On a compile error, use its diagnostics to rewrite the complete source and resubmit in this same scope; do not recreate tasks, repeat earlier operations or claim completion. Passing a check does not execute or prove business acceptance.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

pub fn body_tool() -> Value {
    json!({"type":"function","function":{"name":"define_task_body",
        "description":"Implement the current virtual function using its existing name, positional parameters and return contract. Submit concrete source without @virtual on this function; helper AI capabilities may be marked @virtual. It runs in the current scope and returns to the existing caller. Preserve completed work. Import authorized tools and read their contracts when needed. Compilation errors return diagnostics before effects; correct in this scope. Call alone.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000}},"required":["source"],"additionalProperties":false}}})
}
