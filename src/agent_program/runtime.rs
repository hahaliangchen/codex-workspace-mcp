use super::*;
use anyhow::{Context, Result, bail, ensure};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    function: String,
    pc: usize,
    locals: BTreeMap<String, Value>,
    destination: Option<String>,
    handlers: Vec<(usize, String)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runtime {
    pub program: Program,
    #[serde(default)]
    pub scoped_calls: bool,
    frames: Vec<Frame>,
    #[serde(default)]
    inherited_scope: BTreeMap<String, Value>,
    pub pending: Option<Action>,
    pub finished: bool,
    pub result: Value,
    pub error: Option<Value>,
    fuel: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub at: Location,
    pub destination: Option<String>,
    pub kind: ActionKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ActionKind {
    Call {
        name: String,
        args: Vec<Value>,
    },
    Tool {
        name: String,
        args: Value,
    },
    ParallelReads {
        operations: Vec<(String, Value)>,
    },
    Fork {
        entry: String,
        args: Vec<Value>,
    },
    Join {
        ids: Vec<String>,
    },
    Task {
        name: String,
        description: String,
        input: Value,
    },
}
#[derive(Clone, Debug)]
pub enum State {
    Action(Action),
    Returned(Value),
    Failed(Value),
}

fn eval(e: &Expr, locals: &BTreeMap<String, Value>) -> Result<Value> {
    Ok(match e {
        Expr::Value(v) => v.clone(),
        Expr::Variable(name) => locals
            .get(name)
            .with_context(|| format!("uninitialized variable {name}"))?
            .clone(),
        Expr::Array(a) => Value::Array(a.iter().map(|e| eval(e, locals)).collect::<Result<_>>()?),
        Expr::Object(o) => Value::Object(
            o.iter()
                .map(|(k, e)| Ok((k.clone(), eval(e, locals)?)))
                .collect::<Result<_>>()?,
        ),
        Expr::Member(obj, key) => {
            let obj = eval(obj, locals)?;
            let key = eval(key, locals)?;
            match (&obj, &key) {
                (Value::Array(a), Value::String(k)) if k == "length" => json!(a.len()),
                (Value::String(s), Value::String(k)) if k == "length" => {
                    json!(s.chars().count())
                }
                (Value::Array(a), Value::Number(n)) => a
                    .get({
                        let index = n.as_f64().context("invalid array index")?;
                        ensure!(
                            index >= 0.0 && index.fract() == 0.0 && index < usize::MAX as f64,
                            "array index must be a nonnegative integer"
                        );
                        index as usize
                    })
                    .cloned()
                    .unwrap_or(Value::Null),
                (Value::Array(a), Value::String(k)) => a
                    .get(k.parse::<usize>().context("invalid array member")?)
                    .cloned()
                    .unwrap_or(Value::Null),
                (Value::Object(o), Value::String(k)) => o.get(k).cloned().unwrap_or(Value::Null),
                (Value::String(s), Value::Number(n)) => {
                    let index = n
                        .as_u64()
                        .context("string index must be a nonnegative integer")?;
                    s.chars()
                        .nth(usize::try_from(index).context("string index is too large")?)
                        .map(|c| json!(c.to_string()))
                        .unwrap_or(Value::Null)
                }
                _ => bail!("cannot access member {} on {}", key, obj),
            }
        }
        Expr::Binary(op, left, right) => {
            let left = eval(left, locals)?;
            match op.as_str() {
                "&&" => {
                    if !super::values::boolean(&left)? {
                        json!(false)
                    } else {
                        json!(super::values::boolean(&eval(right, locals)?)?)
                    }
                }
                "||" => {
                    if super::values::boolean(&left)? {
                        json!(true)
                    } else {
                        json!(super::values::boolean(&eval(right, locals)?)?)
                    }
                }
                "??" => {
                    if left.is_null() {
                        eval(right, locals)?
                    } else {
                        left
                    }
                }
                _ => super::values::binary(op, left, eval(right, locals)?)?,
            }
        }
        Expr::Unary(op, value) => super::values::unary(op, eval(value, locals)?)?,
        Expr::Conditional(test, left, right) => {
            if super::values::boolean(&eval(test, locals)?)? {
                eval(left, locals)?
            } else {
                eval(right, locals)?
            }
        }
        Expr::Stringify(value) => json!(serde_json::to_string(&eval(value, locals)?)?),
        Expr::ParseJson(value) => {
            let value = eval(value, locals)?;
            serde_json::from_str(value.as_str().context("JSON.parse needs a string")?)
                .context("invalid JSON")?
        }
        Expr::Template(parts) => json!(
            parts
                .iter()
                .map(|p| super::values::text(&eval(p, locals)?))
                .collect::<Result<Vec<_>>>()?
                .concat()
        ),
        Expr::StringMethod(receiver, name, args) => super::values::method(
            eval(receiver, locals)?,
            name,
            args.iter()
                .map(|arg| eval(arg, locals))
                .collect::<Result<_>>()?,
        )?,
        Expr::NodeBuiltin(name, args) => super::builtins::call(
            name,
            args.iter()
                .map(|arg| eval(arg, locals))
                .collect::<Result<_>>()?,
        )?,
    })
}

impl Runtime {
    /// Live public variables across caller frames; inner bindings shadow outer
    /// ones. Compiler temporaries never become inherited model context.
    pub fn named_bindings(&self) -> BTreeMap<String, Value> {
        let mut bindings = BTreeMap::new();
        for frame in &self.frames {
            bindings.extend(
                frame
                    .locals
                    .iter()
                    .filter(|(name, _)| !name.starts_with("$temp"))
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        }
        bindings
    }
    /// Conservative reachability over the remaining control flow. Include
    /// back-edges and exception handlers; never infer liveness from comments.
    fn collect_dead_bindings(&mut self) {
        fn expression(value: &Expr, names: &mut std::collections::BTreeSet<String>) {
            match value {
                Expr::Variable(name) => {
                    names.insert(name.clone());
                }
                Expr::Array(items) | Expr::Template(items) | Expr::NodeBuiltin(_, items) => {
                    for item in items {
                        expression(item, names);
                    }
                }
                Expr::Object(fields) => {
                    for (_, item) in fields {
                        expression(item, names);
                    }
                }
                Expr::Member(a, b) | Expr::Binary(_, a, b) => {
                    expression(a, names);
                    expression(b, names);
                }
                Expr::Unary(_, a) | Expr::Stringify(a) | Expr::ParseJson(a) => expression(a, names),
                Expr::Conditional(a, b, c) => {
                    expression(a, names);
                    expression(b, names);
                    expression(c, names);
                }
                Expr::StringMethod(a, _, args) => {
                    expression(a, names);
                    for arg in args {
                        expression(arg, names);
                    }
                }
                Expr::Value(_) => {}
            }
        }
        for frame in &mut self.frames {
            let code = &self.program.functions[&frame.function].code;
            let mut pending = vec![frame.pc];
            pending.extend(frame.handlers.iter().map(|(target, _)| *target));
            let mut visited = std::collections::BTreeSet::new();
            let mut live = std::collections::BTreeSet::new();
            let mut dynamic_scope = false;
            while let Some(pc) = pending.pop() {
                if pc >= code.len() || !visited.insert(pc) {
                    continue;
                }
                let mut next = true;
                match &code[pc].op {
                    Op::Set(_, e) => expression(e, &mut live),
                    Op::Return(e) | Op::Throw(e) => {
                        expression(e, &mut live);
                        next = false;
                    }
                    Op::Branch(e, target) => {
                        expression(e, &mut live);
                        pending.push(*target);
                    }
                    Op::Jump(target) => {
                        pending.push(*target);
                        next = false;
                    }
                    Op::ForNext {
                        items, index, end, ..
                    } => {
                        live.insert(items.clone());
                        live.insert(index.clone());
                        pending.push(*end);
                    }
                    Op::Catch { target, .. } => pending.push(*target),
                    Op::EndCatch => {}
                    Op::Append { target, items, .. } => {
                        live.insert(target.clone());
                        for item in items {
                            expression(item, &mut live);
                        }
                    }
                    Op::Effect(_, effect) => match effect {
                        Effect::ParallelReads(operations) => {
                            for (_, args) in operations {
                                for arg in args {
                                    expression(arg, &mut live);
                                }
                            }
                        }
                        Effect::ParallelReadMap { items, args, .. } => {
                            expression(items, &mut live);
                            for arg in args {
                                expression(arg, &mut live);
                            }
                        }
                        Effect::Tool(name, args) => {
                            dynamic_scope |= name == "read_scope";
                            for arg in args {
                                expression(arg, &mut live);
                            }
                        }
                        Effect::Call(_, args) | Effect::Fork(_, args) | Effect::Join(args) => {
                            for arg in args {
                                expression(arg, &mut live);
                            }
                        }
                        Effect::Task { input: e, .. } => expression(e, &mut live),
                    },
                }
                if next {
                    pending.push(pc + 1);
                }
            }
            if let Some(Action {
                kind: ActionKind::Tool { name, args },
                ..
            }) = &self.pending
                && name == "read_scope"
                && let Some(name) = args["name"].as_str()
            {
                live.insert(name.to_owned());
            }
            // Source-level variables are visible to future AI scopes even
            // when deterministic code no longer references them. Only
            // compiler-generated temporaries may be garbage-collected here.
            frame.locals.retain(|name, _| {
                !name.starts_with("$temp") || live.contains(name) || dynamic_scope
            });
        }
    }
    pub fn new(program: Program, entry: &str, args: Vec<Value>) -> Result<Self> {
        Self::new_with_scope(program, entry, args, BTreeMap::new())
    }
    fn new_with_scope(
        program: Program,
        entry: &str,
        args: Vec<Value>,
        inherited_scope: BTreeMap<String, Value>,
    ) -> Result<Self> {
        ensure!(program.version == VERSION, "unsupported program version");
        let frame = Self::frame(&program, entry, args, None, &inherited_scope)?;
        Ok(Self {
            program,
            scoped_calls: false,
            frames: vec![frame],
            inherited_scope,
            pending: None,
            finished: false,
            result: Value::Null,
            error: None,
            fuel: 100_000,
        })
    }
    /// Virtual calls become durable AI task scopes; ordinary calls use frames.
    pub fn task_program(program: Program, entry: &str, args: Vec<Value>) -> Result<Self> {
        let mut runtime = Self::new(program, entry, args)?;
        runtime.scoped_calls = true;
        Ok(runtime)
    }
    pub fn task_program_with_scope(
        program: Program,
        entry: &str,
        args: Vec<Value>,
        inherited_scope: BTreeMap<String, Value>,
    ) -> Result<Self> {
        let mut runtime = Self::new_with_scope(program, entry, args, inherited_scope)?;
        runtime.scoped_calls = true;
        Ok(runtime)
    }
    fn frame(
        program: &Program,
        name: &str,
        args: Vec<Value>,
        destination: Option<String>,
        scope: &BTreeMap<String, Value>,
    ) -> Result<Frame> {
        let f = program
            .functions
            .get(name)
            .with_context(|| format!("unknown function {name}"))?;
        ensure!(
            args.len() == f.params.len(),
            "{name}: expected {} arguments, got {}",
            f.params.len(),
            args.len()
        );
        let mut locals = f
            .captures
            .iter()
            .map(|name| {
                scope
                    .get(name)
                    .cloned()
                    .map(|value| (name.clone(), value))
                    .with_context(|| {
                        format!("{name}: captured variable is not visible at the call site")
                    })
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        locals.extend(f.params.iter().cloned().zip(args));
        Ok(Frame {
            function: name.into(),
            pc: 0,
            locals,
            destination,
            handlers: Vec::new(),
        })
    }
    fn raise(&mut self, mut value: Value, at: &Location) {
        if !value.is_object() {
            value = json!({"message":value});
        } else if value.get("message").is_none() {
            // Keep the original tool error while giving callers a stable
            // exception message across tool and nested-task boundaries.
            value["message"] = value
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!(value.to_string()));
        }
        if value.get("location").is_none() {
            value["location"] = json!(at);
        }
        let stack = self
            .frames
            .iter()
            .rev()
            .map(|f| json!({"function":f.function,"pc":f.pc.saturating_sub(1)}))
            .collect::<Vec<_>>();
        if value.get("stack").is_none() {
            value["stack"] = json!(stack);
        }
        // Keep the failed frames intact when nobody can catch the error. A
        // scoped AI continuation can inspect their actual locals without
        // replaying the instructions or their external effects.
        if !self.frames.iter().any(|frame| !frame.handlers.is_empty()) {
            self.finished = true;
            self.error = Some(value);
            return;
        }
        while let Some(frame) = self.frames.last_mut() {
            if let Some((target, name)) = frame.handlers.pop() {
                frame.pc = target;
                frame.locals.insert(name, value);
                return;
            }
            self.frames.pop();
        }
        self.finished = true;
        self.error = Some(value);
    }
    pub fn resume(&mut self, value: Value, failed: bool) -> Result<()> {
        ensure!(
            self.program.version == VERSION,
            "program language version changed; original source and results remain saved"
        );
        let action = self.pending.take().context("no suspended effect")?;
        if failed {
            self.raise(value, &action.at);
        } else if let Some(name) = action.destination {
            self.frames
                .last_mut()
                .context("effect has no caller frame")?
                .locals
                .insert(name, value);
        }
        Ok(())
    }
    pub fn next(&mut self) -> State {
        if self.program.version != VERSION {
            self.pending = None;
            self.finished = true;
            let error = json!({"message":"program language version changed; original source and results remain saved","version":self.program.version,"required_version":VERSION});
            self.error = Some(error.clone());
            return State::Failed(error);
        }
        if let Some(action) = &self.pending {
            return State::Action(action.clone());
        }
        if self.finished {
            return self
                .error
                .as_ref()
                .map(|e| State::Failed(e.clone()))
                .unwrap_or_else(|| State::Returned(self.result.clone()));
        }
        while !self.finished {
            if self.fuel == 0 {
                self.finished = true;
                self.error = Some(json!({"message":"program instruction budget exhausted"}));
                return State::Failed(self.error.clone().unwrap());
            }
            self.fuel -= 1;
            let instruction = {
                let frame = self.frames.last_mut().unwrap();
                let instruction = self.program.functions[&frame.function].code[frame.pc].clone();
                frame.pc += 1;
                instruction
            };
            let at = instruction.at.clone();
            let outcome = (|| -> Result<Option<Action>> {
                match instruction.op {
                    Op::Set(name, e) => {
                        let frame = self.frames.last_mut().unwrap();
                        let value = eval(&e, &frame.locals)?;
                        frame.locals.insert(name, value);
                    }
                    Op::Branch(test, target) => {
                        let frame = self.frames.last_mut().unwrap();
                        if !super::values::boolean(&eval(&test, &frame.locals)?)? {
                            frame.pc = target;
                        }
                    }
                    Op::Jump(target) => self.frames.last_mut().unwrap().pc = target,
                    Op::ForNext {
                        items,
                        index,
                        item,
                        end,
                    } => {
                        let frame = self.frames.last_mut().unwrap();
                        let values = frame
                            .locals
                            .get(&items)
                            .and_then(Value::as_array)
                            .context("for...of requires an array task result")?;
                        let cursor = frame
                            .locals
                            .get(&index)
                            .and_then(Value::as_u64)
                            .context("invalid internal iterator cursor")?
                            as usize;
                        if let Some(value) = values.get(cursor).cloned() {
                            frame.locals.insert(item, value);
                            frame.locals.insert(index, json!(cursor + 1));
                        } else {
                            frame.pc = end;
                        }
                    }

                    Op::Catch { target, name } => self
                        .frames
                        .last_mut()
                        .unwrap()
                        .handlers
                        .push((target, name)),
                    Op::EndCatch => {
                        self.frames.last_mut().unwrap().handlers.pop();
                    }
                    Op::Throw(e) => {
                        let value = eval(&e, &self.frames.last().unwrap().locals)?;
                        self.raise(value, &at);
                    }
                    Op::Append {
                        target,
                        items,
                        destination,
                    } => {
                        let frame = self.frames.last_mut().unwrap();
                        let values = items
                            .iter()
                            .map(|item| eval(item, &frame.locals))
                            .collect::<Result<Vec<_>>>()?;
                        let array = frame
                            .locals
                            .get_mut(&target)
                            .and_then(Value::as_array_mut)
                            .context("result collection requires a local array")?;
                        array.extend(values);
                        let length = array.len();
                        frame.locals.insert(destination, json!(length));
                    }
                    Op::Return(e) => {
                        let value = eval(&e, &self.frames.last().unwrap().locals)?;
                        let frame = self.frames.pop().unwrap();
                        if let Some(parent) = self.frames.last_mut() {
                            if let Some(dest) = frame.destination {
                                parent.locals.insert(dest, value);
                            }
                        } else {
                            self.result = value;
                            self.finished = true;
                        }
                    }
                    Op::Effect(destination, effect) => {
                        let locals = &self.frames.last().unwrap().locals;
                        let values = |args: &Vec<Expr>| {
                            args.iter()
                                .map(|e| eval(e, locals))
                                .collect::<Result<Vec<_>>>()
                        };
                        let kind = match effect {
                            Effect::Call(name, args) => {
                                let args = values(&args)?;
                                let mut scope = self.inherited_scope.clone();
                                scope.extend(self.named_bindings());
                                Self::frame(
                                    &self.program,
                                    &name,
                                    args.clone(),
                                    destination.clone(),
                                    &scope,
                                )?;
                                if self.scoped_calls && self.program.functions[&name].virtual_task {
                                    ActionKind::Call { name, args }
                                } else {
                                    ensure!(self.frames.len() < 64, "program call depth limit");
                                    self.frames.push(Self::frame(
                                        &self.program,
                                        &name,
                                        args,
                                        destination,
                                        &scope,
                                    )?);
                                    return Ok(None);
                                }
                            }
                            Effect::Tool(name, args) => {
                                let mut args = values(&args)?;
                                let args = if args.is_empty() {
                                    json!({})
                                } else {
                                    args.remove(0)
                                };
                                ensure!(args.is_object(), "tool arguments must be an object");
                                ActionKind::Tool { name, args }
                            }
                            Effect::ParallelReads(operations) => {
                                let mut resolved = Vec::with_capacity(operations.len());
                                for (name, args) in operations {
                                    let mut args = values(&args)?;
                                    let args = if args.is_empty() {
                                        json!({})
                                    } else {
                                        args.remove(0)
                                    };
                                    ensure!(args.is_object(), "tool arguments must be an object");
                                    resolved.push((name, args));
                                }
                                ActionKind::ParallelReads {
                                    operations: resolved,
                                }
                            }
                            Effect::ParallelReadMap {
                                name,
                                items,
                                parameter,
                                args,
                            } => {
                                let list = eval(&items, locals)?;
                                let items =
                                    list.as_array().context("Promise.all map needs an array")?;
                                ensure!(items.len() <= 32, "Promise.all supports at most 32 reads");
                                let mut resolved = Vec::with_capacity(items.len());
                                for item in items {
                                    let mut scope = locals.clone();
                                    scope.insert(parameter.clone(), item.clone());
                                    let mut values = args
                                        .iter()
                                        .map(|arg| eval(arg, &scope))
                                        .collect::<Result<Vec<_>>>()?;
                                    let args = if values.is_empty() {
                                        json!({})
                                    } else {
                                        values.remove(0)
                                    };
                                    ensure!(args.is_object(), "tool arguments must be an object");
                                    resolved.push((name.clone(), args));
                                }
                                ActionKind::ParallelReads {
                                    operations: resolved,
                                }
                            }
                            Effect::Fork(entry, args) => ActionKind::Fork {
                                entry,
                                args: values(&args)?,
                            },
                            Effect::Join(args) => {
                                let values = values(&args)?;
                                let mut ids = Vec::new();
                                for value in values {
                                    if let Some(a) = value.as_array() {
                                        for v in a {
                                            ids.push(
                                                v["agent_id"]
                                                    .as_str()
                                                    .context("join needs fork handles")?
                                                    .to_owned(),
                                            );
                                        }
                                    } else {
                                        ids.push(
                                            value["agent_id"]
                                                .as_str()
                                                .context("join needs fork handles")?
                                                .to_owned(),
                                        );
                                    }
                                }
                                ActionKind::Join { ids }
                            }
                            Effect::Task {
                                name,
                                description,
                                input,
                            } => ActionKind::Task {
                                name,
                                description,
                                input: eval(&input, locals)?,
                            },
                        };
                        return Ok(Some(Action {
                            id: format!("program-{}", crate::agent_service::uuid_like()),
                            at: at.clone(),
                            destination,
                            kind,
                        }));
                    }
                }
                Ok(None)
            })();
            match outcome {
                Ok(Some(action)) => {
                    self.pending = Some(action.clone());
                    self.collect_dead_bindings();
                    return State::Action(action);
                }
                Ok(None) => {}
                Err(e) => self.raise(json!({"message":format!("{e:#}")}), &at),
            }
        }
        self.error
            .as_ref()
            .map(|e| State::Failed(e.clone()))
            .unwrap_or_else(|| State::Returned(self.result.clone()))
    }
}
