import * as io from "host:io";
// 一个普通函数按顺序读取页面，再交给同一个 AI 上下文逐页判断。
// 这是串行基线，不要求视觉检查。
const request = {
  paths: ["a.html", "b.html", "c.html", "d.html", "e.html"],
  requirements: "检查正文"
};

const review = reviewPages(request.paths, request.requirements);

function reviewPages(paths, requirements) {
  const pages = [];
  for (const path of paths) {
    const actual = io.read({path: path});
    pages.push({path: path, initial: actual});
  }
  return inspectPages(pages, requirements);
}

/** @virtual */
function inspectPages(pages, requirements) {
  // 在这个 AI 上下文中按 pages 顺序检查 initial 的已读正文，不因进入函数重复读取。
  // 若 initial 表示内容被分页或截断，按回执只读取尚未取得的部分。
  // 发现职责和授权范围内的问题时可以修复；修改后读回实际文件并复查。
  // 返回逐页实际发现、处理和复查结果；不做浏览器或视觉验收。
}
