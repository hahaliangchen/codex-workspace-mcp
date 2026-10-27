function build_pages(input) {
  const generated = create_page_content(input);
  const output = [];
  for (const page of generated.pages) {
    const saved = io.write({path: page.path, content: page.content});
    output.push({path: page.path, title: page.title, saved: true, evidence: "写入回执：" + JSON.stringify(saved) + "；字符数=" + page.content.length});
  }
  return {owner: input.assignment.owner, files: output};
}
/** @virtual
 * 根据 input.shared 中的源文档摘要、共享事实、页面规范与风格，为 input.assignment.pages 明确分配的页面生成完整、独立、中文 HTML 文本。只生成本分支的两个页面（上下文/记忆/协作；权限/可靠性/原则），正文准确避免把历史机制写成现行要求。每个元素返回 {path:string,title:string,content:string}，页面自包含 HTML/CSS，内部链接可链接另外四页的预期文件名但不依赖它们。
 */
function create_page_content(input) {
  // 为本分支的两个页面创作可直接保存的独立 HTML 文档。
}
