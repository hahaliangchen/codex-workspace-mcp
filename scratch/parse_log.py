import json
import os

input_path = r'C:\Users\梁辰\.gemini\antigravity\brain\5b7dc7eb-afb5-45fc-b728-a0546e7fcf98\raw_input_pretty.json'

print(f"Reading: {input_path}")
if not os.path.exists(input_path):
    print("File not found")
    exit(1)

with open(input_path, 'r', encoding='utf-8') as f:
    data = json.load(f)

print(f"Total elements: {len(data)}")

# 查找所有非 function_call 和非 function_call_output 的元素
print("\n--- Non-Tool Elements in Input ---")
count = 0
for idx, item in enumerate(data):
    itype = item.get("type", "")
    if itype not in ["function_call", "function_call_output"]:
        count += 1
        # 只打印前 15 个和最后 15 个，避免太多
        if count <= 15 or len(data) - idx <= 15:
            print(f"\n[{idx}] type: {itype} | keys: {list(item.keys())}")
            # 打印完整的 JSON
            print(json.dumps(item, ensure_ascii=False, indent=2))

print(f"\nTotal non-tool elements: {count}")
