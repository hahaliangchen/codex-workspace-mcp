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
    /// Names inherited from the calling scope (task bodies and fork children).
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
    code: Vec<Instruction>,
    temp: usize,
    loops: Vec<LoopContext>,
    handlers: usize,
    /// Branches that leave the optional chain being compiled.
    chain_exits: Option<Vec<usize>>,
    /// The next lowered expression is the object of the current chain link.
    chain_object: bool,
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

#[derive(Default)]
struct LoopContext {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    handlers: usize,
    scopes: usize,
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
        self.lower(e)
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
    fn leave_to_loop(&mut self, span: Span) {
        let context = self.loops.last().expect("inside a loop");
        let (handlers, scopes) = (context.handlers, context.scopes);
        for _ in handlers..self.handlers {
            self.emit(span, Op::EndCatch);
        }
        for _ in scopes..self.runtime_scopes {
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
            "tools" | "call" | "fork" | "join" | "ai" | "assert" | "JSON" | "Promise"
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
    /// A callee resolved at compile time: fork/join, an imported tool, a
    /// library method or a top-level function that no inner binding shadows.
    /// Anything else is a function value called at run time.
    fn static_callee(&self, target: &ast::Expr) -> bool {
        match target {
            ast::Expr::Ident(i) => {
                let name = i.sym.as_ref();
                matches!(name, "fork" | "join")
                    || self.imports.contains_key(name)
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
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            chain_exits: None,
            chain_object: false,
        };
        let mut params = Vec::new();
        for pattern in &source.params {
            let ast::Pat::Ident(i) = pattern else {
                return c.error(pattern.span(), "use simple function parameters");
            };
            c.declare(i.id.sym.as_ref(), true, pattern.span())?;
            params.push(i.id.sym.to_string());
        }
        let (vars, lexical) = match &source.body {
            FunctionBody::Block(body) => {
                let vars: Vec<String> = var_names(&body.stmts)
                    .into_iter()
                    .filter(|name| !params.contains(name))
                    .collect();
                for name in &vars {
                    c.declare(name, true, body.span)?;
                }
                let lexical = lexical_names(&body.stmts);
                for (name, mutable) in &lexical {
                    c.declare(name, *mutable, body.span)?;
                }
                (vars, lexical)
            }
            _ => (Vec::new(), Vec::new()),
        };
        // A named function expression sees itself unless its own names shadow it.
        if let Some(name) = &source.self_name {
            let scope = c.scopes.last_mut().expect("function scope");
            if !scope.contains_key(name) {
                scope.insert(name.clone(), false);
            }
        }
        match source.body {
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
        self.nested.push((
            id.clone(),
            Function {
                description: String::new(),
                virtual_task: false,
                params,
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
        if matches!(&**target, ast::Expr::Ident(i) if i.sym == *"join") {
            if call.args.is_empty() {
                return self.error(call.span, "join needs fork handles");
            }
            return Ok(Effect::Join(
                call.args
                    .iter()
                    .map(|a| self.expr(&a.expr))
                    .collect::<Result<_>>()?,
            ));
        }
        let fork = matches!(&**target, ast::Expr::Ident(i) if i.sym == *"fork");
        let (target, actuals) = if fork {
            let Some(first) = call.args.first() else {
                return self.error(call.span, "fork needs a declared helper");
            };
            (&*first.expr, &call.args[1..])
        } else {
            (&**target, &call.args[..])
        };
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
                if i.sym == *"main" {
                    return self.error(call.span, "main is the sole entry, not a callable helper");
                }
                // AI functions and fork targets bind arguments by position for
                // another context, so their arity is exact. Ordinary functions
                // follow JavaScript.
                let exact = fork || self.virtuals.contains(i.sym.as_ref());
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
                return Ok(if fork {
                    Effect::Fork(i.sym.to_string(), args)
                } else {
                    Effect::Call(i.sym.to_string(), args)
                });
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
                    "use helper(args), library.method(args), fork(helper,args) or join(handles); call/ai wrappers are unsupported",
                );
            }
        };
        if fork {
            return self.error(call.span, "fork takes a declared helper function");
        }
        if matches!(
            target.as_str(),
            "execute_program"
                | "check_program"
                | "define_task_body"
                | "create_task"
                | "spawn_agent"
                | "join_agents"
        ) {
            return self.error(call.span,"use local functions and fork/join rather than recursively controlling program/task execution");
        }
        if args.len() > 1 {
            return self.error(call.span, "a tool call takes one JSON argument object");
        }
        Ok(Effect::Tool(target, args))
    }
    // Only chains rooted in Promise.resolve/reject are lowered here. They are
    // consumed by await immediately; no Promise handle is represented as JSON.
    fn is_awaited_promise_chain(e: &ast::Expr) -> bool {
        let ast::Expr::Call(call) = e else {
            return false;
        };
        let ast::Callee::Expr(target) = &call.callee else {
            return false;
        };
        let ast::Expr::Member(member) = &**target else {
            return false;
        };
        let ast::MemberProp::Ident(method) = &member.prop else {
            return false;
        };
        if matches!(&*member.obj, ast::Expr::Ident(i) if i.sym == *"Promise") {
            return matches!(method.sym.as_ref(), "resolve" | "reject");
        }
        matches!(method.sym.as_ref(), "then" | "catch")
            && Self::is_awaited_promise_chain(&member.obj)
    }
    // Keep initial argument evaluation outside the subset with catchable effects:
    // JavaScript evaluates it before Promise.resolve/reject creates a promise.
    fn safe_promise_root_argument(e: &ast::Expr) -> bool {
        match e {
            ast::Expr::Lit(_) | ast::Expr::Ident(_) => true,
            ast::Expr::Array(a) => a.elems.iter().all(|item| {
                item.as_ref().is_none_or(|item| {
                    item.spread.is_none() && Self::safe_promise_root_argument(&item.expr)
                })
            }),
            ast::Expr::Object(o) => o.props.iter().all(|prop| match prop {
                ast::PropOrSpread::Prop(p) => match &**p {
                    ast::Prop::Shorthand(_) => true,
                    ast::Prop::KeyValue(kv) => {
                        matches!(
                            kv.key,
                            ast::PropName::Ident(_) | ast::PropName::Str(_) | ast::PropName::Num(_)
                        ) && Self::safe_promise_root_argument(&kv.value)
                    }
                    _ => false,
                },
                _ => false,
            }),
            _ => false,
        }
    }
    fn promise_callback(&mut self, callback: &ast::Expr, argument: Expr) -> Result<Expr> {
        let ast::Expr::Arrow(arrow) = callback else {
            return self.error(
                callback.span(),
                "awaited Promise callbacks must be expression arrows",
            );
        };
        if arrow.is_generator || arrow.params.len() > 1 {
            return self.error(
                arrow.span,
                "awaited Promise callbacks take zero or one simple parameter",
            );
        }
        let ast::BlockStmtOrExpr::Expr(expression) = &*arrow.body else {
            return self.error(
                arrow.span,
                "awaited Promise callbacks require an expression body",
            );
        };
        let parameter = match arrow.params.first() {
            Some(ast::Pat::Ident(binding)) => Some(binding.id.sym.to_string()),
            Some(other) => {
                return self.error(other.span(), "awaited Promise callbacks require a simple parameter");
            }
            None => None,
        };
        // The parameter is a binding of the callback's own block scope, so
        // closures created in the callback capture it.
        let entered = self.enter_scope(
            arrow.span,
            parameter.iter().map(|name| (name.clone(), true)).collect(),
        )?;
        match &parameter {
            Some(name) => {
                self.emit(arrow.span, Op::Declare(name.clone(), argument));
            }
            // The previous result must still be evaluated if the callback ignores it.
            None => {
                let temporary = self.temp();
                self.emit(arrow.span, Op::Set(temporary, argument));
            }
        }
        let value = if Self::is_awaited_promise_chain(expression) {
            self.awaited_promise_chain(expression)?
        } else {
            self.lower(expression)?
        };
        let value = self.snapshot(arrow.span, value);
        self.leave_scope(arrow.span, entered);
        Ok(value)
    }

    /// Compile a direct array callback into the containing resumable frame.
    /// Its parameters are bindings of a block scope entered for each element,
    /// so closures created in the callback capture them.
    fn array_callback(&mut self, callback: &ast::Expr, arguments: &[Expr]) -> Result<Expr> {
        let (span, params, body) = match callback {
            ast::Expr::Arrow(arrow) if !arrow.is_async && !arrow.is_generator => {
                let ast::BlockStmtOrExpr::Expr(body) = &*arrow.body else {
                    return self.error(arrow.span, "array callback requires an expression body");
                };
                (arrow.span, arrow.params.iter().collect::<Vec<_>>(), &**body)
            }
            ast::Expr::Fn(function) if function.ident.is_none() && !function.function.is_async && !function.function.is_generator => {
                let Some(body) = &function.function.body else { return self.error(function.span(), "array callback needs a body") };
                let [ast::Stmt::Return(ret)] = body.stmts.as_slice() else { return self.error(body.span, "array callback function must contain a single return expression") };
                let Some(value) = &ret.arg else { return self.error(ret.span, "array callback must return an expression") };
                (function.function.span, function.function.params.iter().map(|p| &p.pat).collect(), &**value)
            }
            _ => return self.error(callback.span(), "map/filter requires a synchronous direct callback"),
        };
        if params.len() > arguments.len() { return self.error(callback.span(), "array callback accepts at most three simple parameters"); }
        let mut names = Vec::new();
        for parameter in &params {
            let ast::Pat::Ident(binding) = parameter else { return self.error(parameter.span(), "array callback parameters must be simple names") };
            names.push(binding.id.sym.to_string());
        }
        let entered = self.enter_scope(span, names.iter().map(|name| (name.clone(), true)).collect())?;
        for (name, argument) in names.iter().zip(arguments) {
            self.emit(span, Op::Declare(name.clone(), argument.clone()));
        }
        let value = self.lower(body)?;
        let value = self.snapshot(span, value);
        self.leave_scope(span, entered);
        Ok(value)
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
            return self.callback_method(array, "map", &call.args[1..], call.span).map(Some);
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

    /// `receiver.method(args)` for a built-in receiver kind. Callback methods
    /// are compiled into this frame so that their callbacks can suspend.
    fn call_method_on(
        &mut self,
        receiver: Expr,
        method: &str,
        args: &[ast::ExprOrSpread],
        span: Span,
    ) -> Result<Expr> {
        if CALLBACK_METHODS.contains(&method) && !(matches!(method, "sort" | "toSorted") && args.is_empty()) {
            return self.callback_method(receiver, method, args, span);
        }
        if args.iter().any(|a| a.spread.is_some()) {
            return self.error(span, "method argument spreads are unsupported");
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

    /// Visit indices in order (or in reverse), optionally skipping holes.
    /// `body` gets the element, its index and a list of jumps to the end.
    #[allow(clippy::too_many_arguments)]
    fn array_loop(
        &mut self,
        span: Span,
        source: &Expr,
        length: &Expr,
        from: Option<Expr>,
        backward: bool,
        skip_holes: bool,
        body: impl FnOnce(&mut Self, Expr, Expr, &mut Vec<usize>) -> Result<()>,
    ) -> Result<()> {
        let index = self.temp();
        let one = || Box::new(Expr::Value(json!(1)));
        let start_value = match (from, backward) {
            (Some(from), _) => from,
            (None, false) => Expr::Value(json!(0)),
            (None, true) => Expr::Binary("-".into(), Box::new(length.clone()), one()),
        };
        self.emit(span, Op::Set(index.clone(), start_value));
        let start = self.code.len();
        let position = || Box::new(Expr::Variable(index.clone()));
        let test = if backward {
            Expr::Binary(">=".into(), position(), Box::new(Expr::Value(json!(0))))
        } else {
            Expr::Binary("<".into(), position(), Box::new(length.clone()))
        };
        let exit = self.emit(span, Op::Branch(test, 0));
        let hole = skip_holes.then(|| {
            let present = Expr::Binary("hasIndex".into(), Box::new(source.clone()), position());
            self.emit(span, Op::Branch(present, 0))
        });
        let item = self.snapshot(span, Expr::Member(Box::new(source.clone()), position()));
        let mut breaks = Vec::new();
        body(self, item, Expr::Variable(index.clone()), &mut breaks)?;
        let next = self.code.len();
        let step = if backward { "-" } else { "+" };
        self.emit(span, Op::Set(index.clone(), Expr::Binary(step.into(), position(), one())));
        self.emit(span, Op::Jump(start));
        let end = self.code.len();
        if let Op::Branch(_, target) = &mut self.code[exit].op {
            *target = end;
        }
        if let Some(hole) = hole
            && let Op::Branch(_, target) = &mut self.code[hole].op
        {
            *target = next;
        }
        for at in breaks {
            self.code[at].op = Op::Jump(end);
        }
        Ok(())
    }

    /// Patch a forward branch emitted at `at` to the current position.
    fn land(&mut self, at: usize) {
        let here = self.code.len();
        match &mut self.code[at].op {
            Op::Branch(_, target) => *target = here,
            Op::Jump(target) => *target = here,
            _ => unreachable!("only branches and jumps are patched"),
        }
    }

    /// Array methods with a callback, compiled into the calling frame. The
    /// callback is a direct arrow or function with a single return expression.
    fn callback_method(
        &mut self,
        receiver: Expr,
        method: &str,
        args: &[ast::ExprOrSpread],
        span: Span,
    ) -> Result<Expr> {
        if args.iter().any(|a| a.spread.is_some()) {
            return self.error(span, "callback argument spreads are unsupported");
        }
        let reduce = matches!(method, "reduce" | "reduceRight");
        if (reduce && !(1..=2).contains(&args.len())) || (!reduce && args.len() != 1) {
            return self.error(
                span,
                &format!(
                    "{method} takes one direct callback{}",
                    if reduce { " and an optional initial value" } else { "; a thisArg is unsupported" }
                ),
            );
        }
        let v = |name: &str| Expr::Variable(name.to_owned());
        let num = |n: i64| Expr::Value(json!(n));
        let bin = |op: &str, a: Expr, b: Expr| Expr::Binary(op.into(), Box::new(a), Box::new(b));
        let source = self.snapshot(span, receiver);
        let length = self.snapshot(span, Expr::Unary("arrayLength".into(), Box::new(source.clone())));
        let callback = &*args[0].expr;
        let result = self.temp();
        match method {
            "map" | "flatMap" => {
                self.emit(span, Op::Set(result.clone(), Expr::Array(Vec::new())));
                self.emit(
                    span,
                    Op::SetMember {
                        object: v(&result),
                        key: Expr::Value(json!("length")),
                        value: length.clone(),
                    },
                );
                let target = result.clone();
                let list = source.clone();
                self.array_loop(span, &source, &length, None, false, true, |c, item, index, _| {
                    let value = c.array_callback(callback, &[item, index.clone(), list])?;
                    c.emit(span, Op::SetMember { object: v(&target), key: index, value });
                    Ok(())
                })?;
                if method == "flatMap" {
                    let flat = Expr::StringMethod(Box::new(v(&result)), "flat".into(), vec![num(1)]);
                    return Ok(self.snapshot(span, flat));
                }
            }
            "filter" => {
                self.emit(span, Op::Set(result.clone(), Expr::Array(Vec::new())));
                let target = result.clone();
                let list = source.clone();
                self.array_loop(span, &source, &length, None, false, true, |c, item, index, _| {
                    let value = c.array_callback(callback, &[item.clone(), index, list])?;
                    let skip = c.emit(span, Op::Branch(value, 0));
                    let destination = c.temp();
                    c.emit(span, Op::Append { target: target.clone(), items: vec![item], destination });
                    c.land(skip);
                    Ok(())
                })?;
            }
            "forEach" => {
                self.emit(span, Op::Set(result.clone(), Expr::Undefined));
                let list = source.clone();
                self.array_loop(span, &source, &length, None, false, true, |c, item, index, _| {
                    let value = c.array_callback(callback, &[item, index, list])?;
                    c.snapshot(span, value);
                    Ok(())
                })?;
            }
            "some" | "every" | "find" | "findIndex" | "findLast" | "findLastIndex" => {
                let initial = match method {
                    "some" => Expr::Value(json!(false)),
                    "every" => Expr::Value(json!(true)),
                    "find" | "findLast" => Expr::Undefined,
                    _ => num(-1),
                };
                self.emit(span, Op::Set(result.clone(), initial));
                let backward = method.starts_with("findLast");
                // some/every skip holes; the find family reads them as undefined.
                let skip_holes = matches!(method, "some" | "every");
                let target = result.clone();
                let list = source.clone();
                self.array_loop(span, &source, &length, None, backward, skip_holes, |c, item, index, breaks| {
                    let value = c.array_callback(callback, &[item.clone(), index.clone(), list])?;
                    let value = c.snapshot(span, value);
                    // every stops at the first falsy result, the others at the first truthy one.
                    let test = if method == "every" { Expr::Unary("!".into(), Box::new(value)) } else { value };
                    let skip = c.emit(span, Op::Branch(test, 0));
                    let found = match method {
                        "some" => Expr::Value(json!(true)),
                        "every" => Expr::Value(json!(false)),
                        "find" | "findLast" => item,
                        _ => index,
                    };
                    c.emit(span, Op::Set(target.clone(), found));
                    breaks.push(c.emit(span, Op::Jump(0)));
                    c.land(skip);
                    Ok(())
                })?;
            }
            "reduce" | "reduceRight" => {
                let backward = method == "reduceRight";
                let accumulator = result.clone();
                let from = if let Some(initial) = args.get(1) {
                    let value = self.lower(&initial.expr)?;
                    self.emit(span, Op::Set(accumulator.clone(), value));
                    None
                } else {
                    // The first present element is the initial accumulator.
                    let k = self.temp();
                    let first = if backward { bin("-", length.clone(), num(1)) } else { num(0) };
                    self.emit(span, Op::Set(k.clone(), first));
                    let seek = self.code.len();
                    let in_range = if backward { bin(">=", v(&k), num(0)) } else { bin("<", v(&k), length.clone()) };
                    let empty = self.emit(span, Op::Branch(in_range, 0));
                    let missing = self.emit(span, Op::Branch(bin("hasIndex", source.clone(), v(&k)), 0));
                    self.emit(span, Op::Set(accumulator.clone(), Expr::Member(Box::new(source.clone()), Box::new(v(&k)))));
                    let step = if backward { "-" } else { "+" };
                    self.emit(span, Op::Set(k.clone(), bin(step, v(&k), num(1))));
                    let found = self.emit(span, Op::Jump(0));
                    self.land(missing);
                    self.emit(span, Op::Set(k.clone(), bin(step, v(&k), num(1))));
                    self.emit(span, Op::Jump(seek));
                    self.land(empty);
                    let error = Expr::NodeBuiltin(
                        "new TypeError".into(),
                        vec![Expr::Value(json!("Reduce of empty array with no initial value"))],
                    );
                    self.emit(span, Op::Throw(error));
                    self.land(found);
                    Some(v(&k))
                };
                let list = source.clone();
                self.array_loop(span, &source, &length, from, backward, true, |c, item, index, _| {
                    let value = c.array_callback(callback, &[v(&accumulator), item, index, list])?;
                    c.emit(span, Op::Set(accumulator.clone(), value));
                    Ok(())
                })?;
            }
            "sort" | "toSorted" => {
                // SortIndexedProperties: present values other than undefined
                // are compared; undefined values follow them. sort keeps holes
                // last; toSorted reads holes as undefined.
                let values = self.temp();
                let undefined = self.temp();
                self.emit(span, Op::Set(values.clone(), Expr::Array(Vec::new())));
                self.emit(span, Op::Set(undefined.clone(), num(0)));
                {
                    let (values, undefined) = (values.clone(), undefined.clone());
                    self.array_loop(span, &source, &length, None, false, method == "sort", |c, item, _, _| {
                        let defined = c.emit(span, Op::Branch(bin("===", item.clone(), Expr::Undefined), 0));
                        c.emit(span, Op::Set(undefined.clone(), bin("+", v(&undefined), num(1))));
                        let done = c.emit(span, Op::Jump(0));
                        c.land(defined);
                        let destination = c.temp();
                        c.emit(span, Op::Append { target: values.clone(), items: vec![item], destination });
                        c.land(done);
                        Ok(())
                    })?;
                }
                let sorted = self.merge_sort(span, &values, callback)?;
                let count = self.temp();
                self.emit(span, Op::Set(count.clone(), num(0)));
                let pad = self.code.len();
                let padded = self.emit(span, Op::Branch(bin("<", v(&count), v(&undefined)), 0));
                let destination = self.temp();
                self.emit(span, Op::Append { target: sorted.clone(), items: vec![Expr::Undefined], destination });
                self.emit(span, Op::Set(count.clone(), bin("+", v(&count), num(1))));
                self.emit(span, Op::Jump(pad));
                self.land(padded);
                let value = if method == "sort" {
                    Expr::NodeBuiltin("$sortWriteBack".into(), vec![source.clone(), v(&sorted)])
                } else {
                    v(&sorted)
                };
                self.emit(span, Op::Set(result.clone(), value));
            }
            _ => unreachable!("callback method list"),
        }
        Ok(Expr::Variable(result))
    }

    /// A stable bottom-up merge sort of the array in `values`, with the
    /// comparator inlined. SortCompare treats NaN as 0; equal elements keep
    /// their order because the left run wins ties.
    fn merge_sort(&mut self, span: Span, values: &str, comparator: &ast::Expr) -> Result<String> {
        let v = |name: &str| Expr::Variable(name.to_owned());
        let num = |n: i64| Expr::Value(json!(n));
        let bin = |op: &str, a: Expr, b: Expr| Expr::Binary(op.into(), Box::new(a), Box::new(b));
        let min = |a: Expr, b: Expr| Expr::NodeBuiltin("Math.min".into(), vec![a, b]);
        let at = |list: &str, index: &str| Expr::Member(Box::new(v(list)), Box::new(v(index)));
        let names: Vec<String> = (0..12).map(|_| self.temp()).collect();
        let [arr, tmp, n, width, i, left, mid, right, l, r, k, swap] = <[String; 12]>::try_from(names).unwrap();
        self.emit(span, Op::Set(arr.clone(), v(values)));
        self.emit(span, Op::Set(n.clone(), Expr::Unary("arrayLength".into(), Box::new(v(&arr)))));
        self.emit(span, Op::Set(tmp.clone(), Expr::Array(Vec::new())));
        self.emit(span, Op::SetMember { object: v(&tmp), key: Expr::Value(json!("length")), value: v(&n) });
        self.emit(span, Op::Set(width.clone(), num(1)));
        let outer = self.code.len();
        let outer_exit = self.emit(span, Op::Branch(bin("<", v(&width), v(&n)), 0));
        self.emit(span, Op::Set(i.clone(), num(0)));
        let pass = self.code.len();
        let pass_exit = self.emit(span, Op::Branch(bin("<", v(&i), v(&n)), 0));
        self.emit(span, Op::Set(left.clone(), v(&i)));
        self.emit(span, Op::Set(mid.clone(), min(bin("+", v(&i), v(&width)), v(&n))));
        self.emit(span, Op::Set(right.clone(), min(bin("+", v(&i), bin("*", num(2), v(&width))), v(&n))));
        self.emit(span, Op::Set(l.clone(), v(&left)));
        self.emit(span, Op::Set(r.clone(), v(&mid)));
        self.emit(span, Op::Set(k.clone(), v(&left)));
        let merge = self.code.len();
        let merge_exit = self.emit(span, Op::Branch(bin("<", v(&k), v(&right)), 0));
        let left_done = self.emit(span, Op::Branch(bin("<", v(&l), v(&mid)), 0));
        let right_done = self.emit(span, Op::Branch(bin("<", v(&r), v(&right)), 0));
        let a = self.snapshot(span, at(&arr, &l));
        let b = self.snapshot(span, at(&arr, &r));
        let order = self.array_callback(comparator, &[a, b])?;
        let order = self.snapshot(span, Expr::Unary("+".into(), Box::new(order)));
        let keep_left = self.emit(span, Op::Branch(bin(">", order, num(0)), 0));
        // take the right element
        self.land(left_done);
        self.emit(span, Op::SetMember { object: v(&tmp), key: v(&k), value: at(&arr, &r) });
        self.emit(span, Op::Set(r.clone(), bin("+", v(&r), num(1))));
        let advance_right = self.emit(span, Op::Jump(0));
        // take the left element
        self.land(right_done);
        self.land(keep_left);
        self.emit(span, Op::SetMember { object: v(&tmp), key: v(&k), value: at(&arr, &l) });
        self.emit(span, Op::Set(l.clone(), bin("+", v(&l), num(1))));
        self.land(advance_right);
        self.emit(span, Op::Set(k.clone(), bin("+", v(&k), num(1))));
        self.emit(span, Op::Jump(merge));
        self.land(merge_exit);
        self.emit(span, Op::Set(i.clone(), bin("+", v(&i), bin("*", num(2), v(&width)))));
        self.emit(span, Op::Jump(pass));
        self.land(pass_exit);
        self.emit(span, Op::Set(swap.clone(), v(&arr)));
        self.emit(span, Op::Set(arr.clone(), v(&tmp)));
        self.emit(span, Op::Set(tmp.clone(), v(&swap)));
        self.emit(span, Op::Set(width.clone(), bin("*", v(&width), num(2))));
        self.emit(span, Op::Jump(outer));
        self.land(outer_exit);
        Ok(arr)
    }

    fn awaited_promise_chain(&mut self, e: &ast::Expr) -> Result<Expr> {
        let ast::Expr::Call(call) = e else {
            return self.error(e.span(), "invalid awaited Promise chain");
        };
        let ast::Callee::Expr(target) = &call.callee else {
            return self.error(call.span, "invalid awaited Promise chain");
        };
        let ast::Expr::Member(member) = &**target else {
            return self.error(call.span, "invalid awaited Promise chain");
        };
        let ast::MemberProp::Ident(method) = &member.prop else {
            return self.error(call.span, "invalid awaited Promise method");
        };
        if call.args.iter().any(|arg| arg.spread.is_some()) {
            return self.error(call.span, "Promise argument spreads are unsupported");
        }
        if matches!(&*member.obj, ast::Expr::Ident(i) if i.sym == *"Promise") {
            if call.args.len() > 1 {
                return self.error(
                    call.span,
                    "Promise.resolve/reject accepts at most one argument",
                );
            }
            if call
                .args
                .first()
                .is_some_and(|arg| !Self::safe_promise_root_argument(&arg.expr))
            {
                return self.error(call.span, "Promise.resolve/reject arguments must be simple values without effects or property access");
            }
            let value = if let Some(arg) = call.args.first() {
                self.lower(&arg.expr)?
            } else {
                Expr::Value(Value::Null)
            };
            if method.sym == *"reject" {
                self.emit(call.span, Op::Throw(value));
                return Ok(Expr::Value(Value::Null));
            }
            return Ok(value);
        }
        if call.args.len() != 1 {
            return self.error(call.span, "awaited then/catch requires one callback");
        }
        if method.sym == *"then" {
            let value = self.awaited_promise_chain(&member.obj)?;
            return self.promise_callback(&call.args[0].expr, value);
        }
        let error = self.temp();
        let result = self.temp();
        self.temp_names.insert(error.clone());
        self.temp_names.insert(result.clone());
        let start = self.emit(
            call.span,
            Op::Catch {
                target: 0,
                name: error.clone(),
            },
        );
        self.handlers += 1;
        let value = self.awaited_promise_chain(&member.obj)?;
        self.handlers -= 1;
        self.emit(call.span, Op::Set(result.clone(), value));
        self.emit(call.span, Op::EndCatch);
        let jump = self.emit(call.span, Op::Jump(0));
        let handler = self.code.len();
        let value = self.promise_callback(&call.args[0].expr, Expr::Variable(error))?;
        self.emit(call.span, Op::Set(result.clone(), value));
        let end = self.code.len();
        if let Op::Catch { target, .. } = &mut self.code[start].op {
            *target = handler;
        }
        self.code[jump].op = Op::Jump(end);
        Ok(Expr::Variable(result))
    }
    fn lower_inner(&mut self, e: &ast::Expr) -> Result<Expr> {
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
                if Self::is_awaited_promise_chain(&awaited.arg) {
                    return self.awaited_promise_chain(&awaited.arg);
                }
                let ast::Expr::Call(all) = &*awaited.arg else {
                    return self.lower(&awaited.arg);
                };
                let is_all = matches!(&all.callee, ast::Callee::Expr(e)
                    if matches!(&**e, ast::Expr::Member(m)
                        if matches!(&*m.obj, ast::Expr::Ident(i) if i.sym == *"Promise")
                        && matches!(&m.prop, ast::MemberProp::Ident(i) if i.sym == *"all")));
                // Ordinary flow effects already suspend until their result is available.
                // Awaiting one effect or JSON value preserves that sequencing without
                // creating a Promise or another task.
                if !is_all {
                    return self.lower(&awaited.arg);
                }
                if all.args.len() != 1 || all.args[0].spread.is_some() {
                    return self.error(
                        awaited.span,
                        "use await Promise.all([read-only tool calls])",
                    );
                }
                if let ast::Expr::Call(map) = &*all.args[0].expr {
                    let ast::Callee::Expr(target) = &map.callee else {
                        return self.error(map.span, "use array.map(item => io.read(...))");
                    };
                    let ast::Expr::Member(member) = &**target else {
                        return self.error(map.span, "use array.map(item => io.read(...))");
                    };
                    if !matches!(&member.prop, ast::MemberProp::Ident(i) if i.sym == *"map")
                        || map.args.len() != 1
                        || map.args[0].spread.is_some()
                    {
                        return self.error(map.span, "use array.map(item => io.read(...))");
                    }
                    let items = self.expr(&member.obj)?;
                    let ast::Expr::Arrow(arrow) = &*map.args[0].expr else {
                        return self.error(map.span, "map needs an arrow with one parameter");
                    };
                    if arrow.is_generator || arrow.params.len() != 1 {
                        return self.error(arrow.span, "map needs one arrow parameter");
                    }
                    let ast::Pat::Ident(parameter) = &arrow.params[0] else {
                        return self.error(arrow.span, "map needs a simple parameter");
                    };
                    let name = parameter.id.sym.to_string();
                    if self.resolve(&name).is_some() {
                        return self.error(
                            arrow.span,
                            "map parameter must not shadow an existing binding",
                        );
                    }
                    let ast::BlockStmtOrExpr::Expr(body) = &*arrow.body else {
                        return self.error(arrow.span, "map body must be one read-only tool call");
                    };
                    let body_expr = match &**body {
                        ast::Expr::Await(awaited) if arrow.is_async => &*awaited.arg,
                        expr => expr,
                    };
                    let ast::Expr::Call(call) = body_expr else {
                        return self.error(arrow.span, "map body must be one read-only tool call");
                    };
                    // The parameter exists only while each read's arguments are built.
                    self.scopes.push(BTreeMap::new());
                    self.declare(&name, false, arrow.span)?;
                    let effect = self.effect(call);
                    self.scopes.pop();
                    let (tool, args) = match effect? {
                        Effect::Tool(tool, args)
                            if matches!(
                                tool.as_str(),
                                "read_file" | "read_file_lines" | "search_text" | "list_dir"
                            ) =>
                        {
                            (tool, args)
                        }
                        _ => {
                            return self.error(
                                call.span,
                                "Promise.all accepts only file reads, listings and searches",
                            );
                        }
                    };
                    let dest = self.temp();
                    self.emit(
                        awaited.span,
                        Op::Effect(
                            Some(dest.clone()),
                            Effect::ParallelReadMap {
                                name: tool,
                                items,
                                parameter: name,
                                args,
                            },
                        ),
                    );
                    return Ok(Expr::Variable(dest));
                }
                let ast::Expr::Array(array) = &*all.args[0].expr else {
                    return self.error(
                        awaited.span,
                        "Promise.all needs an array of calls or array.map(item => read-only call)",
                    );
                };
                if array.elems.len() > 32 {
                    return self.error(
                        awaited.span,
                        "Promise.all supports at most 32 read-only calls",
                    );
                }
                let mut operations = Vec::new();
                for item in &array.elems {
                    let Some(item) = item else {
                        return self.error(array.span, "array holes are unsupported");
                    };
                    if item.spread.is_some() {
                        return self.error(item.span(), "array spreads are unsupported");
                    }
                    let ast::Expr::Call(call) = &*item.expr else {
                        return self.error(
                            item.span(),
                            "Promise.all accepts only read-only library calls",
                        );
                    };
                    match self.effect(call)? {
                        Effect::Tool(name, args)
                            if matches!(
                                name.as_str(),
                                "read_file" | "read_file_lines" | "search_text" | "list_dir"
                            ) =>
                        {
                            operations.push((name, args))
                        }
                        _ => {
                            return self.error(
                                call.span,
                                "Promise.all accepts only file reads, listings and searches",
                            );
                        }
                    }
                }
                let dest = self.temp();
                self.emit(
                    awaited.span,
                    Op::Effect(Some(dest.clone()), Effect::ParallelReads(operations)),
                );
                Ok(Expr::Variable(dest))
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
                for (index, arg) in call.args.iter().enumerate() {
                    if index == 0 && matches!(target,ast::Expr::Ident(i) if i.sym == *"fork") {
                        args.push(arg.clone());
                        continue;
                    }
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
    /// Initialize (`declare`) or assign the names of a declaration pattern.
    fn binding(&mut self, p: &ast::Pat, e: &ast::Expr, declare: bool) -> Result<()> {
        let bind = |name: String, value: Expr| {
            if declare {
                Op::Declare(name, value)
            } else {
                Op::Set(name, value)
            }
        };
        match p {
            ast::Pat::Ident(i) => {
                let value = self.lower(e)?;
                self.emit(p.span(), bind(i.id.sym.to_string(), value));
            }
            ast::Pat::Array(a) => {
                let temp = self.temp();
                self.value(e, temp.clone())?;
                for (index, p) in a.elems.iter().enumerate() {
                    match p {
                        Some(ast::Pat::Ident(i)) => {
                            self.emit(
                                p.as_ref().unwrap().span(),
                                bind(
                                    i.id.sym.to_string(),
                                    Expr::Member(
                                        Box::new(Expr::Variable(temp.clone())),
                                        Box::new(Expr::Value(json!(index))),
                                    ),
                                ),
                            );
                        }
                        _ => {
                            return self
                                .error(a.span, "only simple array destructuring is supported");
                        }
                    }
                }
            }
            _ => return self.error(p.span(), "use simple identifier or array bindings"),
        }
        Ok(())
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
            S::Break(b) => {
                if b.label.is_some() || self.loops.is_empty() {
                    return self.error(b.span, "break requires an enclosing loop and no label");
                }
                self.leave_to_loop(b.span);
                let at = self.emit(b.span, Op::Jump(0));
                self.loops.last_mut().unwrap().breaks.push(at);
            }
            S::Continue(c) => {
                if c.label.is_some() || self.loops.is_empty() {
                    return self.error(c.span, "continue requires an enclosing loop and no label");
                }
                self.leave_to_loop(c.span);
                let at = self.emit(c.span, Op::Jump(0));
                self.loops.last_mut().unwrap().continues.push(at);
            }
            S::ForOf(f) => {
                if f.is_await {
                    return self.error(f.span, "for await is unsupported");
                }
                let (name, kind) = match &f.left {
                    ast::ForHead::VarDecl(v) if v.decls.len() == 1 && v.decls[0].init.is_none() => {
                        match &v.decls[0].name {
                            ast::Pat::Ident(i) => (i.id.sym.to_string(), v.kind),
                            _ => return self.error(f.span, "use a simple for...of binding"),
                        }
                    }
                    _ => return self.error(f.span, "use for (const item of array)"),
                };
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
                let entered = if kind == ast::VarDeclKind::Var {
                    false
                } else {
                    self.enter_scope(f.span, vec![(name.clone(), kind == ast::VarDeclKind::Let)])?
                };
                let value = Expr::Variable(item);
                self.emit(
                    f.span,
                    if entered {
                        Op::Declare(name, value)
                    } else {
                        Op::Set(name, value)
                    },
                );
                self.stmt(&f.body)?;
                self.leave_scope(f.span, entered);
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::ForNext { end: target, .. } = &mut self.code[next].op {
                    *target = end;
                }
                self.end_loop(end, start);
            }

            S::Try(t) => {
                if t.finalizer.is_some() {
                    return self.error(t.span, "finally is not supported in this version");
                }
                let h = t.handler.as_ref().context("try requires catch")?;
                let param = match &h.param {
                    Some(ast::Pat::Ident(i)) => Some(i.id.sym.to_string()),
                    None => None,
                    _ => return self.error(h.span, "use catch(error)"),
                };
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
                let mut names: Vec<(String, bool)> =
                    param.iter().map(|name| (name.clone(), true)).collect();
                names.extend(lexical_names(&h.body.stmts));
                let entered = self.enter_scope(h.span, names)?;
                if let Some(param) = param {
                    self.emit(h.span, Op::Declare(param, Expr::Variable(thrown)));
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
    compile_inner(source, "main", false, libraries, BTreeSet::new())
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
    let program = compile_inner(source, entry, true, libraries, external_names)?;
    ensure!(
        !program.functions[entry].virtual_task,
        "define_task_body must implement the current function, not return another empty stub"
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
    compile_inner(source, entry, false, libraries, BTreeSet::new())
}
fn compile_inner(
    source: &str,
    entry: &str,
    refinement: bool,
    libraries: super::libraries::Libraries,
    external_names: BTreeSet<String>,
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
    for item in module.body {
        match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
                has_import = true;
                let module_name = import.src.value.to_string_lossy();
                let Some(namespace) = module_name.strip_prefix("host:") else {
                    bail!("TS_PROGRAM 1:1: tool imports must use host:<library>");
                };
                let Some(methods) = libraries.get(namespace) else {
                    bail!("TS_PROGRAM 1:1: unknown or unavailable tool library {namespace}");
                };
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
            ast::ModuleItem::Stmt(stmt) if !refinement && entry == "main" => top_level.push(stmt),
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
    // Existing function-only programs remain executable without affecting the new top-level form.
    let implicit_entry = !refinement
        && entry == "main"
        && (!top_level.is_empty() || !declarations.iter().any(|f| f.ident.sym == *"main"));
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
        names.insert("main".into());
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
                "tools" | "call" | "fork" | "join" | "ai" | "assert" | "JSON" | "Promise"
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
    ensure!(
        !refinement || !names.contains("main"),
        "task refinement implements the current function, never another main"
    );
    let mut functions = BTreeMap::new();
    let mut arities = declarations
        .iter()
        .map(|f| (f.ident.sym.to_string(), f.function.params.len()))
        .collect::<BTreeMap<_, _>>();
    if implicit_entry {
        arities.insert("main".into(), 1);
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
            if !source.get(lo..hi).is_some_and(|gap| gap.trim().is_empty()) {
                break;
            }
            header.push(note.text.to_string());
            header_start = note.span.lo;
        }
        header.reverse();
        header.extend(
            notes
                .iter()
                .filter(|n| n.span.lo >= body.span.lo && n.span.hi <= body.span.hi)
                .map(|n| n.text.to_string()),
        );
        let contract = header.join("\n");
        let virtual_task = contract
            .split(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '@' | '_')))
            .any(|word| word == "@virtual");
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
    let virtuals = contracts
        .iter()
        .filter(|(_, (virtual_task, _))| *virtual_task)
        .map(|(name, _)| name.clone())
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
            function_id: "main".into(),
            nested: Vec::new(),
            virtuals: virtuals.clone(),
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            chain_exits: None,
            chain_object: false,
        };
        c.declare("input", false, module.span)?;
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
            "main".into(),
            Function {
                description: String::new(),
                virtual_task: false,
                params: vec!["input".into()],
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
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
            chain_exits: None,
            chain_object: false,
        };
        if f.function.is_generator {
            return c.error(f.function.span, "generator functions are unsupported");
        }
        if f.ident.sym == *"main" && f.function.params.len() > 1 {
            return c.error(f.function.span, "main accepts zero or one input parameter");
        }
        let mut params = Vec::new();
        for p in &f.function.params {
            let ast::Pat::Ident(i) = &p.pat else {
                return c.error(p.span, "use simple function parameters");
            };
            c.declare(i.id.sym.as_ref(), true, p.span)?;
            params.push(i.id.sym.to_string());
        }
        let body = f
            .function
            .body
            .as_ref()
            .context("function body is required")?;
        // var names are hoisted to the function; let/const of the body's top
        // level start in their dead zone.
        let vars: Vec<String> = var_names(&body.stmts)
            .into_iter()
            .filter(|name| !params.contains(name))
            .collect();
        for name in &vars {
            c.declare(name, true, body.span)?;
        }
        let lexical = lexical_names(&body.stmts);
        for (name, mutable) in &lexical {
            c.declare(name, *mutable, body.span)?;
        }
        let (virtual_task, description) = contracts[f.ident.sym.as_ref()].clone();
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
            if f.ident.sym == *"main" {
                return c.error(
                    body.span,
                    "main must orchestrate the complete task and cannot be @virtual",
                );
            }
            let input = Expr::Object(
                params
                    .iter()
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
        functions.insert(
            f.ident.sym.to_string(),
            Function {
                description,
                virtual_task,
                params,
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
        functions,
    })
}
