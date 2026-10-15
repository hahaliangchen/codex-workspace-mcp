import '@deepseek-ai/cordis'
import type { api, SkillPackage } from '../api.ts'

export interface PluginHost {
  session<T = unknown>(method: string, args?: Record<string, unknown>): Promise<T>
  service<T = unknown>(method: string, args?: Record<string, unknown>): Promise<T>
  callTool<T = unknown>(name: string, args?: Record<string, unknown>): Promise<T>
}
/** Shared by the Cordis service and the iframe transport. No general Agent API. */
export function createPluginHost(agent: typeof api, owner: SkillPackage): PluginHost {
  const sessionMethods = new Set(['open', 'create', 'prompt', 'events', 'status', 'cancel', 'upload', 'attachment'])
  const serviceMethods = new Set(['project', 'task', 'events', 'resource'])
  return Object.freeze({
    session<T>(method: string, args: Record<string, unknown> = {}): Promise<T> {
      if (!sessionMethods.has(method)) return Promise.reject(new Error('插件会话方法不可用。'))
      return agent.pluginSession<T>(owner.id, { ...args, method, package_scope: owner.scope })
    },
    service<T>(method: string, args: Record<string, unknown> = {}): Promise<T> {
      if (!serviceMethods.has(method)) return Promise.reject(new Error('插件服务方法不可用。'))
      return agent.pluginService<T>(owner.id, { method, arguments: args, package_scope: owner.scope })
    },
    callTool<T>(name: string, args: Record<string, unknown> = {}): Promise<T> {
      if (!owner.enabled || !owner.tools.includes(name)) return Promise.reject(new Error('工具不属于当前插件。'))
      return agent.skillTool<T>(owner.id, name, args)
    },
  })
}
declare module '@deepseek-ai/cordis' { interface Context { pluginHost: PluginHost } }
