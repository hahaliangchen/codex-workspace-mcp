//! Structured npm operations and host-owned processes. No terminal UI or shell command construction.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Mutex, Notify, mpsc},
};

pub const TOOLS: &[&str] = &[
    "install_dependencies",
    "run_project_script",
    "get_project_process",
    "stop_project_process",
];
pub const PROCESS_OBSERVATION_TTL_MS: u64 = 30_000;
static PROCESS_EVENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static HOST_INSTANCE_ID: OnceLock<String> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessObservationCoverage {
    WorkspaceList,
    #[default]
    ProjectList,
    SingleProcess,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProcessObservationScope {
    pub coverage: ProcessObservationCoverage,
    pub project_path: Option<String>,
    pub script: Option<String>,
    pub ready_url: Option<String>,
    pub ready_port: Option<u16>,
    pub process_id: Option<String>,
    pub operation: Option<String>,
    pub script_args: Vec<String>,
}

fn host_instance_id() -> &'static str {
    HOST_INSTANCE_ID.get_or_init(|| format!("host_{}_{}", std::process::id(), now()))
}

fn process_event_generation() -> u64 {
    PROCESS_EVENT_GENERATION.load(Ordering::Relaxed)
}

fn bump_process_event_generation() -> u64 {
    PROCESS_EVENT_GENERATION.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

fn scope_from_args(root: &Path, args: &Value) -> Result<ProcessObservationScope> {
    let (ready_url, ready_port) = probe_target(args)?;
    let explicit_coverage = args.get("coverage").and_then(Value::as_str);
    let coverage = match explicit_coverage {
        Some("workspace_list") => ProcessObservationCoverage::WorkspaceList,
        Some("project_list") => ProcessObservationCoverage::ProjectList,
        Some("single_process") => ProcessObservationCoverage::SingleProcess,
        Some(other) => anyhow::bail!("unknown process observation coverage '{other}'"),
        None if args["process_id"].as_str().is_some() => ProcessObservationCoverage::SingleProcess,
        None if args.get("project_path").and_then(Value::as_str).is_some() => ProcessObservationCoverage::ProjectList,
        None => ProcessObservationCoverage::WorkspaceList,
    };
    ensure!(coverage != ProcessObservationCoverage::WorkspaceList || args.get("project_path").is_none_or(Value::is_null),
        "workspace_list observations cannot be restricted to project_path");
    ensure!(coverage != ProcessObservationCoverage::SingleProcess || args["process_id"].as_str().is_some(),
        "single_process observations require process_id");
    let project_path = match coverage {
        ProcessObservationCoverage::WorkspaceList => None,
        ProcessObservationCoverage::ProjectList | ProcessObservationCoverage::SingleProcess => {
            let (_, relative) = project(root, args["project_path"].as_str().unwrap_or("."))?;
            Some(relative)
        }
    };
    Ok(ProcessObservationScope {
        coverage,
        project_path,
        script: args.get("script").and_then(Value::as_str).map(str::to_owned),
        ready_url,
        ready_port,
        process_id: args.get("process_id").and_then(Value::as_str).map(str::to_owned),
        operation: args.get("operation").and_then(Value::as_str).map(str::to_owned),
        script_args: args.get("script_args").and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
    })
}

fn project_input_version(root: &Path, scope: &ProcessObservationScope) -> Result<String> {
    let canonical_root = root.canonicalize().context("workspace root is unavailable to process observations")?;
    let mut digest = Sha256::new();
    digest.update(canonical_root.to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(scope)?);
    if let Some(project_path) = scope.project_path.as_deref() {
        let (dir, _) = project(&canonical_root, project_path).context("could not resolve project observation scope")?;
        for name in ["package.json", "package-lock.json", "npm-shrinkwrap.json"] {
            digest.update(name.as_bytes());
            digest.update([0]);
            match std::fs::read(dir.join(name)) {
                Ok(bytes) => digest.update(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => digest.update(b"<missing>"),
                Err(error) => return Err(error).with_context(|| format!("could not fingerprint {name}")),
            }
            digest.update([0]);
        }
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub fn observation_is_fresh(root: &Path, args: &Value, sample: &Value) -> bool {
    if !can_reuse_process_observation(args) { return false; }
    let Ok(expected_scope) = scope_from_args(root, args) else { return false; };
    let Ok(scope) = serde_json::from_value::<ProcessObservationScope>(sample["scope"].clone()) else { return false; };
    scope == expected_scope && observation_sample_is_current(root, sample)
}

pub fn observation_sample_is_current(root: &Path, sample: &Value) -> bool {
    let Ok(scope) = serde_json::from_value::<ProcessObservationScope>(sample["scope"].clone()) else { return false; };
    let Ok(version) = project_input_version(root, &scope) else { return false; };
    let now = now();
    sample["input_version"] == version
        && sample["host_instance_id"] == host_instance_id()
        && sample["process_event_generation"].as_u64() == Some(process_event_generation())
        && sample["sampled_at"].as_u64().is_some_and(|sampled| now.saturating_sub(sampled) <= PROCESS_OBSERVATION_TTL_MS)
}

pub fn can_reuse_process_observation(args: &Value) -> bool {
    args["process_id"].as_str().is_none()
        && args["force_refresh"] != true
        && args["wait_seconds"].as_u64().unwrap_or(0) == 0
        && args.get("ready_url").is_none_or(Value::is_null)
        && args.get("ready_port").is_none_or(Value::is_null)
}

fn observation_metadata(root: &Path, scope: &ProcessObservationScope) -> Result<Value> {
    Ok(json!({
        "scope":scope,
        "input_version":project_input_version(root, &scope)?,
        "sampled_at":now(),
        "expires_at":now().saturating_add(PROCESS_OBSERVATION_TTL_MS),
        "host_instance_id":host_instance_id(),
        "process_event_generation":process_event_generation(),
        "ttl_ms":PROCESS_OBSERVATION_TTL_MS
    }))
}

pub fn build_process_observation(root: &Path, args: &Value, processes: &[Value]) -> Result<Value> {
    let scope = scope_from_args(root, args)?;
    build_observation(root, scope, processes)
}

fn build_single_process_observation(root: &Path, args: &Value, processes: &[Value]) -> Result<Value> {
    let fact = processes.first().cloned().unwrap_or(Value::Null);
    let process_id = fact["process_id"].as_str().or_else(|| args["process_id"].as_str())
        .context("single-process observations require a process_id")?.to_owned();
    let project_path = fact["project_path"].as_str().or_else(|| args["project_path"].as_str()).unwrap_or(".");
    let (_, project_path) = project(root, project_path)?;
    let (fallback_url, fallback_port) = probe_target(args)?;
    let scope = ProcessObservationScope {
        coverage: ProcessObservationCoverage::SingleProcess,
        project_path: Some(project_path),
        script: fact["script"].as_str().or_else(|| args["script"].as_str()).map(str::to_owned),
        ready_url: fact["ready_url"].as_str().map(str::to_owned).or(fallback_url),
        ready_port: fact["ready_port"].as_u64().map(|port| port as u16).or(fallback_port),
        process_id: Some(process_id),
        operation: fact["operation"].as_str().or_else(|| args["operation"].as_str()).map(str::to_owned),
        script_args: fact["args"].as_array().or_else(|| args["script_args"].as_array())
            .into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
    };
    build_observation(root, scope, processes)
}

fn build_observation(root: &Path, scope: ProcessObservationScope, processes: &[Value]) -> Result<Value> {
    let mut metadata = observation_metadata(root, &scope)?;
    metadata["processes"] = json!(processes);
    Ok(metadata)
}

fn process_fact(value: &Value) -> Value {
    json!({"process_id":value["process_id"],"project_path":value["project_path"],"script":value["script"],
        "args":value["args"],"operation":value["operation"],"state":value["state"],"running":value["running"],"ready":value["ready"],
        "listener_owned":value["listener_owned"],"page_ready":value["page_ready"],"ready_url":value["ready_url"],
        "ready_port":value["ready_port"],"exit_code":value["exit_code"],"failure_stage":value["failure_stage"],
        "diagnosis":value["diagnosis"]})
}

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}
pub fn is_execution(name: &str) -> bool {
    matches!(
        name,
        "install_dependencies" | "run_project_script" | "stop_project_process"
    )
}
pub fn is_check(name: &str) -> bool {
    matches!(
        name,
        "run_command" | "install_dependencies" | "run_project_script" | "get_project_process" | "http_probe"
    )
}
pub fn may_change_files(name: &str) -> bool {
    matches!(
        name,
        "run_command" | "install_dependencies" | "run_project_script"
    )
}
pub fn normalize_args(root: &Path, args: &mut Value) -> Result<()> {
    ensure!(args.is_object(), "tool arguments must be an object");
    ensure!(
        args.get("project_path")
            .is_none_or(|value| value.is_string()),
        "project_path must be a string"
    );
    if args.get("project_path").is_some() {
        let (_, relative) = project(root, args["project_path"].as_str().unwrap_or("."))?;
        args["project_path"] = json!(relative);
    }
    Ok(())
}
pub fn check_key(name: &str, args: &Value) -> String {
    if name == "http_probe" {
        return crate::http_probe::check_key(args);
    }
    if name == "run_command" {
        return args["command"]
            .as_str()
            .unwrap_or("")
            .trim()
            .replace("\r\n", "\n");
    }
    let project = args["project_path"]
        .as_str()
        .unwrap_or(".")
        .replace('\\', "/");
    if name == "install_dependencies" {
        return format!("npm-install:{project}");
    }
    let prefix = if args["background"] == true {
        "npm-start"
    } else {
        "npm"
    };
    let mut key = format!(
        "{prefix}:{}:{}",
        project,
        args["script"].as_str().unwrap_or("")
    );
    if args["args"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        key.push(':');
        key.push_str(&args["args"].to_string());
    }
    key
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Serialize, Deserialize)]
struct Meta {
    process_id: String,
    pid: u32,
    root: PathBuf,
    project_path: String,
    script: String,
    args: Vec<String>,
    operation: String,
    check_key: String,
    running: bool,
    ready: bool,
    #[serde(default)]
    listener_owned: bool,
    #[serde(default)]
    page_ready: bool,
    #[serde(default)]
    readiness_detail: String,
    #[serde(default)]
    script_command: String,
    #[serde(default)]
    node_executable: String,
    #[serde(default)]
    npm_cli: String,
    #[serde(default)]
    failure_stage: String,
    #[serde(default)]
    failure_diagnosis: String,
    ready_url: Option<String>,
    ready_port: Option<u16>,
    started_at: u64,
    ended_at: Option<u64>,
    exit_code: Option<i32>,
    termination_reason: Option<String>,
    background: bool,
}
#[derive(Default)]
struct Logs {
    chunks: VecDeque<Value>,
    sequence: u64,
    chars: usize,
    disk_bytes: usize,
    truncated: bool,
    error: Option<String>,
}
struct Record {
    meta: Mutex<Meta>,
    logs: Mutex<Logs>,
    persist_guard: Mutex<()>,
    dir: PathBuf,
    stop: mpsc::UnboundedSender<String>,
    ended: Notify,
}
#[derive(Default)]
struct Registry {
    records: BTreeMap<String, Arc<Record>>,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
static SERIAL: AtomicU64 = AtomicU64::new(1);
fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(Default::default)
}

// The guard kills foreground operations when their requesting task is cancelled.
// Successful background launches deliberately transfer ownership to the host.
struct RequestLease(Option<mpsc::UnboundedSender<String>>);
impl Drop for RequestLease {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            let _ = stop.send("request_cancelled".into());
        }
    }
}

impl Record {
    async fn persist(&self) {
        let _guard = self.persist_guard.lock().await;
        let meta = self.meta.lock().await.clone();
        if let Ok(bytes) = serde_json::to_vec(&meta) {
            if let Err(error) =
                tokio::fs::write(self.dir.join(format!("{}.json", meta.process_id)), bytes).await
            {
                tracing::warn!(%error,"could not persist project process metadata");
            }
        }
    }
    async fn push(&self, stream: &str, text: String) {
        if text.is_empty() {
            return;
        }
        let mut logs = self.logs.lock().await;
        logs.sequence += 1;
        let seq = logs.sequence;
        logs.chars += text.chars().count();
        logs.chunks
            .push_back(json!({"seq":seq,"stream":stream,"text":text,"time":now()}));
        while logs.chars > 64_000 {
            if let Some(old) = logs.chunks.pop_front() {
                logs.chars = logs
                    .chars
                    .saturating_sub(old["text"].as_str().unwrap_or("").chars().count());
            } else {
                break;
            }
        }
        if logs.disk_bytes < 8 * 1024 * 1024 {
            use tokio::io::AsyncWriteExt;
            let id = self.meta.lock().await.process_id.clone();
            let chunk = format!("[{stream}] {text}");
            let written = async {
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.dir.join(format!("{id}.log")))
                    .await?;
                file.write_all(chunk.as_bytes()).await
            }
            .await;
            match written {
                Ok(()) => logs.disk_bytes += chunk.len(),
                Err(error) => logs.error = Some(error.to_string()),
            }
        } else {
            logs.truncated = true;
        }
    }
    async fn snapshot(&self, args: &Value) -> Value {
        let meta = self.meta.lock().await.clone();
        let logs = self.logs.lock().await;
        let limit = args["max_chars"]
            .as_u64()
            .unwrap_or(12_000)
            .clamp(4096, 60_000) as usize;
        let after = args["after_seq"].as_u64();
        let cursor_reset = logs
            .chunks
            .front()
            .is_some_and(|chunk| chunk["cursor_reset"] == true);
        let start = if cursor_reset && after.is_some_and(|seq| seq > logs.sequence) {
            0
        } else {
            after.unwrap_or_else(|| logs.sequence.saturating_sub(8))
        };
        let mut selected = Vec::new();
        let mut count = 0;
        let mut next = start.min(logs.sequence);
        for chunk in logs
            .chunks
            .iter()
            .filter(|chunk| chunk["seq"].as_u64().unwrap_or(0) > start)
        {
            let chars = chunk["text"].as_str().unwrap_or("").chars().count();
            if count + chars > limit && !selected.is_empty() {
                break;
            }
            count += chars;
            next = chunk["seq"].as_u64().unwrap_or(next);
            selected.push(chunk.clone());
        }
        let stdout = selected
            .iter()
            .filter(|chunk| chunk["stream"] == "stdout")
            .filter_map(|chunk| chunk["text"].as_str())
            .collect::<String>();
        let recent_stderr = selected
            .iter()
            .filter(|chunk| chunk["stream"] == "stderr")
            .filter_map(|chunk| chunk["text"].as_str())
            .collect::<String>();
        let complete_stderr=logs.chunks.iter().filter(|chunk|chunk["stream"]=="stderr")
            .filter_map(|chunk|chunk["text"].as_str()).collect::<String>();
        let stderr=if !meta.running&&meta.exit_code.is_some_and(|code|code!=0){complete_stderr}else{recent_stderr.clone()};
        let readiness_unverified=meta.background && !meta.ready && (meta.ready_url.is_none()&&meta.ready_port.is_none() || !meta.ready);
        let failure_stage=if !meta.failure_stage.is_empty(){meta.failure_stage.clone()}else if readiness_unverified{"readiness".to_owned()}else{String::new()};
        let failure_diagnosis=if !meta.failure_diagnosis.is_empty(){meta.failure_diagnosis.clone()}else if failure_stage=="readiness"{"readiness".to_owned()}else if failure_stage=="runtime_bootstrap"{"host_runtime".to_owned()}else if failure_stage=="project_script"{"project_script".to_owned()}else if failure_stage=="dependency_install"{"dependency_install".to_owned()}else{"unknown".to_owned()};
        json!({"process_id":meta.process_id,"pid":meta.pid,"project_path":meta.project_path,"script":meta.script,"args":meta.args,"operation":meta.operation,
            "state":if meta.running{"running"}else{"exited"},"running":meta.running,"process_running":meta.running,
            "ready":meta.running&&meta.ready,"listener_owned":meta.listener_owned,"page_ready":meta.page_ready,
            "failure_stage":if failure_stage.is_empty(){Value::Null}else{json!(failure_stage)},"diagnosis":failure_diagnosis,
            "readiness_detail":meta.readiness_detail,"script_command":meta.script_command,
            "host_runtime":{"node_executable":meta.node_executable,"npm_cli":meta.npm_cli,"host_pid":std::process::id(),"host_instance_id":host_instance_id()},
            "port_args":if meta.script_command.contains("concurrently"){"Extra args reach this npm script only. A concurrently parent does not forward --port to the child that listens; set the port on that child script."}else{""},
            "readiness":if !meta.running{"exited"}else if meta.ready{"ready"}else if meta.ready_url.is_none()&&meta.ready_port.is_none(){"not_configured"}else{"pending"},
            "ready_url":meta.ready_url,"ready_port":meta.ready_port,"status":meta.exit_code,"exit_code":meta.exit_code,
            "background":meta.background,"check_key":meta.check_key,"started_at":meta.started_at,"ended_at":meta.ended_at,"termination_reason":meta.termination_reason,
            "logs":selected,"stdout":stdout,"stderr":stderr,"stderr_recent":recent_stderr,"next_seq":next,"latest_seq":logs.sequence,
            "has_more":next<logs.sequence,"cursor_reset":cursor_reset,"log_gap":logs.chunks.front().is_some_and(|chunk|start.saturating_add(1)<chunk["seq"].as_u64().unwrap_or(0)),
            "log_file":self.dir.join(format!("{}.log",meta.process_id)),"disk_log_truncated":logs.truncated,"log_error":logs.error})
    }
    async fn wait(&self) {
        loop {
            let notified = self.ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.meta.lock().await.running {
                return;
            }
            notified.await;
        }
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: R, record: Arc<Record>, stream: &str) {
    let mut buf = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                if !pending.is_empty() {
                    record
                        .push(stream, String::from_utf8_lossy(&pending).into_owned())
                        .await;
                }
                return;
            }
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(error) => {
                record
                    .push("host", format!("log read failed: {error}\n"))
                    .await;
                return;
            }
        }
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(std::str::from_utf8(&pending[..valid]).unwrap());
                    pending.drain(..valid);
                    if let Some(n) = error.error_len() {
                        text.push('\u{fffd}');
                        pending.drain(..n);
                    } else {
                        break;
                    }
                }
            }
        }
        record.push(stream, text).await;
    }
}

fn project(root: &Path, raw: &str) -> Result<(PathBuf, String)> {
    let root = root.canonicalize().context("workspace root is unavailable to process tools")?;
    let checked = crate::file_edit::workspace_path(&root, raw).context("project path failed workspace boundary validation")?;
    let path = checked.canonicalize().context("project directory is unavailable to process tools")?;
    ensure!(path.is_dir(), "project_path is not a directory");
    let rel = path
        .strip_prefix(&root)?
        .to_string_lossy()
        .replace('\\', "/");
    Ok((path, if rel.is_empty() { ".".into() } else { rel }))
}

fn normalize_windows_runtime_path(raw:&str)->Result<String> {
    if let Some(unc)=raw.strip_prefix(r"\\?\UNC\") {
        ensure!(unc.contains('\\') && !unc.starts_with('\\'),"runtime_bootstrap: malformed extended UNC path cannot be passed to Node/npm");
        return Ok(format!(r"\\{unc}"));
    }
    if let Some(path)=raw.strip_prefix(r"\\?\") {
        let bytes=path.as_bytes();
        ensure!(bytes.len()>=3 && bytes[0].is_ascii_alphabetic() && bytes[1]==b':' && matches!(bytes[2],b'\\'|b'/'),
            "runtime_bootstrap: unsupported Windows device path cannot be safely converted for Node/npm");
        return Ok(path.to_owned());
    }
    ensure!(!raw.starts_with(r"\\.\") && !raw.starts_with(r"\??\"),
        "runtime_bootstrap: unsupported Windows device path cannot be safely converted for Node/npm");
    Ok(raw.to_owned())
}

#[cfg(windows)]
fn external_runtime_path(path:&Path)->Result<OsString> {
    let raw=path.as_os_str().to_string_lossy();
    let normalized=normalize_windows_runtime_path(&raw)?;
    // An ordinary path that is too long for the Win32 API cannot be made safe
    // by stripping the extended prefix; stop before invoking the runtime.
    ensure!(normalized.encode_utf16().count()<32_760,"runtime_bootstrap: converted Windows path exceeds the runtime path limit");
    Ok(OsString::from(normalized))
}
#[cfg(not(windows))]
fn external_runtime_path(path:&Path)->Result<OsString> { Ok(path.as_os_str().to_owned()) }

fn node_npm() -> Result<(PathBuf, PathBuf)> {
    let paths = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let executable = if cfg!(windows) { "node.exe" } else { "node" };
    let node = paths
        .iter()
        .map(|dir| dir.join(executable))
        .find(|path| path.is_file())
        .context("Node.js is not installed or not visible in the host PATH")?;
    let mut directories = vec![node.parent().unwrap().to_path_buf()];
    if let Ok(actual) = node.canonicalize() {
        if let Some(parent) = actual.parent() {
            directories.push(parent.to_path_buf());
        }
    }
    directories.extend(paths);
    if let Some(appdata) = std::env::var_os("APPDATA") {
        directories.push(PathBuf::from(appdata).join("npm"));
    }
    for dir in directories {
        for relative in [
            "node_modules/npm/bin/npm-cli.js",
            "../lib/node_modules/npm/bin/npm-cli.js",
            "../share/nodejs/npm/bin/npm-cli.js",
        ] {
            let cli = dir.join(relative);
            if cli.is_file() {
                return Ok((node, cli.canonicalize()?));
            }
        }
    }
    anyhow::bail!(
        "npm-cli.js was not found beside Node.js or in PATH; install/configure npm in the host environment"
    )
}

pub fn failure_result(name:&str,args:&Value,error:&anyhow::Error)->Value {
    let message=format!("{error:#}");
    let failure_stage=if message.contains("runtime_bootstrap:") {"runtime_bootstrap"}else{"unknown"};
    let diagnosis=if failure_stage=="runtime_bootstrap" {"host_runtime"}else{"unknown"};
    json!({"error":message,"failure_stage":failure_stage,"diagnosis":diagnosis,
        "project_path":args["project_path"],"script":args["script"],"check_key":check_key(name,args),
        "exit_code":Value::Null,"stderr":"","host_fix_guidance":if diagnosis=="host_runtime" {
            "npm CLI or the host Node/npm runtime failed before project script execution. Do not infer project dependency damage from this error; preserve the original diagnostic and check the runtime path/installation."
        } else {"The failure stage is unknown. Use the structured result and original error before deciding what to repair."}})
}

fn runtime_bootstrap(error:impl std::fmt::Display)->anyhow::Error {anyhow::anyhow!("runtime_bootstrap: {error}")}
fn probe_target(args: &Value) -> Result<(Option<String>, Option<u16>)> {
    let url = args
        .get("ready_url")
        .filter(|v| !v.is_null())
        .map(|value| -> Result<String> {
            let raw = value.as_str().context("ready_url must be a URL")?;
            ensure!(raw.len() <= 2000, "ready_url is too long");
            let parsed = reqwest::Url::parse(raw)?;
            ensure!(
                matches!(parsed.scheme(), "http" | "https")
                    && parsed.username().is_empty()
                    && parsed.password().is_none(),
                "ready_url must be local HTTP(S) without credentials"
            );
            ensure!(
                matches!(
                    parsed.host_str(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                ),
                "ready_url must address localhost"
            );
            Ok(raw.to_owned())
        })
        .transpose()?;
    let port = args
        .get("ready_port")
        .filter(|v| !v.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|port| *port > 0 && *port <= 65535)
                .map(|port| port as u16)
                .context("ready_port must be 1..65535")
        })
        .transpose()?;
    Ok((url, port))
}
async fn probe(record: &Record, args: &Value) -> Result<()> {
    let (url, port) = probe_target(args)?;
    let endpoint_changed = {
        let mut meta = record.meta.lock().await;
        if url.is_some() || port.is_some() {
            let changed = meta.ready_url != url || meta.ready_port != port || meta.ready;
            meta.ready_url = url;
            meta.ready_port = port;
            meta.ready = false;
            changed
        } else {
            false
        }
    };
    if endpoint_changed { bump_process_event_generation(); }
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(args["wait_seconds"].as_u64().unwrap_or(0).min(60));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(750))
        .build()?;
    loop {
        let meta = record.meta.lock().await.clone();
        if !meta.running {
            break;
        }
        let port = meta.ready_port.or_else(|| {
            meta.ready_url.as_ref().and_then(|url| reqwest::Url::parse(url).ok()).and_then(|url| url.port_or_known_default())
        });
        let Some(port) = port else { break };
        let host = meta.ready_url.as_ref().and_then(|url| reqwest::Url::parse(url).ok()).and_then(|url| url.host_str().map(|host| host.trim_matches(['[', ']']).to_owned())).unwrap_or_else(|| "127.0.0.1".into());
        let ownership = listener_ownership(port, &host, meta.pid);
        let page = if ownership.owned {
            if let Some(url) = meta.ready_url.clone() { page_status(&client, &url).await }
            else { PageStatus { ready: true, detail: "tcp listener belongs to the managed process group; no HTTP page was requested".into() } }
        } else {
            PageStatus { ready: false, detail: ownership.detail.clone() }
        };
        let ready = ownership.owned && page.ready;
        let readiness_changed = {
            let mut state = record.meta.lock().await;
            let previous = (state.ready, state.listener_owned, state.page_ready);
            state.listener_owned = ownership.owned;
            state.page_ready = page.ready;
            state.readiness_detail = page.detail;
            state.ready = ready && state.running;
            previous != (state.ready, state.listener_owned, state.page_ready)
        };
        if readiness_changed { bump_process_event_generation(); }
        if ready || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    record.persist().await;
    Ok(())
}

struct ListenerOwnership { owned: bool, detail: String }
struct PageStatus { ready: bool, detail: String }

enum EndpointFilter {
    V4(u32),
    V6([u8; 16]),
    Localhost,
    Any,
}

impl EndpointFilter {
    fn parse(host: &str) -> Self {
        let host = host.trim().trim_matches(|ch| ch == '[' || ch == ']');
        if host.eq_ignore_ascii_case("localhost") { return Self::Localhost; }
        if let Ok(std::net::IpAddr::V4(addr)) = host.parse() { return Self::V4(u32::from(addr)); }
        if let Ok(std::net::IpAddr::V6(addr)) = host.parse() { return Self::V6(addr.octets()); }
        Self::Any
    }

    fn match_v4(&self, address: u32) -> BindMatch {
        let loopback = u32::from(std::net::Ipv4Addr::LOCALHOST);
        match self {
            Self::V4(expected) if address == *expected => BindMatch::Exact,
            Self::V4(_) if address == 0 => BindMatch::Wildcard,
            Self::Localhost if address == loopback => BindMatch::Exact,
            Self::Localhost if address == 0 => BindMatch::Wildcard,
            Self::Any => BindMatch::Exact,
            _ => BindMatch::None,
        }
    }

    fn match_v6(&self, address: &[u8; 16]) -> BindMatch {
        let unspecified = [0u8; 16];
        let mut loopback = [0u8; 16];
        loopback[15] = 1;
        match self {
            Self::V6(expected) if address == expected => BindMatch::Exact,
            Self::V6(_) if address == &unspecified => BindMatch::Wildcard,
            Self::Localhost if address == &loopback => BindMatch::Exact,
            Self::Localhost if address == &unspecified => BindMatch::Wildcard,
            Self::Any => BindMatch::Exact,
            _ => BindMatch::None,
        }
    }
}

enum BindMatch { Exact, Wildcard, None }

fn prefer_exact(exact: Vec<u32>, wildcard: Vec<u32>) -> Vec<u32> {
    let chosen = if exact.is_empty() { wildcard } else { exact };
    let mut chosen = chosen;
    chosen.sort_unstable();
    chosen.dedup();
    chosen
}

fn listener_ownership(port: u16, host: &str, root_pid: u32) -> ListenerOwnership {
    match listener_pids(port, host) {
        Err(error) => ListenerOwnership { owned: false, detail: format!("could not identify the process listening on {host}:{port}: {error}") },
        Ok(pids) if pids.is_empty() => ListenerOwnership { owned: false, detail: format!("nothing is listening on {host}:{port}") },
        Ok(pids) => {
            let descriptions = pids.iter().map(|pid| format!("pid {pid} {}", process_name(*pid))).collect::<Vec<_>>().join("; ");
            let host_pid = std::process::id();
            if pids.iter().any(|pid| *pid == host_pid) {
                ListenerOwnership { owned: false, detail: format!("port {port} on {host} belongs to this workspace host ({descriptions}), not the target application") }
            } else if root_pid != 0 && pids.iter().all(|pid| *pid == root_pid || process_is_descendant(*pid, root_pid)) {
                ListenerOwnership { owned: true, detail: format!("listener on {host}:{port} belongs to managed pid {root_pid}: {descriptions}") }
            } else {
                ListenerOwnership { owned: false, detail: format!("port {port} on {host} is owned by another process ({descriptions})") }
            }
        }
    }
}

async fn page_status(client: &reqwest::Client, url: &str) -> PageStatus {
    match client.get(url).send().await {
        Err(error) => PageStatus { ready: false, detail: format!("page request failed: {error}") },
        Ok(response) => {
            let status = response.status();
            let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("").to_owned();
            let body = response.text().await.unwrap_or_default();
            let excerpt: String = body.chars().take(400).collect();
            let lower = excerpt.to_lowercase();
            let broken = lower.contains("cannot get") || lower.contains("wasm") && (lower.contains("error") || lower.contains("failed")) || lower.contains("econnrefused");
            if !status.is_success() || broken {
                PageStatus { ready: false, detail: format!("HTTP {status} content-type {content_type}; body: {excerpt}") }
            } else {
                PageStatus { ready: true, detail: format!("HTTP {status} content-type {content_type}") }
            }
        }
    }
}

#[cfg(windows)]
fn listener_pids(port: u16, host: &str) -> Result<Vec<u32>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER};
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    fn table<T>(family: u32) -> Result<Vec<u8>> {
        unsafe {
            let mut size = 0u32;
            let _ = GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0);
            let mut buffer = vec![0u8; size as usize];
            let status = GetExtendedTcpTable(buffer.as_mut_ptr().cast(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0);
            ensure!(status == 0, "GetExtendedTcpTable failed: {status}");
            Ok(buffer)
        }
    }
    let mut exact = Vec::new();
    let mut wildcard = Vec::new();
    let target = EndpointFilter::parse(host);
    let v4 = table::<()>(AF_INET as u32)?;
    if v4.len() >= std::mem::size_of::<u32>() {
        let count = usize::try_from(u32::from_ne_bytes(v4[..4].try_into().unwrap_or([0; 4]))).unwrap_or(0);
        let rows = unsafe { std::slice::from_raw_parts(v4.as_ptr().add(4) as *const MIB_TCPROW_OWNER_PID, count.min(4096)) };
        for row in rows {
            if u16::from_be(row.dwLocalPort as u16) != port { continue; }
            match target.match_v4(u32::from_be(row.dwLocalAddr)) {
                BindMatch::Exact => exact.push(row.dwOwningPid),
                BindMatch::Wildcard => wildcard.push(row.dwOwningPid),
                BindMatch::None => {}
            }
        }
    }
    let v6 = table::<()>(AF_INET6 as u32)?;
    if v6.len() >= std::mem::size_of::<u32>() {
        let count = usize::try_from(u32::from_ne_bytes(v6[..4].try_into().unwrap_or([0; 4]))).unwrap_or(0);
        let rows = unsafe { std::slice::from_raw_parts(v6.as_ptr().add(4) as *const MIB_TCP6ROW_OWNER_PID, count.min(4096)) };
        for row in rows {
            if u16::from_be(row.dwLocalPort as u16) != port { continue; }
            match target.match_v6(&row.ucLocalAddr) {
                BindMatch::Exact => exact.push(row.dwOwningPid),
                BindMatch::Wildcard => wildcard.push(row.dwOwningPid),
                BindMatch::None => {}
            }
        }
    }
    let _ = std::mem::size_of::<MIB_TCPTABLE_OWNER_PID>() + std::mem::size_of::<MIB_TCP6TABLE_OWNER_PID>();
    Ok(prefer_exact(exact, wildcard))
}
#[cfg(not(windows))]
fn listener_pids(_port: u16, _host: &str) -> Result<Vec<u32>> { anyhow::bail!("listener ownership is only resolved on Windows in this build") }

#[cfg(windows)]
fn process_is_descendant(pid: u32, ancestor: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE { return false; }
        let snapshot = std::os::windows::io::OwnedHandle::from_raw_handle(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parents = std::collections::BTreeMap::new();
        let mut found = Process32FirstW(snapshot.as_raw_handle(), &mut entry) != 0;
        while found {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            found = Process32NextW(snapshot.as_raw_handle(), &mut entry) != 0;
        }
        let mut cursor = pid;
        for _ in 0..12 {
            let Some(parent) = parents.get(&cursor).copied() else { return false };
            if parent == ancestor { return true; }
            if parent == 0 || parent == cursor { return false; }
            cursor = parent;
        }
        false
    }
}
#[cfg(not(windows))]
fn process_is_descendant(_pid: u32, _ancestor: u32) -> bool { false }

#[cfg(windows)]
fn process_name(pid: u32) -> String {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE { return String::new(); }
        let snapshot = std::os::windows::io::OwnedHandle::from_raw_handle(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = Process32FirstW(snapshot.as_raw_handle(), &mut entry) != 0;
        while found {
            if entry.th32ProcessID == pid {
                let end = entry.szExeFile.iter().position(|ch| *ch == 0).unwrap_or(entry.szExeFile.len());
                return String::from_utf16_lossy(&entry.szExeFile[..end]);
            }
            found = Process32NextW(snapshot.as_raw_handle(), &mut entry) != 0;
        }
        String::new()
    }
}
#[cfg(not(windows))]
fn process_name(_pid: u32) -> String { String::new() }

#[cfg(test)]
mod tests {
    use super::{build_process_observation,build_single_process_observation,can_reuse_process_observation,external_runtime_path,launch,listener_ownership,normalize_windows_runtime_path,node_npm,observation_is_fresh,observation_sample_is_current,prefer_exact,PROCESS_OBSERVATION_TTL_MS};
    use std::process::Stdio;
    #[cfg(not(windows))]
    use std::path::Path;

    fn fixture_record(root:&std::path::Path,id:&str,project_path:&str,args:&[&str])->std::sync::Arc<super::Record> {
        let (stop,_receiver)=tokio::sync::mpsc::unbounded_channel();
        let meta=super::Meta {
            process_id:id.to_owned(),pid:1,root:root.canonicalize().unwrap(),project_path:project_path.to_owned(),
            script:"dev".into(),args:args.iter().map(|arg|(*arg).to_owned()).collect(),operation:"script".into(),
            check_key:format!("npm-start:{project_path}:dev"),running:true,ready:false,listener_owned:false,page_ready:false,
            readiness_detail:"pending".into(),script_command:"npm run dev".into(),node_executable:"node".into(),npm_cli:"npm-cli.js".into(),
            failure_stage:String::new(),failure_diagnosis:String::new(),ready_url:None,ready_port:None,started_at:super::now(),
            ended_at:None,exit_code:None,termination_reason:None,background:true,
        };
        std::sync::Arc::new(super::Record {
            meta:tokio::sync::Mutex::new(meta),logs:tokio::sync::Mutex::new(super::Logs::default()),
            persist_guard:tokio::sync::Mutex::new(()),dir:root.to_path_buf(),stop,ended:tokio::sync::Notify::new(),
        })
    }

    #[test]
    fn windows_runtime_paths_normalize_dos_unc_and_reject_device_paths() {
        assert_eq!(normalize_windows_runtime_path(r"\\?\C:\Users\Admin\npm-cli.js").unwrap(),r"C:\Users\Admin\npm-cli.js");
        assert_eq!(normalize_windows_runtime_path(r"\\?\UNC\server\share\npm-cli.js").unwrap(),r"\\server\share\npm-cli.js");
        assert_eq!(normalize_windows_runtime_path(r"C:\Users\Admin\npm-cli.js").unwrap(),r"C:\Users\Admin\npm-cli.js");
        assert!(normalize_windows_runtime_path(r"\\?\GLOBALROOT\Device\HarddiskVolume1\npm-cli.js").is_err());
        assert!(normalize_windows_runtime_path(r"\\.\PhysicalDrive0").is_err());
        #[cfg(not(windows))]
        assert_eq!(external_runtime_path(Path::new("/tmp/node")).unwrap(),std::ffi::OsString::from("/tmp/node"));
    }

    #[tokio::test]
    async fn discovered_npm_cli_runs_through_the_external_runtime_path() {
        let Ok(Ok((node,npm)))=tokio::task::spawn_blocking(node_npm).await else {return};
        let node=external_runtime_path(&node).unwrap();let npm=external_runtime_path(&npm).unwrap();
        let output=tokio::process::Command::new(node).arg(npm).arg("--version")
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().await.unwrap();
        assert!(output.status.success(),"npm CLI failed before a project script: {}",String::from_utf8_lossy(&output.stderr));
        assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty(),"npm CLI returned no version");
    }

    #[tokio::test]
    async fn run_project_script_enters_the_package_script_through_the_normalized_npm_cli() {
        if node_npm().is_err(){return;}
        let root=std::env::current_dir().unwrap().join("target")
            .join(format!("test-npm-script-{}-{}",std::process::id(),super::now()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"),r#"{"scripts":{"dev":"node -e \"console.log('npm-script-entered')\""}}"#).unwrap();
        let result=launch(&root,&serde_json::json!({"project_path":".","script":"dev","timeout_seconds":30}),false,
            &tokio_util::sync::CancellationToken::new()).await;
        let _=std::fs::remove_dir_all(&root);
        let result=result.unwrap();
        assert_eq!(result["status"],0,"stderr: {}",result["stderr"]);
        assert!(result["stdout"].as_str().unwrap_or("").contains("npm-script-entered"),"stdout: {}",result["stdout"]);
        assert!(result["failure_stage"].is_null());
        assert!(result["process_observation"]["input_version"].as_str().is_some());
        assert_eq!(result["process_observation"]["scope"]["coverage"],"single_process");
        assert_eq!(result["process_observation"]["scope"]["operation"],"script");
        assert!(!observation_is_fresh(&root,&serde_json::json!({"project_path":"."}),&result["process_observation"]));
    }

    #[test]
    fn scoped_process_observations_expire_on_inputs_host_events_and_ttl() {
        let workspace=std::env::current_dir().unwrap();
        let project_name=format!("test-process-observation-{}-{}",std::process::id(),super::now());
        let root=workspace.join("target").join(&project_name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"),r#"{"scripts":{"dev":"node server.js"}}"#).unwrap();
        std::fs::write(root.join("package-lock.json"),r#"{"lockfileVersion":3}"#).unwrap();
        std::fs::write(root.join("npm-shrinkwrap.json"),r#"{"lockfileVersion":3}"#).unwrap();
        let args=serde_json::json!({"project_path":format!("target/{project_name}"),"script":"dev","ready_url":"http://127.0.0.1:4321"});
        let mut sample=build_process_observation(&workspace,&args,&[]).unwrap();
        assert!(observation_sample_is_current(&workspace,&sample));

        let mut old_host=sample.clone();old_host["host_instance_id"]=serde_json::json!("previous-host");
        assert!(!observation_sample_is_current(&workspace,&old_host));
        let mut old_events=sample.clone();old_events["process_event_generation"]=serde_json::json!(sample["process_event_generation"].as_u64().unwrap().saturating_sub(1));
        assert!(!observation_sample_is_current(&workspace,&old_events));
        sample["sampled_at"]=serde_json::json!(super::now().saturating_sub(PROCESS_OBSERVATION_TTL_MS+1));
        assert!(!observation_sample_is_current(&workspace,&sample));

        let changed=build_process_observation(&workspace,&args,&[]).unwrap();
        std::fs::write(root.join("package.json"),r#"{"scripts":{"dev":"node changed.js"}}"#).unwrap();
        assert!(!observation_sample_is_current(&workspace,&changed));
        let _=std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn process_lists_mark_coverage_preserve_subprojects_and_ignore_log_limits_in_scope() {
        let root=std::env::current_dir().unwrap().join("target")
            .join(format!("test-process-list-{}-{}",std::process::id(),super::now()));
        for project in ["apps/one","apps/two"] {
            let directory=root.join(project);std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("package.json"),r#"{"scripts":{"dev":"node server.js"}}"#).unwrap();
        }
        let workspace=crate::tools::Workspace::new(&root).unwrap();
        let canonical=workspace.root().to_path_buf();
        let ids=["project_fixture_one_3001","project_fixture_one_3002","project_fixture_two_4001"];
        {
            let mut records=super::registry().lock().await;
            records.records.insert(ids[0].into(),fixture_record(&canonical,ids[0],"apps/one",&["--port","3001"]));
            records.records.insert(ids[1].into(),fixture_record(&canonical,ids[1],"apps/one",&["--port","3002"]));
            records.records.insert(ids[2].into(),fixture_record(&canonical,ids[2],"apps/two",&["--port","4001"]));
        }
        let cancel=tokio_util::sync::CancellationToken::new();
        let workspace_list=super::execute_cancellable(&workspace,"get_project_process",serde_json::json!({}),&cancel).await;
        let workspace_script_list=super::execute_cancellable(&workspace,"get_project_process",serde_json::json!({"script":"dev"}),&cancel).await;
        let project_list=super::execute_cancellable(&workspace,"get_project_process",serde_json::json!({"project_path":"apps/one"}),&cancel).await;
        {
            let mut records=super::registry().lock().await;
            for id in ids {records.records.remove(id);}
        }

        let workspace_list=workspace_list.unwrap();
        let workspace_script_list=workspace_script_list.unwrap();
        let project_list=project_list.unwrap();
        assert_eq!(workspace_list["processes"].as_array().unwrap().len(),3);
        assert!(workspace_list["processes"].as_array().unwrap().iter().any(|process|process["project_path"]=="apps/two"),
            "workspace listing must retain processes from child projects");
        assert_eq!(workspace_list["process_observation"]["scope"]["coverage"],"workspace_list");
        assert_eq!(workspace_script_list["processes"].as_array().unwrap().len(),3,
            "script filtering without project_path remains workspace-wide");
        assert_eq!(workspace_script_list["process_observation"]["scope"]["coverage"],"workspace_list");
        assert_eq!(project_list["processes"].as_array().unwrap().len(),2,"same-script processes with different args must both remain visible");
        let process_args=project_list["processes"].as_array().unwrap().iter().map(|process|process["args"].to_string()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(process_args.len(),2,"the list must retain each instance's distinct script args");
        assert_eq!(project_list["process_observation"]["scope"]["coverage"],"project_list");
        assert!(observation_is_fresh(&canonical,&serde_json::json!({"max_chars":4096}),&workspace_list["process_observation"]),
            "max_chars changes log presentation, not process-list identity");
        assert!(observation_is_fresh(&canonical,&serde_json::json!({"project_path":"apps/one","max_chars":8192}),&project_list["process_observation"]));
        let active_probe=serde_json::json!({"project_path":"apps/one","ready_url":"http://127.0.0.1:4321"});
        assert!(!can_reuse_process_observation(&active_probe),"an endpoint request asks for an active readiness probe");

        let first_process=project_list["processes"][0].clone();
        let single=build_single_process_observation(&canonical,&serde_json::json!({"project_path":"apps/one"}),&[super::process_fact(&first_process)]).unwrap();
        assert_eq!(single["scope"]["coverage"],"single_process");
        assert!(single["scope"]["process_id"].as_str().is_some());
        assert!(!observation_is_fresh(&canonical,&serde_json::json!({"project_path":"apps/one"}),&single),
            "a single process snapshot must not satisfy a complete project list query");
        let install_fact=serde_json::json!({"process_id":"project_fixture_install","project_path":"apps/one","script":"install",
            "args":[],"operation":"install","running":false,"ready":false});
        let install_sample=build_single_process_observation(&canonical,&serde_json::json!({"project_path":"apps/one"}),&[install_fact]).unwrap();
        assert_eq!(install_sample["scope"]["operation"],"install");
        assert!(!observation_is_fresh(&canonical,&serde_json::json!({"project_path":"apps/one"}),&install_sample),
            "an install result cannot mask the complete project process list");
        let _=std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn wait_seconds_bypasses_pending_snapshot_and_waits_for_delayed_readiness() {
        if node_npm().is_err(){return;}
        let root=std::env::current_dir().unwrap().join("target")
            .join(format!("test-delayed-readiness-{}-{}",std::process::id(),super::now()));
        std::fs::create_dir_all(&root).unwrap();
        let port_listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port=port_listener.local_addr().unwrap().port();
        drop(port_listener);
        std::fs::write(root.join("package.json"),r#"{"scripts":{"dev":"node server.js"}}"#).unwrap();
        std::fs::write(root.join("server.js"),format!(
            "const http=require('http'); setTimeout(()=>http.createServer((req,res)=>res.end('ready')).listen({port},'127.0.0.1'),1500); setInterval(()=>{{}},1000);"
        )).unwrap();
        let launch_args=serde_json::json!({"project_path":".","script":"dev","background":true,
            "ready_url":format!("http://127.0.0.1:{port}/"),"wait_seconds":0});
        let launched=launch(&root,&launch_args,false,&tokio_util::sync::CancellationToken::new()).await;
        let Ok(launched)=launched else {let _=std::fs::remove_dir_all(&root);panic!("fixture server did not launch: {launched:?}")};
        let process_id=launched["process_id"].as_str().unwrap().to_owned();
        let workspace=crate::tools::Workspace::new(&root).unwrap();
        let query_args=serde_json::json!({"project_path":".","script":"dev","wait_seconds":0});
        let pending_result=super::execute_cancellable(&workspace,"get_project_process",query_args,&tokio_util::sync::CancellationToken::new()).await;
        let wait_args=serde_json::json!({"project_path":".","script":"dev","wait_seconds":5});
        let pending_sample=pending_result.as_ref().ok().map(|result|result["process_observation"].clone()).unwrap_or_default();
        let reuse_rejected=!observation_is_fresh(workspace.root(),&wait_args,&pending_sample);
        let wait_started=tokio::time::Instant::now();
        let ready_result=super::execute_cancellable(&workspace,"get_project_process",wait_args,&tokio_util::sync::CancellationToken::new()).await;
        let waited=wait_started.elapsed();
        let _=super::execute_cancellable(&workspace,"stop_project_process",serde_json::json!({"process_id":process_id}),&tokio_util::sync::CancellationToken::new()).await;
        super::registry().lock().await.records.remove(launched["process_id"].as_str().unwrap());
        let _=std::fs::remove_dir_all(&root);

        let pending=pending_result.unwrap();
        let ready=ready_result.unwrap();
        assert_eq!(pending["processes"][0]["ready"],false,"fixture must first return a pending snapshot");
        assert_eq!(pending["process_observation"]["scope"]["coverage"],"project_list");
        assert!(reuse_rejected,"a waiting query cannot be satisfied by a pending cached snapshot");
        assert!(waited>=std::time::Duration::from_millis(700),"readiness query returned before delayed listener appeared: {waited:?}");
        assert_eq!(ready["processes"][0]["ready"],true,"wait_seconds query must probe until delayed readiness succeeds");
    }

    #[tokio::test]
    async fn an_open_port_is_attributed_to_its_listener_not_an_unrelated_pid() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ownership = listener_ownership(port, "127.0.0.1", 1);
        assert!(!ownership.owned, "{}", ownership.detail);
        // Restricted Windows test runners may deny GetExtendedTcpTable. In that
        // case the production result is conservatively unowned and cannot pass
        // readiness; PID attribution itself requires a privileged runner.
        if ownership.detail.contains("GetExtendedTcpTable failed: 5") { return; }
        assert!(ownership.detail.contains("pid"), "{}", ownership.detail);
        let other_family = listener_ownership(port, "::1", 1);
        assert!(!other_family.owned, "{}", other_family.detail);
        assert!(other_family.detail.contains("nothing is listening"), "{}", other_family.detail);
        let wildcard = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap();
        let loopback = listener_ownership(port, "127.0.0.1", 1);
        assert!(!loopback.owned, "{}", loopback.detail);
        assert!(loopback.detail.contains("pid"), "{}", loopback.detail);
        assert_eq!(prefer_exact(vec![7], vec![9]), vec![7]);
        assert_eq!(prefer_exact(vec![], vec![9, 9]), vec![9]);
        drop(wildcard);
        drop(listener);
        if std::env::var("LIVE_PORT_CHECK").is_ok() {
            let loopback_live = listener_ownership(3000, "127.0.0.1", 1);
            assert!(!loopback_live.owned, "{}", loopback_live.detail);
            assert!(loopback_live.detail.contains("codex-workspace-mcp"), "{}", loopback_live.detail);
            assert!(!loopback_live.detail.contains("node"), "{}", loopback_live.detail);
            if let Ok(host) = std::env::var("LIVE_PORT_HOST") {
                let editor = listener_ownership(3000, &host, 1);
                assert!(editor.detail.contains("node"), "{}", editor.detail);
                assert!(!editor.owned, "{}", editor.detail);
            }
        }
    }
}

#[cfg(windows)]
struct ProcessGroup(std::os::windows::io::OwnedHandle);
#[cfg(windows)]
impl ProcessGroup {
    fn attach(child: &Child) -> Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*},
        };
        // The child starts suspended: assign the job BEFORE npm can spawn a child.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            ensure!(
                !handle.is_null(),
                "CreateJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            let owned = std::os::windows::io::OwnedHandle::from_raw_handle(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            ensure!(
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32
                ) != 0,
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            ensure!(
                AssignProcessToJobObject(
                    handle,
                    child.raw_handle().context("missing process handle")?
                ) != 0,
                "AssignProcessToJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            ensure!(
                snapshot != INVALID_HANDLE_VALUE,
                "thread snapshot failed: {}",
                std::io::Error::last_os_error()
            );
            let snapshot = std::os::windows::io::OwnedHandle::from_raw_handle(snapshot);
            let mut thread: THREADENTRY32 = std::mem::zeroed();
            thread.dwSize = std::mem::size_of_val(&thread) as u32;
            let pid = child.id().context("missing process id")?;
            let mut found = Thread32First(snapshot.as_raw_handle(), &mut thread) != 0;
            while found {
                if thread.th32OwnerProcessID == pid {
                    let handle = OpenThread(THREAD_SUSPEND_RESUME, 0, thread.th32ThreadID);
                    ensure!(
                        !handle.is_null(),
                        "OpenThread failed: {}",
                        std::io::Error::last_os_error()
                    );
                    let handle = std::os::windows::io::OwnedHandle::from_raw_handle(handle);
                    ensure!(
                        ResumeThread(handle.as_raw_handle()) != u32::MAX,
                        "ResumeThread failed: {}",
                        std::io::Error::last_os_error()
                    );
                    return Ok(Self(owned));
                }
                found = Thread32Next(snapshot.as_raw_handle(), &mut thread) != 0;
            }
            anyhow::bail!("suspended process thread not found")
        }
    }
    fn terminate(&self) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        ensure!(
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.0.as_raw_handle(),
                    1,
                )
            } != 0,
            "TerminateJobObject failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}
#[cfg(unix)]
struct ProcessGroup(i32);
#[cfg(unix)]
impl ProcessGroup {
    fn attach(child: &Child) -> Result<Self> {
        Ok(Self(child.id().context("missing process id")? as i32))
    }
    fn terminate(&self) -> Result<()> {
        let status = unsafe { libc::kill(-self.0, libc::SIGKILL) };
        ensure!(
            status == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "could not stop process group: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

async fn supervise(
    mut child: Child,
    group: ProcessGroup,
    record: Arc<Record>,
    mut stop: mpsc::UnboundedReceiver<String>,
) {
    let out = child
        .stdout
        .take()
        .map(|reader| tokio::spawn(pump(reader, record.clone(), "stdout")));
    let err = child
        .stderr
        .take()
        .map(|reader| tokio::spawn(pump(reader, record.clone(), "stderr")));
    let (status, reason) = tokio::select! {
        result=child.wait()=>(result,None),
        reason=stop.recv()=>{
            let reason=reason.unwrap_or_else(||"host_shutdown".into());
            if let Err(error)=group.terminate(){record.push("host",format!("could not terminate process group: {error}\n")).await;let _=child.start_kill();}
            (child.wait().await,Some(reason))
        }
    };
    drop(group); // Also closes surviving descendants' output pipes.
    for reader in [out, err].into_iter().flatten() {
        let _ = reader.await;
    }
    let (exit_code, reason) = match status {
        Ok(status) => (status.code(), reason),
        Err(error) => {
            record
                .push("host", format!("process wait failed: {error}\n"))
                .await;
            (None, Some("wait_error".into()))
        }
    };
    let stderr={record.logs.lock().await.chunks.iter().filter(|chunk|chunk["stream"]=="stderr")
        .filter_map(|chunk|chunk["text"].as_str()).collect::<String>().to_lowercase()};
    let npm_bootstrap_failure=exit_code.is_some_and(|code|code!=0)
        && (stderr.contains("module_not_found")||stderr.contains("cannot find module"))
        && ["npm-cli","node_modules/npm","npm\\lib\\cli","npm/lib/cli"].iter().any(|hint|stderr.contains(hint));
    let (failure_stage,failure_diagnosis)=if npm_bootstrap_failure {("runtime_bootstrap","host_runtime")}
        else if exit_code.is_some_and(|code|code!=0) && reason.as_deref()==Some("timeout") {
            if record.meta.lock().await.operation=="install" {("dependency_install","dependency_install")}else{("project_script","project_script")}
        } else if exit_code.is_some_and(|code|code!=0) {
            if record.meta.lock().await.operation=="install" {("dependency_install","dependency_install")}else{("project_script","project_script")}
        } else {("","")};
    {
        let mut meta = record.meta.lock().await;
        meta.running = false;
        meta.ready = false;
        meta.ended_at = Some(now());
        meta.exit_code = exit_code;
        meta.termination_reason = reason;
        meta.failure_stage=failure_stage.to_owned();meta.failure_diagnosis=failure_diagnosis.to_owned();
    }
    record.persist().await;
    record.ended.notify_waiters();
    bump_process_event_generation();
}

async fn launch(
    root: &Path,
    args: &Value,
    install: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    let root = root.canonicalize()?;
    ensure!(
        args.get("project_path")
            .is_none_or(|value| value.is_string()),
        "project_path must be a string"
    );
    let (dir, relative) = project(&root, args["project_path"].as_str().unwrap_or("."))?;
    let manifest_path = dir.join("package.json");
    ensure!(
        tokio::fs::metadata(&manifest_path)
            .await
            .context("project_path has no readable package.json")?
            .len()
            <= 4 * 1024 * 1024,
        "package.json exceeds 4 MiB"
    );
    let manifest: Value = serde_json::from_slice(&tokio::fs::read(manifest_path).await?)?;
    let script = if install {
        "install".to_owned()
    } else {
        args["script"]
            .as_str()
            .filter(|s| !s.is_empty() && !s.starts_with('-') && s.len() <= 160)
            .context("script is required")?
            .to_owned()
    };
    if !install {
        ensure!(
            manifest["scripts"][&script].is_string(),
            "script '{script}' is absent in package.json; available scripts: {}",
            manifest["scripts"]
                .as_object()
                .map(|scripts| scripts.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .join(", ")
        );
    }
    let extra: Vec<String> = args
        .get("args")
        .map(|args| serde_json::from_value(args.clone()))
        .transpose()?
        .unwrap_or_default();
    ensure!(
        extra.len() <= 32 && extra.iter().all(|s| s.len() <= 2000 && !s.contains('\0')),
        "script args are too large or invalid"
    );
    ensure!(
        !install || extra.is_empty(),
        "installation does not accept script args"
    );
    let background = args["background"].as_bool().unwrap_or(false);
    let (ready_url, ready_port) = probe_target(args)?;
    ensure!(
        !install || ready_url.is_none() && ready_port.is_none(),
        "dependency installation has an exit result, not a server readiness endpoint"
    );
    let (node, npm) = tokio::task::spawn_blocking(node_npm).await.map_err(runtime_bootstrap)?.map_err(runtime_bootstrap)?;
    let node_arg=external_runtime_path(&node).map_err(runtime_bootstrap)?;
    let npm_arg=external_runtime_path(&npm).map_err(runtime_bootstrap)?;
    let cwd_arg=external_runtime_path(&dir).map_err(runtime_bootstrap)?;
    let disk_dir =
        crate::file_edit::workspace_path(&root, ".codex-workspace-mcp/project-processes")?;
    tokio::fs::create_dir_all(&disk_dir).await?;
    let mut registry = registry().lock().await;
    if background {
        for existing in registry.records.values() {
            let meta = existing.meta.lock().await.clone();
            if meta.running
                && meta.root == root
                && meta.project_path == relative
                && meta.script == script
                && meta.args == extra
                && meta.operation == if install { "install" } else { "script" }
            {
                ensure!(
                    ready_url.is_none() || meta.ready_url == ready_url,
                    "existing process uses a different readiness URL"
                );
                ensure!(
                    ready_port.is_none() || meta.ready_port == ready_port,
                    "existing process uses a different readiness port"
                );
                let existing = existing.clone();
                drop(registry);
                probe(&existing, args).await?;
                let mut result = existing.snapshot(args).await;
                result["reused"] = json!(true);
                let fact = process_fact(&result);
                result["process_observation"] = build_single_process_observation(&root, args, &[fact])?;
                return Ok(result);
            }
        }
    }
    let mut running = 0;
    for record in registry.records.values() {
        if record.meta.lock().await.running {
            running += 1;
        }
    }
    ensure!(
        running < 16,
        "host already manages sixteen running project processes; stop unused ones"
    );
    if registry.records.len() >= 128 {
        let mut oldest = None;
        for (id, record) in &registry.records {
            if !record.meta.lock().await.running {
                oldest = Some(id.clone());
                break;
            }
        }
        if let Some(id) = oldest {
            registry.records.remove(&id);
        }
    }
    // Reject an already-open endpoint before launch; it may belong to another app.
    let port = ready_port.or_else(|| {
        ready_url
            .as_ref()
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.port_or_known_default())
    });
    if let Some(port) = port {
        let host = ready_url
            .as_ref()
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.trim_matches(['[', ']']).to_owned())
            })
            .unwrap_or_else(|| "127.0.0.1".into());
        let occupied = tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect((host.as_str(), port))
        ).await.is_ok_and(|v| v.is_ok());
        if occupied {
            let owner = listener_ownership(port, &host, 0);
            anyhow::bail!(
                "readiness port {port} is already occupied ({}); choose a free port supported by the script that actually listens, or report this conflict. Do not stop an unrelated host process and do not keep probing this address",
                owner.detail
            );
        }
    }
    let mut command = Command::new(node_arg);
    command.arg(npm_arg);
    let mode = args["mode"].as_str().unwrap_or("auto");
    if install {
        ensure!(
            matches!(mode, "auto" | "install" | "ci"),
            "mode must be auto/install/ci"
        );
        let locked =
            dir.join("package-lock.json").is_file() || dir.join("npm-shrinkwrap.json").is_file();
        ensure!(
            mode != "ci" || locked,
            "npm ci requires an existing npm lockfile"
        );
        command
            .arg(if mode == "ci" || mode == "auto" && locked {
                "ci"
            } else {
                "install"
            })
            .args(["--no-audit", "--no-fund"]);
    } else {
        command.args(["run", &script]);
        if !extra.is_empty() {
            command.arg("--").args(&extra);
        }
    }
    command
        .current_dir(cwd_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !install {
        command.env("FORCE_COLOR", "0");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    ensure!(!cancel.is_cancelled(), "project execution cancelled");
    let mut child = command.spawn().context("runtime_bootstrap: could not launch Node/npm")?;
    let group = match ProcessGroup::attach(&child) {
        Ok(group) => group,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(runtime_bootstrap(error));
        }
    };
    let pid = child.id().context("missing child id")?;
    let id = format!(
        "project_{}_{}",
        now(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    let (stop, receiver) = mpsc::unbounded_channel();
    let check_key = check_key(
        if install {
            "install_dependencies"
        } else {
            "run_project_script"
        },
        &json!({"project_path":relative,"script":script,"args":extra,"background":background}),
    );
    let script_command = if install { String::new() } else { manifest["scripts"][&script].as_str().unwrap_or("").chars().take(500).collect() };
    let record = Arc::new(Record {
        meta: Mutex::new(Meta {
            process_id: id.clone(),
            pid,
            root:root.clone(),
            project_path: relative,
            script,
            args: extra,
            operation: if install { "install" } else { "script" }.into(),
            check_key,
            running: true,
            ready: false,
            listener_owned: false,
            page_ready: false,
            readiness_detail: String::new(),
            script_command,
            node_executable: node.to_string_lossy().into_owned(),
            npm_cli: npm.to_string_lossy().into_owned(),
            failure_stage:String::new(),
            failure_diagnosis:String::new(),
            ready_url,
            ready_port,
            started_at: now(),
            ended_at: None,
            exit_code: None,
            termination_reason: None,
            background,
        }),
        logs: Mutex::new(Logs::default()),
        persist_guard: Mutex::new(()),
        dir: disk_dir,
        stop: stop.clone(),
        ended: Notify::new(),
    });
    registry.records.insert(id, record.clone());
    bump_process_event_generation();
    tokio::spawn(supervise(child, group, record.clone(), receiver));
    drop(registry);
    let mut lease = RequestLease(Some(stop));
    record.persist().await;
    if background {
        let mut probe_args = args.clone();
        probe_args["wait_seconds"] = json!(args["wait_seconds"].as_u64().unwrap_or(15).min(60));
        let no_endpoint = {
            let meta = record.meta.lock().await;
            meta.ready_url.is_none() && meta.ready_port.is_none()
        };
        if no_endpoint {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::select! {
            result=probe(&record,&probe_args)=>result?,
            _=cancel.cancelled()=>{
                let _=record.stop.send("request_cancelled".into());
                let _=tokio::time::timeout(Duration::from_secs(10),record.wait()).await;
                anyhow::bail!("project launch cancelled");
            }
        }
        lease.0 = None;
        let mut result=record.snapshot(args).await;
        let fact=process_fact(&result);
        result["process_observation"]=build_single_process_observation(&root,args,&[fact])?;
        return Ok(result);
    }
    let timeout = args["timeout_seconds"]
        .as_u64()
        .unwrap_or(if install { 600 } else { 120 })
        .clamp(1, 1800);
    let stop_reason = tokio::select! {
        result=tokio::time::timeout(Duration::from_secs(timeout),record.wait())=>if result.is_err(){Some("timeout")}else{None},
        _=cancel.cancelled()=>Some("request_cancelled"),
    };
    if let Some(reason) = stop_reason {
        let _ = record.stop.send(reason.into());
        let _ = tokio::time::timeout(Duration::from_secs(10), record.wait()).await;
    }
    let mut result = record.snapshot(args).await;
    let fact=process_fact(&result);
    result["process_observation"]=build_single_process_observation(&root,args,&[fact])?;
    lease.0 = None;
    if let Some(reason) = stop_reason {
        result["error"] = json!(if reason == "timeout" {
            format!("operation timed out after {timeout}s")
        } else {
            "operation cancelled".into()
        });
    }
    Ok(result)
}

async fn find(root: &Path, id: &str) -> Result<Arc<Record>> {
    ensure!(
        id.starts_with("project_")
            && id.len() <= 100
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "invalid process_id"
    );
    let root = root.canonicalize()?;
    let record = registry().lock().await.records.get(id).cloned();
    if let Some(record) = record {
        ensure!(
            record.meta.lock().await.root == root,
            "process belongs to a different workspace"
        );
        return Ok(record);
    }
    // After host restart, old PIDs are never treated as owned processes.
    let dir = crate::file_edit::workspace_path(&root, ".codex-workspace-mcp/project-processes")?;
    let mut meta: Meta = serde_json::from_slice(
        &tokio::fs::read(dir.join(format!("{id}.json")))
            .await
            .context("managed process not found")?,
    )?;
    ensure!(
        meta.root == root,
        "process belongs to a different workspace"
    );
    if meta.running {
        meta.running = false;
        meta.ready = false;
        meta.termination_reason = Some("host_restart".into());
    }
    let (stop, _receiver) = mpsc::unbounded_channel();
    let record = Arc::new(Record {
        meta: Mutex::new(meta),
        logs: Mutex::new(Logs::default()),
        persist_guard: Mutex::new(()),
        dir: dir.clone(),
        stop,
        ended: Notify::new(),
    });
    if let Ok(mut file) = tokio::fs::File::open(dir.join(format!("{id}.log"))).await {
        use tokio::io::AsyncSeekExt;
        if let Ok(meta) = file.metadata().await {
            let _ = file
                .seek(std::io::SeekFrom::Start(meta.len().saturating_sub(60_000)))
                .await;
        }
        let mut bytes = Vec::new();
        file.take(60_000).read_to_end(&mut bytes).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut logs = record.logs.lock().await;
        logs.chars = text.chars().count();
        let chars = text.chars().collect::<Vec<_>>();
        for chunk in chars.chunks(4096) {
            logs.sequence += 1;
            let seq = logs.sequence;
            logs.chunks.push_back(json!({"seq":seq,"stream":"history","text":chunk.iter().collect::<String>(),"cursor_reset":true}));
        }
    }
    Ok(record)
}

pub async fn execute(
    workspace: &crate::tools::Workspace,
    name: &str,
    args: Value,
) -> Result<Value> {
    execute_cancellable(
        workspace,
        name,
        args,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
}
pub async fn execute_cancellable(
    workspace: &crate::tools::Workspace,
    name: &str,
    args: Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    ensure!(args.is_object(), "tool arguments must be an object");
    ensure!(
        args.get("process_id").is_none_or(|id| id.is_string()),
        "process_id must be a string"
    );
    ensure!(!cancel.is_cancelled(), "project execution cancelled");
    if name == "get_project_process" && args["force_refresh"] == true {
        ensure!(args["observation_purpose"].as_str().is_some_and(|reason| !reason.trim().is_empty() && reason.chars().count() <= 300),
            "force_refresh requires a concrete observation_purpose (maximum 300 characters)");
    }
    match name {
        "install_dependencies" => launch(workspace.root(), &args, true, cancel).await,
        "run_project_script" => launch(workspace.root(), &args, false, cancel).await,
        "get_project_process" => {
            if let Some(id) = args["process_id"].as_str() {
                let record = find(workspace.root(), id).await?;
                let meta=record.meta.lock().await.clone();
                ensure!(args.get("project_path").and_then(Value::as_str).is_none_or(|path|meta.project_path==path.replace('\\',"/")),
                    "process_id does not belong to the requested project_path scope");
                ensure!(args.get("script").and_then(Value::as_str).is_none_or(|script|meta.script==script),
                    "process_id does not belong to the requested script scope");
                probe(&record, &args).await?;
                let mut result=record.snapshot(&args).await;
                let fact=process_fact(&result);
                let mut scoped_args=args.clone();
                if scoped_args.get("project_path").is_none(){scoped_args["project_path"]=json!(meta.project_path);}
                if scoped_args.get("script").is_none(){scoped_args["script"]=json!(meta.script);}
                result["process_observation"]=build_single_process_observation(workspace.root(),&scoped_args,&[fact])?;
                Ok(result)
            } else {
                let root = workspace.root().canonicalize()?;
                let records = registry()
                    .lock()
                    .await
                    .records
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut list = Vec::new();
                let project_filter=args.get("project_path").and_then(Value::as_str).map(|path|path.replace('\\',"/"));
                for record in records {
                    if record.meta.lock().await.root == root {
                        let meta=record.meta.lock().await.clone();
                        if project_filter.as_ref().is_some_and(|path|meta.project_path!=*path)
                            || args.get("script").and_then(Value::as_str).is_some_and(|script|meta.script!=script)
                        {
                            continue;
                        }
                        if args.get("ready_url").is_some() || args.get("ready_port").is_some()
                            || args["wait_seconds"].as_u64().unwrap_or(0)>0 {
                            probe(&record,&args).await?;
                        }
                        list.push(record.snapshot(&json!({"after_seq":u64::MAX})).await);
                    }
                }
                let facts=list.iter().map(process_fact).collect::<Vec<_>>();
                let mut result=json!({"processes":list,"guidance":"Use process_id and after_seq=next_seq to read incremental logs; provide ready_url or ready_port to verify a running service."});
                result["process_observation"]=build_process_observation(workspace.root(),&args,&facts)?;
                Ok(result)
            }
        }
        "stop_project_process" => {
            let id = args["process_id"]
                .as_str()
                .context("process_id is required")?;
            let record = find(workspace.root(), id).await?;
            if record.meta.lock().await.running {
                record
                    .stop
                    .send("requested_stop".into())
                    .context("process supervisor unavailable")?;
                tokio::time::timeout(Duration::from_secs(10), record.wait())
                    .await
                    .context("process stop is still pending; query its process_id")?;
            }
            Ok(record.snapshot(&args).await)
        }
        _ => anyhow::bail!("unknown project process tool"),
    }
}

pub fn definitions() -> Vec<Value> {
    let project = json!({"type":"string","default":".","description":"Project directory inside this workspace containing package.json."});
    let process_project = json!({"type":"string","description":"Restrict this process list to one project directory. Omit to list processes across the workspace, including subprojects."});
    let background = json!({"type":"boolean","default":false,"description":"Keep the process managed in the background; return process_id for status/logs/stop."});
    let wait = json!({"type":"integer","minimum":0,"maximum":60,"description":"Seconds to wait for the readiness endpoint; background launch defaults to 15, status query to 0."});
    let url = json!({"type":"string","description":"Local HTTP(S) URL for process-scoped startup readiness. Use http_probe for generic URL/API reachability independent of process ownership."});
    let port = json!({"type":"integer","minimum":1,"maximum":65535,"description":"Local TCP readiness port, used when ready_url is omitted."});
    let logs = json!({"type":"integer","minimum":4096,"maximum":60000,"default":12000});
    vec![
        json!({"name":"install_dependencies","description":"Install this npm project's dependencies through Node/npm directly. auto uses ci with an npm lockfile, otherwise install. Returns check_key=npm-install:<project_path>, exit code or a managed background process. No shell command or terminal window required.","inputSchema":{"type":"object","properties":{"project_path":project,"mode":{"type":"string","enum":["auto","install","ci"],"default":"auto"},"background":background,"timeout_seconds":{"type":"integer","minimum":1,"maximum":1800,"default":600},"max_chars":logs}}}),
        json!({"name":"run_project_script","description":"Run a named package.json npm script via Node/npm. Use foreground for build/check and background=true for dev/start. A running process is NOT proof of readiness: supply ready_url/ready_port to verify that this Agent-managed listener belongs to the process group, or query get_project_process later. Use http_probe for generic URL/API reachability independent of process ownership. Returns check_key=npm:<project_path>:<script> for foreground or npm-start:<project_path>:<script> for background (args append their JSON), exit code/logs or process_id.","inputSchema":{"type":"object","required":["script"],"properties":{"project_path":project,"script":{"type":"string"},"args":{"type":"array","maxItems":32,"items":{"type":"string"}},"background":background,"timeout_seconds":{"type":"integer","minimum":1,"maximum":1800,"default":120},"ready_url":url,"ready_port":port,"wait_seconds":wait,"max_chars":logs}}}),
        json!({"name":"get_project_process","description":"List/query only Agent-managed process status, ownership and incremental logs. Without project_path this lists managed processes across the workspace, including subprojects; with project_path it lists that project. An empty list means no process is managed here; it does not establish whether any URL is reachable. Use http_probe for general local URL/API reachability, including services started outside Agent. Every list result carries an observation whose coverage distinguishes workspace_list from project_list; a single_process result cannot stand in for either list. ready_url/ready_port performs a process-scoped startup readiness check and verifies listener ownership; it is not a generic endpoint probe. Reuse a fresh matching list observation from an earlier work packet for status-only queries; set force_refresh=true only when a new process observation is needed for a concrete reason. Pass after_seq=previous next_seq for incremental logs. Old process IDs after host restart are historical and never used to terminate reused OS PIDs.","inputSchema":{"type":"object","properties":{"process_id":{"type":"string"},"project_path":process_project,"script":{"type":"string"},"after_seq":{"type":"integer","minimum":0},"max_chars":logs,"ready_url":url,"ready_port":port,"wait_seconds":wait,"force_refresh":{"type":"boolean","default":false,"description":"Bypass fresh process state reuse only for a concrete new observation purpose."},"observation_purpose":{"type":"string","maxLength":300,"description":"Why this is a new process observation rather than reuse, required when force_refresh=true."}}}}),
        json!({"name":"stop_project_process","description":"Stop only an owned project process by process_id, including its descendant processes. Repeating stop on an exited process returns its actual exit state.","inputSchema":{"type":"object","required":["process_id"],"properties":{"process_id":{"type":"string"},"max_chars":logs}}}),
    ]
}
