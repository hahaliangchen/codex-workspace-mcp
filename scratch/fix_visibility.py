import re
import os

folder = r"e:\项目\codex-workspace-mcp\src\ts_index"
files = ["types.rs", "utils.rs", "resolve.rs", "parser.rs", "api.rs"]

for file in files:
    path = os.path.join(folder, file)
    with open(path, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # 替换顶层非 pub 的 fn/const/struct
    content = re.sub(r"^fn\s+", "pub(crate) fn ", content, flags=re.MULTILINE)
    content = re.sub(r"^const\s+", "pub(crate) const ", content, flags=re.MULTILINE)
    content = re.sub(r"^struct\s+", "pub(crate) struct ", content, flags=re.MULTILINE)
    
    with open(path, 'w', encoding='utf-8') as f:
        f.write(content)

print("Visibility fix completed!")
