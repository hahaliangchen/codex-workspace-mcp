// 最小串行流程：设计 → 保存 → 读回 → 验收 → 交付。不启动子 Agent。
function main(request) {
  const draft = compose(request);
  const receipt = io.write({path: request.output, content: draft.content});
  const actual = io.read({path: request.output});
  const review = inspect(request, actual.content);
  assert(review.passed, "验收未通过，不能交付");
  return {path: request.output, receipt, review};
}

/** @virtual */
function compose(request) {
  // 按 request.topic 和 request.requirements 设计一个完整、独立的中文 HTML。
  // 当前执行 AI 直接完成设计，返回 {content:string}，不只返回计划。
}

/** @virtual */
function inspect(request, content) {
  // content 是已保存文件的真实读回正文；对照 request 检查内容与 HTML 结构。
  // 返回 {passed:boolean, issues:[string], evidence:[string]}。
  // 只做实际能完成的检查，未经浏览器渲染不能声称视觉验收通过。
}
