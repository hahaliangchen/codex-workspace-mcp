import re
import datetime
import json
import sys

# 强制 stdout 输出为 utf-8，避免 GBK 编码报错
if hasattr(sys.stdout, 'reconfigure'):
    sys.stdout.reconfigure(encoding='utf-8')

log_file_path = "e:/项目/codex-workspace-mcp/target/release/ai_proxy.log"

time_pattern = re.compile(r"^\[(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\]")
target_time = datetime.datetime.strptime("2026-06-04 14:11:10.000", "%Y-%m-%d %H:%M:%S.%f")

stream_content_buffer = []

with open(log_file_path, "r", encoding="utf-8", errors="ignore") as f:
    in_target_time = False
    for line in f:
        match = time_pattern.match(line)
        if match:
            time_str = match.group(1)
            try:
                line_time = datetime.datetime.strptime(time_str, "%Y-%m-%d %H:%M:%S.%f")
                if line_time >= target_time:
                    in_target_time = True
            except ValueError:
                pass
        
        if in_target_time and "RECEIVED" in line and "from upstream stream" in line:
            # 提取 RECEIVED 的字节内容
            # 日志格式可能如: [2026-06-04 14:11:27.931] >> RECEIVED 318 bytes from upstream stream
            # 实际上有些行可能后面跟着具体的 SSE data (data: ...)，我们需要看下一行或该行本身是否包含内容。
            # 我们可以直接在该行或其后的行中，寻找 "data: " 关键字
            pass
            
# 其实，我们可以直接寻找以 "data:" 开头的行，或者包含 "choices" 的 JSON 块
# 让我们换一种匹配方式：读取 14:11:10 之后的所有行，并完整打印非 FORWARDING 的所有文本
print("--- 14:11:10 之后的所有原始日志行 ---")
with open(log_file_path, "r", encoding="utf-8", errors="ignore") as f:
    in_target_time = False
    for line in f:
        match = time_pattern.match(line)
        if match:
            time_str = match.group(1)
            try:
                line_time = datetime.datetime.strptime(time_str, "%Y-%m-%d %H:%M:%S.%f")
                if line_time >= target_time:
                    in_target_time = True
            except ValueError:
                pass
        
        if in_target_time:
            # 排除掉 RECEIVED xxx bytes 和 FORWARDING xxx bytes
            if "RECEIVED" in line and "bytes from upstream" in line:
                continue
            if "FORWARDING" in line and "bytes to client" in line:
                continue
            print(line.strip())
