import json
import os

log_path = r"e:\项目\codex-workspace-mcp\target\release\ai_proxy.log"

if not os.path.exists(log_path):
    print("Log file not found at:", log_path)
    exit(1)

with open(log_path, "r", encoding="utf-8", errors="ignore") as f:
    lines = f.readlines()

print(f"Total lines in log: {len(lines)}")

turn_count = 0
for idx, line in enumerate(lines):
    if line.startswith("[2026") and "Codex raw input:" in line:
        turn_count += 1
        p_idx = line.find("Codex raw messages:")
        # Find timestamp
        ts = line[1:24]
        
        # Extract raw input
        inp_idx = line.find("Codex raw input:")
        raw_input_str = line[inp_idx + len("Codex raw input:"):].strip()
        
        try:
            raw_input = json.loads(raw_input_str)
            assistant_msgs = []
            for item_idx, item in enumerate(raw_input):
                itype = item.get("type")
                irole = item.get("role")
                if itype == "message" and irole == "assistant":
                    assistant_msgs.append((item_idx, item))
            
            print(f"Turn {turn_count} at {ts}: total_items={len(raw_input)}, assistant_messages_count={len(assistant_msgs)}")
            for item_idx, msg in assistant_msgs:
                itext = msg.get("text", "")
                icontent = msg.get("content", [])
                text_val = ""
                if icontent:
                    text_val = " ".join([p.get("text", "") for p in icontent if p.get("type") == "text"])
                else:
                    text_val = itext
                print(f"  - Item {item_idx}: {text_val[:120]}")
        except Exception as e:
            pass
