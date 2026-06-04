import re

with open('src/go_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# 1. Add file_imports to GoSymbol
content = content.replace('pub struct GoSymbol {\n    pub id: String,', 'pub struct GoSymbol {\n    pub id: String,\n    #[serde(default)]\n    pub file_imports: Vec<GoImport>,')

# 2. Modify load_all_symbols to fetch file_imports_json
content = content.replace('receiver_type, calls_json FROM go_symbols', 'receiver_type, calls_json, file_imports_json FROM go_symbols')
content = content.replace('            calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),', '            calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),\n            file_imports: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),')

# 3. Modify build_index to insert file_imports_json
content = content.replace('receiver_type, calls_json\n                )', 'receiver_type, calls_json, file_imports_json\n                )')
content = content.replace('receiver_type, calls_json\n                ]', 'receiver_type, calls_json, file_imports_json\n                ]')
content = content.replace('let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();', 'let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();\n            let file_imports_json = serde_json::to_string(&parsed.file.imports).unwrap_or_default();')
content = content.replace('Ok(parsed) => parsed', 'Ok(parsed) => parsed')

# Fix compile errors from go_index
content = content.replace('Ok(parsed) => parsed,\n            Err(_) => continue,', 'parsed')
content = content.replace('let parsed = match parse_go_file(root, path, &content) {\n            parsed\n        };', 'let parsed = parse_go_file(root, path, &content);')
content = content.replace('let parsed = match parse_go_file(root, path, &content) {\n            Ok(parsed) => parsed,\n            Err(_) => continue,\n        };', 'let parsed = parse_go_file(root, path, &content);')

# 4. Fix build_context
build_context_code = """
fn build_context(
    index_symbols: &[GoSymbol],
    symbol: &GoSymbol,
) -> (Vec<GoCaller>, Vec<GoCallee>, Vec<GoSuggestedRead>) {
    let mut id_to_symbol = std::collections::BTreeMap::new();
    for item in index_symbols {
        id_to_symbol.insert(item.id.clone(), item);
    }
    
    let mut file_infos = std::collections::BTreeMap::new();
    for sym in index_symbols {
        file_infos.entry(sym.file_path.clone()).or_insert_with(|| GoFileInfo {
            file_path: sym.file_path.clone(),
            package: sym.package.clone(),
            imports: sym.file_imports.clone(),
        });
    }
    
    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| GoCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, &file_infos, symbol, call)
                .into_iter()
                .map(|s| s.id.clone())
                .collect(),
        })
        .collect();

    let mut suggested_reads = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(GoSuggestedRead {
                    reason: suggestion_reason(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: GoSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }

    let mut callers = Vec::new();
    for item in index_symbols {
        if item.id == symbol.id {
            continue;
        }
        for call in &item.calls {
            let matched = resolve_call(index_symbols, &file_infos, item, call)
                .into_iter()
                .any(|m| m.id == symbol.id);
            if matched {
                callers.push(GoCaller {
                    symbol_id: item.id.clone(),
                    name: item.name.clone(),
                    file_path: item.file_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });
            }
        }
    }

    (callers, callees, suggested_reads)
}
"""

content = re.sub(r'fn build_context\(\n    index_symbols: &\[GoSymbol\],\n    symbol: &GoSymbol,\n\) -> \(Vec<GoCaller>, Vec<GoCallee>, Vec<GoSuggestedRead>\) \{.*?\n\}\n\nfn resolve_call', build_context_code.strip() + '\n\nfn resolve_call', content, flags=re.DOTALL)

content = content.replace('resolve_call(index, &file_infos, symbol, call)', 'resolve_call(index_symbols, &file_infos, symbol, call)')
content = content.replace('resolve_call(index, &file_infos, item, call)', 'resolve_call(index_symbols, &file_infos, item, call)')
content = content.replace('index.symbols.iter().filter', 'index_symbols.iter().filter')

content = content.replace('crate::rust_index::now_unix()', 'now_unix()')
content = content.replace('fn now_unix() -> u64 {', 'pub fn now_unix() -> u64 {')

with open('src/go_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
