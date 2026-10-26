function* makePage(input) {
  const prompt = `请依据提供的通用Agent设计材料，写一个完整、独立、可直接打开的中文HTML介绍页面。只返回HTML，不要Markdown围栏。页面主题：${input.page.title}。本页重点：${input.page.focus}。
公共材料：${input.shared}
制作要求：完整doctype/html/head/body；lang=zh-CN；中文标题和有层次的正文；每页内容具体、充分且忠于材料，不得虚构未说明机制；语义化HTML、移动端响应式、清晰易读的现代视觉设计；CSS全部内嵌，无外部依赖、无JS、无资源链接；这是独立页面，不依赖其他文件；每页使用不同但协调的配色/视觉构图，并有本页主题标题、导读、至少3个内容区块、关键要点/总结。准确区分通用宿主与业务技能/插件的职责，finish不等于业务验收，串行子任务与fork并行Agent不同。文件名为 ${input.page.filename}。`;
  const html = yield ai({instruction: prompt, input: {shared: input.shared, page: input.page}, format: "text"});
  if (html.indexOf("<!DOCTYPE html>") < 0 || html.indexOf("<html") < 0 || html.indexOf("<body") < 0 || html.indexOf("</html>") < 0 || html.indexOf("<h1") < 0) { throw "生成页面HTML结构校验失败: " + input.page.filename; }
  const receipt = yield call(tools.write_file, {path: input.path + "/" + input.page.filename, content: html});
  return {file: input.page.filename, saved: receipt.changed};
}
function* worker(input) {
  let completed = 0;
  for (const page of input.pages) {
    const result = yield call(makePage, {page: page, shared: input.shared, path: input.path});
    if (!result.saved) { throw "文件未确认写入: " + page.filename; }
    completed = completed + 1;
  }
  return {agent: input.agent, completed: completed, files: input.pages};
}
function* main(input) {
  const left = yield fork(worker, input.left);
  const right = yield fork(worker, input.right);
  const joined = yield join(left, right);
  if (joined[0].completed != 10 || joined[1].completed != 10) { throw "执行Agent交付数量不符"; }
  return {joined: joined};
}