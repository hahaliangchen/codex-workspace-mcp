use super::*;
use std::fs;
use std::path::PathBuf;

fn temp_workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("codex_rust_index_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("src")).unwrap();
    path
}

#[test]
fn indexes_rust_symbols_docstrings_and_calls() {
    let root = temp_workspace("basic");
    fs::write(
        root.join("src").join("lib.rs"),
        r#"pub mod service {
    /// Handles PPT workflow.
    pub struct PptService;

    impl PptService {
        /// Creates a PPT workflow.
        pub fn create(&self, topic: String) -> Result<(), String> {
            validate_topic(&topic);
            self.save(topic)
        }

        pub fn save(&self, topic: String) -> Result<(), String> {
            Ok(())
        }
    }

    fn validate_topic(topic: &str) {}
}
"#,
    )
    .unwrap();

    let response = index_workspace(&root).unwrap();
    assert_eq!(response.files_indexed, 1);
    assert!(response.symbols_indexed >= 4);

    let search = search_symbols(
        &root,
        SearchRustSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "create".to_string(),
            limit: 10,
        },
    )
    .unwrap();
    let create = search
        .matches
        .iter()
        .find(|symbol| symbol.name == "create")
        .unwrap();
    assert_eq!(create.kind, RustSymbolKind::Method);
    assert!(create.signature.contains("create"));

    let read = read_symbol(
        &root,
        ReadRustSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: create.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(read.content.contains("pub fn create"));
    assert!(
        read.callees
            .iter()
            .any(|callee| callee.target_text == "save")
    );
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "receiver_method_call" && suggestion.symbol.name == "save"
    }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_builds_index_when_missing() {
    let root = temp_workspace("auto_build");
    fs::write(
        root.join("src").join("lib.rs"),
        "/// AutoBuild proves search can index.\npub fn auto_build() {}\n",
    )
    .unwrap();

    let search = search_symbols(
        &root,
        SearchRustSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "auto_build".to_string(),
            limit: 5,
        },
    )
    .unwrap();

    assert_eq!(search.matches.len(), 1);
    // Bug2: 已迁移 to SQLite,不再生成 JSON 文件,改为验证元数据表中确实有索引记录
    {
        let conn = crate::database::init_db(&root).unwrap();
        let ts = crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "rust");
        assert!(
            ts.is_some(),
            "index metadata should be recorded after auto-build"
        );
    }
    let _ = fs::remove_dir_all(root);
}
