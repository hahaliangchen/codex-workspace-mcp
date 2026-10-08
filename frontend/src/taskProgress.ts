import type { AgentEvent, ResumeTarget } from './api.ts'

type RecordValue = Record<string, unknown>
function record(value: unknown): RecordValue | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value as RecordValue : undefined
}

/** Read the latest transmitted execution record, without inferring completion
 * from the chat text or the number of finished child nodes. */
export function taskProgress(events: readonly AgentEvent[]) {
  for (let index = events.length - 1; index >= 0; index--) {
    const event = events[index]
    if (!event) continue
    const state = event.type === 'scheduler/state' ? record(event.data.state)
      : event.type === 'flow/session' ? record(event.data.scheduler) : undefined
    if (!state) continue
    const frames = record(state.frames)
    const frame = record(frames?.[String(state.current)])
    const order = record(frame?.order)
    const output = record(frame?.output)
    const canResume = state.request_completed !== true && frame?.status !== 'done'
      && frame?.invalidated_by_plan_revision == null && typeof order?.id === 'string'
      && typeof order.revision === 'number' && typeof state.request_started_turn === 'number'
    return {
      goal: typeof state.request_goal === 'string' ? state.request_goal : '',
      completed: state.request_completed === true,
      summary: typeof state.final_result === 'string' ? state.final_result : '',
      unresolved: Array.isArray(state.request_unresolved) ? state.request_unresolved.filter((item): item is string => typeof item === 'string') : [],
      nodeTitle: typeof order?.goal === 'string' ? order.goal : '',
      nodeSummary: typeof output?.summary === 'string' ? output.summary : '',
      resumeTarget: canResume ? { work_id: order.id as string, revision: order.revision as number,
        request_id: state.request_started_turn as number } satisfies ResumeTarget : undefined,
    }
  }
  return undefined
}

export function resumeTargetForWork(events: readonly AgentEvent[], workId: string | undefined) {
  const target = taskProgress(events)?.resumeTarget
  return target?.work_id === workId ? target : undefined
}
