import { useEffect, useRef } from 'react'
import type { PluginPageProps } from './protocol.ts'
import { createPluginHost } from './host.ts'
import { useAgentApi } from '../cordis/react.tsx'
import { useMemo } from 'react'

/** HTML is one implementation adapter for the common page contribution. */
export function HtmlPluginPage({ owner, workspaceRoot, pageId }: PluginPageProps) {
  const frame = useRef<HTMLIFrameElement>(null)
  const api = useAgentApi()
  const host = useMemo(() => createPluginHost(api, owner), [api, owner])
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow || event.data?.type !== 'skill-package/request') return
      const { id, method, name, arguments: args } = event.data as Record<string, unknown>
      if (typeof id !== 'string' || id.length > 128) return
      const source = event.source as Window
      const reply = (result: unknown, error?: string) => {
        if (source === frame.current?.contentWindow) source.postMessage({ type: 'skill-package/response', id, result, error }, '*')
      }
      if (method === 'session' || method === 'service') {
        const operation = event.data.operation
        if (typeof operation !== 'string' || !args || typeof args !== 'object' || Array.isArray(args)) { reply(undefined, '请求格式错误。'); return }
        void host[method](operation, args as Record<string, unknown>).then(value => reply(value)).catch(reason => reply(undefined, String(reason)))
        return
      }
      if (method === 'workspace' && !owner.remote) { reply({ workspace_root: workspaceRoot }); return }
      if (method !== 'tool' || typeof name !== 'string' || !owner.tools.includes(name) || !args || typeof args !== 'object' || Array.isArray(args)) { reply(undefined, '该页面只能调用所属插件声明的工具。'); return }
      void host.callTool(name, owner.remote ? args as Record<string, unknown> : { workspace_root: workspaceRoot, ...args }).then(result => reply(result)).catch(reason => reply(undefined, String(reason)))
    }
    window.addEventListener('message', receive)
    return () => window.removeEventListener('message', receive)
  }, [owner, host, workspaceRoot])
  return <iframe title={owner.pages.find(page => page.id === pageId)?.title ?? pageId} sandbox="allow-scripts" ref={frame} src={`/agent/skill-packages/${encodeURIComponent(owner.id)}/pages/${encodeURIComponent(pageId)}/index`} style={{ width: '100%', height: '100%', border: 0 }} />
}
