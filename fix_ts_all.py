import re

with open('src/ts_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Replace all index: &TsIndex with index_symbols: &[TsSymbol]
content = re.sub(r'index:\s*&TsIndex', 'index_symbols: &[TsSymbol]', content)

# Function check_call_against_import
content = content.replace('build_file_export_map(index)', 'build_file_export_map(index_symbols)')
content = content.replace('resolve_import_source(&caller.file_path, &import.source, index)', 'resolve_import_source(&caller.file_path, &import.source, index_symbols)')
content = content.replace('imported_call_matches(index, item, call, symbol)', 'imported_call_matches(index_symbols, item, call, symbol)')
content = content.replace('resolve_import_source(&symbol.file_path, &import.source, index)', 'resolve_import_source(&symbol.file_path, &import.source, index_symbols)')
content = content.replace('resolve_reexports(\n                            index,\n', 'resolve_reexports(\n                            index_symbols,\n')
content = content.replace('resolve_import_source(&re_export.file_path, &re_export.source, index)', 'resolve_import_source(&re_export.file_path, &re_export.source, index_symbols)')

# missing re_exports
content = content.replace('            import_bindings: Vec::new(),\n            imports: Vec::new(),\n        })', '            import_bindings: Vec::new(),\n            imports: Vec::new(),\n            re_exports: Vec::new(),\n        })')
content = content.replace('            import_bindings,\n            imports,\n        })', '            import_bindings,\n            imports,\n            re_exports: Vec::new(),\n        })')

# resolve_import_source calls
content = content.replace('resolve_import_source(\n                    &re_export.file_path,\n                    &re_export.source,\n                    index,\n                )', 'resolve_import_source(\n                    &re_export.file_path,\n                    &re_export.source,\n                    index_symbols,\n                )')
content = content.replace('resolve_import_source(importer_path, &ts_import.source, index)', 'resolve_import_source(importer_path, &ts_import.source, index_symbols)')

with open('src/ts_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
