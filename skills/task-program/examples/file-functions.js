import * as io from "host:io";
// 普通函数直接执行文件操作；内容写在代码里，不请求 AI 生成。
const files = [
  {path: "notes/a.txt", content: "第一份文件"},
  {path: "notes/b.txt", content: "第二份文件"}
];

const results = [];
for (const item of files) {
  const written = writeFile(item.path, item.content);
  const actual = readFile(item.path);
  results.push({path: item.path, written: written, actual: actual});
}

function writeFile(path, content) {
  return io.write({path: path, content: content});
}

function readFile(path) {
  return io.read({path: path});
}
