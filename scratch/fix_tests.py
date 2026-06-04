import os

path = r"e:\项目\codex-workspace-mcp\src\ts_index\tests.rs"

with open(path, 'r', encoding='utf-8') as f:
    lines = f.readlines()

new_lines = []
for i, line in enumerate(lines):
    if i == 0 and line.strip() == "#[cfg(test)]":
        continue
    if i == 1 and line.strip() == "mod tests {":
        continue
    if i == len(lines) - 1 and line.strip() == "}":
        continue
    # Unindent 4 spaces if present
    if line.startswith("    "):
        new_lines.append(line[4:])
    else:
        new_lines.append(line)

header = """use std::{
    fs,
    path::{Path, PathBuf},
};
use crate::ts_index::*;

"""

content = header + "".join(new_lines)

with open(path, 'w', encoding='utf-8') as f:
    f.write(content)

print("Tests module unwrapped successfully!")
