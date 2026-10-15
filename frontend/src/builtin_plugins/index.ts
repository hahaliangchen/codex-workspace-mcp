import type { FrontendPlugin } from '../plugins/protocol.ts'

/** Explicit project membership. Downloaded modules are never globbed into it. */
export const builtinFrontends: Record<string, () => Promise<{ default: FrontendPlugin }>> = {
  'code-flow': () => import('./code-flow/index.ts'),
}
