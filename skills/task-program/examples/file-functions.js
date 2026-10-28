// 普通函数直接执行文件操作；内容由调用方提供，不请求 AI 生成。
function main(request) {
  const results = [];
  for (const item of request.files) {
    const written = writeFile(item.path, item.content);
    const actual = readFile(item.path);
    results.push({path: item.path, written: written, actual: actual});
  }
  return results;
}

function writeFile(path, content) {
  return io.write({path: path, content: content});
}

function readFile(path) {
  return io.read({path: path});
}
