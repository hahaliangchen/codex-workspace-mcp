//! Allowlisted native foreground programs. Arguments are never interpreted by a shell.

use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::{AsyncRead, AsyncReadExt}, process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub const TOOLS: &[&str] = &["run_program"];
const PROGRAMS: &[&str] = &["cargo", "git", "node", "python"];
const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const MAX_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_OUTPUT_LIMIT: usize = 65_536;
const MAX_OUTPUT_LIMIT: usize = 262_144;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 32_000;

#[derive(Debug, Deserialize)]
struct ProgramRequest {
    program: String,
    args: Vec<String>,
    #[serde(default = "default_project_path")]
    project_path: String,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
    #[serde(default = "default_output_limit")]
    max_output_bytes: usize,
}

fn default_project_path() -> String { ".".to_owned() }
fn default_timeout_seconds() -> u64 { DEFAULT_TIMEOUT_SECONDS }
fn default_output_limit() -> usize { DEFAULT_OUTPUT_LIMIT }

#[derive(Default)]
struct CapturedPipe {
    bytes: Vec<u8>,
    truncated: bool,
}

pub fn is_tool(name: &str) -> bool { TOOLS.contains(&name) }

pub fn check_key(args: &Value) -> String {
    let program = args["program"].as_str().unwrap_or("").trim().to_ascii_lowercase();
    let project = args["project_path"].as_str().unwrap_or(".").replace('\\', "/");
    let arguments = args["args"].as_array().cloned().unwrap_or_default();
    format!("program:{program}:{project}:{}", Value::Array(arguments))
}

pub fn normalize_args(root: &Path, args: &mut Value) -> Result<()> {
    ensure!(args.is_object(), "run_program arguments must be an object");
    if let Some(project) = args.get("project_path").and_then(Value::as_str) {
        let (_, relative) = resolve_project_path(root, project)?;
        args["project_path"] = json!(relative);
    }
    Ok(())
}

pub async fn execute(workspace: &crate::tools::Workspace, args: &Value) -> Result<Value> {
    execute_cancellable(workspace, args, &CancellationToken::new()).await
}

pub async fn execute_cancellable(
    workspace: &crate::tools::Workspace,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let request: ProgramRequest = serde_json::from_value(args.clone())?;
    ensure!(PROGRAMS.contains(&request.program.as_str()), "program must be one of: {}", PROGRAMS.join(", "));
    ensure!(request.args.len() <= MAX_ARGUMENTS, "args must contain at most {MAX_ARGUMENTS} items");
    ensure!(request.args.iter().all(|arg| !arg.contains('\0')), "program arguments cannot contain NUL bytes");
    let argument_size = request.args.iter().map(|arg| arg.len().saturating_add(1)).sum::<usize>();
    ensure!(argument_size <= MAX_ARGUMENT_BYTES, "combined arguments exceed {MAX_ARGUMENT_BYTES} bytes");

    let timeout_seconds = request.timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS);
    let max_output_bytes = request.max_output_bytes.clamp(1, MAX_OUTPUT_LIMIT);
    let (cwd, relative_project) = resolve_project_path(workspace.root(), &request.project_path)?;
    let check_key = format!("program:{}:{}:{}", request.program, relative_project, json!(request.args));
    let base = json!({
        "program":request.program.clone(),
        "args":request.args.clone(),
        "project_path":relative_project,
        "check_key":check_key,
        "timeout_seconds":timeout_seconds,
        "max_output_bytes":max_output_bytes,
    });

    if cancel.is_cancelled() {
        return Ok(result(base, "cancelled", None, false, 0, Some("Program execution was cancelled before launch."), CapturedPipe::default(), CapturedPipe::default(), None, None));
    }

    let started = Instant::now();
    let executable = match resolve_program(&request.program) {
        Ok(path) => path,
        Err(error) => return Ok(result(base, "spawn_failed", None, false, elapsed_ms(started), Some(&format!("{error:#}")), CapturedPipe::default(), CapturedPipe::default(), None, None)),
    };
    let executable = match crate::project_process::native_runtime_path(&executable) {
        Ok(path) => path,
        Err(error) => return Ok(result(base, "spawn_failed", None, false, elapsed_ms(started), Some(&format!("Could not prepare the program path: {error:#}")), CapturedPipe::default(), CapturedPipe::default(), None, None)),
    };

    let mut command = Command::new(executable);
    command.args(&request.args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
    if request.program == "git" { command.env("GIT_PAGER", "cat").env("GIT_TERMINAL_PROMPT", "0"); }
    crate::project_process::prepare_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(result(base, "spawn_failed", None, false, elapsed_ms(started), Some(&format!("Could not start '{}' in '{}': {error}", request.program, relative_project)), CapturedPipe::default(), CapturedPipe::default(), None, None)),
    };
    let group = match crate::project_process::ProcessGroup::attach(&child) {
        Ok(group) => group,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(result(base, "spawn_failed", None, false, elapsed_ms(started), Some(&format!("Program started but process-group supervision could not be established: {error:#}")), CapturedPipe::default(), CapturedPipe::default(), None, None));
        }
    };

    let stdout_task = child.stdout.take().map(|pipe| tokio::spawn(capture_pipe(pipe, max_output_bytes)));
    let stderr_task = child.stderr.take().map(|pipe| tokio::spawn(capture_pipe(pipe, max_output_bytes)));
    let wait = child.wait();
    tokio::pin!(wait);
    let (outcome, status, error_message) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let stop_error = group.terminate().err().map(|error| error.to_string());
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait).await;
            ("cancelled", None, Some(termination_message("Program execution was cancelled.", stop_error)))
        }
        _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => {
            let stop_error = group.terminate().err().map(|error| error.to_string());
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait).await;
            ("timed_out", None, Some(termination_message(&format!("Program exceeded the {timeout_seconds} second timeout."), stop_error)))
        }
        status = &mut wait => match status {
            Ok(status) => ("exited", Some(status), None),
            Err(error) => {
                let stop_error = group.terminate().err().map(|error| error.to_string());
                ("spawn_failed", None, Some(termination_message(&format!("Could not collect the program exit status: {error}"), stop_error)))
            }
        }
    };

    drop(group);
    let stdout = collect_pipe(stdout_task).await;
    let stderr = collect_pipe(stderr_task).await;
    let exit_code = status.as_ref().and_then(ExitStatus::code);
    let process_success = outcome == "exited" && status.as_ref().is_some_and(ExitStatus::success);
    Ok(result(base, outcome, exit_code, process_success, elapsed_ms(started), error_message.as_deref(), stdout.0, stderr.0, stdout.1, stderr.1))
}

fn result(
    base: Value,
    outcome: &str,
    exit_code: Option<i32>,
    process_success: bool,
    elapsed_ms: u64,
    error_message: Option<&str>,
    stdout: CapturedPipe,
    stderr: CapturedPipe,
    stdout_read_error: Option<String>,
    stderr_read_error: Option<String>,
) -> Value {
    let mut value = base;
    value["outcome"] = json!(outcome);
    value["process_exit_code"] = exit_code.map_or(Value::Null, |code| json!(code));
    value["process_success"] = json!(process_success);
    value["elapsed_ms"] = json!(elapsed_ms);
    value["error_message"] = error_message.map_or(Value::Null, |message| json!(message));
    value["stdout_truncated"] = json!(stdout.truncated);
    value["stderr_truncated"] = json!(stderr.truncated);
    value["stdout_read_error"] = stdout_read_error.map_or(Value::Null, |message| json!(message));
    value["stderr_read_error"] = stderr_read_error.map_or(Value::Null, |message| json!(message));
    add_decoded_pipe(&mut value, "stdout", stdout.bytes);
    add_decoded_pipe(&mut value, "stderr", stderr.bytes);
    value
}

fn add_decoded_pipe(value: &mut Value, name: &str, bytes: Vec<u8>) {
    match String::from_utf8(bytes) {
        Ok(text) => {
            value[name] = json!(text);
            value[format!("{name}_encoding")] = json!("utf-8");
            value[format!("{name}_decode_error")] = Value::Null;
        }
        Err(error) => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let raw = error.into_bytes();
            value[name] = Value::Null;
            value[format!("{name}_encoding")] = json!("utf-8");
            value[format!("{name}_decode_error")] = json!(format!("Invalid UTF-8 at byte offset {valid_up_to}; raw captured bytes are preserved as base64."));
            value[format!("{name}_raw_base64")] = json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw));
        }
    }
}

async fn capture_pipe<R: AsyncRead + Unpin>(mut pipe: R, limit: usize) -> std::io::Result<CapturedPipe> {
    let mut captured = CapturedPipe { bytes: Vec::with_capacity(limit.min(16_384)), truncated: false };
    let mut buffer = [0u8; 8192];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 { break; }
        let take = limit.saturating_sub(captured.bytes.len()).min(read);
        captured.bytes.extend_from_slice(&buffer[..take]);
        if take < read { captured.truncated = true; }
    }
    Ok(captured)
}

async fn collect_pipe(task: Option<JoinHandle<std::io::Result<CapturedPipe>>>) -> (CapturedPipe, Option<String>) {
    let Some(mut task) = task else { return (CapturedPipe::default(), Some("captured pipe was unavailable".to_owned())); };
    match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
        Ok(Ok(Ok(output))) => (output, None),
        Ok(Ok(Err(error))) => (CapturedPipe::default(), Some(error.to_string())),
        Ok(Err(error)) => (CapturedPipe::default(), Some(error.to_string())),
        Err(_) => {
            task.abort();
            (CapturedPipe { truncated: true, ..Default::default() }, Some("output pipe did not close after process exit".to_owned()))
        }
    }
}

fn termination_message(message: &str, stop_error: Option<String>) -> String {
    match stop_error { Some(error) => format!("{message} Process-group termination reported: {error}"), None => message.to_owned() }
}

fn elapsed_ms(started: Instant) -> u64 { started.elapsed().as_millis().min(u64::MAX as u128) as u64 }

fn resolve_project_path(root: &Path, raw: &str) -> Result<(PathBuf, String)> {
    ensure!(!raw.trim().is_empty() && raw.len() <= 4096, "project_path must be a nonempty path of at most 4096 bytes");
    let resolved = crate::file_edit::workspace_path(root, raw.trim())?;
    ensure!(resolved.is_dir(), "project_path must identify an existing directory inside the workspace");
    let canonical = resolved.canonicalize()?;
    let root = root.canonicalize()?;
    let relative = canonical.strip_prefix(&root).context("project_path is outside the workspace")?;
    let relative = if relative.as_os_str().is_empty() { ".".to_owned() } else { relative.to_string_lossy().replace('\\', "/") };
    Ok((canonical, relative))
}

fn resolve_program(program: &str) -> Result<PathBuf> {
    if program == "node" { return crate::project_process::resolve_node_executable(); }
    let executable = if cfg!(windows) { format!("{program}.exe") } else { program.to_owned() };
    let paths = std::env::var_os("PATH").map(|value| std::env::split_paths(&value).collect::<Vec<_>>()).unwrap_or_default();
    for directory in paths {
        let candidate = directory.join(&executable);
        if candidate.is_file() { return Ok(candidate.canonicalize().unwrap_or(candidate)); }
    }
    anyhow::bail!("supported program '{program}' was not found in the host PATH")
}

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name":"run_program",
        "description":"Run one allowlisted installed program directly with an argv array; the host never invokes a shell. Supported names are cargo, git, node and python. Use project_path for an existing directory inside the workspace. This is foreground-only; use the managed project tools for npm scripts and background services. process_success describes the actual program exit code only; stderr text alone is not failure. Output is capped per stream with stdout_truncated/stderr_truncated and stdout_read_error/stderr_read_error metadata. Output is decoded as UTF-8; on invalid UTF-8 the field is null, raw captured bytes are retained as base64, and the decode error is explicit. Timeout, cancellation, and launch failures have null exit code and cannot pass a check.",
        "inputSchema":{"type":"object","required":["program","args"],"properties":{
            "program":{"type":"string","enum":PROGRAMS},
            "args":{"type":"array","maxItems":MAX_ARGUMENTS,"items":{"type":"string","maxLength":8192}},
            "project_path":{"type":"string","default":".","maxLength":4096,"description":"Existing working directory inside the workspace; defaults to the workspace root."},
            "timeout_seconds":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_SECONDS,"default":DEFAULT_TIMEOUT_SECONDS},
            "max_output_bytes":{"type":"integer","minimum":1,"maximum":MAX_OUTPUT_LIMIT,"default":DEFAULT_OUTPUT_LIMIT}
        }}
    })]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(label:&str)->(crate::tools::Workspace,PathBuf) {
        let _=label;
        let root=std::env::current_dir().unwrap();
        (crate::tools::Workspace::new(&root).unwrap(),root)
    }

    #[test]
    fn check_key_keeps_program_working_directory_and_argv_boundaries() {
        let key=check_key(&json!({"program":"python","project_path":"apps/demo","args":["-c","print('空 格')","two words"]}));
        assert_eq!(key,"program:python:apps/demo:[\"-c\",\"print('空 格')\",\"two words\"]");
        assert!(definitions()[0]["inputSchema"]["properties"]["program"]["enum"].as_array().unwrap().iter().any(|value|value=="python"));
    }

    #[tokio::test]
    async fn python_receives_argv_without_shell_and_exit_nine_is_not_success() {
        if resolve_program("python").is_err() { return; }
        let (workspace,root)=workspace("argv");
        let code="import json,sys; print(json.dumps(sys.argv[1:], ensure_ascii=False)); sys.exit(9)";
        let result=execute(&workspace,&json!({"program":"python","args":["-c",code,"two words","a\"b","中文"],"project_path":"."})).await.unwrap();
        assert_eq!(result["outcome"],"exited");
        assert_eq!(result["process_exit_code"],9);
        assert_eq!(result["process_success"],false);
        assert!(result["stdout"].as_str().unwrap().contains("two words"));
        assert!(result["stdout"].as_str().unwrap().contains("中文"));
        let _=root;
    }

    #[tokio::test]
    async fn timeout_and_cancellation_have_explicit_outcomes() {
        if resolve_program("python").is_err() { return; }
        let (workspace,root)=workspace("termination");
        let timeout=execute(&workspace,&json!({"program":"python","args":["-c","import time; time.sleep(5)"],"timeout_seconds":1})).await.unwrap();
        assert_eq!(timeout["outcome"],"timed_out");
        assert_eq!(timeout["process_exit_code"],Value::Null);
        let cancel=CancellationToken::new();
        let child_cancel=cancel.clone();
        let args=json!({"program":"python","args":["-c","import time; time.sleep(5)"],"timeout_seconds":10});
        let child_workspace=crate::tools::Workspace::new(workspace.root()).unwrap();
        let task=tokio::spawn(async move {execute_cancellable(&child_workspace,&args,&child_cancel).await.unwrap()});
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap()["outcome"],"cancelled");
        let _=root;
    }
}
