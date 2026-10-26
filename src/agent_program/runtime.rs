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
    /// Decode-only legacy field. Version 8 actions always use None.
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
            .with_context(|| format!("unknown function {name}"))?;
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
    pub fn resume(&mut self, value: Value, failed: bool) -> Result<()> {
        ensure!(
            self.program.version == VERSION,
            "program language version changed; original source and results remain saved"
        );
        let action = self.pending.as_ref().context("no suspended effect")?;
        ensure!(
            action.semantic_boolean.is_none(),
            "legacy implicit semantic tasks cannot resume"
        );
        let action = self.pending.take().unwrap();
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
                            Effect::Semantic { .. } | Effect::Ai(_) => bail!(
                                "legacy implicit AI expression effects are unsupported; use an explicit comment task"
                            ),
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
                        };
                        return Ok(Some(Action {
                            id: format!("program-{}", crate::agent_service::uuid_like()),
                            at: at.clone(),
                            destination,
                            kind,
                            semantic_boolean: None,
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
