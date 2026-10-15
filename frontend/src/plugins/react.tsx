import { useMemo, useSyncExternalStore } from 'react'
import { useAgentApi, useCordis } from '../cordis/react.tsx'
import type { SkillPackage } from '../api.ts'

export function usePluginPages() {
  const pages = useCordis().pluginPages
  return useSyncExternalStore(pages.subscribe, pages.getSnapshot)
}
export function usePluginTools(owner: SkillPackage) {
  const api = useAgentApi()
  return useMemo(() => ({
    call: <T,>(name: string, args: unknown): Promise<T> => {
      if (!owner.enabled || !owner.tools.includes(name)) return Promise.reject(new Error('工具不属于当前插件。'))
      return api.skillTool<T>(owner.id, name, args)
    },
  }), [api, owner.id, owner.scope, owner.enabled, owner.tools])
}
