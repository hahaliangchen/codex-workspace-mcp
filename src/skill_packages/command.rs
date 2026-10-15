//! Package-owned JSON-RPC subprocess tools. Static argv; no shell or interpolation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
}
fn timeout() -> u64 {
    60
}
impl CommandTool {
    pub fn validate(&self, root: &Path) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 128
                && self
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid command tool name"
        );
        ensure!(
            !self.description.trim().is_empty() && self.input_schema["type"] == "object",
            "command requires a description and object input schema"
        );
        ensure!(
            !self.program.is_empty()
                && !self.program.contains('\0')
                && self.args.len() <= 64
                && self.args.iter().all(|arg| !arg.contains('\0'))
                && self.args.iter().map(String::len).sum::<usize>() <= 32000,
            "invalid command argv"
        );
        ensure!(
            (1..=600).contains(&self.timeout_seconds),
            "command timeout must be 1..600 seconds"
        );
        if self.program.contains('/') || self.program.contains('\\') {
            ensure!(
                crate::file_edit::workspace_path(root, &self.program)?.is_file(),
                "package executable missing"
            );
        }
        Ok(())
    }
    pub fn definition(&self) -> Value {
        json!({"name":self.name,"description":self.description,"inputSchema":self.input_schema})
    }
}
async fn bounded(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    (&mut pipe).take(262145).read_to_end(&mut data).await?;
    ensure!(data.len() <= 262144, "package tool output exceeds 256 KiB");
    Ok(data)
}
pub async fn execute(
    root: &Path,
    workspace: &Path,
    tool: &CommandTool,
    args: Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(args.is_object(), "tool arguments must be an object");
    let program = if tool.program.contains('/') || tool.program.contains('\\') {
        crate::file_edit::workspace_path(root, &tool.program)?
    } else {
        tool.program.clone().into()
    };
    let mut command = Command::new(program);
    command
        .args(&tool.args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::project_process::prepare_process_group(&mut command);
    let mut child = command.spawn().context("starting package tool")?;
    let group = crate::project_process::ProcessGroup::attach(&child)?;
    let mut stdin = child.stdin.take().context("tool stdin missing")?;
    let stdout = child.stdout.take().context("tool stdout missing")?;
    let stderr = child.stderr.take().context("tool stderr missing")?;
    let request = serde_json::to_vec(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool.name,"arguments":args,"workspace_root":workspace}}),
    )?;
    ensure!(request.len() <= 1048576, "package tool input exceeds 1 MiB");
    let io = async {
        tokio::try_join!(
            async {
                stdin.write_all(&request).await?;
                stdin.write_all(b"\n").await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            bounded(stdout),
            bounded(stderr),
            async { Ok::<_, anyhow::Error>(child.wait().await?) }
        )
    };
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(tool.timeout_seconds), io) => result.context("package tool timed out")?,
        _ = cancel.cancelled() => Err(anyhow::anyhow!("skill package stopped during tool execution")),
    };
    // Also stop descendants when a child exits but keeps an output pipe open.
    let _ = group.terminate();
    let ((), stdout, stderr, status) = result?;
    ensure!(
        status.success(),
        "package tool exited {status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    let response: Value = serde_json::from_slice(&stdout)
        .context("package tool must return one JSON-RPC response on stdout")?;
    ensure!(
        response["jsonrpc"] == "2.0" && response["id"] == 1,
        "invalid package JSON-RPC response"
    );
    if let Some(error) = response.get("error") {
        anyhow::bail!("package tool error: {error}");
    }
    response
        .get("result")
        .cloned()
        .context("package tool result missing")
}
