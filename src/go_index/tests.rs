use super::*;
use std::fs;
use std::path::PathBuf;

fn temp_workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("codex_go_index_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("service")).unwrap();
    path
}

#[test]
fn indexes_go_symbols_docstrings_and_calls() {
    let root = temp_workspace("basic");
    fs::write(
        root.join("service").join("ppt.go"),
        r#"package service

// PptService handles PPT workflow.
type PptService struct{}

// CreatePPT creates a PPT workflow.
func (s *PptService) CreatePPT(topic string) error {
    validateTopic(topic)
    return SaveWorkflow(topic)
}

// validateTopic checks topic text.
func validateTopic(topic string) {}

// SaveWorkflow stores workflow state.
func SaveWorkflow(topic string) error {
    return nil
}
"#,
    )
    .unwrap();

    let response = index_workspace(&root).unwrap();
    assert_eq!(response.files_indexed, 1);
    assert_eq!(response.symbols_indexed, 4);

    let search = search_symbols(
        &root,
        SearchGoSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "workflow".to_string(),
            limit: 10,
        },
    )
    .unwrap();
    assert!(
        search
            .matches
            .iter()
            .any(|symbol| symbol.name == "CreatePPT")
    );

    let create = search
        .matches
        .iter()
        .find(|symbol| symbol.name == "CreatePPT")
        .unwrap();
    assert_eq!(create.kind, GoSymbolKind::Method);
    assert!(create.docstring.contains("creates a PPT workflow"));

    let read = read_symbol(
        &root,
        ReadGoSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: create.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(read.content.contains("func (s *PptService) CreatePPT"));
    assert!(
        read.callees
            .iter()
            .any(|callee| callee.target_text == "SaveWorkflow")
    );
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "same_package_call" && suggestion.symbol.name == "SaveWorkflow"
    }));
    let save = search
        .matches
        .iter()
        .find(|symbol| symbol.name == "SaveWorkflow")
        .unwrap();
    let save_read = read_symbol(
        &root,
        ReadGoSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: save.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        save_read
            .callers
            .iter()
            .any(|caller| caller.name == "CreatePPT")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolves_import_alias_calls() {
    let root = temp_workspace("import_alias");
    fs::create_dir_all(root.join("handler")).unwrap();
    fs::write(
        root.join("service").join("workflow.go"),
        r#"package service

func SaveWorkflow(topic string) error {
    return nil
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("handler").join("handler.go"),
        r#"package handler

import svc "demo/service"

func Run() {
    _ = svc.SaveWorkflow("demo")
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();
    let run = search_symbols(
        &root,
        SearchGoSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "Run".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "Run")
    .unwrap();
    let read = read_symbol(
        &root,
        ReadGoSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: run.id,
            include_context: true,
        },
    )
    .unwrap();

    assert!(read.callees.iter().any(|callee| {
        callee.target_text == "SaveWorkflow" && !callee.matched_symbol_ids.is_empty()
    }));
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "imported_package_call" && suggestion.symbol.name == "SaveWorkflow"
    }));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolves_receiver_method_calls_and_multiline_signature() {
    let root = temp_workspace("receiver");
    fs::write(
        root.join("service").join("ppt.go"),
        r#"package service

type PptService struct{}

func (s *PptService) Create(
    topic string,
) error {
    return s.Save(topic)
}

func (s *PptService) Save(topic string) error {
    return nil
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();
    let create = search_symbols(
        &root,
        SearchGoSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "Create".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "Create")
    .unwrap();
    assert!(create.signature.contains("topic string"));

    let read = read_symbol(
        &root,
        ReadGoSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: create.id,
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        read.callees.iter().any(|callee| {
            callee.target_text == "Save" && !callee.matched_symbol_ids.is_empty()
        })
    );
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "receiver_method_call" && suggestion.symbol.name == "Save"
    }));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_builds_index_when_missing() {
    let root = temp_workspace("auto_build");
    fs::write(
        root.join("service").join("ppt.go"),
        "package service\n\n// AutoBuild proves search can index.\nfunc AutoBuild() {}\n",
    )
    .unwrap();

    let search = search_symbols(
        &root,
        SearchGoSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "AutoBuild".to_string(),
            limit: 5,
        },
    )
    .unwrap();

    assert_eq!(search.matches.len(), 1);
    // Bug2: 已迁移到 SQLite，不再生成 JSON 文件，改为验证元数据表中确实有索引记录
    {
        let conn = crate::database::init_db(&root).unwrap();
        let ts = crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "go");
        assert!(
            ts.is_some(),
            "index metadata should be recorded after auto-build"
        );
    }
    let _ = fs::remove_dir_all(root);
}
