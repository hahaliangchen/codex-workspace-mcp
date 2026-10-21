import { Tooltip } from '@deepseek-ai/dsh-client-ui-primitives'
import type { AgentEvent } from '../api.ts'

export interface ContextUsage {
  readonly used: number
  readonly budget: number
  readonly compactAt: number
}

/** The latest request's size, as recorded by the main agent before sending it. */
export function latestContextUsage(events: readonly AgentEvent[]): ContextUsage | undefined {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]!
    if (event.type !== 'context/usage') continue
    const used = Number(event.data.estimated_input_tokens)
    const budget = Number(event.data.input_budget)
    if (!(used >= 0) || !(budget > 0)) return undefined
    return { used, budget, compactAt: Number(event.data.compact_at) || budget }
  }
  return undefined
}

const thousands = (value: number) => value >= 10_000 ? `${Math.round(value / 1000)}k` : value >= 1000 ? `${(value / 1000).toFixed(1)}k` : String(value)

/** A small ring by the model name: how full this conversation's context is. */
export function ContextRing({ usage }: { usage: ContextUsage | undefined }) {
  if (!usage) return null
  // Fullness is measured against the point where automatic compression starts.
  const ratio = Math.min(1, usage.used / usage.compactAt)
  const tone = ratio >= 0.85 ? '#d97706' : ratio >= 0.6 ? '#5dcaa5' : '#1d9e75'
  const radius = 7
  const circumference = 2 * Math.PI * radius
  const label = ratio >= 0.85
    ? `上下文快满了：已用 ${thousands(usage.used)} tokens，到 ${thousands(usage.compactAt)} 会自动压缩。换个话题时可以开新会话。`
    : `上下文已用 ${thousands(usage.used)} / ${thousands(usage.compactAt)} tokens（${Math.round(ratio * 100)}%），满了会自动压缩`
  return <Tooltip label={label} side="top" delayMs={200}>
    <span role="img" aria-label={label} style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 11.5, color: 'var(--codex-text-muted, #6b7280)' }}>
      <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
        <circle cx="9" cy="9" r={radius} fill="none" stroke="var(--codex-card-border, #e5e7eb)" strokeWidth="2.2" />
        <circle cx="9" cy="9" r={radius} fill="none" stroke={tone} strokeWidth="2.2" strokeLinecap="round"
          strokeDasharray={`${circumference * ratio} ${circumference}`} transform="rotate(-90 9 9)" />
      </svg>
      {ratio >= 0.85 && <span style={{ color: tone }}>{Math.round(ratio * 100)}%</span>}
    </span>
  </Tooltip>
}
