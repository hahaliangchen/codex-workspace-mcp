import sys

with open('src/rust_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

structs = """
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustFileInfo {
    pub file_path: String,
    #[serde(default)]
    pub uses: Vec<RustUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustUse {
    pub module_path: String,
    pub name: String,
    pub alias: Option<String>,
    pub is_wildcard: bool,
    pub line: usize,
}
"""

content = content.replace('pub type Result<T> = std::result::Result<T, RustIndexError>;', 'pub type Result<T> = std::result::Result<T, RustIndexError>;\n' + structs)

# Fix missing `index` in `build_context`
content = content.replace('resolve_call(index, symbol, call)', 'resolve_call(index_symbols, symbol, call)')
content = content.replace('                index', '                index_symbols')

# Fix unused import `WalkBuilder`
content = content.replace('use ignore::WalkBuilder;\n', '')

with open('src/rust_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
