import re

with open('src/ts_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Fix build_context
content = content.replace('fn build_context(\n    index: &TsIndex,\n    symbol: &TsSymbol,\n) ->', 'fn build_context(\n    index_symbols: &[TsSymbol],\n    symbol: &TsSymbol,\n) ->')
content = content.replace('let id_to_symbol: std::collections::BTreeMap<String, &TsSymbol> =\n        index.symbols.iter()', 'let id_to_symbol: std::collections::BTreeMap<String, &TsSymbol> =\n        index_symbols.iter()')
content = content.replace('resolve_call(index, symbol, call)', 'resolve_call(index_symbols, symbol, call)')
content = content.replace('resolve_imports(index, symbol)', 'resolve_imports(index_symbols, symbol)')
content = content.replace('resolve_call(index, item, call)', 'resolve_call(index_symbols, item, call)')
content = content.replace('for item in &index.symbols {', 'for item in index_symbols {')

# Fix build_file_export_map
content = content.replace('fn build_file_export_map(index: &TsIndex)', 'fn build_file_export_map(index_symbols: &[TsSymbol])')
content = content.replace('for item in &index.symbols {', 'for item in index_symbols {')

# Fix resolve_import_source
content = content.replace('fn resolve_import_source(\n    importer_path: &str,\n    import_source: &str,\n    index: &TsIndex,\n) -> Option<String>', 'fn resolve_import_source(\n    importer_path: &str,\n    import_source: &str,\n    index_symbols: &[TsSymbol],\n) -> Option<String>')
content = content.replace('resolve_import_source(&caller.file_path, &import.source, index)', 'resolve_import_source(&caller.file_path, &import.source, index_symbols)')
content = content.replace('resolve_import_source(&re_export.file_path, &re_export.source, index)', 'resolve_import_source(&re_export.file_path, &re_export.source, index_symbols)')
content = content.replace('resolve_import_source(\n                    &re_export.file_path,\n                    &re_export.source,\n                    index,\n                )', 'resolve_import_source(\n                    &re_export.file_path,\n                    &re_export.source,\n                    index_symbols,\n                )')
content = content.replace('resolve_import_source(importer_path, &ts_import.source, index)', 'resolve_import_source(importer_path, &ts_import.source, index_symbols)')

# And inside resolve_import_source: it iterates index.symbols ?
content = content.replace('for item in &index.symbols {', 'for item in index_symbols {')
content = content.replace('for symbol in &index.symbols {', 'for symbol in index_symbols {')

# Fix check_call_against_import
content = content.replace('fn check_call_against_import(\n    call: &TsCall,\n    caller: &TsSymbol,\n    target: &TsSymbol,\n    index: &TsIndex,\n)', 'fn check_call_against_import(\n    call: &TsCall,\n    caller: &TsSymbol,\n    target: &TsSymbol,\n    index_symbols: &[TsSymbol],\n)')
content = content.replace('build_file_export_map(index)', 'build_file_export_map(index_symbols)')

# check_call_against_import calls in resolve_call
content = content.replace('check_call_against_import(call, caller, target, index)', 'check_call_against_import(call, caller, target, index_symbols)')

# ts_index.rs:1408
content = content.replace('for re_export in index_symbols.iter().flat_map(|s| &s.re_exports) {', 'for re_export in index_symbols.iter().flat_map(|s| &s.re_exports) {') # Keep

with open('src/ts_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
