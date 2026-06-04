import os

source_file = r"e:\项目\codex-workspace-mcp\src\ts_index.rs"
output_dir = r"e:\项目\codex-workspace-mcp\src\ts_index"

if not os.path.exists(output_dir):
    os.makedirs(output_dir)

with open(source_file, 'r', encoding='utf-8') as f:
    lines = f.readlines()

# 1-based line numbers in ts_index.rs
# types: 1 - 263
# api: 264 - 416
# parser: 417 - 1212
# resolve: 1213 - 1589
# utils: 1590 - 1790
# tests: 1791 - 2311

header = """use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use swc_common::{FileName, SourceMap, Span, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::*;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

use crate::ts_index::*;
"""

# Extract segments (0-based list slicing, so line N is index N-1)
# types: lines 1 to 263 -> indices 0 to 263
types_content = "".join(lines[0:263])

# api: lines 264 to 416 -> indices 263 to 416
api_content = header + "\n" + "".join(lines[263:416])

# parser: lines 417 to 1212 -> indices 416 to 1212
parser_content = header + "\n" + "".join(lines[416:1212])

# resolve: lines 1213 to 1589 -> indices 1212 to 1589
resolve_content = header + "\n" + "".join(lines[1212:1589])

# utils: lines 1590 to 1790 -> indices 1589 to 1790
utils_content = header + "\n" + "".join(lines[1589:1790])

# tests: lines 1791 to 2311 -> indices 1790 to 2311
tests_content = "".join(lines[1790:])

# Write new files
with open(os.path.join(output_dir, "types.rs"), "w", encoding="utf-8") as f:
    f.write(types_content)

with open(os.path.join(output_dir, "api.rs"), "w", encoding="utf-8") as f:
    f.write(api_content)

with open(os.path.join(output_dir, "parser.rs"), "w", encoding="utf-8") as f:
    f.write(parser_content)

with open(os.path.join(output_dir, "resolve.rs"), "w", encoding="utf-8") as f:
    f.write(resolve_content)

with open(os.path.join(output_dir, "utils.rs"), "w", encoding="utf-8") as f:
    f.write(utils_content)

with open(os.path.join(output_dir, "tests.rs"), "w", encoding="utf-8") as f:
    f.write(tests_content)

# Overwrite original ts_index.rs as mod facade
facade_content = """pub mod types;
pub mod utils;
pub mod resolve;
pub mod parser;
pub mod api;

pub use types::*;
pub use utils::*;
pub use resolve::*;
pub use parser::*;
pub use api::*;

#[cfg(test)]
mod tests;
"""

with open(source_file, "w", encoding="utf-8") as f:
    f.write(facade_content)

print("Split completed successfully!")
