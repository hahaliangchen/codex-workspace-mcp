//! TypeScript-shaped pseudocode is the reviewable flow language, not a JS VM.
//! SWC compiles it to serializable control flow; effects use the normal host tools.
mod compiler;
pub(crate) mod runtime;
#[cfg(test)]
mod tests;

pub use compiler::{compile, compile_body, compile_entry};
pub use runtime::{Action, Runtime, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const VERSION: u32 = 5;
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
    // Legacy variants remain decodable so old snapshots fail explicitly by version,
    // rather than being silently treated as a fresh task. They are never emitted.
    Binary(String, Box<Expr>, Box<Expr>),
    Unary(String, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Stringify(Box<Expr>),
    StringMethod(Box<Expr>, String, Vec<Expr>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    Tool(String, Vec<Expr>),
    Call(String, Vec<Expr>),
    Fork(String, Vec<Expr>),
    Join(Vec<Expr>),
    Ai(Expr),
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
        "description":"Execute the complete task pseudocode described by the supplied task-program skill. Use one ordinary function main with helper calls. Every AI-defined helper call is a task with its own scope and typed return, whether its body contains code or comments. An empty body requires comments for the executing AI, which may implement the same function with define_task_body and nested helper calls. Calls to host tools are execution steps. Keep source for orchestration: obtain long generated payloads from helper return values or saved assets instead of embedding them as giant source literals. A helper can also orchestrate tools.EXACT_NAME(args) and other helpers, with comments explaining those calls. fork(helper,args) starts an authorized child Agent; join(handles) waits and returns ordered values. Main return ends the task; helper return resumes its caller. Functions, JSON data, local assignment, if/else, assert and array for...of express flow. Descriptive comparisons, operators and data methods are interpreted by an AI task with named bindings and function comments, using actual tools when evidence is needed; the host consumes its explicit value and evidence, never executes JS operators or methods. Bind real tool/helper calls before placing their results inside semantic expressions. Boolean results advance directly; local array.push(value) deterministically collects task returns. The host compiles the whole source before effects, preserves source/frames/locals/position, and executes tools through existing permissions and plugin boundaries. Call alone after creating the task and preparing missing contracts. Checks and delivery belong in main's complete flow; syntax success does not prove business acceptance. No JS VM or arbitrary imports. Legacy generator/effect syntax remains readable for compatibility, but write ordinary function-call pseudocode for new tasks.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000},"input":{}},"required":["source"],"additionalProperties":false}}})
}

pub fn body_tool() -> Value {
    json!({"type":"function","function":{"name":"define_task_body",
        "description":"Implement the current task function using its program_task_input description and named actual arguments. Submit source declaring that same function name; use current argument names or one parameter receiving the complete named-input object, with its calls to tools and/or additional helper functions. AI-defined helper calls create child tasks; tool-library calls are steps. Empty helpers require explanatory comments; they can themselves refine their implementation. No new main: this implements the current function and returns its value to the existing caller. Keep orchestration source separate from long generated payloads: obtain those as helper return values or asset references, not giant source literals. Pass shared materials explicitly. Use the same restricted syntax as task-program skill. Call alone; after compilation the host executes the body in this existing task scope. Do not recreate completed operations: implement only the remaining work with their saved results.",
        "parameters":{"type":"object","properties":{"source":{"type":"string","maxLength":100000}},"required":["source"],"additionalProperties":false}}})
}
