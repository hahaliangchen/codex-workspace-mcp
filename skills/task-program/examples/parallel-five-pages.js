// 教学案例：用户已授权两个执行 Agent 并行制作五个独立 HTML。
import * as io from "host:io";
import * as host from "host:host";
const request = input;

assert(request.pages.length === 5, "此教学示例接收五页清单");
const source = io.read({path: request.source});
const shared = {
  source_path: request.source,
  source_content: source.content,
  requirements: request.requirements
};

// 先启动两路再等待；传相同公共材料与不同的独占页面清单。
const a = fork(makePages, shared, request.pages.slice(0, 3));
const b = fork(makePages, shared, request.pages.slice(3));
const outcomes = join(a, b);

// Git 分支的文件在独立工作树中；合并后才能从主工作区读回。
integrate(a);
integrate(b);

// join 只收齐结果。主 Agent 仍要逐页读回并验收。
const files = [];
const reviews = [];
for (const outcome of outcomes) {
  for (const file of outcome.files) {
    const actual = io.read({path: file.path});
    const review = inspectPage(shared, file, actual.content);
    assert(review.passed, "验收失败：" + file.path);
    files.push(file);
    reviews.push(review);
  }
}
assert(files.length === request.pages.length, "交付数量不符");
// 已检查所有页面；流程自然结束。

function makePages(shared, pages) {
  // 一个分支是一个 Agent；其中逐页串行执行，不再 fork。
  const files = [];
  for (const page of pages) {
    const draft = designPage(shared, page);
    const receipt = io.write({path: page.path, content: draft.content});
    files.push({path: page.path, title: page.title, focus: page.focus, receipt});
  }
  return {files};
}

/** @virtual */
function designPage(shared, page) {
  // shared 含真实源材料与公共设计要求，page 含本页标题、重点和输出路径。
  // 直接完成本页设计：内容忠实于源材料，主题不同于其他页，HTML/CSS 自包含。
  // 返回 {content:string}；不重新查找已传入的材料，不把相同职责改名转交。
}

/** @virtual */
function inspectPage(shared, page, content) {
  // 对照 shared.source_content、requirements 及 page 的标题/重点检查真实读回正文。
  // 检查主题覆盖、事实、完整 HTML、外部依赖和明显缺项。
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
  // 不把成功写入或数量正确视为内容验收，不虚构浏览器视觉检查。
}

function integrate(handle) {
  assert(handle.workspace, "缺少分支工作区信息");
  if (handle.workspace.integration_required) {
    const changes = host.inspect_agent_changes({agent_id: handle.agent_id});
    return host.merge_agent_result({agent_id: handle.agent_id, expected_head: changes.head});
  }
  return null;
}
