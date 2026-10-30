# Agent 页面操作与真实 PPT 上传修复计划

日期：2026-10-06。

依据：agent_font_ppt_upload_process_review_2026-10-06.md 的第 2 轮执行轨迹，以及实际 CDP 只读对比。本文是实施计划，未修改执行代码或重启服务。

## 一、能力目标和当前边界

Agent 已有 browser_open、browser_read、browser_click、browser_upload 和 screenshot 能力。通过浏览器调试协议给文件输入控件设置工作区文件，可以完成真实上传，不必操作系统文件选择对话框。

当前 browser_upload 因跨连接使用 DOM nodeId 失败，需要先修复。修复后要验证实际 File 选择、change 事件及项目 loadPptx 结果，不能将工具调用成功等同于加载完成。

项目“载入示例”按钮在 src/demo-entry.ts:399 调用 viewer.loadVirtualDocument(mockPresentation)。这是内置 JSON 示例，不会验证工作区真实 .pptx 的文件读取、ZIP 解析和字体路径。真实文件任务必须使用 file-uploader 上传工作区样例。

现有 launch 固定 --headless=new，操作独立 Chrome 会话。这个会话的页面状态不会同步到用户当前打开的 localhost:3000 标签页。

- 自动验证目标：可沿用无头会话，返回实际结果与截图。
- 给用户展示已加载 PPT：需要由宿主管理可见浏览器会话并在其中操作。为 browser_open 增加可见展示能力，保留任务内页面所有权与连接复用；工具返回清楚的会话身份和显示状态。
- 操作用户当前标签页：必须有受支持的连接/附着能力及明确目标，不能仅凭 URL 相同假设它就是 Agent 页面。未实现附着时如实说明，使用 Agent 自己的可见窗口展示。

## 二、修复上传工具（首先实施，P1）

涉及：src/browser_control.rs。

1. 抽出同一连接上的 call_on_connection，上传的 DOM.getDocument → DOM.querySelector → DOM.setFileInputFiles 在同一连接完成，分配不同 command id。
2. 此次最低限度修复无需重写所有浏览器工具；后续如采用会话级持久连接，需要同时实现响应匹配、事件处理、断线恢复及串行页面操作。
3. 失效节点时在同一有效页面重新定位，最多有限次数重试。不要自动反复刷新页面，也不让 Worker 为工具内部 nodeId 失效重新搜索业务源码。
4. 验证控件确实为 input[type=file]；若存在多个输入框，返回简明控件候选给 Worker，避免无目的搜索整个 UI 实现。
5. 外部浏览器文件参数使用兼容的普通 Windows 路径，内部工作区和符号链接边界校验保留；不以清除 canonical 校验来修复兼容性。
6. 设置文件后检查 files.length、实际文件名及 change 是否被项目处理。结果区分 file_assigned 和 document_loaded；初次上传只返回前者与加载状态。
7. 分类错误：selector_not_found、stale_dom_node、file_unavailable、page_changed、upload_assignment_failed。返回操作阶段与下一项必要信息。

## 三、让流程按实际依赖顺序执行

本任务在已知前端、后端脚本和字体端点的情况下，合理顺序为：

```text
A 启动缺失后端并确认字体 API
  → B 在编辑器中上传真实示例 PPT
    → C 等待文稿加载并检查字体与实际渲染结果
      → 交付给用户
```

已有服务与有效结果直接消费。确实缺少启动方式时才插入短的发现节点，只要求必要启动输入：脚本、端点、运行状态。不要把上传按钮实现、FileReader 和全部 UI 路径调查塞成后端启动的前置任务。

确认字体 API 可用属于服务节点；编辑器实际字体加载属于真实文稿节点。实际 font 使用若依赖文稿，就必须先加载文稿，不能要求欢迎页凭空展示字体加载结果。

Worker 输入只包含当前节点的目标、必要材料和上游交付。B 只需要编辑器地址、样例路径和必要服务结果，不继承 A 的全部源码调查过程。发现错误时明确失败阶段，向组织者返回具体 blocker，不能空报“再验证一下”。

普通可恢复工具错误在当前 Worker 中处理；工具内部缺陷则上报阻塞或派发针对工具的修复，不修改 PPT 项目来绕过宿主缺陷。

## 四、修组织者状态与依赖身份

涉及：src/work_scheduler.rs、src/flow_tree.rs、src/work_organizer.rs、src/agent_service.rs。

1. 组织者输入提供 node_id、work_id、revision、status、可用交付字段及合法下一操作对照。
2. pending 节点尚未执行，直接派发；只有 blocked/paused 能 resume；done 保持封存，缺陷使用 revisit；新增下一步使用新节点。
3. 依赖引用保留明确身份：work_id 指一次精确工作实例，node_id 指 Flow 节点。提供可选择的宿主交付引用，减少模型自己重新拼 ID。
4. 校验错误指向具体 JSON 路径，例如 orders[0].dependency_inputs[0].node_id；同时返回对应合法 work_id/node_id、revision 与字段。非法 resume 返回当前节点状态和允许操作。
5. 不再把这些错误都归为 field_path=decision，却给 rejected_value=flow_update。保留事务性校验和一次针对性纠正；停止无效循环的机制不变。
6. Observer关注实际数据依赖、调查是否拖延可执行步骤、是否已有交付、是否用错误证据宣布完成；它不能用自然语言催促代替工具和调度器修复。

## 五、页面观察工具应能支持结束判定

为 browser_read 增加简明交互控件描述，或提供 browser_inspect 返回按钮、文件输入和必要加载状态；避免 AI 为一个文件输入控件反复读业务源码。

提供有超时的浏览器等待方法，按明确条件等待元素、文本或加载状态。避免多轮模型调用只为问“好了没有”。

字体排错需要被动的错误观测：捕获导航后受限数量的 console error、page exception、失败的网络请求及状态码，按 URL/请求阶段归类。可以做独立读取方法，不必给 Worker任意页面执行代码能力。日志应聚焦当前工作，限制大小，排除无关数据。

真实 PPT 加载验收至少包含：

- 输入框记录选中正确文件；
- 页面从欢迎状态进入文稿状态，幻灯片数大于零、页码更新；
- 解析或加载错误如实上报，未完成时不能被标题“PPTX Editor Engine”误判为通过；
- 字体目录/实际字体请求成功，或明确列出缺失字体与回退情况；
- 涉及画面正确性的结论必须经截图和实际图像输入检查，不能仅凭有 canvas 或 DOM 文本称渲染无误。

## 六、实施顺序与回归要求

顺序：上传同连接修复 → 精确状态/依赖反馈 → 页面等待及诊断 → 用户展示能力 → 完整真实任务验收。

后续验证应覆盖：

1. 临时 HTML 文件输入：正确文件名、files.length=1、change 事件被接收，跨调用不遗失 DOM 会话。
2. 隐藏输入、多个文件输入、中文/空格路径及导航导致的节点失效；失败阶段准确，不无限重试。
3. pending 不允许 resume；正确 work_id/node_id 依赖有效，误用时返回精确字段反馈。
4. 真正工作区 .pptx 被上传并显示非零幻灯片；不能用 mockPresentation 按钮替代这项验证。
5. 人为延迟加载和失败字体请求能被观察，等待超时后报告实际错误。
6. 若承诺用户直接看到已加载文稿，验证实际可见的受控浏览器窗口；访问同一个 URL 的另一标签页不算状态同步。

不强行设定所有任务统一工具次数上限。重点减少没有新信息的源码查询和模型往返，已知可执行条件及时进入下一节点。
