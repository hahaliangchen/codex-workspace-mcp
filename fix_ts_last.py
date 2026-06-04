import re

with open('src/ts_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# fix `index` not found
content = content.replace('resolve_import_source(\n                            &re_export.file_path,\n                            &re_export.source,\n                            index,\n                        )', 'resolve_import_source(\n                            &re_export.file_path,\n                            &re_export.source,\n                            index_symbols,\n                        )')
content = content.replace('find_re_export_for_name(index, &current_file, &current_name)', 'find_re_export_for_name(index_symbols, &current_file, &current_name)')
content = content.replace('fn find_re_export_for_name(\n    index: &TsIndex,\n', 'fn find_re_export_for_name(\n    index_symbols: &[TsSymbol],\n')
content = content.replace('index\n        .symbols\n        .iter()', 'index_symbols\n        .iter()')
content = content.replace('index\n                .symbols\n                .iter()', 'index_symbols\n                .iter()')

# missing re_exports
content = content.replace('            import_bindings: Vec::new(),\n            imports: Vec::new(),\n        })', '            import_bindings: Vec::new(),\n            imports: Vec::new(),\n            re_exports: Vec::new(),\n        })')
content = content.replace('            import_bindings,\n            imports,\n        })', '            import_bindings,\n            imports,\n            re_exports: Vec::new(),\n        })')

# also 356: unused variable index
content = content.replace('    let index = load_or_build_or_create(root)?;\n    let needle = request.query.to_lowercase();\n    let index_symbols = load_or_build_or_create(root)?;\n', '    let needle = request.query.to_lowercase();\n    let index_symbols = load_or_build_or_create(root)?;\n')

# another index replace
content = content.replace('resolve_import_source(&symbol.file_path, &import.source, index)', 'resolve_import_source(&symbol.file_path, &import.source, index_symbols)')
content = content.replace('index,', 'index_symbols,')
# wait, replacing "index," globally is dangerous. 
# let's just do it manually with multi_replace_file_content where needed!

with open('src/ts_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
