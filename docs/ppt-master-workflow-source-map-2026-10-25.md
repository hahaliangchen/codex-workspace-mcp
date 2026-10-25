# PPT 制作流程来源与维护边界

本轮已将活动插件的制作流程对照上游整理。只应用流程，沿用现有工具方法、参数、HTML 规范、转换行为和大纲确认卡片；未改通用 Agent，也未运行真实生成测试。

活动目录：`target/ppt-real-e2e-20261022/ai-ppt-server/resource/ai_skills/ppt-master`。

上游：[hugohe3/ppt-master](https://github.com/hugohe3/ppt-master/tree/78759bf62e9a1ef43e926ff9e50afa4cfe9a63cb/skills/ppt-master)，版本 6.7.3，提交 `78759bf62e9a1ef43e926ff9e50afa4cfe9a63cb`。本地原版在 `D:/enterpriseProject/ppt-master/skills/ppt-master`，原样提取副本在 `D:/enterpriseProject/ppt-master-skill`，均不修改。

## 流程对应

| 本地流程/职责 | 上游依据 | 在当前产品中的落实 |
| --- | --- | --- |
| 需求、来源材料、模板准备 | `workflows/generate-pptx.md` Step 1–3 | 使用已有用户需求、附件与模板选择，缺关键事实才补问；已有项目直接复用 |
| 沟通目标与完整方案 | `references/plan-core.md`，Generate Step 4 | 分析受众、目的、预期结果、核心信息、使用场景；保存完整页纲、内容关系、品牌与资源需求 |
| 方案确认与事实归属 | Generate Step 4，`references/artifact-ownership.md` | 继续使用 `commit_design_spec` 和现有版本化大纲确认卡片；共享决策与逐页内容各归其原有字段，不另造计划/锁文件 |
| 按需素材准备 | Generate Step 5，artifact ownership | 仅准备实际需要且现有工具支持的资源，必需资源不可用时如实报告 |
| 策划与页面表达分工 | `references/executor-base.md`，Generate Step 6 | 已确认内容、关系、页序、资源和品牌约束有效；页面构图与表达自由选择 |
| 逐页制作、完整内容覆盖 | Generate Step 6，`workflows/stages/resume-execute.md` | 使用当前有效材料及页面基线，按完整页纲制作；使用现有 HTML 工具，不采用上游 SVG 命令 |
| 实际失败与阶段恢复 | `workflows/governance/failure-recovery.md` | 修复失败所属内容或资源，只重做受影响操作及后续工作；警告继续为提示；不从头发现模板或重做已有效方案 |
| 检查、保存、导出 | Generate Step 6–7，`workflows/stages/visual-review.md` | 核对内容覆盖与实际保存结果，再用现有导出工具；模型视觉检查只在明确请求时运行 |

## 活动文件归属

- `SKILL.md`：入口、资源导航及既有工具/格式契约；移除任务创建、上下文压缩和历史测试页数指令。
- `references/production-workflow.md`：唯一制作顺序，覆盖从需求到修订交付的阶段衔接。
- `references/strategist.md`：策划要决定的内容，不再另设八项聊天确认或要求先确认后保存。
- `references/executor-base.md`：已确认内容与自由表达的分工，不再要求 SVG 页面、旧后处理命令或通用 Agent 调度。
- `AGENTS.md`：面向维护者记录本次边界及以后必须先对照上游的要求，不作为制作提示词加载。

改前快照：`target/ppt-skill-flow-20261025/before`，对应 SHA-256 记录在同级 `before-sha256.json`。未删除其他历史参考文件；新制作入口不将旧 SVG 技术参考列为活动执行依据。

## 明确保留的本地差异

本地继续使用现有 HTML 编辑与转换、异步初始化、设计/保存版本字段、模板语义、诊断提示、大纲卡片和导出接口。上游两阶段确认界面、SVG 检查器、预览服务器、脚本后处理及尚未接入的生产能力不导入；不声称完整照搬上游工具链。

本次没有修改前端、引擎、Go/Rust 实现或工具定义。也不以流程提示词修复通用 Agent 的资产驻留问题：有效材料若被执行层移出上下文，仍需在通用层另行排查。

## 后续用户测试发现流程问题时

先定位该问题属于哪一制作阶段，再对照上述上游对应文档，确认其输入、内容归属、确认条件、输出和恢复位置。修本地对应流程及确有冲突的引用，并更新本表；不累加针对某一轮测试的临时禁令，不越界修改工具或通用 Agent。若上游版本变化，先记录新提交及相关流程差异，再决定是否采用。

## 本轮验证与加载

skill-creator 的 `quick_validate.py` 校验通过；四份活动制作文档的九个 `skill:` 资源引用均存在。快照比较确认 `SKILL.md` 中既有工具契约及其后的转换、图标、视觉检查、附件等段落未改动。

三项现有本地单元测试通过：`TestSkillMakesVisualInspectionOptional`、`TestAsyncInitializationAndTemplateAllowIndependentSkillReads`、`TestBatchReadsValidateEveryPathAndPreserveReadResults`。首次沙箱内执行的临时文件替换被 Windows 拒绝，移到沙箱外按相同用例重跑通过；使用本地测试资源，未调用真实模型。

确认 Agent 没有运行或取消中的任务后，调用现有 `POST /agent/skill-packages/ppt-master/reload` 刷新插件缓存，返回插件已启用。后端继续按当前配置从活动目录读取参考文档；未重新编译或重启前端、引擎、后端、Agent，未发起实际制作请求。已存在的对话历史不会因刷新而被重写，后续新测试应以新一轮加载的 skill 为准。
