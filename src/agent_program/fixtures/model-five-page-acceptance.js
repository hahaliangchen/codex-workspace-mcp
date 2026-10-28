import { read as readFile, write as writeFile } from "host:io";

const PAGE_FILES = [
  "agent-design-html/01-overview.html",
  "agent-design-html/02-program-flow.html",
  "agent-design-html/03-task-context.html",
  "agent-design-html/04-parallel-agents.html",
  "agent-design-html/05-verification.html"
];

const REFERENCE_FILES = [
  "docs/README.md",
  "docs/agent-program.md",
  "docs/generic-agent-design.md"
];

const MAX_FILE_BYTES = 250000;
const MAX_REPAIR_ROUNDS = 10;

// @virtual
async function deriveAcceptanceModel(args) {
  // 输入：三份参考材料的实际文本，以及五个目标文件路径。
  // 输出：
  // {
  //   pageRequirements: [{
  //     file, purpose,
  //     requirements: [{ id, criterion, evidence: [{ file, quote }] }]
  //   }],
  //   crossPageRules: [{ id, rule, evidence: [{ file, quote }] }],
  //   terminology: [{ preferred, variants: [], evidence: [{ file, quote }] }]
  // }
  // 只提取能由参考材料支持的验收标准；不得臆造文档未支持的事实。
}

// @virtual
async function reviewPageContent(args) {
  // 输入：一个页面的实际 HTML、该页验收标准、参考材料及确定性检查发现。
  // 输出：问题数组；每项为
  // { id, finding, evidence, severity, repairable, target_file, criterion_id }。
  // evidence 必须指出页面中的实际文本或明确说明缺少的要求；
  // target_file 必须是当前页面路径。
}

// @virtual
async function reviewCrossPageConsistency(args) {
  // 输入：五页的实际 HTML、验收模型及三份参考材料。
  // 输出：跨页一致性问题数组；每项为
  // { id, finding, evidence, severity, repairable, target_file, criterion_id }。
  // 可修复问题必须指定五个目标页面之一作为 target_file；
  // 不可修复问题可将 target_file 设为 null。
}

// @virtual
async function proposeMinimalEdits(args) {
  // 输入：当前五页文本、目标页实际 HTML、验收模型及针对该页的问题。
  // 输出：{ edits: [{ before, after, issue_ids: [] }] }。
  // 仅提出修复所列问题的最小文本替换；before 必须是目标页中唯一出现的原文。
  // 不得修改参考材料或五页以外的文件；无法安全修复时返回空 edits。
}

function getReadText(result, path) {
  if (typeof result === "string") return result;
  if (result && typeof result.content === "string") return result.content;
  if (result && typeof result.text === "string") return result.text;
  if (result && result.data && typeof result.data.content === "string") {
    return result.data.content;
  }
  throw new Error("无法取得文件文本：" + path);
}

async function readText(path) {
  const result = await readFile({ path: path, max_bytes: MAX_FILE_BYTES });
  const text = getReadText(result, path);
  if (text.trim().length === 0) throw new Error("文件为空：" + path);
  return text;
}

async function readFileSet(paths) {
  const result = {};
  for (let i = 0; i < paths.length; i++) {
    result[paths[i]] = await readText(paths[i]);
  }
  return result;
}

function nonEmptyString(value) {
  return typeof value === "string" && value.trim().length > 0;
}

function validateEvidence(evidence, references, label) {
  if (!Array.isArray(evidence) || evidence.length === 0) {
    throw new Error(label + " 缺少参考材料引文");
  }

  for (let i = 0; i < evidence.length; i++) {
    const item = evidence[i];
    if (!item || !nonEmptyString(item.file) || !nonEmptyString(item.quote)) {
      throw new Error(label + " 的引文格式无效");
    }
    if (REFERENCE_FILES.indexOf(item.file) < 0) {
      throw new Error(label + " 引用了范围外文件：" + item.file);
    }
    if (references[item.file].indexOf(item.quote) < 0) {
      throw new Error(label + " 的引文未能在实际参考材料中找到：" + item.file);
    }
  }
}

function validateAcceptanceModel(model, references) {
  if (!model || !Array.isArray(model.pageRequirements)) {
    throw new Error("AI 未返回有效的逐页验收标准");
  }
  if (!Array.isArray(model.crossPageRules) || !Array.isArray(model.terminology)) {
    throw new Error("AI 返回的跨页规则或术语表格式无效");
  }
  if (model.pageRequirements.length !== PAGE_FILES.length) {
    throw new Error("逐页验收标准未覆盖恰好五个目标页面");
  }

  const seenFiles = [];
  for (let i = 0; i < model.pageRequirements.length; i++) {
    const page = model.pageRequirements[i];
    if (!page || PAGE_FILES.indexOf(page.file) < 0 || seenFiles.indexOf(page.file) >= 0) {
      throw new Error("逐页验收标准包含缺失、重复或范围外页面");
    }
    seenFiles.push(page.file);

    if (!nonEmptyString(page.purpose) ||
        !Array.isArray(page.requirements) ||
        page.requirements.length === 0) {
      throw new Error("页面验收标准不完整：" + page.file);
    }

    for (let j = 0; j < page.requirements.length; j++) {
      const requirement = page.requirements[j];
      if (!requirement || !nonEmptyString(requirement.id) ||
          !nonEmptyString(requirement.criterion)) {
        throw new Error("页面验收条目格式无效：" + page.file);
      }
      validateEvidence(requirement.evidence, references, requirement.id);
    }
  }

  for (let i = 0; i < model.crossPageRules.length; i++) {
    const rule = model.crossPageRules[i];
    if (!rule || !nonEmptyString(rule.id) || !nonEmptyString(rule.rule)) {
      throw new Error("跨页规则格式无效");
    }
    validateEvidence(rule.evidence, references, rule.id);
  }

  for (let i = 0; i < model.terminology.length; i++) {
    const term = model.terminology[i];
    if (!term || !nonEmptyString(term.preferred) || !Array.isArray(term.variants)) {
      throw new Error("术语表格式无效");
    }
    validateEvidence(term.evidence, references, term.preferred);
  }
}

function stripHtmlComments(html) {
  return html.replace(/<!--[\s\S]*?-->/g, "");
}

function extractAttribute(tag, name) {
  const expression = new RegExp(
    "(?:^|\\s)" + name + "\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
    "i"
  );
  const match = tag.match(expression);
  if (!match) return null;
  if (match[1] !== undefined) return match[1];
  if (match[2] !== undefined) return match[2];
  return match[3];
}

function extractIds(html) {
  const clean = stripHtmlComments(html);
  const tags = clean.match(/<[A-Za-z][^>]*>/g) || [];
  const ids = [];

  for (let i = 0; i < tags.length; i++) {
    const id = extractAttribute(tags[i], "id");
    if (id !== null && id.length > 0) ids.push(id);
  }
  return ids;
}

function normalizeWorkspacePath(fromFile, linkPath) {
  let parts;

  if (linkPath.charAt(0) === "/") {
    parts = [];
    linkPath = linkPath.slice(1);
  } else {
    parts = fromFile.split("/");
    parts.pop();
  }

  const pathParts = linkPath.split("/");
  for (let i = 0; i < pathParts.length; i++) {
    const part = pathParts[i];
    if (part === "" || part === ".") continue;
    if (part === "..") {
      if (parts.length === 0) return null;
      parts.pop();
    } else {
      parts.push(part);
    }
  }
  return parts.join("/");
}

function addFinding(findings, file, text, evidence) {
  findings.push({
    id: "deterministic-" + String(findings.length + 1),
    file: file,
    target_file: file,
    finding: text,
    evidence: evidence,
    severity: "error",
    repairable: true,
    origin: "deterministic"
  });
}

function inspectCorpus(pages, references, model) {
  const findings = [];
  const unverifiedLinks = [];
  const idsByPage = {};
  const knownTexts = {};

  for (let i = 0; i < PAGE_FILES.length; i++) {
    const file = PAGE_FILES[i];
    knownTexts[file] = pages[file];
    idsByPage[file] = extractIds(pages[file]);
  }
  for (let i = 0; i < REFERENCE_FILES.length; i++) {
    knownTexts[REFERENCE_FILES[i]] = references[REFERENCE_FILES[i]];
  }

  for (let i = 0; i < PAGE_FILES.length; i++) {
    const file = PAGE_FILES[i];
    const html = pages[file];
    const clean = stripHtmlComments(html);

    if (!/<!doctype\s+html\s*>/i.test(clean)) {
      addFinding(findings, file, "缺少 HTML doctype", "未找到 <!doctype html>");
    }
    if (!/<html\b/i.test(clean) || !/<\/html\s*>/i.test(clean)) {
      addFinding(findings, file, "HTML 根元素不完整", "未同时找到 html 开始与结束标签");
    }
    if (!/<head\b/i.test(clean) || !/<\/head\s*>/i.test(clean)) {
      addFinding(findings, file, "head 结构不完整", "未同时找到 head 开始与结束标签");
    }
    if (!/<body\b/i.test(clean) || !/<\/body\s*>/i.test(clean)) {
      addFinding(findings, file, "body 结构不完整", "未同时找到 body 开始与结束标签");
    }
    if (!/<title\b[^>]*>[\s\S]*?<\/title\s*>/i.test(clean)) {
      addFinding(findings, file, "缺少非空 title 元素", "未找到完整 title 元素");
    }
    if (!/<h1\b/i.test(clean)) {
      addFinding(findings, file, "缺少页面主标题 h1", "未找到 h1 元素");
    }

    const ids = idsByPage[file];
    const duplicateIds = [];
    const tags = clean.match(/<[A-Za-z][^>]*>/g) || [];
    for (let j = 0; j < tags.length; j++) {
      const id = extractAttribute(tags[j], "id");
      if (id !== null && id.length > 0) {
        if (duplicateIds.indexOf(id) >= 0) {
          addFinding(findings, file, "页面包含重复 id：" + id, "id=\"" + id + "\" 出现多次");
        } else if (ids.indexOf(id) !== ids.lastIndexOf(id)) {
          duplicateIds.push(id);
        }
      }
    }

    const anchors = clean.match(/<a\b[^>]*>/gi) || [];
    for (let j = 0; j < anchors.length; j++) {
      const href = extractAttribute(anchors[j], "href");
      if (href === null) {
        addFinding(findings, file, "a 元素缺少 href", anchors[j]);
        continue;
      }

      const value = href.trim();
      if (/^javascript:/i.test(value)) {
        addFinding(findings, file, "链接使用 javascript: 协议", "href=\"" + value + "\"");
        continue;
      }
      if (value === "") continue;

      if (value.indexOf("//") === 0 || /^https?:\/\//i.test(value) ||
          /^(mailto|tel):/i.test(value)) {
        unverifiedLinks.push({
          file: file,
          href: value,
          reason: "外部链接仅检查格式，未请求目标地址"
        });
        continue;
      }
      if (/^[a-z][a-z0-9+.-]*:/i.test(value)) {
        unverifiedLinks.push({
          file: file,
          href: value,
          reason: "非标准或非 HTTP 链接协议未验证"
        });
        continue;
      }

      const hashAt = value.indexOf("#");
      let fragment = "";
      let pathAndQuery = value;
      if (hashAt >= 0) {
        fragment = value.slice(hashAt + 1);
        pathAndQuery = value.slice(0, hashAt);
      }

      const queryAt = pathAndQuery.indexOf("?");
      const linkPath = queryAt >= 0 ? pathAndQuery.slice(0, queryAt) : pathAndQuery;
      const target = linkPath === "" ? file : normalizeWorkspacePath(file, linkPath);

      if (target === null) {
        addFinding(findings, file, "链接路径越出工作区范围", "href=\"" + value + "\"");
        continue;
      }

      if (target in knownTexts) {
        if (fragment !== "") {
          if (target in idsByPage) {
            if (fragment.indexOf("%") >= 0) {
              unverifiedLinks.push({
                file: file,
                href: value,
                reason: "百分号编码的片段未解码验证"
              });
            } else if (idsByPage[target].indexOf(fragment) < 0) {
              addFinding(findings, file, "页面片段链接不存在", "href=\"" + value + "\"");
            }
          } else {
            unverifiedLinks.push({
              file: file,
              href: value,
              reason: "Markdown 标题片段未验证"
            });
          }
        }
      } else {
        unverifiedLinks.push({
          file: file,
          href: value,
          reason: "链接目标不在本次读取的五页及三份参考材料内"
        });
      }
    }
  }

  return {
    findings: findings,
    unverifiedLinks: unverifiedLinks,
    checkedPages: PAGE_FILES.slice(),
    checkedReferences: REFERENCE_FILES.slice()
  };
}

function validateReviewIssues(result, stage, requiredTarget) {
  if (!Array.isArray(result)) {
    throw new Error(stage + " 未返回问题数组");
  }

  const normalized = [];
  for (let i = 0; i < result.length; i++) {
    const issue = result[i];
    if (!issue || !nonEmptyString(issue.finding) ||
        !nonEmptyString(issue.evidence) ||
        typeof issue.repairable !== "boolean") {
      throw new Error(stage + " 返回了格式无效的问题");
    }

    const target = issue.target_file;
    if (issue.repairable && PAGE_FILES.indexOf(target) < 0) {
      throw new Error(stage + " 将可修复问题指向范围外文件");
    }
    if (requiredTarget !== null && target !== requiredTarget) {
      throw new Error(stage + " 将逐页问题指向了错误页面");
    }
    if (!issue.repairable && target !== null && PAGE_FILES.indexOf(target) < 0) {
      throw new Error(stage + " 将不可修复问题指向范围外文件");
    }

    normalized.push({
      id: stage + "-" + String(i + 1) + "-" +
        (nonEmptyString(issue.id) ? issue.id : "issue"),
      file: target,
      target_file: target,
      finding: issue.finding,
      evidence: issue.evidence,
      severity: nonEmptyString(issue.severity) ? issue.severity : "review",
      repairable: issue.repairable,
      criterion_id: issue.criterion_id || null,
      origin: stage
    });
  }
  return normalized;
}

async function evaluateCorpus(pages, references, model) {
  const deterministic = inspectCorpus(pages, references, model);
  let findings = deterministic.findings.slice();

  for (let i = 0; i < PAGE_FILES.length; i++) {
    const file = PAGE_FILES[i];
    let pageCriteria = null;
    for (let j = 0; j < model.pageRequirements.length; j++) {
      if (model.pageRequirements[j].file === file) {
        pageCriteria = model.pageRequirements[j];
        break;
      }
    }

    const review = await reviewPageContent({
      file: file,
      html: pages[file],
      criteria: pageCriteria,
      referenceDocs: references,
      deterministicFindings: deterministic.findings.filter(function (item) {
        return item.file === file;
      })
    });

    const pageIssues = validateReviewIssues(review, "page-review:" + file, file);
    findings = findings.concat(pageIssues);
  }

  const crossReview = await reviewCrossPageConsistency({
    pages: PAGE_FILES.map(function (file) {
      return { file: file, html: pages[file] };
    }),
    criteria: model,
    referenceDocs: references
  });

  const crossIssues = validateReviewIssues(crossReview, "cross-page-review", null);
  findings = findings.concat(crossIssues);

  return {
    findings: findings,
    unverifiedLinks: deterministic.unverifiedLinks
  };
}

function countOccurrences(text, needle) {
  if (needle.length === 0) return 0;
  let count = 0;
  let from = 0;
  while (true) {
    const at = text.indexOf(needle, from);
    if (at < 0) break;
    count++;
    from = at + needle.length;
  }
  return count;
}

function applyEdits(currentText, edits, issues) {
  if (!Array.isArray(edits) || edits.length === 0) {
    throw new Error("修复方案未包含可应用的文本替换");
  }

  let result = currentText;
  const allowedIds = issues.map(function (issue) { return issue.id; });

  for (let i = 0; i < edits.length; i++) {
    const edit = edits[i];
    if (!edit || typeof edit.before !== "string" ||
        typeof edit.after !== "string" ||
        edit.before.length === 0 ||
        edit.before === edit.after ||
        !Array.isArray(edit.issue_ids) ||
        edit.issue_ids.length === 0) {
      throw new Error("修复方案包含无效或非最小替换");
    }

    for (let j = 0; j < edit.issue_ids.length; j++) {
      if (allowedIds.indexOf(edit.issue_ids[j]) < 0) {
        throw new Error("修复方案引用了未提交给修复器的问题");
      }
    }

    if (countOccurrences(result, edit.before) !== 1) {
      throw new Error("原文未唯一匹配，拒绝应用修复");
    }
    const at = result.indexOf(edit.before);
    result = result.slice(0, at) + edit.after + result.slice(at + edit.before.length);
  }

  if (result === currentText) throw new Error("修复没有改变目标文件");
  return result;
}

async function writeAndVerify(file, oldText, newText) {
  const beforeWrite = await readText(file);
  if (beforeWrite !== oldText) {
    return { applied: false, reason: "写入前文件已变化；未覆盖新内容" };
  }

  let writeError = null;
  try {
    const result = await writeFile({ path: file, content: newText });
    if (result && (result.ok === false || result.success === false)) {
      writeError = "写入工具返回失败状态";
    }
  } catch (error) {
    writeError = String(error);
  }

  // 写入结果即使不确定也只重读，不重放写入操作。
  let observed;
  try {
    observed = await readText(file);
  } catch (error) {
    return {
      applied: false,
      reason: "写入后无法确认文件状态；未重试写入：" + String(error)
    };
  }

  if (observed !== newText) {
    return {
      applied: false,
      reason: "写入后内容与预期不符；未重试写入" +
        (writeError ? "；工具信息：" + writeError : "")
    };
  }

  return {
    applied: true,
    text: observed,
    note: writeError ? "工具报告异常，但重读确认目标内容已写入" : null
  };
}

async function repairOneTarget(file, issues, pages, references, model) {
  const plan = await proposeMinimalEdits({
    targetFile: file,
    currentHtml: pages[file],
    allPages: PAGE_FILES.map(function (pageFile) {
      return { file: pageFile, html: pages[pageFile] };
    }),
    criteria: model,
    issues: issues,
    referenceDocs: references
  });

  if (!plan || !Array.isArray(plan.edits)) {
    throw new Error("修复器未返回有效 edits");
  }

  const newText = applyEdits(pages[file], plan.edits, issues);
  const writeResult = await writeAndVerify(file, pages[file], newText);
  return writeResult;
}

async function runAcceptance() {
  try {
    const references = await readFileSet(REFERENCE_FILES);
    let pages = await readFileSet(PAGE_FILES);

    const model = await deriveAcceptanceModel({
      targetFiles: PAGE_FILES,
      referenceDocs: references
    });
    validateAcceptanceModel(model, references);

    const repairLog = [];
    const blockedTargets = [];
    let anyWrite = false;

    for (let round = 0; round < MAX_REPAIR_ROUNDS; round++) {
      const scan = await evaluateCorpus(pages, references, model);
      if (scan.findings.length === 0) break;

      let target = null;
      for (let i = 0; i < scan.findings.length; i++) {
        const issue = scan.findings[i];
        if (issue.repairable &&
            PAGE_FILES.indexOf(issue.target_file) >= 0 &&
            blockedTargets.indexOf(issue.target_file) < 0) {
          target = issue.target_file;
          break;
        }
      }
      if (target === null) break;

      const targetIssues = scan.findings.filter(function (issue) {
        return issue.repairable && issue.target_file === target;
      });

      try {
        const result = await repairOneTarget(
          target, targetIssues, pages, references, model
        );

        if (!result.applied) {
          blockedTargets.push(target);
          repairLog.push({
            file: target,
            status: "not-applied",
            reason: result.reason
          });
          continue;
        }

        anyWrite = true;
        repairLog.push({
          file: target,
          status: "written-and-reread",
          issueIds: targetIssues.map(function (issue) { return issue.id; }),
          note: result.note
        });

        // 修复后重读全部目标页；下一轮重新执行所有检查。
        pages = await readFileSet(PAGE_FILES);
        if (pages[target] !== result.text) {
          blockedTargets.push(target);
          repairLog.push({
            file: target,
            status: "changed-after-write-verification"
          });
        }
      } catch (error) {
        blockedTargets.push(target);
        repairLog.push({
          file: target,
          status: "repair-failed",
          reason: String(error)
        });
      }
    }

    // 最终复查使用重读后的现状，不传入之前的 AI 审查结论或修复方案。
    if (anyWrite) pages = await readFileSet(PAGE_FILES);
    const finalScan = await evaluateCorpus(pages, references, model);

    let status = "pass";
    if (finalScan.findings.length > 0) {
      status = "fail";
    } else if (finalScan.unverifiedLinks.length > 0) {
      status = "partial";
    }

    return {
      status: status,
      checkedFiles: REFERENCE_FILES.concat(PAGE_FILES),
      repairLog: repairLog,
      finalFindings: finalScan.findings,
      unverifiedLinks: finalScan.unverifiedLinks,
      limitations: [
        "外部 HTTP、HTTPS、mailto 和 tel 目标未实际请求。",
        "五页及三份参考材料以外的本地链接目标未读取，列为未验证。",
        "Markdown 标题片段、百分号编码片段及资源文件链接未完整验证。"
      ]
    };
  } catch (error) {
    return {
      status: "blocked",
      reason: String(error),
      checkedFiles: REFERENCE_FILES.concat(PAGE_FILES),
      limitations: ["输入读取、验收标准生成或验收检查未能完成。"]
    };
  }
}

const acceptanceResult = await runAcceptance();
