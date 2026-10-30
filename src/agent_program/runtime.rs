use super::*;
use anyhow::{Context, Result, bail, ensure};
use super::heap::{HeapId, HeapNode, VmValue, JsNumber};
use super::vm_eval::{Locals, eval, is_temp};
use std::collections::{BTreeSet, VecDeque};

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
    #[serde(default)]
    completion: Option<HeapId>,
    #[serde(default)]
    executor: Option<HeapId>,
    #[serde(default)]
    origin: Vec<HeapId>,
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
struct SuspendedCoroutine {
    frames: Vec<Frame>,
    destination: String,
    at: Location,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PromiseReaction {
    fulfilled: VmValue,
    rejected: VmValue,
    target: HeapId,
    origin: Vec<HeapId>,
    #[serde(default)]
    scope: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingAsyncAction {
    action: Action,
    promise: HeapId,
    scope: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runtime {
    pub program: Program,
    // Retained for old snapshots; virtual calls always use scoped dispatch.
    #[serde(default)]
    pub scoped_calls: bool,
    frames: Vec<Frame>,
    #[serde(default)]
    inherited_scope: BTreeMap<String, Value>,
    // Persist Promise state alongside the serial effect and asynchronous queue.
    #[serde(default)]
    promises: super::promise::PromiseScheduler,
    #[serde(default)]
    continuations: BTreeMap<super::promise::ContinuationId, SuspendedCoroutine>,
    #[serde(default)]
    next_continuation: u64,
    #[serde(default)]
    forwarding: BTreeMap<super::promise::ContinuationId, Option<HeapId>>,
    /// Queued adoptions of a Promise, as JavaScript's
    /// NewPromiseResolveThenableJob; the job's value is the adopted Promise.
    #[serde(default)]
    adopting: BTreeMap<super::promise::ContinuationId, Option<HeapId>>,
    #[serde(default)]
    reactions:BTreeMap<super::promise::ContinuationId, PromiseReaction>,
    #[serde(default)]
    async_promises: Vec<HeapId>,
    #[serde(default)]
    root_result: Option<VmValue>,
    /// Where a value was thrown out of an async function, keyed by the
    /// rejected Promise and by the thrown object, for unhandled reports.
    #[serde(default)]
    throw_sites: BTreeMap<HeapId, Value>,
    #[serde(default)]
    heap: super::heap::Heap,
    /// Top-level declarations. Inherited caller names form its parent scope.
    #[serde(default)]
    module_env: Option<HeapId>,
    pub pending: Option<Action>,
    #[serde(default)]
    async_actions: VecDeque<PendingAsyncAction>,
    #[serde(default)]
    active_async_action: Option<PendingAsyncAction>,
    #[serde(default)]
    active_reaction: Option<PendingAsyncAction>,
    #[serde(default)]
    active_async_batch: Vec<PendingAsyncAction>,
    #[serde(default)]
    inflight_async: BTreeMap<String, PendingAsyncAction>,
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
        /// The `@uses` variables of a virtual function, read at the call.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        captured: BTreeMap<String, Value>,
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
    Waiting,
    Returned(Value),
    Failed(Value),
}

#[derive(Clone, Debug)]
pub enum ConcurrentState {
    Actions(Vec<Action>),
    Waiting,
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
        if let Some(action) = self.active_async_action.as_ref().or(self.active_reaction.as_ref()) { return action.scope.clone(); }
        let export = |value: &VmValue| {
            self.heap.to_json(value).unwrap_or_else(|error| {
                json!({"$vm_unavailable":format!("value cannot be represented as JSON: {error:#}")})
            })
        };
        let mut bindings = BTreeMap::new();
        for frame in &self.frames {
            if frame.function.starts_with("$$") { continue; }
            for env in frame.origin.iter().copied().chain(frame.env) {
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
                        if name.starts_with("$$") { continue; }
                        // Undefined and functions are not data, but they still
                        // shadow any outer variable of the same name.
                        if self.heap.json_absent(&value) {
                            bindings.remove(&name);
                        } else {
                            bindings.insert(name, export(&value));
                        }
                    }
                }
            }
            if frame.env.is_none() && let Some(locals) = &frame.vm_locals {
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
                    Op::EnterScope(_) | Op::EnterFunctionBody | Op::LeaveScope | Op::CopyScope => {}
                    Op::SetMember { object, key, value } => { expression(object, &mut live); expression(key, &mut live); expression(value, &mut live); }
                    Op::Await { value, .. } => expression(value, &mut live),
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
                        Effect::PromiseCreate { value, .. } => expression(value, &mut live),
                        Effect::PromiseConstruct { executor } => expression(executor, &mut live),
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
                        Effect::Apply { callee, args } => { expression(callee, &mut live); expression(args, &mut live); },
                        Effect::MethodApply { receiver, args, .. } => { expression(receiver, &mut live); expression(args, &mut live); },
                        Effect::DynamicMethodApply { receiver, name, args } => { expression(receiver, &mut live); expression(name, &mut live); expression(args, &mut live); },
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
        ensure!(program.prelude_hash == super::prelude::hash(), "program prelude changed");
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
            scoped_calls: true,
            frames: vec![frame],
            inherited_scope,
            promises: Default::default(),
            continuations: BTreeMap::new(),
            next_continuation: 0,
            forwarding: BTreeMap::new(),
            adopting: BTreeMap::new(),
            reactions: BTreeMap::new(),
            async_promises: Vec::new(),
            root_result: None,
            throw_sites: BTreeMap::new(),
            heap,
            module_env: Some(module_env),
            pending: None,
            async_actions: VecDeque::new(),
            active_async_action: None,
            active_reaction: None,
            active_async_batch: Vec::new(),
            inflight_async: BTreeMap::new(),
            finished: false,
            result: Value::Null,
            error: None,
            fuel: 1_000_000,
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
        if let Some(index) = function.rest_parameter {
            ensure!(index + 1 == function.params.len(), "invalid rest parameter index");
            if args.len() < index { args.resize(index, VmValue::Undefined); }
            let rest = args.split_off(index);
            let rest = VmValue::Reference(heap.allocate(HeapNode::Array(rest))?);
            args.push(rest);
        }
        args.resize(function.params.len(), VmValue::Undefined);
        let mut bindings: BTreeMap<String, VmValue> = if function.parameter_scope {
            BTreeMap::new()
        } else {
            function.vars.iter().map(|name| (name.clone(), VmValue::Undefined)).collect()
        };
        bindings.extend(function.params.iter().cloned().zip(args));
        let lexical = if function.parameter_scope { &function.parameter_lexical } else { &function.lexical };
        let env = heap.new_environment(parent, bindings, lexical.iter().cloned().collect())?;
        Ok(Frame {
            function: name.into(),
            pc: 0,
            locals: BTreeMap::new(),
            vm_locals: Some(BTreeMap::new()),
            env: Some(env),
            destination,
            handlers: Vec::new(),
            handler_envs: Vec::new(),
            completion: None,
            executor: None,
            origin: Vec::new(),
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

    fn invoke_method(&mut self, receiver: VmValue, name: &str, args: Vec<VmValue>, destination: Option<String>, ignore_property: bool) -> Result<Option<ActionKind>> {
        let property = if ignore_property { None } else { match &receiver {
            VmValue::Reference(id) if self.heap.regex_parts(*id).is_none() => self.heap.own_property(*id, name)?,
            _ => None,
        }};
        match property {
            Some(value) if super::vm_eval::callable(&self.heap, &value) => return self.invoke(value, args, destination),
            Some(_) => bail!("TypeError: {name} is not a function"),
            None => {}
        }
        if let VmValue::Reference(source) = receiver
            && matches!(self.heap.get(source)?, HeapNode::Promise { .. })
            && matches!(name, "then" | "catch")
        {
            let target = self.promises.create_heap_promise(&mut self.heap)?;
            self.async_promises.push(target);
            let (fulfilled, rejected) = if name == "catch" {
                (VmValue::Undefined, args.first().cloned().unwrap_or(VmValue::Undefined))
            } else {
                (args.first().cloned().unwrap_or(VmValue::Undefined), args.get(1).cloned().unwrap_or(VmValue::Undefined))
            };
            let id = super::promise::ContinuationId(self.next_continuation);
            self.next_continuation = self.next_continuation.checked_add(1).context("continuation id exhausted")?;
            self.reactions.insert(id, PromiseReaction { fulfilled, rejected, target, origin: self.async_origin(), scope: self.named_bindings() });
            self.promises.subscribe_heap_promise(&mut self.heap, source, id)?;
            self.store(destination, VmValue::Reference(target))?;
            return Ok(None);
        }
        if let VmValue::Reference(source) = receiver
            && matches!(self.heap.get(source)?, HeapNode::Promise { .. }) && name == "finally"
        {
            let callback = args.first().cloned().unwrap_or(VmValue::Undefined);
            let frame = Self::vm_frame(&self.program, &mut self.heap, self.module_env, "$$promiseFinally", vec![VmValue::Reference(source), callback], destination)?;
            self.frames.push(frame);
            return Ok(None);
        }
        let callback_method = matches!(name, "map" | "filter" | "forEach" | "some" | "every" | "find" | "findIndex" | "findLast" | "findLastIndex" | "flatMap" | "reduce" | "reduceRight") || matches!(name, "sort" | "toSorted") && !args.is_empty();
        let is_array = matches!(&receiver, VmValue::Reference(id) if matches!(self.heap.get(*id), Ok(HeapNode::Array(_))));
        if callback_method && is_array {
            ensure!(!args.is_empty() && super::vm_eval::callable(&self.heap, &args[0]), "TypeError: callback is not a function");
            if name != "map" { self.ensure_sync_callback(&args[0])?; }
            let mut call_args = vec![receiver];
            call_args.extend(args.iter().cloned());
            if matches!(name, "reduce" | "reduceRight") {
                let supplied = args.len() > 1;
                if !supplied { call_args.push(VmValue::Undefined); }
                call_args.push(VmValue::Boolean(supplied));
            }
            ensure!(self.frames.len() < 64, "program call depth limit");
            let frame = Self::vm_frame(&self.program, &mut self.heap, self.module_env, &format!("$${name}"), call_args, destination)?;
            self.frames.push(frame);
            return Ok(None);
        }
        if matches!(&receiver, VmValue::String(_)) && matches!(name, "replace" | "replaceAll") && args.get(1).is_some_and(|v| super::vm_eval::callable(&self.heap, v)) {
            self.ensure_sync_callback(&args[1])?;
            ensure!(self.frames.len() < 64, "program call depth limit");
            let mut call_args = vec![receiver];
            call_args.extend(args);
            call_args.push(VmValue::Boolean(name == "replaceAll"));
            let frame = Self::vm_frame(&self.program, &mut self.heap, self.module_env, "$$replace", call_args, destination)?;
            self.frames.push(frame);
            return Ok(None);
        }
        let value = super::js_methods::call_method(&mut self.heap, receiver, name, &args)?;
        self.store(destination, value)?;
        Ok(None)
    }
    fn ensure_sync_callback(&self, callback: &VmValue) -> Result<()> {
        if let VmValue::Reference(id) = callback {
            if let HeapNode::Closure { function, .. } = self.heap.get(*id)? {
                ensure!(!self.program.functions[function].is_async, "async callbacks require Promise support");
            }
        }
        Ok(())
    }

    /// A virtual function's `@uses` variables, read now in the scope it closes
    /// over, as a JavaScript closure reads its free variables when it runs.
    fn captured(&self, function: &str, environment: Option<HeapId>) -> Result<BTreeMap<String, Value>> {
        let uses = &self.program.functions[function].uses;
        if uses.is_empty() {
            return Ok(BTreeMap::new());
        }
        let environment = environment.context("the function's scope is missing")?;
        uses.iter()
            .map(|name| {
                let value = match self.heap.lookup_binding(environment, name)? {
                    VmValue::Undefined => Value::Null,
                    value => self.heap.to_json(&value)?,
                };
                Ok((name.clone(), value))
            })
            .collect()
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
                if f.virtual_task {
                    ensure!(
                        args.len() == f.params.len(),
                        "{function}: expected {} arguments, got {}",
                        f.params.len(),
                        args.len()
                    );
                    let captured = self.captured(&function, Some(environment))?;
                    return Ok(Some(ActionKind::Call {
                        name: function,
                        args: json_args(args, &self.heap)?,
                        captured,
                    }));
                }
                // A named function expression refers to itself unless one of
                // its own names shadows it.
                let self_name = f.self_name.clone().filter(|name| {
                    !f.params.contains(name) && !f.parameter_lexical.contains(name)
                        && (f.parameter_scope || (!f.vars.contains(name) && !f.lexical.contains(name)))
                });
                ensure!(self.frames.len() < 64, "program call depth limit");
                let is_async = f.is_async;
                let mut frame = Self::vm_frame(
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
                if is_async {
                    let promise = self.promises.create_heap_promise(&mut self.heap)?;
                    self.async_promises.push(promise);
                    self.store(frame.destination.take(), VmValue::Reference(promise))?;
                    frame.completion = Some(promise);
                    frame.origin = self.async_origin();
                }
                self.frames.push(frame);
                Ok(None)
            }
            HeapNode::PromiseResolver { capability, reject } => {
                self.call_resolver(capability, reject, args.into_iter().next().unwrap_or(VmValue::Undefined))?;
                self.store(destination, VmValue::Undefined)?;
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
    fn async_origin(&self) -> Vec<HeapId> {
        let mut origin = Vec::new();
        for frame in &self.frames {
            if frame.function.starts_with("$$") { continue; }
            if !frame.origin.is_empty() { origin.clone_from(&frame.origin); }
            origin.extend(frame.env);
        }
        origin
    }

    /// The resolve/reject pair shares a single durable already-resolved flag.
    fn call_resolver(&mut self, capability: HeapId, reject: bool, value: VmValue) -> Result<()> {
        let HeapNode::PromiseCapability { promise, resolved } = self.heap.get_mut(capability)? else {
            bail!("invalid Promise capability");
        };
        if *resolved { return Ok(()); }
        *resolved = true;
        let promise = *promise;
        if reject {
            self.promises.settle_heap_promise(&mut self.heap, promise, super::promise::Outcome::Rejected(value))?;
        } else {
            self.resolve_async(Some(promise), value)?;
        }
        Ok(())
    }

    /// Resolve an async return, adopting another Promise before completing it.
    fn resolve_async(&mut self, target: Option<HeapId>, value: VmValue) -> Result<()> {
        if let VmValue::Reference(source) = value {
            if matches!(self.heap.get(source)?, HeapNode::Promise { .. }) {
                if target == Some(source) {
                    let error = self.runtime_error("TypeError: a promise cannot resolve to itself".into(), &Location { line: 0, column: 0 });
                    self.promises.settle_heap_promise(&mut self.heap, source, super::promise::Outcome::Rejected(error))?;
                    return Ok(());
                }
                // As in JavaScript, a job subscribes to the adopted Promise
                // later, so adoption takes two more turns than a plain value.
                let id = super::promise::ContinuationId(self.next_continuation);
                self.next_continuation = self.next_continuation.checked_add(1).context("continuation id exhausted")?;
                self.adopting.insert(id, target);
                self.promises.enqueue(id, super::promise::Outcome::Fulfilled(VmValue::Reference(source)));
                return Ok(());
            }
        }
        if let Some(target) = target {
            self.promises.settle_heap_promise(&mut self.heap, target, super::promise::Outcome::Fulfilled(value))?;
        } else {
            self.root_result = Some(value);
        }
        Ok(())
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
        let boundary = self.frames.iter().rposition(|frame| frame.completion.is_some() || frame.executor.is_some()).unwrap_or(0);
        if !self.frames[boundary..].iter().any(|frame| !frame.handlers.is_empty()) {
            let target = self.frames.get(boundary).and_then(|frame| match (frame.executor, frame.completion) {
                (Some(capability), _) => match self.heap.get(capability) {
                    Ok(HeapNode::PromiseCapability { promise, .. }) => Some(*promise),
                    _ => None,
                },
                (None, completion) => completion,
            });
            if let Some(promise) = target {
                // The frames are dropped here, so remember the site now.
                let site = json!({"location":at,"stack":self.stack()});
                let thrown = match value { VmValue::Reference(id) => Some(id), _ => None };
                for id in [Some(promise), thrown].into_iter().flatten() {
                    self.throw_sites.entry(id).or_insert_with(|| site.clone());
                }
            }
            if let Some(capability) = self.frames.get(boundary).and_then(|frame| frame.executor) {
                self.frames.truncate(boundary);
                if let Err(error) = self.call_resolver(capability, true, value) {
                    self.finished = true;
                    self.error = Some(json!({"message":format!("{error:#}")}));
                }
                return;
            }
            if let Some(promise) = self.frames.get(boundary).and_then(|frame| frame.completion) {
                self.frames.truncate(boundary);
                if let Err(error) = self.promises.settle_heap_promise(&mut self.heap, promise, super::promise::Outcome::Rejected(value)) {
                    self.finished = true;
                    self.error = Some(json!({"message": format!("{error:#}")}));
                }
                return;
            }
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
    /// Dispatch virtual calls already issued by JS while preserving serial effects.
    pub fn next_concurrent(&mut self) -> ConcurrentState {
        let state = self.next();
        if let State::Action(action) = state {
            let mut actions = Vec::new();
            if self.active_async_action.as_ref().is_some_and(|pending| matches!(pending.action.kind, ActionKind::Call { .. })) {
                self.pending = None;
                let pending = self.active_async_action.take().expect("active async action");
                actions.push(pending.action.clone());
                self.inflight_async.insert(pending.action.id.clone(), pending);
            }
            let mut retained = VecDeque::new();
            while let Some(pending) = self.async_actions.pop_front() {
                if matches!(pending.action.kind, ActionKind::Call { .. }) {
                    actions.push(pending.action.clone());
                    self.inflight_async.insert(pending.action.id.clone(), pending);
                } else { retained.push_back(pending); }
            }
            self.async_actions = retained;
            return if actions.is_empty() { ConcurrentState::Actions(vec![action]) } else { ConcurrentState::Actions(actions) };
        }
        match state {
            State::Waiting => ConcurrentState::Waiting,
            State::Returned(value) => ConcurrentState::Returned(value),
            State::Failed(error) => ConcurrentState::Failed(error),
            State::Action(_) => unreachable!(),
        }
    }
    pub fn async_scope(&self, id: &str) -> Option<&BTreeMap<String, Value>> {
        self.inflight_async.get(id).map(|pending| &pending.scope)
    }
    pub fn inflight_actions(&self) -> Vec<Action> {
        self.inflight_async.values().map(|pending| pending.action.clone()).collect()
    }
    pub fn resume_action(&mut self, id: &str, value: Value, failed: bool) -> Result<()> {
        if let Some(pending) = self.inflight_async.get(id).cloned() {
            let outcome = if failed { super::promise::Outcome::Rejected(self.host_error(value, &pending.action.at)) }
                else { super::promise::Outcome::Fulfilled(self.heap.from_json(&value)?) };
            self.promises.settle_heap_promise(&mut self.heap, pending.promise, outcome)?;
            self.inflight_async.remove(id);
            return Ok(());
        }
        ensure!(self.pending.as_ref().is_some_and(|action| action.id == id), "unknown or already completed action");
        self.resume(value, failed)
    }
    pub fn is_pending_async_batch(&self) -> bool { !self.active_async_batch.is_empty() }
    pub fn resume(&mut self, value: Value, failed: bool) -> Result<()> {
        ensure!(
            self.program.version == VERSION,
            "program language version changed; original source and results remain saved"
        );
        ensure!(self.program.prelude_hash == super::prelude::hash(), "program prelude changed");
        self.migrate_frames()?;
        if !self.active_async_batch.is_empty() {
            let action = self.pending.as_ref().context("no suspended batch")?.clone();
            let results = if failed {
                vec![(true, value); self.active_async_batch.len()]
            } else {
                let items = value.as_array().context("async read batch must return per-item results")?;
                ensure!(items.len() == self.active_async_batch.len(), "async read batch result count mismatch");
                items.iter().map(|item| {
                    let failed = item.get("failed").and_then(Value::as_bool).context("async read item missing failed flag")?;
                    let value = item.get("value").cloned().context("async read item missing value")?;
                    Ok((failed, value))
                }).collect::<Result<Vec<_>>>()?
            };
            let mut outcomes = Vec::with_capacity(results.len());
            for (failed, value) in results {
                outcomes.push(if failed { super::promise::Outcome::Rejected(self.host_error(value, &action.at)) }
                    else { super::promise::Outcome::Fulfilled(self.heap.from_json(&value)?) });
            }
            self.pending = None;
            for (item, outcome) in std::mem::take(&mut self.active_async_batch).into_iter().zip(outcomes) {
                self.promises.settle_heap_promise(&mut self.heap, item.promise, outcome)?;
            }
            return Ok(());
        }
        let imported = if !failed && ((self.active_async_action.is_some() || self.active_reaction.is_some()) || self.pending.as_ref().and_then(|action| action.destination.as_ref()).is_some()) {
            Some(self.heap.from_json(&value)?)
        } else { None };
        let action = self.pending.take().context("no suspended effect")?;
        if let Some(pending) = self.active_async_action.take().or_else(|| self.active_reaction.take()) {
            ensure!(pending.action.id == action.id, "async effect id mismatch");
            let outcome = if failed {
                super::promise::Outcome::Rejected(self.host_error(value, &action.at))
            } else {
                super::promise::Outcome::Fulfilled(imported.context("missing imported async result")?)
            };
            self.promises.settle_heap_promise(&mut self.heap, pending.promise, outcome)?;
            return Ok(());
        }
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
        if self.program.prelude_hash != super::prelude::hash() {
            let error = json!({"message":"program prelude changed; saved state cannot be resumed"});
            self.finished = true; self.error = Some(error.clone()); return State::Failed(error);
        }
        if let Err(error) = self.migrate_frames() {
            self.finished = true;
            let error = json!({"message":format!("failed to restore VM values: {error:#}")});
            self.error = Some(error.clone());
            return State::Failed(error);
        }
        if let Some(action) = &self.pending {
            if matches!(action.kind, ActionKind::Fork { .. } | ActionKind::Join { .. }) {
                self.finished = true;
                let error = json!({"message":"saved fork/join programs are retired and cannot be resumed"});
                self.error = Some(error.clone());
                return State::Failed(error);
            }
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
            if self.frames.is_empty() {
                if let Some(job) = self.promises.pop_job() {
                    if let Some(reaction) = self.reactions.remove(&job.continuation) {
                        let (callback, value, rejected) = match job.outcome {
                            super::promise::Outcome::Fulfilled(value) => (reaction.fulfilled, value, false),
                            super::promise::Outcome::Rejected(value) => (reaction.rejected, value, true),
                        };
                        if !super::vm_eval::callable(&self.heap, &callback) {
                            let outcome = if rejected { super::promise::Outcome::Rejected(value) } else { super::promise::Outcome::Fulfilled(value) };
                            if let Err(error) = self.promises.settle_heap_promise(&mut self.heap, reaction.target, outcome) {
                                self.finished = true; self.error = Some(json!({"message":format!("{error:#}")}));
                            }
                            continue;
                        }
                        let node = match &callback { VmValue::Reference(id) => self.heap.get(*id).cloned(), _ => unreachable!() };
                        if let Ok(HeapNode::Native(name)) = &node && let Some(tool) = name.strip_prefix("tool:") {
                            let input = json_args(vec![value.clone()], &self.heap).and_then(|args| {
                                let input = args.into_iter().next().unwrap_or_else(|| json!({}));
                                ensure!(input.is_object(), "tool arguments must be an object");
                                Ok(input)
                            });
                            match input {
                                Ok(input) => {
                                    let asynchronous = tool.starts_with("async-tool:");
                                    let tool = tool.strip_prefix("async-tool:").unwrap_or(tool);
                                    let action = Action { id: format!("program-{}", crate::agent_service::uuid_like()), at: Location { line: 0, column: 0 }, destination: None, kind: ActionKind::Tool { name: tool.into(), args: input } };
                                    let pending = PendingAsyncAction { action: action.clone(), promise: reaction.target, scope: reaction.scope };
                                    if asynchronous { self.async_actions.push_back(pending); continue; }
                                    self.pending = Some(action.clone());
                                    self.active_reaction = Some(pending);
                                    return State::Action(action);
                                }
                                Err(error) => {
                                    let error = self.runtime_error(format!("{error:#}"), &Location { line: 0, column: 0 });
                                    if let Err(error) = self.promises.settle_heap_promise(&mut self.heap, reaction.target, super::promise::Outcome::Rejected(error)) {
                                        self.finished = true; self.error = Some(json!({"message":format!("{error:#}")}));
                                    }
                                    continue;
                                }
                            }
                        }
                        let result = match node {
                            Ok(HeapNode::Closure { function, environment }) => {
                                match Self::vm_frame(&self.program, &mut self.heap, Some(environment), &function, vec![value], None) {
                                    Ok(mut frame) => {
                                        let function = &self.program.functions[&function];
                                        if let Some(name) = function.self_name.as_ref().filter(|name| {
                                            !function.params.contains(name) && !function.parameter_lexical.contains(name)
                                                && (function.parameter_scope || (!function.vars.contains(name) && !function.lexical.contains(name)))
                                        }) {
                                            if let Some(env) = frame.env {
                                                if let Err(error) = self.heap.define_binding(env, name, callback) {
                                                    let error = self.runtime_error(format!("{error:#}"), &Location { line: 0, column: 0 });
                                                    let _ = self.promises.settle_heap_promise(&mut self.heap, reaction.target, super::promise::Outcome::Rejected(error));
                                                    continue;
                                                }
                                            }
                                        }
                                        frame.completion = Some(reaction.target); frame.origin = reaction.origin; self.frames.push(frame); Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Ok(HeapNode::PromiseResolver { capability, reject }) => {
                                self.call_resolver(capability, reject, value).and_then(|_| self.resolve_async(Some(reaction.target), VmValue::Undefined))
                            }
                            Ok(HeapNode::Native(name)) if !name.starts_with("tool:") => {
                                match super::js_methods::builtin(&mut self.heap, &name, &[value]) {
                                    Ok(value) => self.resolve_async(Some(reaction.target), value),
                                    Err(error) => Err(error),
                                }
                            }
                            Ok(_) => Err(anyhow::anyhow!("TypeError: this Promise callback is unavailable")),
                            Err(error) => Err(error),
                        };
                        if let Err(error) = result {
                            let value = self.runtime_error(format!("{error:#}"), &Location { line: 0, column: 0 });
                            if let Err(error) = self.promises.settle_heap_promise(&mut self.heap, reaction.target, super::promise::Outcome::Rejected(value)) {
                                self.finished = true; self.error = Some(json!({"message":format!("{error:#}")}));
                            }
                        }
                        continue;
                    }
                    if let Some(target) = self.adopting.remove(&job.continuation) {
                        let super::promise::Outcome::Fulfilled(VmValue::Reference(source)) = job.outcome else { unreachable!() };
                        let id = super::promise::ContinuationId(self.next_continuation);
                        let result = self.next_continuation.checked_add(1).context("continuation id exhausted").and_then(|next| {
                            self.next_continuation = next;
                            self.forwarding.insert(id, target);
                            self.promises.subscribe_heap_promise(&mut self.heap, source, id)
                        });
                        if let Err(error) = result { self.finished = true; self.error = Some(json!({"message":format!("{error:#}")})); }
                        continue;
                    }
                    if let Some(target) = self.forwarding.remove(&job.continuation) {
                        let result = match job.outcome {
                            super::promise::Outcome::Fulfilled(value) => self.resolve_async(target, value),
                            super::promise::Outcome::Rejected(value) => {
                                if let Some(target) = target {
                                    self.promises.settle_heap_promise(&mut self.heap, target, super::promise::Outcome::Rejected(value)).map(|_| ())
                                } else {
                                    self.error = Some(self.error_report(&value, &Location { line: 0, column: 0 }));
                                    self.finished = true;
                                    Ok(())
                                }
                            }
                        };
                        if let Err(error) = result { self.finished = true; self.error = Some(json!({"message":format!("{error:#}")})); }
                        continue;
                    }
                    let Some(coroutine) = self.continuations.remove(&job.continuation) else {
                        self.finished = true; self.error = Some(json!({"message":"missing coroutine continuation"})); continue;
                    };
                    self.frames = coroutine.frames;
                    match job.outcome {
                        super::promise::Outcome::Fulfilled(value) => {
                            if let Err(error) = self.store(Some(coroutine.destination), value) {
                                self.finished = true; self.error = Some(json!({"message":format!("{error:#}")}));
                            }
                        }
                        super::promise::Outcome::Rejected(value) => self.throw(value, &coroutine.at),
                    }
                    continue;
                }
                if let Some(pending) = self.async_actions.pop_front() {
                    let read = |action: &Action| matches!(&action.kind, ActionKind::Tool { name, .. } if matches!(name.as_str(), "read_file" | "read_file_lines" | "search_text" | "list_dir"));
                    if read(&pending.action) && self.async_actions.front().is_some_and(|item| read(&item.action)) {
                        let at = pending.action.at.clone();
                        let mut batch = vec![pending];
                        while batch.len() < 32 && self.async_actions.front().is_some_and(|item| read(&item.action)) {
                            batch.push(self.async_actions.pop_front().unwrap());
                        }
                        let operations = batch.iter().map(|item| match &item.action.kind {
                            ActionKind::Tool { name, args } => (name.clone(), args.clone()), _ => unreachable!(),
                        }).collect();
                        let action = Action { id: format!("program-{}", crate::agent_service::uuid_like()), at, destination: None, kind: ActionKind::ParallelReads { operations } };
                        self.pending = Some(action.clone());
                        self.active_async_batch = batch;
                        return State::Action(action);
                    }
                    let action = pending.action.clone();
                    self.pending = Some(action.clone());
                    self.active_async_action = Some(pending);
                    return State::Action(action);
                }
                if !self.inflight_async.is_empty() { return State::Waiting; }
                let unhandled = self.async_promises.iter().find_map(|id| match self.heap.get(*id) {
                    Ok(HeapNode::Promise { outcome: Some(super::promise::Outcome::Rejected(value)), handled: false, .. }) => Some((*id, value.clone())),
                    _ => None,
                });
                if let Some((promise, value)) = unhandled {
                    let thrown = match &value { VmValue::Reference(id) => Some(*id), _ => None };
                    let site = self.throw_sites.get(&promise).or_else(|| thrown.and_then(|id| self.throw_sites.get(&id))).cloned();
                    let mut report = self.error_report(&value, &Location { line: 0, column: 0 });
                    if let Some(site) = site {
                        report["location"] = site["location"].clone();
                        report["stack"] = site["stack"].clone();
                    }
                    self.error = Some(report);
                    self.finished = true;
                    continue;
                }
                if let Some(value) = self.root_result.take() {
                    match self.heap.to_json(&value) {
                        Ok(value) => self.result = value,
                        Err(_error) if value == VmValue::Undefined => self.result = Value::Null,
                        Err(error) => { self.error = Some(json!({"message":format!("{error:#}")})); }
                    }
                    self.finished = true;
                } else {
                    self.finished = true;
                    self.error = Some(json!({"message":"async execution is waiting for an unresolved promise"}));
                }
                continue;
            }
            if self.fuel == 0 {
                self.finished = true;
                self.error = Some(json!({"message":"program instruction budget exhausted"}));
                return State::Failed(self.error.clone().unwrap());
            }
            self.fuel -= 1;
            if self.fuel % 256 == 0 {
                self.async_promises.retain(|id| match self.heap.get(*id) {
                    Ok(HeapNode::Promise { outcome: None, .. }) => true,
                    Ok(HeapNode::Promise { outcome: Some(super::promise::Outcome::Rejected(_)), handled: false, .. }) => true,
                    _ => false,
                });
                let mut roots = self.promises.roots();
                roots.extend(self.async_promises.iter().copied().map(VmValue::Reference));
                roots.extend(self.root_result.iter().cloned());
                roots.extend(self.async_actions.iter().map(|action| VmValue::Reference(action.promise)));
                roots.extend(self.active_async_action.iter().map(|action| VmValue::Reference(action.promise)));
                roots.extend(self.active_reaction.iter().map(|action| VmValue::Reference(action.promise)));
                roots.extend(self.active_async_batch.iter().map(|action| VmValue::Reference(action.promise)));
                roots.extend(self.inflight_async.values().map(|action| VmValue::Reference(action.promise)));
                roots.extend(self.forwarding.values().chain(self.adopting.values()).flatten().copied().map(VmValue::Reference));
                for reaction in self.reactions.values() {
                    roots.extend([reaction.fulfilled.clone(), reaction.rejected.clone(), VmValue::Reference(reaction.target)]);
                    roots.extend(reaction.origin.iter().copied().map(VmValue::Reference));
                }
                for frame in self.frames.iter().chain(self.continuations.values().flat_map(|c| c.frames.iter())) {
                    if let Some(locals) = &frame.vm_locals { roots.extend(locals.values().cloned()); }
                    roots.extend(frame.origin.iter().copied().map(VmValue::Reference));
                    roots.extend(frame.executor.map(VmValue::Reference));
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
                let heap = &self.heap;
                self.throw_sites.retain(|id, _| heap.get(*id).is_ok());
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
                    Op::EnterFunctionBody => {
                        let frame = self.frames.last().unwrap();
                        let parent = frame.env.context("missing parameter scope")?;
                        let function = &self.program.functions[&frame.function];
                        ensure!(function.parameter_scope, "unexpected function body scope");
                        let mut bindings = BTreeMap::new();
                        for name in &function.vars {
                            let value = if function.parameter_lexical.contains(name) {
                                self.heap.lookup_binding(parent, name)?
                            } else { VmValue::Undefined };
                            bindings.insert(name.clone(), value);
                        }
                        let env = self.heap.new_environment(Some(parent), bindings, function.lexical.iter().cloned().collect())?;
                        self.frames.last_mut().unwrap().env = Some(env);
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
                    Op::Await { value, destination } => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let value = eval(&value, locals, &mut self.heap)?;
                        let id = super::promise::ContinuationId(self.next_continuation);
                        self.next_continuation = self.next_continuation.checked_add(1).context("continuation id exhausted")?;
                        let boundary = self.frames.iter().rposition(|frame| frame.completion.is_some()).unwrap_or(0);
                        let frames = self.frames.split_off(boundary);
                        self.continuations.insert(id, SuspendedCoroutine { frames, destination, at: at.clone() });
                        if let VmValue::Reference(promise) = value {
                            if matches!(self.heap.get(promise)?, HeapNode::Promise { .. }) {
                                self.promises.subscribe_heap_promise(&mut self.heap, promise, id)?;
                            } else { self.promises.enqueue(id, super::promise::Outcome::Fulfilled(VmValue::Reference(promise))); }
                        } else { self.promises.enqueue(id, super::promise::Outcome::Fulfilled(value)); }
                    }
                    Op::Return(e) => {
                        let locals = self.frames.last().unwrap().locals()?;
                        let value = eval(&e, locals, &mut self.heap)?;
                        let frame = self.frames.pop().unwrap();
                        if let Some(promise) = frame.completion {
                            self.resolve_async(Some(promise), value)?;
                        } else if let Some(parent) = self.frames.last_mut() {
                            if let Some(dest) = frame.destination {
                                parent.vm_locals.as_mut().unwrap().insert(dest, value);
                            }
                        } else {
                            self.resolve_async(None, value)?;
                        }
                    }
                    Op::Effect(destination, effect) => {
                        let frame = self.frames.last().unwrap();
                        let temps = frame.vm_locals.clone().context("missing VM locals")?;
                        let env = frame.env;
                        let locals = Locals { temps: &temps, env };
                        let kind = match effect {
                            Effect::PromiseConstruct { executor } => {
                                let executor = eval(&executor, locals, &mut self.heap)?;
                                ensure!(super::vm_eval::callable(&self.heap, &executor), "TypeError: Promise executor is not a function");
                                let promise = self.promises.create_heap_promise(&mut self.heap)?;
                                self.async_promises.push(promise);
                                let capability = self.heap.allocate(HeapNode::PromiseCapability { promise, resolved: false })?;
                                let resolve = self.heap.allocate(HeapNode::PromiseResolver { capability, reject: false })?;
                                let reject = self.heap.allocate(HeapNode::PromiseResolver { capability, reject: true })?;
                                self.store(destination, VmValue::Reference(promise))?;
                                let before = self.frames.len();
                                match self.invoke(executor, vec![VmValue::Reference(resolve), VmValue::Reference(reject)], None) {
                                    Ok(None) => {
                                        if self.frames.len() > before && self.frames.last().unwrap().completion.is_none() {
                                            self.frames.last_mut().unwrap().executor = Some(capability);
                                        }
                                    }
                                    Ok(Some(_)) => {
                                        let error = self.runtime_error("TypeError: host tools cannot be Promise executors".into(), &at);
                                        self.call_resolver(capability, true, error)?;
                                    }
                                    Err(error) => {
                                        let error = self.runtime_error(format!("{error:#}"), &at);
                                        self.call_resolver(capability, true, error)?;
                                    }
                                }
                                return Ok(None);
                            }
                            Effect::PromiseCreate { reject, value } => {
                                let value = eval(&value, locals, &mut self.heap)?;
                                if !reject && let VmValue::Reference(id) = &value
                                    && matches!(self.heap.get(*id)?, HeapNode::Promise { .. })
                                {
                                    self.store(destination, value)?;
                                } else {
                                    let id = self.promises.create_heap_promise(&mut self.heap)?;
                                    self.async_promises.push(id);
                                    let outcome = if reject { super::promise::Outcome::Rejected(value) } else { super::promise::Outcome::Fulfilled(value) };
                                    self.promises.settle_heap_promise(&mut self.heap, id, outcome)?;
                                    self.store(destination, VmValue::Reference(id))?;
                                }
                                return Ok(None);
                            }
                            Effect::Call(name, args) => {
                                let args = vm_args(&args, locals, &mut self.heap)?;
                                let function = self
                                    .program
                                    .functions
                                    .get(&name)
                                    .with_context(|| format!("unknown function {name}"))?;
                                if function.virtual_task {
                                    // AI functions take exactly their declared parameters.
                                    ensure!(
                                        args.len() == function.params.len(),
                                        "{name}: expected {} arguments, got {}",
                                        function.params.len(),
                                        args.len()
                                    );
                                    // A top-level function closes over the module scope.
                                    let captured = self.captured(&name, self.module_env)?;
                                    ActionKind::Call { name, args: json_args(args, &self.heap)?, captured }
                                } else {
                                    // Functions see the module scope, not their caller's variables.
                                    ensure!(self.frames.len() < 64, "program call depth limit");
                                    let is_async = function.is_async;
                                    let mut frame = Self::vm_frame(
                                        &self.program,
                                        &mut self.heap,
                                        self.module_env,
                                        &name,
                                        args,
                                        destination,
                                    )?;
                                    if is_async {
                                        let promise = self.promises.create_heap_promise(&mut self.heap)?;
                                        self.async_promises.push(promise);
                                        self.store(frame.destination.take(), VmValue::Reference(promise))?;
                                        frame.completion = Some(promise);
                                        frame.origin = self.async_origin();
                                    }
                                    self.frames.push(frame);
                                    return Ok(None);
                                }
                            }
                            Effect::Apply { callee, args } => {
                                let callee = eval(&callee, locals, &mut self.heap)?;
                                let args = eval(&args, locals, &mut self.heap)?;
                                let VmValue::Reference(id) = args else { bail!("TypeError: apply arguments must be an array") };
                                let HeapNode::Array(args) = self.heap.get(id)? else { bail!("TypeError: apply arguments must be an array") };
                                let args = args.clone();
                                match self.invoke(callee, args, destination.clone())? {
                                    Some(kind) => kind,
                                    None => return Ok(None),
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
                                match self.invoke_method(receiver, &name, args, destination.clone(), false)? {
                                    Some(kind) => kind,
                                    None => return Ok(None),
                                }
                            }
                            Effect::DynamicMethodApply { receiver, name, args } => {
                                let receiver = eval(&receiver, locals, &mut self.heap)?;
                                let name = eval(&name, locals, &mut self.heap)?;
                                let name = super::js_value::to_string(&self.heap, &name)?;
                                let args = eval(&args, locals, &mut self.heap)?;
                                let VmValue::Reference(id) = args else { bail!("TypeError: apply arguments must be an array") };
                                let HeapNode::Array(args) = self.heap.get(id)? else { bail!("TypeError: apply arguments must be an array") };
                                let args = args.clone();
                                match self.invoke_method(receiver, &name, args, destination.clone(), true)? {
                                    Some(kind) => kind,
                                    None => return Ok(None),
                                }
                            }
                            Effect::MethodApply { receiver, name, args, ignore_property } => {
                                let receiver = eval(&receiver, locals, &mut self.heap)?;
                                let args = eval(&args, locals, &mut self.heap)?;
                                let VmValue::Reference(id) = args else { bail!("TypeError: apply arguments must be an array") };
                                let HeapNode::Array(args) = self.heap.get(id)? else { bail!("TypeError: apply arguments must be an array") };
                                let args = args.clone();
                                match self.invoke_method(receiver, &name, args, destination.clone(), ignore_property)? {
                                    Some(kind) => kind,
                                    None => return Ok(None),
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
                            Effect::Fork(..) | Effect::Join(..) => bail!("saved fork/join programs are retired and cannot be resumed"),
                            Effect::Task {
                                name,
                                description,
                                input,
                            } => {
                                let value = eval(&input, locals, &mut self.heap)?;
                                ActionKind::Task { name, description, input: self.heap.to_json(&value)? }
                            },
                        };
                        let mut kind = kind;
                        let asynchronous = match &mut kind {
                            ActionKind::Tool { name, .. } if name.starts_with("async-tool:") => { *name = name["async-tool:".len()..].to_owned(); true }
                            ActionKind::Call { name, .. } => self.program.functions.get(name).is_some_and(|f| f.virtual_task && f.is_async),
                            _ => false,
                        };
                        let action = Action {
                            id: format!("program-{}", crate::agent_service::uuid_like()),
                            at: at.clone(),
                            destination: if asynchronous { None } else { destination.clone() },
                            kind,
                        };
                        if asynchronous {
                            ensure!(self.async_actions.len() + self.inflight_async.len() + usize::from(self.active_async_action.is_some()) + self.active_async_batch.len() < 64, "too many pending async host actions");
                            let promise = self.promises.create_heap_promise(&mut self.heap)?;
                            self.async_promises.push(promise);
                            self.store(destination, VmValue::Reference(promise))?;
                            let scope = self.named_bindings();
                            self.async_actions.push_back(PendingAsyncAction { action, promise, scope });
                            return Ok(None);
                        }
                        return Ok(Some(action));
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
