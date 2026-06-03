import json
import os
import re

log_path = r"e:\项目\codex-workspace-mcp\target\release\ai_proxy.log"

if not os.path.exists(log_path):
    print("Log file not found at:", log_path)
    exit(1)

with open(log_path, "r", encoding="utf-8", errors="ignore") as f:
    lines = f.readlines()

print(f"Total lines in log: {len(lines)}")

# Find all lines starting with [2026 and containing "Codex raw messages:"
msg_lines = []
for line in lines:
    if line.startswith("[2026") and "Codex raw messages:" in line:
        msg_lines.append(line)

print(f"Found {len(msg_lines)} raw messages lines.")

# Inspect the last few ones
for idx, line in enumerate(msg_lines[-10:]):
    p_idx = line.find("Codex raw messages:")
    json_str = line[p_idx + len("Codex raw messages:"):].strip()
    # Find timestamp at the beginning of the line
    ts_match = re.match(r"^\[(.*?)\]", line)
    ts_str = ts_match.group(1) if ts_match else "Unknown Time"
    
    print(f"--- Messages Entry {idx} at {ts_str} ---")
    if not json_str:
        print("(Empty)")
    else:
        try:
            msgs = json.loads(json_str)
            print(f"Parsed {len(msgs)} messages:")
            for m in msgs:
                role = m.get("role")
                content = m.get("content")
                print(f"  Role: {role}")
                if isinstance(content, str):
                    print(f"    Content (Str): {content[:150]}")
                elif isinstance(content, list):
                    print(f"    Content (List): {len(content)} items")
                    for pi, part in enumerate(content):
                        print(f"      [{pi}] Type: {part.get('type')}, Text: {part.get('text', '')[:150]}")
                else:
                    print(f"    Content (Other): {content}")
        except Exception as e:
            print("Parse failed:", e)
            print("Raw snippet:", json_str[:500])
