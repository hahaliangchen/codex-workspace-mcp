import sys

# 强制 stdout 输出为 utf-8，避免 GBK 编码报错
if hasattr(sys.stdout, 'reconfigure'):
    sys.stdout.reconfigure(encoding='utf-8')

log_file_path = "e:/项目/codex-workspace-mcp/target/release/ai_proxy.log"

with open(log_file_path, "r", encoding="utf-8", errors="ignore") as f:
    lines = f.readlines()

# 从后往前找最后一个 "/v1/responses received from Codex" 
last_response_index = -1
for i in range(len(lines) - 1, -1, -1):
    if "=== /v1/responses received from Codex" in lines[i]:
        last_response_index = i
        break

if last_response_index != -1:
    print(f"找到最后一次请求的起始行: Line {last_response_index + 1}")
    # 打印接下来的 200 行
    end_line = min(len(lines), last_response_index + 200)
    for j in range(last_response_index, end_line):
        line = lines[j].strip()
        if "messages" in line or "input" in line or "instructions" in line:
            # 过滤掉冗长的 messages 等输入参数，只打印关键信息
            if len(line) > 300:
                line = line[:300] + " ... [TRUNCATED]"
        print(line)
else:
    print("未找到 responses 记录")
