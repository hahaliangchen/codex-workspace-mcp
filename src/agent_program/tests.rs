use super::*;
use runtime::ActionKind;

// Test-only effect targets, declared just like plugin methods.
fn compile(source: &str) -> anyhow::Result<Program> {
    let mut bindings = libraries::builtin();
    bindings.insert(
        "host".into(),
        ["inspect_agent_changes", "merge_agent_result"]
            .into_iter()
            .map(|n| (n.into(), n.into()))
            .collect(),
    );
    bindings.insert(
        "test".into(),
        ["record", "never", "selected"]
            .into_iter()
            .map(|n| (n.into(), n.into()))
            .collect(),
    );
    compile_with_libraries(source, bindings)
}
fn runtime(source: &str) -> Runtime {
    Runtime::new(compile(source).unwrap(), "main", vec![]).unwrap()
}
fn returned(mut r: Runtime) -> Value {
    match r.next() {
        State::Returned(v) => v,
        other => panic!("expected return, got {other:?}"),
    }
}

#[test]
fn serial_page_review_reads_in_order_then_uses_one_ai_scope() {
    let source = include_str!("../../skills/task-program/examples/serial-page-review.js");
    let program = compile(source).unwrap();
    let mut runtime = Runtime::task_program(program, "main", vec![json!({
        "paths":["1.html","2.html","3.html","4.html","5.html"],
        "requirements":"检查正文"
    })]).unwrap();
    for n in 1..=5 {
        let State::Action(action) = runtime.next() else { panic!("missing page read") };
        assert!(matches!(action.kind, ActionKind::Tool { ref name, ref args }
            if name == "read_file" && args["path"] == format!("{n}.html")));
        runtime.resume(json!({"content":format!("page {n}")}), false).unwrap();
    }
    let State::Action(action) = runtime.next() else { panic!("missing AI inspection") };
    match action.kind {
        ActionKind::Call { name, args } => {
            assert_eq!(name, "inspectPages");
            assert_eq!(args[0].as_array().unwrap().len(), 5);
            assert_eq!(args[0][0]["initial"]["content"], "page 1");
            assert_eq!(args[0][4]["path"], "5.html");
            assert_eq!(args[1], "检查正文");
        }
        other => panic!("expected one AI task, got {other:?}"),
    }
    runtime.resume(json!({"reviewed":5}), false).unwrap();
    assert_eq!(returned(runtime), json!({"reviewed":5}));
}

#[test]
fn await_promise_all_suspends_once_and_returns_ordered_read_results() {
    let mut run = runtime(r#"
async function main() {
  const paths = ["first.txt", "second.txt"];
  return await Promise.all([
    io.read({path:paths[0]}),
    io.read({path:paths[1]}),
    io.search({path:".",query:"needle"})
  ]);
}"#);
    let State::Action(action) = run.next() else { panic!("missing parallel reads") };
    match action.kind {
        ActionKind::ParallelReads { operations } => {
            assert_eq!(operations.len(), 3);
            assert_eq!(operations[0].0, "read_file");
            assert_eq!(operations[0].1["path"], "first.txt");
            assert_eq!(operations[1].1["path"], "second.txt");
            assert_eq!(operations[2].0, "search_text");
        }
        other => panic!("unexpected action {other:?}"),
    }
    let mut recovered: Runtime = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
    let State::Action(again) = recovered.next() else { panic!("missing saved action") };
    assert_eq!(again.id, action.id);
    recovered.resume(json!([{"content":"first"},{"content":"second"},{"matches":[]}]), false).unwrap();
    assert_eq!(returned(recovered), json!([{"content":"first"},{"content":"second"},{"matches":[]}]));
}

#[test]
fn promise_all_rejects_writes_and_ai_calls() {
    assert!(compile("async function main(){return await Promise.all([io.write({path:'a',content:'b'})]);}").is_err());
    assert!(compile("async function main(){return await Promise.all([judge()]);} /** @virtual */ function judge(){}").is_err());
    assert!(compile("async function main(){return await Promise.all([fork(worker)]);} function worker(){return 1;}").is_err());
}

#[test]
fn failed_parallel_reads_follow_catch_without_repeating_the_batch() {
    let mut run = runtime(r#"
async function main() {
  try { return await Promise.all([io.read({path:"a"}),io.read({path:"b"})]); }
  catch(error) { return error.message; }
}"#);
    assert!(matches!(run.next(), State::Action(action) if matches!(action.kind, ActionKind::ParallelReads { .. })));
    run.resume(json!({"message":"read failed"}), true).unwrap();
    assert_eq!(returned(run), json!("read failed"));
}

#[test]
fn promise_all_maps_dynamic_page_list_without_agents() {
    let source = include_str!("../../skills/task-program/examples/async-page-review.js");
    let program = compile(source).unwrap();
    let mut run = Runtime::task_program(program, "main", vec![json!({"paths":["a.html","b.html","c.html","d.html","e.html"],"requirements":"检查正文"})]).unwrap();
    let State::Action(action) = run.next() else { panic!("missing batch") };
    let ActionKind::ParallelReads { operations } = action.kind else { panic!("expected read batch") };
    assert_eq!(operations.len(), 5);
    assert_eq!(operations.iter().map(|(_, args)|args["path"].as_str().unwrap()).collect::<Vec<_>>(), vec!["a.html","b.html","c.html","d.html","e.html"]);
    run.resume(json!([{ "content":"a" },{ "content":"b" },{ "content":"c" },{ "content":"d" },{ "content":"e" }]), false).unwrap();
    let State::Action(action) = run.next() else { panic!("missing AI inspection") };
    assert!(matches!(action.kind, ActionKind::Call { ref name, ref args } if name == "inspectPages" && args[0][4]["initial"]["content"] == "e" && args[1] == "检查正文"));
}

#[test]
fn virtual_function_body_can_be_refined_to_an_async_entry() {
    let program = compile_body(r#"
async function review(paths) {
  return await Promise.all(paths.map(path => io.read({path:path})));
}"#, "review").unwrap();
    let mut run = Runtime::task_program(program, "review", vec![json!(["a","b"])]).unwrap();
    let State::Action(action) = run.next() else { panic!("missing read batch") };
    assert!(matches!(action.kind, ActionKind::ParallelReads { ref operations } if operations.len() == 2));
    run.resume(json!([{ "content":"a" },{ "content":"b" }]), false).unwrap();
    assert_eq!(returned(run), json!([{ "content":"a" },{ "content":"b" }]));
    assert!(compile_body("async function review(){return review();}", "review").is_err());
}
#[test]
fn data_loops_local_calls_and_destructuring_need_no_operators() {
    let mut r = runtime(
        r#"
function echo(input) {return input;}
function main() {
  const result = echo({ready:true, rows:[{n:2},{n:3}]});
  for (const item of result.rows) {test.record({item});}
  const [a,b] = result.rows;
  if(result.ready){return {a,b};} else {throw 'not ready';}
}"#,
    );
    for n in [2, 3] {
        let State::Action(action) = r.next() else {
            panic!()
        };
        assert!(
            matches!(action.kind,ActionKind::Tool{ref name,ref args} if name=="record" && args["item"]["n"]==n)
        );
        r.resume(json!({}), false).unwrap();
    }
    assert_eq!(returned(r), json!({"a":{"n":2},"b":{"n":3}}));
}
#[test]
fn suspended_effect_serializes_and_resumes_once() {
    let mut r = runtime(
        "function main(){ const receipt = io.write({path:'a.txt',content:'x'}); return receipt.version; }",
    );
    let State::Action(first) = r.next() else {
        panic!()
    };
    assert!(matches!(first.kind,ActionKind::Tool{ref name,..} if name=="write_file"));
    let mut restored: Runtime = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    let State::Action(again) = restored.next() else {
        panic!()
    };
    assert_eq!(first.id, again.id);
    restored.resume(json!({"version":"v1"}), false).unwrap();
    assert!(restored.resume(json!({}), false).is_err());
    assert_eq!(returned(restored), json!("v1"));
}
#[test]
fn child_failure_unwinds_to_callers_catch_and_skips_following_effect() {
    let mut r = runtime(
        "function child(){ io.read({path:'missing'}); io.write({path:'bad'}); } function main(){ try { child(); } catch(error) { return error.message; } }",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"message":"not found"}), true).unwrap();
    assert_eq!(returned(r), json!("not found"));
    let mut r = runtime("function main(){ io.read({path:'missing'}); io.write({path:'bad'}); }");
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"message":"not found"}), true).unwrap();
    let State::Failed(error) = r.next() else {
        panic!()
    };
    assert_eq!(error["message"], "not found");
    assert!(error["location"]["line"].is_number());
}
#[test]
fn fork_and_join_preserve_handles_and_typed_results_in_order() {
    let mut r = runtime(
        "function worker(input){return input;} function main(){ const a=fork(worker,{n:1}); const b=fork(worker,{n:2}); const [x,y]=join(a,b); return {x,y}; }",
    );
    let State::Action(a) = r.next() else { panic!() };
    assert!(
        matches!(a.kind,ActionKind::Fork{ref entry,ref args} if entry=="worker" && args[0]["n"]==1.0)
    );
    r.resume(json!({"agent_id":"a"}), false).unwrap();
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"agent_id":"b"}), false).unwrap();
    let State::Action(join) = r.next() else {
        panic!()
    };
    assert!(matches!(join.kind,ActionKind::Join{ids} if ids==vec!["a","b"]));
    r.resume(json!([{"output":"A"},{"output":"B"}]), false)
        .unwrap();
    assert_eq!(returned(r), json!({"x":{"output":"A"},"y":{"output":"B"}}));
}
#[test]
fn unsupported_code_is_rejected_with_location_before_effects() {
    for source in [
        "function main(){io.write({path:'bad'});\n fetch('https://example.com');}",
        "function main(){const x=1; x=2;}",
        "import fs from 'fs'; function main(){}",
        "function main(){tools.create_task({});}",
    ] {
        let error = compile(source).unwrap_err().to_string();
        assert!(error.starts_with("TS_PROGRAM "), "{error}");
        assert!(error.contains(':'), "missing source location: {error}");
    }
}

#[test]
fn call_depth_errors_are_catchable() {
    let r = runtime(
        "function recurse(){return recurse();} function main(){try {return recurse();} catch(e){return e.message;}}",
    );
    assert_eq!(returned(r), json!("program call depth limit"));
}

#[test]
fn deterministic_expressions_return_without_model_actions() {
    for (expression, expected) in [
        ("1+2*3", json!(7)),
        ("5/2", json!(2.5)),
        ("-5%2", json!(-1)),
        ("1===1", json!(true)),
        ("1 !== '1'", json!(true)),
        ("[1,{x:2}]===[1,{x:2}]", json!(true)),
        ("!false", json!(true)),
        ("+3", json!(3)),
        ("false && true", json!(false)),
        ("true || false", json!(true)),
        ("null ?? 4", json!(4)),
        ("false ?? 4", json!(false)),
        ("true ? 1 : 2", json!(1)),
        ("`value:${1+2}`", json!("value:3")),
        ("'a'+'b'", json!("ab")),
        ("'中文😀'.length", json!(3)),
        ("'中文😀'.indexOf('😀')", json!(2)),
        ("'abcd'.slice(1,-1)", json!("bc")),
        ("[1,2,3].slice(1)", json!([2, 3])),
        ("' a '.trim().toUpperCase()", json!("A")),
        ("'a,b'.split(',')", json!(["a", "b"])),
        ("[1,true,null].join('-')", json!("1-true-null")),
        ("[1,2].includes(2)", json!(true)),
        ("JSON.parse(JSON.stringify({ok:true}))", json!({"ok":true})),
        ("9007199254740993 + 2", json!(9007199254740995u64)),
    ] {
        for _ in 0..3 {
            assert_eq!(
                returned(runtime(&format!("function main(){{return {expression};}}"))),
                expected,
                "{expression}"
            );
        }
    }
    assert!(compile("function* main(){yield io.read({path: 'x'});}").is_err());
    assert_eq!(
        returned(runtime(
            "function main(){return 'function* x(){yield call();}';}"
        )),
        json!("function* x(){yield call();}")
    );
}

#[test]
fn executable_extensions_and_unknown_data_methods_are_rejected_before_effects() {
    for expression in [
        "delete x.a",
        "x &&= true",
        "x ??= 1",
        "x!",
        "(1,2)",
        "(()=>1)()",
        "new Date()",
        "x.map(item=>item)",
        "x.check({run(){return 1;}})",
        "x.unknown()",
    ] {
        let error = compile(&format!(
            "function main(){{let x=1;io.write({{path:'never'}});\nreturn {expression};}}"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("TS_PROGRAM 2:"), "{error}");
    }
    assert!(compile("function main(){const x=1;x=2;}").is_err());
    assert!(compile("function main(){let x={};x.a=2;}").is_err());
}

#[test]
fn business_judgment_is_an_explicit_task_with_named_inputs_and_evidence() {
    let source = "function main(要求){let 产物={path:'artifact.html'};const check=检查(产物,要求);if(!check.passed){产物={path:'repaired.html'};}return 产物;} function 检查(产物,要求){/* @virtual */ // Inspect real artifacts; return {passed:boolean,evidence:[...]}.\n}";
    for (passed, path) in [(true, "artifact.html"), (false, "repaired.html")] {
        let mut r = Runtime::new(
            compile(source).unwrap(),
            "main",
            vec![json!({"expected":"approved"})],
        )
        .unwrap();
        let State::Action(action) = r.next() else {
            panic!()
        };
        assert!(
            matches!(action.kind,ActionKind::Task{name,description,input} if name=="检查" && description.contains("evidence") && input["产物"]["path"]=="artifact.html" && input["要求"]["expected"]=="approved")
        );
        r.resume(
            json!({"passed":passed,"evidence":[{"source":"actual tool receipt"}]}),
            false,
        )
        .unwrap();
        assert_eq!(returned(r), json!({"path":path}));
    }
    assert!(compile("function main(){if(实际产物符合要求){return 1;}return 0;}").is_err());
}

#[test]
fn type_errors_and_assertions_use_the_existing_exception_path() {
    for statement in [
        "1/0;",
        "JSON.parse('bad');",
        "for(1/0;false;){}",
        "for(var i=0;i<1;1/0){i++;}",
    ] {
        assert_eq!(
            returned(runtime(&format!(
                "function main(){{try{{{statement}}}catch(error){{return 'caught';}}return 'missed';}}"
            ))),
            json!("caught"),
            "{statement}"
        );
    }
    for expression in [
        "1/0",
        "1%0",
        "true+1",
        "1<false",
        "18446744073709551615+1",
        "1e308*1e308",
        "JSON.parse('bad')",
        "`${{x:1}}`",
        "'x'.slice(0.5)",
        "9007199254740993/2",
    ] {
        let r = runtime(&format!(
            "function main(){{try{{return {expression};}}catch(error){{return {{caught:true,message:error.message}};}}}}"
        ));
        assert_eq!(returned(r)["caught"], true, "{expression}");
    }
    assert!(
        matches!(runtime("function main(){assert(false,'failed');io.write({path:'never'});}").next(),State::Failed(e) if e["message"]=="failed")
    );
    assert_eq!(
        returned(runtime("function main(){assert(1+1===2);return 'passed';}")),
        json!("passed")
    );
}

#[test]
fn if_else_uses_returned_boolean_or_field_directly() {
    let source = "function check(input){return input;} function main(input){const result=check(input);if(result.ok){return 'yes';}else if(result.retry){return 'retry';}else{return 'no';}}";
    for (input, expected) in [
        (json!({"ok":true}), "yes"),
        (json!({"ok":false,"retry":true}), "retry"),
        (json!({"ok":false,"retry":false}), "no"),
    ] {
        assert_eq!(
            returned(Runtime::new(compile(source).unwrap(), "main", vec![input]).unwrap()),
            json!(expected)
        );
    }
    let mut r = runtime(
        "function main(){const ok=check();if(ok){return 'yes';}else{return 'no';}} function check(){/* @virtual */ // Return a boolean conclusion from actual evidence.\n}",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!(false), false).unwrap();
    assert_eq!(returned(r), json!("no"));
}

#[test]
fn c_style_truth_values_are_deterministic_for_all_json_types() {
    for (value, truth) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(0.0), false),
        (json!(-0.0), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.25), true),
        (json!(""), true),
        (json!("false"), true),
        (json!([]), true),
        (json!({}), true),
    ] {
        for expression in [
            "input ? true : false",
            "!!input",
            "input && true",
            "input || false",
        ] {
            let source = format!("function main(input){{return {expression};}}");
            let r = Runtime::new(compile(&source).unwrap(), "main", vec![value.clone()]).unwrap();
            assert_eq!(returned(r), json!(truth), "{expression}: {value}");
        }
    }
    assert_eq!(
        returned(runtime(
            "function main(){const items=[1];const result={};const obj={};if(items.length && !result.error && obj){return true;}return false;}"
        )),
        json!(true)
    );
    assert_eq!(
        returned(runtime(
            "function main(){var i=3;var count=0;while(i){i--;count++;}return count;}"
        )),
        json!(3)
    );
    assert!(
        matches!(runtime("function main(){assert(null,'missing');}").next(),State::Failed(e) if e["message"]=="missing")
    );
}

#[test]
fn old_operator_enabled_runtime_cannot_resume_pending_effects() {
    let mut r = runtime("function main(){io.write({path:'not-replayed',content:'x'});}");
    assert!(matches!(r.next(), State::Action(_)));
    r.program.version = 1;
    let mut restored: Runtime = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
    assert!(matches!(restored.next(),State::Failed(error) if error["required_version"]==VERSION));
}

#[test]
fn integer_literals_and_task_parameters_remain_json_integers() {
    let source = "function main(input){io.list({path:'.',max_depth:2,limit:input.limit});}";
    let mut r = Runtime::new(compile(source).unwrap(), "main", vec![json!({"limit":3})]).unwrap();
    let State::Action(action) = r.next() else {
        panic!()
    };
    let ActionKind::Tool { args, .. } = action.kind else {
        panic!()
    };
    assert_eq!(args["max_depth"].as_u64(), Some(2));
    assert_eq!(args["limit"].as_u64(), Some(3));
}

#[test]
fn programs_have_one_main_and_helpers_use_normal_returns() {
    assert!(compile("function prepare(){return 'ready';}").is_err());
    assert!(compile("function main(){} function main(){}").is_err());
    let mut r = runtime(
        "function prepare(){return {ready:true};} function execute(input){io.write({path:'done.txt',content:'done'});return input;} function main(){const prepared=prepare();const result=execute(prepared);return result;}",
    );
    assert!(
        matches!(r.next(), State::Action(_)),
        "prepare returning must resume main and run execute"
    );
    r.resume(json!({"changed":true}), false).unwrap();
    assert_eq!(returned(r), json!({"ready":true}));
}

#[test]
fn comment_helpers_bind_arguments_and_resume_mixed_tool_library_calls() {
    let source = r#"
function main(request) {
  const original = io.read({path: request.source});
  const saved = process(original.content, request);
  return {saved, original: original.content};
}
function process(content, request) {
  // Explicit tool calls and task calls share one sequence.
  return io.write({path: request.output, content: transform(content, request).content});
}
/** @virtual */
function transform(content, request) {
  // Transform content according to request; preserve the requested scope.
  /* Return {content: string} with the transformed text. */
}
"#;
    let mut r = Runtime::new(
        compile(source).unwrap(),
        "main",
        vec![json!({"source":"source.txt","output":"out.txt"})],
    )
    .unwrap();
    assert!(
        matches!(r.next(),State::Action(Action{kind:ActionKind::Tool{name,args},..}) if name=="read_file" && args["path"]=="source.txt")
    );
    r.resume(json!({"content":"SOURCE"}), false).unwrap();
    let State::Action(task) = r.next() else {
        panic!()
    };
    assert!(
        matches!(task.kind,ActionKind::Task{name,description,input} if name=="transform" && description.contains("Return {content: string}") && input==json!({"content":"SOURCE","request":{"source":"source.txt","output":"out.txt"}}))
    );
    let mut restored: Runtime = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
    restored
        .resume(json!({"content":"CHANGED"}), false)
        .unwrap();
    assert!(
        matches!(restored.next(),State::Action(Action{kind:ActionKind::Tool{name,args},..}) if name=="write_file" && args==json!({"path":"out.txt","content":"CHANGED"}))
    );
    restored.resume(json!({"path":"out.txt"}), false).unwrap();
    assert_eq!(
        returned(restored),
        json!({"saved":{"path":"out.txt"},"original":"SOURCE"})
    );
}

#[test]
fn comment_errors_propagate_and_nested_calls_choose_boolean_branches() {
    let mut r = runtime(
        "function check(){/* @virtual */ // Return {passed:boolean} after inspecting actual artifacts.\n} function main(){try{if(check().passed){return 'ok';}else{throw 'bad';}}catch(error){return {error};}}",
    );
    assert!(matches!(
        r.next(),
        State::Action(Action {
            kind: ActionKind::Task { .. },
            ..
        })
    ));
    r.resume(json!({"passed":false}), false).unwrap();
    assert_eq!(returned(r)["error"]["message"], json!("bad"));
    let mut r = runtime(
        "function work(){/* @virtual */ // Complete the assigned work.\n} function main(){try{return work();}catch(error){return error;}}",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"message":"cannot complete"}), true)
        .unwrap();
    assert_eq!(returned(r)["message"], json!("cannot complete"));
    for source in [
        "function main(){// only preparation\n}",
        "function main(){return blank();} function blank(){}",
        "function main(){const $temp1=1;return $temp1;}",
    ] {
        assert!(compile(source).is_err(), "{source}");
    }
    assert_eq!(
        returned(runtime("function main(){return '// this is data';}")),
        json!("// this is data")
    );
}

#[test]
fn task_refinement_has_no_new_main_and_ordinary_calls_use_frames() {
    let p = compile_body(
        "function phase(input){return helper(input);} function helper(input){return input;}",
        "phase",
    )
    .unwrap();
    let r = Runtime::task_program(p, "phase", vec![json!({"ok":true})]).unwrap();
    assert_eq!(returned(r), json!({"ok":true}));
    assert!(
        compile_body(
            "function main(){return null;} function phase(){return null;}",
            "phase"
        )
        .is_err()
    );
    assert!(compile_body("function phase(){// only restates the task\n}", "phase").is_err());
    assert!(compile("function main(){return tools.define_task_body({source:'bad'});}").is_err());
}

#[test]
fn local_result_arrays_collect_nested_task_returns_without_operators() {
    let source = "function main(){const rows=[];for(const item of [{n:1},{n:2}]){rows.push(process(item));}const length=rows.push({n:3});return {rows,length};} function process(item){return item;}";
    let r = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    let restored: Runtime = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
    assert_eq!(
        returned(restored),
        json!({"rows":[{"n":1},{"n":2},{"n":3}],"length":3})
    );
    assert!(matches!(
        runtime("function main(){const x={};x.push(1);return x;}").next(),
        State::Failed(_)
    ));
    assert_eq!(
        returned(runtime("function main(){const x=[];x.push(1+2);return x;}")),
        json!([3])
    );
}

#[test]
fn ordinary_frames_suspend_for_virtual_work_and_survive_restore() {
    let source = r#"
function main(){const outer='OUTER';const result=work('INPUT');return {result,outer};}
function work(input){const local='LOCAL';const answer=think(input);return {answer,local};}
/** @virtual */ function think(input){/* Return the inspected result. */}
"#;
    let mut run = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    let State::Action(action) = run.next() else {
        panic!("expected virtual task")
    };
    assert!(
        matches!(&action.kind, ActionKind::Call { name, args } if name == "think" && args == &vec![json!("INPUT")])
    );
    assert_eq!(run.named_bindings()["outer"], "OUTER");
    assert_eq!(run.named_bindings()["local"], "LOCAL");
    let mut restored: Runtime = serde_json::from_value(serde_json::to_value(run).unwrap()).unwrap();
    assert!(matches!(restored.next(), State::Action(ref pending) if pending.id == action.id));
    restored.resume(json!("DONE"), false).unwrap();
    assert_eq!(
        returned(restored),
        json!({"result":{"answer":"DONE","local":"LOCAL"},"outer":"OUTER"})
    );
}

#[test]
fn ordinary_frames_restore_tool_receipts_and_propagate_errors_to_caller() {
    let source = r#"
function main(){try{return helper();}catch(problem){return {error:problem.message};}}
function helper(){return io.write({path:'out.txt',content:'hello'});}
"#;
    let mut run = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    let State::Action(action) = run.next() else {
        panic!("expected write")
    };
    assert!(matches!(&action.kind, ActionKind::Tool { name, .. } if name == "write_file"));
    let saved = serde_json::to_value(run).unwrap();
    let mut success: Runtime = serde_json::from_value(saved.clone()).unwrap();
    assert!(matches!(success.next(), State::Action(ref pending) if pending.id == action.id));
    success.resume(json!({"path":"out.txt"}), false).unwrap();
    assert_eq!(returned(success), json!({"path":"out.txt"}));
    let mut failure: Runtime = serde_json::from_value(saved).unwrap();
    failure
        .resume(json!({"error":"write failed"}), true)
        .unwrap();
    assert_eq!(returned(failure), json!({"error":"write failed"}));
}

#[test]
fn inline_caller_bindings_preserve_inner_shadowing() {
    let source = r#"
function main(){const shared='OUTER';const result=helper();return {shared,result};}
function helper(){const shared='INNER';const result=think();return {shared,result};}
/** @virtual */ function think(){/* Return a result. */}
"#;
    let mut run = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    assert!(matches!(
        run.next(),
        State::Action(Action {
            kind: ActionKind::Call { .. },
            ..
        })
    ));
    assert_eq!(run.named_bindings()["shared"], "INNER");
    run.resume(json!("OK"), false).unwrap();
    assert_eq!(
        returned(run),
        json!({"shared":"OUTER","result":{"shared":"INNER","result":"OK"}})
    );
}

#[test]
fn version_eight_rejects_javascript_extensions_at_every_source_entry() {
    // Valid-looking code can precede invalid syntax: compilation must reject
    // the entire source, including unused functions, before any effects run.
    for fragment in [
        "function* hidden(){return 1;}",
        "async function hidden(){return 1;}",
        "function hidden(){return 1;}", // nested functions
        "const fn = function(){return 1;};",
        "const fn = () => 1;",
        "yield io.read({});",
        "yield* rows;",
        "await io.read({});",
        "call(io.read,{});",
        "call('read_file',{});",
        "ai({instruction:'work'});",
        "const typed: string = 'x';",
        "const typed = value as number;",
        "const typed = <number>value;",
        "const typed = value!;",
        "for(const key in data){}",
        "switch(value){case 1:break;}",
        "try{}catch(error){}finally{}",
        "const copy = {...data};",
        "const copy = [...rows];",
        "helper(...rows);",
        "const missing = data?.value;",
        "const bits = value | 1;",
        "const bits = ~value;",
        "const power = value ** 2;",
        "const kind = typeof value;",
        "void value;",
        "delete data.value;",
        "data.value = 2;",
        "const regex = /x/;",
        "const bigint = 1n;",
        "const infinity = 1e999;",
        "const infinity = value + 1e999;",
        "const hidden = data.check(/x/);",
        "const hidden = data.check(1n);",
        "const value2 = new Date();",
        "const value2 = this.value;",
        "class Thing{}",
        "const runtime = Promise.resolve(value);",
        "const runtime = process.exit();",
        "const runtime = data.check({run(){return 1;}});",
        "const runtime = data.check([,]);",
        "const runtime = data.check({[value]:1});",
        "const runtime = value instanceof data;",
        "const runtime = 'value' in data;",
        "return import('module');",
    ] {
        let body = format!(
            "const data={{value:1}}; const rows=[]; let value=1; io.write({{path:'never-written'}});\n{fragment}"
        );
        for (source, mode) in [
            (
                format!("function main(){{{body}}} function helper(input){{return input;}}"),
                0,
            ),
            (
                format!("function work(){{{body}}} function helper(input){{return input;}}"),
                1,
            ),
            (
                format!("function work(){{{body}}} function helper(input){{return input;}}"),
                2,
            ),
        ] {
            let result = match mode {
                0 => compile(&source),
                1 => compile_body(&source, "work"),
                _ => compile_entry(&source, "work"),
            };
            let error = result.expect_err(&format!("must reject {fragment}, entry {mode}"));
            assert!(
                error.to_string().starts_with("TS_PROGRAM "),
                "{fragment}: {error}"
            );
        }
    }
    for source in [
        "function* main(){return 1;}",
        "function main(){return 1;} function* unused(){return 1;}",
        "function main(){return 1;} async function unused(){return 1;}",
        "import {tools} from '@agent/runtime'; function main(){return 1;}",
        "export function main(){return 1;}",
        "type Result = string; function main(){return 1;}",
        "interface Result{} function main(){return 1;}",
        "function main(input: string){return input;}",
        "function main(){return 1;} function helper(input=1){return input;}",
        "function main(){return 1;} function helper(...inputs){return inputs;}",
    ] {
        assert!(compile(source).is_err(), "{source}");
    }
}

#[test]
fn static_call_contracts_are_checked_before_execution() {
    for source in [
        "function main(){return JSON.trim();}",
        "function main(){return JSON.includes('x');}",
        "function main(){return JSON.stringify(1,2);}",
        "function main(){return 0xffffffffffffffff;}",
        "function main(){io.write({path:'never'}); return helper();} function helper(input){return input;}",
        "function main(){io.write({path:'never'}); return fork(helper,1,2);} function helper(input){return input;}",
        "function main(){return main();}",
        "function main(a,b){return a;}",
        "function main(){return fork(main);}",
        "function main(){return io.read({},{});}",
        "function main(){return tools[name]({});}",
    ] {
        let error = compile(source).unwrap_err().to_string();
        assert!(error.starts_with("TS_PROGRAM "), "{error}");
    }
}

#[test]
fn json_fields_and_elements_capture_values_before_later_array_mutation() {
    for (source, expected) in [
        (
            "function main(){const rows=[];return {before:rows,length:rows.push('item'),after:rows};}",
            json!({"before":[],"length":1,"after":["item"]}),
        ),
        (
            "function main(){const rows=[];return [rows,rows.push('item'),rows];}",
            json!([[], 1, ["item"]]),
        ),
        (
            "function main(){const rows=['old'];return rows[choose(rows.push('new'))];} function choose(length){return 1;}",
            json!(null),
        ),
        (
            "function main(){let result='before';result='after';return result;}",
            json!("after"),
        ),
    ] {
        assert_eq!(returned(runtime(source)), expected, "{source}");
    }
}

#[test]
fn all_older_program_versions_reject_pending_effects_without_replaying() {
    for version in 1..VERSION {
        let mut r = runtime("function main(){io.write({path:'never-replayed'});}");
        assert!(matches!(r.next(), State::Action(_)));
        r.program.version = version;
        let mut restored: Runtime =
            serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        assert!(matches!(restored.next(), State::Failed(_)));
        assert!(restored.pending.is_none());
    }
}

#[test]
fn embedded_skill_and_tool_contracts_advertise_the_same_source_whitelist() {
    for text in [
        SKILL.to_owned(),
        tool()["function"]["description"]
            .as_str()
            .unwrap()
            .to_owned(),
    ] {
        for required in [
            "function*",
            "yield",
            "Promise.all",
            "call()/ai()",
            "if/else",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
    }
}

#[test]
fn recorded_twenty_page_parallel_example_compiles_with_declared_host_tools() {
    let source = include_str!("../../skills/task-program/examples/parallel-twenty-html.js");
    let program =
        compile(source).expect("recorded example must compile against its declared tools");
    assert!(program.functions["designStyles"].virtual_task);
    assert!(program.functions["generatePageBodies"].virtual_task);
    assert!(!program.functions["writePages"].virtual_task);
}

#[test]
fn published_flow_examples_compile_with_the_fixed_whitelist() {
    for text in [SKILL, include_str!("../../docs/agent-program.md")] {
        let mut examples = 0;
        let text = text.replace("\r\n", "\n");
        for block in text.split("```js\n").skip(1) {
            let source = block.split("```").next().unwrap();
            let result = if source.contains("function main(") {
                compile(source)
            } else {
                compile_body(source, "process")
            };
            result.unwrap_or_else(|error| panic!("published example failed: {error}\n{source}"));
            examples += 1;
        }
        assert!(examples >= 2);
    }
}

// Run the published files themselves. Tool/AI observations are controlled here;
// this verifies flow semantics, not real model quality or wall-clock parallelism.
#[derive(Default)]
struct TutorialRun {
    files: BTreeMap<String, String>,
    events: Vec<String>,
    workers: BTreeMap<String, Runtime>,
    reject_review: bool,
}

impl TutorialRun {
    fn run(&mut self, mut run: Runtime) -> Result<Value, Value> {
        loop {
            let action = match run.next() {
                State::Returned(value) => return Ok(value),
                State::Failed(error) => return Err(error),
                State::Action(action) => action,
            };
            let result = match action.kind {
                ActionKind::ParallelReads { .. } => panic!("tutorial does not use parallel reads"),
                ActionKind::Call { name, args } => {
                    self.run(Runtime::task_program(run.program.clone(), &name, args).unwrap())
                }
                ActionKind::Tool { name, args } => {
                    let path = args["path"].as_str().unwrap();
                    self.events.push(format!("{name}:{path}"));
                    match name.as_str() {
                        "read_file" => Ok(json!({"content":self.files[path]})),
                        "write_file" => {
                            self.files
                                .insert(path.into(), args["content"].as_str().unwrap().into());
                            Ok(json!({"changed":true}))
                        }
                        _ => panic!("unexpected tutorial tool {name}"),
                    }
                }
                ActionKind::Task { name, input, .. } => {
                    self.events.push(name.clone());
                    match name.as_str() {
                        "compose" => {
                            assert!(input["request"]["topic"].is_string());
                            Ok(json!({"content":"<html><body>串行示例</body></html>"}))
                        }
                        "designPage" => {
                            assert_eq!(input["shared"]["source_content"], "SOURCE");
                            assert!(input["shared"]["requirements"].is_string());
                            let title = input["page"]["title"].as_str().unwrap();
                            Ok(json!({"content":format!("<html><body>{title}</body></html>")}))
                        }
                        "inspect" | "inspectPage" => {
                            let content = input["content"].as_str().unwrap();
                            assert!(
                                self.files.values().any(|value| value == content),
                                "must inspect actual saved content"
                            );
                            if name == "inspectPage" {
                                let path = input["page"]["path"].as_str().unwrap();
                                assert_eq!(self.files[path], content);
                                assert_eq!(input["shared"]["source_content"], "SOURCE");
                                assert!(content.contains(input["page"]["title"].as_str().unwrap()));
                            }
                            Ok(
                                json!({"passed":!self.reject_review,"issues":[],"evidence":[content]}),
                            )
                        }
                        _ => panic!("unexpected tutorial task {name}"),
                    }
                }
                ActionKind::Fork { entry, args } => {
                    assert_eq!(entry, "makePages");
                    assert_eq!(
                        args.len(),
                        2,
                        "shared and pages are separate positional arguments"
                    );
                    let id = format!("worker-{}", self.workers.len());
                    self.events
                        .push(format!("fork:{}", args[1].as_array().unwrap().len()));
                    self.workers.insert(
                        id.clone(),
                        Runtime::task_program(run.program.clone(), &entry, args).unwrap(),
                    );
                    Ok(
                        json!({"agent_id":id,"workspace":{"mode":"shared","integration_required":false}}),
                    )
                }
                ActionKind::Join { ids } => {
                    self.events.push("join".into());
                    let mut outputs = Vec::new();
                    for id in ids {
                        let worker = self.workers.remove(&id).unwrap();
                        outputs.push(self.run(worker)?);
                    }
                    Ok(Value::Array(outputs))
                }
            };
            let failed = result.is_err();
            run.resume(result.unwrap_or_else(|error| error), failed)
                .unwrap();
        }
    }
}

fn tutorial(source: &str, input: &str) -> Runtime {
    assert!(
        SKILL.contains(source.trim()),
        "the AI must receive the same example that is tested"
    );
    Runtime::task_program(
        compile(source).unwrap(),
        "main",
        vec![serde_json::from_str(input).unwrap()],
    )
    .unwrap()
}

#[test]
fn documented_serial_example_writes_reads_and_checks_without_fork() {
    let mut harness = TutorialRun::default();
    let output = harness
        .run(tutorial(
            include_str!("../../skills/task-program/examples/serial.js"),
            include_str!("../../skills/task-program/examples/serial.input.json"),
        ))
        .unwrap();
    assert_eq!(output["path"], "agent-overview.html");
    assert_eq!(output["review"]["passed"], true);
    assert_eq!(
        harness.events,
        vec![
            "compose",
            "write_file:agent-overview.html",
            "read_file:agent-overview.html",
            "inspect"
        ]
    );
    assert!(harness.workers.is_empty());
}

#[test]
fn documented_parallel_example_forks_disjoint_parts_then_checks_all_five() {
    let mut harness = TutorialRun::default();
    harness
        .files
        .insert("docs/generic-agent-design.md".into(), "SOURCE".into());
    let output = harness
        .run(tutorial(
            include_str!("../../skills/task-program/examples/parallel-five-pages.js"),
            include_str!("../../skills/task-program/examples/parallel-five-pages.input.json"),
        ))
        .unwrap();
    assert_eq!(
        &harness.events[..4],
        [
            "read_file:docs/generic-agent-design.md",
            "fork:3",
            "fork:2",
            "join"
        ]
    );
    assert_eq!(output["files"].as_array().unwrap().len(), 5);
    assert_eq!(output["reviews"].as_array().unwrap().len(), 5);
    assert_eq!(harness.files.len(), 6); // one source plus five distinct outputs
    assert_eq!(
        harness.events.iter().filter(|e| *e == "designPage").count(),
        5
    );
    assert_eq!(
        harness
            .events
            .iter()
            .filter(|e| *e == "inspectPage")
            .count(),
        5
    );
    for file in output["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        for operation in ["write_file", "read_file"] {
            assert_eq!(
                harness
                    .events
                    .iter()
                    .filter(|e| **e == format!("{operation}:{path}"))
                    .count(),
                1
            );
        }
    }
    assert!(harness.workers.is_empty());
}

#[test]
fn documented_examples_fail_instead_of_delivering_failed_reviews() {
    for (source, input) in [
        (
            include_str!("../../skills/task-program/examples/serial.js"),
            include_str!("../../skills/task-program/examples/serial.input.json"),
        ),
        (
            include_str!("../../skills/task-program/examples/parallel-five-pages.js"),
            include_str!("../../skills/task-program/examples/parallel-five-pages.input.json"),
        ),
    ] {
        let mut harness = TutorialRun {
            reject_review: true,
            ..Default::default()
        };
        harness
            .files
            .insert("docs/generic-agent-design.md".into(), "SOURCE".into());
        let error = harness.run(tutorial(source, input)).unwrap_err();
        assert!(error["message"].as_str().unwrap().contains("验收"));
        assert_eq!(
            harness
                .events
                .iter()
                .filter(|e| matches!(e.as_str(), "inspect" | "inspectPage"))
                .count(),
            1
        );
    }
}

#[test]
fn common_loops_updates_templates_and_assignment_order_are_deterministic() {
    for (source, expected) in [
        (
            "function main(){var sum=0;for(var i=0;i<5;i++){if(i===1)continue;if(i===4)break;sum+=i;}return {sum,i};}",
            json!({"sum":5,"i":4}),
        ),
        (
            "function main(){var i=0;var sum=0;while(i<4){i++;if(i===2)continue;sum+=i;}return sum;}",
            json!(8),
        ),
        (
            "function main(){var i=0;do{i++;if(i<3)continue;}while(i<3);return i;}",
            json!(3),
        ),
        (
            "function main(){var count=0;for(var i=0;i<3;i++){for(var j=0;j<3;j++){if(j===1)break;count++;}}return count;}",
            json!(3),
        ),
        (
            "function main(){var rows=[];for(var item of [1,2,3]){if(item===2)continue;rows.push(item);}return rows;}",
            json!([1, 3]),
        ),
        (
            "function main(){var i=1;var rows=[i++,++i,i--,--i];return {rows,i};}",
            json!({"rows":[1,3,3,1],"i":1}),
        ),
        (
            "function main(){var x='before';return {first:x,second:(x='after'),third:x};}",
            json!({"first":"before","second":"after","third":"after"}),
        ),
        (
            "function main(){var x=1;return `${x++}-${x}-${x+=2}`;}",
            json!("1-2-4"),
        ),
        (
            "function main(){var value;var value=2;var value;return value;}",
            json!(2),
        ),
        (
            "function main(){var value=0;for(;;){value++;if(value===3)break;}return value;}",
            json!(3),
        ),
    ] {
        for _ in 0..3 {
            assert_eq!(returned(runtime(source)), expected, "{source}");
        }
    }
}

#[test]
fn lazy_expressions_execute_only_selected_effects_once() {
    for source in [
        "function main(){return false && test.never({});}",
        "function main(){return true || test.never({});}",
        "function main(){return true ? 'selected' : test.never({});}",
        "function main(){return false ? test.never({}) : 'selected';}",
        "function main(){return 'selected' ?? test.never({});}",
    ] {
        assert!(
            matches!(runtime(source).next(), State::Returned(_)),
            "{source}"
        );
    }
    for expression in [
        "true && test.selected({})",
        "false || test.selected({})",
        "null ?? test.selected({})",
        "true ? test.selected({}) : test.never({})",
    ] {
        let mut r = runtime(&format!("function main(){{return {expression};}}"));
        let State::Action(action) = r.next() else {
            panic!("{expression}")
        };
        assert!(matches!(action.kind,ActionKind::Tool{name,..} if name=="selected"));
        let mut restored: Runtime =
            serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        restored.resume(json!(true), false).unwrap();
        assert_eq!(returned(restored), json!(true));
    }
    // Earlier arithmetic failures must precede later effects.
    assert!(matches!(
        runtime("function main(){return (1/0)+test.never({});}").next(),
        State::Failed(_)
    ));
}

#[test]
fn loop_jumps_unwind_only_the_handlers_they_leave() {
    for source in [
        "function main(){try{for(var i=0;i<2;i++){try{if(i===0)continue;break;}catch(inner){return 'wrong';}}throw 'outer';}catch(outer){return outer.message;}}",
        "function main(){try{while(true){try{break;}catch(inner){return 'wrong';}}throw 'outer';}catch(outer){return outer.message;}}",
        "function main(){try{for(var item of [1,2]){try{if(item===1)continue;break;}catch(inner){return 'wrong';}}throw 'outer';}catch(outer){return outer.message;}}",
        "function main(){try{do{try{break;}catch(inner){return 'wrong';}}while(true);throw 'outer';}catch(outer){return outer.message;}}",
    ] {
        assert_eq!(returned(runtime(source)), json!("outer"), "{source}");
    }
    assert!(
        matches!(runtime("function main(){while(true){try{break;}catch(inner){return 'leaked';}}throw 'outside';}").next(),State::Failed(e) if e["message"]=="outside")
    );
    assert!(
        matches!(runtime("function main(){try{while(true){}}catch(error){return 'cannot hide exhaustion';}}").next(),State::Failed(e) if e["message"]=="program instruction budget exhausted")
    );
}

#[test]
fn suspended_loops_do_not_repeat_updates_or_effects_after_restore() {
    let mut r = runtime(
        "function main(){var rows=[];for(var i=0;i<3;i++){var next=i+1;const receipt=test.record({i,next});rows.push(receipt);}return {rows,i};}",
    );
    for i in 0..3 {
        let State::Action(action) = r.next() else {
            panic!()
        };
        assert!(matches!(action.kind,ActionKind::Tool{args,..} if args==json!({"i":i,"next":i+1})));
        r = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        assert!(matches!(r.next(),State::Action(ref again) if again.id==action.id));
        r.resume(json!(i), false).unwrap();
    }
    assert_eq!(returned(r), json!({"rows":[0,1,2],"i":3}));
}

#[test]
fn no_implicit_semantic_effects_are_emitted_or_executed() {
    let program =
        compile("function main(){var total=0;for(var i=0;i<4;i++){total+=i;}return `${total}`;}")
            .unwrap();
    assert!(
        program
            .functions
            .values()
            .flat_map(|f| &f.code)
            .all(|i| !matches!(i.op, Op::Effect(_, _)))
    );
    let mut runtime = Runtime::task_program(program, "main", vec![]).unwrap();
    assert!(matches!(runtime.next(),State::Returned(v) if v=="6"));
}

#[test]
fn library_calls_compile_to_exact_tools_and_keep_json_arguments() {
    let mut bindings = libraries::builtin();
    bindings.insert(
        "ppt".into(),
        BTreeMap::from([("get_design".into(), "presentation__get_design".into())]),
    );
    for (source, name, args) in [
        (
            "function main(){return io.read({path:'source.txt'});}",
            "read_file",
            json!({"path":"source.txt"}),
        ),
        (
            "function main(){return ppt.get_design({project:'actual'});}",
            "presentation__get_design",
            json!({"project":"actual"}),
        ),
    ] {
        let mut r = Runtime::new(
            compile_with_libraries(source, bindings.clone()).unwrap(),
            "main",
            vec![],
        )
        .unwrap();
        let State::Action(action) = r.next() else {
            panic!()
        };
        assert!(
            matches!(action.kind,ActionKind::Tool{name:ref target,args:ref actual} if target==name && actual==&args)
        );
        let mut restored: Runtime =
            serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        restored.resume(json!({"actual":"receipt"}), false).unwrap();
        assert_eq!(returned(restored), json!({"actual":"receipt"}));
    }
    for source in [
        "function main(){io.write({path:'never'});return io.unknown({});}",
        "function main(){return ppt.get_design({});}",
        "function main(){return io['read']({});}",
        "function main(){const io={};return io;}",
    ] {
        assert!(compile(source).is_err(), "{source}");
    }
}

#[test]
fn library_catalog_uses_real_ownership_and_omits_unavailable_or_ambiguous_names() {
    let definitions = ["read_file", "ppt_master__get_design", "other__inspect"]
        .map(|name| json!({"function":{"name":name}}));
    let packages = json!([
        {"id":"ppt-master","library":"ppt","enabled":true,"tools":["ppt_master__get_design","ppt_master__missing"]},
        {"id":"other","enabled":false,"tools":["other__inspect"]},
    ]);
    let catalog = libraries::catalog(&definitions, &packages);
    assert_eq!(catalog["io"]["read"], "read_file");
    assert!(!catalog["io"].contains_key("write"));
    assert_eq!(catalog["ppt"]["get_design"], "ppt_master__get_design");
    assert!(!catalog["ppt"].contains_key("missing"));
    assert!(!catalog.contains_key("other"));
    let collision = json!([
        {"id":"a-b","enabled":true,"tools":["read_file"]},
        {"id":"a_b","enabled":true,"tools":["other__inspect"]},
    ]);
    assert!(!libraries::catalog(&definitions, &collision).contains_key("a_b"));
}

#[test]
fn numeric_and_null_short_circuits_preserve_c_boolean_results_and_skip_effects() {
    for (expression, expected) in [
        ("0 && test.never({})", json!(false)),
        ("null && test.never({})", json!(false)),
        ("{} || test.never({})", json!(true)),
        ("1 || test.never({})", json!(true)),
        ("0 ?? test.never({})", json!(0)),
        ("!({}).error", json!(true)),
        ("0 || 7", json!(true)),
        ("1 && {}", json!(true)),
    ] {
        assert_eq!(
            returned(runtime(&format!("function main(){{return {expression};}}"))),
            expected,
            "{expression}"
        );
    }
    let mut r = runtime("function main(){return 0 || io.read({path:'selected'});}");
    assert!(
        matches!(r.next(),State::Action(Action{kind:ActionKind::Tool{name,..},..}) if name=="read_file")
    );
    r.resume(json!({}), false).unwrap();
    assert_eq!(returned(r), json!(true));
}

#[test]
fn raw_tool_spellings_are_rejected_before_any_effect() {
    for body in [
        "return tools.read_file({path:'x'});",
        "return tools['read_file']({});",
        "return invoke_library({method:'io.read',args:{}});",
    ] {
        for source in [
            format!("function main(){{io.write({{path:'never'}});{body}}}"),
            format!("function main(){{return null;}} function unused(){{{body}}}"),
        ] {
            assert!(super::compile(&source).is_err(), "{source}");
        }
    }
    assert!(
        super::tool()["function"]["parameters"]["properties"]
            .get("libraries")
            .is_none()
    );
    assert!(
        super::body_tool()["function"]["parameters"]["properties"]
            .get("libraries")
            .is_none()
    );
}

#[test]
fn explicit_virtual_contract_runs_ai_and_preserves_result_across_restore() {
    let source = r#"
function main(request) { return inspect(request); }
/**
 * @virtual
 * Read actual evidence for request; return {passed, evidence}.
 */
function inspect(request) {}
"#;
    let program = compile(source).unwrap();
    assert!(program.functions["inspect"].virtual_task);
    assert!(!program.functions["main"].virtual_task);
    let mut run = Runtime::new(program, "main", vec![json!({"path":"real.txt"})]).unwrap();
    let State::Action(action) = run.next() else {
        panic!("must enter AI work")
    };
    assert!(
        matches!(action.kind, ActionKind::Task { ref name, ref description, ref input }
        if name == "inspect" && description.contains("actual evidence") && !description.contains("@virtual") && input["request"]["path"] == "real.txt")
    );
    let mut restored: Runtime =
        serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
    let State::Action(pending) = restored.next() else {
        panic!()
    };
    assert_eq!(pending.id, action.id);
    let actual = json!({"passed":true,"evidence":[{"path":"real.txt","content":"actual"}]});
    restored.resume(actual.clone(), false).unwrap();
    assert_eq!(returned(restored), actual);
}

#[test]
fn virtual_placeholders_and_ambiguous_empty_helpers_fail_before_effects() {
    for declaration in [
        "/** @virtual Create actual outputs; return {files}. */ function work(){return {status:'completed'};}",
        "function work(){/* @virtual Create actual outputs; return {files}. */ io.write({path:'bad'});}",
        "/** @virtual */ function work(){}",
        "function work(){// Create outputs and return their paths.\n}",
    ] {
        let error = compile(&format!(
            "function main(){{io.write({{path:'never'}});return work();}} {declaration}"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("@virtual"), "{error}");
        assert!(error.starts_with("TS_PROGRAM "), "{error}");
    }
    assert!(compile("/** @virtual Execute the main task. */ function main(){}").is_err());
}

#[test]
fn virtual_tags_are_local_and_concrete_refinement_can_call_another_virtual_task() {
    let source = r#"
function main() {return concrete();}
/** @virtual Read the real evidence; return a JSON result. */
function inspect() {}
function concrete() { /* A normal code comment. */ return {status:'computed'}; }
"#;
    assert_eq!(returned(runtime(source)), json!({"status":"computed"}));
    // A word that merely starts with the tag is not a declaration marker.
    assert_eq!(
        returned(runtime(
            "function main(){/* @virtualized is ordinary prose. */return 3;}"
        )),
        json!(3)
    );
    let concrete = compile_body(
        r#"
function phase(input) {return inspect(input);}
/** @virtual Inspect actual input; return {passed,evidence}. */
function inspect(input) {}
"#,
        "phase",
    )
    .unwrap();
    assert!(!concrete.functions["phase"].virtual_task);
    assert!(concrete.functions["inspect"].virtual_task);
    assert!(
        compile_body(
            "/** @virtual Restate work and return results. */ function phase(){}",
            "phase"
        )
        .is_err()
    );
}

#[test]
fn string_concatenation_accepts_primitives_and_preserves_numeric_addition_order() {
    for (expression, expected) in [
        ("'字符数=' + 8000", json!("字符数=8000")),
        ("8000 + '字'", json!("8000字")),
        ("'数值=' + -2.5", json!("数值=-2.5")),
        ("'状态=' + true + '/' + null", json!("状态=true/null")),
        ("1 + 2 + '项'", json!("3项")),
        ("'项' + 1 + 2", json!("项12")),
        (
            "'大整数=' + 9007199254740993",
            json!("大整数=9007199254740993"),
        ),
        (
            "18446744073709551615 + '字'",
            json!("18446744073709551615字"),
        ),
    ] {
        assert_eq!(
            returned(runtime(&format!("function main(){{return {expression};}}"))),
            expected
        );
    }
    assert_eq!(
        returned(runtime(
            "function main(){let label='数量=';label+=2;let total=3;total+=2;return {label,total};}"
        )),
        json!({"label":"数量=2","total":5})
    );
    for expression in ["'x'+{}", "[]+'x'", "true+1", "'3'-1", "'3'<4"] {
        assert!(
            matches!(
                runtime(&format!("function main(){{return {expression};}}")).next(),
                State::Failed(_)
            ),
            "{expression}"
        );
    }
}

#[test]
fn captured_receipt_concatenation_allows_both_writes_and_resumes_without_replay() {
    let source = include_str!("fixtures/receipt-string-number.js");
    let program = compile_body(source, "build_pages").unwrap();
    let mut run = Runtime::new(
        program,
        "build_pages",
        vec![json!({"assignment":{"owner":"worker"},"shared":{}})],
    )
    .unwrap();
    assert!(
        matches!(run.next(),State::Action(Action{kind:ActionKind::Task{name,..},..}) if name=="create_page_content")
    );
    run.resume(json!({"pages":[{"path":"one.html","title":"One","content":"first"},{"path":"two.html","title":"Two","content":"second"}]}),false).unwrap();
    for path in ["one.html", "two.html"] {
        let State::Action(action) = run.next() else {
            panic!("both writes must be reached")
        };
        assert!(
            matches!(action.kind,ActionKind::Tool{name,args} if name=="write_file" && args["path"]==path)
        );
        run.resume(json!({"path":path,"changed":true}), false)
            .unwrap();
        // The second iteration must preserve the first receipt after recovery.
        run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
    }
    let result = returned(run);
    assert_eq!(result["files"].as_array().unwrap().len(), 2);
    assert!(
        result["files"][0]["evidence"]
            .as_str()
            .unwrap()
            .ends_with("字符数=5")
    );
    assert!(
        result["files"][1]["evidence"]
            .as_str()
            .unwrap()
            .ends_with("字符数=6")
    );
}

#[test]
fn caller_can_reject_an_inspection_result_for_a_different_input() {
    let source = "function main(){const subject={id:'correct'};const result=inspect(subject);assert(result.id===subject.id,'result does not match its input');return result;} /** @virtual Return the inspected id and conclusion. */ function inspect(subject){}";
    let mut run = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    assert!(matches!(run.next(), State::Action(_)));
    run.resume(json!({"id":"other","passed":true}), false)
        .unwrap();
    assert!(
        matches!(run.next(), State::Failed(error) if error["message"] == "result does not match its input")
    );
}

#[test]
fn caller_visible_variables_are_captured_only_when_the_function_uses_them() {
    let source = "function main(){const wanted='YES';const unused='NO';return helper();} function helper(){return wanted;}";
    let program = compile(source).unwrap();
    assert_eq!(
        program.functions["helper"].captures,
        ["wanted".to_owned()].into_iter().collect()
    );
    assert_eq!(
        returned(Runtime::new(program, "main", vec![]).unwrap()),
        json!("YES")
    );
    let missing = "function main(){return helper();} function other(){const wanted='wrong scope';return wanted;} function helper(){return wanted;}";
    let mut run = Runtime::new(compile(missing).unwrap(), "main", vec![]).unwrap();
    assert!(
        matches!(run.next(), State::Failed(e) if e["message"].as_str().unwrap().contains("not visible"))
    );
}

#[test]
fn source_variables_remain_visible_until_their_function_returns() {
    let source = "function main(){const doc=io.read({path:'large.txt'});const count=measure(doc.content);io.write({path:'count.txt',content:`${count}`});return count;} function measure(text){return text.length;}";
    let mut runtime = Runtime::new(compile(source).unwrap(), "main", vec![]).unwrap();
    let State::Action(_read) = runtime.next() else {
        panic!("read required")
    };
    let payload = "DEAD_INPUT_".repeat(6000);
    runtime.resume(json!({"content":payload}), false).unwrap();
    let State::Action(_write) = runtime.next() else {
        panic!("write required")
    };
    assert_eq!(runtime.named_bindings()["doc"]["content"], payload);
    let saved = serde_json::to_string(&runtime).unwrap();
    assert!(
        saved.contains("DEAD_INPUT_"),
        "source variables remain available to inherited AI scopes"
    );
    let mut restored: Runtime = serde_json::from_str(&saved).unwrap();
    restored.resume(json!({"saved":true}), false).unwrap();
    assert!(matches!(restored.next(),State::Returned(value) if value==payload.chars().count()));
}
