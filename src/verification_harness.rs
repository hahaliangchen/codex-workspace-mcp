use std::path::Path;
use std::process::Command;

use crate::expert_surgery::{
    emit_event, ExpertCodeSurgeryRequest, ExpertCodeSurgeryResponse, ExpertSurgeryDraft,
    SurgeryEvent, VerificationStatus,
};
use crate::tools::Workspace;

pub async fn apply_and_verify(
    workspace: &Workspace,
    request: &ExpertCodeSurgeryRequest,
    draft: ExpertSurgeryDraft,
) -> anyhow::Result<ExpertCodeSurgeryResponse> {
    let path = workspace.root().join(&draft.symbol.file_path);
    let patch = &draft.patch;
    let symbol = &draft.symbol;
    let original_file_content_before_await = &draft.original_file_content_before_await;

    if crate::conflict_resolver::normalize_newlines(&patch.search).trim()
        != crate::conflict_resolver::normalize_newlines(&symbol.source).trim()
    {
        // For search mismatch, we still bail because the model completely hallucinated the context,
        // it's a fundamental failure of patch application, not a compilation error.
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

    if current_disk_content == *original_file_content_before_await {
        file_content.replace_range(symbol.byte_start..symbol.byte_end, &replacement);
        emit_event(SurgeryEvent::FileConsistentApproved);
    } else {
        emit_event(SurgeryEvent::OffsetDriftDetected);
        if let Some((start_byte, end_byte)) =
            crate::conflict_resolver::find_unique_normalized_match(&current_disk_content, &patch.search)
        {
            file_content.replace_range(start_byte..end_byte, &replacement);
            emit_event(SurgeryEvent::ThreeWayRelocationSuccess {
                byte_range: (start_byte, end_byte),
            });
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
            let merged = crate::conflict_resolver::call_flash_merge(
                &flash_provider,
                &patch.search,
                &replacement,
                ours_block,
            )
            .await?;

            file_content.replace_range(ours_start..ours_end, &merged);
            emit_event(SurgeryEvent::FlashResolverSuccess);
        }
    }

    // Step 2: AST syntax validation in-memory
    let mut syntax_ok = true;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut syntax_error_msg = String::new();

    emit_event(SurgeryEvent::LocalLintStarted);
    if let Err(err) = validate_syntax_in_memory(&file_content, ext) {
        syntax_ok = false;
        syntax_error_msg = err;
        emit_event(SurgeryEvent::TransactionRolledBack {
            reason: format!("AST syntax validation failed: {}", syntax_error_msg),
        });
        // We do NOT bail! Soft-landing: we will just return success=false.
    } else {
        emit_event(SurgeryEvent::SyntaxTreeVerified);
    }

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

    if !request.dry_run && syntax_ok {
        // Step 3: Physical write and Semantic Gate
        let verify_res = run_semantic_gate(
            &path,
            workspace.root(),
            &file_content,
            &current_disk_content,
            ext,
        )
        .await?;
        fmt_status = verify_res.0;
        check_status = verify_res.1;
    } else if !syntax_ok {
        // If syntax failed, we never write to disk, we just mark check_status as failed
        check_status.ran = true;
        check_status.success = false;
        check_status.output = format!("AST Syntax Error before compilation:\n{}", syntax_error_msg);
    }

    Ok(ExpertCodeSurgeryResponse {
        symbol_id: draft.symbol.id.clone(),
        file_path: draft.symbol.file_path.clone(),
        start_byte: draft.symbol.byte_start,
        end_byte: draft.symbol.byte_end,
        dry_run: request.dry_run,
        fixed_prefix_chars: draft.fixed_prefix_chars,
        volatile_chars: draft.volatile_chars,
        related_blocks: draft.related_blocks,
        replacement_bytes: replacement.len(),
        syntax_ok,
        fmt_status,
        check_status,
        patch: draft.patch,
    })
}

fn validate_syntax_in_memory(content: &str, ext: &str) -> Result<(), String> {
    match ext {
        "rs" => {
            syn::parse_file(content)
                .map(|_| ())
                .map_err(|error| format!("Rust syntax parse failed after patch: {}", error))
        }
        // Add fast in-memory checks for ts/py if available, for now assume ok
        _ => Ok(()),
    }
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

async fn run_semantic_gate(
    path: &Path,
    root: &Path,
    file_content: &str,
    current_disk_content: &str,
    ext: &str,
) -> anyhow::Result<(VerificationStatus, VerificationStatus)> {
    let path_clone = path.to_path_buf();
    let file_content_clone = file_content.to_string();
    let root_clone = root.to_path_buf();
    let ext_clone = ext.to_string();

    std::fs::write(&path_clone, file_content_clone.as_bytes())?;

    let (fmt_status, check_status) = tokio::task::spawn_blocking(move || {
        match ext_clone.as_str() {
            "rs" => {
                let fmt = run_command_capture(&root_clone, "cargo", &["fmt"]);
                let chk = run_command_capture(&root_clone, "cargo", &["check"]);
                (fmt, chk)
            }
            "ts" | "tsx" => {
                let fmt = VerificationStatus { ran: false, success: true, output: "".to_string() };
                let chk = run_command_capture(&root_clone, "npx", &["tsc", "--noEmit"]);
                (fmt, chk)
            }
            _ => {
                // Fallback to true if no semantic checker defined
                (
                    VerificationStatus { ran: false, success: true, output: "".to_string() },
                    VerificationStatus { ran: false, success: true, output: "".to_string() },
                )
            }
        }
    })
    .await?;

    if fmt_status.success && check_status.success {
        emit_event(SurgeryEvent::CargoCheckPassed);
        Ok((fmt_status, check_status))
    } else {
        // Atomic rollback
        std::fs::write(&path_clone, current_disk_content.as_bytes())?;
        
        let mut reason = format!(
            "Semantic check/fmt failed\nfmt success={}\ncheck success={}",
            fmt_status.success, check_status.success
        );

        let mut final_error_message = format!(
            "local verification failed after byte-span merge\nfmt:\n{}\n\ncheck:\n{}",
            fmt_status.output,
            check_status.output
        );

        // If Rust cargo check failed, try to get JSON diagnostics
        if ext == "rs" && !check_status.success {
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

        // Soft landing: we DO NOT bail!. We return the failed status naturally.
        let failed_check = VerificationStatus {
            ran: true,
            success: false,
            output: final_error_message,
        };
        Ok((fmt_status, failed_check))
    }
}
