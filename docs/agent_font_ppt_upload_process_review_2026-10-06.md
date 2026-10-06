# 字体服务与 PPT 上传过程复核

日期：2026-10-06。任务：task_1a110e3186b18dbebed335f3d08，第 2 轮。

## 当前结果

8080 后端已经由用户 Agent 成功启动。进程 project_1791285687305_2，脚本 start:backend，ready_url 为 http://127.0.0.1:8080/api/fonts。启动日志报告扫描到 321 个字体 face。复核时该接口可访问，返回 145 个列表项；这是列表项与 face 的不同计数，不混为一谈。

PPT 打开验证未完成。browser_upload 在 DOM.querySelector 阶段失败，尚未执行 DOM.setFileInputFiles，也没有将样例交给编辑器解析。本轮最终因组织者后续契约错误结束，状态 failed。

## 已确认的上传工具缺陷（P1）

位置：src/browser_control.rs:143–147 与 237–249。

upload 调用顺序是 DOM.getDocument → DOM.querySelector → DOM.setFileInputFiles，传递 nodeId。但 call 每次都重新建立 WebSocket 连接并在返回时丢弃连接，前一连接登记的 DOM nodeId 不能用于后一连接。

页面 index.html 确实存在 input#file-uploader；本轮对实际 Agent 页面进行了只读对比，没有上传文件或修改页面：

| 查询方式 | 结果 |
| --- | --- |
| 连接 A 取根 nodeId，连接 B 用它 querySelector | -32000: Could not find node with given id |
| 同一连接内 getDocument、querySelector | 成功，返回 input nodeId=17 |

因此本次根因是浏览器工具内部连接生命周期，不是已证明的 PPT 文件错误、选择器错误或字体后端再次离线。“重新读页面”不能修复工具内部跨连接传 nodeId。

修复要求：至少让上传的三个 DOM 操作共用同一连接；更完整的做法是在 Session 中维护连接及命令 ID、分派响应与事件。导航导致节点失效时重新获取，限制针对失效节点的重试；不能把跨连接节点失效当成页面没有上传控件。补充真实文件输入回归，确认 files.length、文件名及 change 事件，并确保不是仅测试打开页面。

## 实际流程及效率

第 1–10 步为调查，第 11 步才启动后端。全轮 27 次实质工具调用：search_text 12、read_file 6、read_file_lines 2、list_dir 1、get_project_process 1、run_project_script 1、browser_read 1、browser_open 1、browser_upload 1、yield_work 1。

调查既包含必要的后端配置，也包含上传入口、FileReader、openFile、dropzone、按钮 ID 等多次搜索。在已确认后端脚本和 8080 字体 API 后，查上传实现不应成为启动后端的前置条件；可先完成启动，再在浏览器执行必要操作。上传工具本已有 input[type=file] 默认选择器，不必为了最初尝试穷举全部 UI 源码。

后端启动后的节点先验证字体、再打开 PPT。需要确认实际字体请求是否依赖 PPT 加载，然后按依赖安排：服务就绪 → 打开 PPT → 验证字体使用/加载结果。欢迎页的可见文本本身不足以证明字体加载成功。

浏览器 read 首次得到 about:blank，是这一轮 Agent 新建受控浏览器会话的页面，不代表用户打开的编辑器被关闭；后续 browser_open 正常进入 localhost:3000。

## 后续组织者错误

1. 第 18 步给 pending 的 open-sample 设置 resume=true，宿主拒绝：resume applies only to a blocked/paused node。
2. 第 19 步删除 resume 后，把工作包 ID ppt-inspect-services 和 backend-start 填入 dependency_inputs.node_id。实际节点 ID 分别为 inspect-services 和 start-services，宿主无法解析前序交付，拒绝该决策。

这两个错误都被归为 INVALID_WORK_CONTRACT，field_path=decision，rejected_value 却指向 flow_update。第二个真正错误在 orders[].dependency_inputs，反馈没有指出字段/正确身份，削弱了针对性纠错。

修复要求：组织者明确收到可恢复节点及其状态；依赖反馈分别列出 work_id/node_id/revision 对照并定位具体订单、具体依赖字段。不能用工作包 ID 静默替换为任意节点。一次纠正后停止的保护已生效，但它不能代替正确反馈。

本轮只检查与诊断，没有修改源码、重启服务或替代 Agent 上传样例。8080 服务仍可用，编辑器中 PPT 加载与字体验证仍未完成。
