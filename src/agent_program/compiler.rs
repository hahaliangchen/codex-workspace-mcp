use super::*;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use swc_common::{FileName, SourceMap, Span, Spanned, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast as ast;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

struct Compiler {
    map: Lrc<SourceMap>,
    names: BTreeSet<String>,
    arities: BTreeMap<String, usize>,
    libraries: super::libraries::Libraries,
    vars: BTreeMap<String, bool>,
    code: Vec<Instruction>,
    temp: usize,
    loops: Vec<LoopContext>,
    handlers: usize,
}

#[derive(Default)]
struct LoopContext {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    handlers: usize,
}
impl Compiler {
    fn condition(&mut self, e: &ast::Expr) -> Result<Expr> {
        self.lower(e)
    }
    fn logical(&mut self, op: &str, left: Expr, right: &ast::Expr, span: Span) -> Result<Expr> {
        let left = self.snapshot(span, left);
        let result = self.temp();
        let initial = if op == "??" {
            left.clone()
        } else {
            Expr::Unary("bool".into(), Box::new(left.clone()))
        };
        self.emit(span, Op::Set(result.clone(), initial));
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
        let value = if op == "??" {
            value
        } else {
            Expr::Unary("bool".into(), Box::new(value))
        };
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
    fn local_target(&mut self, name: &str, span: Span, create: bool) -> Result<()> {
        match self.vars.get(name) {
            Some(false) => self.error(span, "cannot rebind a const value"),
            Some(true) => Ok(()),
            None if create => self.declare(name, true, span),
            None => self.error(span, "update requires an initialized local variable"),
        }
    }
    fn begin_loop(&mut self) {
        self.loops.push(LoopContext {
            handlers: self.handlers,
            ..Default::default()
        });
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
    fn variable_declaration(&mut self, v: &ast::VarDecl) -> Result<()> {
        for d in &v.decls {
            let existing = if let ast::Pat::Ident(i) = &d.name {
                v.kind == ast::VarDeclKind::Var && self.vars.get(i.id.sym.as_ref()) == Some(&true)
            } else {
                false
            };
            if existing {
                if let Some(e) = &d.init {
                    if let ast::Pat::Ident(i) = &d.name {
                        self.value(e, i.id.sym.to_string())?;
                    }
                }
            } else if let Some(e) = &d.init {
                self.binding(&d.name, e, v.kind != ast::VarDeclKind::Const)?;
            } else if let ast::Pat::Ident(i) = &d.name {
                if v.kind == ast::VarDeclKind::Const {
                    return self.error(d.span, "const needs an initializer");
                }
                self.declare(i.id.sym.as_ref(), true, d.span)?;
                self.emit(
                    d.span,
                    Op::Set(i.id.sym.to_string(), Expr::Value(Value::Null)),
                );
            } else {
                return self.error(d.span, "destructuring needs an initializer");
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
    fn declare(&mut self, name: &str, mutable: bool, span: Span) -> Result<()> {
        if matches!(
            name,
            "tools" | "call" | "fork" | "join" | "ai" | "assert" | "JSON"
        ) || name.starts_with("$temp")
            || self.vars.contains_key(name)
            || self.libraries.contains_key(name)
        {
            return self.error(span, "duplicate or reserved local binding");
        }
        self.vars.insert(name.into(), mutable);
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
        Ok(match e {
            E::Lit(ast::Lit::Str(s)) => Expr::Value(json!(s.value.to_string_lossy())),
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
            E::Ident(i) if self.vars.contains_key(i.sym.as_ref()) => {
                Expr::Variable(i.sym.to_string())
            }
            E::Ident(_) => {
                return self.error(
                    e.span(),
                    "unknown variable; pass parent values as function parameters",
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
    fn effect(&self, call: &ast::CallExpr) -> Result<Effect> {
        if call.args.iter().any(|a| a.spread.is_some()) {
            return self.error(call.span, "argument spreads are unsupported");
        }
        let ast::Callee::Expr(target) = &call.callee else {
            return self.error(call.span, "use a declared helper or tools.EXACT_NAME");
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
            ast::Expr::Ident(i) if self.names.contains(i.sym.as_ref()) => {
                if i.sym == *"main" {
                    return self.error(call.span, "main is the sole entry, not a callable helper");
                }
                if self.arities[i.sym.as_ref()] != args.len() {
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
            ast::Expr::Member(m) if matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools") => {
                match &m.prop {
                    ast::MemberProp::Ident(i) => i.sym.to_string(),
                    ast::MemberProp::Computed(c) => match &*c.expr {
                        ast::Expr::Lit(ast::Lit::Str(s)) => s.value.to_string_lossy().into_owned(),
                        _ => return self.error(call.span, "tool names must be static"),
                    },
                    _ => return self.error(call.span, "invalid tool name"),
                }
            }
            _ => {
                return self.error(
                    call.span,
                    "use helper(args), tools.EXACT_NAME(args), fork(helper,args) or join(handles); call/ai wrappers are unsupported",
                );
            }
        };
        if fork {
            return self.error(call.span, "fork takes a declared helper function");
        }
        if matches!(
            target.as_str(),
            "execute_program"
                | "define_task_body"
                | "create_task"
                | "spawn_agent"
                | "join_agents"
                | "return_task"
        ) {
            return self.error(call.span,"use local functions and fork/join rather than recursively controlling program/task execution");
        }
        if args.len() > 1 {
            return self.error(call.span, "a tool call takes one JSON argument object");
        }
        Ok(Effect::Tool(target, args))
    }
    fn lower(&mut self, e: &ast::Expr) -> Result<Expr> {
        match e {
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
                    _ => return self.error(b.span, "unsupported numeric/logical operator"),
                };
                let left = self.lower(&b.left)?;
                if matches!(op, "&&" | "||" | "??") {
                    return self.logical(op, left, &b.right, b.span);
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
                let ast::AssignTarget::Simple(ast::SimpleAssignTarget::Ident(i)) = &a.left else {
                    return self.error(a.span, "only local variable assignment is supported");
                };
                let name = i.id.sym.to_string();
                self.local_target(&name, a.span, a.op == ast::AssignOp::Assign)?;
                let op = match a.op {
                    ast::AssignOp::Assign => None,
                    ast::AssignOp::AddAssign => Some("+"),
                    ast::AssignOp::SubAssign => Some("-"),
                    ast::AssignOp::MulAssign => Some("*"),
                    ast::AssignOp::DivAssign => Some("/"),
                    ast::AssignOp::ModAssign => Some("%"),
                    _ => return self.error(a.span, "unsupported compound assignment"),
                };
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
                let ast::Expr::Ident(i) = &*u.arg else {
                    return self.error(u.span, "update requires a local variable");
                };
                let name = i.sym.to_string();
                self.local_target(&name, u.span, false)?;
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
                // Local result collection is a deterministic language operation,
                // like for...of, not arbitrary JavaScript method execution.
                if let ast::Callee::Expr(target) = &call.callee
                    && let ast::Expr::Member(m) = &**target
                    && let ast::Expr::Ident(array) = &*m.obj
                    && matches!(&m.prop,ast::MemberProp::Ident(p) if p.sym==*"push")
                    && self.vars.contains_key(array.sym.as_ref())
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
                if matches!(&call.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Member(m) if !matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools" || self.libraries.contains_key(i.sym.as_ref()))))
                {
                    let ast::Callee::Expr(target) = &call.callee else {
                        unreachable!()
                    };
                    let ast::Expr::Member(member) = &**target else {
                        unreachable!()
                    };
                    let ast::MemberProp::Ident(method) = &member.prop else {
                        return self
                            .error(call.span, "data method names must be static identifiers");
                    };
                    if call.args.iter().any(|a| a.spread.is_some()) {
                        return self.error(call.span, "method argument spreads are unsupported");
                    }
                    let name = method.sym.as_ref();
                    let json = matches!(&*member.obj,ast::Expr::Ident(i) if i.sym==*"JSON");
                    let (min, max) = if json {
                        if !matches!(name, "stringify" | "parse") {
                            return self.error(
                                call.span,
                                "only JSON.stringify and JSON.parse are available",
                            );
                        }
                        (1, 1)
                    } else {
                        super::values::method_arity(name).ok_or_else(|| {let at=self.at(call.span);anyhow::anyhow!("TS_PROGRAM {}:{}: unsupported data method; use an explicit helper task",at.line,at.column)})?
                    };
                    if !(min..=max).contains(&call.args.len()) {
                        return self.error(call.span, "invalid data method argument count");
                    }
                    let receiver = if json {
                        Expr::Value(Value::Null)
                    } else {
                        let value = self.lower(&member.obj)?;
                        self.snapshot(member.obj.span(), value)
                    };
                    let mut args = Vec::new();
                    for arg in &call.args {
                        let value = self.lower(&arg.expr)?;
                        args.push(self.snapshot(arg.span(), value));
                    }
                    return Ok(if json {
                        if name == "stringify" {
                            Expr::Stringify(Box::new(args.remove(0)))
                        } else {
                            Expr::ParseJson(Box::new(args.remove(0)))
                        }
                    } else {
                        Expr::StringMethod(Box::new(receiver), name.into(), args)
                    });
                }
                if call.args.iter().any(|a| a.spread.is_some()) {
                    return self.error(call.span, "argument spread is unsupported");
                }
                let mut args = Vec::new();
                let target = match &call.callee {
                    ast::Callee::Expr(e) => &**e,
                    _ => {
                        return self
                            .error(call.span, "use a declared function or tools.EXACT_NAME");
                    }
                };
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
                    self.vars.insert(
                        match &*args.last().unwrap().expr {
                            ast::Expr::Ident(i) => i.sym.to_string(),
                            _ => unreachable!(),
                        },
                        false,
                    );
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
                            _ => return self.error(p.span(), "object methods are unsupported"),
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
            ast::Expr::Member(m) => {
                let object = self.lower(&m.obj)?;
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
    fn binding(&mut self, p: &ast::Pat, e: &ast::Expr, mutable: bool) -> Result<()> {
        match p {
            ast::Pat::Ident(i) => {
                self.declare(i.id.sym.as_ref(), mutable, p.span())?;
                self.value(e, i.id.sym.to_string())?;
            }
            ast::Pat::Array(a) => {
                let temp = self.temp();
                self.value(e, temp.clone())?;
                for (index, p) in a.elems.iter().enumerate() {
                    match p {
                        Some(ast::Pat::Ident(i)) => {
                            self.declare(i.id.sym.as_ref(), mutable, i.span())?;
                            self.emit(
                                p.as_ref().unwrap().span(),
                                Op::Set(
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
                for s in &b.stmts {
                    self.stmt(s)?;
                }
            }
            S::Empty(_) => {}
            S::Decl(ast::Decl::Var(v)) => self.variable_declaration(v)?,
            S::Expr(s) => {
                self.discard(&s.expr)?;
            }
            S::Return(r) => {
                let value = if let Some(e) = &r.arg {
                    self.lower(e)?
                } else {
                    Expr::Value(Value::Null)
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
                if let Some(init) = &f.init {
                    match init {
                        ast::VarDeclOrExpr::VarDecl(v) => self.variable_declaration(v)?,
                        ast::VarDeclOrExpr::Expr(e) => {
                            self.discard(e)?;
                        }
                    }
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
                if let Some(update) = &f.update {
                    self.discard(update)?;
                }
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::Branch(_, target) = &mut self.code[exit].op {
                    *target = end;
                }
                self.end_loop(end, next);
            }
            S::Break(b) => {
                if b.label.is_some() || self.loops.is_empty() {
                    return self.error(b.span, "break requires an enclosing loop and no label");
                }
                let handlers = self.loops.last().unwrap().handlers;
                for _ in handlers..self.handlers {
                    self.emit(b.span, Op::EndCatch);
                }
                let at = self.emit(b.span, Op::Jump(0));
                self.loops.last_mut().unwrap().breaks.push(at);
            }
            S::Continue(c) => {
                if c.label.is_some() || self.loops.is_empty() {
                    return self.error(c.span, "continue requires an enclosing loop and no label");
                }
                let handlers = self.loops.last().unwrap().handlers;
                for _ in handlers..self.handlers {
                    self.emit(c.span, Op::EndCatch);
                }
                let at = self.emit(c.span, Op::Jump(0));
                self.loops.last_mut().unwrap().continues.push(at);
            }
            S::ForOf(f) => {
                if f.is_await {
                    return self.error(f.span, "for await is unsupported");
                }
                let name = match &f.left {
                    ast::ForHead::VarDecl(v) if v.decls.len() == 1 && v.decls[0].init.is_none() => {
                        match &v.decls[0].name {
                            ast::Pat::Ident(i) => {
                                if !(v.kind == ast::VarDeclKind::Var
                                    && self.vars.get(i.id.sym.as_ref()) == Some(&true))
                                {
                                    self.declare(
                                        i.id.sym.as_ref(),
                                        v.kind != ast::VarDeclKind::Const,
                                        i.span(),
                                    )?;
                                }
                                i.id.sym.to_string()
                            }
                            _ => return self.error(f.span, "use a simple for...of binding"),
                        }
                    }
                    _ => return self.error(f.span, "use for (const item of array)"),
                };
                let items = self.temp();
                let index = self.temp();
                let list = self.lower(&f.right)?;
                self.emit(f.span, Op::Set(items.clone(), list));
                self.emit(f.span, Op::Set(index.clone(), Expr::Value(json!(0))));
                let start = self.code.len();
                let next = self.emit(
                    f.span,
                    Op::ForNext {
                        items,
                        index,
                        item: name,
                        end: 0,
                    },
                );
                self.begin_loop();
                self.stmt(&f.body)?;
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
                let name = match &h.param {
                    Some(ast::Pat::Ident(i)) => i.id.sym.to_string(),
                    None => self.temp(),
                    _ => return self.error(h.span, "use catch(error)"),
                };
                if self.vars.contains_key(&name) {
                    return self.error(h.span, "catch binding must have a distinct name");
                }
                self.vars.insert(name.clone(), false);
                let start = self.emit(t.span, Op::Catch { target: 0, name });
                self.handlers += 1;
                for s in &t.block.stmts {
                    self.stmt(s)?;
                }
                self.handlers -= 1;
                self.emit(t.span, Op::EndCatch);
                let jump = self.emit(t.span, Op::Jump(0));
                let handler = self.code.len();
                for s in &h.body.stmts {
                    self.stmt(s)?;
                }
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

pub fn compile(source: &str) -> Result<Program> {
    compile_with_libraries(source, super::libraries::builtin())
}
pub fn compile_body(source: &str, entry: &str) -> Result<Program> {
    compile_body_with_libraries(source, entry, super::libraries::builtin())
}
pub fn compile_with_libraries(
    source: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    compile_inner(source, "main", false, libraries)
}
pub fn compile_body_with_libraries(
    source: &str,
    entry: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    let program = compile_inner(source, entry, true, libraries)?;
    ensure!(
        !program.functions[entry].comment_only,
        "define_task_body must implement the current function, not return another empty stub"
    );
    Ok(program)
}
pub fn compile_entry(source: &str, entry: &str) -> Result<Program> {
    compile_entry_with_libraries(source, entry, super::libraries::builtin())
}
pub fn compile_entry_with_libraries(
    source: &str,
    entry: &str,
    libraries: super::libraries::Libraries,
) -> Result<Program> {
    compile_inner(source, entry, false, libraries)
}
fn compile_inner(
    source: &str,
    entry: &str,
    refinement: bool,
    libraries: super::libraries::Libraries,
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
    let mut declarations = Vec::new();
    for item in module.body {
        match item {
            ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(f))) => declarations.push(f),
            other => {
                let p = map.lookup_char_pos(other.span().lo);
                bail!(
                    "TS_PROGRAM {}:{}: top level only supports ordinary function declarations; no imports, exports or type declarations",
                    p.line,
                    p.col.0 + 1
                );
            }
        }
    }
    ensure!(
        !declarations.is_empty() && declarations.len() <= 64,
        "TS_PROGRAM needs 1..=64 functions"
    );
    let mut names = BTreeSet::new();
    for f in &declarations {
        ensure!(
            !libraries.contains_key(f.ident.sym.as_ref()),
            "function name conflicts with library namespace {}",
            f.ident.sym
        );
        ensure!(
            !matches!(
                f.ident.sym.as_ref(),
                "tools" | "call" | "fork" | "join" | "ai" | "assert" | "JSON"
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
    let arities = declarations
        .iter()
        .map(|f| (f.ident.sym.to_string(), f.function.params.len()))
        .collect::<BTreeMap<_, _>>();
    for f in declarations {
        let mut c = Compiler {
            map: map.clone(),
            names: names.clone(),
            arities: arities.clone(),
            libraries: libraries.clone(),
            vars: BTreeMap::new(),
            code: Vec::new(),
            temp: 0,
            loops: Vec::new(),
            handlers: 0,
        };
        if f.function.is_async || f.function.is_generator {
            return c.error(
                f.function.span,
                "only ordinary functions are allowed; function*, yield and async/await are unsupported",
            );
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
        let description = notes
            .iter()
            .filter(|n| n.span.lo >= body.span.lo && n.span.hi <= body.span.hi)
            .map(|n| n.text.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        let empty = body.stmts.iter().all(|s| matches!(s, ast::Stmt::Empty(_)));
        if empty && !description.is_empty() {
            if f.ident.sym == *"main" {
                return c.error(
                    body.span,
                    "main must orchestrate the complete task; put comment-only work in a helper",
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
                    "an empty helper needs comments describing its task and return value",
                );
            }
            for s in &body.stmts {
                c.stmt(s)?;
            }
            c.emit(body.span, Op::Return(Expr::Value(Value::Null)));
        }

        functions.insert(
            f.ident.sym.to_string(),
            Function {
                description,
                comment_only: empty,
                params,
                code: c.code,
            },
        );
    }
    Ok(Program {
        version: VERSION,
        source: source.into(),
        functions,
        libraries,
    })
}
