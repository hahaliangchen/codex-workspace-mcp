use super::*;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use swc_common::{
    FileName, SourceMap, SourceMapper, Span, Spanned, comments::SingleThreadedComments, sync::Lrc,
};
use swc_ecma_ast as ast;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

struct Compiler {
    map: Lrc<SourceMap>,
    names: BTreeSet<String>,
    vars: BTreeMap<String, bool>,
    code: Vec<Instruction>,
    temp: usize,
    description: String,
}

/// Inspect opaque semantic expressions without implementing their operators or
/// methods. Calls with real effects must stay visible to the normal dispatcher.
struct SemanticInputs<'a> {
    compiler: &'a Compiler,
    names: BTreeSet<String>,
    error: Option<(Span, &'static str)>,
}
impl Visit for SemanticInputs<'_> {
    fn visit_prop(&mut self, p: &ast::Prop) {
        if !matches!(p, ast::Prop::KeyValue(_) | ast::Prop::Shorthand(_)) {
            self.error = Some((
                p.span(),
                "semantic object methods and accessors are unsupported",
            ));
            return;
        }
        p.visit_children_with(self);
    }
    fn visit_spread_element(&mut self, s: &ast::SpreadElement) {
        self.error = Some((s.span(), "semantic spreads are unsupported"));
    }
    fn visit_expr(&mut self, e: &ast::Expr) {
        use ast::Expr as E;
        match e {
            E::Lit(_)
            | E::Ident(_)
            | E::Bin(_)
            | E::Cond(_)
            | E::Tpl(_)
            | E::Member(_)
            | E::Paren(_)
            | E::TsAs(_)
            | E::Array(_)
            | E::Object(_)
            | E::Call(_) => {}
            E::Unary(u) if u.op != ast::UnaryOp::Delete => {}
            _ => {
                self.error = Some((
                    e.span(),
                    "semantic expressions describe values, not mutations, closures or executable JavaScript",
                ));
                return;
            }
        }
        e.visit_children_with(self);
    }
    fn visit_ident(&mut self, i: &ast::Ident) {
        if self.compiler.vars.contains_key(i.sym.as_ref()) {
            self.names.insert(i.sym.to_string());
        }
    }
    fn visit_call_expr(&mut self, call: &ast::CallExpr) {
        // Only descriptive methods are opaque. Never hide a tool/helper call
        // inside a condition (in particular a short-circuit expression).
        let descriptive = matches!(&call.callee, ast::Callee::Expr(e)
            if matches!(&**e, ast::Expr::Member(m)
                if !matches!(&*m.obj, ast::Expr::Ident(i) if i.sym == *"tools")
                    && !(matches!(&*m.obj, ast::Expr::Ident(i) if self.compiler.vars.contains_key(i.sym.as_ref()))
                        && matches!(&m.prop, ast::MemberProp::Ident(i) if i.sym == *"push"))));
        if !descriptive {
            self.error = Some((
                call.span,
                "bind task/tool calls to variables before using them in a semantic expression",
            ));
            return;
        }
        if call.args.iter().any(|a| a.spread.is_some()) {
            self.error = Some((call.span, "semantic argument spreads are unsupported"));
            return;
        }
        call.visit_children_with(self);
    }
}
impl Compiler {
    fn semantic(&mut self, e: &ast::Expr, boolean: bool, direct: Option<Expr>) -> Result<Expr> {
        let mut inputs = SemanticInputs {
            compiler: self,
            names: BTreeSet::new(),
            error: None,
        };
        // A direct call has already been lowered through the real dispatcher.
        // Its result is supplied separately, never requested again from the AI.
        if direct.is_none() {
            e.visit_with(&mut inputs);
            if let Some((span, message)) = inputs.error {
                return self.error(span, message);
            }
        } else {
            struct References(BTreeSet<String>);
            impl Visit for References {
                fn visit_ident(&mut self, i: &ast::Ident) {
                    self.0.insert(i.sym.to_string());
                }
            }
            let mut refs = References(BTreeSet::new());
            e.visit_with(&mut refs);
            inputs.names = refs
                .0
                .into_iter()
                .filter(|n| self.vars.contains_key(n))
                .collect();
        }
        let mut fields = inputs
            .names
            .into_iter()
            .map(|n| (n.clone(), Expr::Variable(n)))
            .collect::<Vec<_>>();
        if let Some(value) = &direct {
            fields.push(("$condition_value".into(), value.clone()));
        }
        let expression = self
            .map
            .span_to_snippet(e.span())
            .map_err(|e| anyhow::anyhow!("source expression unavailable: {e:?}"))?;
        let destination = self.temp();
        self.emit(
            e.span(),
            Op::Effect(
                Some(destination.clone()),
                Effect::Semantic {
                    expression,
                    description: self.description.clone(),
                    bindings: Expr::Object(fields),
                    boolean,
                    direct,
                },
            ),
        );
        Ok(Expr::Variable(destination))
    }
    fn condition(&mut self, e: &ast::Expr) -> Result<Expr> {
        match e {
            ast::Expr::Ident(i) if !self.vars.contains_key(i.sym.as_ref()) => {
                self.semantic(e, true, None)
            }
            ast::Expr::Paren(p) => self.condition(&p.expr),
            ast::Expr::TsAs(p) => self.condition(&p.expr),
            ast::Expr::Bin(_) | ast::Expr::Unary(_) | ast::Expr::Cond(_) | ast::Expr::Tpl(_) => {
                self.semantic(e, true, None)
            }
            ast::Expr::Call(c) if matches!(&c.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Member(m) if !matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools"))) => {
                self.semantic(e, true, None)
            }
            _ => {
                let direct = self.lower(e)?;
                self.semantic(e, true, Some(direct))
            }
        }
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
    fn declare(&mut self, name: &str, mutable: bool, span: Span) -> Result<()> {
        if matches!(
            name,
            "tools" | "call" | "fork" | "join" | "ai" | "assert" | "JSON"
        ) || name.starts_with("$temp")
            || self.vars.contains_key(name)
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
                ensure!(n.value.is_finite(), "non-finite number");
                Expr::Value(number_value(n.value))
            }
            E::Lit(ast::Lit::Bool(b)) => Expr::Value(json!(b.value)),
            E::Lit(ast::Lit::Null(_)) => Expr::Value(Value::Null),
            E::Ident(i) if i.sym == *"undefined" => Expr::Value(Value::Null),
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
            E::TsAs(p) => return self.expr(&p.expr),
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
            E::Tpl(t) if t.exprs.is_empty() => Expr::Value(json!(
                t.quasis[0]
                    .cooked
                    .as_ref()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| t.quasis[0].raw.to_string())
            )),
            E::Bin(_)
            | E::Unary(_)
            | E::Update(_)
            | E::Assign(_)
            | E::Cond(_)
            | E::TsNonNull(_)
            | E::Tpl(_) => {
                return self.error(e.span(),"this expression requires semantic lowering; non-null assertions and mutation operators are unsupported");
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
                    "unsupported expression; use JSON values and yield call/fork/join/ai",
                );
            }
        })
    }
    fn effect(&self, e: &ast::YieldExpr) -> Result<Effect> {
        if e.delegate {
            return self.error(
                e.span,
                "yield* is unsupported; use yield call(helper, ...args)",
            );
        }
        let call = match e.arg.as_deref() {
            Some(ast::Expr::Call(c)) => c,
            _ => return self.error(e.span, "yield requires an effect call"),
        };
        let name = match &call.callee {
            ast::Callee::Expr(x) => match &**x {
                ast::Expr::Ident(i) => i.sym.as_ref(),
                _ => return self.error(e.span, "unknown effect"),
            },
            _ => return self.error(e.span, "unknown effect"),
        };
        if call.args.iter().any(|a| a.spread.is_some()) {
            return self.error(e.span, "effect spreads are unsupported");
        }
        if name == "join" {
            ensure!(!call.args.is_empty(), "join needs handles");
            return Ok(Effect::Join(
                call.args
                    .iter()
                    .map(|a| self.expr(&a.expr))
                    .collect::<Result<_>>()?,
            ));
        }
        if name == "ai" {
            if call.args.len() != 1 {
                return self.error(e.span, "ai takes one configuration object");
            }
            return Ok(Effect::Ai(self.expr(&call.args[0].expr)?));
        }
        if !matches!(name, "call" | "fork") {
            return self.error(e.span, "unknown effect; use call, fork, join or ai");
        }
        let first = call.args.first().context("call/fork needs a target")?;
        let args = call.args[1..]
            .iter()
            .map(|a| self.expr(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let target = match &*first.expr {
            ast::Expr::Ident(i) if self.names.contains(i.sym.as_ref()) => {
                return Ok(if name == "fork" {
                    Effect::Fork(i.sym.to_string(), args)
                } else {
                    Effect::Call(i.sym.to_string(), args)
                });
            }
            ast::Expr::Member(m) if matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools") => {
                match &m.prop {
                    ast::MemberProp::Ident(i) => i.sym.to_string(),
                    ast::MemberProp::Computed(c) => match &*c.expr {
                        ast::Expr::Lit(ast::Lit::Str(s)) => s.value.to_string_lossy().into_owned(),
                        _ => return self.error(e.span, "tool names must be static"),
                    },
                    _ => return self.error(e.span, "invalid tool name"),
                }
            }
            ast::Expr::Lit(ast::Lit::Str(s)) => s.value.to_string_lossy().into_owned(),
            _ => {
                return self.error(
                    e.span,
                    "target must be a declared function or tools.EXACT_TOOL_NAME; descriptive methods belong in semantic expressions",
                );
            }
        };
        if name == "fork" {
            return self.error(e.span, "fork takes a declared helper function");
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
            return self.error(e.span,"use local functions and fork/join rather than recursively controlling program/task execution");
        }
        if args.len() > 1 {
            return self.error(e.span, "a tool call takes one JSON argument object");
        }
        Ok(Effect::Tool(target, args))
    }
    fn lower(&mut self, e: &ast::Expr) -> Result<Expr> {
        match e {
            ast::Expr::Bin(_) | ast::Expr::Unary(_) | ast::Expr::Cond(_) => {
                self.semantic(e, false, None)
            }
            ast::Expr::Tpl(t) if !t.exprs.is_empty() => self.semantic(e, false, None),
            ast::Expr::Assign(a) => {
                if a.op != ast::AssignOp::Assign {
                    return self.error(
                        a.span,
                        "use a simple local assignment to save a task result",
                    );
                }
                let ast::AssignTarget::Simple(ast::SimpleAssignTarget::Ident(i)) = &a.left else {
                    return self.error(
                        a.span,
                        "assignment binds a local value; member mutation is unsupported",
                    );
                };
                let name = i.id.sym.to_string();
                match self.vars.get(&name) {
                    Some(false) => {
                        return self.error(a.span, "cannot rebind a const value; use let");
                    }
                    Some(true) => {}
                    None => self.declare(&name, true, a.span)?,
                }
                let value = self.lower(&a.right)?;
                self.emit(a.span, Op::Set(name.clone(), value));
                Ok(Expr::Variable(name))
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
                        Expr::Value(json!({"message":"semantic assertion failed"}))
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
                    let items = call
                        .args
                        .iter()
                        .map(|a| self.lower(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
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
                if matches!(&call.callee,ast::Callee::Expr(e) if matches!(&**e,ast::Expr::Member(m) if !matches!(&*m.obj,ast::Expr::Ident(i) if i.sym==*"tools")))
                {
                    return self.semantic(e, false, None);
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
                let builtin = matches!(target,ast::Expr::Ident(i) if matches!(i.sym.as_ref(),"call"|"fork"|"join"|"ai"));
                for (index, arg) in call.args.iter().enumerate() {
                    if index == 0
                        && matches!(target,ast::Expr::Ident(i) if matches!(i.sym.as_ref(),"call"|"fork"))
                    {
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
                if !builtin {
                    translated.args.insert(
                        0,
                        ast::ExprOrSpread {
                            spread: None,
                            expr: Box::new(target.clone()),
                        },
                    );
                    translated.callee = ast::Callee::Expr(Box::new(ast::Expr::Ident(
                        ast::Ident::new_no_ctxt("call".into(), call.span),
                    )));
                }
                let effect = self.effect(&ast::YieldExpr {
                    span: call.span,
                    arg: Some(Box::new(ast::Expr::Call(translated))),
                    delegate: false,
                })?;
                let dest = self.temp();
                self.emit(call.span, Op::Effect(Some(dest.clone()), effect));
                Ok(Expr::Variable(dest))
            }
            ast::Expr::Yield(y) => {
                if y.delegate {
                    return self.error(y.span, "yield* is unsupported; use a normal helper call");
                }
                match y.arg.as_deref() {
                    Some(ast::Expr::Call(call)) => self.lower(&ast::Expr::Call(call.clone())),
                    _ => self.error(y.span, "yield requires an effect call"),
                }
            }
            ast::Expr::Paren(p) => self.lower(&p.expr),
            ast::Expr::TsAs(p) => self.lower(&p.expr),
            ast::Expr::Object(o) => {
                let mut fields = Vec::new();
                for p in &o.props {
                    match p {
                        ast::PropOrSpread::Prop(p) => match &**p {
                            ast::Prop::KeyValue(p) => {
                                let key = self.property(&p.key)?;
                                fields.push((key, self.lower(&p.value)?));
                            }
                            ast::Prop::Shorthand(i) => fields.push((
                                i.sym.to_string(),
                                self.expr(&ast::Expr::Ident(i.clone()))?,
                            )),
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
                        Some(e) if e.spread.is_none() => values.push(self.lower(&e.expr)?),
                        _ => return self.error(a.span, "array holes/spread are unsupported"),
                    }
                }
                Ok(Expr::Array(values))
            }
            ast::Expr::Member(m) => {
                let object = self.lower(&m.obj)?;
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
            S::Decl(ast::Decl::Var(v)) => {
                if v.kind == ast::VarDeclKind::Var {
                    return self.error(v.span, "use const or let");
                }
                for d in &v.decls {
                    let e = d
                        .init
                        .as_ref()
                        .context("variables require an initializer")?;
                    self.binding(&d.name, e, v.kind == ast::VarDeclKind::Let)?;
                }
            }
            S::Expr(s) => match &*s.expr {
                ast::Expr::Yield(_) | ast::Expr::Call(_) => {
                    self.lower(&s.expr)?;
                }
                ast::Expr::Assign(_) => {
                    self.lower(&s.expr)?;
                }
                ast::Expr::Update(_) => {
                    return self.error(
                        s.span,
                        "use a simple local assignment to save a task result",
                    );
                }
                _ => {
                    return self.error(
                        s.span,
                        "expression statements must yield a task/tool effect",
                    );
                }
            },
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
            S::ForOf(f) => {
                if f.is_await {
                    return self.error(f.span, "for await is unsupported");
                }
                let name = match &f.left {
                    ast::ForHead::VarDecl(v) if v.decls.len() == 1 && v.decls[0].init.is_none() => {
                        match &v.decls[0].name {
                            ast::Pat::Ident(i) => {
                                self.declare(
                                    i.id.sym.as_ref(),
                                    v.kind == ast::VarDeclKind::Let,
                                    i.span(),
                                )?;
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
                self.stmt(&f.body)?;
                self.emit(f.span, Op::Jump(start));
                let end = self.code.len();
                if let Op::ForNext { end: target, .. } = &mut self.code[next].op {
                    *target = end;
                }
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
                for s in &t.block.stmts {
                    self.stmt(s)?;
                }
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
    compile_inner(source, "main", false)
}
pub fn compile_body(source: &str, entry: &str) -> Result<Program> {
    let program = compile_inner(source, entry, true)?;
    ensure!(
        !program.functions[entry].comment_only,
        "define_task_body must implement the current function, not return another empty stub"
    );
    Ok(program)
}
pub fn compile_entry(source: &str, entry: &str) -> Result<Program> {
    compile_inner(source, entry, false)
}
fn compile_inner(source: &str, entry: &str, refinement: bool) -> Result<Program> {
    ensure!(
        source.len() <= 100_000,
        "TS_PROGRAM source exceeds 100000 bytes"
    );
    let map: Lrc<SourceMap> = Default::default();
    let file = map.new_source_file(
        FileName::Custom("agent-program.ts".into()).into(),
        source.to_owned(),
    );
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax::default()),
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
        ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(f)))=>declarations.push(f),
        ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(ast::ExportDecl{decl:ast::Decl::Fn(f),..}))=>declarations.push(f),
        ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::TsInterface(_)|ast::Decl::TsTypeAlias(_)))=>{},
        ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(i)) if i.src.value==*"@agent/runtime" && i.specifiers.iter().all(|s|matches!(s,ast::ImportSpecifier::Named(n) if n.imported.is_none() && ["call","fork","join","ai","assert","tools"].contains(&n.local.sym.as_ref())))=>{},
        other=>{let p=map.lookup_char_pos(other.span().lo);bail!("TS_PROGRAM {}:{}: top level only supports functions, type declarations and @agent/runtime named imports",p.line,p.col.0+1);}
    }
    }
    ensure!(
        !declarations.is_empty() && declarations.len() <= 64,
        "TS_PROGRAM needs 1..=64 functions"
    );
    let mut names = BTreeSet::new();
    for f in &declarations {
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
    for f in declarations {
        let mut c = Compiler {
            map: map.clone(),
            names: names.clone(),
            vars: BTreeMap::new(),
            code: Vec::new(),
            temp: 0,
            description: String::new(),
        };
        if f.function.is_async {
            return c.error(
                f.function.span,
                "use ordinary functions, not async functions",
            );
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
        c.description = description.clone();
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
    })
}
