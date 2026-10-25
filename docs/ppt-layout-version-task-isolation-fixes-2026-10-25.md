# PPT 文本转换、版本契约与任务结果隔离修复

本次只修三个已确认的问题。远程 MySQL 的性能、连接池、超时和服务器磁盘配置未调整；没有调用真实模型或生成真实业务 PPT。

## 文本转换

引擎测量时比较浏览器的段落矩形与单一原生文本流能表达的位置。对于左右排布、独立宽度或其他无法由同一段落流表达的位置，记录实际文本块矩形；导出时生成原生分组，背景与每个文本块作为可编辑子对象。普通连续段落仍使用一个文本框。已有源对象转成分组时保留原物理 ID，分组关系与未修改的对象继续保留。转换后重新导入原生结构，不把旧的横向文本流数据覆盖回分组。

涉及引擎 src/design/model.ts、measure-dom.ts、export-core.ts、projection.ts、service-core.ts。没有强制 AI 使用某种布局，也没有恢复强制视觉验收。

## 版本契约

- get_design、apply_html 等页面设计工具返回 design_revision；apply_html 和 manage_design_pages 的公开参数为 expected_design_revision。
- 保存回执与图像来源的 document 使用 stored_revision；只读工具的保存状态明确为 base_stored_revision、stored_revision。
- 前端保存和恢复请求发送 expected_stored_revision，后端保存回复增加 stored_revision。旧 REST 请求及旧回复保持兼容；页面进度兼容旧历史的保存回执。
- 引擎内部设计模型仍使用自身 revision，不与保存版本进行推算或转换。旧页面直接调用的 expected_revision 保留为兼容别名，新工具声明不再提供该参数。
- PPT 技能、模板标注说明、前端调用及后端 page_tools.json 已同步。可选视觉记录可读取新 stored_revision，也可读取历史 revision。

## 旧任务恢复

Runtime.CallWithEvidence 清理当前用户、当前项目下没有执行器的旧任务后，继续处理当前调用。多个旧占位可以依次释放；旧错误留在旧任务自己的记录，当前调用有自己的工具名称、任务 ID 和返回值。仍有执行器的任务继续返回 not_ready；其他用户的任务不受影响。

回归覆盖数据库 INSERT 已提交但应答丢失、查询旧任务后重试、直接重试，以及旧 list_templates/read_files 占位不能冒充当前 init_project 结果。

## 验证与本地运行

- 引擎相关 48 项回归通过，包含 Flex/Grid 文本块转换、PPTX 重开、继续修改、已有原生文本框重排、普通连续段落、模板保护及原始 XML 保留。
- 前端保存、恢复、版本与进度相关 35 项通过；使用当前 Windows 引擎路径的 TypeScript 检查通过。
- Go PPT 任务、保存、页面接口和可选视觉证据相关回归通过。旧测试要求默认视觉验收的断言已同步为此前确认的按需检查流程，没有更改生产工作流。
- 引擎浏览器包、整合引擎源码的前端生产包、PPT 后端重新构建完成。WASM 源码未变化，复用现有构建。
- 确认 Agent 无活动任务后，重启 PPT 前端 6001、PPT 后端 18897 和转换/渲染服务 8787。Agent 3001 保持上一轮已部署版本。四个健康接口均返回 200。

产物、源码哈希、构建日志与健康记录位于 target/ppt-fixes-20261025；runtime.json 与后续全量重启配置已更新后端路径。页面需刷新以加载新前端和工具契约。未对真实业务稿件进行新的视觉验收。
