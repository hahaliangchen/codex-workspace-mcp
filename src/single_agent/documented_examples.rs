//! Execute the published source, not a rewritten approximation of its main.
use super::*;

fn latest_run_marker(name: &str) -> std::path::PathBuf {
    if cfg!(windows) {
        std::env::temp_dir().join(name)
    } else {
        Path::new("/tmp").join(name)
    }
}

fn missing_local_links(root: &Path, files: &[String]) -> Result<Vec<Value>> {
    let href = regex::Regex::new(r#"(?i)\bhref\s*=\s*[\"']([^\"']+)[\"']"#)?;
    let mut missing = Vec::new();
    for file in files {
        let full = root.join(file);
        let content = std::fs::read_to_string(&full)?;
        for found in href.captures_iter(&content) {
            let link = &found[1];
            let path = link.split(['#', '?']).next().unwrap_or("");
            if path.is_empty() || path.contains(':') || path.starts_with('/') {
                continue;
            }
            if !full.parent().unwrap_or(root).join(path).exists() {
                missing.push(json!({"page":file,"href":link}));
            }
        }
    }
    Ok(missing)
}

#[test]
fn local_navigation_check_reports_missing_files() {
    let root =
        std::env::temp_dir().join(format!("navigation-check-{}", agent_service::uuid_like()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("present.html"), "ok").unwrap();
    std::fs::write(
        root.join("index.html"),
        "<a href='./present.html#part'>ok</a><a href='missing.html'>broken</a>",
    )
    .unwrap();
    let missing = missing_local_links(&root, &["index.html".into()]).unwrap();
    assert_eq!(
        missing,
        vec![json!({"page":"index.html","href":"missing.html"})]
    );
    std::fs::remove_dir_all(root).unwrap();
}



#[tokio::test]
#[ignore = "real Luna TypeScript object-oriented design experiment; consumes provider tokens"]
async fn real_luna_typescript_oop_acceptance_design() -> Result<()> {
    let base=Path::new(env!("CARGO_MANIFEST_DIR"));
    let evidence=base.join("runs").join(format!("luna-ts-oop-five-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(&evidence)?;
    std::fs::write(latest_run_marker("luna-ts-oop-five-latest"),evidence.to_string_lossy().as_bytes())?;
    let mut state=agent_service::tests::test_state(&evidence,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    let prompt="请为一个已有的五页中文 HTML 项目写 TypeScript 验收脚本：先理解工作区 README.md、docs/agent-program.md、docs/generic-agent-design.md 的项目设计，再从整组页面是否有效介绍项目的角度，结合 runs/acceptance-five-ai-review 下五个 HTML 文件，检查整体和各页问题，并把结论、依据、问题和未检查事项写入 acceptance-report.md。不要修改页面。";
    let instruction="使用传统面向对象方式编写 TypeScript：用类组织职责、状态和流程。能用普通代码与现有工具可靠实现的步骤应写出具体实现；需要理解、判断或其他尚无法直接编程实现的能力，声明为职责内聚的抽象方法，清楚定义参数和返回类型，不必编写这些抽象方法的实现类。宿主可提供满足抽象类接口的实例。文件系统能力也可以作为接口声明。适度设计类，避免将整个流程变成一个抽象方法；检查基于真实内容和证据，准确记录无法确认的内容。请只返回完整 TypeScript 源码，不要解释或 Markdown 围栏。无需为当前特定虚拟机限制代码。";
    std::fs::write(evidence.join("request.json"),serde_json::to_vec_pretty(&json!({"model":model,"system":instruction,"user":prompt}))?)?;
    eprintln!("TS_OOP_START {} model={model}",evidence.display());
    let started=std::time::Instant::now();
    let response=state.model_client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/'))).bearer_auth(&state.api_key).json(&json!({"model":model,"messages":[{"role":"system","content":instruction},{"role":"user","content":prompt}],"stream":false})).send().await?.error_for_status()?;
    let result:Value=response.json().await?;
    let raw=result["choices"][0]["message"]["content"].as_str().context("model returned no source")?.trim();
    let source=if raw.starts_with("```"){raw.split_once('\n').map(|(_,s)|s).unwrap_or(raw).trim_end_matches('`').trim()}else{raw};
    std::fs::write(evidence.join("acceptance.ts"),source)?;
    let map:swc_common::sync::Lrc<swc_common::SourceMap>=Default::default();
    let file=map.new_source_file(swc_common::FileName::Custom("acceptance.ts".into()).into(),source.to_owned());
    let lexer=swc_ecma_parser::lexer::Lexer::new(swc_ecma_parser::Syntax::Typescript(swc_ecma_parser::TsSyntax::default()),Default::default(),swc_ecma_parser::StringInput::from(&*file),None);
    let mut parser=swc_ecma_parser::Parser::new_from(lexer);
    let parsed=parser.parse_typescript_module();
    let errors=parser.take_errors();
    let syntax_ok=parsed.is_ok() && errors.is_empty();
    let report=json!({"model":model,"seconds":started.elapsed().as_secs_f64(),"usage":result["usage"],"typescript_syntax_ok":syntax_ok,"syntax_error":parsed.err().map(|e|format!("{:?}",e.kind())),"parser_errors":errors.iter().map(|e|format!("{:?}",e.kind())).collect::<Vec<_>>(),"executed":false});
    std::fs::write(evidence.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    eprintln!("TS_OOP_REPORT {report}");
    Ok(())
}

#[tokio::test]
#[ignore = "real Luna minimal user-only TypeScript acceptance experiment"]
async fn real_luna_typescript_minimal_prompt() -> Result<()> {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("runs").join(format!("luna-html-js-single-responsibility-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(&root)?;
    let mut state=agent_service::tests::test_state(&root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    let prompt="请用 JavaScript 写一份过程式代码，验收我已有的五页 HTML 项目。项目根目录是 /home/goodtime/Documents/project/codex-workspace-mcp，五个页面都在 runs/acceptance-five-ai-review 目录下，分别是 01-overview.html、02-program.html、03-tools.html、04-agents.html、05-quality.html。项目设计资料是根目录下的 README.md、docs/agent-program.md 和 docs/generic-agent-design.md。

请先根据设计资料理解项目目标，再检查五页作为一组是否完整、内容是否与设计一致、各页之间是否协调；同时检查页面本地链接与资源是否有效。根据实际检查结果写一份验收报告，包含整体和逐页结论、具体问题、依据以及没有检查或无法确认的事项。不要修改已有页面。

用普通代码组织完整流程，并实现可以用代码和工具可靠完成的步骤。需要 AI 的能力用带 @virtual 注释的空函数声明，由宿主提供实现；写清职责、参数和返回类型。

按单一职责原则设计函数，保持高内聚、低耦合。每个虚函数尽量只做一件事，不要让一个虚函数承担过多不同职责；普通函数负责组合调用和具体执行。按实际需要划分，不为形式机械拆分。";
    std::fs::write(root.join("prompt.txt"),prompt)?;
    eprintln!("TS_MINIMAL_START {} model={model}",root.display());
    let started=std::time::Instant::now();
    let response=state.model_client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/'))).bearer_auth(&state.api_key).json(&json!({"model":model,"messages":[{"role":"user","content":prompt}],"stream":false})).send().await?.error_for_status()?;
    let result:Value=response.json().await?;
    let raw=result["choices"][0]["message"]["content"].as_str().context("model returned no output")?;
    std::fs::write(root.join("response.md"),raw)?;
    let source=raw.split("```javascript").nth(1).or_else(||raw.split("```js").nth(1)).or_else(||raw.split("```typescript").nth(1)).or_else(||raw.split("```ts").nth(1)).map(|s|s.split("```").next().unwrap_or(s));
    let mut syntax_ok=None;
    if let Some(source)=source {
        std::fs::write(root.join("acceptance.js"),source.trim())?;
        let map:swc_common::sync::Lrc<swc_common::SourceMap>=Default::default();
        let file=map.new_source_file(swc_common::FileName::Custom("acceptance.ts".into()).into(),source.to_owned());
        let lexer=swc_ecma_parser::lexer::Lexer::new(swc_ecma_parser::Syntax::Typescript(swc_ecma_parser::TsSyntax::default()),Default::default(),swc_ecma_parser::StringInput::from(&*file),None);
        let mut parser=swc_ecma_parser::Parser::new_from(lexer);
        syntax_ok=Some(parser.parse_typescript_module().is_ok() && parser.take_errors().is_empty());
    }
    let report=json!({"model":model,"seconds":started.elapsed().as_secs_f64(),"usage":result["usage"],"user_messages":1,"system_messages":0,"tool_declarations":0,"typescript_syntax_ok":syntax_ok,"executed":false});
    std::fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    eprintln!("TS_MINIMAL_REPORT {report}");
    Ok(())
}

#[tokio::test]
#[ignore = "real configured model, isolated workspace, consumes provider tokens"]
async fn real_model_async_five_pages_published_example() -> Result<()> {
    let root=std::env::temp_dir().join(format!("async-five-real-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(root.join("docs"))?;
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/agent-design-guide.md"),root.join("docs/agent-design-guide.md"))?;
    let source=include_str!("../../skills/task-program/examples/parallel-five-pages.js");
    let mut state=agent_service::tests::test_state(&root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get(&state.default_model).cloned().unwrap_or_else(||state.default_model.clone());
    ensure!(!state.provider_url.is_empty() && !state.api_key.is_empty(),"configured model unavailable");
    agent_service::open_db(&root)?.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('example','',?1,'running',0,0)",[&model])?;
    eprintln!("REAL_ASYNC_FIVE_START model={model} root={}",root.display());
    let prompt=format!("Run exactly this program: reply with it unchanged as your one ```js block.\n```js\n{}\n```",source.trim());
    let cancel=CancellationToken::new();
    let result=tokio::time::timeout(Duration::from_secs(600),run_task(state,"example".into(),model,prompt,80,cancel.clone(),1,false)).await;
    if result.is_err(){cancel.cancel();}
    result.context("real async example timed out")??;
    for page in super::program_flow::example_pages(source) {
        let file=root.join(page);
        ensure!(file.is_file() && std::fs::metadata(&file)?.len()>0,"missing page: {}",file.display());
    }
    let conn=agent_service::open_db(&root)?;
    let status:String=conn.query_row("SELECT status FROM agent_tasks WHERE id='example'",[],|row|row.get(0))?;
    ensure!(status=="completed","real async example status: {status}");
    let calls:i64=conn.query_row("SELECT count(*) FROM agent_request_contexts WHERE task_id='example'",[],|row|row.get(0))?;
    eprintln!("REAL_ASYNC_FIVE_COMPLETE model_requests={calls} root={}",root.display());
    Ok(())
}

#[tokio::test]
#[ignore = "real configured model, isolated workspace, consumes provider tokens"]
async fn real_model_serial_published_example() -> Result<()> {
    let root=std::env::temp_dir().join(format!("serial-real-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(&root)?;
    let source=include_str!("../../skills/task-program/examples/serial.js");
    let mut state=agent_service::tests::test_state(&root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get(&state.default_model).cloned().unwrap_or_else(||state.default_model.clone());
    ensure!(!state.provider_url.is_empty() && !state.api_key.is_empty(),"configured model unavailable");
    agent_service::open_db(&root)?.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('example','',?1,'running',0,0)",[&model])?;
    eprintln!("REAL_SERIAL_START model={model} root={}",root.display());
    // The production path: the main AI replies with the program, and every
    // virtual function in it is then implemented by the real model.
    let prompt=format!("Run exactly this program: reply with it unchanged as your one ```js block.\n```js\n{}\n```",source.trim());
    let cancel=CancellationToken::new();
    let result=tokio::time::timeout(Duration::from_secs(480),run_task(state,"example".into(),model,prompt,40,cancel.clone(),1,false)).await;
    if result.is_err(){cancel.cancel();}
    result.context("real serial example timed out")??;
    let file=root.join("agent-overview.html");
    ensure!(file.is_file() && std::fs::metadata(&file)?.len()>0,"missing output: {}",file.display());
    let conn=agent_service::open_db(&root)?;
    let status:String=conn.query_row("SELECT status FROM agent_tasks WHERE id='example'",[],|row|row.get(0))?;
    ensure!(status=="completed","real serial example status: {status}");
    eprintln!("REAL_SERIAL_COMPLETE root={}",root.display());
    Ok(())
}
