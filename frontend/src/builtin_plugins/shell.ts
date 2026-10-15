import type { Context } from '@deepseek-ai/cordis'
import { api } from '../api.ts'
import { ChatView } from '../components/ChatView.tsx'
import { Composer } from '../components/Composer.tsx'
import { Sidebar } from '../components/Sidebar.tsx'
import { Trajectory } from '../components/Trajectory.tsx'

declare module '@deepseek-ai/cordis' { interface Context { agentApi: typeof api } }
export function AgentApiPlugin(ctx: Context): void { ctx.provide('agentApi', api) }
export const shellPlugins = [
  { name: 'agent-sidebar', inject: ['slots'], apply(ctx: Context) { ctx.slots.register({ name: 'agent.sidebar' }, Sidebar) } },
  { name: 'agent-chat', inject: ['slots', 'agentApi'], apply(ctx: Context) { ctx.slots.register({ name: 'agent.chat' }, ChatView) } },
  { name: 'agent-trajectory', inject: ['slots'], apply(ctx: Context) { ctx.slots.register({ name: 'agent.trajectory' }, Trajectory) } },
  { name: 'agent-composer', inject: ['slots', 'agentApi'], apply(ctx: Context) { ctx.slots.register({ name: 'agent.composer' }, Composer) } },
]
