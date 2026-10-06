import { createContext, useContext, type ReactNode } from 'react'
import type { Context } from '@deepseek-ai/cordis'

const BrowserCordis = createContext<Context | null>(null)

export function CordisProvider({ context, children }: { context: Context; children: ReactNode }) {
  return <BrowserCordis.Provider value={context}>{children}</BrowserCordis.Provider>
}

function useCordis(): Context {
  const ctx = useContext(BrowserCordis)
  if (ctx === null) throw new Error('browser Cordis has not booted')
  return ctx
}

export function useAgentApi() {
  return useCordis().agentApi
}
