use super::*;
use runtime::ActionKind;

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
import {call, tools} from '@agent/runtime';
type Item = {ready:boolean};
function* echo(input: Item) {return input;}
function* main() {
  const result = yield call(echo, {ready:true, rows:[{n:2},{n:3}]});
  for (const item of result.rows) {yield call(tools.record, {item});}
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
        "function* main(){ const receipt = yield call(tools.write_file,{path:'a.txt',content:'x'}); return receipt.version; }",
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
        "function* child(){ yield call(tools.read_file,{path:'missing'}); yield call(tools.write_file,{path:'bad'}); } function* main(){ try { yield call(child); } catch(error) { return error.message; } }",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"message":"not found"}), true).unwrap();
    assert_eq!(returned(r), json!("not found"));
    let mut r = runtime(
        "function* main(){ yield call(tools.read_file,{path:'missing'}); yield call(tools.write_file,{path:'bad'}); }",
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
        "function* worker(input){return input;} function* main(){ const a=yield fork(worker,{n:1}); const b=yield fork(worker,{n:2}); const [x,y]=yield join(a,b); return {x,y}; }",
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
        "function* main(){yield call(tools.write_file,{path:'bad'});\n fetch('https://example.com');}",
        "function* main(){const x=1; x=2;}",
        "import fs from 'fs'; function* main(){}",
        "function* main(){yield call(tools.create_task,{});}",
    ] {
        let error = compile(source).unwrap_err().to_string();
        assert!(error.starts_with("TS_PROGRAM "), "{error}");
        assert!(error.contains(':'), "missing source location: {error}");
    }
}

#[test]
fn call_depth_errors_are_catchable() {
    let r = runtime(
        "function* recurse(){return yield call(recurse);} function* main(){try {return yield call(recurse);} catch(e){return e.message;}}",
    );
    assert_eq!(returned(r), json!("program call depth limit"));
}

#[test]
fn semantic_expressions_suspend_instead_of_executing_javascript() {
    for expression in [
        "1 + 2",
        "1 - 2",
        "1 * 2",
        "1 / 2",
        "1 % 2",
        "2 ** 2",
        "1 === 1",
        "1 !== 1",
        "1 == 1",
        "1 != 1",
        "1 < 2",
        "1 > 2",
        "1 <= 2",
        "1 >= 2",
        "true && false",
        "true || false",
        "null ?? false",
        "!false",
        "-1",
        "+1",
        "~1",
        "typeof x",
        "void x",
        "1 & 2",
        "1 | 2",
        "1 ^ 2",
        "1 << 2",
        "1 >> 2",
        "1 >>> 2",
        "true ? 1 : 2",
        "`value:${x}`",
        "'html'.indexOf('html')",
        "x.includes('a')",
        "JSON.stringify({ok:true})",
    ] {
        let mut r = runtime(&format!(
            "function main(){{let x='a';return {expression};}}"
        ));
        let State::Action(action) = r.next() else {
            panic!("{expression} must reach AI")
        };
        assert_eq!(action.semantic_boolean, Some(false));
        assert!(
            matches!(action.kind,ActionKind::Task{input,..} if input["expression"]==expression)
        );
        // Deliberately differ from JS: the runtime must pass through AI's result.
        r.resume(
            json!({"value":"semantic result","evidence":[{"input":"source and bindings"}]}),
            false,
        )
        .unwrap();
        assert_eq!(returned(r), json!("semantic result"));
    }
    for expression in ["[1 + 2]", "{value:1 + 2}", "[1][0 + 0]"] {
        let mut r = runtime(&format!("function main(){{return {expression};}}"));
        assert!(matches!(r.next(), State::Action(_)));
    }
    // Historical source is flow pseudocode now, including ordinary assignments.
    compile(include_str!("fixtures/two-workers.ts")).unwrap();
    assert_eq!(
        returned(runtime(
            "function main(){return `if(x < 2){x.indexOf('x');}`;}"
        )),
        json!("if(x < 2){x.indexOf('x');}")
    );
}

#[test]
fn executable_mutations_closures_and_hidden_effects_are_rejected_before_effects() {
    for expression in [
        "delete x.a",
        "x++",
        "++x",
        "x--",
        "--x",
        "x += 1",
        "x &&= true",
        "x ??= 1",
        "x!",
        "(1,2)",
        "(()=>1)()",
        "new Date()",
        "x.map(item => item)",
        "x.check({run(){return 1;}})",
        "tools.read_file({path:'x'}).content !== 'expected'",
        "false && tools.write_file({path:'hidden'})",
        "helper() !== 'expected'",
        "x.check(tools.write_file({path:'hidden'}))",
        "x.push(1) > 0",
    ] {
        let source = format!(
            "function helper(){{return 'ok';}} function main(){{let x=1;tools.write_file({{path:'never-written'}});\nreturn {expression};}}"
        );
        let error = compile(&source).unwrap_err().to_string();
        assert!(error.starts_with("TS_PROGRAM 2:"), "{expression}: {error}");
    }
    assert!(compile("function main(){const x=1;x=2;}").is_err());
    assert!(compile("function main(){let x={};x.a=2;}").is_err());
}

#[test]
fn semantic_conditions_bind_only_referenced_locals_and_preserve_comments_and_unicode() {
    let source = r#"function main(要求) {
        // Compare actual artifact quality against the requirements, not object identity.
        let 产物 = {path:'artifact.html'};
        const unrelated = 'private';
        if (产物 !== 要求.期待产物) {产物 = {path:'repaired.html'};}
        return 产物;
    }"#;
    for (decision, path) in [(true, "repaired.html"), (false, "artifact.html")] {
        let mut r = Runtime::new(
            compile(source).unwrap(),
            "main",
            vec![json!({"期待产物":"approved"})],
        )
        .unwrap();
        let State::Action(action) = r.next() else {
            panic!()
        };
        assert_eq!(action.semantic_boolean, Some(true));
        let ActionKind::Task {
            input, description, ..
        } = action.kind
        else {
            panic!()
        };
        assert_eq!(input["expression"], "产物 !== 要求.期待产物");
        assert_eq!(
            input["bindings"],
            json!({"产物":{"path":"artifact.html"},"要求":{"期待产物":"approved"}})
        );
        assert!(description.contains("not object identity"));
        let mut restored: Runtime =
            serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        restored
            .resume(
                json!({"value":decision,"evidence":[{"observed":"actual artifact"}]}),
                false,
            )
            .unwrap();
        assert_eq!(returned(restored), json!({"path":path}));
    }
}

#[test]
fn semantic_contract_failures_propagate_through_catch_and_assertions_stop_effects() {
    for reply in [
        json!(true),
        json!({"value":true}),
        json!({"value":true,"evidence":[]}),
        json!({"value":"true","evidence":["input"]}),
    ] {
        let mut r =
            runtime("function main(){try{if(1 !== 2){return 'bad';}}catch(error){return error;}}");
        assert!(matches!(r.next(), State::Action(_)));
        r.resume(reply, false).unwrap();
        assert!(
            returned(r)["message"]
                .as_str()
                .unwrap()
                .contains("semantic task must return")
        );
    }
    for decision in [true, false] {
        let mut r = runtime(
            "function main(){try{assert(实际产物符合要求,'inspection failed');return 'passed';}catch(error){return error.message;}}",
        );
        assert!(matches!(r.next(), State::Action(_)));
        r.resume(
            json!({"value":decision,"evidence":["actual inspection"]}),
            false,
        )
        .unwrap();
        assert_eq!(
            returned(r),
            json!(if decision {
                "passed"
            } else {
                "inspection failed"
            })
        );
    }
    let mut r = runtime("function main(){assert(false);tools.write_file({path:'never-written'});}");
    assert!(matches!(r.next(), State::Failed(_)));
    assert_eq!(
        returned(runtime("function main(){assert(true);return 'ok';}")),
        json!("ok")
    );
}

#[test]
fn if_else_uses_returned_boolean_or_field_directly() {
    let source = "function* check(input){return input;} function* main(input){const result=yield call(check,input);if(result.ok){return 'yes';}else if(result.retry){return 'retry';}else{return 'no';}}";
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
        "function* main(){const ok=yield ai({instruction:'Return a boolean',format:'json'});if(ok){return 'yes';}else{return 'no';}}",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!(false), false).unwrap();
    assert_eq!(returned(r), json!("no"));
}

#[test]
fn non_boolean_conditions_use_semantic_tasks_without_truthiness_coercion() {
    let source = "function main(input){if(input){return 'yes';}else{return 'no';}}";
    for input in [
        json!(1),
        json!(0),
        json!("false"),
        json!(null),
        json!([]),
        json!({"ok":false}),
    ] {
        let mut r = Runtime::new(compile(source).unwrap(), "main", vec![input.clone()]).unwrap();
        assert!(
            matches!(r.next(),State::Action(Action{kind:ActionKind::Task{input:ref bindings,..},..}) if bindings["bindings"]["$condition_value"]==input)
        );
        r.resume(json!({"value":false,"evidence":[{"input":input}]}), false)
            .unwrap();
        assert_eq!(returned(r), json!("no"));
    }
    let mut r =
        runtime("function main(){for(const item of 'abc'){tools.write_file({content:item});}}");
    assert!(matches!(r.next(), State::Failed(_)));
}

#[test]
fn old_operator_enabled_runtime_cannot_resume_pending_effects() {
    let mut r = runtime(
        "function* main(){yield call(tools.write_file,{path:'not-replayed',content:'x'});}",
    );
    assert!(matches!(r.next(), State::Action(_)));
    r.program.version = 1;
    let mut restored: Runtime = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
    assert!(matches!(restored.next(),State::Failed(error) if error["required_version"]==VERSION));
}

#[test]
fn integer_literals_and_task_parameters_remain_json_integers() {
    let source = "function* main(input){yield call(tools.list_dir,{path:'.',max_depth:2,limit:input.limit});}";
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
    assert!(compile("function* prepare(){return 'ready';}").is_err());
    assert!(compile("function* main(){} function* main(){}").is_err());
    let mut r = runtime(
        "function* prepare(){return {ready:true};} function* execute(input){yield call(tools.write_file,{path:'done.txt',content:'done'});return input;} function* main(){const prepared=yield call(prepare);const result=yield call(execute,prepared);return result;}",
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
  const original = tools["files/read"]({path: request.source});
  const saved = process(original.content, request);
  return {saved, original: original.content};
}
function process(content, request) {
  // Explicit tool calls and task calls share one sequence.
  return tools.write_file({path: request.output, content: transform(content, request).content});
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
        matches!(r.next(),State::Action(Action{kind:ActionKind::Tool{name,args},..}) if name=="files/read" && args["path"]=="source.txt")
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
    let mut r = runtime("function main(){const x=[];x.push(1 + 2);return x;}");
    assert!(matches!(r.next(), State::Action(_)));
    r.resume(json!({"value":3,"evidence":["supplied operands"]}), false)
        .unwrap();
    assert_eq!(returned(r), json!([3]));
}
