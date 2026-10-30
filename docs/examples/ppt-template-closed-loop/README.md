# 单模板示例

`unicom-brand-template.pptx` 是从真实联通/联通元景模板副本整理出的单页品牌模板。右上角双标识和品牌装饰固定，文字可变；用途标注保存在 PPTX 中。用于验证单页模板自由设计和页面复制，不是必须采用的页面骨架。

`acceptance-design.ts` 是验收 HTML 的来源，按本项目四仓库路径编写；在编辑器目录用 `npx vite-node /home/goodtime/Documents/project/codex-workspace-mcp/docs/examples/ppt-template-closed-loop/acceptance-design.ts` 执行。输出在 `/tmp/ppt-template-closed-loop-20261022`，不会覆盖原模板 fixture。

`converter-acceptance.json` 记录了 5/20 页转换与独立渲染验证。此脚本直接提交 AI 撰写的 HTML，不调用产品中的生成模型；实际产品生成结果单独记录在 [实施验收记录](../../archive/ppt-template-closed-loop-delivery-2026-10-22.md)。

完整制作规则见 [模板制作标准](../../ppt-template-authoring-standard.md)。不要通过 LibreOffice 另存该模板再当作用途仍完整的同一版本；已验证其会去掉用途扩展，重新导入需再次审视标注。
