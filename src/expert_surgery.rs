use std::path::{Path, PathBuf};
use std::process::Command;

use reqwest::Client;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::rust_index::ReadRustSymbolRequest;
use crate::tools::Workspace;

#[derive(Debug, Deserialize)]
pub struct ExpertCodeSurgeryRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub symbol_id: String,
    pub instruction: String,
    #[serde(default)]
    pub related_symbol_ids: Vec<String>,
    #[serde(default)]
    pub architecture_query: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub expert_url: Option<String>,
    #[serde(default)]
    pub expert_api_key: Option<String>,
    #[serde(default)]
    pub expert_model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExpertCodeSurgeryResponse {
    pub symbol_id: String,
    pub file_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub dry_run: bool,
    pub fixed_prefix_chars: usize,
    pub volatile_chars: usize,
    pub related_blocks: Vec<RelatedCodeBlock>,
    pub replacement_bytes: usize,
    pub syntax_ok: bool,
    pub fmt_status: VerificationStatus,
    pub check_status: VerificationStatus,
    pub patch: SearchReplacePatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SurgeryEvent {
    ProModelInvoked,
    ProModelGraphDone { elapsed_ms: u64 },
    PreWriteVerificationStarted,
    FileConsistentApproved,
    OffsetDriftDetected,
    ThreeWayRelocationSuccess { byte_range: (usize, usize) },
    HardConflictEncountered,
    FlashResolverStarted,
    FlashResolverSuccess,
    LocalLintStarted,
    SyntaxTreeVerified,
    CargoCheckPassed,
    TransactionRolledBack { reason: String },
}

pub fn emit_event(event: SurgeryEvent) {
    if let Ok(sender) = crate::agent_runtime::SURGERY_SENDER.try_with(|s| s.clone()) {
        let _ = sender.try_send(event);
    }
}

#[derive(Debug, Serialize)]
pub struct VerificationStatus {
    pub ran: bool,
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchReplacePatch {
    pub search: String,
    pub replace: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedCodeBlock {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub reason: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct ExpertProvider {
    pub url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug)]
struct SymbolSpan {
    id: String,
    name: String,
    kind: String,
    file_path: String,
    start_line: usize,
    end_line: usize,
    signature: String,
    docstring: String,
    byte_start: usize,
    byte_end: usize,
    source: String,
}

pub async fn run_expert_code_surgery(
    workspace: &Workspace,
    request: ExpertCodeSurgeryRequest,
) -> anyhow::Result<ExpertCodeSurgeryResponse> {
    let workspace = workspace.with_root(request.workspace_root.as_deref())?;
    crate::rust_index::index_workspace(workspace.root())?;
    let symbol = load_rust_symbol_span(&workspace, &request.symbol_id)?;
    let related_blocks =
        load_related_rust_blocks(&workspace, &symbol, &request.related_symbol_ids)?;
    let architecture_memory =
        load_architecture_memory(&workspace, request.architecture_query.as_deref())?;
    let symbol_contexts = load_symbol_contexts(&workspace, &symbol)?;

    let fixed_prefix = build_fixed_prefix(&architecture_memory, &symbol_contexts);
    let volatile = build_volatile_payload(&symbol, &related_blocks, &request.instruction);
    let provider = load_expert_provider(&request)?;

    let path = workspace.root().join(&symbol.file_path);
    let original_file_content_before_await = std::fs::read_to_string(&path)?;

    emit_event(SurgeryEvent::ProModelInvoked);
    let start_time = std::time::Instant::now();
    let raw_patch = call_expert_model(&provider, &fixed_prefix, &volatile).await?;
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    emit_event(SurgeryEvent::ProModelGraphDone { elapsed_ms });
    let patch = parse_search_replace_patch(&raw_patch)?;

    if crate::conflict_resolver::normalize_newlines(&patch.search).trim() != crate::conflict_resolver::normalize_newlines(&symbol.source).trim() {
        anyhow::bail!(
            "expert patch SEARCH block does not match indexed symbol span; refusing fuzzy merge"
        );
    }

    let mut replacement = crate::conflict_resolver::normalize_newlines(&patch.replace);
    if symbol.source.ends_with('\n') && !replacement.ends_with('\n') {
        replacement.push('\n');
    }

    emit_event(SurgeryEvent::PreWriteVerificationStarted);
    let current_disk_content = std::fs::read_to_string(&path)?;

    let mut file_content = current_disk_content.clone();

    if current_disk_content == original_file_content_before_await {
        file_content.replace_range(symbol.byte_start..symbol.byte_end, &replacement);
        emit_event(SurgeryEvent::FileConsistentApproved);
    } else {
        emit_event(SurgeryEvent::OffsetDriftDetected);
        if let Some((start_byte, end_byte)) = crate::conflict_resolver::find_unique_normalized_match(&current_disk_content, &patch.search) {
            file_content.replace_range(start_byte..end_byte, &replacement);
            emit_event(SurgeryEvent::ThreeWayRelocationSuccess { byte_range: (start_byte, end_byte) });
        } else {
            emit_event(SurgeryEvent::HardConflictEncountered);
            emit_event(SurgeryEvent::FlashResolverStarted);

            let (ours_start, ours_end) = crate::conflict_resolver::locate_drifted_symbol(
                &current_disk_content,
                &symbol.name,
                &symbol.signature,
                symbol.byte_start,
            );
            let ours_block = &current_disk_content[ours_start..ours_end];

            let flash_provider = crate::conflict_resolver::load_default_provider()?;
            let merged = crate::conflict_resolver::call_flash_merge(&flash_provider, &patch.search, &replacement, ours_block).await?;

            file_content.replace_range(ours_start..ours_end, &merged);
            emit_event(SurgeryEvent::FlashResolverSuccess);
        }
    }

    // AST syntax validation in-memory
    emit_event(SurgeryEvent::LocalLintStarted);
    if let Err(err) = validate_rust_syntax(&file_content) {
        emit_event(SurgeryEvent::TransactionRolledBack {
            reason: format!("AST syntax validation failed: {}", err),
        });
        anyhow::bail!("AST syntax validation failed after patch: {}", err);
    }
    emit_event(SurgeryEvent::SyntaxTreeVerified);

    let mut fmt_status = VerificationStatus {
        ran: false,
        success: true,
        output: String::new(),
    };
    let mut check_status = VerificationStatus {
        ran: false,
        success: true,
        output: String::new(),
    };

    if !request.dry_run {
        let verify_res = write_and_verify(&path, workspace.root(), &file_content, &current_disk_content).await?;
        fmt_status = verify_res.0;
        check_status = verify_res.1;
    }

    Ok(ExpertCodeSurgeryResponse {
        symbol_id: symbol.id,
        file_path: symbol.file_path,
        start_byte: symbol.byte_start,
        end_byte: symbol.byte_end,
        dry_run: request.dry_run,
        fixed_prefix_chars: fixed_prefix.chars().count(),
        volatile_chars: volatile.chars().count(),
        related_blocks,
        replacement_bytes: replacement.len(),
        syntax_ok: true,
        fmt_status,
        check_status,
        patch,
    })
}

fn load_expert_provider(request: &ExpertCodeSurgeryRequest) -> anyhow::Result<ExpertProvider> {
    if let (Some(url), Some(api_key), Some(model)) = (
        &request.expert_url,
        &request.expert_api_key,
        &request.expert_model,
    ) {
        return Ok(ExpertProvider {
            url: url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
        });
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let config_path = exe_dir.join("ai_proxy_config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let default_provider = config
        .get("default_provider")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing default_provider in ai_proxy_config.json"))?;
    let provider_name = config
        .get("expert_provider")
        .and_then(Value::as_str)
        .unwrap_or(default_provider);
    let provider = config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(provider_name))
        .ok_or_else(|| anyhow::anyhow!("expert provider '{}' not found", provider_name))?;
    let model = config
        .get("expert_model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            provider
                .get("model_map")
                .and_then(Value::as_object)
                .and_then(|map| map.values().next())
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("expert provider has no model_map or expert_model"))?;

    Ok(ExpertProvider {
        url: provider
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("expert provider missing url"))?
            .trim_end_matches('/')
            .to_string(),
        api_key: provider
            .get("api_key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("expert provider missing api_key"))?
            .to_string(),
        model,
    })
}

async fn call_expert_model(
    provider: &ExpertProvider,
    fixed_prefix: &str,
    volatile: &str,
) -> anyhow::Result<String> {
    let body = json!({
        "model": provider.model,
        "stream": false,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": fixed_prefix
            },
            {
                "role": "user",
                "content": volatile
            }
        ]
    });

    let response = Client::new()
        .post(format!("{}/chat/completions", provider.url))
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "expert model request failed: status={} body={}",
            status,
            text
        );
    }
    let value: Value = serde_json::from_str(&text)?;
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("expert model response missing message content"))
}

fn build_fixed_prefix(architecture_memory: &str, symbol_contexts: &str) -> String {
    format!(
        "You are expert_code_surgery, a stateless atomic code surgery compiler. \
You have no tools, no memory, no authority to ask for more context, and no permission to rewrite whole files. \
Return exactly one structured diff block and no prose:\n\
<<<<<<< SEARCH\n<exact input code block>\n=======\n<replacement code block>\n>>>>>>> REPLACE\n\n\
The SEARCH block must be byte-span equivalent to the provided symbol source after newline normalization. \
The REPLACE block must contain only the new source for that same symbol span.\n\n\
[ARCHITECTURE MEMORY]\n{}\n\n[SYMBOL BUSINESS CONTEXT]\n{}\n",
        architecture_memory, symbol_contexts
    )
}

fn build_volatile_payload(
    symbol: &SymbolSpan,
    related_blocks: &[RelatedCodeBlock],
    instruction: &str,
) -> String {
    let mut related = String::new();
    for block in related_blocks {
        related.push_str(&format!(
            "[READONLY RELATED BLOCK]\n\
reason: {}\n\
id: {}\nname: {}\nfile: {}\nlines: {}-{}\n{}\n\n",
            block.reason,
            block.symbol_id,
            block.name,
            block.file_path,
            block.start_line,
            block.end_line,
            block.source
        ));
    }
    if related.is_empty() {
        related.push_str("No readonly related blocks were selected.\n");
    }

    format!(
        "[TARGET SYMBOL]\n\
id: {}\nname: {}\nkind: {}\nfile: {}\nlines: {}-{}\nsignature: {}\ndocstring: {}\n\n\
[EDITABLE AST CODE BLOCK]\n{}\n\n[READONLY RELATED CONTEXT]\n{}\n[REWRITE COMMAND]\n{}\n",
        symbol.id,
        symbol.name,
        symbol.kind,
        symbol.file_path,
        symbol.start_line,
        symbol.end_line,
        symbol.signature,
        symbol.docstring,
        symbol.source,
        related,
        instruction
    )
}

fn load_related_rust_blocks(
    workspace: &Workspace,
    primary: &SymbolSpan,
    explicit_symbol_ids: &[String],
) -> anyhow::Result<Vec<RelatedCodeBlock>> {
    let root = workspace.root().display().to_string();
    let context = crate::rust_index::read_symbol(
        workspace.root(),
        ReadRustSymbolRequest {
            workspace_root: root,
            symbol_id: primary.id.clone(),
            include_context: true,
        },
    )?;

    let mut candidates = Vec::new();
    for id in explicit_symbol_ids {
        candidates.push((id.clone(), "explicit_related_symbol".to_string()));
    }
    for suggested in context.suggested_reads.iter().take(6) {
        candidates.push((
            suggested.symbol.id.clone(),
            format!("suggested_{}", suggested.reason),
        ));
    }
    for caller in context.callers.iter().take(4) {
        candidates.push((caller.symbol_id.clone(), "caller".to_string()));
    }
    for callee in &context.callees {
        for id in callee.matched_symbol_ids.iter().take(2) {
            candidates.push((id.clone(), format!("callee_{}", callee.target_text)));
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut blocks = Vec::new();
    for (symbol_id, reason) in candidates {
        if symbol_id == primary.id || !seen.insert(symbol_id.clone()) {
            continue;
        }
        let Ok(span) = load_rust_symbol_span(workspace, &symbol_id) else {
            continue;
        };
        blocks.push(RelatedCodeBlock {
            symbol_id: span.id,
            name: span.name,
            file_path: span.file_path,
            start_line: span.start_line,
            end_line: span.end_line,
            reason,
            source: span.source,
        });
        if blocks.len() >= 12 {
            break;
        }
    }

    Ok(blocks)
}

fn load_rust_symbol_span(workspace: &Workspace, symbol_id: &str) -> anyhow::Result<SymbolSpan> {
    let conn = crate::database::init_db(workspace.root())?;
    let root = workspace.root().display().to_string();
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, file_path, start_line, end_line, signature, docstring
         FROM rust_symbols WHERE workspace_root = ? AND id = ?",
    )?;
    let (id, name, kind, file_path, start_line, end_line, signature, docstring) = stmt
        .query_row(params![root, symbol_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, usize>(4)?,
                row.get::<_, usize>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|_| anyhow::anyhow!("rust symbol not found in SQLite index: {}", symbol_id))?;

    let path = workspace.root().join(&file_path);
    let content = std::fs::read_to_string(&path)?;
    let (byte_start, byte_end) = line_range_to_byte_span(&content, start_line, end_line)?;
    let source = content
        .get(byte_start..byte_end)
        .ok_or_else(|| anyhow::anyhow!("invalid UTF-8 byte span for indexed symbol"))?
        .to_string();

    Ok(SymbolSpan {
        id,
        name,
        kind,
        file_path,
        start_line,
        end_line,
        signature,
        docstring,
        byte_start,
        byte_end,
        source,
    })
}

fn load_architecture_memory(workspace: &Workspace, query: Option<&str>) -> anyhow::Result<String> {
    let conn = crate::database::init_db(workspace.root())?;
    let root = workspace.root().display().to_string();
    let mut out = String::new();
    if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        let pattern = format!("%{}%", query.trim());
        let mut stmt = conn.prepare(
            "SELECT area, summary, key_symbols, key_files, boundaries, risks
             FROM architecture_memories
             WHERE workspace_root = ? AND (
                area LIKE ? OR summary LIKE ? OR key_symbols LIKE ? OR key_files LIKE ? OR common_tasks LIKE ?
             )
             ORDER BY updated_at_unix DESC LIMIT 8",
        )?;
        let mut rows = stmt.query(params![root, pattern, pattern, pattern, pattern, pattern])?;
        while let Some(row) = rows.next()? {
            append_arch_row(&mut out, row)?;
        }
    }
    if out.trim().is_empty() {
        let mut stmt = conn.prepare(
            "SELECT area, summary, key_symbols, key_files, boundaries, risks
             FROM architecture_memories
             WHERE workspace_root = ?
             ORDER BY updated_at_unix DESC LIMIT 8",
        )?;
        let mut rows = stmt.query(params![root])?;
        while let Some(row) = rows.next()? {
            append_arch_row(&mut out, row)?;
        }
    }
    Ok(if out.trim().is_empty() {
        "No durable architecture memory recorded for this workspace.".to_string()
    } else {
        out
    })
}

fn append_arch_row(out: &mut String, row: &rusqlite::Row<'_>) -> rusqlite::Result<()> {
    let area: String = row.get(0)?;
    let summary: String = row.get(1)?;
    let key_symbols: String = row.get(2)?;
    let key_files: String = row.get(3)?;
    let boundaries: String = row.get(4)?;
    let risks: String = row.get(5)?;
    out.push_str(&format!(
        "## {}\nsummary: {}\nkey_symbols: {}\nkey_files: {}\nboundaries: {}\nrisks: {}\n\n",
        area, summary, key_symbols, key_files, boundaries, risks
    ));
    Ok(())
}

fn load_symbol_contexts(workspace: &Workspace, symbol: &SymbolSpan) -> anyhow::Result<String> {
    let conn = crate::database::init_db(workspace.root())?;
    let root = workspace.root().display().to_string();
    let mut stmt = conn.prepare(
        "SELECT symbol_name, belongs_to_area, business_role, read_when, avoid_when, risks, confidence
         FROM symbol_business_contexts
         WHERE workspace_root = ? AND (symbol_id = ? OR symbol_name = ? OR file_path = ?)
         ORDER BY confidence DESC, updated_at_unix DESC LIMIT 12",
    )?;
    let mut rows = stmt.query(params![root, symbol.id, symbol.name, symbol.file_path])?;
    let mut out = String::new();
    while let Some(row) = rows.next()? {
        let symbol_name: String = row.get(0)?;
        let area: String = row.get(1)?;
        let role: String = row.get(2)?;
        let read_when: String = row.get(3)?;
        let avoid_when: String = row.get(4)?;
        let risks: String = row.get(5)?;
        let confidence: f64 = row.get(6)?;
        out.push_str(&format!(
            "- {} [{} confidence {:.2}]: role={}; read_when={}; avoid_when={}; risks={}\n",
            symbol_name, area, confidence, role, read_when, avoid_when, risks
        ));
    }
    Ok(if out.trim().is_empty() {
        "No symbol business context recorded for this target.".to_string()
    } else {
        out
    })
}

fn parse_search_replace_patch(text: &str) -> anyhow::Result<SearchReplacePatch> {
    let search_marker = "<<<<<<< SEARCH";
    let sep_marker = "=======";
    let replace_marker = ">>>>>>> REPLACE";
    let search_start = text
        .find(search_marker)
        .ok_or_else(|| anyhow::anyhow!("expert output missing <<<<<<< SEARCH marker"))?
        + search_marker.len();
    let sep = text[search_start..]
        .find(sep_marker)
        .map(|idx| search_start + idx)
        .ok_or_else(|| anyhow::anyhow!("expert output missing ======= marker"))?;
    let replace_end = text[sep..]
        .find(replace_marker)
        .map(|idx| sep + idx)
        .ok_or_else(|| anyhow::anyhow!("expert output missing >>>>>>> REPLACE marker"))?;
    let replace_start = sep + sep_marker.len();

    Ok(SearchReplacePatch {
        search: trim_one_boundary_newline(&text[search_start..sep]),
        replace: trim_one_boundary_newline(&text[replace_start..replace_end]),
    })
}

fn trim_one_boundary_newline(value: &str) -> String {
    value
        .trim_start_matches(['\r', '\n'])
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

fn line_range_to_byte_span(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> anyhow::Result<(usize, usize)> {
    if start_line == 0 || end_line == 0 || start_line > end_line {
        anyhow::bail!("invalid indexed line range {}-{}", start_line, end_line);
    }
    let mut line = 1usize;
    let mut start = None;
    let mut end = None;
    for (idx, ch) in content.char_indices() {
        if line == start_line && start.is_none() {
            start = Some(idx);
        }
        if line == end_line + 1 {
            end = Some(idx);
            break;
        }
        if ch == '\n' {
            line += 1;
        }
    }
    if line == start_line && start.is_none() {
        start = Some(content.len());
    }
    let start = start.ok_or_else(|| anyhow::anyhow!("start_line is outside file"))?;
    let end = end.unwrap_or(content.len());
    Ok((start, end))
}

fn validate_rust_syntax(content: &str) -> anyhow::Result<()> {
    syn::parse_file(content)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("Rust syntax parse failed after patch: {}", error))
}

fn run_command_capture(cwd: &Path, program: &str, args: &[&str]) -> VerificationStatus {
    let output = Command::new(program).args(args).current_dir(cwd).output();
    match output {
        Ok(output) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            VerificationStatus {
                ran: true,
                success: output.status.success(),
                output: text,
            }
        }
        Err(error) => VerificationStatus {
            ran: true,
            success: false,
            output: error.to_string(),
        },
    }
}

async fn write_and_verify(
    path: &Path,
    root: &Path,
    file_content: &str,
    current_disk_content: &str,
) -> anyhow::Result<(VerificationStatus, VerificationStatus)> {
    let path_clone = path.to_path_buf();
    let file_content_clone = file_content.to_string();
    let root_clone = root.to_path_buf();

    std::fs::write(&path_clone, file_content_clone.as_bytes())?;

    let (fmt_status, check_status) = tokio::task::spawn_blocking(move || {
        let fmt_status = run_command_capture(&root_clone, "cargo", &["fmt"]);
        let check_status = run_command_capture(&root_clone, "cargo", &["check"]);
        (fmt_status, check_status)
    })
    .await?;

    if fmt_status.success && check_status.success {
        emit_event(SurgeryEvent::CargoCheckPassed);
        Ok((fmt_status, check_status))
    } else {
        std::fs::write(&path_clone, current_disk_content.as_bytes())?;
        let mut reason = format!(
            "cargo check/fmt failed\ncargo fmt success={}\ncargo check success={}",
            fmt_status.success, check_status.success
        );

        let mut final_error_message = format!(
            "local verification failed after byte-span merge\ncargo fmt:\n{}\n\ncargo check:\n{}",
            fmt_status.output,
            check_status.output
        );

        if !check_status.success {
            // Run again with JSON output to generate diagnostic report
            let root_clone2 = root.to_path_buf();
            let check_json = tokio::task::spawn_blocking(move || {
                run_command_capture(&root_clone2, "cargo", &["check", "--message-format=json"])
            })
            .await?;
            
            let diagnostic_report = crate::sandbox_diagnostic::generate_report(root, path, &check_json.output);
            
            reason.push_str("\nGenerated AST Diagnostic Report.");
            final_error_message = format!(
                "local verification failed after byte-span merge.\n{}\n\nRaw cargo check output:\n{}",
                diagnostic_report,
                check_status.output
            );
        }

        emit_event(SurgeryEvent::TransactionRolledBack { reason });
        anyhow::bail!("{}", final_error_message);
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_replace_block() {
        let patch = parse_search_replace_patch(
            "x\n<<<<<<< SEARCH\nold()\n=======\nnew()\n>>>>>>> REPLACE\n",
        )
        .unwrap();
        assert_eq!(patch.search, "old()");
        assert_eq!(patch.replace, "new()");
    }

    #[test]
    fn computes_byte_span_by_line_range() {
        let content = "a\nbb\nccc\n";
        let (start, end) = line_range_to_byte_span(content, 2, 2).unwrap();
        assert_eq!(&content[start..end], "bb\n");
    }

    #[test]
    fn rejects_patch_without_markers() {
        let error = parse_search_replace_patch("old\nnew").unwrap_err();
        assert!(error.to_string().contains("<<<<<<< SEARCH"));
    }

    #[test]
    fn rejects_invalid_line_range() {
        let error = line_range_to_byte_span("a\n", 3, 2).unwrap_err();
        assert!(error.to_string().contains("invalid indexed line range"));
    }

    #[test]
    fn volatile_payload_marks_related_blocks_readonly() {
        let symbol = SymbolSpan {
            id: "rust:src/lib.rs:main:1".to_string(),
            name: "main".to_string(),
            kind: "function".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 3,
            signature: "fn main()".to_string(),
            docstring: String::new(),
            byte_start: 0,
            byte_end: 12,
            source: "fn main() {}".to_string(),
        };
        let related = vec![RelatedCodeBlock {
            symbol_id: "rust:src/lib.rs:helper:5".to_string(),
            name: "helper".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 5,
            end_line: 7,
            reason: "callee_helper".to_string(),
            source: "fn helper() {}".to_string(),
        }];

        let payload = build_volatile_payload(&symbol, &related, "change main");

        assert!(payload.contains("[EDITABLE AST CODE BLOCK]"));
        assert!(payload.contains("[READONLY RELATED BLOCK]"));
        assert!(payload.contains("reason: callee_helper"));
        assert!(payload.contains("fn helper() {}"));
    }



    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("codex_expert_surgery_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn test_transaction_rollback_on_failed_checks() {
        let root = temp_workspace("rollback");
        let path = root.join("lib.rs");

        let original_content = "pub fn foo() {}";
        std::fs::write(&path, original_content).unwrap();

        let new_content = "pub fn foo() { invalid_syntax }";

        let result = write_and_verify(&path, &root, new_content, original_content).await;

        assert!(result.is_err());

        let rolled_back_content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(rolled_back_content, original_content);

        let _ = std::fs::remove_dir_all(root);
    }
}
