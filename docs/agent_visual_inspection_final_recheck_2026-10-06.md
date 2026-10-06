# 视觉检查：当前修复复核

日期：2026-10-06。范围：前两轮发现的视觉结果放行问题、新截图状态同步问题及相关 Worker / Observer 链路。

## 结论

本次检查范围内，前两轮发现的问题均已修复；未发现新的阻断问题。真实模型看图质量、完整 PPT 产品场景和运行服务是否加载了当前代码，不由本轮源码及自动测试证明。

## 状态同步修复

- WorkFrame 新增 current_visual_artifact_ids。成功截图由宿主接收并记录当前材料，原始截图元数据保持不可变。
- is_current_result 保留源码与 session/page/page_epoch 的匹配要求；仅对宿主接受的当前截图允许捕获 epoch 与接收后的 frame epoch 存在差异。
- view_image 不把旧图重新标成当前图。页面版本变化、源码版本变化及截图失败会撤销当前材料引用并清空旧视觉判断。
- Worker 构造 VisualContext 时传入宿主当前引用；Observer 复用材料使用同一判断逻辑。
- Observer 主动截图成功后，将本次实际页面身份及新截图引用放入自己的请求上下文，避免继续使用捕获前页面版本。

修复没有通过重写旧截图版本或全局忽略 execution_epoch 放宽校验。

## 回归覆盖

| 检查点 | 结果 |
| --- | --- |
| 异步 DOM 更新后首次截图 | 新图 current_result=true；前图仍为 false |
| 已接受新图随后源码改变 | 恢复该图也不能冒充当前结果 |
| 截图过程中页面发生变化 | 稳定性校验拒绝，结果 uncertain |
| 捕获失败后恢复旧图 | 当前身份撤销，旧图不再有效 |
| 旧截图、失效缓存、uncertain 视觉服务升级为 pass | 前一轮修复的对应回归继续通过 |
| Worker 独立图片输入与 Observer 复用/只读捕获 | 运行链回归继续通过 |

## 本轮运行结果

- `cargo test -- --nocapture`：167 passed、0 failed、2 ignored，14.69 秒。忽略项为需显式运行的真实网络验收。
- `node --test tests/*.test.mjs`：11 passed、0 failed。
- `npm run build`：TypeScript 与 Vite 成功，707 modules，3.69 秒。
- `git diff --check`：通过，只有现存 CRLF 提示。

后端测试与前端构建在自动审批允许的本机权限下执行，避免已确认的 Windows 沙箱路径解析限制。没有新增真实模型请求，没有修改执行代码、追加临时测试或重启项目。
