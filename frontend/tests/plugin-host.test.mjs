import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

test('Cordis page services isolate owners and use the same scoped methods as the iframe', async () => {
  const server = await createServer({ configFile: fileURLToPath(new URL('../vite.config.ts', import.meta.url)), server: { middlewareMode: true }, optimizeDeps: { noDiscovery: true, entries: [], include: [] }, appType: 'custom' })
  try {
    const { Context } = await server.ssrLoadModule('/src/dsh/cordis/index.ts')
    const { createPluginHost } = await server.ssrLoadModule('/src/plugins/host.ts')
    const calls = [], services = []
    const api = { pluginSession: async (...args) => calls.push(['session', ...args]), pluginService: async (...args) => calls.push(['service', ...args]), skillTool: async (...args) => calls.push(['tool', ...args]) }
    const ctx = new Context()
    try {
      for (const [id, scope] of [['ppt-master', 3], ['other', 4]]) {
        const owner = { id, scope, enabled: true, tools: [id + '__read'] }
        const fiber = await ctx.isolate('pluginHost').plugin({ name: id, apply(ctx) {
          ctx.provide('pluginHost', createPluginHost(api, owner))
          ctx.plugin({ name: id + '-page', inject: ['pluginHost'], apply(ctx) { services.push(ctx.pluginHost) } })
        } })
        assert.ok(fiber)
      }
      await services[0].session('prompt', { task_id: 'task-ppt', prompt: '改标题', package_scope: 999, method: 'general' })
      await services[1].session('open')
      await services[0].service('project', { project_id: 7 })
      assert.deepEqual(calls[0], ['session', 'ppt-master', { task_id: 'task-ppt', prompt: '改标题', package_scope: 3, method: 'prompt' }])
      assert.equal(calls[1][1], 'other')
      assert.equal(calls[2][2].method, 'project')
      await services[0].models()
      assert.deepEqual(calls[3], ['session','ppt-master',{ method:'models',package_scope:3 }])
      await assert.rejects(services[0].session('run_program'), /方法不可用/)
      await assert.rejects(services[0].service('call'), /方法不可用/)
      await assert.rejects(services[0].callTool('other__read'), /不属于/)
    } finally { await ctx.fiber.dispose() }
  } finally { await server.close() }
})
