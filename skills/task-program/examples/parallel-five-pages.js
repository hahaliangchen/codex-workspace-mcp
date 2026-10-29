// 五页并发设计、统一写入，再并发检查实际读回的内容。
import * as io from "host:io";
const request = input;

assert(request.pages.length === 5, "此教学示例接收五页清单");
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
const reviews = await Promise.all(request.pages.map(async page => {
  const actual = io.read({path: page.path});
  return await inspectPage(shared, page, actual.content);
}));
for (const review of reviews) {
  assert(review.passed, "验收失败：" + review.path);
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
  // 返回 {passed:boolean, path:string, issues:[string], evidence:[string]}。
  // 不虚构未执行的浏览器或视觉检查。
}
