# 页面截图与视觉检查：独立实现复核

日期：2026-10-06。对照 `agent_visual_inspection_plan_2026-10-06.md`、当前源码和实施报告。

## 结论

Worker / Observer 实际图片输入主链路已实现，任务页面隔离、不可变图片、模型能力配置、前端展示和基本交付校验均有代码与回归覆盖。不过，本轮额外复现了 **3 个可接受错误视觉 pass 的漏洞**，还不能判定视觉验收闭环可靠。

本轮仅审查、运行测试并新增本报告与复现材料；没有修复执行代码，没有重启项目，没有新增真实模型请求。

## 已确认实现

- 浏览器按 workspace/task 建独立会话，操作使用页面锁；截图保存独立 artifact_id、PNG、hash、页面和执行身份。
- Worker 最终请求包含真实 image_url 内容块；持久上下文保留引用。MCP 返回实际图片内容块，协议转换保留图片。
- 图片预算独立，过大图片缩放并记录输入尺寸/hash，原图保留。
- supported 直接附图；unknown 不默默猜测模型能力；unsupported 只有显式视觉服务配置才降级。
- 视觉要求使用独立 output 节点，没有强行重开已完成的写入/检查节点。
- Observer 能复用修改前后两张图，缺图时有一次只读捕获路径；关闭 Observer 时 Worker 图片流程独立。
- Flow/工具结果展示材料和判断，调试视图折叠 base64。

## 待修复问题

### 1. [P1] 重新发送旧截图会把它包装成当前版本的有效验收材料

位置：`src/visual_artifacts.rs:101`、`:140`、`:187`；Worker 调用入口为 `src/agent_service.rs:3174`。

`authorize` 检查任务、请求和实例，但不检查截图捕获时的页面/源码版本。`prepare_request` 把当前 execution_epoch 放在请求 manifest 上，同时允许重新装配本实例的旧图片。`validate_check` 比较的是“本次请求 epoch == 当前 epoch”，没有要求被引用的图片中存在对应当前页面/源码版本的结果图。

**实际复现：** 打开有 Canvas 幻灯片的页面并截图，导航到空白页面，再重新选择旧截图、用新 epoch 发送图片。主模型返回引用旧图的 pass，宿主接受。两次截图 hash 不同，实际页面已改变；这是同一实例内旧图重新恢复的路径，不涉及跨任务权限绕过。

影响：Worker 可把操作前或修复前的画面认成当前交付；请求版本校验不能防止旧像素被重新包装。Observer 的结果校验也共用这个函数。

建议：

- 图片引用明确区分历史对比材料和当前结果材料；旧图仍可用于 before/after 对比。
- 当前验收的 pass/issue 必须有一个结果图匹配宿主已知的当前 browser_session/page/page_epoch 和相关源码/构建版本，且 observed_facts 引用该结果图。
- 不能仅因为旧图在新请求中重新发送，就更新它的有效性；失效结果要求聚焦重新截图，无法建立版本关系则 uncertain。
- 补充“只有旧图”“前后两图包含当前图”“导航后恢复旧图”“源文件修改后恢复旧图”的回归。

### 2. [P1] 视觉服务没有确定观察结果，文字 Worker 仍可编造通过

位置：`src/visual_artifacts.rs:175`、`:187`。

视觉服务 JSON 除 assessment=unavailable 外一律被标成 fallback，包括 uncertain 和不完整对象。随后 `validate_check` 对 fallback 只确认图片引用、请求身份以及 Worker 自报的事实，没有校验视觉服务本身是否提供与图片绑定的有效观察/判断。

**实际复现：** Worker 声明 unsupported；显式视觉服务收到图片后返回 assessment=uncertain、observed_facts=[]。Worker 请求实际没有任何图片内容块，但它填写 pass 和自造 observed_facts 后，宿主仍接受该判断。

影响：虽然 UI 记录为 fallback，最终通过结论并无可追溯的有效视觉服务事实；不支持图片的角色可以绕过实际看图限制。

建议：

- 降级服务自身返回完整、绑定图片和目标的结果，宿主先校验该结果，再提供给 Worker / Observer。
- unavailable、缺失字段、无有效观察的 uncertain 保持不可作为 pass 的来源；主角色不能自行新增它未看到、服务也没提供的图片事实。
- 视觉服务结论与主角色交付分开记录，交付检查明确引用已校验的服务结果 ID。
- 对无效响应有界返回限制，不通过无限重试追求 pass。

### 3. [P1] 已保存的视觉通过结论不会随所有源码版本更新失效

位置：`src/work_scheduler.rs:987`、`:1038`。

浏览器交互和实际写入会清空 visual_check_result，但 `update_versions` 仅增加 epoch、清空编译检查并更新 versions，没有清空视觉判断。`return_work` 对已保存结果只检查 assessment 是否 pass/issue，不再次检查保存时的执行版本。

**实际复现：** 先通过真实图片装配和 `validate_check` 得到有效 pass，存入 frame；调用 `update_versions` 注入不同源码 hash，确认 epoch 已增加；然后 `yield_work` 不带新视觉结果，节点仍可完成。

影响：已检测到源码变化，旧视觉结论仍满足交付。即使修复问题 1 的重新装配路径，这条缓存路径也要处理。

建议：

- 所有会使视觉结论过期的源码/构建/页面更新统一清空结果。
- 宿主在保存判断时写入不可由模型覆写的执行版本、页面和源码关系；交付时校验这些关系仍有效。
- 补充“校验通过后检测版本变化再交付”的回归，避免只测试同轮浏览器操作后的新结果拒绝。

## 本轮验证

| 项目 | 结果 |
| --- | --- |
| 现有 `cargo test -- --nocapture` | 161 passed，0 failed，2 ignored；17.12 秒 |
| 前端 `node --test tests/*.test.mjs` | 11 passed，0 failed |
| 前端 `npm run build` | TypeScript + Vite 成功；4.02 秒 |
| `git diff --check` | 通过，只有仓库现存 CRLF 提示 |
| 额外审查复现 | 3 项拒绝错误 pass 的断言均失败，即当前宿主实际接受了这些错误结果 |

复现函数保存在 [复现材料](agent_visual_inspection_review_repro_2026-10-06.rs)。将其追加到 `src/visual_tests.rs`，可复用该模块已有 TestRoot、PNG、context 和脚本模型辅助方法；运行 `cargo test visual_tests::review_ -- --nocapture`。

本轮临时测试已从源码撤回；`src/visual_tests.rs` 恢复前后 SHA256 一致：`A72E67CD7F48270A626F8BA89855624177443E769378775BBFBE8A7BFCE8ABA7`。报告中的复现材料不参与日常测试编译。

## 真实模型与启用情况的边界

本轮核对了仓库保留的验收报告，没有再次运行真实网络验收。已有记录表明模型能识别空白 PPT 页面以及编辑器组件删除前后的差异；同时保留了 checked_goal 改写、artifact_ids 遗漏和 Observer 超时。示例 PPT 加载仍为 issue，不能说完整 PPT 产品渲染验收已通过。

实施报告说明全局模型能力仍为 unknown，只有隔离验收工作区临时声明图片能力。实现代码具备支持图片的链路，不代表当前生产会话已启用直接看图；需核对实际 provider/model 的能力配置和运行服务版本。
