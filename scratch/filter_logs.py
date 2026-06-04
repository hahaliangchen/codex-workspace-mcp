import re
import datetime
import sys

# 强制 stdout 输出为 utf-8，避免 GBK 编码报错
if hasattr(sys.stdout, 'reconfigure'):
    sys.stdout.reconfigure(encoding='utf-8')

log_file_path = "e:/项目/codex-workspace-mcp/target/release/ai_proxy.log"
output_file_path = "e:/项目/codex-workspace-mcp/scratch/filtered_output.log"

time_pattern = re.compile(r"^\[(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\]")
target_time = datetime.datetime.strptime("2026-06-04 13:55:00.000", "%Y-%m-%d %H:%M:%S.%f")

interesting_keywords = [
    "tool_calls", "tool_outputs", "Blocked", "exec_command", "index", 
    "symbol", "search", "workspace_info", "shell", "read_file", "write_file",
    "mcp__", "response", "upstream", "resolved", "error"
]

print("开始解析日志，输出到 filtered_output.log...")

with open(log_file_path, "r", encoding="utf-8", errors="ignore") as f, \
     open(output_file_path, "w", encoding="utf-8") as out:
    
    in_target_time = False
    line_num = 0
    match_count = 0
    
    for line in f:
        line_num += 1
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
            # 过滤不需要的非常冗长的 FORWARDING/RECEIVED SSE 字节日志，除非它们有 error
            if "RECEIVED" in line or "FORWARDING" in line or "FINISHED responses" in line or "SPAWNED responses" in line:
                if "error" not in line.lower():
                    continue
            
            # 判断是否包含我们感兴趣的关键字
            has_kw = any(kw.lower() in line.lower() for kw in interesting_keywords)
            
            # 如果行不太长或者包含关键字
            if has_kw or len(line) < 150:
                truncated_line = line if len(line) < 1000 else line[:1000] + " ... [TRUNCATED]"
                out.write(f"Line {line_num}: {truncated_line}")
                match_count += 1

print(f"解析完成，共匹配到 {match_count} 行日志，已写入 {output_file_path}")
