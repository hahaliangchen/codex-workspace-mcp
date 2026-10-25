import type { ReactNode } from 'react'
import {
  IconApiOutlineMedium, IconArchiveCheckOutlineMedium, IconArchiveOutlineMedium, IconBranchOutlineMedium,
  IconBrowseOutlineMedium, IconCheckCircleOutlineMedium, IconChecklistOutlineMedium, IconClockOutlineMedium,
  IconCloseOutlineMedium, IconCompactOutlineMedium, IconCompareSplitOutlineMedium, IconCordisPluginOutlineMedium,
  IconDatabaseOutlineMedium, IconEditOutlineMedium, IconFolderOpenOutlineMedium, IconGoalOutlineMedium,
  IconInspectOutlineMedium, IconListPenOutlineMedium, IconPaperclipOutlineMedium, IconPinOutlineMedium,
  IconPlayOutlineMedium, IconQuestionOutlineMedium, IconQueueOutlineMedium, IconRefreshOutlineMedium,
  IconSearchOutlineMedium, IconSkillOutlineMedium, IconUnarchiveOutlineMedium, IconUsersOutlineMedium,
  IconWarningTriangleOutlineMedium, IconWorkspaceTreeOutlineMedium,
} from '@deepseek-ai/dsh-client-ui-primitives'

/** Colour family of a tool row's icon chip; the palette lives in ToolRow.module.css. */
export type ToolTone = 'read' | 'edit' | 'run' | 'search' | 'task' | 'agent' | 'media' | 'web' | 'context' | 'danger' | 'neutral'

export interface ToolVisual {
  readonly icon: ReactNode
  readonly tone: ToolTone
  readonly title: string
  /** Replaces the generic argument summary when the tool has a better one. */
  readonly summary?: string | undefined
}

function Glyph({ children }: { children: ReactNode }) {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3"
      strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {children}
    </svg>
  )
}

const FileTextIcon = () => (
  <Glyph>
    <path d="M9.5 1.75H4.25a1.5 1.5 0 0 0-1.5 1.5v9.5a1.5 1.5 0 0 0 1.5 1.5h7.5a1.5 1.5 0 0 0 1.5-1.5V5.5L9.5 1.75Z" />
    <path d="M9.5 1.75V5.5h3.75M5.5 8.5h5M5.5 11h3.5" />
  </Glyph>
)

const FilePenIcon = () => (
  <Glyph>
    <path d="M8 14.25H4.25a1.5 1.5 0 0 1-1.5-1.5v-9.5a1.5 1.5 0 0 1 1.5-1.5H9.5l3.75 3.75V7" />
    <path d="M9.5 1.75V5.5h3.75M12.6 9.15l1.25 1.25-3.6 3.6H9v-1.25l3.6-3.6Z" />
  </Glyph>
)

const TerminalIcon = () => (
  <Glyph>
    <rect x="1.75" y="2.75" width="12.5" height="10.5" rx="2" />
    <path d="M4.75 6.25 6.75 8l-2 1.75M8.5 10h2.75" />
  </Glyph>
)

const ImageIcon = () => (
  <Glyph>
    <rect x="1.75" y="2.75" width="12.5" height="10.5" rx="2" />
    <circle cx="5.75" cy="6.25" r="1.1" />
    <path d="m2.25 11.75 3.5-3.25 2.5 2.25 2-1.75 3.5 3" />
  </Glyph>
)

const ReturnIcon = () => (
  <Glyph>
    <path d="M6 4.25 2.75 7.5 6 10.75" />
    <path d="M2.75 7.5h7a3.5 3.5 0 0 1 3.5 3.5v1.25" />
  </Glyph>
)

const EXACT: Record<string, Omit<ToolVisual, 'summary'>> = {
  // Task graph: the agent creates tasks and steps; the host iterates them.
  create_task: { tone: 'task', icon: <IconGoalOutlineMedium />, title: '创建了任务' },
  create_step: { tone: 'task', icon: <IconPlayOutlineMedium />, title: '进入下一步' },
  // Older sessions: plan, enter, branch, fold and scope work.
  plan_tasks: { tone: 'task', icon: <IconChecklistOutlineMedium />, title: '规划了子任务' },
  start_task: { tone: 'task', icon: <IconPlayOutlineMedium />, title: '开始子任务' },
  branch_task: { tone: 'task', icon: <IconBranchOutlineMedium />, title: '插入分支任务' },
  split_task: { tone: 'task', icon: <IconCompareSplitOutlineMedium />, title: '拆分了任务' },
  scope_message: { tone: 'task', icon: <IconPinOutlineMedium />, title: '确定了指令范围' },
  read_task_board: { tone: 'task', icon: <IconGoalOutlineMedium />, title: '查看了任务面板' },
  update_task_definition: { tone: 'task', icon: <IconListPenOutlineMedium />, title: '提议了任务定义' },
  // Delegates.
  spawn_agent: { tone: 'agent', icon: <IconUsersOutlineMedium />, title: '派发了子代理' },
  wait_agents: { tone: 'agent', icon: <IconQueueOutlineMedium />, title: '等待子代理' },
  review_agent_result: { tone: 'agent', icon: <IconCheckCircleOutlineMedium />, title: '验收了子代理结果' },
  inspect_agent_changes: { tone: 'agent', icon: <IconInspectOutlineMedium />, title: '检查了子代理改动' },
  merge_agent_result: { tone: 'agent', icon: <IconBranchOutlineMedium />, title: '合并了子代理结果' },
  cancel_agent: { tone: 'agent', icon: <IconCloseOutlineMedium />, title: '取消了子代理' },
  // Context, assets, history and skills.
  compress_context: { tone: 'context', icon: <IconCompactOutlineMedium />, title: '整理了上下文' },
  release_assets: { tone: 'context', icon: <IconUnarchiveOutlineMedium />, title: '释放了资产' },
  list_context_assets: { tone: 'context', icon: <IconArchiveCheckOutlineMedium />, title: '查看了上下文资产' },
  read_session_history: { tone: 'context', icon: <IconClockOutlineMedium />, title: '查阅了历史记录' },
  list_skills: { tone: 'context', icon: <IconSkillOutlineMedium />, title: '列出了技能' },
  read_skill: { tone: 'context', icon: <IconSkillOutlineMedium />, title: '读取了技能' },
  unload_skill: { tone: 'context', icon: <IconSkillOutlineMedium />, title: '卸载了技能' },
  propose_project_note: { tone: 'context', icon: <IconEditOutlineMedium />, title: '建议了项目笔记' },
  read_asset: { tone: 'read', icon: <IconArchiveOutlineMedium />, title: '读取了资产' },
  refresh_asset: { tone: 'read', icon: <IconRefreshOutlineMedium />, title: '刷新了资产' },
  read_attachment: { tone: 'read', icon: <IconPaperclipOutlineMedium />, title: '读取了附件' },
  // Images and the web.
  analyze_images: { tone: 'media', icon: <ImageIcon />, title: '分析了图片' },
  image_analysis_results: { tone: 'media', icon: <ImageIcon />, title: '获取了图片分析' },
  view_image: { tone: 'media', icon: <ImageIcon />, title: '查看了图片' },
  browser_screenshot: { tone: 'media', icon: <ImageIcon />, title: '截取了页面' },
  http_probe: { tone: 'web', icon: <IconApiOutlineMedium />, title: '探测了接口' },
  run_program: { tone: 'run', icon: <TerminalIcon />, title: '运行了程序' },
}

const RETURNS: Record<string, Omit<ToolVisual, 'summary'>> = {
  done: { tone: 'task', icon: <IconCheckCircleOutlineMedium />, title: '完成了子任务' },
  failed: { tone: 'danger', icon: <IconWarningTriangleOutlineMedium />, title: '子任务未完成' },
  blocked: { tone: 'danger', icon: <IconWarningTriangleOutlineMedium />, title: '子任务受阻' },
  needs_user: { tone: 'read', icon: <IconQuestionOutlineMedium />, title: '等待你的决定' },
}

function fallback(name: string): Omit<ToolVisual, 'summary'> {
  if (name === 'run_command' || /bash|shell|exec|powershell|cmd/.test(name)) return { tone: 'run', icon: <TerminalIcon />, title: '运行了命令' }
  if (/project_script/.test(name)) return { tone: 'run', icon: <TerminalIcon />, title: '运行了项目脚本' }
  if (/process/.test(name)) return { tone: 'run', icon: <TerminalIcon />, title: '查看了进程' }
  if (/^browser_/.test(name)) return { tone: 'web', icon: <IconBrowseOutlineMedium />, title: '操作了浏览器' }
  if (/write|edit|replace|apply|patch|create|delete/.test(name)) return { tone: 'edit', icon: <FilePenIcon />, title: '编辑了文件' }
  if (/search|grep/.test(name)) return { tone: 'search', icon: <IconSearchOutlineMedium />, title: '搜索了代码' }
  if (/symbol|index|code_map|code_flow/.test(name)) return { tone: 'search', icon: <IconWorkspaceTreeOutlineMedium />, title: '检索了符号' }
  if (/list|find/.test(name)) return { tone: 'search', icon: <IconFolderOpenOutlineMedium />, title: '浏览了目录' }
  if (/memory|architecture|business/.test(name)) return { tone: 'context', icon: <IconDatabaseOutlineMedium />, title: '读取了工作区记忆' }
  if (/read|view_file|open|view/.test(name)) return { tone: 'read', icon: <FileTextIcon />, title: '读取了文件' }
  return { tone: 'neutral', icon: <IconCordisPluginOutlineMedium />, title: name }
}

const text = (value: unknown) => typeof value === 'string' ? value.replace(/\s+/g, ' ').trim() : ''
const count = (value: unknown) => Array.isArray(value) ? value.length : 0
/** Host node IDs are long hashes; the leading characters are enough to tell them apart. */
const shortNode = (value: unknown) => text(value) === 'n0' ? '主任务' : `子任务 ${text(value).slice(0, 7)}`

function summaryFor(name: string, args: Record<string, unknown>): string | undefined {
  switch (name) {
    case 'create_task': return [text(args.goal), text(args.reason) && `原因：${text(args.reason)}`].filter(Boolean).join(' · ')
    case 'create_step': return [text(args.result) && `完成：${text(args.result)}`, text(args.next) && `下一步：${text(args.next)}`].filter(Boolean).join(' · ')
    case 'plan_tasks': {
      const tasks = Array.isArray(args.tasks) ? args.tasks : []
      const titles = tasks.map(task => text((task as Record<string, unknown> | null)?.title)).filter(Boolean)
      return `${tasks.length} 项${titles.length > 0 ? `：${titles.join('、')}` : ''}`
    }
    case 'start_task': return text(args.briefing) || text(args.node_id)
    case 'branch_task': return [text(args.title), text(args.reason) && `原因：${text(args.reason)}`].filter(Boolean).join(' · ')
    case 'return_task': return text(args.status === 'needs_user' ? args.question : args.result)
    case 'split_task': return `${text(args.title)}${count(args.tasks) > 0 ? ` · 剩余 ${count(args.tasks)} 项` : ''}`
    case 'scope_message': return `作用于${shortNode(args.node_id)}`
    case 'update_task_definition': return text(args.objective)
    case 'spawn_agent': return text(args.name) || text(args.task)
    case 'compress_context': return text(args.reason)
    default: return undefined
  }
}

/** Icon, colour family, Chinese action title and summary for one tool call.
 * `result` is the parsed tool output when available: a return from the root
 * node (n0) finishes the main task rather than a subtask. */
export function toolVisual(name: string, args: unknown, result?: unknown): ToolVisual {
  const record = typeof args === 'object' && args !== null ? args as Record<string, unknown> : {}
  let base = name === 'return_task'
    ? RETURNS[text(record.status)] ?? { tone: 'task', icon: <ReturnIcon />, title: '返回了结果' }
    : EXACT[name] ?? fallback(name)
  if (name === 'return_task' && typeof result === 'object' && result !== null
    && (result as Record<string, unknown>).returned === 'n0') {
    base = { ...base, title: base.title.replace('子任务', '主任务') }
  }
  return { ...base, summary: summaryFor(name, record) }
}
