import { Context } from '@deepseek-ai/cordis'
import { api } from '../api.ts'
import { ChatView } from '../components/ChatView.tsx'
import { Composer } from '../components/Composer.tsx'
import { Sidebar } from '../components/Sidebar.tsx'
import { Trajectory } from '../components/Trajectory.tsx'
import { apply as applyUiRenderer } from '../dsh/ui-renderer/index.ts'
import { RootPlugin } from './root.tsx'
import { SessionScopePlugin } from './sessionScope.ts'

declare module '@deepseek-ai/cordis' {
  interface Context {
    agentApi: typeof api
  }
}

function AgentApiPlugin(ctx: Context): void {
  ctx.provide('agentApi', api)
}

const SidebarPlugin = {
  name: 'agent-sidebar',
  inject: ['slots'],
  apply(ctx: Context): void {
    ctx.slots.register({ name: 'agent.sidebar' }, Sidebar)
  },
}

const ChatPlugin = {
  name: 'agent-chat',
  inject: ['slots', 'agentApi'],
  apply(ctx: Context): void {
    ctx.slots.register({ name: 'agent.chat' }, ChatView)
  },
}

const TrajectoryPlugin = {
  name: 'agent-trajectory',
  inject: ['slots'],
  apply(ctx: Context): void {
    ctx.slots.register({ name: 'agent.trajectory' }, Trajectory)
  },
}

const ComposerPlugin = {
  name: 'agent-composer',
  inject: ['slots', 'agentApi'],
  apply(ctx: Context): void {
    ctx.slots.register({ name: 'agent.composer' }, Composer)
  },
}

/** DSH Cordis core and original UI renderer, with Rust-facing page plugins. */
export async function bootBrowserCordis(): Promise<Context> {
  const ctx = new Context()
  try {
    await ctx.plugin(AgentApiPlugin)
    await ctx.plugin({ name: 'ui-renderer', apply: applyUiRenderer })
    await ctx.plugin(SessionScopePlugin)
    await ctx.plugin(RootPlugin)
    await ctx.plugin(SidebarPlugin)
    await ctx.plugin(ChatPlugin)
    await ctx.plugin(TrajectoryPlugin)
    await ctx.plugin(ComposerPlugin)
    return ctx
  } catch (error) {
    await ctx.fiber.dispose()
    throw error
  }
}
