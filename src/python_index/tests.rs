use super::*;
use std::fs;
use std::path::PathBuf;

fn temp_workspace(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("codex_python_index_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn indexes_functions_classes_and_methods() {
    let root = temp_workspace("basic");
    fs::write(
        root.join("service.py"),
        r#"import os
from pathlib import Path

class PptService:
    """Generates PPT files."""

    def __init__(self, topic: str):
        """Init with topic."""
        self.topic = topic

    def create(self) -> str:
        """Create the PPT."""
        return self._render()

    def _render(self) -> str:
        return self.topic

def validate(topic: str) -> bool:
    """Validate a topic string."""
    return bool(topic)
"#,
    )
    .unwrap();

    let resp = index_workspace(&root).unwrap();
    assert!(resp.files_indexed >= 1);
    assert!(resp.symbols_indexed >= 4);

    let search = search_symbols(
        &root,
        SearchPythonSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "create".to_string(),
            limit: 10,
        },
    )
    .unwrap();
    let create = search.matches.iter().find(|s| s.name == "create").unwrap();
    assert_eq!(create.kind, PythonSymbolKind::Method);
    assert_eq!(create.class_name.as_deref(), Some("PptService"));
    assert!(create.signature.contains("def create"));
    assert!(create.docstring.contains("Create"));

    let read = read_symbol(
        &root,
        ReadPythonSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: create.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(read.content.contains("def create"));
    assert!(read.callees.iter().any(|c| c.target_text == "_render"));
    assert!(
        read.suggested_reads
            .iter()
            .any(|s| s.symbol.name == "_render")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn extracts_imports() {
    let root = temp_workspace("imports");
    fs::write(
        root.join("app.py"),
        "import os\nfrom pathlib import Path, PurePath\ndef dummy(): pass\n",
    )
    .unwrap();
    let resp = index_workspace(&root).unwrap();
    assert!(!resp.index_path.is_empty());

    // 已迁移到 SQLite，导入信息存在于每个 symbol 的 file_imports 字段中
    let symbols = load_all_symbols(&root).unwrap();
    let dummy = symbols
        .iter()
        .find(|s| s.name == "dummy")
        .expect("dummy symbol should exist");
    assert!(dummy.file_imports.iter().any(|i| i.module == "os"));
    assert!(
        dummy
            .file_imports
            .iter()
            .any(|i| i.name.as_deref() == Some("Path"))
    );

    let _ = fs::remove_dir_all(root);
}
