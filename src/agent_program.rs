//! A fixed JavaScript-shaped subset is the flow language, not a JS/TS runtime.
//! SWC compiles it to serializable control flow; effects use the normal host tools.
mod builtins;
mod compiler;
mod heap;
mod js_methods;
mod js_value;
mod vm_eval;
pub(crate) mod declarations;
pub(crate) mod libraries;
mod promise;
mod prelude;
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

pub const VERSION: u32 = 14;
/// Id of the function formed by a program's top-level statements. It is not
/// a JavaScript identifier, so no declared function can take it.
pub const ENTRY: &str = "<module>";
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
    #[serde(default)]
    pub prelude_hash: String,
    pub functions: BTreeMap<String, Function>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Function {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub virtual_task: bool,
    /// Outer variables a virtual function's contract names with `@uses`: its
    /// closure, read in the scope where it is declared when it is called.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// A virtual declared inside a function implementation is a leaf: a plain
    /// tool loop limited to the tools that implementation imported.
    #[serde(default)]
    pub leaf_tools: Option<Vec<String>>,
    pub params: Vec<String>,
    /// Index of the final rest parameter, if present.
    #[serde(default)]
    pub rest_parameter: Option<usize>,
    /// Non-simple parameters initialize in a separate environment.
    #[serde(default)]
    pub parameter_scope: bool,
    #[serde(default)]
    pub parameter_lexical: Vec<String>,
    /// Hoisted `var` names, undefined on entry.
    #[serde(default)]
    pub vars: Vec<String>,
    /// let/const names of the body's top level, in their dead zone on entry.
    #[serde(default)]
    pub lexical: Vec<String>,
    /// The top-level flow: its frame runs in the module environment itself.
    #[serde(default)]
    pub module: bool,
    #[serde(default)]
    pub is_async: bool,
    /// A named function expression sees its own name inside its body.
    #[serde(default)]
    pub self_name: Option<String>,
    pub code: Vec<Instruction>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instruction {
    pub at: Location,
    pub op: Op,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    /// Assign a compiler temporary, or an existing variable through the scope chain.
    Set(String, Expr),
    /// Initialize a let/const/var binding and end its dead zone.
    Declare(String, Expr),
    /// Enter a block scope whose let/const names start in their dead zone.
    EnterScope(Vec<String>),
    /// Enter the body environment after non-simple parameter initialization.
    EnterFunctionBody,
    LeaveScope,
    /// Fresh per-iteration copy of the current scope for `for (let …)`.
    CopyScope,
    SetMember { object: Expr, key: Expr, value: Expr },
    Effect(Option<String>, Effect),
    Append {
        target: String,
        items: Vec<Expr>,
        destination: String,
    },
    Await { value: Expr, destination: String },
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
    Undefined,
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
    RegExp(Vec<Expr>),
    /// A closure over the current scope for the function with this id.
    Function(String),
    /// A built-in function or tool as a value: "String", "Math.max", "tool:read_file".
    Native(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    PromiseCreate { reject: bool, value: Expr },
    PromiseConstruct { executor: Expr },
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
    /// Call a function value: a closure, a built-in or a tool.
    Dynamic { callee: Expr, args: Vec<Expr> },
    /// Internal prelude call using an array of arguments.
    Apply { callee: Expr, args: Expr },
    /// A method call with a computed name and an array of arguments.
    DynamicMethodApply { receiver: Expr, name: Expr, args: Expr },
    /// A method call whose arguments are supplied in an array.
    MethodApply { receiver: Expr, name: String, args: Expr, #[serde(default)] ignore_property: bool },
    /// `receiver.name(args)`: a function-valued property, else a built-in method.
    Method {
        receiver: Expr,
        name: String,
        args: Vec<Expr>,
    },
}

#[cfg(test)]
pub const SKILL: &str = include_str!("../skills/task-program/SKILL.md");
pub const ENVIRONMENT: &str = include_str!("../prompts/flow_environment.md");
pub const CAPABILITIES: &str = include_str!("../prompts/flow_capabilities.md");

