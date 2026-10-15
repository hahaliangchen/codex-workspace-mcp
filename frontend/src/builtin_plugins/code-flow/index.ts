import type { FrontendPlugin } from '../../plugins/protocol.ts'
import CodeFlowPage from './page.tsx'

const plugin: FrontendPlugin = {
  name: 'code-flow',
  inject: ['agentApi', 'pluginPages'],
  apply(ctx, { owner }) {
    ctx.pluginPages.register(owner, 'architecture', 'code-flow.architecture', CodeFlowPage)
  },
}
export default plugin
