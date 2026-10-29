import * as io from "host:io/promises";
// 文件读取并发；AI 在一个上下文中按顺序判断。
const request = {
  paths: ["a.html", "b.html", "c.html", "d.html", "e.html"],
  requirements: "检查正文"
};

const actuals = await Promise.all(request.paths.map(path => io.read({path: path})));
const pages = pairPages(request.paths, actuals);
const review = inspectPages(pages, request.requirements);

function pairPages(paths, actuals) {
  const pages = [];
  for (let i = 0; i < paths.length; i++) {
    pages.push({path: paths[i], initial: actuals[i]});
  }
  return pages;
}

/** @virtual */
function inspectPages(pages, requirements) {
  // 在这个 AI 上下文中按顺序判断每页 initial 中的已读正文，不因进入函数重复读取。
  // 若 initial 表示内容被分页或截断，按回执只读取尚未取得的部分。
  // 发现职责与授权范围内的问题时可以修复；修改后读回实际文件并复查。
  // 返回实际检查、处理和复查结果；不做浏览器或视觉验收。
}
