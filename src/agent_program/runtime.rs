use super::*;
use anyhow::{Context, Result, bail, ensure};
use super::heap::{HeapId, HeapNode, VmValue, JsNumber};
use super::vm_eval::{Locals, eval, is_temp};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    function: String,
    pc: usize,
    locals: BTreeMap<String, Value>,
    /// Compiler temporaries. Source variables live in `env`.
    #[serde(default)]
    vm_locals: Option<BTreeMap<String, VmValue>>,
    /// The frame's current lexical environment.
    #[serde(default)]
    env: Option<HeapId>,
    destination: Option<String>,
    handlers: Vec<(usize, String)>,
    /// The environment each handler restores, parallel to `handlers`.
    #[serde(default)]
    handler_envs: Vec<Option<HeapId>>,
}
impl Frame {
    fn locals(&self) -> Result<Locals<'_>> {
        Ok(Locals {
            temps: self.vm_locals.as_ref().context("missing VM locals")?,
            env: self.env,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runtime {
    pub program: Program,
    #[serde(default)]
    pub scoped_calls: bool,
    frames: Vec<Frame>,
    #[serde(default)]
    inherited_scope: BTreeMap<String, Value>,
    // Persisted separately from the current serial effect while the async
    // interpreter and host integration are being introduced.
    #[serde(default)]
    promises: super::promise::PromiseScheduler,
    #[serde(default)]
    heap: super::heap::Heap,
    /// Top-level declarations. Inherited caller names form its parent scope.
    #[serde(default)]
    module_env: Option<HeapId>,
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

fn vm_args(args: &[Expr], locals: Locals<'_>, heap: &mut super::heap::Heap) -> Result<Vec<VmValue>> {
    args.iter().map(|expr| eval(expr, locals, heap)).collect()
}

/// Positional arguments cross as a JSON array: an undefined argument is null.
fn json_args(args: Vec<VmValue>, heap: &super::heap::Heap) -> Result<Vec<Value>> {
    args.iter()
        .map(|value| match value {
            VmValue::Undefined => Ok(Value::Null),
            value => heap.to_json(value),
        })
        .collect()
}

impl Runtime {
    fn migrate_frames(&mut self) -> Result<()> {
        for frame in &mut self.frames {
            if frame.vm_locals.is_none() {
                let mut values = BTreeMap::new();
                for (name, value) in &frame.locals {
                    values.insert(name.clone(), self.heap.from_json(value)?);
                }
                frame.vm_locals = Some(values);
                frame.locals.clear();
            }
        }
        Ok(())
    }

    /// The variables an AI function sees: every frame of the call chain, each
    /// through its own scope chain, inner bindings shadowing outer ones.
    /// Temporaries and names still in their dead zone are not included.
    pub fn named_bindings(&self) -> BTreeMap<String, Value> {
        let export = |value: &VmValue| {
            self.heap.to_json(value).unwrap_or_else(|error| {
                json!({"$vm_unavailable":format!("value cannot be represented as JSON: {error:#}")})
            })
        };
        let mut bindings = BTreeMap::new();
        for frame in &self.frames {
            if let Some(env) = frame.env {
                let mut chain = Vec::new();
                let mut current = Some(env);
                while let Some(id) = current {
                    if chain.contains(&id) {
                        break;
                    }
                    chain.push(id);
                    current = self.heap.environment_parent(id).ok().flatten();
                }
                for id in chain.iter().rev() {
                    for (name, value) in self.heap.initialized_bindings(*id).unwrap_or_default() {
                        // Undefined and functions are not data, but they still
                        // shadow any outer variable of the same name.
                        if self.heap.json_absent(&value) {
                            bindings.remove(&name);
                        } else {
                            bindings.insert(name, export(&value));
                        }
                    }
                }
            } else if let Some(locals) = &frame.vm_locals {
                bindings.extend(
                    locals
                        .iter()
                        .filter(|(name, _)| !is_temp(name))
                        .map(|(name, value)| (name.clone(), export(value))),
                );
            } else {
                bindings.extend(frame.locals.iter().filter(|(name, _)| !is_temp(name))
                    .map(|(name, value)| (name.clone(), value.clone())));
            }
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
                Expr::Array(items) | Expr::Template(items) | Expr::NodeBuiltin(_, items) | Expr::RegExp(items) => {
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
                Expr::Value(_) | Expr::Undefined | Expr::Function(_) | Expr::Native(_) => {}
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
                    Op::Set(_, e) | Op::Declare(_, e) => expression(e, &mut live),
                    Op::EnterScope(_) | Op::LeaveScope | Op::CopyScope => {}
                    Op::SetMember { object, key, value } => { expression(object, &mut live); expression(key, &mut live); expression(value, &mut live); }
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
                        Effect::Dynamic { callee: target, args }
                        | Effect::Method { receiver: target, args, .. } => {
                            expression(target, &mut live);
                            for arg in args {
                                expression(arg, &mut live);
                            }
                        }
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
            if let Some(locals) = &mut frame.vm_locals {
                locals.retain(|name, _| !name.starts_with("$temp") || live.contains(name) || dynamic_scope);
            }
        }
    }
    pub fn new(program: Program, entry: &str, args: Vec<Value>) -> Result<Self> {
        Self::new_with_scope(program, entry, args, BTreeMap::new())
    }
    /// Inherited caller names become the outermost scope. The top-level flow
    /// runs in the module environment itself; any other entry gets an empty
    /// module environment above its own.
    fn new_with_scope(
        program: Program,
        entry: &str,
        args: Vec<Value>,
        inherited_scope: BTreeMap<String, Value>,
    ) -> Result<Self> {
        ensure!(program.version == VERSION, "unsupported program version");
        let mut heap = super::heap::Heap::default();
        let inherited = if inherited_scope.is_empty() {
            None
        } else {
            let mut bindings = BTreeMap::new();
            for (name, value) in &inherited_scope {
                bindings.insert(name.clone(), heap.from_json(value)?);
            }
            Some(heap.new_environment(None, bindings, BTreeSet::new())?)
        };
        let args = args
            .iter()
            .map(|value| heap.from_json(value))
            .collect::<Result<Vec<_>>>()?;
        let module_entry = program.functions.get(entry).is_some_and(|f| f.module);
        let (frame, module_env) = if module_entry {
            let frame = Self::vm_frame(&program, &mut heap, inherited, entry, args, None)?;
            let env = frame.env.context("entry frame has no scope")?;
            (frame, env)
        } else {
            let module_env = heap.new_environment(inherited, BTreeMap::new(), BTreeSet::new())?;
            let frame = Self::vm_frame(&program, &mut heap, Some(module_env), entry, args, None)?;
            (frame, module_env)
        };
        // Top-level function declarations are hoisted into the module scope.
        // Nested functions have ids containing '/' and are bound where declared.
        for (id, function) in &program.functions {
            if !function.module && !id.contains('/') {
                let closure = heap.allocate(HeapNode::Closure {
                    function: id.clone(),
                    environment: module_env,
                })?;
                heap.define_binding(module_env, id, VmValue::Reference(closure))?;
            }
        }
        Ok(Self {
            program,
            scoped_calls: false,
            frames: vec![frame],
            inherited_scope,
            promises: Default::default(),
            heap,
            module_env: Some(module_env),
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
    /// A frame for `name` with its own environment below `parent`: parameters,
    /// hoisted vars (undefined) and top-level let/const (in their dead zone).
    fn vm_frame(
        program: &Program,
        heap: &mut super::heap::Heap,
        parent: Option<HeapId>,
        name: &str,
        args: Vec<VmValue>,
        destination: Option<String>,
    ) -> Result<Frame> {
        let function = program
            .functions
            .get(name)
            .with_context(|| format!("unknown function {name}"))?;
        // As in JavaScript: missing arguments are undefined, extra ones ignored.
        let mut args = args;
        args.resize(function.params.len(), VmValue::Undefined);
        let mut bindings: BTreeMap<String, VmValue> = function
            .vars
            .iter()
            .map(|name| (name.clone(), VmValue::Undefined))
            .collect();
        bindings.extend(function.params.iter().cloned().zip(args));
        let env = heap.new_environment(parent, bindings, function.lexical.iter().cloned().collect())?;
        Ok(Frame {
            function: name.into(),
            pc: 0,
            locals: BTreeMap::new(),
            vm_locals: Some(BTreeMap::new()),
            env: Some(env),
            destination,
            handlers: Vec::new(),
            handler_envs: Vec::new(),
        })
    }
    /// Write a call result into the caller's temporary.
    fn store(&mut self, destination: Option<String>, value: VmValue) -> Result<()> {
        if let Some(name) = destination {
            self.frames
                .last_mut()
                .context("call has no caller frame")?
                .vm_locals
                .as_mut()
                .context("missing VM locals")?
                .insert(name, value);
        }
        Ok(())
    }

    /// Call a function value. A closure runs in a new frame over the scope it
    /// was created in and a built-in runs here; AI functions and tools become
    /// actions for the host.
    fn invoke(
        &mut self,
        callee: VmValue,
        args: Vec<VmValue>,
        destination: Option<String>,
    ) -> Result<Option<ActionKind>> {
        let node = match &callee {
            VmValue::Reference(id) => self.heap.get(*id)?.clone(),
            other => bail!(
                "TypeError: {} is not a function",
                super::js_value::to_string(&self.heap, other)?
            ),
        };
        match node {
            HeapNode::Closure { function, environment } => {
                let f = self
                    .program
                    .functions
                    .get(&function)
                    .with_context(|| format!("unknown function {function}"))?;
                if self.scoped_calls && f.virtual_task {
                    ensure!(
                        args.len() == f.params.len(),
                        "{function}: expected {} arguments, got {}",
                        f.params.len(),
                        args.len()
                    );
                    return Ok(Some(ActionKind::Call {
                        name: function,
                        args: json_args(args, &self.heap)?,
                    }));
                }
                // A named function expression refers to itself unless one of
                // its own names shadows it.
                let self_name = f.self_name.clone().filter(|name| {
                    !f.params.contains(name) && !f.vars.contains(name) && !f.lexical.contains(name)
                });
                ensure!(self.frames.len() < 64, "program call depth limit");
                let frame = Self::vm_frame(
                    &self.program,
                    &mut self.heap,
                    Some(environment),
                    &function,
                    args,
                    destination,
                )?;
                if let (Some(name), Some(env)) = (self_name, frame.env) {
                    self.heap.define_binding(env, &name, callee)?;
                }
                self.frames.push(frame);
                Ok(None)
            }
            HeapNode::Native(name) => {
                if let Some(tool) = name.strip_prefix("tool:") {
                    let mut args = json_args(args, &self.heap)?;
                    let args = if args.is_empty() { json!({}) } else { args.remove(0) };
                    ensure!(args.is_object(), "tool arguments must be an object");
                    return Ok(Some(ActionKind::Tool {
                        name: tool.to_owned(),
                        args,
                    }));
                }
                let value = super::js_methods::builtin(&mut self.heap, &name, &args)?;
                self.store(destination, value)?;
                Ok(None)
            }
            _ => bail!(
                "TypeError: {} is not a function",
                super::js_value::to_string(&self.heap, &callee)?
            ),
        }
    }

    fn stack(&self) -> Value {
        json!(self
            .frames
            .iter()
            .rev()
            .map(|f| json!({"function":f.function,"pc":f.pc.saturating_sub(1)}))
            .collect::<Vec<_>>())
    }
    /// A failure reported by a tool, AI task or the host becomes an Error
    /// object. Its original fields stay available, with a stable message and
    /// the program location.
    fn host_error(&mut self, mut value: Value, at: &Location) -> VmValue {
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
        if !value["message"].is_string() {
            value["message"] = json!(value["message"].to_string());
        }
        if value.get("location").is_none() {
            value["location"] = json!(at);
        }
        if value.get("stack").is_none() {
            value["stack"] = self.stack();
        }
        let class = value["name"]
            .as_str()
            .filter(|name| super::vm_eval::ERROR_CLASSES.contains(name))
            .unwrap_or("Error")
            .to_owned();
        if value.get("name").is_none() {
            value["name"] = json!(class);
        }
        match self.heap.from_json(&value) {
            Ok(VmValue::Reference(id)) => {
                self.heap.set_class(id, &class);
                VmValue::Reference(id)
            }
            _ => {
                let message = VmValue::String(value["message"].as_str().unwrap_or("error").to_owned());
                super::js_methods::new_error(&mut self.heap, &class, &message, &VmValue::Undefined)
                    .unwrap_or(message)
            }
        }
    }
    /// A runtime failure such as "TypeError: ..." becomes an Error of that class.
    fn runtime_error(&mut self, message: String, at: &Location) -> VmValue {
        let (class, text) = message
            .split_once(": ")
            .filter(|(class, _)| super::vm_eval::ERROR_CLASSES.contains(class))
            .map(|(class, text)| (class.to_owned(), text.to_owned()))
            .unwrap_or_else(|| ("Error".to_owned(), message.clone()));
        self.host_error(json!({"name":class,"message":text}), at)
    }
    /// The JSON report of an uncaught throw, for the host and the AI.
    fn error_report(&self, value: &VmValue, at: &Location) -> Value {
        let message = super::js_value::to_string(&self.heap, value).unwrap_or_else(|_| "uncaught exception".into());
        let mut report = match self.heap.to_json(value) {
            Ok(Value::Object(object)) => Value::Object(object),
            Ok(other) => json!({"message":message.clone(),"thrown":other}),
            Err(_) => json!({"message":message.clone()}),
        };
        if !report["message"].is_string() {
            report["message"] = json!(message);
        }
        if report.get("location").is_none() {
            report["location"] = json!(at);
        }
        if report.get("stack").is_none() {
            report["stack"] = self.stack();
        }
        report
    }
    /// Unwind to the nearest catch handler with the thrown value itself.
    fn throw(&mut self, value: VmValue, at: &Location) {
        // Keep the failed frames intact when nobody can catch the error. A
        // scoped AI continuation can inspect their actual locals without
        // replaying the instructions or their external effects.
        if !self.frames.iter().any(|frame| !frame.handlers.is_empty()) {
            self.finished = true;
            self.error = Some(self.error_report(&value, at));
            return;
        }
        while let Some(frame) = self.frames.last_mut() {
            if let Some((target, name)) = frame.handlers.pop() {
                frame.pc = target;
                // Leave the block scopes opened inside the try block.
                if let Some(env) = frame.handler_envs.pop() {
                    frame.env = env;
                }
                frame.vm_locals.as_mut().expect("VM frame is initialized").insert(name, value);
                return;
            }
            self.frames.pop();
        }
        self.finished = true;
        self.error = Some(self.error_report(&value, at));
    }
    fn raise(&mut self, value: Value, at: &Location) {
        let error = self.host_error(value, at);
        self.throw(error, at);
    }
    pub fn resume(&mut self, value: Value, failed: bool) -> Result<()> {
        ensure!(
            self.program.version == VERSION,
            "program language version changed; original source and results remain saved"
        );
        self.migrate_frames()?;
        let imported = if !failed && self.pending.as_ref().and_then(|action| action.destination.as_ref()).is_some() {
            Some(self.heap.from_json(&value)?)
        } else { None };
        let action = self.pending.take().context("no suspended effect")?;
        if failed {
            self.raise(value, &action.at);
        } else if let Some(name) = action.destination {
            let value = imported.context("missing imported effect result")?;
            self.frames.last_mut().context("effect has no caller frame")?
                .vm_locals.as_mut().context("missing VM locals")?.insert(name, value);
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
        if let Err(error) = self.migrate_frames() {
            self.finished = true;
            let error = json!({"message":format!("failed to restore VM values: {error:#}")});
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
            if self.fuel % 256 == 0 {
                let mut roots = self.promises.roots();
                for frame in &self.frames {
                    if let Some(locals) = &frame.vm_locals { roots.extend(locals.values().cloned()); }
                    roots.extend(
                        frame
                            .env
                            .iter()
                            .chain(frame.handler_envs.iter().flatten())
                            .map(|id| VmValue::Reference(*id)),
                    );
                }
                roots.extend(self.module_env.map(VmValue::Reference));
                if let Err(error) = self.heap.collect(&roots) {
                    self.finished = true;
                    let error = json!({"message":format!("VM heap collection failed: {error:#}")});
                    self.error = Some(error.clone());
                    return State::Failed(error);
                }
            }
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
                        let frame = self.frames.last().unwrap();
                        let value = eval(&e, frame.locals()?, &mut self.heap)?;
                        if is_temp(&name) {
                            self.frames.last_mut().unwrap().vm_locals.as_mut().unwrap().insert(name, value);
                        } else {
                            let env = self.frames.last().unwrap().env.context("missing scope")?;
                            self.heap.assign_binding(env, &name, value)?;
                        }
                    }
                    Op::Declare(name, e) => {
                        let frame = self.frames.last().unwrap();
                        let env = frame.env.context("missing scope")?;
                        let value = eval(&e, frame.locals()?, &mut self.heap)?;
                        self.heap.initialize_binding(env, &name, value)?;
                    }
                    Op::EnterScope(names) => {
                        let frame = self.frames.last_mut().unwrap();
                        frame.env = Some(self.heap.new_environment(
                            frame.env,
                            BTreeMap::new(),
                            names.into_iter().collect(),
                        )?);
                    }
                    Op::LeaveScope => {
                        let frame = self.frames.last_mut().unwrap();
                        let env = frame.env.context("missing scope")?;
                        frame.env = self.heap.environment_parent(env)?;
                    }
                    Op::CopyScope => {
                        let frame = self.frames.last_mut().unwrap();
                        let env = frame.env.context("missing scope")?;
                        frame.env = Some(self.heap.copy_environment(env)?);
                    }
                    Op::SetMember { object, key, value } => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let object = eval(&object, locals, &mut self.heap)?;
                        let key = eval(&key, locals, &mut self.heap)?;
                        let value = eval(&value, locals, &mut self.heap)?;
                        let VmValue::Reference(id) = object else { bail!("member assignment requires an object") };
                        let key = super::js_value::to_string(&self.heap, &key)?;
                        self.heap.set_property(id, key, value)?;
                    }
                    Op::Branch(test, target) => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let value = eval(&test, locals, &mut self.heap)?;
                        if !super::vm_eval::truthy(&value) {
                            self.frames.last_mut().unwrap().pc = target;
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
                        let locals = frame.vm_locals.as_mut().context("missing VM locals")?;
                        let VmValue::Reference(id) = locals.get(&items).context("missing array")? else { bail!("for...of requires an array") };
                        let HeapNode::Array(values) = self.heap.get(*id)? else { bail!("for...of requires an array") };
                        let VmValue::Number(cursor) = locals.get(&index).context("invalid internal iterator cursor")? else { bail!("invalid internal iterator cursor") };
                        let cursor = cursor.get() as usize;
                        if let Some(value) = values.get(cursor).cloned() {
                            locals.insert(item, value);
                            locals.insert(index, VmValue::Number(JsNumber::new((cursor + 1) as f64)));
                        } else {
                            frame.pc = end;
                        }
                    }

                    Op::Catch { target, name } => {
                        let frame = self.frames.last_mut().unwrap();
                        frame.handlers.push((target, name));
                        frame.handler_envs.push(frame.env);
                    }
                    Op::EndCatch => {
                        let frame = self.frames.last_mut().unwrap();
                        frame.handlers.pop();
                        frame.handler_envs.pop();
                    }
                    Op::Throw(e) => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let value = eval(&e, locals, &mut self.heap)?;
                        self.throw(value, &at);
                    }
                    Op::Append {
                        target,
                        items,
                        destination,
                    } => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let values = items
                            .iter()
                            .map(|item| eval(item, locals, &mut self.heap))
                            .collect::<Result<Vec<_>>>()?;
                        let array = eval(&Expr::Variable(target), locals, &mut self.heap)?;
                        let VmValue::Reference(array) = array else { bail!("result collection requires an array") };
                        let length = self.heap.append(array, values)?;
                        self.frames.last_mut().unwrap().vm_locals.as_mut().unwrap().insert(destination, VmValue::Number(JsNumber::new(length as f64)));
                    }
                    Op::Return(e) => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let value = eval(&e, locals, &mut self.heap)?;
                        let frame = self.frames.pop().unwrap();
                        if let Some(parent) = self.frames.last_mut() {
                            if let Some(dest) = frame.destination {
                                parent.vm_locals.as_mut().unwrap().insert(dest, value);
                            }
                        } else {
                            self.result = if value == VmValue::Undefined { Value::Null } else { self.heap.to_json(&value)? };
                            self.finished = true;
                        }
                    }
                    Op::Effect(destination, effect) => {
                        let frame = self.frames.last().unwrap();
                        let temps = frame.vm_locals.clone().context("missing VM locals")?;
                        let env = frame.env;
                        let locals = Locals { temps: &temps, env };
                        let kind = match effect {
                            Effect::Call(name, args) => {
                                let args = vm_args(&args, locals, &mut self.heap)?;
                                let function = self
                                    .program
                                    .functions
                                    .get(&name)
                                    .with_context(|| format!("unknown function {name}"))?;
                                if self.scoped_calls && function.virtual_task {
                                    // AI functions take exactly their declared parameters.
                                    ensure!(
                                        args.len() == function.params.len(),
                                        "{name}: expected {} arguments, got {}",
                                        function.params.len(),
                                        args.len()
                                    );
                                    ActionKind::Call { name, args: json_args(args, &self.heap)? }
                                } else {
                                    // Functions see the module scope, not their caller's variables.
                                    ensure!(self.frames.len() < 64, "program call depth limit");
                                    let frame = Self::vm_frame(
                                        &self.program,
                                        &mut self.heap,
                                        self.module_env,
                                        &name,
                                        args,
                                        destination,
                                    )?;
                                    self.frames.push(frame);
                                    return Ok(None);
                                }
                            }
                            Effect::Dynamic { callee, args } => {
                                let callee = eval(&callee, locals, &mut self.heap)?;
                                let args = vm_args(&args, locals, &mut self.heap)?;
                                match self.invoke(callee, args, destination.clone())? {
                                    Some(kind) => kind,
                                    None => return Ok(None),
                                }
                            }
                            Effect::Method { receiver, name, args } => {
                                let receiver = eval(&receiver, locals, &mut self.heap)?;
                                let args = vm_args(&args, locals, &mut self.heap)?;
                                // A function-valued own property is called; anything
                                // else is a built-in method of the receiver.
                                let property = match &receiver {
                                    VmValue::Reference(id) if self.heap.regex_parts(*id).is_none() => {
                                        self.heap.own_property(*id, &name)?
                                    }
                                    _ => None,
                                };
                                match property {
                                    Some(value) if super::vm_eval::callable(&self.heap, &value) => {
                                        match self.invoke(value, args, destination.clone())? {
                                            Some(kind) => kind,
                                            None => return Ok(None),
                                        }
                                    }
                                    Some(_) => bail!("TypeError: {name} is not a function"),
                                    None => {
                                        let value = super::js_methods::call_method(&mut self.heap, receiver, &name, &args)?;
                                        self.store(destination, value)?;
                                        return Ok(None);
                                    }
                                }
                            }
                            Effect::Tool(name, args) => {
                                let values = vm_args(&args, locals, &mut self.heap)?;
                                let mut args = json_args(values, &self.heap)?;
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
                                    let values = vm_args(&args, locals, &mut self.heap)?;
                                    let mut args = json_args(values, &self.heap)?;
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
                                let list = eval(&items, locals, &mut self.heap)?;
                                let VmValue::Reference(list) = list else { bail!("Promise.all map needs an array") };
                                let HeapNode::Array(items) = self.heap.get(list)? else { bail!("Promise.all map needs an array") };
                                let items = items.clone();
                                ensure!(items.len() <= 32, "Promise.all supports at most 32 reads");
                                let mut resolved = Vec::with_capacity(items.len());
                                for item in &items {
                                    // The map parameter is bound for this item only.
                                    let scope = self.heap.new_environment(
                                        env,
                                        [(parameter.clone(), item.clone())].into(),
                                        BTreeSet::new(),
                                    )?;
                                    let values = vm_args(&args, Locals { temps: &temps, env: Some(scope) }, &mut self.heap)?;
                                    let mut values = json_args(values, &self.heap)?;
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
                                args: json_args(vm_args(&args, locals, &mut self.heap)?, &self.heap)?,
                            },
                            Effect::Join(args) => {
                                let values = vm_args(&args, locals, &mut self.heap)?;
                                let values = json_args(values, &self.heap)?;
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
                            } => {
                                let value = eval(&input, locals, &mut self.heap)?;
                                ActionKind::Task { name, description, input: self.heap.to_json(&value)? }
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
                Err(e) => {
                    let error = self.runtime_error(format!("{e:#}"), &at);
                    self.throw(error, &at);
                }
            }
        }
        self.error
            .as_ref()
            .map(|e| State::Failed(e.clone()))
            .unwrap_or_else(|| State::Returned(self.result.clone()))
    }
}
