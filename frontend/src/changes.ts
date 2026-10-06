import type { AgentEvent, ChangesSummary } from './api.ts'

/** Last server-recorded net change summary for each turn; no inferred edits. */
export function changeSummaries(events: readonly AgentEvent[]): Map<number, ChangesSummary> {
  const summaries = new Map<number, ChangesSummary>()
  for (const event of events) {
    if (event.type !== 'workspace/changes') continue
    const data = event.data as unknown as ChangesSummary
    if (typeof data.turn === 'number' && Array.isArray(data.files)) summaries.set(data.turn, data)
  }
  return summaries
}
