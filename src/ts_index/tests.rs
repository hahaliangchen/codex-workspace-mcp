use std::{
    fs,
    path::{Path, PathBuf},
};
use crate::ts_index::*;

use super::*;

fn temp_workspace(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("codex_ts_index_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn indexes_ts_symbols_and_suggested_reads() {
    let root = temp_workspace("basic");
    fs::write(
        root.join("api.ts"),
        r#"import { request } from './request';

// Create session for PPT
export async function createPptSession(input: string) {
  return request.post('/ppt/session', input);
}

// format id
export const formatId = (id: string) => {
  return normalize(id);
}

export class PptEditor {
  save() {
return createPptSession('demo');
  }
}

function normalize(id: string) {
  return id.trim();
}
"#,
    )
    .unwrap();

    let response = index_workspace(&root).unwrap();
    assert_eq!(response.files_indexed, 1);
    assert!(response.symbols_indexed >= 4);

    let search = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "createPptSession".to_string(),
            limit: 5,
        },
    )
    .unwrap();
    let symbol = search
        .matches
        .iter()
        .find(|item| item.name == "createPptSession")
        .unwrap();
    assert!(symbol.docstring.contains("Create session"));

    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: symbol.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        read.callees
            .iter()
            .any(|callee| callee.target_text == "post")
    );

    let save_search = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "save".to_string(),
            limit: 5,
        },
    )
    .unwrap();
    let save = save_search
        .matches
        .iter()
        .find(|item| item.name == "save")
        .unwrap();
    let save_read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: save.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(save_read.suggested_reads.iter().any(|suggestion| {
        suggestion.symbol.name == "createPptSession" && suggestion.reason == "direct_callee"
    }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_builds_index_when_missing() {
    let root = temp_workspace("auto_build");
    fs::write(
        root.join("api.ts"),
        "export function AutoBuild() { return 1; }\n",
    )
    .unwrap();

    let search = search_symbols(
        &root,
        SearchTsSymbolsRequest {
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
        let ts_meta = crate::database::get_index_generated_at(
            &conn,
            &root.to_string_lossy(),
            "ts",
        );
        assert!(ts_meta.is_some(), "index metadata should be recorded after auto-build");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolves_imported_symbols_in_context() {
    let root = temp_workspace("imports");
    fs::write(
        root.join("provider.ts"),
        r#"export function createThing(name: string) {
  return name.trim();
}

export default function defaultThing() {
  return createThing('default');
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("consumer.ts"),
        r#"import defaultThing, { createThing as makeThing } from './provider';

export function run() {
  makeThing('demo');
  return defaultThing();
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();
    let run = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "run".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "run")
    .unwrap();
    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: run.id,
            include_context: true,
        },
    )
    .unwrap();

    assert!(read.resolved_imports.iter().any(|import| {
        import.local_name == "makeThing"
            && import.target_file_path.as_deref() == Some("provider.ts")
            && !import.matched_symbol_ids.is_empty()
    }));
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
    }));
    assert!(read.callees.iter().any(|callee| {
        callee.target_text == "makeThing" && !callee.matched_symbol_ids.is_empty()
    }));

    let create = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "createThing".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "createThing")
    .unwrap();
    let create_read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: create.id,
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        create_read
            .callers
            .iter()
            .any(|caller| caller.name == "run")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolves_namespace_import_calls() {
    let root = temp_workspace("namespace_imports");
    fs::write(
        root.join("provider.ts"),
        "export function createThing(name: string) { return name.trim(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("consumer.ts"),
        r#"import * as provider from './provider';

export function run() {
  return provider.createThing('demo');
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();
    let run = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "run".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "run")
    .unwrap();
    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: run.id,
            include_context: true,
        },
    )
    .unwrap();

    assert!(read.callees.iter().any(|callee| {
        callee.target_text == "createThing" && !callee.matched_symbol_ids.is_empty()
    }));
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
    }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolves_export_star_barrel_imports() {
    let root = temp_workspace("barrel_imports");
    fs::write(
        root.join("provider.ts"),
        "export function createThing(name: string) { return name.trim(); }\n",
    )
    .unwrap();
    fs::write(root.join("barrel.ts"), "export * from './provider';\n").unwrap();
    fs::write(
        root.join("consumer.ts"),
        r#"import { createThing } from './barrel';

export function run() {
  return createThing('demo');
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();
    let run = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "run".to_string(),
            limit: 5,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "run")
    .unwrap();
    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: run.id,
            include_context: true,
        },
    )
    .unwrap();
    assert!(read.resolved_imports.iter().any(|import| {
        import.local_name == "createThing"
            && import.target_file_path.as_deref() == Some("barrel.ts")
            && !import.matched_symbol_ids.is_empty()
            && import.re_export_chain.iter().any(|step| {
                step.file_path == "barrel.ts"
                    && step.target_file_path.as_deref() == Some("provider.ts")
            })
    }));
    assert!(read.suggested_reads.iter().any(|suggestion| {
        suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
    }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_hoc_inner_class_with_visit() {
    let root = temp_workspace("hoc_inner_class");
    fs::write(
        root.join("container.tsx"),
        r#"import React from 'react';

export default function container(Comp: any) {
  class Wrapper extends React.Component {
updateDisk() {
  this.foo();
}
getMax() {
  return 1;
}
foo() {}
  }
  return Wrapper;
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();

    let update_disk = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "updateDisk".to_string(),
            limit: 10,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "updateDisk")
    .expect("updateDisk should be indexed");
    assert_eq!(update_disk.kind, TsSymbolKind::Method);
    assert_eq!(update_disk.scope_path, "container.Wrapper");

    let wrapper = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "Wrapper".to_string(),
            limit: 10,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "Wrapper")
    .expect("Wrapper should be indexed");
    assert_eq!(wrapper.kind, TsSymbolKind::Class);
    assert_eq!(wrapper.scope_path, "container");
    assert_eq!(update_disk.parent_id.as_deref(), Some(wrapper.id.as_str()));

    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: update_disk.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        read.callees.iter().any(|c| c.target_text == "foo"),
        "updateDisk should record `this.foo()` call"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_anonymous_class_expression() {
    let root = temp_workspace("anon_class_expr");
    fs::write(
        root.join("wrap.ts"),
        r#"class Base {}
const Wrapped = class extends Base {
  foo() { return 1; }
};
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();

    let foo = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "foo".to_string(),
            limit: 10,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "foo")
    .expect("foo should be indexed");
    assert_eq!(foo.kind, TsSymbolKind::Method);
    assert_eq!(foo.scope_path, "Wrapped");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_nested_function() {
    let root = temp_workspace("nested_fn");
    fs::write(
        root.join("util.ts"),
        r#"export function outer() {
  function inner() {
return 42;
  }
  return inner();
}
"#,
    )
    .unwrap();

    index_workspace(&root).unwrap();

    let inner = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "inner".to_string(),
            limit: 10,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "inner")
    .expect("inner should be indexed");
    assert_eq!(inner.kind, TsSymbolKind::Function);
    assert_eq!(inner.scope_path, "outer");

    let outer = search_symbols(
        &root,
        SearchTsSymbolsRequest {
            workspace_root: root.display().to_string(),
            query: "outer".to_string(),
            limit: 10,
        },
    )
    .unwrap()
    .matches
    .into_iter()
    .find(|symbol| symbol.name == "outer")
    .expect("outer should be indexed");
    let read = read_symbol(
        &root,
        ReadTsSymbolRequest {
            workspace_root: root.display().to_string(),
            symbol_id: outer.id.clone(),
            include_context: true,
        },
    )
    .unwrap();
    assert!(
        read.callees.iter().any(|c| c.target_text == "inner"),
        "outer should record inner() call as callee"
    );

    let _ = fs::remove_dir_all(root);
}
