use std::path::Path;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct CompilerError {
    pub file_path: String,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct AstNodeSnippet {
    pub kind: String,
    pub signature: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct DiagnosticSandboxReport {
    pub file_path: String,
    pub diagnostics: Vec<CompilerDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub error: CompilerError,
    pub snippet: Option<AstNodeSnippet>,
}

/// Step 1: Parse pure string output from compiler into structured errors
pub fn parse_compiler_output(output: &str) -> Vec<CompilerError> {
    let mut errors = Vec::new();

    // Cargo JSON format parsing
    for line in output.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(msg) = value.get("message") {
                // message is usually an object in rustc json
                if let Some(level) = msg.get("level").and_then(|l| l.as_str()) {
                    if level == "error" {
                        let text = msg.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                        if let Some(spans) = msg.get("spans").and_then(|s| s.as_array()) {
                            // Find the primary span
                            if let Some(span) = spans.iter().find(|s| s.get("is_primary").and_then(|p| p.as_bool()).unwrap_or(false)) {
                                let file_name = span.get("file_name").and_then(|f| f.as_str()).unwrap_or("").to_string();
                                let line_start = span.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
                                let column_start = span.get("column_start").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
                                
                                errors.push(CompilerError {
                                    file_path: file_name,
                                    line: line_start,
                                    column: column_start,
                                    message: text,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    errors
}

/// Step 2: Route the error to the appropriate language diagnostic module
pub fn dispatch_diagnostic_by_language(error: &CompilerError, workspace_root: &Path) -> Option<AstNodeSnippet> {
    if error.file_path.is_empty() || error.line == 0 {
        return None;
    }
    
    let path = workspace_root.join(&error.file_path);
    let Ok(source_code) = std::fs::read_to_string(&path) else {
        return None;
    };

    if error.file_path.ends_with(".rs") {
        crate::rust_index::diagnostic::diagnose_ast_error(error.line, error.column, &source_code)
    } else if error.file_path.ends_with(".py") {
        crate::python_index::diagnostic::diagnose_ast_error(error.line, error.column, &source_code)
    } else if error.file_path.ends_with(".go") {
        crate::go_index::diagnostic::diagnose_ast_error(error.line, error.column, &source_code)
    } else if error.file_path.ends_with(".ts") || error.file_path.ends_with(".tsx") {
        crate::ts_index::diagnostic::diagnose_ast_error(error.line, error.column, &source_code)
    } else {
        None
    }
}

/// Step 4: Assemble the final markdown report
pub fn format_diagnostic_report(report: &DiagnosticSandboxReport) -> String {
    if report.diagnostics.is_empty() {
        return "Compiler failed, but no specific file/line errors could be extracted.".to_string();
    }

    let mut out = format!("### Diagnostic Sandbox Report for {}\n\n", report.file_path);
    out.push_str("> The following compilation errors occurred after the patch was applied. The errors have been cross-referenced with the AST to pinpoint the problematic nodes.\n\n");

    for (i, diag) in report.diagnostics.iter().enumerate() {
        out.push_str(&format!("#### Error {}: {}\n", i + 1, diag.error.message));
        out.push_str(&format!("**Location**: `{}:{}:{}`\n\n", diag.error.file_path, diag.error.line, diag.error.column));

        if let Some(ref snippet) = diag.snippet {
            out.push_str(&format!("**AST Node Context**: {} `{}`\n", snippet.kind, snippet.signature));
            out.push_str("```rust\n");
            out.push_str(&snippet.body);
            out.push_str("\n```\n\n");
        } else {
            out.push_str("*AST snippet not available for this node.*\n\n");
        }
    }

    out
}

pub fn generate_report(workspace_root: &Path, file_path: &Path, compiler_raw_output: &str) -> String {
    let all_errors = parse_compiler_output(compiler_raw_output);
    
    // Filter errors specific to the file we just edited, to avoid noise
    let target_file_str = file_path.strip_prefix(workspace_root).unwrap_or(file_path).display().to_string();
    // Use ends_with or exact match to filter
    let file_errors: Vec<_> = all_errors.into_iter().filter(|e| {
        let normalized_e = e.file_path.replace("\\", "/");
        let normalized_t = target_file_str.replace("\\", "/");
        normalized_e == normalized_t || normalized_e.ends_with(&normalized_t) || normalized_t.ends_with(&normalized_e)
    }).collect();

    let mut diagnostics = Vec::new();
    for err in file_errors {
        let snippet = dispatch_diagnostic_by_language(&err, workspace_root);
        diagnostics.push(CompilerDiagnostic {
            error: err,
            snippet,
        });
    }

    let report = DiagnosticSandboxReport {
        file_path: target_file_str,
        diagnostics,
    };

    format_diagnostic_report(&report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compiler_output_json() {
        let raw_json = r#"{"message":{"level":"error","message":"cannot find value `x` in this scope","spans":[{"file_name":"src/main.rs","is_primary":true,"line_start":10,"column_start":5}]}}"#;
        let errors = parse_compiler_output(raw_json);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].file_path, "src/main.rs");
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert_eq!(errors[0].message, "cannot find value `x` in this scope");
    }

    #[test]
    fn test_format_diagnostic_report() {
        let report = DiagnosticSandboxReport {
            file_path: "src/main.rs".to_string(),
            diagnostics: vec![
                CompilerDiagnostic {
                    error: CompilerError {
                        file_path: "src/main.rs".to_string(),
                        line: 10,
                        column: 5,
                        message: "cannot find value `x`".to_string(),
                    },
                    snippet: Some(AstNodeSnippet {
                        kind: "Function".to_string(),
                        signature: "fn main()".to_string(),
                        body: "fn main() {\n    x = 1;\n}".to_string(),
                    }),
                }
            ],
        };

        let output = format_diagnostic_report(&report);
        assert!(output.contains("### Diagnostic Sandbox Report for src/main.rs"));
        assert!(output.contains("#### Error 1: cannot find value `x`"));
        assert!(output.contains("**Location**: `src/main.rs:10:5`"));
        assert!(output.contains("**AST Node Context**: Function `fn main()`"));
        assert!(output.contains("```rust\nfn main() {\n    x = 1;\n}\n```"));
    }
}
