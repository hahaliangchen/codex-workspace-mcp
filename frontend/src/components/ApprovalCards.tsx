import { useMemo, useState } from 'react'
import type { AgentEvent } from '../api.ts'
import css from './NoteProposal.module.css'

type Decision = 'once' | 'session' | 'project' | 'deny'

interface PendingApproval {
  readonly callId: string
  readonly tool: string
  readonly summary: string
  readonly project: boolean
}

const TOOL_LABELS: Record<string, string> = {
  run_program: '运行命令', run_project_script: '运行项目脚本', install_dependencies: '安装依赖',
  stop_project_process: '停止项目进程', browser_open: '打开浏览器', browser_click: '在浏览器里点击',
  browser_press_key: '在浏览器里按键', browser_upload: '在浏览器里上传文件', browser_screenshot: '浏览器截图',
  browser_read: '读取浏览器页面', browser_wait: '等待浏览器页面', browser_diagnostics: '读取浏览器诊断', browser_close: '关闭浏览器',
}

/** The most telling part of the call: a command line, script, URL or path. */
function summarize(raw: string): string {
  try {
    const args = JSON.parse(raw) as Record<string, unknown>
    const text = (value: unknown) => typeof value === 'string' ? value : ''
    const list = (value: unknown) => Array.isArray(value) ? value.map(String).join(' ') : ''
    const program = text(args.program) || text(args.command)
    if (program) return [program, list(args.args)].filter(Boolean).join(' ')
    const script = text(args.script) || text(args.name)
    if (script) return [text(args.project) || text(args.cwd), script].filter(Boolean).join(' · ')
    return text(args.url) || text(args.path) || raw
  } catch {
    return raw
  }
}

/** Approval requests the agent is waiting on, oldest first. */
export function pendingApprovals(events: readonly AgentEvent[]): PendingApproval[] {
  const decided = new Set(events.filter(event => event.type === 'approval/decision').map(event => String(event.data.call_id)))
  return events
    .filter(event => event.type === 'approval/request' && !decided.has(String(event.data.call_id)))
    .map(event => ({
      callId: String(event.data.call_id),
      tool: String(event.data.tool),
      summary: summarize(String(event.data.arguments ?? '')),
      project: typeof event.data.project === 'string',
    }))
}

/** "Ask before running" questions, shown at the end of the conversation while the agent waits. */
export function ApprovalCards({ taskId, events, className }: { taskId: string; events: readonly AgentEvent[]; className?: string | undefined }) {
  const pending = useMemo(() => pendingApprovals(events), [events])
  const [sent, setSent] = useState<Record<string, Decision>>({})
  const [error, setError] = useState('')
  if (pending.length === 0) return null
  const decide = (callId: string, decision: Decision) => {
    setError('')
    setSent(current => ({ ...current, [callId]: decision }))
    fetch(`/agent/tasks/${encodeURIComponent(taskId)}/approvals`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ call_id: callId, decision }),
    }).then(async response => {
      if (!response.ok) throw new Error((await response.json().catch(() => ({}))).error ?? response.statusText)
    }).catch((cause: unknown) => {
      setSent(current => { const next = { ...current }; delete next[callId]; return next })
      setError(cause instanceof Error ? cause.message : String(cause))
    })
  }
  return <div className={className}>{pending.map(item => {
    const chosen = sent[item.callId]
    return <div key={item.callId} className={css.card} role="alertdialog" aria-label={`是否允许${TOOL_LABELS[item.tool] ?? item.tool}`}>
      <div className={css.title}>要{TOOL_LABELS[item.tool] ?? `调用 ${item.tool}`}吗？</div>
      <code className={css.command}>{item.summary}</code>
      {error && <p className={css.error} role="alert">{error}</p>}
      <div className={css.actions}>
        <button type="button" className={css.secondary} disabled={!!chosen} onClick={() => decide(item.callId, 'deny')}>拒绝</button>
        {item.project && <button type="button" className={css.secondary} disabled={!!chosen} onClick={() => decide(item.callId, 'project')}>这个项目以后都允许</button>}
        <button type="button" className={css.secondary} disabled={!!chosen} onClick={() => decide(item.callId, 'session')}>本会话都允许</button>
        <button type="button" className={css.primary} disabled={!!chosen} onClick={() => decide(item.callId, 'once')}>{chosen ? '已提交…' : '允许这一次'}</button>
      </div>
    </div>
  })}</div>
}
