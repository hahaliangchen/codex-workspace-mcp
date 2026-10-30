# 视觉检查修复复核

日期：2026-10-06。复核上次 `agent_visual_inspection_review_2026-10-06.md` 的三个问题和当前实际执行链。

## 结论

上次三个错误放行漏洞已有有效修复，并增加了对应回归。此次仍复现一个新截图版本同步问题：截图首次发现页面变化时，它会在宿主接收后立即被判成历史材料。这影响正常视觉检查效率，建议继续修复。

## 上次问题的修复

| 问题 | 当前实现 | 复核结果 |
| --- | --- | --- |
| 重新发送旧截图可冒充当前图 | VisualContext 和 manifest 增加 current_page / source versions；is_current_result 比较截图的执行、页面和源码版本；pass/issue 必须引用当前结果图的事实 | 已修复原路径；前后对比可保留历史图 |
| uncertain 视觉服务被文字 Worker 升级成 pass | 服务结果先校验完整契约，绑定 service result ID；主角色判断须保留服务 assessment，事实必须存在于服务结果 | 已修复原路径，缺字段不能冒充有效服务结果 |
| 已保存 pass 不随源码更新失效 | update_versions 和读取到新 hash 清空视觉判断；return_work 校验宿主 source_binding 与当前 frame 一致 | 已修复原路径，重新塞入旧缓存结果也无法完成节点 |

Worker / Observer 使用相同的当前材料校验入口，调用方已经传入 current_page。Observer 的过期判断也增加了 session、执行版本和源码关系。

## 本轮发现：[P2] 新截图在接收后被误判为过期

代码位置：

- `src/browser_control.rs:151`：截图前读取页面状态；DOM 签名变化时推进 session.page_epoch；保存截图使用调用前传入的 context.execution_epoch。
- `src/work_scheduler.rs:909`：收到截图的新 page_epoch 后，再推进 frame.epoch。
- `src/visual_artifacts.rs:107`：当前材料判断要求 artifact.execution_epoch == frame.epoch。

### 实际复现

使用真实本地浏览器和现有调度器：

1. 打开 Canvas 页面，记录页面状态。
2. 页面 DOM 自行更新，模拟加载数据后的异步渲染；宿主尚未运行新的读取工具。
3. Worker 捕获当前画面。浏览器正确发现变化，截图包含更新后的页面，保存 execution_epoch=0、page_epoch=2。
4. scheduler.observe 接收截图，发现 page_epoch 变化，将 frame.epoch 改为 1。
5. 下一轮准备发送刚捕获的图片，current_result=false。

临时回归的失败输出：

```text
review_fresh_capture_after_async_page_change_is_current ... FAILED
fresh screenshot rejected as stale: captured_epoch=0, frame_epoch=1, page_epoch=2
```

这张图实际是最新画面，且页面与源码没有在截图后再次变化。问题来自捕获绑定与宿主状态推进的先后顺序。

### 影响

加载数据、点击后异步更新等常见页面流程中，Worker 需要多截图一次才可能交付。若页面每次捕获前都更新 DOM，则可能反复截图/推理，直到 blocked 或耗尽预算。现有静态页面、点击后立即同步更新的测试没有覆盖这个分支。

### 修复要求

- 统一“发现页面变化、推进宿主状态、绑定本次捕获”的顺序，使刚捕获的画面绑定接收后的有效版本。
- 可以先同步页面状态再用更新后的上下文捕获，或在同一受保护操作内完成同步和捕获；保持任务/实例校验。
- 仍要拒绝旧图，不能通过忽略所有 execution_epoch 或改写已有不可变截图来让回归通过。
- 增加浏览器运行链回归：异步 DOM 更新后首次截图即为 current_result；已有旧图仍为 false；捕获期间页面变化仍 uncertain；源码更新后恢复旧图仍被拒绝。
- Observer 主动捕获也检查同类版本关系；无法确定捕获期间归属时保持 uncertain。

## 验证记录

| 检查 | 本轮结果 |
| --- | --- |
| 后端完整回归 | 164 passed、0 failed、2 ignored，14.56 秒 |
| 前端测试 | 11 passed、0 failed |
| 前端构建 | TypeScript + Vite 成功，3.70 秒 |
| 新增异步截图复现 | 1 failed，证明新图被误判过期 |

首次在受限 Windows 沙箱中运行后端测试和 Vite 出现 canonicalize / realpath 权限错误；随后在自动审批允许的本机权限下重跑，得到上表结果。权限失败未作为代码回归缺陷。

复现函数保存在 [复现材料](agent_visual_inspection_recheck_repro_2026-10-06.rs)，追加到 `src/visual_tests.rs` 可使用已有 scheduler_context 和浏览器辅助方法；运行 `cargo test visual_tests::review_fresh_capture_after_async_page_change_is_current -- --nocapture`。

临时测试已撤回，源码恢复前后 SHA256 一致：`F2904AC46E247ACF29A756954E7A033A8A23A415D891CBD45EA276C9F928C069`。

本轮仅审查并保存报告/复现材料，没有修改执行代码、重启项目或运行新的真实模型验收。完整 PPT 加载成功和模型实际看图质量不由本轮自动回归证明。
