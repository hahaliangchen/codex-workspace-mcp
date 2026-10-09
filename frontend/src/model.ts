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
  | { readonly kind: 'turn'; readonly key: string; readonly turn: number }
  | { readonly kind: 'user'; readonly key: string; readonly text: string; readonly time: number }
  | { readonly kind: 'assistant'; readonly key: string; readonly text: string; readonly reasoning: string; readonly time: number; readonly streaming: boolean; readonly turn: number; readonly step: number }
  | { readonly kind: 'progress'; readonly key: string; readonly purpose: string; readonly knownConditions: readonly string[]; readonly nextAction: string; readonly time: number }
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
  | { readonly kind: 'subagent'; readonly key: string; readonly childTaskId: string; readonly prompt: string }
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
  const results = new Map<string, ToolResult>()
  const assessments = new Map<number, { goalAchieved: boolean; unresolved: readonly string[] }>()
  for (const event of events) {
    if (event.type === 'organizer/decision') {
      const decision = event.data.decision as Record<string, unknown> | undefined
      if (decision && (decision.action === 'finish' || decision.action === 'blocked')) {
        assessments.set(numberOf(event.data.turn) ?? 1, {
          goalAchieved: typeof decision.achieved === 'boolean' ? decision.achieved : decision.action === 'finish',
          unresolved: Array.isArray(decision.unresolved) ? decision.unresolved.filter((item): item is string => typeof item === 'string') : [],
        })
      }
    }
    if (event.type !== 'tool/result') continue
    const id = callIdOfResult(event)
    if (id !== undefined) results.set(id, resultOf(event))
  }
  const items: ChatItem[] = []
  let turnStartTime: number | undefined
  let currentTurn = 1

  for (const event of events) {
    const key = `${event.type}:${event.seq}`
    switch (event.type) {
      case 'turn/start': {
        turnStartTime = event.time
        const turn = numberOf(event.data.turn) ?? 1
        currentTurn = turn
        if (turn > 1) items.push({ kind: 'turn', key, turn })
        break
      }
      case 'user/message': {
        if (turnStartTime === undefined) {
          turnStartTime = event.time
        }
        const text = textOf(messageOf(event).content)
        if (text !== '') items.push({ kind: 'user', key, text, time: event.time })
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
        const text = textOf(content)
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
      case 'worker/progress': {
        const knownConditions = Array.isArray(event.data.knownConditions)
          ? event.data.knownConditions.filter((value): value is string => typeof value === 'string')
          : []
        items.push({
          kind: 'progress', key,
          purpose: stringOf(event.data.purpose) ?? '',
          knownConditions,
          nextAction: stringOf(event.data.nextAction) ?? '',
          time: event.time,
        })
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
        const text = [stringOf(report.summary) ?? stringOf(event.data.summary),
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
      case 'subagent/start': {
        const childTaskId = stringOf(event.data.child_task_id)
        if (childTaskId !== undefined) {
          items.push({ kind: 'subagent', key, childTaskId, prompt: stringOf(event.data.prompt) ?? '' })
        }
        break
      }
      case 'turn/end': {
        const durationMs = turnStartTime !== undefined ? Math.max(0, event.time - turnStartTime) : undefined
        const reason = reasonOf(event.data.reason)
        const assessment = assessments.get(numberOf(event.data.turn) ?? currentTurn)
        items.push({
          kind: 'turn-end',
          key,
          reason: reason.kind === 'completed' ? { ...reason,
            goalAchieved: reason.goalAchieved ?? assessment?.goalAchieved,
            unresolved: reason.unresolved ?? assessment?.unresolved } : reason,
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
      reason: task.status === 'completed' ? { kind: 'completed', ...assessments.get(currentTurn) } : { kind: 'aborted' },
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
    if (event.type === 'tool/call') calls.set(stringOf(event.data.callId) ?? '', event)
  }
  return events.filter((event) => event.type !== 'assistant/delta' && event.type !== 'observer/config' && !event.type.startsWith('debug/')
    && (event.type !== 'organizer/progress' || terminalOrganizerPhases.has(String(event.data.phase)))).map((event) => {
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
      case 'tool/call':
        return { ...base, name: stringOf(data.name), detail: argumentsOf(data.arguments) }
      case 'organizer/progress': {
        const phase = stringOf(data.phase) ?? ''
        const progress: FlowOrganizerProgress = { phase, turn: numberOf(data.turn) ?? 0, time: event.time, elapsedMs: numberOf(data.elapsed_ms),
          responseHeadersMs: numberOf(data.response_headers_ms), firstChunkMs: numberOf(data.first_chunk_ms), firstDeltaMs: numberOf(data.first_delta_ms),
          firstToolDeltaMs: numberOf(data.first_tool_delta_ms), receivedBytes: numberOf(data.received_bytes) }
        return { ...base, name: organizerPhaseLabel(phase), detail: organizerProgressTimings(progress), durationMs: progress.elapsedMs, failed: phase !== 'completed' }
      }
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
      case 'worker/work_state': {
        const state = typeof data.state === 'object' && data.state !== null ? data.state as Record<string, unknown> : {}
        const files = typeof state.files === 'object' && state.files !== null ? Object.keys(state.files) : []
        return { ...base, name: '工作记录', detail: [
          `已读取：${files.join(', ')}`, ...observerStringList(state.open_questions).map(question => `待确认：${question}`),
          `下一步：${stringOf(state.next_action) ?? ''}`,
        ].join('\n') }
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
        return { ...base, name: '复盘中', detail: 'Worker 已结束，Observer 正在回看完整工作路径。' }
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
      case 'subagent/start':
        return { ...base, detail: stringOf(data.prompt) ?? '', childTaskId: stringOf(data.child_task_id) }
      case 'subagent/end':
        return { ...base, detail: stringOf(data.status) ?? '', childTaskId: stringOf(data.child_task_id) }
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
export type FlowNodeStatus = 'ready' | 'running' | 'done' | 'waiting_children' | 'pending' | 'paused' | 'blocked' | 'completed' | 'skipped' | 'interrupted' | 'failed' | 'deprecated'

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
  return review.identity.work_id === (node.workUnit?.id ?? raw) &&
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

export interface FlowProgressReport {
  readonly findings: readonly string[]
  readonly openQuestions: readonly string[]
  readonly observerResponses: readonly string[]
  readonly step?: number | undefined
  readonly purpose: string
  readonly knownConditions: readonly string[]
  readonly nextAction: string
}

export interface FlowWorkUnit {
  readonly id: string
  readonly nodeId?: string | undefined
  readonly status: 'ready' | 'running' | 'done'
  readonly done: boolean
  readonly goal: string
  readonly doneWhen: string
  readonly upstreamIds: readonly string[]
  readonly checks: readonly string[]
  readonly completedChecks: readonly string[]
  readonly failedChecks?: readonly string[] | undefined
  readonly outputSummary?: string | undefined
  readonly outcome?: string | undefined
  readonly expectationMet?: boolean | undefined
}

export interface FlowRewindRecord {
  readonly id: string
  readonly sourceNode: string
  readonly sourceWorkId?: string | undefined
  readonly sourceRevision?: number | undefined
  readonly targetNode: string
  readonly targetWorkId?: string | undefined
  readonly targetRevision?: number | undefined
  readonly reason: string
  readonly timestamp?: number | undefined
  readonly planRevision?: number | undefined
  readonly invalidatedTasks?: readonly string[] | undefined
}

export interface FlowNodeData {
  [key: string]: unknown
  readonly id: string
  readonly title: string
  readonly kind: FlowNodeKind
  readonly status: FlowNodeStatus
  readonly description?: string | undefined
  readonly parentId?: string | undefined
  readonly isMainTask?: boolean | undefined
  readonly objective?: string | undefined
  readonly doneWhen?: string | undefined
  readonly workUnit?: FlowWorkUnit | undefined
  readonly constraints?: readonly string[] | undefined
  readonly result?: { readonly summary: string; readonly materialIds: readonly number[]; readonly outcome?: string | undefined; readonly goalAchieved?: boolean | undefined; readonly expectationMet?: boolean | undefined; readonly limitations?: readonly string[] | undefined } | undefined
  readonly progress: readonly FlowProgressReport[]
  readonly tools: readonly FlowToolExecution[]
  readonly consults: readonly FlowObserverConsult[]
  readonly files: readonly string[]
  readonly turn?: number | undefined
  readonly step?: number | undefined
  readonly revision?: number | undefined
  readonly requestId?: number | undefined
  readonly planRevision?: number | undefined
  readonly invalidatedByPlanRevision?: number | undefined
}

export interface FlowEdgeData {
  readonly id: string
  readonly source: string
  readonly target: string
  readonly label?: string | undefined
  readonly animated?: boolean | undefined
  readonly rewind?: boolean | undefined
  readonly deprecated?: boolean | undefined
}

export type OrganizerPhase = 'waiting_response' | 'waiting_first_delta' | 'reasoning' | 'receiving_decision' | 'completed' | 'failed' | 'cancelled' | 'timeout'

const organizerPhaseLabels: Record<OrganizerPhase, string> = {
  waiting_response: '等待响应',
  waiting_first_delta: '已连接，等待首个增量',
  reasoning: '正在推理',
  receiving_decision: '正在接收决策',
  completed: '决策已完成',
  failed: '决策请求失败',
  cancelled: '已取消',
  timeout: '请求超时',
}

const terminalOrganizerPhases = new Set<string>(['completed', 'failed', 'cancelled', 'timeout'])

export function organizerPhaseLabel(phase: string): string {
  return organizerPhaseLabels[phase as OrganizerPhase] ?? phase
}

/** Latest Organizer request state; all times are milliseconds after the request started, null when not received. */
export interface FlowOrganizerProgress {
  readonly phase: string
  readonly turn: number
  readonly step?: number | undefined
  readonly nodeId?: string | undefined
  readonly time: number
  readonly elapsedMs?: number | undefined
  readonly responseHeadersMs?: number | undefined
  readonly firstChunkMs?: number | undefined
  readonly firstDeltaMs?: number | undefined
  readonly firstToolDeltaMs?: number | undefined
  readonly receivedBytes?: number | undefined
  readonly reasoningChars?: number | undefined
  readonly toolArgumentBytes?: number | undefined
}

export function organizerProgressTimings(progress: FlowOrganizerProgress): string {
  const ms = (label: string, value: number | undefined) => `${label} ${value === undefined ? '—' : `${value}ms`}`
  return [ms('响应头', progress.responseHeadersMs), ms('首块', progress.firstChunkMs), ms('首增量', progress.firstDeltaMs),
    ms('首个工具增量', progress.firstToolDeltaMs), ms('已用', progress.elapsedMs),
    `接收 ${progress.receivedBytes ?? 0} 字节`].join(' · ')
}

export interface FlowState {
  readonly organizerProgress?: FlowOrganizerProgress | undefined
  readonly nodeReviews: readonly FlowNodeReview[]
  readonly currentWork?: FlowWorkUnit | undefined
  readonly nodes: readonly FlowNodeData[]
  readonly edges: readonly FlowEdgeData[]
  readonly activeNodeId?: string | undefined
  readonly observerEnabled?: boolean | undefined
  readonly observerNotes: readonly FlowObserverNote[]
  readonly observerRetrospective?: FlowObserverRetrospective | undefined
  readonly organizedMode: 'direct' | 'dag' | 'tree'
  readonly organizedTurn: number
  readonly activePath: readonly string[]
  readonly unfinishedTree: boolean
  readonly planRevision?: number | undefined
  readonly rewindRecords?: readonly FlowRewindRecord[] | undefined
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
  parentId?: string | undefined
  isMainTask?: boolean | undefined
  objective?: string | undefined
  doneWhen?: string | undefined
  workUnit?: FlowWorkUnit | undefined
  constraints?: string[] | undefined
  result?: { summary: string; materialIds: number[]; outcome?: string | undefined; goalAchieved?: boolean | undefined; expectationMet?: boolean | undefined; limitations?: readonly string[] | undefined } | undefined
  progress: FlowProgressReport[]
  tools: FlowToolExecution[]
  consults: FlowObserverConsult[]
  files: Set<string>
  turn?: number | undefined
  step?: number | undefined
  revision?: number | undefined
  requestId?: number | undefined
  planRevision?: number | undefined
  invalidatedByPlanRevision?: number | undefined
} {
  return {
    id,
    title,
    kind,
    status,
    description,
    progress: [],
    tools: [],
    consults: [],
    files: new Set<string>(),
    turn,
  }
}

export function flowState(events: readonly AgentEvent[]): FlowState {
  let currentWork: FlowWorkUnit | undefined
  let currentTurn = 1
  let currentPlanRevision = 1
  const rewindRecordsMap = new Map<string, FlowRewindRecord>()
  let taskLabel = ''
  let observerEnabled: boolean | undefined
  const observerNotes: FlowObserverNote[] = []
  const nodeReviews = new Map<string, FlowNodeReview>()
  let observerRetrospective: FlowObserverRetrospective | undefined
  let organizerProgress: FlowOrganizerProgress | undefined
  const nodeKey = (id: string, turn = currentTurn) => 'turn_' + turn + ':' + id
  const nodeMap = new Map<string, ReturnType<typeof createFlowNode>>()
  const edges: FlowEdgeData[] = []
  const callsToNode = new Map<string, string>()
  const nodeToWorkMap = new Map<string, string>()
  let activeNodeId: string | undefined
  let organizedMode: FlowState['organizedMode'] = 'direct'
  let activePath: string[] = []
  let unfinishedTree = false
  const treeTurns = new Set<number>()
  const treeFields = (node: ReturnType<typeof createFlowNode>, raw: Record<string, unknown>, turn: number) => {
    node.isMainTask = raw.is_main_task === true || node.isMainTask
    node.requestId = numberOf(raw.request_id) ?? node.requestId
    const parent = stringOf(raw.parent_id)
    if (parent) {
      const mappedParent = nodeToWorkMap.get(nodeKey(parent, turn)) ?? parent
      node.parentId = nodeKey(mappedParent, turn)
    }
    node.objective = stringOf(raw.objective) ?? node.objective
    node.doneWhen = stringOf(raw.done_when) ?? node.doneWhen
    node.constraints = observerStringList(raw.constraints)
    const result = raw.result as Record<string, unknown> | null | undefined
    node.result = result && typeof result.summary === 'string' ? { summary: result.summary,
      materialIds: Array.isArray(result.material_ids) ? result.material_ids.filter((id): id is number => typeof id === 'number') : [],
      outcome: stringOf(result.outcome),
      goalAchieved: typeof result.goal_achieved === 'boolean' ? result.goal_achieved : undefined,
      expectationMet: typeof result.expectation_met === 'boolean' ? result.expectation_met : undefined,
      limitations: observerStringList(result.limitations) } : undefined
  }

  const flowEvents: readonly AgentEvent[] = events.flatMap(event => {
    if (event.type !== 'flow/session') return [event]
    const data = event.data
    const state = data.scheduler as Record<string, unknown> | undefined
    const plan = data.flow_plan as Record<string, unknown> | undefined
    const planNodes = (Array.isArray(plan?.nodes) ? plan.nodes : []).map((raw: Record<string, unknown>) => ({ ...raw,
      status: raw.status === 'running' && (raw.work_id === state?.current || raw.is_main_task === true) ? 'paused' : raw.status,
    }))
    return [
      { ...event, type: 'scheduler/state', data: { turn: data.turn, state } },
      { ...event, type: 'flow/plan', data: { ...plan, nodes: planNodes, turn: data.turn, active_node_id: '', replaceCurrentTurn: true } },
    ]
  })
  for (const event of flowEvents) {
    const data = event.data || {}
    const turn = numberOf(data.turn) ?? currentTurn
    switch (event.type) {
      case 'turn/start':
        currentTurn = Math.max(currentTurn, turn)
        currentWork = undefined
        organizerProgress = undefined
        organizedMode = 'direct'; activePath = []; unfinishedTree = false
        break
      case 'organizer/start':
        organizerProgress = { phase: 'waiting_response', turn, step: numberOf(data.step), nodeId: stringOf(data.nodeId), time: event.time }
        break
      case 'organizer/progress':
        organizerProgress = {
          phase: stringOf(data.phase) ?? 'waiting_response', turn, step: numberOf(data.step), nodeId: stringOf(data.nodeId), time: event.time,
          elapsedMs: numberOf(data.elapsed_ms), responseHeadersMs: numberOf(data.response_headers_ms), firstChunkMs: numberOf(data.first_chunk_ms),
          firstDeltaMs: numberOf(data.first_delta_ms), firstToolDeltaMs: numberOf(data.first_tool_delta_ms), receivedBytes: numberOf(data.received_bytes),
          reasoningChars: numberOf(data.reasoning_chars), toolArgumentBytes: numberOf(data.tool_argument_bytes),
        }
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
                sourceRefs: observerStringList(finding.sourceRefs),
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
      case 'flow/plan': {
        currentTurn = Math.max(currentTurn, turn)
        organizedMode = data.mode === 'tree' ? 'tree' : data.mode === 'direct' ? 'direct' : 'dag'
        if (organizedMode === 'tree') treeTurns.add(turn)
        activePath = observerStringList(data.active_path).map(id => nodeKey(id, turn))
        if (typeof data.plan_revision === 'number') {
          currentPlanRevision = data.plan_revision
        }
        if (Array.isArray(data.rewind_records)) {
          for (const raw of data.rewind_records) {
            if (raw && typeof raw === 'object') {
              const rec = raw as Record<string, unknown>
              const id = stringOf(rec.id) ?? ''
              if (id && !rewindRecordsMap.has(id)) {
                rewindRecordsMap.set(id, {
                  id,
                  sourceNode: stringOf(rec.source_node) ?? '',
                  sourceWorkId: stringOf(rec.source_work_id),
                  sourceRevision: numberOf(rec.source_revision) ?? 1,
                  targetNode: stringOf(rec.target_node) ?? '',
                  targetWorkId: stringOf(rec.target_work_id),
                  targetRevision: numberOf(rec.target_revision) ?? 1,
                  reason: stringOf(rec.reason) ?? '',
                  timestamp: numberOf(rec.timestamp) ?? 0,
                  planRevision: numberOf(rec.plan_revision) ?? 1,
                  invalidatedTasks: observerStringList(rec.invalidated_tasks),
                })
              }
            }
          }
        }
        const rawNodes = Array.isArray(data.nodes) ? data.nodes : []
        const plannedIds = new Set(rawNodes
          .filter((raw): raw is Record<string, unknown> => !!raw && typeof raw === 'object')
          .map(raw => stringOf(raw.id) ?? stringOf(raw.work_id))
          .filter((id): id is string => !!id))
        const prefix = 'turn_' + turn + ':'
        if (data.replaceCurrentTurn === true) {
          for (const [id, node] of nodeMap) {
            if (node.turn === turn && !plannedIds.has(id.slice(prefix.length))) {
              if (node.status !== 'deprecated') {
                nodeMap.delete(id)
              }
            }
          }
          for (let index = edges.length - 1; index >= 0; index -= 1) {
            if (edges[index]?.id.startsWith(prefix) && !edges[index]?.deprecated && !edges[index]?.rewind) {
              edges.splice(index, 1)
            }
          }
          if (activeNodeId?.startsWith(prefix) && !plannedIds.has(activeNodeId.slice(prefix.length))) {
            activeNodeId = undefined
          }
        } else if (data.replaceEdgesCurrentTurn === true) {
          for (let index = edges.length - 1; index >= 0; index -= 1) {
            if (edges[index]?.id.startsWith(prefix) && !edges[index]?.deprecated && !edges[index]?.rewind) {
              edges.splice(index, 1)
            }
          }
        }
        for (const raw of rawNodes) {
          if (!raw || typeof raw !== 'object') continue
          const rawNode = raw as Record<string, unknown>
          const rawId = stringOf(rawNode.id) ?? stringOf(rawNode.work_id) ?? ('node_' + (nodeMap.size + 1))
          const rawNodeId = stringOf(rawNode.node_id)
          const isDeprecated = rawNode.status === 'deprecated' || (rawNode.invalidated_by_plan_revision !== null && rawNode.invalidated_by_plan_revision !== undefined)
          if (rawNodeId && rawId) {
            const key = nodeKey(rawNodeId, turn)
            if (!nodeToWorkMap.has(key) || !isDeprecated) {
              nodeToWorkMap.set(key, rawId)
            }
          }
          const id = nodeKey(rawId, turn)
          const previous = nodeMap.get(id)
          const node = previous ?? createFlowNode(id, turn)
          node.title = stringOf(rawNode.title) ?? node.title
          node.kind = stringOf(rawNode.kind) ?? node.kind
          node.description = stringOf(rawNode.description) ?? node.description
          node.status = flowStatus(rawNode.status ?? node.status)
          node.revision = numberOf(rawNode.revision) ?? node.revision
          node.planRevision = numberOf(rawNode.plan_revision) ?? node.planRevision
          node.invalidatedByPlanRevision = numberOf(rawNode.invalidated_by_plan_revision) ?? node.invalidatedByPlanRevision
          treeFields(node, rawNode, turn)
          nodeMap.set(id, node)
          if (node.status === 'running') activeNodeId = id
        }
        const rawEdges = Array.isArray(data.edges) ? data.edges : []
        for (const raw of rawEdges) {
          if (!raw || typeof raw !== 'object') continue
          const edge = raw as Record<string, unknown>
          const source = stringOf(edge.source)
          const target = stringOf(edge.target)
          if (!source || !target) continue
          const id = nodeKey(stringOf(edge.id) ?? ('e_' + source + '_' + target), turn)
          const existing = edges.findIndex(item => item.id === id)
          const isRewind = edge.rewind === true
          const isDeprecated = edge.deprecated === true
          const nextEdge: FlowEdgeData = {
            id,
            source: nodeKey(source, turn),
            target: nodeKey(target, turn),
            label: stringOf(edge.label),
            animated: isRewind ? true : !isDeprecated,
            rewind: isRewind,
            deprecated: isDeprecated,
          }
          if (existing >= 0) edges[existing] = nextEdge
          else edges.push(nextEdge)
        }
        if (data.active_node_id !== undefined) activeNodeId = stringOf(data.active_node_id) ? nodeKey(String(data.active_node_id), turn) : undefined
        break
      }
      case 'organizer/assignment': {
        const order = data.order as Record<string, unknown> | undefined
        if (!order) break
        const id = nodeKey(stringOf(order.id) ?? stringOf(order.node_id) ?? 'direct', turn)
        const node = nodeMap.get(id) ?? createFlowNode(id, turn)
        node.title = (numberOf(order.revision) ?? 1) > 1 ? `${stringOf(order.goal)} (r${order.revision})` : (stringOf(order.goal) ?? node.title)
        node.objective = stringOf(order.goal)
        node.description = node.objective
        node.doneWhen = stringOf(order.done_when)
        node.constraints = observerStringList(order.constraints)
        node.revision = numberOf(order.revision) ?? node.revision
        node.planRevision = numberOf(order.plan_revision) ?? node.planRevision
        node.status = 'running'
        nodeMap.set(id, node); activeNodeId = id
        break
      }
      case 'scheduler/rewind': {
        const rewind = data.rewind as Record<string, unknown> | undefined
        if (rewind) {
          const id = stringOf(rewind.id) ?? ('rewind_' + rewindRecordsMap.size)
          rewindRecordsMap.set(id, {
            id,
            sourceNode: stringOf(rewind.source_node) ?? '',
            sourceWorkId: stringOf(rewind.source_work_id),
            sourceRevision: numberOf(rewind.source_revision) ?? 1,
            targetNode: stringOf(rewind.target_node) ?? '',
            targetWorkId: stringOf(rewind.target_work_id),
            targetRevision: numberOf(rewind.target_revision) ?? 1,
            reason: stringOf(rewind.reason) ?? '',
            timestamp: numberOf(rewind.timestamp) ?? event.time,
            planRevision: numberOf(rewind.plan_revision) ?? 1,
            invalidatedTasks: observerStringList(rewind.invalidated_tasks),
          })
        }
        break
      }
      case 'flow/request_archived': {
        const firstTurn = numberOf(data.started_turn) ?? turn - 1
        const ids = new Set(observerStringList(data.node_ids))
        const abandoned = new Set<string>()
        for (const [id, node] of nodeMap) {
          if (node.turn === undefined || node.turn < firstTurn || node.turn >= turn) continue
          if (!ids.has(id.slice(('turn_' + node.turn + ':').length))) continue
          node.status = 'deprecated'
          node.invalidatedByPlanRevision = numberOf(data.plan_revision)
          abandoned.add(id)
        }
        for (let index = 0; index < edges.length; index += 1) {
          const edge = edges[index]!
          if (abandoned.has(edge.source) || abandoned.has(edge.target)) {
            edges[index] = { ...edge, deprecated: true, animated: false }
          }
        }
        break
      }
      case 'scheduler/state': {
        const state = data.state as Record<string, unknown> | undefined
        if (typeof state?.plan_revision === 'number') {
          currentPlanRevision = state.plan_revision
        }
        const frames = state?.frames as Record<string, Record<string, unknown>> | undefined
        // F03: Select best workId for each nodeId avoiding deprecated/invalidated overwriting active
        const bestFramesForNode = new Map<string, { workId: string, revision: number, isInvalidated: boolean, sequence: number }>()
        for (const frame of Object.values(frames ?? {})) {
          const order = frame.order as Record<string, unknown> | undefined
          if (!order) continue
          const rawOrderId = stringOf(order.id)
          const rawNodeId = stringOf(order.node_id)
          if (!rawOrderId || !rawNodeId) continue
          const isInvalidated = frame.invalidated_by_plan_revision !== null && frame.invalidated_by_plan_revision !== undefined
          const revision = numberOf(order.revision) ?? 1
          const sequence = numberOf(frame.sequence) ?? 0
          const key = nodeKey(rawNodeId, turn)
          const existing = bestFramesForNode.get(key)
          if (!existing) {
            bestFramesForNode.set(key, { workId: rawOrderId, revision, isInvalidated, sequence })
          } else {
            if (existing.isInvalidated && !isInvalidated) {
              bestFramesForNode.set(key, { workId: rawOrderId, revision, isInvalidated, sequence })
            } else if (existing.isInvalidated === isInvalidated) {
              if (revision > existing.revision || (revision === existing.revision && sequence > existing.sequence)) {
                bestFramesForNode.set(key, { workId: rawOrderId, revision, isInvalidated, sequence })
              }
            }
          }
        }
        for (const [key, best] of bestFramesForNode.entries()) {
          nodeToWorkMap.set(key, best.workId)
        }
        for (const frame of Object.values(frames ?? {})) {
          const order = frame.order as Record<string, unknown> | undefined
          if (!order) continue
          const checked = frame.checked as Record<string, unknown> | undefined
          const output = frame.output as Record<string, unknown> | undefined
          const work: FlowWorkUnit = {
            id: stringOf(order.id) ?? '',
            nodeId: stringOf(order.node_id),
            status: frame.status === 'done' ? 'done' : frame.status === 'running' ? 'running' : 'ready',
            done: frame.status === 'done', goal: stringOf(order.goal) ?? '', doneWhen: stringOf(order.done_when) ?? '',
            upstreamIds: observerStringList(order.upstream_ids), checks: observerStringList(order.checks),
            completedChecks: Object.keys(checked ?? {}), outputSummary: stringOf(output?.summary),
            outcome: stringOf(output?.outcome), expectationMet: typeof output?.expectation_met === 'boolean' ? output.expectation_met : undefined,
            failedChecks: Object.keys((frame.check_errors as Record<string, unknown> | undefined) ?? {}),
          }
          if (order.id === state?.current) currentWork = work
          const id = nodeKey(stringOf(order.id) ?? stringOf(order.node_id) ?? 'direct', turn)
          const node = nodeMap.get(id) ?? createFlowNode(id, turn, work.goal, 'worker')
          node.workUnit = work
          node.requestId = numberOf(state?.request_started_turn) ?? node.requestId
          node.objective = work.goal
          node.description = work.goal
          node.doneWhen = work.doneWhen
          node.constraints = observerStringList(order.constraints)
          node.revision = numberOf(order.revision) ?? node.revision
          node.planRevision = numberOf(order.plan_revision) ?? node.planRevision
          node.invalidatedByPlanRevision = numberOf(frame.invalidated_by_plan_revision) ?? node.invalidatedByPlanRevision
          const isInvalidated = frame.invalidated_by_plan_revision !== null && frame.invalidated_by_plan_revision !== undefined
          if (!treeTurns.has(turn)) {
            node.title = (numberOf(order.revision) ?? 1) > 1 ? `${work.goal} (r${order.revision})` : (work.goal || node.title)
            node.status = isInvalidated ? 'deprecated' : work.done ? 'done' : work.status === 'running' ? 'running' : 'ready'
            if (work.outputSummary) node.result = { summary: work.outputSummary, materialIds: [], outcome: work.outcome,
              expectationMet: work.expectationMet }
          } else if (isInvalidated) {
            node.status = 'deprecated'
          }
          nodeMap.set(id, node)
          if (order.id === state?.current) activeNodeId = (work.done || node.status === 'deprecated') ? undefined : id
        }
        break
      }
      case 'flow/tree_state': {
        currentTurn = Math.max(currentTurn, turn); organizedMode = 'tree'; treeTurns.add(turn)
        const state = data.state as Record<string, unknown> | undefined
        if (!state) break
        const rawNodes = state.nodes as Record<string, Record<string, unknown>> | undefined
        for (const raw of Object.values(rawNodes ?? {})) {
          const rawId = stringOf(raw.id)
          if (!rawId) continue
          const mappedWorkId = nodeToWorkMap.get(nodeKey(rawId, turn))
          const effectiveId = mappedWorkId ?? rawId
          const id = nodeKey(effectiveId, turn)
          const node = nodeMap.get(id) ?? createFlowNode(id, turn)
          // Deprecated instances preserve their own state and must not be overwritten by tree state
          if (node.status === 'deprecated') {
            treeFields(node, raw, turn)
            nodeMap.set(id, node)
            continue
          }
          if (!mappedWorkId) {
            node.title = stringOf(raw.title) ?? node.title
            node.kind = stringOf(raw.kind) ?? node.kind
            node.status = flowStatus(raw.status)
          }
          treeFields(node, raw, turn)
          nodeMap.set(id, node)
        }
        const active = stringOf(data.active_node_id) ?? stringOf(state.active)
        const mappedActive = active ? (nodeToWorkMap.get(nodeKey(active, turn)) ?? active) : undefined
        activeNodeId = mappedActive ? nodeKey(mappedActive, turn) : undefined
        const rawPath = observerStringList(data.active_path)
        activePath = rawPath.map(id => {
          const mapped = nodeToWorkMap.get(nodeKey(id, turn)) ?? id
          return nodeKey(mapped, turn)
        })
        if (data.ended === true) unfinishedTree = data.root_finished !== true
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
      case 'worker/progress': {
        currentTurn = Math.max(currentTurn, turn)
        const rawWorkId = stringOf(data.workId)
        const rawNodeId = stringOf(data.nodeId)
        const mappedWorkId = rawNodeId ? nodeToWorkMap.get(nodeKey(rawNodeId, turn)) : undefined
        const effectiveWorkId = rawWorkId ?? mappedWorkId
        const workKey = effectiveWorkId ? nodeKey(effectiveWorkId, turn) : undefined
        const nodeKeyVal = rawNodeId ? nodeKey(rawNodeId, turn) : undefined
        const id = workKey ?? nodeKeyVal ?? nodeKey('worker', turn)
        let node = (workKey ? nodeMap.get(workKey) : undefined) ?? (nodeKeyVal ? nodeMap.get(nodeKeyVal) : undefined)
        if (!node) {
          node = createFlowNode(id, turn, '工作者步骤', 'step', undefined, 'running')
          nodeMap.set(id, node)
        }
        if (effectiveWorkId && rawNodeId) {
          nodeToWorkMap.set(nodeKey(rawNodeId, turn), effectiveWorkId)
        }
        if (!treeTurns.has(turn)) node.status = 'running'
        if (data.revision !== undefined) node.revision = numberOf(data.revision) ?? node.revision
        if (data.planRevision !== undefined) node.planRevision = numberOf(data.planRevision) ?? node.planRevision
        const known = Array.isArray(data.knownConditions)
          ? data.knownConditions.filter((item): item is string => typeof item === 'string')
          : typeof data.knownConditions === 'string' ? [data.knownConditions] : []
        node.progress.push({
          step: numberOf(data.step),
          purpose: stringOf(data.purpose) ?? '',
          knownConditions: known,
          openQuestions: observerStringList(data.openQuestions),
          findings: Array.isArray(data.findings) ? data.findings.flatMap(item => {
            if (typeof item !== 'object' || item === null) return []
            const finding = item as Record<string, unknown>
            const text = stringOf(finding.text)
            return text ? [`${text}${observerStringList(finding.files).length ? ` · ${observerStringList(finding.files).join(', ')}` : ''}`] : []
          }) : [],
          observerResponses: Array.isArray(data.observerResponses) ? data.observerResponses.flatMap(item => {
            if (typeof item !== 'object' || item === null) return []
            const response = item as Record<string, unknown>
            return [`${stringOf(response.id) ?? ''} · ${stringOf(response.disposition) ?? ''}：${stringOf(response.reason) ?? ''}`]
          }) : [],
          nextAction: stringOf(data.nextAction) ?? '',
        })
        if (!treeTurns.has(turn)) activeNodeId = id
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
          nodeMap.set(id, node)
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
      case 'tool/call': {
        currentTurn = Math.max(currentTurn, turn)
        const callId = stringOf(data.callId) ?? ''
        const name = stringOf(data.name) ?? ''
        const argsStr = argumentsOf(data.arguments)
        const flowNodeId = stringOf(data.flowNodeId)
        const targetId = flowNodeId
          ? nodeKey(nodeToWorkMap.get(nodeKey(flowNodeId, turn)) ?? flowNodeId, turn)
          : activeNodeId ?? nodeKey('worker', turn)
        if (callId) callsToNode.set(callId, targetId)
        let targetNode = nodeMap.get(targetId)
        if (!targetNode) {
          targetNode = createFlowNode(targetId, turn, taskLabel || '执行任务', 'worker', taskLabel, 'running')
          nodeMap.set(targetId, targetNode)
        }
        targetNode.tools.push({ id: callId, name, input: argsStr })
        for (const file of extractFilesFromArgs(argsStr)) targetNode.files.add(file)
        activeNodeId = targetId
        break
      }
      case 'tool/result': {
        const callId = callIdOfResult(event) ?? ''
        const targetId = callsToNode.get(callId) ?? activeNodeId
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
          if (node.turn !== turn || node.status === 'deprecated') continue
          if (node.isMainTask) {
            if (node.result?.goalAchieved !== true) node.status = 'paused'
          } else if (reason.kind === 'completed' && !treeTurns.has(turn) && !node.workUnit) {
            if (node.status === 'running') node.status = 'done'
            else if (node.status === 'ready' || node.status === 'pending') node.status = 'skipped'
          } else if (reason.kind === 'error' && node.id === activeNodeId) {
            node.status = 'failed'
          } else if (reason.kind === 'aborted' && node.id === activeNodeId) {
            node.status = 'interrupted'
          }
        }
        if (treeTurns.has(turn)) {
          const root = Array.from(nodeMap.values()).find(node => node.turn === turn && !node.parentId)
          unfinishedTree = !!root && !['done', 'completed', 'skipped'].includes(root.status)
          // Preserve explicit node statuses when a run ends; final text is not completion.
        }
        activeNodeId = undefined
        break
      }
    }
  }

  if (nodeMap.size === 0) {
    const ended = [...events].reverse().find(event => event.type === 'turn/end')
    const reason = ended ? reasonOf(ended.data.reason) : undefined
    const status: FlowNodeStatus = reason?.kind === 'completed'
      ? 'done'
      : reason?.kind === 'error' ? 'failed'
        : reason?.kind === 'aborted' ? 'interrupted'
          : events.some(event => event.type === 'turn/start') ? 'running' : 'ready'
    const id = nodeKey('worker', currentTurn)
    const node = createFlowNode(
      id,
      currentTurn,
      taskLabel ? (taskLabel.length > 52 ? taskLabel.slice(0, 52) + '…' : taskLabel) : '工作者任务',
      'worker',
      taskLabel || undefined,
      status,
    )
    nodeMap.set(id, node)
    if (status === 'running') activeNodeId = id
  }

  const nodes: FlowNodeData[] = Array.from(nodeMap.values()).map(node => ({
    id: node.id,
    title: node.title,
    kind: node.kind,
    status: node.status,
    description: node.description,
    parentId: node.parentId,
    isMainTask: node.isMainTask,
    objective: node.objective,
    doneWhen: node.doneWhen,
    workUnit: node.workUnit,
    constraints: node.constraints,
    result: node.result,
    progress: node.progress,
    tools: node.tools,
    consults: node.consults,
    files: Array.from(node.files),
    turn: node.turn,
    step: node.step,
    revision: node.revision,
    requestId: node.requestId,
    planRevision: node.planRevision,
    invalidatedByPlanRevision: node.invalidatedByPlanRevision,
  }))
  return { nodes, edges, activeNodeId, observerEnabled, nodeReviews: [...nodeReviews.values()], observerNotes, observerRetrospective, organizerProgress, currentWork,
    organizedMode, organizedTurn: currentTurn, activePath, unfinishedTree,
    planRevision: currentPlanRevision,
    rewindRecords: Array.from(rewindRecordsMap.values()),
  }
}
