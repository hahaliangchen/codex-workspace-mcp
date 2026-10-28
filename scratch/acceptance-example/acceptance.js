// 只读源码验收示例。请求由调用方提供 pages 和 requirements。
function main(request) {
  const pages = [];
  const issues = [];
  const limitations = [];
  for (const path of request.pages) {
    const actual = io.read({path: path, max_chars: 60000});
    if (actual.complete !== true) {
      issues.push({path: path, problem: "文件读取不完整，无法验收"});
    } else {
      const links = checkLinks(path, actual.content);
      const content = reviewContent(path, actual.content, request.requirements);
      if (content.passed !== true) {
        issues.push({path: path, problem: "内容审查未通过或返回无效结果", review: content});
      }
      for (const problem of links.issues) issues.push(problem);
      for (const limit of links.limitations) limitations.push(limit);
      if (content.limitations === null || content.limitations.length === null) {
        issues.push({path: path, problem: "AI 未返回限制清单"});
      } else {
        for (const contentLimit of content.limitations) limitations.push({path: path, detail: contentLimit});
      }
      pages.push({path: path, links: links, contentReview: content});
    }
  }
  return {
    sourceChecksPassed: issues.length === 0 && limitations.length === 0,
    pages: pages,
    issues: issues,
    limitations: limitations,
    visualReview: "未执行浏览器渲染或视觉验收"
  };
}

// 检查源码中带引号的 a[href]。未覆盖的语法不能当成通过。
function checkLinks(pagePath, html) {
  const issues = [];
  const limitations = [];
  const checked = [];
  const tags = html.match(/<a\s[^>]*>/gi) ?? [];
  for (const tag of tags) {
    const attr = tag.match(/\bhref\s*=\s*["']([^"']*)["']/i);
    if (attr === null) {
      limitations.push({path: pagePath, detail: "链接标签未能解析，需进一步核验", tag: tag});
    } else {
      const href = attr[1];
      const result = checkLinkTarget(pagePath, href);
      checked.push({href: href, result: result});
      if (result.status === "missing") issues.push({path: pagePath, href: href, problem: "目标目录列表中未发现链接目标"});
      if (result.status === "unknown") limitations.push({path: pagePath, href: href, detail: result.reason});
    }
  }
  limitations.push({path: pagePath, detail: "正则仅抽查静态 a[href]；HTML 解析、其他资源、脚本生成的链接及片段锚点未完整核验"});
  return {checked: checked, issues: issues, limitations: limitations};
}

function checkLinkTarget(pagePath, href) {
  const lower = href.toLowerCase();
  if (lower.startsWith("http:") || lower.startsWith("https:") || lower.startsWith("mailto:") || lower.startsWith("tel:")) {
    return {status: "unknown", reason: "外部链接未检查"};
  }
  if (href.startsWith("//") || href.includes("%") || href.includes("&") || href.startsWith("/")) {
    return {status: "unknown", reason: "链接需要 URL 解码或其他解析"};
  }
  const pieces = href.split("#");
  if (pieces.length > 1) return {status: "unknown", reason: "未核验页面片段锚点"};
  const beforeQuery = pieces[0].split("?");
  if (beforeQuery.length > 1 || beforeQuery[0].length === 0) return {status: "unknown", reason: "查询参数或空目标未核验"};
  const slash = pagePath.lastIndexOf("/");
  const parent = slash < 0 ? "." : pagePath.slice(0, slash);
  const target = normalizeRelative(parent + "/" + beforeQuery[0]);
  if (target === null) return {status: "unknown", reason: "目标路径超出工作区"};
  const targetSlash = target.lastIndexOf("/");
  const directory = targetSlash < 0 ? "." : target.slice(0, targetSlash);
  try {
    const listing = io.list({path: directory, recursive: false});
    for (const entry of listing.entries) {
      if (entry.path === target && entry.kind === "file") return {status: "exists", path: target};
      if (entry.path === target) return {status: "unknown", reason: "目标不是普通文件，需进一步核验"};
    }
    return {status: "missing", path: target};
  } catch (error) {
    return {status: "unknown", reason: "无法读取目标目录", error: error.message};
  }
}

function normalizeRelative(path) {
  const parts = path.split("/");
  let kept = [];
  for (const part of parts) {
    if (part === "" || part === ".") {
      // 忽略空段和当前目录。
    } else if (part === "..") {
      if (kept.length === 0) return null;
      kept = kept.slice(0, kept.length - 1);
    } else {
      kept.push(part);
    }
  }
  return kept.join("/");
}

/** @virtual */
function reviewContent(path, content, requirements) {
  // 对照 requirements，基于实际读回的 content 判断内容是否准确、完整且符合职责。
  // 不重新读取已传入的正文；必要时可读取未传入的依据。不得把结构或关键词当作内容通过。
  // 返回 {passed:boolean, issues:[string], evidence:[string], limitations:[string]}。
  // 不能确认时在 limitations 中说明，不声称做过视觉验收。
}
