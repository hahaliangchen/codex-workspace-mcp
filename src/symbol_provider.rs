use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::expert_surgery::{RelatedCodeBlock, SymbolSpan};
use crate::tools::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

impl SymbolLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolLanguage::Rust => "rust",
            SymbolLanguage::TypeScript => "typescript",
            SymbolLanguage::Python => "python",
            SymbolLanguage::Go => "go",
        }
    }
}

pub trait SymbolProvider: Send + Sync {
    fn language(&self) -> SymbolLanguage;

    fn load_symbol_span(
        &self,
        workspace: &Workspace,
        symbol_id: &str,
    ) -> anyhow::Result<SymbolSpan>;

    fn load_related_blocks(
        &self,
        workspace: &Workspace,
        primary: &SymbolSpan,
        explicit_symbol_ids: &[String],
    ) -> anyhow::Result<Vec<RelatedCodeBlock>>;
}

pub fn infer_language_from_symbol_id(symbol_id: &str) -> Option<SymbolLanguage> {
    if symbol_id.starts_with("rust:") {
        Some(SymbolLanguage::Rust)
    } else if symbol_id.starts_with("ts:") {
        Some(SymbolLanguage::TypeScript)
    } else if symbol_id.starts_with("python:") {
        Some(SymbolLanguage::Python)
    } else if symbol_id.starts_with("go:") {
        Some(SymbolLanguage::Go)
    } else {
        None
    }
}

pub fn provider_for_language(language: SymbolLanguage) -> Box<dyn SymbolProvider> {
    match language {
        SymbolLanguage::Rust => Box::new(RustSymbolProvider),
        SymbolLanguage::TypeScript => Box::new(TypeScriptSymbolProvider),
        SymbolLanguage::Python => Box::new(PythonSymbolProvider),
        SymbolLanguage::Go => Box::new(GoSymbolProvider),
    }
}

pub fn line_range_to_byte_span(
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

fn source_span(
    root: &Path,
    file_path: &str,
    start_line: usize,
    end_line: usize,
) -> anyhow::Result<(usize, usize, String)> {
    let content = std::fs::read_to_string(root.join(file_path))?;
    let (byte_start, byte_end) = line_range_to_byte_span(&content, start_line, end_line)?;
    let source = content
        .get(byte_start..byte_end)
        .ok_or_else(|| anyhow::anyhow!("invalid UTF-8 byte span for indexed symbol"))?
        .to_string();
    Ok((byte_start, byte_end, source))
}

fn related_from_span(span: SymbolSpan, reason: String) -> RelatedCodeBlock {
    RelatedCodeBlock {
        symbol_id: span.id,
        name: span.name,
        file_path: span.file_path,
        start_line: span.start_line,
        end_line: span.end_line,
        reason,
        source: span.source,
    }
}

struct RustSymbolProvider;

impl SymbolProvider for RustSymbolProvider {
    fn language(&self) -> SymbolLanguage {
        SymbolLanguage::Rust
    }

    fn load_symbol_span(
        &self,
        workspace: &Workspace,
        symbol_id: &str,
    ) -> anyhow::Result<SymbolSpan> {
        let root = workspace.root().display().to_string();
        let response = crate::rust_index::read_symbol(
            workspace.root(),
            crate::rust_index::ReadRustSymbolRequest {
                workspace_root: root,
                symbol_id: symbol_id.to_string(),
                include_context: false,
            },
        )?;
        let symbol = response.symbol;
        let (byte_start, byte_end, source) = source_span(
            workspace.root(),
            &symbol.file_path,
            symbol.start_line,
            symbol.end_line,
        )?;
        Ok(SymbolSpan {
            language: self.language(),
            id: symbol.id,
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            file_path: symbol.file_path,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature,
            docstring: symbol.docstring,
            byte_start,
            byte_end,
            source,
        })
    }

    fn load_related_blocks(
        &self,
        workspace: &Workspace,
        primary: &SymbolSpan,
        explicit_symbol_ids: &[String],
    ) -> anyhow::Result<Vec<RelatedCodeBlock>> {
        let root = workspace.root().display().to_string();
        let context = crate::rust_index::read_symbol(
            workspace.root(),
            crate::rust_index::ReadRustSymbolRequest {
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
        load_related_candidates(self, workspace, primary, candidates)
    }
}

struct TypeScriptSymbolProvider;

impl SymbolProvider for TypeScriptSymbolProvider {
    fn language(&self) -> SymbolLanguage {
        SymbolLanguage::TypeScript
    }

    fn load_symbol_span(
        &self,
        workspace: &Workspace,
        symbol_id: &str,
    ) -> anyhow::Result<SymbolSpan> {
        let root = workspace.root().display().to_string();
        let response = crate::ts_index::read_symbol(
            workspace.root(),
            crate::ts_index::ReadTsSymbolRequest {
                workspace_root: root,
                symbol_id: symbol_id.to_string(),
                include_context: false,
            },
        )?;
        let symbol = response.symbol;
        let (byte_start, byte_end, source) = source_span(
            workspace.root(),
            &symbol.file_path,
            symbol.start_line,
            symbol.end_line,
        )?;
        Ok(SymbolSpan {
            language: self.language(),
            id: symbol.id,
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            file_path: symbol.file_path,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature,
            docstring: symbol.docstring,
            byte_start,
            byte_end,
            source,
        })
    }

    fn load_related_blocks(
        &self,
        workspace: &Workspace,
        primary: &SymbolSpan,
        explicit_symbol_ids: &[String],
    ) -> anyhow::Result<Vec<RelatedCodeBlock>> {
        let root = workspace.root().display().to_string();
        let context = crate::ts_index::read_symbol(
            workspace.root(),
            crate::ts_index::ReadTsSymbolRequest {
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
        for import in &context.resolved_imports {
            for id in import.matched_symbol_ids.iter().take(2) {
                candidates.push((id.clone(), format!("import_{}", import.local_name)));
            }
        }
        load_related_candidates(self, workspace, primary, candidates)
    }
}

struct PythonSymbolProvider;

impl SymbolProvider for PythonSymbolProvider {
    fn language(&self) -> SymbolLanguage {
        SymbolLanguage::Python
    }

    fn load_symbol_span(
        &self,
        workspace: &Workspace,
        symbol_id: &str,
    ) -> anyhow::Result<SymbolSpan> {
        let root = workspace.root().display().to_string();
        let response = crate::python_index::read_symbol(
            workspace.root(),
            crate::python_index::ReadPythonSymbolRequest {
                workspace_root: root,
                symbol_id: symbol_id.to_string(),
                include_context: false,
            },
        )?;
        let symbol = response.symbol;
        let (byte_start, byte_end, source) = source_span(
            workspace.root(),
            &symbol.file_path,
            symbol.start_line,
            symbol.end_line,
        )?;
        Ok(SymbolSpan {
            language: self.language(),
            id: symbol.id,
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            file_path: symbol.file_path,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature,
            docstring: symbol.docstring,
            byte_start,
            byte_end,
            source,
        })
    }

    fn load_related_blocks(
        &self,
        workspace: &Workspace,
        primary: &SymbolSpan,
        explicit_symbol_ids: &[String],
    ) -> anyhow::Result<Vec<RelatedCodeBlock>> {
        let root = workspace.root().display().to_string();
        let context = crate::python_index::read_symbol(
            workspace.root(),
            crate::python_index::ReadPythonSymbolRequest {
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
        load_related_candidates(self, workspace, primary, candidates)
    }
}

struct GoSymbolProvider;

impl SymbolProvider for GoSymbolProvider {
    fn language(&self) -> SymbolLanguage {
        SymbolLanguage::Go
    }

    fn load_symbol_span(
        &self,
        workspace: &Workspace,
        symbol_id: &str,
    ) -> anyhow::Result<SymbolSpan> {
        let root = workspace.root().display().to_string();
        let response = crate::go_index::read_symbol(
            workspace.root(),
            crate::go_index::ReadGoSymbolRequest {
                workspace_root: root,
                symbol_id: symbol_id.to_string(),
                include_context: false,
            },
        )?;
        let symbol = response.symbol;
        let (byte_start, byte_end, source) = source_span(
            workspace.root(),
            &symbol.file_path,
            symbol.start_line,
            symbol.end_line,
        )?;
        Ok(SymbolSpan {
            language: self.language(),
            id: symbol.id,
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            file_path: symbol.file_path,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature,
            docstring: symbol.docstring,
            byte_start,
            byte_end,
            source,
        })
    }

    fn load_related_blocks(
        &self,
        workspace: &Workspace,
        primary: &SymbolSpan,
        explicit_symbol_ids: &[String],
    ) -> anyhow::Result<Vec<RelatedCodeBlock>> {
        let root = workspace.root().display().to_string();
        let context = crate::go_index::read_symbol(
            workspace.root(),
            crate::go_index::ReadGoSymbolRequest {
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
        load_related_candidates(self, workspace, primary, candidates)
    }
}

fn load_related_candidates<P: SymbolProvider + ?Sized>(
    provider: &P,
    workspace: &Workspace,
    primary: &SymbolSpan,
    candidates: Vec<(String, String)>,
) -> anyhow::Result<Vec<RelatedCodeBlock>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut blocks = Vec::new();
    for (symbol_id, reason) in candidates {
        if symbol_id == primary.id || !seen.insert(symbol_id.clone()) {
            continue;
        }
        let Ok(span) = provider.load_symbol_span(workspace, &symbol_id) else {
            continue;
        };
        blocks.push(related_from_span(span, reason));
        if blocks.len() >= 12 {
            break;
        }
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_language_from_symbol_prefix() {
        assert_eq!(
            infer_language_from_symbol_id("rust:src/lib.rs:f:1"),
            Some(SymbolLanguage::Rust)
        );
        assert_eq!(
            infer_language_from_symbol_id("ts:src/app.ts:f:1"),
            Some(SymbolLanguage::TypeScript)
        );
        assert_eq!(
            infer_language_from_symbol_id("python:src/app.py:f:1"),
            Some(SymbolLanguage::Python)
        );
        assert_eq!(
            infer_language_from_symbol_id("go:main.go:f:1"),
            Some(SymbolLanguage::Go)
        );
        assert_eq!(infer_language_from_symbol_id("unknown"), None);
    }

    #[test]
    fn computes_byte_span_by_line_range() {
        let content = "a\nbb\nccc\n";
        let (start, end) = line_range_to_byte_span(content, 2, 2).unwrap();
        assert_eq!(&content[start..end], "bb\n");
    }

    #[test]
    fn rejects_invalid_line_range() {
        let error = line_range_to_byte_span("a\n", 3, 2).unwrap_err();
        assert!(error.to_string().contains("invalid indexed line range"));
    }
}
