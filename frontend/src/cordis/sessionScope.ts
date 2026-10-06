import type { Context } from '@deepseek-ai/cordis'
import type { SlotScopeAdapter, StandardSourceBinding } from '@deepseek-ai/dsh-client-ui-slots'

const absentBinding: StandardSourceBinding = {
  key: undefined,
  hooks: {},
  keyedHooks: {},
  props: {},
}

const absentSource = {
  getSnapshot: () => absentBinding,
  subscribe: (_listener: () => void) => () => {},
}

/** The Rust page has root-scoped slots; DSH's renderer still requires this seat. */
const adapter: SlotScopeAdapter = {
  current: absentSource,
  bindingSource: () => absentSource,
}

export const SessionScopePlugin = {
  name: 'agent-session-scope',
  inject: ['slots'],
  apply(ctx: Context): void {
    ctx.slots.installScope('session', adapter)
  },
}
