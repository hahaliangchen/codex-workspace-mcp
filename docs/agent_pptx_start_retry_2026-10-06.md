# PPT 编辑器启动真实重试

日期：2026-10-06。

## 结果

用户自己的 Agent 已成功启动 `D:/enterpriseProject/pptx-editor-engine` 的现有前端并在浏览器打开；任务状态 completed。

- 页面：http://localhost:3000/
- 标题：PPTX Editor Engine。
- 启动脚本：npm run dev:frontend。
- 受管进程：project_1791284803306_1；工具报告 running=true、ready=true。
- WASM 构建完成，Rspack 编译成功；页面 HTTP 200。
- browser_open 和 browser_read 均确认匹配编辑器标题，实际文本包含“上传 PPTX”“打开演示文稿”“载入示例”。页面处于尚未加载文稿的欢迎状态。
- 没有安排业务源码修改，也没有要求加载示例或做画面渲染验收。现有启动脚本自动生成 WASM 构建产物。

## 执行方式

试跑前 Agent 服务没有运行，旧 exe 早于修复。先 cargo build 成功，再启动最新宿主，工作区绑定目标 PPT 项目；Agent 页面/API 保持 3001，MCP 接口使用 3002，保留 PPT 默认端口 3000。宿主 PID 为 15104。

创建新测试会话 task_1a110e3186b18dbebed335f3d08，开启上下文调试，沿用 gpt-6-luna/max。只下达原始启动需求，没有提供启动脚本、修改组织者输出或代替 Agent 启动目标应用。

Flow 实际顺序：

1. discover：发现现有脚本、配置端口、依赖及生成 WASM，查询一次现有进程，交付启动条件。
2. start-editor：消费前序交付，运行 dev:frontend，完成受管进程与 HTTP 就绪检查。
3. open-editor：browser_open 打开目标页面；查询具体 process_id 确认运行；browser_read 匹配实际标题和可见内容。
4. 汇总根目标，组织者 finish；没有契约纠错或人工继续指令。

本次只启动前端页面，没有启动 Rust 后端；满足本轮打开现有编辑器页面的目标，不代表所有依赖后端的功能已验证。

## 统计

- 任务用时：181.4 秒，约 3 分 1 秒，不含宿主构建恢复。
- 工具调用：15 次；run_project_script 1、browser_open 1、browser_read 1、get_project_process 2、read_file 4、list_dir 3、workspace_info 1、yield_work 2。
- 两次进程查询分别是启动前列表观察和打开页面后按具体 process_id 核验，没有重现之前反复确认空列表的六次查询。
- 模型请求：Organizer 4、Worker 8、Observer 6，总计 18。其中可能包含因状态切换中断的 Observer 请求，不把它们全部计为完整完成审查。
- Organizer 契约错误：0。

## 证据与限制

[任务会话](http://127.0.0.1:3001/agent/#/tasks/task_1a110e3186b18dbebed335f3d08)

[工具调用与请求统计](agent_pptx_start_retry_evidence_2026-10-06.json)

本轮确认实际进程和 DOM 页面身份，没有截图视觉验收，也没有确认示例 PPT 渲染。此次启动成功是修复后的一个真实样本，不能据此保证所有任务都不会重复调用。
