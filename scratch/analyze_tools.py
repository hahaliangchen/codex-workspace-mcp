import re

filtered_log_path = "e:/项目/codex-workspace-mcp/scratch/filtered_output.log"

# 正则匹配，如:
# Line 25218: [2026-06-04 13:56:43.493]    [AGENT] SQLite Registry Match: ID 'call_48d315b3796b4aeb8f27662e' -> tool 'codex_workspace_mcp__workspace_info'
# Line 25219: [2026-06-04 13:56:43.502]    [AGENT] Executing tool natively: workspace_info
# Line 25220: [2026-06-04 13:56:43.503]    [AGENT] Custom execution succeeded. len=732

match_pattern = re.compile(r"Line \d+: \[(.*?)\]\s+\[AGENT\] SQLite Registry Match: ID '(.*?)' -> tool '(.*?)'")
exec_pattern = re.compile(r"Line \d+: \[(.*?)\]\s+\[AGENT\] Executing tool natively: (.*)")
result_pattern = re.compile(r"Line \d+: \[(.*?)\]\s+\[AGENT\] (Custom execution succeeded|Custom execution failed|Error executing tool).*")

events = []
current_match = None
current_exec = None

with open(filtered_log_path, "r", encoding="utf-8", errors="ignore") as f:
    for line in f:
        # 寻找匹配
        m_match = match_pattern.search(line)
        if m_match:
            time_str, call_id, tool_name = m_match.groups()
            events.append({
                "type": "match",
                "time": time_str,
                "call_id": call_id,
                "tool": tool_name
            })
            continue
            
        m_exec = exec_pattern.search(line)
        if m_exec:
            time_str, tool_name = m_exec.groups()
            events.append({
                "type": "exec",
                "time": time_str,
                "tool": tool_name
            })
            continue
            
        m_res = result_pattern.search(line)
        if m_res:
            time_str, result_info = m_res.groups()
            # 获取整行结果信息以取得 len=... 或是错误详情
            events.append({
                "type": "result",
                "time": time_str,
                "info": line.strip()
            })
            continue

# 整理打印
print("--- 13:55 之后的原生工具调用统计 ---")
tool_stats = {}
last_tool = None

for ev in events:
    if ev["type"] == "match":
        print(f"[{ev['time']}] 匹配到伪装工具: ID={ev['call_id']} -> 原生工具={ev['tool']}")
        last_tool = ev['tool']
    elif ev["type"] == "exec":
        print(f"[{ev['time']}] 准备在本地原生执行工具: {ev['tool']}")
    elif ev["type"] == "result":
        print(f"[{ev['time']}] 执行结果: {ev['info'].split('[AGENT]')[-1].strip()}")
        if last_tool:
            tool_stats[last_tool] = tool_stats.get(last_tool, 0) + 1
            last_tool = None

print("\n--- 各工具调用次数统计 ---")
for t, count in tool_stats.items():
    print(f"- {t}: {count} 次")
