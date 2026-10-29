// 五页并发设计、统一写入，再并发检查实际读回的内容；未通过的页修正一轮后复检。
import * as io from "host:io";
const request = {
  source: "docs/agent-design-guide.md",
  requirements: "制作五个内容不同、风格协调的独立中文 HTML。忠实于源材料的当前设计，准确区分已废弃的机制和现行设计；内嵌 CSS、无外部资源依赖，兼顾桌面与手机阅读。完成后对实际读回的各页进行验收。",
  pages: [
    {path: "agent-01-overview.html", title: "设计出发点", focus: "朴素循环为何最稳、上下文有限带来的问题，以及用函数作用域代替压缩"},
    {path: "agent-02-program.html", title: "流程与虚函数", focus: "流程代码就是 JavaScript、虚函数是唯一的 AI 入口、第一层与叶子的两层边界"},
    {path: "agent-03-scope.html", title: "作用域与上下文", focus: "参数显式传入、返回值是跨作用域的唯一通道、上下文超限时抛错而不压缩"},
    {path: "agent-04-libraries.html", title: "工具库与并发", focus: "工具即库并通过 import 引入、叶子的工具范围与运行时查找、Promise 取代子 Agent"},
    {path: "agent-05-acceptance.html", title: "错误与交付", focus: "错误统一走异常、副作用边界、已废弃机制，以及真实证据与完成条件"}
  ]
};

const source = io.read({path: request.source});
const shared = {
  source_path: request.source,
  source_content: source.content,
  requirements: request.requirements
};

const designs = await Promise.all(request.pages.map(page => designPage(shared, page)));
for (let i = 0; i < request.pages.length; i++) {
  io.write({path: request.pages[i].path, content: designs[i].content});
}
const review = async page => {
  const actual = io.read({path: page.path});
  return await inspectPage(shared, page, actual.content);
};
const reviews = await Promise.all(request.pages.map(review));
// 未通过的页按验收意见修正一轮，写入后重新读回复检。
const revised = await Promise.all(request.pages.map(async (page, i) => {
  if (reviews[i].passed) return reviews[i];
  const current = io.read({path: page.path});
  const fixed = await revisePage(shared, page, current.content, reviews[i].issues);
  io.write({path: page.path, content: fixed.content});
  return await review(page);
}));
for (let i = 0; i < request.pages.length; i++) {
  assert(revised[i].passed, "修正后验收仍未通过：" + request.pages[i].path);
}

/** @virtual */
async function designPage(shared, page) {
  // shared 含实际源材料与统一要求，page 含标题、重点和输出路径。
  // 设计本页，逐项覆盖 page.focus 中的要求并依据实际源材料说明；
  // 区分现行机制和历史记录，不擅自推断源材料的版本或日期。
  // 主题不同于其他页，HTML/CSS 自包含。
  // 返回 {content:string}；不写文件，不重复读取已传入的材料。
}

/** @virtual */
async function inspectPage(shared, page, content) {
  // 对照实际源材料和本页要求检查真实读回正文。
  // 只验收本页；五页之间是否不同、是否协调不在本函数职责内，不因此判为不通过。
  // 只有与源材料不符、遗漏本页要求等实质问题判为不通过；措辞上的建议可写进 issues，但不影响 passed。
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
  // 不虚构未执行的浏览器或视觉检查。
}

/** @virtual */
async function revisePage(shared, page, content, issues) {
  // content 是本页已保存的真实正文，issues 是验收指出的问题。
  // 依据 shared 中的源材料逐条修正这些问题，保留本页其余正确内容和风格。
  // 返回 {content:string}；不写文件。
}
