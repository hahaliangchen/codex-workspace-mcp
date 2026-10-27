function author_pages(args) {
  const generated = compose_pages(args);
  const files = [];
  const issues = [];
  for (const page of generated.pages) {
    const written = io.write({path:page.path, content:page.content});
    const reread = io.read({path:page.path, max_chars:60000});
    const content = reread.content;
    const valid = content && content.includes("<!doctype html") && content.includes("<html") && content.includes("</html>") && content.includes("<title>") && content.includes("lang=\"zh-CN\"");
    if (!valid) issues.push({path:page.path, issue:"实际读回内容缺少完整中文HTML5文档结构"});
    files.push({path:page.path, title:page.title, write_result:written, read_evidence:{exists:!!content, chars:content ? content.length : 0, has_doctype:!!(content && content.includes("<!doctype html")), has_html_close:!!(content && content.includes("</html>")), has_zh_lang:!!(content && content.includes("lang=\"zh-CN\""))}});
  }
  return {owner:args.owner, files:files, issues:issues};
}
/** @virtual */
function compose_pages(args) {
  // 为传入的专属页面制作高质量中文独立专题网页。严格遵守 args.common 与每个 page 的 focus、requiredFacts、excludeOverlap；依据 source 核实事实，不混淆历史/当前设计及测试适用范围。返回 {pages:[{path,title,content}]}，每份 content 是完整可离线打开的 HTML5，内嵌响应式 CSS、中文语义结构、页内导航、分层章节、醒目重点框与页脚来源说明；无外部资源。三个文件内容互补但独立，避免重复成通用泛文。