use super::*;
use runtime::ActionKind;

// Test-only effect targets, declared just like plugin methods.
fn compile(source: &str) -> anyhow::Result<Program> {
    let mut bindings=libraries::builtin();
    bindings.insert("test".into(),["record","never","selected"].into_iter().map(|n|(n.into(),n.into())).collect());
    compile_with_libraries(source,bindings)
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
    let mut r = runtime(
        "function main(){ io.read({path:'missing'}); io.write({path:'bad'}); }",
    );
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
    assert!(compile(include_str!("fixtures/two-workers.ts")).is_err());
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
    let source = "function main(要求){let 产物={path:'artifact.html'};const check=检查(产物,要求);if(!check.passed){产物={path:'repaired.html'};}return 产物;} function 检查(产物,要求){// Inspect real artifacts; return {passed:boolean,evidence:[...]}.\n}";
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
        assert!(action.semantic_boolean.is_none());
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
        "1+'x'",
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
        "function main(){const ok=check();if(ok){return 'yes';}else{return 'no';}} function check(){// Return a boolean conclusion from actual evidence.\n}",
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
        "function check(){// Return {passed:boolean} after inspecting actual artifacts.\n} function main(){try{if(check().passed){return 'ok';}else{throw 'bad';}}catch(error){return {error};}}",
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
        "function work(){// Complete the assigned work.\n} function main(){try{return work();}catch(error){return error;}}",
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
fn task_refinement_has_no_new_main_and_scoped_calls_are_external_tasks() {
    let p = compile_body(
        "function phase(input){return helper(input);} function helper(input){return input;}",
        "phase",
    )
    .unwrap();
    let mut r = Runtime::task_program(p, "phase", vec![json!({"ok":true})]).unwrap();
    assert!(
        matches!(r.next(),State::Action(Action{kind:ActionKind::Call{name,args},..}) if name=="helper" && args==vec![json!({"ok":true})])
    );
    r.resume(json!("READY"), false).unwrap();
    assert_eq!(returned(r), json!("READY"));
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
    let mut r = Runtime::task_program(compile(source).unwrap(), "main", vec![]).unwrap();
    for n in [1, 2] {
        assert!(
            matches!(r.next(),State::Action(Action{kind:ActionKind::Call{name,args},..}) if name=="process" && args[0]["n"]==n)
        );
        r.resume(json!({"n":n}), false).unwrap();
    }
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
        "async function main(){return 1;}",
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
            "async/await",
            "call()/ai()",
            "if/else",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
    }
}

#[test]
fn published_flow_examples_compile_with_the_fixed_whitelist() {
    for text in [SKILL, include_str!("../../docs/agent-program.md")] {
        let mut examples = 0;
        let text=text.replace("\r\n","\n");
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
        !program
            .functions
            .values()
            .flat_map(|f| &f.code)
            .any(|i| matches!(i.op, Op::Effect(_, Effect::Semantic { .. } | Effect::Ai(_))))
    );
    let mut runtime = Runtime::task_program(program, "main", vec![]).unwrap();
    assert!(matches!(runtime.next(),State::Returned(v) if v=="6"));
}

#[test]
fn library_calls_compile_to_exact_tools_and_keep_json_arguments() {
    let mut bindings=libraries::builtin();
    bindings.insert("ppt".into(),BTreeMap::from([("get_design".into(),"presentation__get_design".into())]));
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
    assert_eq!(
        catalog["ppt"]["get_design"],
        "ppt_master__get_design"
    );
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
    for body in ["return tools.read_file({path:'x'});", "return tools['read_file']({});", "return invoke_library({method:'io.read',args:{}});"] {
        for source in [format!("function main(){{io.write({{path:'never'}});{body}}}"),format!("function main(){{return null;}} function unused(){{{body}}}")] {assert!(super::compile(&source).is_err(),"{source}");}
    }
    assert!(super::tool()["function"]["parameters"]["properties"].get("libraries").is_none());
    assert!(super::body_tool()["function"]["parameters"]["properties"].get("libraries").is_none());
}
