use super::*;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use swc_common::{FileName, SourceMap, Span, Spanned, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast as ast;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

struct Compiler {
    map: Lrc<SourceMap>,
    names: BTreeSet<String>,
    arities: BTreeMap<String, usize>,
    async_entry: Option<String>,
    libraries: super::libraries::Libraries,
    imports: BTreeMap<String, String>,
    /// Lexical scopes, outermost first: module, function, then blocks. Each
    /// maps a declared name to whether it may be reassigned.
    scopes: Vec<BTreeMap<String, bool>>,
    /// Names inherited from the calling scope of task bodies.
    external: BTreeSet<String>,
    /// Compiler temporaries that rewritten source refers to by name.
    temp_names: BTreeSet<String>,
    /// Block scopes entered at run time at the current point of the code.
    runtime_scopes: usize,
    /// Id of the function being compiled; nested functions extend it.
    function_id: String,
    /// Nested functions compiled so far, including their own nested ones.
    nested: Vec<(String, Function)>,
    /// Top-level @virtual functions: their calls keep exact arity.
    virtuals: BTreeSet<String>,
    /// Top-level async functions, whose calls return a Promise.
    async_functions: BTreeSet<String>,
    code: Vec<Instruction>,
    temp: usize,
    loops: Vec<LoopContext>,
    handlers: usize,
    finalizers: Vec<FinallyContext>,
    /// Branches that leave the optional chain being compiled.
    chain_exits: Option<Vec<usize>>,
    /// The next lowered expression is the object of the current chain link.
    chain_object: bool,
    /// Header contracts of every function declaration, nested ones included,
    /// keyed by the position of the function.
    contracts: std::sync::Arc<BTreeMap<u32, (bool, String)>>,
    /// The tools a virtual declared here may use, when this is an implementation.
    leaf_tools: Option<Vec<String>>,
}

/// Global built-in functions that can be used as values, as in `.filter(Boolean)`.
const NATIVE_GLOBALS: [&str; 7] = [
    "String", "Number", "Boolean", "parseInt", "parseFloat", "isNaN", "isFinite",
];

/// Array methods whose callback is compiled into the calling frame.
const CALLBACK_METHODS: [&str; 14] = [
    "map", "filter", "forEach", "find", "findIndex", "findLast", "findLastIndex", "some", "every",
    "reduce", "reduceRight", "flatMap", "sort", "toSorted",
];

/// A numeric constant, including NaN and the infinities that JSON cannot hold.
fn number_expr(value: f64) -> Expr {
    if value.is_finite() {
        Expr::Value(number_value(value))
    } else {
        Expr::Unary(
            "+".into(),
            Box::new(Expr::Value(json!(super::js_value::number_to_string(value)))),
        )
    }
}

#[derive(Clone)]
struct FinallyContext {
    block: ast::BlockStmt,
    handlers: usize,
    runtime_scopes: usize,
    scopes_len: usize,
    loops_len: usize,
}

#[derive(Default)]
struct LoopContext {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    handlers: usize,
    scopes: usize,
    is_switch: bool,
    label: Option<String>,
    label_loop: bool,
}

/// Names bound by a declaration pattern, without default-value expressions.
fn pattern_names(pattern: &ast::Pat) -> Vec<String> {
    struct Names(Vec<String>);
    impl Visit for Names {
        fn visit_binding_ident(&mut self, node: &ast::BindingIdent) {
            self.0.push(node.id.sym.to_string());
        }
        fn visit_expr(&mut self, _: &ast::Expr) {}
    }
    let mut names = Names(Vec::new());
    pattern.visit_with(&mut names);
    names.0
}

/// let/const and function declarations directly in a statement list, with
/// whether each may be reassigned. Nested blocks have their own scopes.
fn lexical_names(statements: &[ast::Stmt]) -> Vec<(String, bool)> {
    let mut names = Vec::new();
    for statement in statements {
        match statement {
            ast::Stmt::Decl(ast::Decl::Var(v)) if v.kind != ast::VarDeclKind::Var => {
                for d in &v.decls {
                    for name in pattern_names(&d.name) {
                        names.push((name, v.kind == ast::VarDeclKind::Let));
                    }
                }
            }
            ast::Stmt::Decl(ast::Decl::Fn(f)) => names.push((f.ident.sym.to_string(), true)),
            _ => {}
        }
    }
    names
}

/// The parts of a function, arrow or method that the compiler needs.
struct FunctionSource<'a> {
    /// Its own name, used in its id and, for a named function expression,
    /// visible inside its body.
    name: Option<String>,
    self_name: Option<String>,
    params: Vec<&'a ast::Pat>,
    body: FunctionBody<'a>,
    is_async: bool,
    is_generator: bool,
    span: Span,
}
enum FunctionBody<'a> {
    Block(&'a ast::BlockStmt),
    Expr(&'a ast::Expr),
    Missing,
}
impl<'a> FunctionSource<'a> {
    fn function(f: &'a ast::Function, name: Option<String>, self_name: Option<String>) -> Self {
        Self {
            name,
            self_name,
            params: f.params.iter().map(|p| &p.pat).collect(),
            body: match &f.body {
                Some(body) => FunctionBody::Block(body),
                None => FunctionBody::Missing,
            },
            is_async: f.is_async,
            is_generator: f.is_generator,
            span: f.span,
        }
    }
    fn arrow(a: &'a ast::ArrowExpr) -> Self {
        Self {
            name: None,
            self_name: None,
            params: a.params.iter().collect(),
            body: match &*a.body {
                ast::BlockStmtOrExpr::BlockStmt(body) => FunctionBody::Block(body),
                ast::BlockStmtOrExpr::Expr(body) => FunctionBody::Expr(body),
            },
            is_async: a.is_async,
            is_generator: a.is_generator,
            span: a.span,
        }
    }
}

/// var names hoisted to the enclosing function, including those in nested
/// blocks and loop heads but not in nested functions.
fn var_names(statements: &[ast::Stmt]) -> Vec<String> {
    struct Vars(Vec<String>);
    impl Visit for Vars {
        fn visit_var_decl(&mut self, node: &ast::VarDecl) {
            if node.kind == ast::VarDeclKind::Var {
                for d in &node.decls {
                    for name in pattern_names(&d.name) {
                        if !self.0.contains(&name) {
                            self.0.push(name);
                        }
                    }
                }
            }
        }
        fn visit_function(&mut self, _: &ast::Function) {}
        fn visit_arrow_expr(&mut self, _: &ast::ArrowExpr) {}
        fn visit_class(&mut self, _: &ast::Class) {}
    }
    let mut vars = Vars(Vec::new());
    for statement in statements {
        statement.visit_with(&mut vars);
    }
    vars.0
}
impl Compiler {
    fn condition(&mut self, e: &ast::Expr) -> Result<Expr> {
        self.reject_unawaited(e, "using it as a condition")?;
        self.lower(e)
    }
    /// The name of a call known to return a Promise: a top-level async
    /// function or a host:<library>/promises tool not shadowed by a local.
    fn unawaited_call(&self, e: &ast::Expr) -> Option<String> {
        let mut e = e;
        while let ast::Expr::Paren(p) = e { e = &p.expr; }
        let ast::Expr::Call(call) = e else { return None };
        let ast::Callee::Expr(callee) = &call.callee else { return None };
        let shadowed = |name: &str| self.temp_names.contains(name) || self.scopes.iter().skip(1).any(|scope| scope.contains_key(name));
        match &**callee {
            ast::Expr::Ident(i) if !shadowed(i.sym.as_ref()) => {
                let name = i.sym.as_ref();
                let asynchronous = self.async_functions.contains(name)
                    || self.imports.get(name).is_some_and(|tool| tool.starts_with("async-tool:"));
                asynchronous.then(|| name.to_owned())
            }
            ast::Expr::Member(m) => {
                let (ast::Expr::Ident(library), ast::MemberProp::Ident(method)) = (&*m.obj, &m.prop) else { return None };
                if shadowed(library.sym.as_ref()) { return None; }
                let tool = self.libraries.get(library.sym.as_ref())?.get(method.sym.as_ref())?;
                tool.starts_with("async-tool:").then(|| format!("{}.{}", library.sym, method.sym))
            }
            _ => None,
        }
    }
    /// Using an un-awaited Promise as a value is almost always a missing
    /// await, and a Promise is always truthy, so such checks would pass.
    fn reject_unawaited(&self, e: &ast::Expr, usage: &str) -> Result<()> {
        match self.unawaited_call(e) {
            Some(name) => self.error(e.span(), &format!("{name}(...) returns a Promise; await it before {usage}")),
            None => Ok(()),
        }
    }
    /// The direct operands of `e` that must not be un-awaited Promises.
    fn reject_unawaited_operands(&self, e: &ast::Expr) -> Result<()> {
        let promise_method = |prop: &ast::MemberProp| matches!(prop, ast::MemberProp::Ident(p) if matches!(p.sym.as_ref(), "then" | "catch" | "finally"));
        match e {
            ast::Expr::Member(m) if !promise_method(&m.prop) => self.reject_unawaited(&m.obj, "reading its properties"),
            ast::Expr::Call(ast::CallExpr { callee: ast::Callee::Expr(callee), .. }) => match &**callee {
                ast::Expr::Member(m) if !promise_method(&m.prop) => self.reject_unawaited(&m.obj, "calling its methods"),
                _ => Ok(()),
            },
            ast::Expr::OptChain(chain) => match &*chain.base {
                ast::OptChainBase::Member(m) if !promise_method(&m.prop) => self.reject_unawaited(&m.obj, "reading its properties"),
                _ => Ok(()),
            },
            ast::Expr::Bin(b) => {
                self.reject_unawaited(&b.left, "using it in an operator")?;
                self.reject_unawaited(&b.right, "using it in an operator")
            }
            ast::Expr::Unary(u) if u.op != ast::UnaryOp::Void => self.reject_unawaited(&u.arg, "using it in an operator"),
            ast::Expr::Tpl(t) => t.exprs.iter().try_for_each(|e| self.reject_unawaited(e, "putting it in a template string")),
            _ => Ok(()),
        }
    }
    fn logical(&mut self, op: &str, left: Expr, right: &ast::Expr, span: Span) -> Result<Expr> {
        let left = self.snapshot(span, left);
        let result = self.temp();
        self.emit(span, Op::Set(result.clone(), left.clone()));
        let test = if op == "??" {
            Expr::Binary(
                "==".into(),
                Box::new(left),
                Box::new(Expr::Value(Value::Null)),
            )
        } else {
            left
        };
        let branch = self.emit(span, Op::Branch(test, 0));
        let skip = if op == "||" {
            Some(self.emit(span, Op::Jump(0)))
        } else {
            None
        };
        let rhs = self.code.len();
        let value = self.lower(right)?;
        self.emit(span, Op::Set(result.clone(), value));
        let end = self.code.len();
        if let Op::Branch(_, target) = &mut self.code[branch].op {
            *target = if op == "||" { rhs } else { end };
        }
        if let Some(skip) = skip {
            self.code[skip].op = Op::Jump(end);
        }
        Ok(Expr::Variable(result))
    }
    /// `x &&= y` assigns when x is truthy, `x ||= y` when falsy and
    /// `x ??= y` when nullish; the branch skips the assignment otherwise.
    fn logical_assignment_test(op: ast::AssignOp, old: Expr) -> Expr {
        match op {
            ast::AssignOp::AndAssign => old,
            ast::AssignOp::OrAssign => Expr::Unary("!".into(), Box::new(old)),
            _ => Expr::Binary("==".into(), Box::new(old), Box::new(Expr::Value(Value::Null))),
        }
    }
    fn compound_operator(op: ast::AssignOp) -> Option<&'static str> {
        Some(match op {
            ast::AssignOp::AddAssign => "+",
            ast::AssignOp::SubAssign => "-",
            ast::AssignOp::MulAssign => "*",
            ast::AssignOp::DivAssign => "/",
            ast::AssignOp::ModAssign => "%",
            ast::AssignOp::ExpAssign => "**",
            ast::AssignOp::BitAndAssign => "&",
            ast::AssignOp::BitOrAssign => "|",
            ast::AssignOp::BitXorAssign => "^",
            ast::AssignOp::LShiftAssign => "<<",
            ast::AssignOp::RShiftAssign => ">>",
            ast::AssignOp::ZeroFillRShiftAssign => ">>>",
            _ => return None,
        })
    }
    fn local_target(&mut self, name: &str, span: Span) -> Result<()> {
        match self.resolve(name) {
            Some(false) => self.error(span, "cannot rebind a const value"),
            Some(true) => Ok(()),
            None => self.error(
                span,
                "assignment to an undeclared variable; declare it with let, const or var",
            ),
        }
    }
    /// The outer variables a virtual function's contract names with JSDoc
    /// `@uses <name>` tags: its closure. Each must be visible where the function
    /// is declared, as a closure's free variables are in JavaScript.
    fn captures(&self, description: &str, params: &[String], span: Span) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for line in description.lines() {
            let Some(rest) = line.trim().strip_prefix("@uses") else { continue };
            let Some(name) = rest.split_whitespace().next().map(|word| word.trim_matches(|c: char| matches!(c, '`' | '{' | '}' | ','))) else {
                return self.error(span, "@uses names an outer variable: @uses <name> <what it is>");
            };
            if params.iter().any(|param| param == name) {
                return self.error(span, &format!("@uses {name}: {name} is a parameter; a parameter is already in scope"));
            }
            if self.resolve(name).is_none() {
                return self.error(span, &format!("@uses {name}: no variable {name} is in scope where this function is declared"));
            }
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }
    /// The innermost binding of `name`: Some(true) if it may be reassigned.
    fn resolve(&self, name: &str) -> Option<bool> {
        if self.temp_names.contains(name) {
            return Some(true);
        }
        for scope in self.scopes.iter().rev() {
            if let Some(mutable) = scope.get(name) {
                return Some(*mutable);
            }
        }
        self.external.contains(name).then_some(true)
    }
    /// Open a block scope for its let/const names; no scope is needed without them.
    fn enter_scope(&mut self, span: Span, names: Vec<(String, bool)>) -> Result<bool> {
        if names.is_empty() {
            return Ok(false);
        }
        self.scopes.push(BTreeMap::new());
        for (name, mutable) in &names {
            self.declare(name, *mutable, span)?;
        }
        self.emit(
            span,
            Op::EnterScope(names.into_iter().map(|(name, _)| name).collect()),
        );
        self.runtime_scopes += 1;
        Ok(true)
    }
    fn leave_scope(&mut self, span: Span, entered: bool) {
        if entered {
            self.emit(span, Op::LeaveScope);
            self.runtime_scopes -= 1;
            self.scopes.pop();
        }
    }
    fn begin_loop(&mut self) {
        self.loops.push(LoopContext {
            handlers: self.handlers,
            scopes: self.runtime_scopes,
            ..Default::default()
        });
    }
    /// break/continue leave the exception handlers and block scopes opened
    /// inside the loop; the loop's own stay.
    fn leave_to_loop(&mut self, span: Span, index: usize, current: (usize, usize)) {
        let context = &self.loops[index];
        let (handlers, scopes) = (context.handlers, context.scopes);
        for _ in handlers..current.0 {
            self.emit(span, Op::EndCatch);
        }
        for _ in scopes..current.1 {
            self.emit(span, Op::LeaveScope);
        }
    }
    fn end_loop(&mut self, end: usize, next: usize) {
        let context = self.loops.pop().unwrap();
        for at in context.breaks {
            self.code[at].op = Op::Jump(end);
        }
        for at in context.continues {
            self.code[at].op = Op::Jump(next);
        }
    }
    /// Emit finalizers crossed by a return or a jump to an enclosing loop.
    /// The generated code unwinds to the lexical environment of each finally;
    /// compiler state is restored afterwards for the other control-flow paths.
    fn emit_finalizers(&mut self, target_loop: Option<usize>, span: Span) -> Result<(usize, usize)> {
        let original_handlers = self.handlers;
        let original_runtime_scopes = self.runtime_scopes;
        let mut removed = Vec::new();
        while self.finalizers.last().is_some_and(|ctx| target_loop.is_none_or(|target| target < ctx.loops_len)) {
            let ctx = self.finalizers.pop().unwrap();
            for _ in ctx.handlers..self.handlers { self.emit(span, Op::EndCatch); }
            for _ in ctx.runtime_scopes..self.runtime_scopes { self.emit(span, Op::LeaveScope); }
            self.handlers = ctx.handlers;
            self.runtime_scopes = ctx.runtime_scopes;
            let scopes = self.scopes.split_off(ctx.scopes_len);
            let loops = self.loops.split_off(ctx.loops_len);
            self.stmt(&ast::Stmt::Block(ctx.block.clone()))?;
            removed.push((ctx, scopes, loops));
        }
        let remaining = (self.handlers, self.runtime_scopes);
        while let Some((ctx, scopes, loops)) = removed.pop() {
            self.scopes.extend(scopes);
            self.loops.extend(loops);
            self.finalizers.push(ctx);
        }
        self.handlers = original_handlers;
        self.runtime_scopes = original_runtime_scopes;
        Ok(remaining)
    }

    /// Names are bound when their scope is entered; a declaration initializes
    /// a let/const and assigns a hoisted var.
    fn variable_declaration(&mut self, v: &ast::VarDecl) -> Result<()> {
        let declare = v.kind != ast::VarDeclKind::Var;
        for d in &v.decls {
            match (&d.init, &d.name) {
                (Some(e), _) => self.binding(&d.name, e, declare)?,
                (None, _) if v.kind == ast::VarDeclKind::Const => {
                    return self.error(d.span, "const needs an initializer");
                }
                (None, ast::Pat::Ident(i)) => {
                    // A repeated `var x;` keeps the current value.
                    if declare {
                        self.emit(d.span, Op::Declare(i.id.sym.to_string(), Expr::Undefined));
                    }
                }
                (None, _) => return self.error(d.span, "destructuring needs an initializer"),
            }
        }
        Ok(())
    }
    fn at(&self, span: Span) -> Location {
        let p = self.map.lookup_char_pos(span.lo);
        Location {
            line: p.line,
            column: p.col.0 + 1,
        }
    }
    fn error<T>(&self, span: Span, text: &str) -> Result<T> {
        let p = self.at(span);
        bail!("TS_PROGRAM {}:{}: {text}", p.line, p.column)
    }
    fn emit(&mut self, span: Span, op: Op) -> usize {
        let n = self.code.len();
        self.code.push(Instruction {
            at: self.at(span),
            op,
        });
        n
    }
    fn temp(&mut self) -> String {
        self.temp += 1;
        format!("$temp{}", self.temp)
    }
    // Freeze each argument/field before lowering later effectful expressions.
    // JSON values are snapshots, not references that later push() can change.
    fn snapshot(&mut self, span: Span, value: Expr) -> Expr {
        let name = self.temp();
        self.emit(span, Op::Set(name.clone(), value));
        Expr::Variable(name)
    }
    /// Bind `name` in the innermost scope.
    fn declare(&mut self, name: &str, mutable: bool, span: Span) -> Result<()> {
        if matches!(
            name,
            "tools" | "call" | "ai" | "assert" | "JSON" | "Promise"
        ) || name.starts_with("$temp")
            || self.libraries.contains_key(name)
            || self.imports.contains_key(name)
        {
            return self.error(span, "duplicate or reserved local binding");
        }
        let scope = self.scopes.last_mut().expect("a scope is open");
        if scope.contains_key(name) {
            return self.error(span, &format!("{name} is already declared in this scope"));
        }
        scope.insert(name.into(), mutable);
        Ok(())
    }
    fn property(&self, p: &ast::PropName) -> Result<String> {
        match p {
            ast::PropName::Ident(i) => Ok(i.sym.to_string()),
            ast::PropName::Str(s) => Ok(s.value.to_string_lossy().into_owned()),
            _ => self.error(p.span(), "use a static object key"),
        }
    }
    fn expr(&self, e: &ast::Expr) -> Result<Expr> {
        use ast::Expr as E;
        if let Some(native) = self.native_value(e)? {
            return Ok(native);
        }
        Ok(match e {
            E::Lit(ast::Lit::Str(s)) => Expr::Value(json!(s.value.to_string_lossy())),
            E::Lit(ast::Lit::Regex(r)) => {
                let pattern = r.exp.to_string();
                let flags = r.flags.to_string();
                super::values::compile_regex(&pattern, &flags).map_err(|e| {
                    let p = self.at(r.span);
                    anyhow::anyhow!(
                        "TS_PROGRAM {}:{}: invalid regular expression: {e}",
                        p.line,
                        p.column
                    )
                })?;
                Expr::Value(json!({"$flow_regex": {"pattern": pattern, "flags": flags}}))
            }
            E::Lit(ast::Lit::Num(n)) => {
                if !n.value.is_finite() {
                    return self.error(n.span, "numbers must be finite JSON values");
                }
                if let Some(raw) = &n.raw {
                    let raw = raw.replace('_', "");
                    if raw.chars().all(|c| c.is_ascii_digit()) {
                        return raw
                            .parse::<u64>()
                            .map(|n| Expr::Value(json!(n)))
                            .or_else(|_| {
                                self.error(n.span, "integer literal exceeds the JSON integer range")
                            });
                    }
                    if ["0x", "0X", "0o", "0O", "0b", "0B"]
                        .iter()
                        .any(|prefix| raw.starts_with(prefix))
                        && n.value.abs() > 9_007_199_254_740_991.0
                    {
                        return self.error(n.span, "large integers must use decimal notation");
                    }
                }
                Expr::Value(number_value(n.value))
            }
            E::Lit(ast::Lit::Bool(b)) => Expr::Value(json!(b.value)),
            E::Lit(ast::Lit::Null(_)) => Expr::Value(Value::Null),
            E::Ident(i) if self.resolve(i.sym.as_ref()).is_some() => {
                Expr::Variable(i.sym.to_string())
            }
            E::Ident(i) if i.sym == *"undefined" => Expr::Undefined,
            E::Ident(i) if matches!(i.sym.as_ref(), "NaN" | "Infinity") => {
                number_expr(if i.sym == *"NaN" { f64::NAN } else { f64::INFINITY })
            }
            E::Member(m) if self.constant_member(m).is_some() => self.constant_member(m).unwrap(),
            E::Ident(_) => {
                return self.error(
                    e.span(),
                    "unknown variable; declare it in this function or at the top level, or pass it as a parameter",
                );
            }
            E::Paren(p) => return self.expr(&p.expr),
            E::Array(a) => Expr::Array(
                a.elems
                    .iter()
                    .map(|x| match x {
                        Some(x) if x.spread.is_none() => self.expr(&x.expr),
                        _ => self.error(e.span(), "array holes and spreads are unsupported"),
                    })
                    .collect::<Result<_>>()?,
            ),
            E::Object(o) => Expr::Object(
                o.props
                    .iter()
                    .map(|p| match p {
                        ast::PropOrSpread::Prop(p) => match &**p {
                            ast::Prop::KeyValue(p) => {
                                Ok((self.property(&p.key)?, self.expr(&p.value)?))
                            }
                            ast::Prop::Shorthand(i) => {
                                Ok((i.sym.to_string(), self.expr(&E::Ident(i.clone()))?))
                            }
                            _ => self.error(p.span(), "object methods/accessors are unsupported"),
                        },
                        _ => self.error(e.span(), "object spread is unsupported"),
                    })
                    .collect::<Result<_>>()?,
            ),
            E::Member(m) => Expr::Member(
                Box::new(self.expr(&m.obj)?),
                Box::new(match &m.prop {
                    ast::MemberProp::Ident(i) => Expr::Value(json!(i.sym.to_string())),
                    ast::MemberProp::Computed(c) => self.expr(&c.expr)?,
                    _ => return self.error(e.span(), "private members are unsupported"),
                }),
            ),
            E::Assign(_) => {
                return self.error(
                    e.span(),
                    "assignment is only allowed as a standalone local statement",
                );
            }
            E::Cond(_) | E::Tpl(_) => {
                return self.error(
                    e.span(),
                    "templates and ternaries are unsupported; use a task helper and if/else",
                );
            }
            E::Update(_) | E::TsNonNull(_) => {
                return self.error(
                    e.span(),
                    "mutation operators and type assertions are unsupported",
                );
            }
            E::Call(_) => {
                return self.error(
                    e.span(),
                    "calls must be lowered through the task/tool or semantic boundary",
                );
            }
            _ => {
                return self.error(
                    e.span(),
                    "unsupported expression; use JSON values, ordinary helper/tool calls and if/else",
                );
            }
        })
    }
    /// Index of the scope that binds `name`; the module scope is 0.
    fn resolve_level(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rposition(|scope| scope.contains_key(name))
    }
    /// A callee resolved at compile time: an imported tool, a
    /// library method or a top-level function that no inner binding shadows.
    /// Anything else is a function value called at run time.
    fn static_callee(&self, target: &ast::Expr) -> bool {
        match target {
            ast::Expr::Ident(i) => {
                let name = i.sym.as_ref();
                self.imports.contains_key(name)
                    || (self.names.contains(name) && matches!(self.resolve_level(name), Some(0) | None))
                    || self.resolve(name).is_none()
            }
            ast::Expr::Member(m) => matches!(&*m.obj, ast::Expr::Ident(i)
                if self.libraries.contains_key(i.sym.as_ref()) && self.resolve(i.sym.as_ref()).is_none()),
            _ => false,
        }
    }
    /// Call a function value. The callee is evaluated before the arguments.
    fn dynamic_call(&mut self, call: &ast::CallExpr, callee: &ast::Expr) -> Result<Expr> {
        if call.args.iter().any(|a| a.spread.is_some()) {
            return self.error(call.span, "argument spread is unsupported");
        }
        let value = self.lower(callee)?;
        let callee = self.snapshot(callee.span(), value);
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            let value = self.lower(&arg.expr)?;
            args.push(self.snapshot(arg.span(), value));
        }
        let dest = self.temp();
        self.emit(
            call.span,
            Op::Effect(Some(dest.clone()), Effect::Dynamic { callee, args }),
        );
        Ok(Expr::Variable(dest))
    }
    /// Compile a nested function, arrow or method into its own code unit and
    /// return its id. Its body sees every scope visible at this point.
    fn compile_function(&mut self, source: FunctionSource<'_>) -> Result<String> {
        if source.is_generator {
            return self.error(source.span, "generator functions are unsupported");
        }
        let at = self.at(source.span);
        let id = format!(
            "{}/{}@{}:{}",
            self.function_id,
            source.name.as_deref().unwrap_or("<anonymous>"),
            at.line,
            at.column
        );
        let mut scopes = self.scopes.clone();
        scopes.push(BTreeMap::new());
        let mut c = Compiler {
            map: self.map.clone(),
            names: self.names.clone(),
            arities: self.arities.clone(),
            async_entry: None,
            libraries: self.libraries.clone(),
            imports: self.imports.clone(),
            scopes,
            external: self.external.clone(),
            temp_names: BTreeSet::new(),
            runtime_scopes: 0,
            function_id: id.clone(),
            nested: Vec::new(),
            virtuals: self.virtuals.clone(),
            async_functions: self.async_functions.clone(),
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            finalizers: Vec::new(),
            chain_exits: None,
            chain_object: false,
            contracts: self.contracts.clone(),
            leaf_tools: self.leaf_tools.clone(),
        };
        let mut params = Vec::new();
        let mut parameter_lexical = Vec::new();
        let mut parameter_patterns = Vec::new();
        let mut parameter_names = Vec::new();
        let patterned = source.params.iter().any(|p| !matches!(p, ast::Pat::Ident(_)));
        let rest_parameter = source.params.iter().position(|p| matches!(p, ast::Pat::Rest(_)));
        for (index, pattern) in source.params.iter().enumerate() {
            let names = pattern_names(pattern);
            for name in &names { c.declare(name, true, pattern.span())?; }
            parameter_names.extend(names.iter().cloned());
            if let ast::Pat::Ident(i) = pattern && !patterned {
                params.push(i.id.sym.to_string());
            } else {
                let argument = format!("$$param{index}");
                params.push(argument.clone());
                parameter_lexical.extend(names);
                let pattern = match pattern { ast::Pat::Rest(rest) => &*rest.arg, _ => pattern };
                parameter_patterns.push((pattern, argument));
            }
        }
        // The function name is available in parameter initializers.
        if let Some(name) = &source.self_name {
            let scope = c.scopes.last_mut().expect("function scope");
            if !scope.contains_key(name) { scope.insert(name.clone(), false); }
        }
        for (pattern, argument) in parameter_patterns {
            c.bind_value(pattern, Expr::Variable(argument), true)?;
        }
        let (vars, lexical) = match &source.body {
            FunctionBody::Block(body) => {
                let vars: Vec<String> = var_names(&body.stmts).into_iter()
                    .filter(|name| patterned || !parameter_names.contains(name)).collect();
                let lexical = lexical_names(&body.stmts);
                let lexical_only = body.stmts.iter().filter(|stmt| matches!(stmt, ast::Stmt::Decl(ast::Decl::Var(v)) if v.kind != ast::VarDeclKind::Var)).cloned().collect::<Vec<_>>();
                if let Some((name, _)) = lexical_names(&lexical_only).iter().find(|(name, _)| parameter_names.contains(name)) {
                    return c.error(body.span, &format!("duplicate parameter and lexical binding {name}"));
                }
                if patterned { c.scopes.push(BTreeMap::new()); }
                for name in &vars { c.declare(name, true, body.span)?; }
                for (name, mutable) in &lexical { c.declare(name, *mutable, body.span)?; }
                if patterned { c.emit(body.span, Op::EnterFunctionBody); }
                (vars, lexical)
            }
            _ => (Vec::new(), Vec::new()),
        };
        // A nested declaration marked @virtual is AI work, as at the top level.
        let contract = source.name.as_ref().and_then(|_| self.contracts.get(&source.span.lo.0)).filter(|(v, _)| *v).cloned();
        match source.body {
            FunctionBody::Block(body) if contract.is_some() => {
                let (_, description) = contract.clone().unwrap();
                if patterned {
                    return c.error(source.span, "@virtual functions require simple parameters");
                }
                if !body.stmts.iter().all(|s| matches!(s, ast::Stmt::Empty(_))) {
                    return c.error(body.span, "@virtual declares AI work: its body holds only comments describing it");
                }
                if description.is_empty() {
                    return c.error(body.span, "@virtual needs a task description and return contract");
                }
                let uses = self.captures(&description, &params, source.span)?;
                // Captured variables join the input, read in this closure's scope.
                let input = Expr::Object(params.iter().chain(&uses).map(|p| (p.clone(), Expr::Variable(p.clone()))).collect());
                let dest = c.temp();
                c.emit(body.span, Op::Effect(Some(dest.clone()), Effect::Task { name: id.clone(), description, input }));
                c.emit(body.span, Op::Return(Expr::Variable(dest)));
            }
            FunctionBody::Block(body) => {
                c.hoist_functions(&body.stmts)?;
                for s in &body.stmts {
                    c.stmt(s)?;
                }
                c.emit(body.span, Op::Return(Expr::Undefined));
            }
            FunctionBody::Expr(body) => {
                let value = c.lower(body)?;
                c.emit(body.span(), Op::Return(value));
            }
            FunctionBody::Missing => return c.error(source.span, "function body is required"),
        }
        self.nested.extend(c.nested);
        let virtual_task = contract.is_some();
        let description = contract.map(|(_, description)| description).unwrap_or_default();
        let uses = if virtual_task { self.captures(&description, &params, source.span)? } else { Vec::new() };
        self.nested.push((
            id.clone(),
            Function {
                description,
                virtual_task,
                uses,
                leaf_tools: if virtual_task { self.leaf_tools.clone() } else { None },
                params,
                rest_parameter,
                parameter_scope: patterned,
                parameter_lexical,
                vars,
                lexical: lexical.into_iter().map(|(name, _)| name).collect(),
                module: false,
                is_async: source.is_async,
                self_name: source.self_name,
                code: c.code,
            },
        ));
        Ok(id)
    }
    /// Function declarations are initialized as soon as their scope is entered.
    fn hoist_functions(&mut self, statements: &[ast::Stmt]) -> Result<()> {
        for statement in statements {
            if let ast::Stmt::Decl(ast::Decl::Fn(f)) = statement {
                let name = f.ident.sym.to_string();
                let id = self.compile_function(FunctionSource::function(
                    &f.function,
                    Some(name.clone()),
                    None,
                ))?;
                self.emit(f.function.span, Op::Declare(name, Expr::Function(id)));
            }
        }
        Ok(())
    }
    /// A built-in function or tool named where a value is expected, such as
    /// `String`, `Math.max`, an imported tool or `io.read`.
    fn native_value(&self, e: &ast::Expr) -> Result<Option<Expr>> {
        Ok(match e {
            ast::Expr::Ident(i) => {
                let name = i.sym.as_ref();
                if self.resolve(name).is_some() {
                    None
                } else if let Some(tool) = self.imports.get(name) {
                    Some(Expr::Native(format!("tool:{tool}")))
                } else if self.global(name) && NATIVE_GLOBALS.contains(&name) {
                    Some(Expr::Native(name.to_owned()))
                } else {
                    None
                }
            }
            ast::Expr::Member(m) => {
                let (ast::Expr::Ident(namespace), ast::MemberProp::Ident(property)) = (&*m.obj, &m.prop)
                else {
                    return Ok(None);
                };
                let (namespace, property) = (namespace.sym.as_ref(), property.sym.as_ref());
                if self.resolve(namespace).is_some() {
                    None
                } else if let Some(methods) = self.libraries.get(namespace) {
                    match methods.get(property) {
                        Some(tool) => Some(Expr::Native(format!("tool:{tool}"))),
                        None => {
                            return self.error(
                                m.span,
                                &format!("unknown library method {namespace}.{property}; use the published library catalog"),
                            );
                        }
                    }
                } else {
                    let name = format!("{namespace}.{property}");
                    (matches!(namespace, "Math" | "Number" | "Object" | "Array" | "JSON")
                        && self.global(namespace)
                        && super::js_methods::builtin_arity(&name).is_some())
                    .then_some(Expr::Native(name))
                }
            }
            _ => None,
        })
    }
    fn effect(&self, call: &ast::CallExpr) -> Result<Effect> {
        if call.args.iter().any(|a| a.spread.is_some()) {
            return self.error(call.span, "argument spreads are unsupported");
        }
        let ast::Callee::Expr(target) = &call.callee else {
            return self.error(call.span, "use a declared helper or library.method");
        };
        if matches!(&**target, ast::Expr::Ident(i) if matches!(i.sym.as_ref(), "fork" | "join") && !self.names.contains(i.sym.as_ref()) && self.resolve(i.sym.as_ref()).is_none()) {
            return self.error(call.span, "fork/join have been retired; use async virtual functions and Promise.all");
        }
        let actuals = &call.args[..];
        let target = &**target;
        let args = actuals
            .iter()
            .map(|a| self.expr(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let target = match target {
            ast::Expr::Ident(i) if self.imports.contains_key(i.sym.as_ref()) => {
                self.imports[i.sym.as_ref()].clone()
            }
            ast::Expr::Ident(i) if self.names.contains(i.sym.as_ref()) => {
                if self.async_entry.as_deref() == Some(i.sym.as_ref()) {
                    return self.error(
                        call.span,
                        "the async entry is awaited by the host and cannot be called as a helper",
                    );
                }
                // Virtual calls bind arguments exactly; ordinary functions use JS arity.
                let exact = self.virtuals.contains(i.sym.as_ref());
                if exact && self.arities[i.sym.as_ref()] != args.len() {
                    return self.error(
                        call.span,
                        &format!(
                            "{} expects {} arguments, got {}",
                            i.sym,
                            self.arities[i.sym.as_ref()],
                            args.len()
                        ),
                    );
                }
                return Ok(Effect::Call(i.sym.to_string(), args));
            }
            ast::Expr::Member(m) if matches!(&*m.obj,ast::Expr::Ident(i) if self.libraries.contains_key(i.sym.as_ref())) =>
            {
                let ast::Expr::Ident(library) = &*m.obj else {
                    unreachable!()
                };
                let ast::MemberProp::Ident(method) = &m.prop else {
                    return self
                        .error(call.span, "library method names must be static identifiers");
                };
                match self.libraries[library.sym.as_ref()].get(method.sym.as_ref()) {
                    Some(tool) => tool.clone(),
                    None => {
                        return self.error(
                            call.span,
                            &format!(
                                "unknown library method {}.{}; use the published library catalog",
                                library.sym, method.sym
                            ),
                        );
                    }
                }
            }
            _ => {
                return self.error(
                    call.span,
                    "use helper(args) or library.method(args); call/ai wrappers are unsupported",
                );
            }
        };
        if args.len() > 1 {
            return self.error(call.span, "a tool call takes one JSON argument object");
        }
        Ok(Effect::Tool(target, args))
    }
    // ------------------------------------------------------------ globals

    /// A global name, unless a local, function, import or library shadows it.
    fn global(&self, name: &str) -> bool {
        self.resolve(name).is_none()
            && !self.libraries.contains_key(name)
            && !self.imports.contains_key(name)
            && !self.names.contains(name)
    }
    /// Math.PI, Number.MAX_SAFE_INTEGER and the other static constants.
    fn constant_member(&self, m: &ast::MemberExpr) -> Option<Expr> {
        let ast::Expr::Ident(namespace) = &*m.obj else {
            return None;
        };
        let ast::MemberProp::Ident(property) = &m.prop else {
            return None;
        };
        if !matches!(namespace.sym.as_ref(), "Math" | "Number") || !self.global(namespace.sym.as_ref()) {
            return None;
        }
        super::js_methods::constant(&format!("{}.{}", namespace.sym, property.sym)).map(number_expr)
    }
    /// Global functions, static methods of Math/Number/Object/Array/JSON and
    /// Error constructors called without `new`.
    fn builtin_call(&mut self, call: &ast::CallExpr) -> Result<Option<Expr>> {
        let ast::Callee::Expr(callee) = &call.callee else {
            return Ok(None);
        };
        if matches!(&**callee, ast::Expr::Ident(i) if i.sym == *"$$apply") {
            ensure!(call.args.len() == 2 && call.args.iter().all(|arg| arg.spread.is_none()), "internal apply requires a function and argument array");
            let callee = self.lower(&call.args[0].expr)?;
            let callee = self.snapshot(call.span, callee);
            let args = self.lower(&call.args[1].expr)?;
            let dest = self.temp();
            self.emit(call.span, Op::Effect(Some(dest.clone()), Effect::Apply { callee, args }));
            return Ok(Some(Expr::Variable(dest)));
        }
        let name = match &**callee {
            ast::Expr::Ident(i) if self.global(i.sym.as_ref()) => {
                let name = i.sym.to_string();
                if super::vm_eval::ERROR_CLASSES.contains(&name.as_str()) {
                    format!("new {name}")
                } else {
                    name
                }
            }
            ast::Expr::Member(m) => {
                let (ast::Expr::Ident(namespace), ast::MemberProp::Ident(method)) = (&*m.obj, &m.prop)
                else {
                    return Ok(None);
                };
                if !matches!(namespace.sym.as_ref(), "Math" | "Number" | "Object" | "Array" | "JSON")
                    || !self.global(namespace.sym.as_ref())
                {
                    return Ok(None);
                }
                format!("{}.{}", namespace.sym, method.sym)
            }
            _ => return Ok(None),
        };
        let Some((min, max)) = super::js_methods::builtin_arity(&name) else {
            return match &**callee {
                ast::Expr::Member(_) => self.error(call.span, &format!("unsupported built-in {name}")),
                _ => Ok(None),
            };
        };
        if call.args.iter().any(|a| a.spread.is_some()) {
            return self.error(call.span, "built-in argument spreads are unsupported");
        }
        if name == "Array.from" && call.args.len() == 2 {
            // Array.from(items, mapFn) is Array.from(items).map(mapFn).
            let source = self.lower(&call.args[0].expr)?;
            let source = self.snapshot(call.args[0].span(), source);
            let array = self.snapshot(call.span, Expr::NodeBuiltin(name, vec![source]));
            return self.call_method_on(array, "map", &call.args[1..], call.span).map(Some);
        }
        if !(min..=max).contains(&call.args.len()) {
            return self.error(call.span, &format!("{name} takes {min} to {max} arguments"));
        }
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            let value = self.lower(&arg.expr)?;
            args.push(self.snapshot(arg.span(), value));
        }
        Ok(Some(self.snapshot(call.span, Expr::NodeBuiltin(name, args))))
    }

    // ------------------------------------------------------- optional chains

    /// Lower a member object or method receiver. Inside an optional chain it
    /// continues that chain; anywhere else it is an ordinary expression.
    fn lower_object(&mut self, e: &ast::Expr) -> Result<Expr> {
        self.chain_object = true;
        self.lower(e)
    }
    /// Lower an expression. Arguments, keys and operands inside an optional
    /// chain are not part of it: their own chains short-circuit on their own.
    fn lower(&mut self, e: &ast::Expr) -> Result<Expr> {
        let joins_chain = std::mem::take(&mut self.chain_object);
        if !joins_chain && self.chain_exits.is_some() {
            let saved = self.chain_exits.take();
            let result = self.lower_inner(e);
            self.chain_exits = saved;
            return result;
        }
        self.lower_inner(e)
    }
    /// One optional chain: a `?.` whose left side is null or undefined makes
    /// the whole chain undefined without evaluating the rest of it.
    fn with_chain(&mut self, span: Span, f: impl FnOnce(&mut Self) -> Result<Expr>) -> Result<Expr> {
        if self.chain_exits.is_some() {
            return f(self);
        }
        self.chain_exits = Some(Vec::new());
        let value = f(self);
        let exits = self.chain_exits.take().unwrap_or_default();
        let value = value?;
        if exits.is_empty() {
            return Ok(value);
        }
        let result = self.temp();
        self.emit(span, Op::Set(result.clone(), value));
        let jump = self.emit(span, Op::Jump(0));
        let exit = self.code.len();
        self.emit(span, Op::Set(result.clone(), Expr::Undefined));
        let end = self.code.len();
        self.code[jump].op = Op::Jump(end);
        for at in exits {
            if let Op::Branch(_, target) = &mut self.code[at].op {
                *target = exit;
            }
        }
        Ok(Expr::Variable(result))
    }
    fn chain_guard(&mut self, span: Span, value: Expr) {
        let test = Expr::Binary("!=".into(), Box::new(value), Box::new(Expr::Value(Value::Null)));
        let at = self.emit(span, Op::Branch(test, 0));
        self.chain_exits
            .as_mut()
            .expect("a chain guard is emitted inside an optional chain")
            .push(at);
    }
    fn member_key(&mut self, prop: &ast::MemberProp, span: Span) -> Result<Expr> {
        Ok(match prop {
            ast::MemberProp::Ident(i) => Expr::Value(json!(i.sym.to_string())),
            ast::MemberProp::Computed(c) => {
                let key = self.lower(&c.expr)?;
                self.snapshot(c.span, key)
            }
            _ => return self.error(span, "private members are unsupported"),
        })
    }
    fn chain_link(&mut self, chain: &ast::OptChainExpr) -> Result<Expr> {
        match &*chain.base {
            ast::OptChainBase::Member(m) => {
                let object = self.lower_object(&m.obj)?;
                let object = self.snapshot(m.obj.span(), object);
                if chain.optional {
                    self.chain_guard(chain.span, object.clone());
                }
                let key = self.member_key(&m.prop, m.span)?;
                Ok(Expr::Member(Box::new(object), Box::new(key)))
            }
            ast::OptChainBase::Call(call) => {
                if chain.optional {
                    let callee = self.lower_object(&call.callee)?;
                    let callee = self.snapshot(call.callee.span(), callee);
                    self.chain_guard(call.span, callee.clone());
                    if call.args.iter().any(|arg| arg.spread.is_some()) {
                        let array = ast::Expr::Array(ast::ArrayLit { span: call.span, elems: call.args.iter().cloned().map(Some).collect() });
                        let args = self.lower(&array)?;
                        let dest = self.temp();
                        self.emit(call.span, Op::Effect(Some(dest.clone()), Effect::Apply { callee, args }));
                        return Ok(Expr::Variable(dest));
                    }
                    let mut args = Vec::new();
                    for arg in &call.args {
                        let value = self.lower(&arg.expr)?;
                        args.push(self.snapshot(arg.span(), value));
                    }
                    let dest = self.temp();
                    self.emit(call.span, Op::Effect(Some(dest.clone()), Effect::Dynamic { callee, args }));
                    return Ok(Expr::Variable(dest));
                }
                let (member, optional) = match &*call.callee {
                    ast::Expr::OptChain(inner) => match &*inner.base {
                        ast::OptChainBase::Member(m) => (m, inner.optional),
                        _ => {
                            return self.error(
                                call.span,
                                "calling a call result needs first-class functions, which are not available yet",
                            );
                        }
                    },
                    ast::Expr::Member(m) => (m, false),
                    _ => {
                        return self.error(
                            call.span,
                            "optional calls such as f?.() need first-class functions, which are not available yet",
                        );
                    }
                };
                let receiver = self.lower_object(&member.obj)?;
                let receiver = self.snapshot(member.obj.span(), receiver);
                if optional {
                    self.chain_guard(call.span, receiver.clone());
                }
                let ast::MemberProp::Ident(method) = &member.prop else {
                    return self.error(call.span, "method names must be static identifiers");
                };
                self.call_method_on(receiver, method.sym.as_ref(), &call.args, call.span)
            }
        }
    }

    // --------------------------------------------------------------- methods

    fn call_method_spread(&mut self, receiver: Expr, key: Expr, args: &[ast::ExprOrSpread], span: Span) -> Result<Expr> {
        let key = self.snapshot(span, Expr::NodeBuiltin("String".into(), vec![key]));
        let is_object = Expr::Binary("===".into(), Box::new(Expr::Unary("typeof".into(), Box::new(receiver.clone()))), Box::new(Expr::Value(json!("object"))));
        let not_null = Expr::Binary("!=".into(), Box::new(receiver.clone()), Box::new(Expr::Value(Value::Null)));
        let check = Expr::Binary("&&".into(), Box::new(is_object), Box::new(not_null));
        let has = Expr::Conditional(Box::new(check), Box::new(Expr::Binary("in".into(), Box::new(key.clone()), Box::new(receiver.clone()))), Box::new(Expr::Value(json!(false))));
        let has = self.snapshot(span, has);
        let callee = self.snapshot(span, Expr::Member(Box::new(receiver.clone()), Box::new(key.clone())));
        let array = ast::Expr::Array(ast::ArrayLit { span, elems: args.iter().cloned().map(Some).collect() });
        let args = self.lower(&array)?;
        let dest = self.temp();
        let builtin = self.emit(span, Op::Branch(has, 0));
        self.emit(span, Op::Effect(Some(dest.clone()), Effect::Apply { callee, args: args.clone() }));
        let done = self.emit(span, Op::Jump(0));
        self.land(builtin);
        self.emit(span, Op::Effect(Some(dest.clone()), Effect::DynamicMethodApply { receiver, name: key, args }));
        self.land(done);
        Ok(Expr::Variable(dest))
    }

    /// `receiver.method(args)` for a built-in receiver kind. Callback methods
    /// are compiled into this frame so that their callbacks can suspend.
    fn call_method_on(
        &mut self,
        receiver: Expr,
        method: &str,
        args: &[ast::ExprOrSpread],
        span: Span,
    ) -> Result<Expr> {
        if method != "map" && CALLBACK_METHODS.contains(&method) && args.first().is_some_and(|a| match &*a.expr {
            ast::Expr::Arrow(f) => f.is_async,
            ast::Expr::Fn(f) => f.function.is_async,
            _ => false,
        }) {
            return self.error(span, "async callbacks require Promise support");
        }
        if args.iter().any(|a| a.spread.is_some()) {
            return self.call_method_spread(receiver, Expr::Value(json!(method)), args, span);
        }
        if let Some(reason) = super::js_methods::unavailable_method(method) {
            return self.error(span, reason);
        }
        // Built-in methods keep their argument checks. Any other name may be a
        // function-valued property, resolved when the call runs.
        if let Some((min, max)) = super::js_methods::method_arity(method)
            && !(min..=max).contains(&args.len())
        {
            return self.error(span, &format!("{method} takes {min} to {max} arguments"));
        }
        let mut lowered = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.lower(&arg.expr)?;
            lowered.push(self.snapshot(arg.span(), value));
        }
        let dest = self.temp();
        self.emit(
            span,
            Op::Effect(
                Some(dest.clone()),
                Effect::Method {
                    receiver,
                    name: method.into(),
                    args: lowered,
                },
            ),
        );
        Ok(Expr::Variable(dest))
    }

    fn land(&mut self, at: usize) {
        let here = self.code.len();
        match &mut self.code[at].op {
            Op::Branch(_, target) | Op::Jump(target) => *target = here,
            _ => unreachable!("only branches and jumps are patched"),
        }
    }

    fn lower_inner(&mut self, e: &ast::Expr) -> Result<Expr> {
        self.reject_unawaited_operands(e)?;
        match e {
            ast::Expr::OptChain(chain) => self.with_chain(chain.span, |c| c.chain_link(chain)),
            ast::Expr::New(new)
                if matches!(&*new.callee, ast::Expr::Ident(i)
                    if super::vm_eval::ERROR_CLASSES.contains(&i.sym.as_ref()) && self.global(i.sym.as_ref())) =>
            {
                let ast::Expr::Ident(class) = &*new.callee else { unreachable!() };
                let args = new.args.as_deref().unwrap_or(&[]);
                if args.len() > 2 || args.iter().any(|a| a.spread.is_some()) {
                    return self.error(new.span, "Error constructors take a message and an optional options object");
                }
                let mut lowered = Vec::with_capacity(args.len());
                for arg in args {
                    let value = self.lower(&arg.expr)?;
                    lowered.push(self.snapshot(arg.span(), value));
                }
                Ok(self.snapshot(new.span, Expr::NodeBuiltin(format!("new {}", class.sym), lowered)))
            }
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Delete => {
                let ast::Expr::Member(m) = &*u.arg else {
                    return self.error(u.span, "delete needs a property access such as delete object.key");
                };
                let object = self.lower_object(&m.obj)?;
                let object = self.snapshot(m.obj.span(), object);
                let key = self.member_key(&m.prop, m.span)?;
                Ok(self.snapshot(u.span, Expr::NodeBuiltin("delete".into(), vec![object, key])))
            }
            ast::Expr::Unary(u)
                if u.op == ast::UnaryOp::TypeOf
                    && matches!(&*u.arg, ast::Expr::Ident(i) if self.resolve(i.sym.as_ref()).is_none()) =>
            {
                // typeof never throws for an undeclared name.
                let ast::Expr::Ident(i) = &*u.arg else { unreachable!() };
                let kind = if self.names.contains(i.sym.as_ref()) {
                    "function"
                } else if matches!(i.sym.as_ref(), "Math" | "JSON" | "Number" | "Object" | "Array" | "path" | "url") {
                    "object"
                } else if i.sym == *"undefined" {
                    "undefined"
                } else if i.sym == *"NaN" || i.sym == *"Infinity" {
                    "number"
                } else {
                    "undefined"
                };
                Ok(Expr::Value(json!(kind)))
            }
            ast::Expr::New(new) if matches!(&*new.callee, ast::Expr::Ident(i) if i.sym == *"Promise") && self.global("Promise") => {
                let args = new.args.as_deref().unwrap_or(&[]);
                if args.iter().any(|arg| arg.spread.is_some()) {
                    return self.error(new.span, "Promise constructor spread arguments are not yet supported");
                }
                let mut values = Vec::new();
                for arg in args {
                    let value = self.lower(&arg.expr)?;
                    values.push(self.snapshot(arg.span(), value));
                }
                let executor = values.into_iter().next().unwrap_or(Expr::Undefined);
                let destination = self.temp();
                self.emit(new.span, Op::Effect(Some(destination.clone()), Effect::PromiseConstruct { executor }));
                Ok(Expr::Variable(destination))
            }
            ast::Expr::New(new) if matches!(&*new.callee, ast::Expr::Ident(i) if i.sym == *"RegExp") => {
                if self.resolve("RegExp").is_some() {
                    return self.error(new.span, "shadowed RegExp constructor is unsupported");
                }
                let args = new.args.as_deref().unwrap_or(&[]);
                if args.len() > 2 || args.iter().any(|a| a.spread.is_some()) {
                    return self.error(new.span, "RegExp accepts at most two arguments without spread");
                }
                let mut lowered = Vec::new();
                for arg in args {
                    let value = self.lower(&arg.expr)?;
                    lowered.push(self.snapshot(arg.span(), value));
                }
                Ok(Expr::RegExp(lowered))
            }
            ast::Expr::Await(awaited) => {
                let value = self.lower(&awaited.arg)?;
                let destination = self.temp();
                self.emit(awaited.span, Op::Await { value, destination: destination.clone() });
                Ok(Expr::Variable(destination))
            }
            ast::Expr::Bin(b) => {
                let op = match b.op {
                    ast::BinaryOp::EqEq => "==",
                    ast::BinaryOp::NotEq => "!=",
                    ast::BinaryOp::EqEqEq => "===",
                    ast::BinaryOp::NotEqEq => "!==",
                    ast::BinaryOp::Lt => "<",
                    ast::BinaryOp::LtEq => "<=",
                    ast::BinaryOp::Gt => ">",
                    ast::BinaryOp::GtEq => ">=",
                    ast::BinaryOp::Add => "+",
                    ast::BinaryOp::Sub => "-",
                    ast::BinaryOp::Mul => "*",
                    ast::BinaryOp::Div => "/",
                    ast::BinaryOp::Mod => "%",
                    ast::BinaryOp::LogicalAnd => "&&",
                    ast::BinaryOp::LogicalOr => "||",
                    ast::BinaryOp::NullishCoalescing => "??",
                    ast::BinaryOp::Exp => "**",
                    ast::BinaryOp::BitAnd => "&",
                    ast::BinaryOp::BitOr => "|",
                    ast::BinaryOp::BitXor => "^",
                    ast::BinaryOp::LShift => "<<",
                    ast::BinaryOp::RShift => ">>",
                    ast::BinaryOp::ZeroFillRShift => ">>>",
                    ast::BinaryOp::In => "in",
                    ast::BinaryOp::InstanceOf => "instanceof",
                };
                let left = self.lower(&b.left)?;
                if matches!(op, "&&" | "||" | "??") {
                    return self.logical(op, left, &b.right, b.span);
                }
                if op == "instanceof" {
                    let known = matches!(&*b.right, ast::Expr::Ident(class)
                        if (super::vm_eval::ERROR_CLASSES.contains(&class.sym.as_ref())
                            || matches!(class.sym.as_ref(), "Array" | "Object" | "RegExp" | "Promise" | "Function"))
                            && self.global(class.sym.as_ref()));
                    let ast::Expr::Ident(class) = &*b.right else {
                        return self.error(b.span, "instanceof needs a built-in constructor such as Error or Array");
                    };
                    if !known {
                        return self.error(b.span, "instanceof needs a built-in constructor such as Error or Array");
                    }
                    let left = self.snapshot(b.left.span(), left);
                    return Ok(Expr::Binary(
                        "instanceof".into(),
                        Box::new(left),
                        Box::new(Expr::Value(json!(class.sym.to_string()))),
                    ));
                }
                let left = self.snapshot(b.left.span(), left);
                let right = self.lower(&b.right)?;
                let right = self.snapshot(b.right.span(), right);
                Ok(Expr::Binary(op.into(), Box::new(left), Box::new(right)))
            }
            ast::Expr::Unary(u) => {
                let op = match u.op {
                    ast::UnaryOp::Bang => "!",
                    ast::UnaryOp::Plus => "+",
                    ast::UnaryOp::Minus => "-",
                    ast::UnaryOp::TypeOf => "typeof",
                    ast::UnaryOp::Tilde => "~",
                    ast::UnaryOp::Void => "void",
                    _ => return self.error(u.span, "unsupported unary operator"),
                };
                let value = self.lower(&u.arg)?;
                Ok(Expr::Unary(op.into(), Box::new(value)))
            }
            ast::Expr::Cond(c) => {
                let test = self.condition(&c.test)?;
                let branch = self.emit(c.span, Op::Branch(test, 0));
                let result = self.temp();
                self.value(&c.cons, result.clone())?;
                let jump = self.emit(c.span, Op::Jump(0));
                let alternative = self.code.len();
                self.value(&c.alt, result.clone())?;
                let end = self.code.len();
                if let Op::Branch(_, target) = &mut self.code[branch].op {
                    *target = alternative;
                }
                self.code[jump].op = Op::Jump(end);
                Ok(Expr::Variable(result))
            }
            ast::Expr::Tpl(t) => {
                let mut parts = Vec::new();
                for (i, quasi) in t.quasis.iter().enumerate() {
                    let text = quasi
                        .cooked
                        .as_ref()
                        .context("invalid template escape")?
                        .to_string_lossy()
                        .into_owned();
                    parts.push(Expr::Value(json!(text)));
                    if let Some(e) = t.exprs.get(i) {
                        let value = self.lower(e)?;
                        // Convert now, before any later interpolation has effects.
                        let value = Expr::Template(vec![value]);
                        parts.push(self.snapshot(e.span(), value));
                    }
                }
                Ok(Expr::Template(parts))
            }
            ast::Expr::Assign(a) => {
                if let ast::AssignTarget::Simple(ast::SimpleAssignTarget::Member(member)) = &a.left {
                    let object = self.lower(&member.obj)?;
                    let object = self.snapshot(member.obj.span(), object);
                    let key = match &member.prop {
                        ast::MemberProp::Ident(name) => Expr::Value(json!(name.sym.to_string())),
                        ast::MemberProp::Computed(key) => { let value = self.lower(&key.expr)?; self.snapshot(key.span, value) },
                        _ => return self.error(member.span, "private members are unsupported"),
                    };
                    if matches!(a.op, ast::AssignOp::AndAssign | ast::AssignOp::OrAssign | ast::AssignOp::NullishAssign) {
                        let old = self.snapshot(a.span, Expr::Member(Box::new(object.clone()), Box::new(key.clone())));
                        let result = self.temp();
                        self.emit(a.span, Op::Set(result.clone(), old.clone()));
                        let skip = self.emit(a.span, Op::Branch(Self::logical_assignment_test(a.op, old), 0));
                        let value = self.lower(&a.right)?;
                        let value = self.snapshot(a.right.span(), value);
                        self.emit(a.span, Op::SetMember { object, key, value: value.clone() });
                        self.emit(a.span, Op::Set(result.clone(), value));
                        self.land(skip);
                        return Ok(Expr::Variable(result));
                    }
                    let op = Self::compound_operator(a.op);
                    if a.op != ast::AssignOp::Assign && op.is_none() {
                        return self.error(a.span, "unsupported compound member assignment");
                    }
                    let old = op.map(|_| self.snapshot(a.span, Expr::Member(Box::new(object.clone()), Box::new(key.clone()))));
                    let value = self.lower(&a.right)?;
                    let value = if let (Some(op), Some(old)) = (op, old) { Expr::Binary(op.into(), Box::new(old), Box::new(value)) } else { value };
                    let value = self.snapshot(a.right.span(), value);
                    self.emit(a.span, Op::SetMember { object, key, value: value.clone() });
                    return Ok(value);
                }
                let ast::AssignTarget::Simple(ast::SimpleAssignTarget::Ident(i)) = &a.left else {
                    return self.error(a.span, "only local variable assignment is supported");
                };
                let name = i.id.sym.to_string();
                self.local_target(&name, a.span)?;
                if matches!(a.op, ast::AssignOp::AndAssign | ast::AssignOp::OrAssign | ast::AssignOp::NullishAssign) {
                    let old = self.snapshot(a.span, Expr::Variable(name.clone()));
                    let skip = self.emit(a.span, Op::Branch(Self::logical_assignment_test(a.op, old), 0));
                    let value = self.lower(&a.right)?;
                    self.emit(a.span, Op::Set(name.clone(), value));
                    self.land(skip);
                    return Ok(Expr::Variable(name));
                }
                let op = Self::compound_operator(a.op);
                if a.op != ast::AssignOp::Assign && op.is_none() {
                    return self.error(a.span, "unsupported compound assignment");
                }
                let old = op.map(|_| self.snapshot(a.span, Expr::Variable(name.clone())));
                let value = self.lower(&a.right)?;
                let value = if let (Some(op), Some(old)) = (op, old) {
                    Expr::Binary(op.into(), Box::new(old), Box::new(value))
                } else {
                    value
                };
                self.emit(a.span, Op::Set(name.clone(), value));
                Ok(Expr::Variable(name))
            }
            ast::Expr::Update(u) => {
                if let ast::Expr::Member(member) = &*u.arg {
                    let object = self.lower(&member.obj)?;
                    let object = self.snapshot(member.obj.span(), object);
                    let key = match &member.prop {
                        ast::MemberProp::Ident(name) => Expr::Value(json!(name.sym.to_string())),
                        ast::MemberProp::Computed(key) => { let value = self.lower(&key.expr)?; self.snapshot(key.span, value) },
                        _ => return self.error(member.span, "private members are unsupported"),
                    };
                    let old = self.snapshot(u.span, Expr::Unary("+".into(), Box::new(Expr::Member(Box::new(object.clone()), Box::new(key.clone())))));
                    let op = if u.op == ast::UpdateOp::PlusPlus { "+" } else { "-" };
                    let value = self.snapshot(u.span, Expr::Binary(op.into(), Box::new(old.clone()), Box::new(Expr::Value(json!(1)))));
                    self.emit(u.span, Op::SetMember { object, key, value: value.clone() });
                    return Ok(if u.prefix { value } else { old });
                }
                let ast::Expr::Ident(i) = &*u.arg else {
                    return self.error(u.span, "update requires a local variable");
                };
                let name = i.sym.to_string();
                self.local_target(&name, u.span)?;
                let old = self.snapshot(
                    u.span,
                    Expr::Unary("+".into(), Box::new(Expr::Variable(name.clone()))),
                );
                let op = if u.op == ast::UpdateOp::PlusPlus {
                    "+"
                } else {
                    "-"
                };
                self.emit(
                    u.span,
                    Op::Set(
                        name.clone(),
                        Expr::Binary(
                            op.into(),
                            Box::new(old.clone()),
                            Box::new(Expr::Value(json!(1))),
                        ),
                    ),
                );
                Ok(if u.prefix { Expr::Variable(name) } else { old })
            }
            ast::Expr::Call(call) => {
                if matches!(&call.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Member(m) if matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools")))
                {
                    return self.error(call.span,"tools.* is unsupported; use the fixed library.method(args) names in program_tool_headers");
                }
                if matches!(&call.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Ident(i) if i.sym==*"assert"))
                {
                    if !(1..=2).contains(&call.args.len())
                        || call.args.iter().any(|a| a.spread.is_some())
                    {
                        return self.error(
                            call.span,
                            "assert takes a condition and optional failure message",
                        );
                    }
                    let test = self.condition(&call.args[0].expr)?;
                    let branch = self.emit(call.span, Op::Branch(test, 0));
                    let success = self.emit(call.span, Op::Jump(0));
                    let failure = self.code.len();
                    let error = if let Some(message) = call.args.get(1) {
                        self.lower(&message.expr)?
                    } else {
                        Expr::Value(json!({"message":"assertion failed"}))
                    };
                    self.emit(call.span, Op::Throw(error));
                    let end = self.code.len();
                    self.code[branch].op = match &self.code[branch].op {
                        Op::Branch(test, _) => Op::Branch(test.clone(), failure),
                        _ => unreachable!(),
                    };
                    self.code[success].op = Op::Jump(end);
                    return Ok(Expr::Value(json!(true)));
                }
                if call.args.iter().any(|arg| arg.spread.is_some()) {
                    let ast::Callee::Expr(target) = &call.callee else { return self.error(call.span, "spread call needs a function value"); };
                    if matches!(&**target, ast::Expr::Ident(i) if i.sym == *"assert") {
                        return self.error(call.span, "spread is unavailable for this special call");
                    }
                    if let ast::Expr::Member(member) = &**target && self.native_value(target)?.is_none() {
                        let receiver = self.lower(&member.obj)?;
                        let receiver = self.snapshot(member.obj.span(), receiver);
                        let key = self.member_key(&member.prop, member.span)?;
                        return self.call_method_spread(receiver, key, &call.args, call.span);
                    }
                    let callee = self.lower(target)?;
                    let callee = self.snapshot(target.span(), callee);
                    let array = ast::Expr::Array(ast::ArrayLit { span: call.span, elems: call.args.iter().cloned().map(Some).collect() });
                    let args = self.lower(&array)?;
                    let dest = self.temp();
                    self.emit(call.span, Op::Effect(Some(dest.clone()), Effect::Apply { callee, args }));
                    return Ok(Expr::Variable(dest));
                }
                if let ast::Callee::Expr(target) = &call.callee
                    && let ast::Expr::Member(member) = &**target
                    && matches!(&*member.obj, ast::Expr::Ident(i) if i.sym == *"Promise")
                    && self.global("Promise")
                    && let ast::MemberProp::Ident(method) = &member.prop
                    && matches!(method.sym.as_ref(), "resolve" | "reject")
                {
                    if call.args.iter().any(|arg| arg.spread.is_some()) {
                        return self.error(call.span, "Promise.resolve/reject spread arguments are not yet supported");
                    }
                    let mut args = Vec::new();
                    for arg in &call.args {
                        let value = self.lower(&arg.expr)?;
                        args.push(self.snapshot(arg.span(), value));
                    }
                    let value = args.into_iter().next().unwrap_or(Expr::Undefined);
                    let destination = self.temp();
                    self.emit(call.span, Op::Effect(Some(destination.clone()), Effect::PromiseCreate { reject: method.sym == *"reject", value }));
                    return Ok(Expr::Variable(destination));
                }
                if let ast::Callee::Expr(target) = &call.callee
                    && let ast::Expr::Member(member) = &**target
                    && matches!(&*member.obj, ast::Expr::Ident(i) if i.sym == *"Promise")
                    && self.global("Promise")
                    && let ast::MemberProp::Ident(method) = &member.prop
                    && matches!(method.sym.as_ref(), "all" | "allSettled" | "race" | "any")
                {
                    if call.args.iter().any(|arg| arg.spread.is_some()) { return self.error(call.span, "Promise combinator spread arguments are not yet supported"); }
                    let mut args = Vec::new();
                    for arg in &call.args { let value = self.lower(&arg.expr)?; args.push(self.snapshot(arg.span(), value)); }
                    let value = args.into_iter().next().unwrap_or(Expr::Undefined);
                    let name = match method.sym.as_ref() { "all" => "$$promiseAll", "allSettled" => "$$promiseAllSettled", "race" => "$$promiseRace", _ => "$$promiseAny" };
                    let destination = self.temp();
                    self.emit(call.span, Op::Effect(Some(destination.clone()), Effect::Call(name.into(), vec![value])));
                    return Ok(Expr::Variable(destination));
                }
                if let Some(value) = self.builtin_call(call)? {
                    return Ok(value);
                }
                // Local result collection is deterministic, like for...of.
                if let ast::Callee::Expr(target) = &call.callee
                    && let ast::Expr::Member(m) = &**target
                    && let ast::Expr::Ident(array) = &*m.obj
                    && matches!(&m.prop,ast::MemberProp::Ident(p) if p.sym==*"push")
                    && self.resolve(array.sym.as_ref()).is_some()
                {
                    if call.args.is_empty() || call.args.iter().any(|a| a.spread.is_some()) {
                        return self
                            .error(call.span, "array result collection needs explicit values");
                    }
                    let mut items = Vec::new();
                    for arg in &call.args {
                        let value = self.lower(&arg.expr)?;
                        items.push(self.snapshot(arg.span(), value));
                    }
                    let destination = self.temp();
                    self.emit(
                        call.span,
                        Op::Append {
                            target: array.sym.to_string(),
                            items,
                            destination: destination.clone(),
                        },
                    );
                    return Ok(Expr::Variable(destination));
                }
                if let ast::Callee::Expr(target) = &call.callee
                    && let ast::Expr::Member(member) = &**target
                    && let ast::Expr::Ident(namespace) = &*member.obj
                    && matches!(namespace.sym.as_ref(), "path" | "url")
                    && self.resolve(namespace.sym.as_ref()).is_none()
                    && !self.libraries.contains_key(namespace.sym.as_ref())
                {
                    let ast::MemberProp::Ident(method) = &member.prop else {
                        return self.error(call.span, "built-in method must be a static name");
                    };
                    let name = format!("{}.{}", namespace.sym, method.sym);
                    let (min, max) = super::builtins::arity(&name).ok_or_else(|| {
                        let p = self.at(call.span);
                        anyhow::anyhow!(
                            "TS_PROGRAM {}:{}: unsupported built-in {name}",
                            p.line,
                            p.column
                        )
                    })?;
                    if !(min..=max).contains(&call.args.len())
                        || call.args.iter().any(|arg| arg.spread.is_some())
                    {
                        return self.error(call.span, "invalid built-in arguments");
                    }
                    let mut args = Vec::new();
                    for arg in &call.args {
                        let value = self.lower(&arg.expr)?;
                        args.push(self.snapshot(arg.span(), value));
                    }
                    return Ok(Expr::NodeBuiltin(name, args));
                }
                if matches!(&call.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Member(m) if !matches!(&*m.obj,ast::Expr::Ident(i) if self.libraries.contains_key(i.sym.as_ref()))))
                {
                    let ast::Callee::Expr(target) = &call.callee else {
                        unreachable!()
                    };
                    let ast::Expr::Member(member) = &**target else {
                        unreachable!()
                    };
                    let ast::MemberProp::Ident(method) = &member.prop else {
                        // obj[key](…) calls the function stored at that key.
                        return self.dynamic_call(call, target);
                    };
                    if matches!(&*member.obj, ast::Expr::Ident(i) if i.sym == *"JSON") {
                        return self.error(call.span, "only JSON.stringify and JSON.parse are available");
                    }
                    let receiver = self.lower_object(&member.obj)?;
                    let receiver = self.snapshot(member.obj.span(), receiver);
                    return self.call_method_on(receiver, method.sym.as_ref(), &call.args, call.span);
                }
                if call.args.iter().any(|a| a.spread.is_some()) {
                    return self.error(call.span, "argument spread is unsupported");
                }
                let mut args = Vec::new();
                let target = match &call.callee {
                    ast::Callee::Expr(e) => &**e,
                    _ => {
                        return self.error(call.span, "use a declared function or library.method");
                    }
                };
                if !self.static_callee(target) {
                    return self.dynamic_call(call, target);
                }
                for arg in &call.args {
                    let lowered = self.lower(&arg.expr)?;
                    let name = self.temp();
                    self.emit(arg.span(), Op::Set(name.clone(), lowered));
                    args.push(ast::ExprOrSpread {
                        spread: None,
                        expr: Box::new(ast::Expr::Ident(ast::Ident::new_no_ctxt(
                            name.into(),
                            arg.span(),
                        ))),
                    });
                    self.temp_names.insert(match &*args.last().unwrap().expr {
                        ast::Expr::Ident(i) => i.sym.to_string(),
                        _ => unreachable!(),
                    });
                }
                let mut translated = call.clone();
                translated.args = args;
                let effect = self.effect(&translated)?;
                let dest = self.temp();
                self.emit(call.span, Op::Effect(Some(dest.clone()), effect));
                Ok(Expr::Variable(dest))
            }
            ast::Expr::Paren(p) => self.lower(&p.expr),
            ast::Expr::Object(o) if o.props.iter().any(|p| matches!(p, ast::PropOrSpread::Spread(_))) => {
                let target = self.temp();
                self.emit(o.span, Op::Set(target.clone(), Expr::Object(Vec::new())));
                for prop in &o.props {
                    match prop {
                        ast::PropOrSpread::Spread(spread) => {
                            let value = self.lower(&spread.expr)?;
                            let merged = Expr::NodeBuiltin("Object.assign".into(), vec![Expr::Variable(target.clone()), value]);
                            self.emit(spread.dot3_token, Op::Set(target.clone(), merged));
                        }
                        ast::PropOrSpread::Prop(prop) => {
                            let (key, value) = match &**prop {
                                ast::Prop::KeyValue(p) => (self.property(&p.key)?, self.lower(&p.value)?),
                                ast::Prop::Shorthand(i) => (i.sym.to_string(), self.expr(&ast::Expr::Ident(i.clone()))?),
                                ast::Prop::Method(method) => {
                                    let key = self.property(&method.key)?;
                                    let id = self.compile_function(FunctionSource::function(&method.function, Some(key.clone()), None))?;
                                    (key, Expr::Function(id))
                                }
                                _ => return self.error(prop.span(), "object getters and setters are unsupported"),
                            };
                            self.emit(prop.span(), Op::SetMember { object: Expr::Variable(target.clone()), key: Expr::Value(json!(key)), value });
                        }
                    }
                }
                Ok(Expr::Variable(target))
            }
            ast::Expr::Object(o) => {
                let mut fields = Vec::new();
                for p in &o.props {
                    match p {
                        ast::PropOrSpread::Prop(p) => match &**p {
                            ast::Prop::KeyValue(p) => {
                                let key = self.property(&p.key)?;
                                let value = self.lower(&p.value)?;
                                fields.push((key, self.snapshot(p.value.span(), value)));
                            }
                            ast::Prop::Shorthand(i) => {
                                let value = self.expr(&ast::Expr::Ident(i.clone()))?;
                                fields.push((i.sym.to_string(), self.snapshot(i.span, value)));
                            }
                            // `{ run(x) { … } }` is a function-valued property.
                            ast::Prop::Method(method) => {
                                let key = self.property(&method.key)?;
                                let id = self.compile_function(FunctionSource::function(
                                    &method.function,
                                    Some(key.clone()),
                                    None,
                                ))?;
                                fields.push((key, Expr::Function(id)));
                            }
                            _ => return self.error(p.span(), "object getters and setters are unsupported"),
                        },
                        _ => return self.error(o.span, "object spread is unsupported"),
                    }
                }
                Ok(Expr::Object(fields))
            }
            ast::Expr::Array(a) if a.elems.iter().any(|item| item.as_ref().is_some_and(|item| item.spread.is_some())) => {
                let target = self.temp();
                self.emit(a.span, Op::Set(target.clone(), Expr::Array(Vec::new())));
                for item in &a.elems {
                    let Some(item) = item else { return self.error(a.span, "array holes are unsupported"); };
                    let value = self.lower(&item.expr)?;
                    if item.spread.is_some() {
                        let value = self.snapshot(item.span(), value);
                        let array = Expr::Unary("isArray".into(), Box::new(value.clone()));
                        let string = Expr::Binary("===".into(), Box::new(Expr::Unary("typeof".into(), Box::new(value.clone()))), Box::new(Expr::Value(json!("string"))));
                        let invalid = self.emit(item.span(), Op::Branch(Expr::Binary("||".into(), Box::new(array), Box::new(string)), 0));
                        let success = self.emit(item.span(), Op::Jump(0));
                        self.land(invalid);
                        self.emit(item.span(), Op::Throw(Expr::NodeBuiltin("new TypeError".into(), vec![Expr::Value(json!("spread value is not iterable"))])));
                        self.land(success);
                        let source = Expr::NodeBuiltin("Array.from".into(), vec![value]);
                        let combined = Expr::StringMethod(Box::new(Expr::Variable(target.clone())), "concat".into(), vec![source]);
                        self.emit(item.span(), Op::Set(target.clone(), combined));
                    } else {
                        let destination = self.temp();
                        self.emit(item.span(), Op::Append { target: target.clone(), items: vec![value], destination });
                    }
                }
                Ok(Expr::Variable(target))
            }
            ast::Expr::Array(a) => {
                let mut values = Vec::new();
                for e in &a.elems {
                    match e {
                        Some(e) if e.spread.is_none() => {
                            let value = self.lower(&e.expr)?;
                            values.push(self.snapshot(e.span(), value));
                        }
                        _ => return self.error(a.span, "array holes/spread are unsupported"),
                    }
                }
                Ok(Expr::Array(values))
            }
            ast::Expr::Member(m) if self.constant_member(m).is_some() => Ok(self.constant_member(m).unwrap()),
            ast::Expr::Arrow(arrow) => Ok(Expr::Function(
                self.compile_function(FunctionSource::arrow(arrow))?,
            )),
            ast::Expr::Fn(f) => {
                let name = f.ident.as_ref().map(|i| i.sym.to_string());
                Ok(Expr::Function(self.compile_function(FunctionSource::function(
                    &f.function,
                    name.clone(),
                    name,
                ))?))
            }
            ast::Expr::Member(m) => {
                if let Some(native) = self.native_value(e)? {
                    return Ok(native);
                }
                let object = self.lower_object(&m.obj)?;
                let object = self.snapshot(m.obj.span(), object);
                let key = match &m.prop {
                    ast::MemberProp::Ident(i) => Expr::Value(json!(i.sym.to_string())),
                    ast::MemberProp::Computed(c) => self.lower(&c.expr)?,
                    _ => return self.error(m.span, "private members are unsupported"),
                };
                Ok(Expr::Member(Box::new(object), Box::new(key)))
            }
            _ => self.expr(e),
        }
    }
    fn value(&mut self, e: &ast::Expr, dest: String) -> Result<()> {
        let value = self.lower(e)?;
        self.emit(e.span(), Op::Set(dest, value));
        Ok(())
    }
    fn discard(&mut self, e: &ast::Expr) -> Result<()> {
        // Even an unused expression must run and report arithmetic/type errors.
        let value = self.lower(e)?;
        self.snapshot(e.span(), value);
        Ok(())
    }
    /// Evaluate the source once, then initialize the pattern left-to-right.
    fn binding(&mut self, p: &ast::Pat, e: &ast::Expr, declare: bool) -> Result<()> {
        let value = self.lower(e)?;
        let value = self.snapshot(e.span(), value);
        self.bind_value(p, value, declare)
    }
    fn default_value(&mut self, span: Span, value: Expr, default: &ast::Expr) -> Result<Expr> {
        let name = self.temp();
        self.emit(span, Op::Set(name.clone(), value));
        let test = Expr::Binary("===".into(), Box::new(Expr::Variable(name.clone())), Box::new(Expr::Undefined));
        let skip = self.emit(span, Op::Branch(test, 0));
        let replacement = self.lower(default)?;
        self.emit(span, Op::Set(name.clone(), replacement));
        self.land(skip);
        Ok(Expr::Variable(name))
    }
    fn bind_value(&mut self, p: &ast::Pat, value: Expr, declare: bool) -> Result<()> {
        match p {
            ast::Pat::Ident(i) => {
                let op = if declare { Op::Declare(i.id.sym.to_string(), value) } else { Op::Set(i.id.sym.to_string(), value) };
                self.emit(p.span(), op);
            }
            ast::Pat::Assign(a) => {
                let value = self.default_value(a.span, value, &a.right)?;
                self.bind_value(&a.left, value, declare)?;
            }
            ast::Pat::Array(a) => {
                let value = self.snapshot(a.span, value);
                let array = Expr::Unary("isArray".into(), Box::new(value.clone()));
                let string = Expr::Binary("===".into(), Box::new(Expr::Unary("typeof".into(), Box::new(value.clone()))), Box::new(Expr::Value(json!("string"))));
                let valid = Expr::Binary("||".into(), Box::new(array), Box::new(string));
                let invalid = self.emit(a.span, Op::Branch(valid, 0));
                let success = self.emit(a.span, Op::Jump(0));
                self.land(invalid);
                self.emit(a.span, Op::Throw(Expr::NodeBuiltin("new TypeError".into(), vec![Expr::Value(json!("destructuring value is not iterable"))])));
                self.land(success);
                for (index, item) in a.elems.iter().enumerate() {
                    if let Some(item) = item {
                        if let ast::Pat::Rest(rest) = item {
                            let array = Expr::NodeBuiltin("Array.from".into(), vec![value.clone()]);
                            let tail = Expr::StringMethod(Box::new(array), "slice".into(), vec![Expr::Value(json!(index))]);
                            self.bind_value(&rest.arg, tail, declare)?;
                        } else {
                            let member = Expr::Member(Box::new(value.clone()), Box::new(Expr::Value(json!(index))));
                            self.bind_value(item, member, declare)?;
                        }
                    }
                }
            }
            ast::Pat::Object(o) => {
                let value = self.snapshot(o.span, value);
                // Property access rejects null and undefined even for {}.
                self.snapshot(o.span, Expr::Member(Box::new(value.clone()), Box::new(Expr::Value(json!("")))));
                let mut excluded = Vec::new();
                for prop in &o.props {
                    match prop {
                        ast::ObjectPatProp::KeyValue(k) => {
                            let key = match &k.key {
                                ast::PropName::Ident(i) => Expr::Value(json!(i.sym.to_string())),
                                ast::PropName::Str(v) => Expr::Value(json!(v.value.to_string_lossy().into_owned())),
                                ast::PropName::Num(v) => number_expr(v.value),
                                ast::PropName::Computed(v) => { let key = self.lower(&v.expr)?; self.snapshot(v.span, key) },
                                _ => return self.error(k.key.span(), "unsupported destructuring key"),
                            };
                            let key = self.snapshot(k.key.span(), Expr::NodeBuiltin("String".into(), vec![key]));
                            excluded.push(key.clone());
                            self.bind_value(&k.value, Expr::Member(Box::new(value.clone()), Box::new(key)), declare)?;
                        }
                        ast::ObjectPatProp::Assign(a) => {
                            let key = Expr::Value(json!(a.key.id.sym.to_string()));
                            excluded.push(key.clone());
                            let member = Expr::Member(Box::new(value.clone()), Box::new(key));
                            let member = if let Some(default) = &a.value { self.default_value(a.span, member, default)? } else { member };
                            let op = if declare { Op::Declare(a.key.id.sym.to_string(), member) } else { Op::Set(a.key.id.sym.to_string(), member) };
                            self.emit(a.span, op);
                        }
                        ast::ObjectPatProp::Rest(r) => {
                            let copy = Expr::NodeBuiltin("Object.assign".into(), vec![Expr::Object(Vec::new()), value.clone()]);
                            let copy = self.snapshot(r.span, copy);
                            for key in &excluded {
                                self.snapshot(r.span, Expr::NodeBuiltin("delete".into(), vec![copy.clone(), key.clone()]));
                            }
                            self.bind_value(&r.arg, copy, declare)?;
                        }
                    }
                }
            }
            _ => return self.error(p.span(), "unsupported destructuring pattern"),
        }
        Ok(())
    }
    fn loop_head(&mut self, head: &ast::ForHead, span: Span) -> Result<(ast::Pat, Option<ast::VarDeclKind>)> {
        match head {
            ast::ForHead::VarDecl(v) if v.decls.len() == 1 && v.decls[0].init.is_none() => Ok((v.decls[0].name.clone(), Some(v.kind))),
            ast::ForHead::Pat(pattern) => {
                for name in pattern_names(pattern) { self.local_target(&name, span)?; }
                Ok(((**pattern).clone(), None))
            }
            _ => self.error(span, "unsupported loop binding"),
        }
    }
    fn loop_bind(&mut self, pattern: &ast::Pat, value: Expr, declare: bool) -> Result<()> {
        if let ast::Pat::Expr(expression) = pattern {
            if let ast::Expr::Member(member) = &**expression {
                let object = self.lower(&member.obj)?;
                let object = self.snapshot(member.obj.span(), object);
                let key = self.member_key(&member.prop, member.span)?;
                self.emit(member.span, Op::SetMember { object, key, value });
                return Ok(());
            }
            return self.error(pattern.span(), "unsupported loop assignment target");
        }
        self.bind_value(pattern, value, declare)
    }

    fn stmt(&mut self, s: &ast::Stmt) -> Result<()> {
        use ast::Stmt as S;
        match s {
            S::Block(b) => {
                let entered = self.enter_scope(b.span, lexical_names(&b.stmts))?;
                self.hoist_functions(&b.stmts)?;
                for s in &b.stmts {
                    self.stmt(s)?;
                }
                self.leave_scope(b.span, entered);
            }
            S::Empty(_) => {}
            S::Decl(ast::Decl::Var(v)) => self.variable_declaration(v)?,
            // Initialized when the enclosing scope was entered.
            S::Decl(ast::Decl::Fn(_)) => {}
            S::Expr(s) => {
                self.discard(&s.expr)?;
            }
            S::Return(r) => {
                let value = if let Some(e) = &r.arg {
                    self.lower(e)?
                } else {
                    Expr::Undefined
                };
                let value = if self.finalizers.is_empty() { value } else { self.snapshot(r.span, value) };
                self.emit_finalizers(None, r.span)?;
                self.emit(r.span, Op::Return(value));
            }
            S::If(i) => {
                let test = self.condition(&i.test)?;
                let branch = self.emit(i.span, Op::Branch(test, 0));
                self.stmt(&i.cons)?;
                let end = self.emit(i.span, Op::Jump(0));
                let alt = self.code.len();
                if let Some(a) = &i.alt {
                    self.stmt(a)?;
                }
                let done = self.code.len();
                if let Op::Branch(_, t) = &mut self.code[branch].op {
                    *t = alt;
                }
                self.code[end].op = Op::Jump(done);
            }
            S::While(w) => {
                let start = self.code.len();
                let test = self.condition(&w.test)?;
                let exit = self.emit(w.span, Op::Branch(test, 0));
                self.begin_loop();
                self.stmt(&w.body)?;
                self.emit(w.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::Branch(_, target) = &mut self.code[exit].op {
                    *target = end;
                }
                self.end_loop(end, start);
            }
            S::DoWhile(w) => {
                let start = self.code.len();
                self.begin_loop();
                self.stmt(&w.body)?;
                let next = self.code.len();
                let test = self.condition(&w.test)?;
                let exit = self.emit(w.span, Op::Branch(test, 0));
                self.emit(w.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::Branch(_, target) = &mut self.code[exit].op {
                    *target = end;
                }
                self.end_loop(end, next);
            }
            S::For(f) => {
                // `for (let …)` gets its own scope, copied for every iteration so
                // each iteration has fresh bindings.
                let mut entered = false;
                if let Some(init) = &f.init {
                    match init {
                        ast::VarDeclOrExpr::VarDecl(v) => {
                            if v.kind != ast::VarDeclKind::Var {
                                let names = v
                                    .decls
                                    .iter()
                                    .flat_map(|d| pattern_names(&d.name))
                                    .map(|name| (name, v.kind == ast::VarDeclKind::Let))
                                    .collect();
                                entered = self.enter_scope(f.span, names)?;
                            }
                            self.variable_declaration(v)?
                        }
                        ast::VarDeclOrExpr::Expr(e) => {
                            self.discard(e)?;
                        }
                    }
                }
                if entered {
                    self.emit(f.span, Op::CopyScope);
                }
                let start = self.code.len();
                let test = if let Some(test) = &f.test {
                    self.condition(test)?
                } else {
                    Expr::Value(json!(true))
                };
                let exit = self.emit(f.span, Op::Branch(test, 0));
                self.begin_loop();
                self.stmt(&f.body)?;
                let next = self.code.len();
                if entered {
                    self.emit(f.span, Op::CopyScope);
                }
                if let Some(update) = &f.update {
                    self.discard(update)?;
                }
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::Branch(_, target) = &mut self.code[exit].op {
                    *target = end;
                }
                self.end_loop(end, next);
                self.leave_scope(f.span, entered);
            }
            S::Labeled(label) => {
                fn targets_loop(stmt: &ast::Stmt) -> bool {
                    match stmt {
                        ast::Stmt::For(_) | ast::Stmt::ForIn(_) | ast::Stmt::ForOf(_) | ast::Stmt::While(_) | ast::Stmt::DoWhile(_) => true,
                        ast::Stmt::Labeled(inner) => targets_loop(&inner.body),
                        _ => false,
                    }
                }
                let name = label.label.sym.to_string();
                if self.loops.iter().any(|context| context.label.as_deref() == Some(&name)) {
                    return self.error(label.span, "duplicate label");
                }
                self.begin_loop();
                let context = self.loops.last_mut().unwrap();
                context.is_switch = true;
                context.label = Some(name);
                context.label_loop = targets_loop(&label.body);
                self.stmt(&label.body)?;
                let end = self.code.len();
                self.end_loop(end, end);
            }
            S::Switch(switch) => {
                let discriminant = self.lower(&switch.discriminant)?;
                let discriminant = self.snapshot(switch.discriminant.span(), discriminant);
                let statements: Vec<_> = switch.cases.iter().flat_map(|case| case.cons.iter().cloned()).collect();
                let entered = self.enter_scope(switch.span, lexical_names(&statements))?;
                self.hoist_functions(&statements)?;
                let mut jumps = Vec::new();
                let mut default = None;
                for (index, case) in switch.cases.iter().enumerate() {
                    if let Some(test) = &case.test {
                        let value = self.lower(test)?;
                        let equal = Expr::Binary("===".into(), Box::new(discriminant.clone()), Box::new(value));
                        let next = self.emit(case.span, Op::Branch(equal, 0));
                        jumps.push((index, self.emit(case.span, Op::Jump(0))));
                        self.land(next);
                    } else { default = Some(index); }
                }
                let unmatched = self.emit(switch.span, Op::Jump(0));
                self.begin_loop();
                self.loops.last_mut().unwrap().is_switch = true;
                let mut locations = Vec::new();
                for case in &switch.cases {
                    locations.push(self.code.len());
                    for stmt in &case.cons { self.stmt(stmt)?; }
                }
                let end = self.code.len();
                for (index, jump) in jumps { self.code[jump].op = Op::Jump(locations[index]); }
                self.code[unmatched].op = Op::Jump(default.map(|i| locations[i]).unwrap_or(end));
                self.end_loop(end, end);
                self.leave_scope(switch.span, entered);
            }
            S::Break(b) => {
                let target = if let Some(label) = &b.label {
                    self.loops.iter().rposition(|context| context.label.as_deref() == Some(label.sym.as_ref()))
                } else {
                    self.loops.iter().rposition(|context| context.label.is_none())
                };
                let Some(target) = target else { return self.error(b.span, "break requires an enclosing loop, switch, or matching label"); };
                let remaining = self.emit_finalizers(Some(target), b.span)?;
                self.leave_to_loop(b.span, target, remaining);
                let at = self.emit(b.span, Op::Jump(0));
                self.loops[target].breaks.push(at);
            }
            S::Continue(c) => {
                let target = if let Some(label) = &c.label {
                    self.loops.iter().rposition(|context| context.label.as_deref() == Some(label.sym.as_ref()) && context.label_loop)
                        .and_then(|index| self.loops.iter().enumerate().skip(index + 1).find(|(_, context)| !context.is_switch).map(|(index, _)| index))
                } else {
                    self.loops.iter().rposition(|context| !context.is_switch)
                };
                let Some(target) = target else { return self.error(c.span, "continue requires an enclosing loop or matching loop label"); };
                let remaining = self.emit_finalizers(Some(target), c.span)?;
                self.leave_to_loop(c.span, target, remaining);
                let at = self.emit(c.span, Op::Jump(0));
                self.loops[target].continues.push(at);
            }
            S::ForIn(f) => {
                let (pattern, kind) = self.loop_head(&f.left, f.span)?;
                let head_scope = if matches!(kind, Some(ast::VarDeclKind::Let | ast::VarDeclKind::Const)) {
                    let names = pattern_names(&pattern).into_iter().map(|name| (name, kind == Some(ast::VarDeclKind::Let))).collect();
                    self.enter_scope(f.span, names)?
                } else { false };
                let items = self.temp();
                let index = self.temp();
                let item = self.temp();
                let source = self.lower(&f.right)?;
                let source = self.snapshot(f.right.span(), source);
                let is_null = Expr::Binary("==".into(), Box::new(source.clone()), Box::new(Expr::Value(Value::Null)));
                let keys = Expr::Conditional(Box::new(is_null), Box::new(Expr::Array(Vec::new())), Box::new(Expr::NodeBuiltin("Object.keys".into(), vec![source.clone()])));
                self.emit(f.span, Op::Set(items.clone(), keys));
                self.emit(f.span, Op::Set(index.clone(), Expr::Value(json!(0))));
                let start = self.code.len();
                let next = self.emit(f.span, Op::ForNext { items, index, item: item.clone(), end: 0 });
                // JS skips a property deleted before it is visited. Primitive
                // strings have immutable indexed properties.
                let object = Expr::Binary("===".into(), Box::new(Expr::Unary("typeof".into(), Box::new(source.clone()))), Box::new(Expr::Value(json!("object"))));
                let present = Expr::Binary("in".into(), Box::new(Expr::Variable(item.clone())), Box::new(source));
                self.emit(f.span, Op::Branch(Expr::Conditional(Box::new(object), Box::new(present), Box::new(Expr::Value(json!(true)))), start));
                self.begin_loop();
                let entered = if matches!(kind, Some(ast::VarDeclKind::Let | ast::VarDeclKind::Const)) {
                    let names = pattern_names(&pattern).into_iter().map(|name| (name, kind == Some(ast::VarDeclKind::Let))).collect();
                    self.enter_scope(f.span, names)?
                } else { false };
                self.loop_bind(&pattern, Expr::Variable(item), entered)?;
                self.stmt(&f.body)?;
                self.leave_scope(f.span, entered);
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::ForNext { end: target, .. } = &mut self.code[next].op { *target = end; }
                self.end_loop(end, start);
                self.leave_scope(f.span, head_scope);
            }
            S::ForOf(f) => {
                if f.is_await {
                    return self.error(f.span, "for await is unsupported");
                }
                let (pattern, kind) = self.loop_head(&f.left, f.span)?;
                let head_scope = if matches!(kind, Some(ast::VarDeclKind::Let | ast::VarDeclKind::Const)) {
                    let names = pattern_names(&pattern).into_iter().map(|name| (name, kind == Some(ast::VarDeclKind::Let))).collect();
                    self.enter_scope(f.span, names)?
                } else { false };
                let items = self.temp();
                let index = self.temp();
                let item = self.temp();
                let list = self.lower(&f.right)?;
                self.emit(f.span, Op::Set(items.clone(), list));
                self.emit(f.span, Op::Set(index.clone(), Expr::Value(json!(0))));
                let start = self.code.len();
                let next = self.emit(
                    f.span,
                    Op::ForNext {
                        items,
                        index,
                        item: item.clone(),
                        end: 0,
                    },
                );
                // A let/const binding is fresh in every iteration; a var is hoisted.
                self.begin_loop();
                let entered = if matches!(kind, Some(ast::VarDeclKind::Let | ast::VarDeclKind::Const)) {
                    let names = pattern_names(&pattern).into_iter().map(|name| (name, kind == Some(ast::VarDeclKind::Let))).collect();
                    self.enter_scope(f.span, names)?
                } else { false };
                self.loop_bind(&pattern, Expr::Variable(item), entered)?;
                self.stmt(&f.body)?;
                self.leave_scope(f.span, entered);
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::ForNext { end: target, .. } = &mut self.code[next].op {
                    *target = end;
                }
                self.end_loop(end, start);
                self.leave_scope(f.span, head_scope);
            }

            S::Try(t) => {
                if let Some(finalizer) = &t.finalizer {
                    let context = FinallyContext {
                        block: finalizer.clone(), handlers: self.handlers,
                        runtime_scopes: self.runtime_scopes, scopes_len: self.scopes.len(), loops_len: self.loops.len(),
                    };
                    let thrown = self.temp();
                    let outer = self.emit(t.span, Op::Catch { target: 0, name: thrown.clone() });
                    self.handlers += 1;
                    self.finalizers.push(context);
                    if t.handler.is_some() {
                        let mut inner = t.clone();
                        inner.finalizer = None;
                        self.stmt(&ast::Stmt::Try(inner))?;
                    } else {
                        self.stmt(&ast::Stmt::Block(t.block.clone()))?;
                    }
                    self.finalizers.pop();
                    self.handlers -= 1;
                    self.emit(t.span, Op::EndCatch);
                    self.stmt(&ast::Stmt::Block(finalizer.clone()))?;
                    let end_jump = self.emit(t.span, Op::Jump(0));
                    let handler = self.code.len();
                    self.stmt(&ast::Stmt::Block(finalizer.clone()))?;
                    self.emit(t.span, Op::Throw(Expr::Variable(thrown)));
                    let end = self.code.len();
                    if let Op::Catch { target, .. } = &mut self.code[outer].op { *target = handler; }
                    self.code[end_jump].op = Op::Jump(end);
                    return Ok(());
                }
                let h = t.handler.as_ref().context("try requires catch")?;
                let param = h.param.as_ref();
                // The handler receives the thrown value in a temporary; the
                // catch parameter is a binding of the handler's own scope.
                let thrown = self.temp();
                let start = self.emit(
                    t.span,
                    Op::Catch {
                        target: 0,
                        name: thrown.clone(),
                    },
                );
                self.handlers += 1;
                let entered = self.enter_scope(t.block.span, lexical_names(&t.block.stmts))?;
                self.hoist_functions(&t.block.stmts)?;
                for s in &t.block.stmts {
                    self.stmt(s)?;
                }
                self.leave_scope(t.block.span, entered);
                self.handlers -= 1;
                self.emit(t.span, Op::EndCatch);
                let jump = self.emit(t.span, Op::Jump(0));
                let handler = self.code.len();
                let mut names: Vec<(String, bool)> = param
                    .map(pattern_names).unwrap_or_default().into_iter().map(|name| (name, true)).collect();
                names.extend(lexical_names(&h.body.stmts));
                let entered = self.enter_scope(h.span, names)?;
                if let Some(param) = param {
                    self.bind_value(param, Expr::Variable(thrown), true)?;
                }
                self.hoist_functions(&h.body.stmts)?;
                for s in &h.body.stmts {
                    self.stmt(s)?;
                }
                self.leave_scope(h.span, entered);
                let end = self.code.len();
                if let Op::Catch { target, .. } = &mut self.code[start].op {
                    *target = handler;
                }
                self.code[jump].op = Op::Jump(end);
            }
            S::Throw(t) => {
                let value = self.lower(&t.arg)?;
                self.emit(t.span, Op::Throw(value));
            }
            _ => return self.error(
                s.span(),
                "unsupported statement; use declarations, if, for...of, return and try/catch/throw",
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
pub fn compile(source: &str) -> Result<Program> {
    compile_with_libraries(source, super::libraries::builtin())
}
#[cfg(test)]
pub fn compile_body(source: &str, entry: &str) -> Result<Program> {
    compile_body_with_libraries(source, entry, super::libraries::builtin())
}
pub fn compile_with_libraries(
    source: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    link_prelude(compile_inner(source, super::ENTRY, false, libraries, BTreeSet::new(), false)?)
}
#[cfg(test)]
pub fn compile_body_with_libraries(
    source: &str,
    entry: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    compile_body_with_scope(source, entry, libraries, BTreeSet::new())
}
pub fn compile_body_with_scope(
    source: &str,
    entry: &str,
    libraries: super::libraries::Libraries,
    external_names: BTreeSet<String>,
) -> Result<Program> {
    let program = link_prelude(compile_inner(source, entry, true, libraries, external_names, false)?)?;
    ensure!(
        !program.functions[entry].virtual_task,
        "the implementation of {entry} needs a real body; do not mark it @virtual again"
    );
    Ok(program)
}
#[cfg(test)]
pub fn compile_entry(source: &str, entry: &str) -> Result<Program> {
    compile_entry_with_libraries(source, entry, super::libraries::builtin())
}
#[cfg(test)]
pub fn compile_entry_with_libraries(
    source: &str,
    entry: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    link_prelude(compile_inner(source, entry, false, libraries, BTreeSet::new(), false)?)
}
fn compile_inner(
    source: &str,
    entry: &str,
    refinement: bool,
    libraries: super::libraries::Libraries,
    external_names: BTreeSet<String>,
    internal: bool,
) -> Result<Program> {
    ensure!(
        source.len() <= 100_000,
        "TS_PROGRAM source exceeds 100000 bytes"
    );
    let map: Lrc<SourceMap> = Default::default();
    let file = map.new_source_file(
        FileName::Custom("agent-program.js".into()).into(),
        source.to_owned(),
    );
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        Default::default(),
        StringInput::from(&*file),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser.parse_module().map_err(|e| {
        let p = map.lookup_char_pos(e.span().lo);
        anyhow::anyhow!("TS_PROGRAM {}:{}: {:?}", p.line, p.col.0 + 1, e.kind())
    })?;
    if let Some(e) = parser.take_errors().first() {
        let p = map.lookup_char_pos(e.span().lo);
        bail!("TS_PROGRAM {}:{}: {:?}", p.line, p.col.0 + 1, e.kind());
    }
    if !internal {
        struct Reserved(bool);
        impl Visit for Reserved {
            fn visit_ident(&mut self, node: &ast::Ident) {
                if node.sym.starts_with("$$") { self.0 = true; }
            }
        }
        let mut reserved = Reserved(false);
        module.visit_with(&mut reserved);
        ensure!(!reserved.0, "TS_PROGRAM: names beginning with $$ are reserved for the prelude");
    }
    let (leading, trailing) = comments.take_all();
    let mut notes = leading
        .borrow()
        .values()
        .chain(trailing.borrow().values())
        .flat_map(|v| v.clone())
        .collect::<Vec<_>>();
    notes.sort_by_key(|c| c.span.lo);
    notes.dedup_by_key(|c| (c.span.lo, c.span.hi));
    let external = external_names;
    let mut declarations = Vec::new();
    let mut top_level = Vec::new();
    let mut imports = BTreeMap::new();
    let mut imported_libraries = super::libraries::Libraries::new();
    let mut imported_names = BTreeSet::new();
    let mut has_import = false;
    // Every function declaration, nested ones included, for their contracts.
    struct Declarations(Vec<ast::FnDecl>);
    impl Visit for Declarations {
        fn visit_fn_decl(&mut self, node: &ast::FnDecl) {
            self.0.push(node.clone());
            node.visit_children_with(self);
        }
    }
    let mut all_declarations = Declarations(Vec::new());
    module.visit_with(&mut all_declarations);
    for item in module.body {
        match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
                has_import = true;
                let module_name = import.src.value.to_string_lossy();
                let Some(namespace) = module_name.strip_prefix("host:") else {
                    bail!("TS_PROGRAM 1:1: tool imports must use host:<library>");
                };
                let (library, asynchronous) = namespace.strip_suffix("/promises").map(|base| (base, true)).unwrap_or((namespace, false));
                let Some(base_methods) = libraries.get(library) else {
                    bail!("TS_PROGRAM 1:1: unknown or unavailable tool library {namespace}");
                };
                let methods: BTreeMap<String, String> = base_methods.iter().map(|(name, tool)| (name.clone(), if asynchronous { format!("async-tool:{tool}") } else { tool.clone() })).collect();
                ensure!(
                    !import.type_only && !import.specifiers.is_empty(),
                    "TS_PROGRAM 1:1: import tool methods or a namespace"
                );
                for spec in import.specifiers {
                    let (local, method) = match spec {
                        ast::ImportSpecifier::Named(named) => {
                            ensure!(
                                !named.is_type_only,
                                "TS_PROGRAM 1:1: type imports are unsupported"
                            );
                            let method = match named.imported {
                                Some(ast::ModuleExportName::Ident(i)) => i.sym.to_string(),
                                Some(ast::ModuleExportName::Str(s)) => {
                                    s.value.to_string_lossy().into_owned()
                                }
                                None => named.local.sym.to_string(),
                            };
                            (named.local.sym.to_string(), Some(method))
                        }
                        ast::ImportSpecifier::Namespace(ns) => (ns.local.sym.to_string(), None),
                        ast::ImportSpecifier::Default(_) => {
                            bail!("TS_PROGRAM 1:1: default tool imports are unsupported")
                        }
                    };
                    ensure!(
                        !super::libraries::reserved(&local) && imported_names.insert(local.clone()),
                        "TS_PROGRAM 1:1: duplicate or reserved import {local}"
                    );
                    if let Some(method) = method {
                        let tool = methods.get(&method).with_context(|| {
                            format!("TS_PROGRAM 1:1: unknown tool {namespace}.{method}")
                        })?;
                        imports.insert(local, tool.clone());
                    } else {
                        imported_libraries.insert(local, methods.clone());
                    }
                }
            }
            ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(f))) => declarations.push(f),
            ast::ModuleItem::Stmt(stmt) if !refinement && entry == super::ENTRY => top_level.push(stmt),
            other => {
                let p = map.lookup_char_pos(other.span().lo);
                bail!(
                    "TS_PROGRAM {}:{}: unsupported module declaration or statement in function refinement",
                    p.line,
                    p.col.0 + 1
                );
            }
        }
    }
    let imported_tools = imports
        .values()
        .chain(imported_libraries.values().flat_map(|methods| methods.values()))
        .map(|tool| tool.trim_start_matches("async-tool:").to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    // The top-level statements are the program, as in a JavaScript module.
    let implicit_entry = !refinement && entry == super::ENTRY;
    let libraries = if has_import {
        imported_libraries
    } else {
        libraries
    };
    ensure!(
        declarations.len() <= 64,
        "TS_PROGRAM supports at most 64 functions"
    );
    let mut names = BTreeSet::new();
    if implicit_entry {
        names.insert(super::ENTRY.into());
    }
    for f in &declarations {
        ensure!(
            !imported_names.contains(f.ident.sym.as_ref()),
            "function name conflicts with import {}",
            f.ident.sym
        );
        ensure!(
            !libraries.contains_key(f.ident.sym.as_ref()),
            "function name conflicts with library namespace {}",
            f.ident.sym
        );
        ensure!(
            !matches!(
                f.ident.sym.as_ref(),
                "tools" | "call" | "ai" | "assert" | "JSON" | "Promise"
            ),
            "reserved function name {}",
            f.ident.sym
        );
        ensure!(
            names.insert(f.ident.sym.to_string()),
            "duplicate function {}",
            f.ident.sym
        );
    }
    ensure!(
        names.contains(entry),
        "TS_PROGRAM 1:1: declare the required entry function {entry}"
    );
    let mut functions = BTreeMap::new();
    let mut arities = declarations
        .iter()
        .map(|f| (f.ident.sym.to_string(), f.function.params.len()))
        .collect::<BTreeMap<_, _>>();
    if implicit_entry {
        arities.insert(super::ENTRY.into(), 0);
    }
    let async_entry = declarations
        .iter()
        .find(|f| f.ident.sym.as_ref() == entry && f.function.is_async)
        .map(|f| f.ident.sym.to_string());
    // A leading JSDoc and comments inside the body form the task contract.
    // Walk only adjacent comments: a tag inside the previous function must
    // never attach to the next declaration. Every contract is read first, so
    // calls compiled before a declaration know whether it is AI work.
    let contract_of = |f: &ast::FnDecl| -> (bool, String) {
        let Some(body) = &f.function.body else {
            return (false, String::new());
        };
        let mut header_start = f.function.span.lo;
        let mut header = Vec::new();
        for note in notes.iter().rev().filter(|n| n.span.hi <= f.function.span.lo) {
            let lo = (note.span.hi.0 - file.start_pos.0) as usize;
            let hi = (header_start.0 - file.start_pos.0) as usize;
            // A blank line ends the header: a comment above it belongs to the code before.
            if !source.get(lo..hi).is_some_and(|gap| gap.trim().is_empty() && gap.matches('\n').count() <= 1) {
                break;
            }
            header.push(note.text.to_string());
            header_start = note.span.lo;
        }
        header.reverse();
        let marked = |text: &str| {
            text.split(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '@' | '_')))
                .any(|word| word == "@virtual")
        };
        let inner = notes
            .iter()
            .filter(|n| n.span.lo >= body.span.lo && n.span.hi <= body.span.hi)
            .map(|n| n.text.to_string())
            .collect::<Vec<_>>();
        // The header marks a function virtual, and so does a marker in a body
        // that holds only comments. A nested helper's marker belongs to it.
        let comment_only = body.stmts.iter().all(|s| matches!(s, ast::Stmt::Empty(_)));
        let virtual_task = marked(&header.join("\n")) || (comment_only && marked(&inner.join("\n")));
        header.extend(inner);
        let contract = header.join("\n");
        let description = contract
            .lines()
            .map(|line| line.trim().trim_start_matches('*').trim())
            .map(|line| {
                if virtual_task {
                    line.replace("@virtual", "")
                } else {
                    line.to_owned()
                }
            })
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (virtual_task, description)
    };
    let contracts = declarations
        .iter()
        .map(|f| (f.ident.sym.to_string(), contract_of(f)))
        .collect::<BTreeMap<_, _>>();
    let contracts_by_position = std::sync::Arc::new(
        all_declarations.0.iter().map(|f| (f.function.span.lo.0, contract_of(f))).collect::<BTreeMap<_, _>>(),
    );
    let leaf_tools = refinement.then(|| imported_tools.clone());
    let virtuals = contracts
        .iter()
        .filter(|(_, (virtual_task, _))| *virtual_task)
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let async_functions = declarations
        .iter()
        .filter(|f| f.function.is_async)
        .map(|f| f.ident.sym.to_string())
        .collect::<BTreeSet<_>>();
    // Top-level declarations form the module scope. The top-level flow runs
    // in it, and every top-level function sees it through its scope chain.
    // Top-level functions are bindings of it too, so they can be used as values.
    let function_bindings = declarations
        .iter()
        .map(|f| (f.ident.sym.to_string(), false))
        .collect::<BTreeMap<_, _>>();
    let mut module_scope = function_bindings.clone();
    if implicit_entry {
        let module_vars = var_names(&top_level);
        let module_lexical = lexical_names(&top_level);
        let mut c = Compiler {
            map: map.clone(),
            names: names.clone(),
            arities: arities.clone(),
            async_entry: None,
            libraries: libraries.clone(),
            imports: imports.clone(),
            scopes: vec![BTreeMap::new()],
            external: external.clone(),
            temp_names: BTreeSet::new(),
            runtime_scopes: 0,
            function_id: super::ENTRY.into(),
            nested: Vec::new(),
            virtuals: virtuals.clone(),
            async_functions: async_functions.clone(),
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            finalizers: Vec::new(),
            chain_exits: None,
            chain_object: false,
            contracts: contracts_by_position.clone(),
            leaf_tools: leaf_tools.clone(),
        };
        for name in &module_vars {
            c.declare(name, true, module.span)?;
        }
        for (name, mutable) in &module_lexical {
            c.declare(name, *mutable, module.span)?;
        }
        if let Some(name) = c.scopes[0].keys().find(|name| names.contains(*name)) {
            return c.error(
                module.span,
                &format!("{name} is declared both as a function and as a variable"),
            );
        }
        c.scopes[0].extend(function_bindings.clone());
        module_scope = c.scopes[0].clone();
        for stmt in &top_level {
            c.stmt(stmt)?;
        }
        c.emit(module.span, Op::Return(Expr::Undefined));
        functions.extend(std::mem::take(&mut c.nested));
        functions.insert(
            super::ENTRY.into(),
            Function {
                uses: Vec::new(),
                description: String::new(),
                virtual_task: false,
                leaf_tools: None,
                params: Vec::new(),
                rest_parameter: None,
                parameter_scope: false,
                parameter_lexical: Vec::new(),
                vars: module_vars,
                lexical: module_lexical.into_iter().map(|(name, _)| name).collect(),
                module: true,
                is_async: true,
                self_name: None,
                code: c.code,
            },
        );
    }
    for f in declarations {
        let mut c = Compiler {
            map: map.clone(),
            names: names.clone(),
            arities: arities.clone(),
            async_entry: async_entry.clone(),
            libraries: libraries.clone(),
            imports: imports.clone(),
            scopes: vec![module_scope.clone(), BTreeMap::new()],
            external: external.clone(),
            temp_names: BTreeSet::new(),
            runtime_scopes: 0,
            function_id: f.ident.sym.to_string(),
            nested: Vec::new(),
            virtuals: virtuals.clone(),
            async_functions: async_functions.clone(),
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            finalizers: Vec::new(),
            chain_exits: None,
            chain_object: false,
            contracts: contracts_by_position.clone(),
            leaf_tools: leaf_tools.clone(),
        };
        if f.function.is_generator {
            return c.error(f.function.span, "generator functions are unsupported");
        }
        let mut params = Vec::new();
        let mut parameter_names = Vec::new();
        let mut parameter_lexical = Vec::new();
        let mut parameter_patterns = Vec::new();
        let patterned = f.function.params.iter().any(|p| !matches!(p.pat, ast::Pat::Ident(_)));
        let rest_parameter = f.function.params.iter().position(|p| matches!(p.pat, ast::Pat::Rest(_)));
        for (index, p) in f.function.params.iter().enumerate() {
            let names = pattern_names(&p.pat);
            for name in &names { c.declare(name, true, p.span)?; }
            parameter_names.extend(names.iter().cloned());
            if let ast::Pat::Ident(i) = &p.pat && !patterned {
                params.push(i.id.sym.to_string());
            } else {
                let argument = format!("$$param{index}");
                params.push(argument.clone());
                parameter_lexical.extend(names);
                let pattern = match &p.pat { ast::Pat::Rest(rest) => &*rest.arg, other => other };
                parameter_patterns.push((pattern, argument));
            }
        }
        let body = f
            .function
            .body
            .as_ref()
            .context("function body is required")?;
        let (virtual_task, description) = contracts[f.ident.sym.as_ref()].clone();
        if virtual_task && !parameter_patterns.is_empty() {
            return c.error(f.function.span, "@virtual functions require simple parameters");
        }
        for (pattern, argument) in parameter_patterns {
            c.bind_value(pattern, Expr::Variable(argument), true)?;
        }
        let vars: Vec<String> = var_names(&body.stmts).into_iter()
            .filter(|name| patterned || !parameter_names.contains(name)).collect();
        let lexical = lexical_names(&body.stmts);
        let lexical_only = body.stmts.iter().filter(|stmt| matches!(stmt, ast::Stmt::Decl(ast::Decl::Var(v)) if v.kind != ast::VarDeclKind::Var)).cloned().collect::<Vec<_>>();
        if let Some((name, _)) = lexical_names(&lexical_only).iter().find(|(name, _)| parameter_names.contains(name)) {
            return c.error(body.span, &format!("duplicate parameter and lexical binding {name}"));
        }
        if patterned { c.scopes.push(BTreeMap::new()); }
        for name in &vars { c.declare(name, true, body.span)?; }
        for (name, mutable) in &lexical { c.declare(name, *mutable, body.span)?; }
        if patterned { c.emit(body.span, Op::EnterFunctionBody); }
        let empty = body.stmts.iter().all(|s| matches!(s, ast::Stmt::Empty(_)));
        // A sole `throw new Error("literal")` is a conventional unavailable-implementation
        // sentinel. Do not ignore arbitrary throws, expressions or side effects.
        let executable = body
            .stmts
            .iter()
            .filter(|s| !matches!(s, ast::Stmt::Empty(_)))
            .collect::<Vec<_>>();
        let error_placeholder = matches!(executable.as_slice(), [ast::Stmt::Throw(t)]
            if matches!(&*t.arg, ast::Expr::New(n)
                if matches!(&*n.callee, ast::Expr::Ident(i) if i.sym == *"Error")
                    && n.args.as_ref().is_some_and(|args| args.len() == 1
                        && args[0].spread.is_none()
                        && matches!(&*args[0].expr, ast::Expr::Lit(ast::Lit::Str(_))))));
        if virtual_task {
            if !empty && !error_placeholder {
                return c.error(body.span,
                    "@virtual declares AI work: use an empty body or a sole throw new Error with a literal message; executable work and placeholder returns are unsupported");
            }
            if description.is_empty() {
                return c.error(
                    body.span,
                    "@virtual needs a task description and return contract",
                );
            }
            let uses = c.captures(&description, &params, f.function.span)?;
            let input = Expr::Object(
                params
                    .iter()
                    .chain(&uses)
                    .map(|p| (p.clone(), Expr::Variable(p.clone())))
                    .collect(),
            );
            let dest = c.temp();
            c.emit(
                body.span,
                Op::Effect(
                    Some(dest.clone()),
                    Effect::Task {
                        name: f.ident.sym.to_string(),
                        description: description.clone(),
                        input,
                    },
                ),
            );
            c.emit(body.span, Op::Return(Expr::Variable(dest)));
        } else {
            if empty {
                return c.error(
                    body.span,
                    "an empty helper must be explicitly marked @virtual with its task description and return contract",
                );
            }
            c.hoist_functions(&body.stmts)?;
            for s in &body.stmts {
                c.stmt(s)?;
            }
            c.emit(body.span, Op::Return(Expr::Undefined));
        }
        functions.extend(std::mem::take(&mut c.nested));
        let uses = if virtual_task { c.captures(&description, &params, f.function.span)? } else { Vec::new() };
        functions.insert(
            f.ident.sym.to_string(),
            Function {
                description,
                virtual_task,
                uses,
                leaf_tools: if virtual_task { leaf_tools.clone() } else { None },
                params,
                rest_parameter,
                parameter_scope: patterned,
                parameter_lexical,
                vars,
                lexical: lexical.into_iter().map(|(name, _)| name).collect(),
                module: false,
                is_async: f.function.is_async,
                self_name: None,
                code: c.code,
            },
        );
    }
    ensure!(
        functions.len() <= 1024,
        "TS_PROGRAM supports at most 1024 functions, including nested functions and arrows"
    );
    Ok(Program {
        version: VERSION,
        source: source.into(),
        prelude_hash: String::new(),
        functions,
    })
}

fn link_prelude(mut program: Program) -> Result<Program> {
    for (name, function) in &super::prelude::program().functions {
        if name.starts_with("$$") { program.functions.insert(name.clone(), function.clone()); }
    }
    program.prelude_hash = super::prelude::hash();
    Ok(program)
}

pub(super) fn compile_prelude() -> Result<Program> {
    compile_inner(super::prelude::SOURCE, "$$map", false, super::libraries::Libraries::new(), BTreeSet::new(), true)
}
