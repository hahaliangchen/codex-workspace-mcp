function main(request) {
  const rootListing = io.list({path: ".", recursive: false});
  const readmeSource = io.read({path: "README.md"});
  const programSource = io.read({path: "docs/agent-program.md"});
  const designSource = io.read({path: "docs/generic-agent-design.md"});
  const shared = {
    sources: [
      {path: "README.md", content: readmeSource.content},
      {path: "docs/agent-program.md", content: programSource.content},
      {path: "docs/generic-agent-design.md", content: designSource.content}
    ],
    purpose: "以中文制作自包含HTML页面，忠实介绍本项目通用Agent设计；不得修改源文档；不声称未验证的事实或浏览器视觉验收。",
    output_listing: rootListing
  };
  const requestedPages = [
    {path: "agent-design-zh/01-overview.html", title: "通用 Agent：从目标到交付", focus: "项目总览、核心理念与工作流全景"},
    {path: "agent-design-zh/02-program.html", title: "把任务写成可执行流程", focus: "task-program、main/helper、确定性计算与显式AI任务"},
    {path: "agent-design-zh/03-tools.html", title: "工具、权限与真实证据", focus: "工具调用、权限边界、读写、证据与验收"},
    {path: "agent-design-zh/04-agents.html", title: "并发 Agent 与工作区整合", focus: "fork/join、分工、Git worktree、结果整合"},
    {path: "agent-design-zh/05-quality.html", title: "验证、修复与可靠交付", focus: "检查、修复复查、停止条件、交付和限制"}
  ];
  const pathPlan = planPaths(shared, requestedPages);
  assert(pathPlan.pages.length === 5, "路径规划必须保留五页");
  const child = fork(makePages, shared, pathPlan.pages.slice(0, 3));
  const mainPages = makePages(shared, pathPlan.pages.slice(3));
  const childOutcomes = join(child);
  integrate(child);
  const fileList = mainPages.files;
  for (const childGroup of childOutcomes) {
    for (const childFile of childGroup.files) fileList.push(childFile);
  }
  assert(fileList.length === 5, "页面制作数量不是5");
  const realFiles = [];
  for (const writtenFile of fileList) {
    const readBack = io.read({path: writtenFile.path});
    realFiles.push({path: writtenFile.path, title: writtenFile.title, focus: writtenFile.focus, content: readBack.content});
  }
  const review = reviewAll(shared, realFiles);
  assert(review.passed, "HTML内容验收未通过，停止交付");
  return {files: fileList, review: review, visual_review: "未进行浏览器渲染；不声称视觉验收通过。", source_documents_modified: false};
}

/** @virtual */
function planPaths(shared, pages) {
  // 检视 shared.output_listing 中的真实目录条目，选一个不覆盖现有文件的专用输出目录；保留输入五页各自的文件名、标题、主题和顺序，必要时对目录名使用数字后缀以避开冲突。
  // 返回 {pages:[与输入对应、无冲突的path/title/focus对象]}。不得读取或写入文件。不得重新读取源文档。
}

function makePages(shared, pages) {
  const writtenPages = [];
  for (const pageSpec of pages) {
    const draftPage = designPage(shared, pageSpec);
    assert(draftPage.content, "页面生成未返回HTML正文");
    const writeReceipt = io.write({path: pageSpec.path, content: draftPage.content});
    writtenPages.push({path: pageSpec.path, title: pageSpec.title, focus: pageSpec.focus, receipt: writeReceipt});
  }
  return {files: writtenPages};
}

/** @virtual */
function designPage(shared, page) {
  // 依据 shared.sources 的真实正文，直接制作本页完整的中文HTML；页面重点明确且与其他页面区分，准确介绍项目概念与工作流。页面自包含、语义清晰、中文易读、响应式、内嵌CSS、不依赖外部网络资源，含标题、正文层级、可读视觉层次、摘要/导航和来源说明。不得修改源文档或声称执行了浏览器测试。
  // 返回 {content:string}，是完整 <!doctype html> 文档，含实质内容而非计划或占位。
}

function integrate(handle) {
  assert(handle.workspace, "缺少子Agent工作区信息");
  if (handle.workspace.integration_required) {
    const diffInspection = host.inspect_agent_changes({agent_id: handle.agent_id});
    assert(diffInspection.head, "未能取得子Agent分支head，不能整合");
    return host.merge_agent_result({agent_id: handle.agent_id, expected_head: diffInspection.head});
  }
  return null;
}

/** @virtual */
function reviewAll(shared, actual) {
  // 唯一的验收虚函数。于自身上下文中逐步检查五个真实读回的HTML文件：逐页核查完整HTML骨架、中文实质内容、主题覆盖与相互差异、对三份源材料的事实忠实性、无明显占位/损坏/外链依赖及遵守用户限制。若发现授权范围内可修复的问题，直接修复相应HTML；随后必须通过io.read重新读实际文件并独立复查。所有文件检查/修复/复查均在本函数自己的调用上下文逐步完成。
  // 返回 {passed:boolean, issues:[string], evidence:[string], checked_paths:[string], repaired_paths:[string], browser_visual_review:"not_performed"}。以真实读回证据为准；不能修复或复查失败则passed=false并说明具体问题；不得声称浏览器视觉验收通过。
}
