import json

with open(r'e:\项目\codex-workspace-mcp\target\release\ai_proxy.log', encoding='utf-8', errors='replace') as f:
    lines = f.readlines()

body_lines = [l for l in lines if 'Codex Responses body' in l]
print(f'Codex Responses body 行数: {len(body_lines)}')

for i, line in enumerate(body_lines):
    marker = 'Codex Responses body: '
    idx = line.find(marker)
    if idx == -1:
        continue
    json_str = line[idx + len(marker):].strip()
    
    try:
        data = json.loads(json_str)
    except Exception as e:
        # 日志截断了，尝试找 previous_response_id
        prev_match = '"previous_response_id"' in json_str
        print(f'\n[{i+1}] JSON解析失败(可能被截断): {str(e)[:60]}')
        print(f'  含 previous_response_id: {prev_match}')
        print(f'  前300字符: {json_str[:300]}')
        # 扫描关键字
        for kw in ['previous_response_id', 'function_call', 'assistant', 'messages']:
            print(f'  含"{kw}": {kw in json_str}')
        continue

    keys = list(data.keys())
    prev_id = data.get('previous_response_id')
    input_items = data.get('input', [])

    type_counts = {}
    for it in input_items:
        if isinstance(it, dict):
            t = it.get('type', 'no-type')
        else:
            t = type(it).__name__
        type_counts[t] = type_counts.get(t, 0) + 1

    assistant_items = [it for it in input_items
                       if isinstance(it, dict) and (it.get('role') == 'assistant' or it.get('type') == 'function_call')]
    message_items = [it for it in input_items
                     if isinstance(it, dict) and it.get('type') == 'message']

    print(f'\n===== 请求 {i+1} =====')
    print(f'  顶层字段: {keys}')
    print(f'  previous_response_id: {repr(prev_id)}')
    print(f'  input条目数: {len(input_items)}, type分布: {type_counts}')
    print(f'  assistant/function_call条目数: {len(assistant_items)}')
    print(f'  message type条目数: {len(message_items)}')
    # 打出非系统的 message 条目
    non_sys_msgs = [it for it in message_items if it.get('role') != 'user' or 
                    (isinstance(it.get('content'), list) and any('permissions' not in str(c) and 'app-context' not in str(c) for c in it.get('content', [])))]
    if assistant_items:
        print(f'  assistant首条: {json.dumps(assistant_items[0], ensure_ascii=False)[:400]}')
    if non_sys_msgs:
        print(f'  非系统message首条: {json.dumps(non_sys_msgs[0], ensure_ascii=False)[:400]}')
