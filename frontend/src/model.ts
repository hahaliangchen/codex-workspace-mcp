import type { AgentEvent, Task } from './api.ts'

export interface ToolResult {
  readonly text: string
  readonly isError: boolean
  readonly durationMs: number | undefined
  readonly childTaskId: string | undefined
  readonly time: number
}

export type TurnEndReason =
  | { readonly kind: 'completed'; readonly goalAchieved?: boolean | undefined; readonly unresolved?: readonly string[] | undefined }
  | { readonly kind: 'aborted' }
  | { readonly kind: 'error'; readonly message: string; readonly code: string | undefined }

export type ChatItem =
  | { readonly kind: 'subagent'; readonly key: string; readonly agentId: string; readonly name: string; readonly status: string; readonly events: readonly AgentEvent[]; readonly report: string; readonly review: string; readonly time: number }
  | { readonly kind: 'turn'; readonly key: string; readonly turn: number }
  | { readonly kind: 'interjection'; readonly key: string; readonly text: string; readonly time: number; readonly delivered: boolean }
  | { readonly kind: 'user'; readonly key: string; readonly text: string; readonly time: number; readonly attachments?:readonly {name:string;url:string;image:boolean}[] }
  | { readonly kind: 'assistant'; readonly key: string; readonly text: string; readonly reasoning: string; readonly time: number; readonly streaming: boolean; readonly turn: number; readonly step: number }
  | { readonly kind: 'observer'; readonly key: string; readonly summary: string; readonly suggestions: readonly string[]; readonly time: number }
  | { readonly kind: 'retrospective'; readonly key: string; readonly text: string; readonly time: number }
  | {
    readonly kind: 'tool'
    readonly key: string
    readonly callId: string
    readonly name: string
    readonly arguments: string
    readonly time: number
    readonly result: ToolResult | undefined
  }
  | { readonly kind: 'turn-end'; readonly key: string; readonly reason: TurnEndReason; readonly durationMs?: number | undefined; readonly time?: number | undefined }

type Block = { readonly type?: unknown; readonly text?: unknown; readonly content?: unknown }

/** Rust nests the SessionEvent message under `data.message`; DSH puts it at `data`. */
function messageOf(event: AgentEvent): Record<string, unknown> {
  const nested = event.data.message
  return typeof nested === 'object' && nested !== null ? nested as Record<string, unknown> : event.data
}

function textOf(content: unknown, type: 'text' | 'reasoning' = 'text'): string {
  if (typeof content === 'string') return type === 'text' ? content : ''
  if (!Array.isArray(content)) return ''
  const parts: string[] = []
  for (const block of content as Block[]) {
    if (block?.type === type && typeof block.text === 'string') parts.push(block.text)
    else if (type === 'text' && block?.type === 'tool-result') parts.push(textOf(block.content))
  }
  return parts.join('\n')
}

/** Some model providers put a thinking block in text instead of the reasoning channel. */
function assistantDisplay(text: string, reasoning: string): { text: string; reasoning: string } {
  const leading = text.trimStart()
  const opening = /^<thinking>/i.exec(leading)
  if (!opening) {
    // A streaming chunk may stop halfway through the opening tag.
    if (leading.startsWith('<') && '<thinking>'.startsWith(leading.toLowerCase())) {
      return { text: '', reasoning }
    }
    return { text, reasoning }
  }
  const closing = /<\/thinking>/i.exec(leading.slice(opening[0].length))
  if (!closing) return { text: '', reasoning }
  const innerStart = opening[0].length
  const innerEnd = innerStart + closing.index
  const thought = leading.slice(innerStart, innerEnd).trim()
  const body = leading.slice(innerEnd + closing[0].length).trimStart()
  return { text: body, reasoning: [reasoning, thought].filter(Boolean).join('\n\n') }
}

function stringOf(value: unknown): string | undefined {
  return typeof value === 'string' && value !== '' ? value : undefined
}

function numberOf(value: unknown): number | undefined {
  return typeof value === 'number' ? value : undefined
}

function argumentsOf(value: unknown): string {
  if (typeof value === 'string') return value
  return value === undefined ? '' : JSON.stringify(value)
}

function reasonOf(value: unknown): TurnEndReason {
  const reason = (value ?? {}) as { kind?: unknown; goal_achieved?: unknown; unresolved?: unknown; error?: { message?: unknown; code?: unknown } }
  if (reason.kind === 'completed') return { kind: 'completed',
    goalAchieved: typeof reason.goal_achieved === 'boolean' ? reason.goal_achieved : undefined,
    unresolved: Array.isArray(reason.unresolved) ? reason.unresolved.filter((item): item is string => typeof item === 'string') : undefined }
  if (reason.kind === 'error') {
    return { kind: 'error', message: stringOf(reason.error?.message) ?? '', code: stringOf(reason.error?.code) }
  }
  return { kind: 'aborted' }
}

function resultOf(event: AgentEvent): ToolResult {
  const message = messageOf(event)
  const meta = (event.data.meta ?? {}) as { durationMs?: unknown; result?: { child_task_id?: unknown } }
  return {
    text: textOf(message.content),
    isError: message.isError === true,
    durationMs: numberOf(meta.durationMs),
    childTaskId: stringOf(meta.result?.child_task_id),
    time: event.time,
  }
}

export function callIdOfResult(event: AgentEvent): string | undefined {
  const message = messageOf(event)
  const source = message.source as { callId?: unknown } | undefined
  return stringOf(source?.callId) ?? stringOf(message.toolCallId)
}

/** Project an ordered event log into transcript rows; tool results attach to their call. */
export function chatItems(events: readonly AgentEvent[], task?: Task): ChatItem[] {
  events = [...events].sort((a, b) => a.seq - b.seq)
  const delegates = new Map<string, { status: string; events: AgentEvent[]; report: string; review: string }>()
  for (const event of events) {
    const id = stringOf(event.data.agent_id)
    if (!id || !event.type.startsWith('subagent/')) continue
    const child = delegates.get(id) ?? { status: 'running', events: [], report: '', review: '' }
    if (event.type === 'subagent/event' && typeof event.data.event_type === 'string') {
      child.events.push({ ...event, type: event.data.event_type, data: event.data.event as Record<string, unknown> })
    } else if (event.type === 'subagent/end') {
      child.status = stringOf(event.data.status) ?? 'interrupted'
      const result = event.data.result as Record<string, unknown> | undefined
      child.report = [textOf(result?.report), stringOf(result?.error)].filter(Boolean).join('\n\n')
    } else if (event.type === 'subagent/review') {
      const review = event.data.review as Record<string, unknown> | undefined
      child.review = `${stringOf(review?.decision) ?? ''}：${stringOf(review?.evidence) ?? ''}`
    }
    delegates.set(id, child)
  }
  const deliveredInterjections = new Set(events.filter(event => event.type === 'user/interjection_delivered').map(event => Number(event.data.interjection_seq)))
  const results = new Map<string, ToolResult>()
  for (const event of events) {
    if (event.type !== 'tool/result') continue
    const id = callIdOfResult(event)
    if (id !== undefined) results.set(id, resultOf(event))
  }
  const items: ChatItem[] = []
  let turnStartTime: number | undefined

  for (const event of events) {
    const key = `${event.type}:${event.seq}`
    switch (event.type) {
      case 'subagent/start': {
        const agentId = stringOf(event.data.agent_id)
        if (!agentId) break
        const child = delegates.get(agentId)
        items.push({ kind: 'subagent', key: `delegate:${agentId}`, agentId, name: stringOf(event.data.name) ?? '子任务', status: child?.status ?? 'running', events: child?.events ?? [], report: child?.report ?? '', review: child?.review ?? '', time: event.time })
        break
      }
      case 'turn/start': {
        turnStartTime = event.time
        const turn = numberOf(event.data.turn) ?? 1
        if (turn > 1) items.push({ kind: 'turn', key, turn })
        break
      }
      case 'user/interjection':
        items.push({ kind: 'interjection', key, text: stringOf(event.data.text) ?? '', time: event.time, delivered: deliveredInterjections.has(event.seq) })
        break
      case 'user/message': {
        if (turnStartTime === undefined) {
          turnStartTime = event.time
        }
        const text = textOf(messageOf(event).content)
        const content=messageOf(event).content
        const attachments=(Array.isArray(content)?content:[]).filter(p=>p&&typeof p==='object'&&('attachment' in p)).map(p=>{
          const part=p as {type:string;attachment:{attachmentId:string;name:string}}
          return {name:part.attachment.name,url:`/agent/tasks/${encodeURIComponent(String(event.data.task_id))}/attachments/${encodeURIComponent(part.attachment.attachmentId)}`,image:part.type==='image'}
        })
        if (text !== ''||attachments.length) items.push({ kind: 'user', key, text, attachments, time: event.time })
        break
      }
      case 'assistant/delta': {
        const turn = numberOf(event.data.turn) ?? 0
        const step = numberOf(event.data.step) ?? 0
        const piece = stringOf(event.data.text) ?? ''
        const reasoningPiece = stringOf(event.data.reasoning) ?? ''
        const last = items.at(-1)
        if (last?.kind === 'assistant' && last.streaming && last.turn === turn && last.step === step) {
          items[items.length - 1] = {
            ...last,
            text: last.text + piece,
            reasoning: last.reasoning + reasoningPiece,
            time: event.time,
          }
        } else if (piece !== '' || reasoningPiece !== '') {
          items.push({ kind: 'assistant', key, text: piece, reasoning: reasoningPiece, time: event.time, streaming: true, turn, step })
        }
        break
      }
      case 'assistant/message': {
        const content = messageOf(event).content
        const text = (event.data.acceptance_pending === true ? '本轮执行结果（提交 Observer 验收）\n\n' : '') + textOf(content)
        const reasoning = textOf(content, 'reasoning')
        const turn = numberOf(event.data.turn) ?? 0
        const step = numberOf(event.data.step) ?? 0
        const last = items.at(-1)
        if (text === '' && reasoning === '') break
        if (last?.kind === 'assistant' && last.streaming && last.turn === turn && last.step === step) {
          items[items.length - 1] = { kind: 'assistant', key, text, reasoning, time: event.time, streaming: false, turn, step }
        } else {
          items.push({ kind: 'assistant', key, text, reasoning, time: event.time, streaming: false, turn, step })
        }
        break
      }
      case 'observer/plan_review':
      case 'observer/progress_review': {
        const suggestions = Array.isArray(event.data.suggestions)
          ? event.data.suggestions.filter((value): value is string => typeof value === 'string' && value.trim() !== '')
          : []
        if (suggestions.length > 0) {
          items.push({ kind: 'observer', key, summary: stringOf(event.data.summary) ?? '', suggestions, time: event.time })
        }
        break
      }
      case 'observer/retrospective': {
        const original = event.data.observer_return
        const report = original && typeof original === 'object' ? original as Record<string, unknown> : event.data
        const acceptance = report.acceptance && typeof report.acceptance === 'object' ? report.acceptance as Record<string, unknown> : undefined
        const labels: Record<string, string> = { passed: '要求已满足', needs_work: '需要补齐', blocked: '受限，存在未完成项', uncertain: '尚未确认' }
        const requirements = Array.isArray(acceptance?.requirements) ? acceptance.requirements.flatMap(value => {
          if (!value || typeof value !== 'object') return []
          const item = value as Record<string, unknown>
          return [`- ${String(item.requirement ?? '')}：${String(item.result ?? '')}${item.explanation ? `；${String(item.explanation)}` : ''}`]
        }) : []
        const text = [acceptance ? `验收：${labels[String(acceptance.assessment)] ?? String(acceptance.assessment ?? '尚未确认')}` : undefined,
          ...requirements, report.action === 'continue' ? 'Observer 请求将具体缺口交回 Organizer，继续补齐。' : undefined,
          stringOf(report.summary) ?? stringOf(event.data.summary),
          stringOf(report.path_review) ?? stringOf(event.data.pathReview),
          ...observerStringList(report.shortening_opportunities ?? event.data.shorteningOpportunities).map(item => `- 下次可改进：${item}`),
          event.data.status === 'unavailable' ? `复盘未完成：${stringOf(event.data.message) ?? '未返回结果'}` : undefined]
          .filter(Boolean).join('\n\n')
        if (text) items.push({ kind: 'retrospective', key, text, time: event.time })
        break
      }
      case 'tool/call': {
        const callId = stringOf(event.data.callId) ?? key
        items.push({
          kind: 'tool',
          key,
          callId,
          name: stringOf(event.data.name) ?? 'tool',
          arguments: argumentsOf(event.data.arguments),
          time: event.time,
          result: results.get(callId),
        })
        break
      }
      case 'turn/end': {
        const durationMs = turnStartTime !== undefined ? Math.max(0, event.time - turnStartTime) : undefined
        const reason = reasonOf(event.data.reason)
        items.push({
          kind: 'turn-end',
          key,
          reason,
          durationMs,
          time: event.time,
        })
        turnStartTime = undefined
        break
      }
    }
  }

  // If task is completed/stopped but no turn/end event was recorded:
  if (
    items.length > 0 &&
    items.findLastIndex(item => item.kind === 'turn-end') < items.findLastIndex(item => item.kind === 'user' || item.kind === 'turn') &&
    task &&
    (task.status === 'completed' || task.status === 'failed' || task.status === 'interrupted' || task.status === 'cancelled')
  ) {
    const durationMs = turnStartTime !== undefined
      ? Math.max(0, (task.updated_at || Date.now()) - turnStartTime)
      : (task.updated_at && task.created_at ? Math.max(0, task.updated_at - task.created_at) : undefined)
    items.push({
      kind: 'turn-end',
      key: `synthetic:turn-end:${task.task_id}`,
      reason: task.status === 'completed' ? { kind: 'completed' } : { kind: 'aborted' },
      durationMs,
      time: task.updated_at,
    })
  }

  return items.map(item => item.kind === 'assistant'
    ? { ...item, ...assistantDisplay(item.text, item.reasoning) }
    : item)
}

/** Formats a millisecond duration into a human-friendly Chinese string (e.g. "28秒", "1分15秒", "1小时2分"). */
export function formatDuration(ms: number | undefined): string {
  if (ms === undefined || ms < 0 || isNaN(ms)) return ''
  const totalSeconds = Math.round(ms / 1000)
  if (totalSeconds < 1) return '< 1秒'
  if (totalSeconds < 60) return `${totalSeconds}秒`
  const minutes = Math.floor(totalSeconds / 60)
  const remainingSeconds = totalSeconds % 60
  if (minutes < 60) {
    return remainingSeconds > 0 ? `${minutes}分${remainingSeconds}秒` : `${minutes}分钟`
  }
  const hours = Math.floor(minutes / 60)
  const remainingMinutes = minutes % 60
  return remainingMinutes > 0 ? `${hours}小时${remainingMinutes}分` : `${hours}小时`
}

/** Formats milliseconds into mm:ss timer format (e.g. "00:23", "01:15"). */
export function formatTimer(ms: number | undefined): string {
  if (ms === undefined || ms < 0 || isNaN(ms)) return '00:00'
  const totalSeconds = Math.floor(ms / 1000)
  const mins = Math.floor(totalSeconds / 60)
  const secs = totalSeconds % 60
  return `${String(mins).padStart(2, '0')}:${String(secs).padStart(2, '0')}`
}

export interface TaskDurationInfo {
  readonly currentTurnDurationMs?: number | undefined
  readonly isRunning: boolean
  readonly currentTurnStartTime?: number | undefined
  readonly currentTurnEndTime?: number | undefined
  readonly totalTaskDurationMs?: number | undefined
}

export function getTaskDurationInfo(
  events: readonly AgentEvent[],
  task?: Task,
  running?: boolean,
  now: number = Date.now()
): TaskDurationInfo {
  let firstStartTime: number | undefined
  let lastTurnStartTime: number | undefined
  let lastTurnEndTime: number | undefined
  let hasTurnEndAfterLastStart = false

  for (const event of events) {
    if (event.type === 'turn/start' || event.type === 'user/message') {
      if (firstStartTime === undefined) {
        firstStartTime = event.time
      }
      lastTurnStartTime = event.time
      hasTurnEndAfterLastStart = false
    } else if (event.type === 'turn/end') {
      lastTurnEndTime = event.time
      hasTurnEndAfterLastStart = true
    }
  }

  if (firstStartTime === undefined && task?.created_at) {
    firstStartTime = task.created_at
  }
  if (lastTurnStartTime === undefined && task?.created_at) {
    lastTurnStartTime = task.created_at
  }

  const isRunning = Boolean(
    running ||
    task?.status === 'running' ||
    task?.status === 'cancelling'
  )

  let currentTurnDurationMs: number | undefined
  let totalTaskDurationMs: number | undefined

  if (isRunning) {
    if (lastTurnStartTime !== undefined) {
      currentTurnDurationMs = Math.max(0, now - lastTurnStartTime)
    }
    if (firstStartTime !== undefined) {
      totalTaskDurationMs = Math.max(0, now - firstStartTime)
    }
  } else {
    if (lastTurnStartTime !== undefined) {
      const endTime = (hasTurnEndAfterLastStart && lastTurnEndTime !== undefined)
        ? lastTurnEndTime
        : (task?.updated_at && task.updated_at >= lastTurnStartTime ? task.updated_at : lastTurnStartTime)
      currentTurnDurationMs = Math.max(0, endTime - lastTurnStartTime)
      lastTurnEndTime = endTime
    }
    if (firstStartTime !== undefined) {
      const overallEndTime = lastTurnEndTime ?? task?.updated_at ?? firstStartTime
      totalTaskDurationMs = Math.max(0, overallEndTime - firstStartTime)
    }
  }

  return {
    currentTurnDurationMs,
    isRunning,
    currentTurnStartTime: lastTurnStartTime,
    currentTurnEndTime: isRunning ? undefined : lastTurnEndTime,
    totalTaskDurationMs,
  }
}

/** Whether the log is waiting on the model: a step opened with no reply yet. */
export function awaitingModel(events: readonly AgentEvent[]): boolean {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const type = events[index]?.type
    if (type === 'step/start') return true
    if (type === 'assistant/delta' || type === 'assistant/message' || type === 'tool/call' || type === 'tool/result' || type === 'turn/end') return false
  }
  return false
}

export interface TrajectoryRow {
  readonly key: string
  readonly seq: number
  readonly time: number
  readonly type: string
  readonly turn: number | undefined
  readonly step: number | undefined
  readonly name: string | undefined
  readonly detail: string
  readonly durationMs: number | undefined
  readonly failed: boolean
  readonly childTaskId: string | undefined
  readonly input: string | undefined
}

export function trajectoryRows(events: readonly AgentEvent[]): TrajectoryRow[] {
  const calls = new Map<string, AgentEvent>()
  for (const event of events) {
    if (event.type === 'tool/call' || event.type === 'context/tool_call') calls.set(stringOf(event.data.callId) ?? '', event)
  }
  return events.filter((event) => event.type !== 'assistant/delta' && event.type !== 'observer/config' && !event.type.startsWith('debug/')).map((event) => {
    const data = event.data
    const base = {
      key: `${event.type}:${event.seq}`,
      seq: event.seq,
      time: event.time,
      type: event.type,
      turn: numberOf(data.turn),
      step: numberOf(data.step),
      name: undefined as string | undefined,
      detail: '',
      durationMs: undefined as number | undefined,
      failed: false,
      childTaskId: undefined as string | undefined,
      input: undefined as string | undefined,
    }
    switch (event.type) {
      case 'user/message':
        return { ...base, detail: textOf(messageOf(event).content) }
      case 'assistant/message':
        return { ...base, detail: assistantDisplay(textOf(messageOf(event).content), '').text }
      case 'visual/input_result':
        return { ...base, name: '视觉输入结果', detail: textOf(messageOf(event).content) }
      case 'context/tool_call':
      case 'tool/call':
        return { ...base, name: stringOf(data.name), detail: argumentsOf(data.arguments) }
      case 'context/tool_result':
      case 'tool/result': {
        const result = resultOf(event)
        const call = calls.get(callIdOfResult(event) ?? '')
        return {
          ...base,
          name: call === undefined ? undefined : stringOf(call.data.name),
          detail: result.text,
          durationMs: result.durationMs,
          failed: result.isError,
          childTaskId: result.childTaskId,
          input: call === undefined ? undefined : argumentsOf(call.data.arguments),
        }
      }
      case 'observer/brief': {
        const focus = stringOf(data.focus) ?? ''
        const deferred = Array.isArray(data.defer)
          ? data.defer.filter((item): item is string => typeof item === 'string').map(item => `暂缓：${item}`)
          : []
        const question = stringOf(data.question) ?? ''
        const evidence = Array.isArray(data.evidence)
          ? data.evidence.map(item => {
              if (typeof item !== 'object' || item === null) return ''
              const source = item as Record<string, unknown>
              const id = stringOf(source.source_id) ?? 'history'
              const excerpt = stringOf(source.text) ?? stringOf(source.summary) ?? ''
              return `[${id}] ${excerpt}`
            }).filter(Boolean)
          : []
        return { ...base, name: 'scope', detail: [focus, ...deferred, question, ...evidence].filter(Boolean).join('\n') }
      }
      case 'observer/advice_delivered':
      case 'observer/advice_response': {
        const advice = typeof data.advice === 'object' && data.advice !== null ? data.advice as Record<string, unknown> : {}
        const disposition = stringOf(advice.disposition) ?? 'unread'
        const labels: Record<string, string> = { unread: '待回应', accepted: '采纳', adjusted: '调整', declined: '不采纳', resolved: '已解决' }
        return { ...base, name: event.type === 'observer/advice_response' ? stringOf(data.actor) ?? 'worker' : stringOf(advice.id) ?? 'Observer', detail: [
          stringOf(advice.summary), ...observerStringList(advice.suggestions),
          `回应：${labels[disposition] ?? disposition}`, stringOf(advice.reason),
        ].filter(Boolean).join('\n') }
      }
      case 'observer/node_review': {
        const identity = (data.identity ?? {}) as Record<string, unknown>
        const result = (data.result ?? {}) as Record<string, unknown>
        return { ...base, name: stringOf(data.stage), detail: `${String(identity.work_id ?? '')}@${String(identity.revision ?? '')} · ${String(data.status ?? 'unassessed')}\n${String(result.summary ?? result.message ?? '')}`, failed: data.status === 'failed' || data.status === 'timeout' }
      }
      case 'observer/plan_review':
      case 'observer/progress_review': {
        const suggestions = observerStringList(data.suggestions)
        const summary = stringOf(data.summary) ?? ''
        const message = stringOf(data.message) ?? ''
        return {
          ...base,
          name: event.type === 'observer/plan_review' ? '计划建议' : '方向观察',
          detail: [summary, ...suggestions.map(item => `建议：${item}`), message].filter(Boolean).join('\n'),
          failed: data.status === 'unavailable',
        }
      }
      case 'observer/retrospective_start':
        return { ...base, name: '验收中', detail: '本轮执行结束，Observer 正在核对当前用户要求及实际成果。' }
      case 'observer/followup':
        return { ...base, name: '补齐中', detail: JSON.stringify(data.observer_return) }
      case 'observer/retrospective': {
        const shortening = observerStringList(data.shorteningOpportunities)
        const findings = Array.isArray(data.workFindings) ? data.workFindings.map(item => {
          if (!item || typeof item !== 'object') return ''
          const finding = item as Record<string, unknown>
          return [
            stringOf(finding.finding),
            stringOf(finding.relevance) ? `相关性：${stringOf(finding.relevance)}` : undefined,
            stringOf(finding.watch_for) ? `后续关注：${stringOf(finding.watch_for)}` : undefined,
          ].filter(Boolean).join('\n')
        }).filter(Boolean) : []
        return {
          ...base,
          name: '完整路径复盘',
          detail: [
            stringOf(data.summary),
            stringOf(data.pathReview),
            ...shortening.map(item => `可缩短：${item}`),
            ...findings,
            data.memoryRecorded === true ? `已记录工作记忆：${stringOf(data.memorySummary) ?? ''}` : undefined,
            stringOf(data.memoryError) ? `工作记忆写入失败：${stringOf(data.memoryError)}` : undefined,
            stringOf(data.message),
          ].filter(Boolean).join('\n'),
          failed: data.status === 'unavailable',
        }
      }
      case 'observer/review': {
        const evidence = Array.isArray(data.evidence)
          ? data.evidence.filter((item): item is string => typeof item === 'string').join('\n')
          : ''
        const reason = stringOf(data.reason) ?? ''
        const suggestion = stringOf(data.suggestion) ?? ''
        const question = stringOf(data.question) ?? ''
        return {
          ...base,
          name: stringOf(data.decision) ?? 'review',
          detail: [reason, evidence, suggestion, question].filter(Boolean).join('\n'),
          input: stringOf(data.tool),
          durationMs: numberOf(data.durationMs),
          failed: data.decision === 'interrupt' || data.decision === 'ask_worker',
        }
      }
      case 'observer/error':
        return { ...base, name: 'unavailable', detail: stringOf(data.message) ?? 'Observer unavailable; action continued', failed: true }
      case 'observer/cancelled':
        return { ...base, name: 'cancelled', detail: 'Observer review cancelled with the task' }
      case 'turn/end': {
        const reason = reasonOf(data.reason)
        return { ...base, detail: reason.kind === 'error' ? reason.message : '', failed: reason.kind === 'error' }
      }
      default:
        return base
    }
  })
}

export type FlowNodeKind = string
export type FlowNodeStatus = 'ready' | 'running' | 'done' | 'waiting_children' | 'suspended' | 'needs_user' | 'cancelled' | 'pending' | 'paused' | 'blocked' | 'completed' | 'skipped' | 'interrupted' | 'failed' | 'deprecated'

export interface FlowToolExecution {
  readonly id: string
  readonly name: string
  readonly input?: string | undefined
  readonly output?: string | undefined
  readonly failed?: boolean | undefined
  readonly durationMs?: number | undefined
}

export interface FlowObserverConsult {
  readonly question: string
  readonly answer?: string | undefined
}

export interface FlowObserverNote {
  readonly kind: 'plan' | 'progress'
  readonly turn?: number | undefined
  readonly nodeId?: string | undefined
  readonly summary: string
  readonly suggestions: readonly string[]
  readonly unavailable?: string | undefined
}

export interface FlowNodeReview {
  readonly id: string
  readonly identity: { readonly request_id: number; readonly work_id: string; readonly node_id: string; readonly revision: number; readonly plan_revision: number }
  readonly turn: number
  readonly stage: string
  readonly status: string
  readonly sourceEventId: string
  readonly decision: Record<string, unknown>
  readonly delivery: Record<string, unknown>
  readonly result: Record<string, unknown>
  readonly responses: readonly Record<string, unknown>[]
}

export function reviewMatchesNode(review: FlowNodeReview, node: FlowNodeData): boolean {
  const raw = node.id.replace(/^turn_\d+:/, '').replace(/^request_\d+:/, '')
  return review.identity.work_id === raw &&
    (node.requestId === undefined ? review.turn === node.turn : review.identity.request_id === node.requestId) &&
    (node.revision === undefined || review.identity.revision === node.revision) &&
    (node.planRevision === undefined || review.identity.plan_revision === node.planRevision)
}

export interface FlowObserverFinding {
  readonly finding: string
  readonly relevance: string
  readonly watchFor: string
  readonly sourceRefs: readonly string[]
  readonly certainty: 'confirmed' | 'uncertain'
}

export interface FlowObserverRouteShortcut {
  readonly situation: string
  readonly lookFirst: string
  readonly avoid: string
}

export interface FlowObserverRetrospective {
  readonly status: 'reviewing' | 'completed' | 'unavailable'
  readonly outcome?: string | undefined
  readonly summary?: string | undefined
  readonly pathReview?: string | undefined
  readonly shorteningOpportunities: readonly string[]
  readonly workFindings: readonly FlowObserverFinding[]
  readonly routeShortcuts: readonly FlowObserverRouteShortcut[]
  readonly memoryRecorded?: boolean | undefined
  readonly memorySummary?: string | undefined
  readonly memoryError?: string | undefined
  readonly message?: string | undefined
}

export interface FlowNodeData {
  [key: string]: unknown
  readonly id: string
  readonly title: string
  readonly kind: FlowNodeKind
  readonly status: FlowNodeStatus
  readonly description?: string | undefined
  readonly response?: string | undefined
  readonly ordinal?: number | undefined
  readonly parentId?: string | undefined
  readonly agentId?: string | undefined
  readonly agentName?: string | undefined
  readonly agentOrder?: number | undefined
  readonly tools: readonly FlowToolExecution[]
  readonly consults: readonly FlowObserverConsult[]
  readonly files: readonly string[]
  readonly turn?: number | undefined
  readonly step?: number | undefined
  readonly revision?: number | undefined
  readonly requestId?: number | undefined
  readonly planRevision?: number | undefined
}

export interface FlowEdgeData {
  readonly id: string
  readonly source: string
  readonly target: string
  readonly label?: string | undefined
  readonly animated?: boolean | undefined
}

export interface FlowState {
  readonly nodeReviews: readonly FlowNodeReview[]
  readonly nodes: readonly FlowNodeData[]
  readonly edges: readonly FlowEdgeData[]
  readonly activeNodeId?: string | undefined
  readonly observerEnabled?: boolean | undefined
  readonly observerNotes: readonly FlowObserverNote[]
  readonly observerRetrospective?: FlowObserverRetrospective | undefined
  readonly organizedTurn: number
}

function observerStringList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string' && item.trim() !== '') : []
}

function extractFilesFromArgs(argsStr: string): string[] {
  try {
    const parsed = typeof argsStr === 'string' ? JSON.parse(argsStr) : argsStr
    if (!parsed || typeof parsed !== 'object') return []
    const candidates = [
      parsed.path,
      parsed.file,
      parsed.filepath,
      parsed.relative_path,
      parsed.filename,
      parsed.target_path,
    ]
    const found: string[] = []
    for (const candidate of candidates) {
      if (typeof candidate === 'string' && candidate.trim()) found.push(candidate.trim())
    }
    return found
  } catch {
    return []
  }
}

function flowStatus(value: unknown): FlowNodeStatus {
  switch (value) {
    // The step a task is executing.
    case 'current': return 'running'
    case 'suspended': return 'suspended'
    case 'needs_user': return 'needs_user'
    case 'cancelled': return 'cancelled'
    case 'pending': return 'pending'
    case 'ready':
    case 'done':
    case 'running':
    case 'waiting_children':
    case 'paused':
    case 'blocked':
    case 'completed':
    case 'skipped':
    case 'interrupted':
    case 'failed':
    case 'deprecated':
      return value
    default:
      return 'ready'
  }
}

/** Task graph node kinds: the conversation, main and temporary tasks, their steps and finish nodes. */
function taskNodeKind(data: Record<string, unknown>): string {
  const metadata = (data.metadata ?? {}) as Record<string, unknown>
  switch (data.kind) {
    case 'root': return 'task_root'
    case 'task': return data.executor === 'delegate' ? 'task_delegate' : metadata.role === 'temp' ? 'task_temp' : 'task_main'
    case 'branch': return 'task_temp'
    case 'step': return 'task_step'
    case 'finish':
    case 'end': return 'task_finish'
    default: return data.executor === 'delegate' ? 'task_delegate' : 'task_planned'
  }
}

function createFlowNode(
  id: string,
  turn?: number,
  title = '任务',
  kind = 'step',
  description?: string,
  status: FlowNodeStatus = 'ready',
): {
  id: string
  title: string
  kind: string
  status: FlowNodeStatus
  description?: string | undefined
  response?: string | undefined
  ordinal?: number | undefined
  parentId?: string | undefined
  agentId?: string | undefined
  agentName?: string | undefined
  agentOrder?: number | undefined
  tools: FlowToolExecution[]
  consults: FlowObserverConsult[]
  files: Set<string>
  turn?: number | undefined
  step?: number | undefined
  revision?: number | undefined
  requestId?: number | undefined
  planRevision?: number | undefined
} {
  return {
    id,
    title,
    kind,
    status,
    description,
    tools: [],
    consults: [],
    files: new Set<string>(),
    turn,
  }
}

export function flowState(events: readonly AgentEvent[]): FlowState {
  events = events.map(event => event.type === 'subagent/event'
    ? { ...event, type: String(event.data.event_type), data: { agent_id: event.data.agent_id, ...event.data.event as Record<string, unknown> } }
    : event)
  // Step results reference original response events, including execution drafts.
  const responsesBySeq = new Map<number, string>()
  const responsesByStep = new Map<string, string>()
  for (const event of events) {
    if (event.type !== 'assistant/message' && event.type !== 'assistant/draft') continue
    const message = messageOf(event)
    const text = assistantDisplay(textOf(message.content), '').text
    if (event.seq !== undefined) responsesBySeq.set(event.seq, text)
    const step = stringOf(event.data.task_node_id)
    if (step) responsesByStep.set(step, text)
  }
  let currentTurn = 1
  let taskLabel = ''
  let observerEnabled: boolean | undefined
  const observerNotes: FlowObserverNote[] = []
  const nodeReviews = new Map<string, FlowNodeReview>()
  let observerRetrospective: FlowObserverRetrospective | undefined
  const nodeKey = (id: string, turn = currentTurn) => 'turn_' + turn + ':' + id
  const nodeMap = new Map<string, ReturnType<typeof createFlowNode>>()
  const edges: FlowEdgeData[] = []
  const callsToNode = new Map<string, string>()
  const delegates = new Map<string, { id: string; name: string; order: number }>()
  const delegatesByParent = new Map<string, string>()
  const delegateCounts = new Map<number, number>()
  let activeNodeId: string | undefined
  for (const event of events) {
    const data = event.data || {}
    const turn = numberOf(data.turn) ?? currentTurn
    switch (event.type) {
      case 'task/node_planned':
      case 'task/node_revised':
      case 'task/node_started':
      case 'task/node_suspended':
      case 'task/node_resumed':
      case 'task/node_returned':
      case 'task/node_reviewed': {
        const rawId = stringOf(data.node_id)
        if (!rawId) break
        const id = `task:${rawId}`
        const node = nodeMap.get(id) ?? createFlowNode(id, turn)
        node.title = stringOf(data.title) ?? node.title
        node.kind = taskNodeKind(data)
        const result = data.result as Record<string, unknown> | undefined
        node.description = stringOf(result?.result) ?? stringOf(data.goal) ?? node.description
        node.ordinal = numberOf(data.ordinal) ?? node.ordinal
        if (node.kind === 'task_step') {
          const response = result?.response as Record<string, unknown> | undefined
          const seq = numberOf(response?.event_seq)
          node.response = (seq !== undefined ? responsesBySeq.get(seq) : undefined) ?? responsesByStep.get(rawId)
            ?? stringOf(result?.result) ?? node.response // Older steps saved the AI's own result text.
        }
        node.status = flowStatus(data.status)
        node.agentId = stringOf(data.agent_id)
        const parent = stringOf(data.parent_id)
        node.parentId = parent ? `task:${parent}` : undefined
        nodeMap.set(id, node)
        if (parent && !edges.some(e => e.id === `task-edge:${rawId}`)) edges.push({ id: `task-edge:${rawId}`, source: `task:${parent}`, target: id, label: stringOf(data.spawned_from), animated: false })
        if (data.executor === 'inline' && node.status === 'running') activeNodeId = id
        break
      }
      case 'subagent/start': {
        const id = stringOf(data.agent_id)
        if (!id || delegates.has(id)) break
        const order = delegateCounts.get(turn) ?? 0
        delegates.set(id, { id, name: stringOf(data.name) ?? '任务分支', order })
        delegateCounts.set(turn, order + 1)
        const parent = stringOf(data.parent_node_id)
        if (parent) delegatesByParent.set(nodeKey(parent, turn), id)
        break
      }
      case 'turn/start':
        currentTurn = Math.max(currentTurn, turn)
        activeNodeId = undefined
        break
      case 'user/message': {
        currentTurn = Math.max(currentTurn, turn)
        const content = textOf(messageOf(event).content)
        if (content.trim()) taskLabel = content.trim()
        break
      }
      case 'observer/node_review': {
        const id = stringOf(data.review_id)
        if (!id) break
        const identity = (data.identity ?? {}) as FlowNodeReview['identity']
        nodeReviews.set(id, { id, identity, turn, stage: stringOf(data.stage) ?? 'handoff', status: stringOf(data.status) ?? 'unassessed',
          sourceEventId: stringOf(data.source_event_id) ?? '', decision: (data.decision ?? {}) as Record<string, unknown>,
          delivery: (data.delivery ?? {}) as Record<string, unknown>, result: (data.result ?? {}) as Record<string, unknown>, responses: nodeReviews.get(id)?.responses ?? [] })
        break
      }
      case 'observer/advice_response': {
        const advice = (data.advice ?? {}) as Record<string, unknown>
        const id = stringOf(advice.review_id)
        const review = id ? nodeReviews.get(id) : undefined
        if (review) nodeReviews.set(review.id, { ...review, responses: [...review.responses, { actor: data.actor ?? 'worker-legacy', ...advice }] })
        break
      }
      case 'observer/config':
        observerEnabled = data.enabled === true
        break
      case 'observer/plan_review':
      case 'observer/progress_review':
        currentTurn = Math.max(currentTurn, turn)
        observerNotes.push({
          kind: event.type === 'observer/plan_review' ? 'plan' : 'progress',
          turn,
          nodeId: stringOf(data.nodeId),
          summary: stringOf(data.summary) ?? '',
          suggestions: observerStringList(data.suggestions),
          unavailable: data.status === 'unavailable' ? stringOf(data.message) ?? '观察者暂时不可用' : undefined,
        })
        break
      case 'observer/retrospective_start':
        currentTurn = Math.max(currentTurn, turn)
        observerRetrospective = {
          status: 'reviewing',
          outcome: stringOf(data.outcome),
          shorteningOpportunities: [],
          workFindings: [],
          routeShortcuts: [],
        }
        break
      case 'observer/retrospective': {
        currentTurn = Math.max(currentTurn, turn)
        const findings: FlowObserverFinding[] = Array.isArray(data.workFindings)
          ? data.workFindings.flatMap(item => {
              if (!item || typeof item !== 'object') return []
              const finding = item as Record<string, unknown>
              const text = stringOf(finding.finding)
              return text ? [{
                finding: text,
                relevance: stringOf(finding.relevance) ?? '',
                watchFor: stringOf(finding.watch_for) ?? '',
                sourceRefs: observerStringList(finding.source_refs ?? finding.sourceRefs),
                certainty: finding.certainty === 'confirmed' ? 'confirmed' as const : 'uncertain' as const,
              }] : []
            })
          : []
        const routeShortcuts: FlowObserverRouteShortcut[] = Array.isArray(data.routeShortcuts)
          ? data.routeShortcuts.flatMap(item => {
              if (!item || typeof item !== 'object') return []
              const route = item as Record<string, unknown>
              const situation = stringOf(route.situation)
              return situation ? [{
                situation,
                lookFirst: stringOf(route.lookFirst) ?? '',
                avoid: stringOf(route.avoid) ?? '',
              }] : []
            })
          : []
        observerRetrospective = {
          status: data.status === 'unavailable' ? 'unavailable' : 'completed',
          outcome: stringOf(data.outcome),
          summary: stringOf(data.summary),
          pathReview: stringOf(data.pathReview),
          shorteningOpportunities: observerStringList(data.shorteningOpportunities),
          workFindings: findings,
          routeShortcuts,
          memoryRecorded: data.memoryRecorded === true,
          memorySummary: stringOf(data.memorySummary),
          memoryError: stringOf(data.memoryError),
          message: stringOf(data.message),
        }
        break
      }
      case 'flow/node_state': {
        currentTurn = Math.max(currentTurn, turn)
        const rawId = stringOf(data.id)
        const id = rawId ? nodeKey(rawId, turn) : undefined
        const node = id ? nodeMap.get(id) : undefined
        if (node && data.status) {
          node.status = flowStatus(data.status)
          if (node.status === 'running') activeNodeId = id
        }
        break
      }
      case 'flow/node_add': {
        currentTurn = Math.max(currentTurn, turn)
        const rawNode = data.node as Record<string, unknown> | undefined
        if (rawNode && typeof rawNode === 'object') {
          const rawId = stringOf(rawNode.id) ?? ('node_' + (nodeMap.size + 1))
          const id = nodeKey(rawId, turn)
          const node = nodeMap.get(id) ?? createFlowNode(id, turn)
          node.title = stringOf(rawNode.title) ?? node.title
          node.kind = stringOf(rawNode.kind) ?? node.kind
          node.description = stringOf(rawNode.description) ?? node.description
          node.status = flowStatus(rawNode.status ?? node.status)
          const parent = stringOf(rawNode.parent_id)
          const taskParent = stringOf(rawNode.task_node_id)
          node.parentId = taskParent ? `task:${taskParent}` : parent ? nodeKey(parent, turn) : node.parentId
          node.agentId = stringOf(rawNode.agent_id) ?? stringOf(data.agent_id)
            ?? (node.kind === 'worker' && node.parentId ? delegatesByParent.get(node.parentId) : undefined)
            ?? node.agentId
          const delegate = node.agentId ? delegates.get(node.agentId) : undefined
          node.agentName = delegate?.name ?? node.agentName
          node.agentOrder = delegate?.order ?? node.agentOrder
          node.requestId = numberOf(rawNode.request_id) ?? node.requestId
          node.revision = numberOf(rawNode.revision) ?? node.revision
          node.planRevision = numberOf(rawNode.plan_revision) ?? node.planRevision
          nodeMap.set(id, node)
          if (node.status === 'running') activeNodeId = id
        }
        for (const raw of Array.isArray(data.edges) ? data.edges : []) {
          if (!raw || typeof raw !== 'object') continue
          const edge = raw as Record<string, unknown>
          const source = stringOf(edge.source)
          const target = stringOf(edge.target)
          if (!source || !target) continue
          const id = nodeKey(stringOf(edge.id) ?? ('e_' + source + '_' + target), turn)
          if (!edges.some(item => item.id === id)) {
            edges.push({
              id,
              source: nodeKey(source, turn),
              target: nodeKey(target, turn),
              label: stringOf(edge.label),
              animated: false,
            })
          }
        }
        break
      }
      case 'observer/consult': {
        currentTurn = Math.max(currentTurn, turn)
        const question = stringOf(data.question)
        if (question) {
          const targetNode = (activeNodeId && nodeMap.get(activeNodeId)) || Array.from(nodeMap.values()).at(-1)
          targetNode?.consults.push({ question })
        }
        break
      }
      case 'observer/consult_reply': {
        currentTurn = Math.max(currentTurn, turn)
        const answer = stringOf(data.answer)
        if (answer) {
          const targetNode = (activeNodeId && nodeMap.get(activeNodeId)) || Array.from(nodeMap.values()).at(-1)
          const last = targetNode?.consults[targetNode.consults.length - 1]
          if (last && !last.answer) (last as { answer?: string }).answer = answer
        }
        break
      }
      case 'context/tool_call':
      case 'tool/call': {
        currentTurn = Math.max(currentTurn, turn)
        const callId = stringOf(data.callId) ?? ''
        const name = stringOf(data.name) ?? ''
        const argsStr = argumentsOf(data.arguments)
        const flowNodeId = stringOf(data.flowNodeId)
        const targetId = flowNodeId
          ? nodeKey(flowNodeId, turn)
          : activeNodeId ?? nodeKey('agent', turn)
        if (callId) callsToNode.set(`${stringOf(data.agent_id) ?? 'main'}:${callId}`, targetId)
        let targetNode = nodeMap.get(targetId)
        if (!targetNode) {
          targetNode = createFlowNode(targetId, turn, taskLabel || '执行任务', 'agent', taskLabel, 'running')
          nodeMap.set(targetId, targetNode)
        }
        targetNode.tools.push({ id: callId, name, input: argsStr })
        for (const file of extractFilesFromArgs(argsStr)) targetNode.files.add(file)
        activeNodeId = targetId
        break
      }
      case 'context/tool_result':
      case 'tool/result': {
        const callId = callIdOfResult(event) ?? ''
        const flowNodeId = stringOf(data.flowNodeId)
        const targetId = flowNodeId ? nodeKey(flowNodeId, turn) : callsToNode.get(`${stringOf(data.agent_id) ?? 'main'}:${callId}`) ?? activeNodeId
        const targetNode = targetId ? nodeMap.get(targetId) : undefined
        if (targetNode) {
          const result = resultOf(event)
          const tool = targetNode.tools.find(item => item.id === callId)
          if (tool) {
            ;(tool as { output?: string }).output = result.text
            ;(tool as { failed?: boolean }).failed = result.isError
            if (result.durationMs !== undefined) {
              ;(tool as { durationMs?: number }).durationMs = result.durationMs
            }
          }
        }
        break
      }
      case 'turn/end': {
        currentTurn = Math.max(currentTurn, turn)
        const reason = reasonOf(data.reason)
        for (const node of nodeMap.values()) {
          if (node.turn !== turn) continue
          if (node.id.startsWith('task:')) continue
          if (node.status === 'running') {
            node.status = reason.kind === 'error' ? 'failed'
              : reason.kind === 'aborted' ? 'interrupted' : 'done'
          } else if (reason.kind === 'completed' && node.status === 'ready') {
            node.status = 'skipped'
          }
        }
        activeNodeId = undefined
        break
      }
    }
  }
  if (nodeMap.size === 0) {
    const ended = [...events].reverse().find(event => event.type === 'turn/end')
    const reason = ended ? reasonOf(ended.data.reason) : undefined
    const status: FlowNodeStatus = reason?.kind === 'completed' ? 'done'
      : reason?.kind === 'error' ? 'failed' : reason?.kind === 'aborted' ? 'interrupted'
        : events.some(event => event.type === 'turn/start') ? 'running' : 'ready'
    const id = nodeKey('agent', currentTurn)
    nodeMap.set(id, createFlowNode(id, currentTurn, taskLabel || 'Agent 任务', 'agent', taskLabel, status))
    if (status === 'running') activeNodeId = id
  }
  const nodes: FlowNodeData[] = [...nodeMap.values()].map(node => ({ ...node, files: [...node.files] }))
  for (const node of nodes) {
    if (!node.id.startsWith('task:')) continue
    const requests = nodes.filter(request => request.parentId === node.id && !request.id.startsWith('task:'))
    ;(node as { tools: readonly FlowToolExecution[] }).tools = [...node.tools, ...requests.flatMap(request => request.tools)]
  }
  const activeTask = nodes.find(node => node.id.startsWith('task:') && node.status === 'running' && !node.agentId)
  if (activeTask) activeNodeId = activeTask.id
  const nodesById = new Map(nodes.map(node => [node.id, node]))
  const labeledEdges = edges.map(edge => {
    const target = nodesById.get(edge.target)
    if (target?.parentId !== edge.source) return edge
    if (target.kind === 'worker') return { ...edge, label: target.agentName ? `派发 · ${target.agentName}` : '派发任务' }
    if (target.kind === 'context_agent') return { ...edge, label: '上下文压缩' }
    return edge
  })
  return { nodes, edges: labeledEdges, activeNodeId, observerEnabled, nodeReviews: [...nodeReviews.values()], observerNotes, observerRetrospective, organizedTurn: currentTurn }
}
