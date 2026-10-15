import { Context } from '@deepseek-ai/cordis'
import { apply as applyUiRenderer } from '../dsh/ui-renderer/index.ts'
import { RootPlugin } from './root.tsx'
import { SessionScopePlugin } from './sessionScope.ts'
import { PluginFrontendRuntime } from '../plugins/runtime.ts'
import { AgentApiPlugin, shellPlugins } from '../builtin_plugins/shell.ts'

/** DSH Cordis core and original UI renderer, with Rust-facing page plugins. */
export async function bootBrowserCordis(): Promise<Context> {
  const ctx = new Context()
  try {
    await ctx.plugin(AgentApiPlugin)
    await ctx.plugin({ name: 'ui-renderer', apply: applyUiRenderer })
    await ctx.plugin(PluginFrontendRuntime)
    await ctx.plugin(SessionScopePlugin)
    await ctx.plugin(RootPlugin)
    for (const plugin of shellPlugins) await ctx.plugin(plugin)
    return ctx
  } catch (error) {
    await ctx.fiber.dispose()
    throw error
  }
}
