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
    /// Some(true) requires a boolean; Some(false) accepts any semantic JSON value.
    #[serde(default)]
    pub semantic_boolean: Option<bool>,
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
    Fork {
        entry: String,
        args: Vec<Value>,
    },
    Join {
        ids: Vec<String>,
    },
    Ai {
        config: Value,
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
                    json!(s.encode_utf16().count())
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
                _ => bail!("cannot access member {} on {}", key, obj),
            }
        }
        Expr::Binary(..)
        | Expr::Unary(..)
        | Expr::Conditional(..)
        | Expr::Stringify(..)
        | Expr::StringMethod(..) => {
            bail!("operators and ordinary methods are forbidden in orchestration programs")
        }
    })
}

impl Runtime {
    pub fn new(program: Program, entry: &str, args: Vec<Value>) -> Result<Self> {
        ensure!(program.version == VERSION, "unsupported program version");
        let frame = Self::frame(&program, entry, args, None)?;
        Ok(Self {
            program,
            scoped_calls: false,
            frames: vec![frame],
            pending: None,
            finished: false,
            result: Value::Null,
            error: None,
            fuel: 100_000,
        })
    }
    /// The host represents every AI-defined call as a durable task scope.
    /// Standalone runtimes can still execute inline frames without a host.
    pub fn task_program(program: Program, entry: &str, args: Vec<Value>) -> Result<Self> {
        let mut runtime = Self::new(program, entry, args)?;
        runtime.scoped_calls = true;
        Ok(runtime)
    }
    fn frame(
        program: &Program,
        name: &str,
        args: Vec<Value>,
        destination: Option<String>,
    ) -> Result<Frame> {
        let f = program
            .functions
            .get(name)
            .with_context(|| format!("unknown generator {name}"))?;
        ensure!(
            args.len() == f.params.len(),
            "{name}: expected {} arguments, got {}",
            f.params.len(),
            args.len()
        );
        Ok(Frame {
            function: name.into(),
            pc: 0,
            locals: f.params.iter().cloned().zip(args).collect(),
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
    pub fn resume(&mut self, mut value: Value, mut failed: bool) -> Result<()> {
        let action = self.pending.take().context("no suspended effect")?;
        if !failed && let Some(boolean) = action.semantic_boolean {
            // Preserve the original envelope in the normal task/tool receipt.
            // Validate the handoff contract, never infer whether its evidence
            // establishes a domain-specific acceptance criterion.
            if value.get("value").is_none()
                || (boolean && !value["value"].is_boolean())
                || !value["evidence"].as_array().is_some_and(|a| !a.is_empty())
            {
                failed = true;
                value = json!({"message":"semantic task must return {value, evidence:[...]} with nonempty evidence and a boolean value for conditions","receipt":value});
            } else {
                value = value["value"].clone();
            }
        }
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
                self.raise(
                    json!({"message":"program instruction budget exhausted"}),
                    &Location { line: 1, column: 1 },
                );
                break;
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
                        if !eval(&test, &frame.locals)?.as_bool().context("if condition must be a boolean task result; truthiness coercion is forbidden")? {
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
                        let semantic_boolean = match &effect {
                            Effect::Semantic { boolean, .. } => Some(*boolean),
                            _ => None,
                        };
                        let kind = match effect {
                            Effect::Semantic {
                                expression,
                                description,
                                bindings,
                                boolean,
                                direct,
                            } => {
                                if let Some(value) = direct {
                                    let value = eval(&value, locals)?;
                                    if value.is_boolean() {
                                        if let Some(destination) = destination {
                                            self.frames
                                                .last_mut()
                                                .unwrap()
                                                .locals
                                                .insert(destination, value);
                                        }
                                        return Ok(None);
                                    }
                                }
                                ActionKind::Task {
                                    name: "semantic_expression".into(),
                                    description: format!(
                                        "Interpret this flow pseudocode expression according to the current task, its requirements, comments and supplied bindings: {expression}\nFunction comments: {description}\nOperators and method names describe intent; do not execute JavaScript or assume object identity comparison. Use the inherited materials and actual tool evidence; read or inspect real artifacts when the condition requires it. Bindings may include $condition_value, the already obtained value: never repeat its original tool/helper call. Do only this expression, then return JSON {{\"value\":{},\"evidence\":[...]}}. Evidence must be nonempty and cite supplied inputs, saved results or actual tool observations; do not invent observations. If evidence is missing, obtain it with the advertised tools or return $task_error. The host alone resumes the caller and chooses the branch.",
                                        if boolean {
                                            "true_or_false"
                                        } else {
                                            "the_requested_JSON_value"
                                        }
                                    ),
                                    input: json!({"expression":expression,"bindings":eval(&bindings, locals)?,"return_type":if boolean {"boolean"} else {"json"}}),
                                }
                            }
                            Effect::Call(name, args) => {
                                let args = values(&args)?;
                                Self::frame(
                                    &self.program,
                                    &name,
                                    args.clone(),
                                    destination.clone(),
                                )?;
                                if self.scoped_calls {
                                    ActionKind::Call { name, args }
                                } else {
                                    ensure!(self.frames.len() < 64, "program call depth limit");
                                    self.frames.push(Self::frame(
                                        &self.program,
                                        &name,
                                        args,
                                        destination,
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
                            Effect::Ai(config) => {
                                let config = eval(&config, locals)?;
                                ensure!(
                                    config.is_object() && config["instruction"].is_string(),
                                    "ai needs {{instruction,input,format}}"
                                );
                                ensure!(
                                    config["format"].is_null()
                                        || matches!(
                                            config["format"].as_str(),
                                            Some("text" | "json")
                                        ),
                                    "ai format must be text or json"
                                );
                                ActionKind::Ai { config }
                            }
                        };
                        return Ok(Some(Action {
                            id: format!("program-{}", crate::agent_service::uuid_like()),
                            at: at.clone(),
                            destination,
                            kind,
                            semantic_boolean,
                        }));
                    }
                }
                Ok(None)
            })();
            match outcome {
                Ok(Some(action)) => {
                    self.pending = Some(action.clone());
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
