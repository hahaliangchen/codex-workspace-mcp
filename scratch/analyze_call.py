import re

import sys
if hasattr(sys.stdout, 'reconfigure'):
    sys.stdout.reconfigure(encoding='utf-8')

filtered_log_path = "e:/项目/codex-workspace-mcp/scratch/filtered_output.log"
call_id = "call_ebc9fa74fbf34c57948ce9e8"

with open(filtered_log_path, "r", encoding="utf-8", errors="ignore") as f:
    lines = f.readlines()

for i, line in enumerate(lines):
    if call_id in line:
        # 打印前后 5 行
        start = max(0, i - 2)
        end = min(len(lines), i + 10)
        print(f"--- Context for {call_id} ---")
        for j in range(start, end):
            print(lines[j].strip())
