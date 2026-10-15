import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

test('builtin and external page contributions obey Cordis ownership, dependencies and disposal', async () => {
  const server = await createServer({ configFile: fileURLToPath(new URL('../vite.config.ts', import.meta.url)), server: { middlewareMode: true }, optimizeDeps: { noDiscovery: true, entries: [], include: [] }, appType: 'custom' })
  try {
    const { Context } = await server.ssrLoadModule('/src/dsh/cordis/index.ts')
    const { PluginPages } = await server.ssrLoadModule('/src/plugins/pages.ts')
    const ctx = new Context()
    try {
      await ctx.plugin({ name: 'pages', apply(ctx) { new PluginPages(ctx) } })
      const owner = (id, origin, scope) => ({ id, name: id, title: id, description: id, enabled: true, scope, origin, inject: [], tools: [], pages: [{ id: 'view', title: 'view', renderer: 'react', entry: `${id}.view` }] })
      const builtin = owner('builtin-test', 'builtin', 1), external = owner('external-test', 'external', 2)
      ctx.pluginPages.setPackages([builtin, external])
      const builtinFiber = await ctx.plugin({ name: builtin.id, inject: ['pluginPages'], apply(ctx) { ctx.pluginPages.register(builtin, 'view', 'builtin-test.view', () => null) } })
      const externalFiber = await ctx.plugin({ name: external.id, inject: ['pluginPages'], apply(ctx) { ctx.pluginPages.register(external, 'view', 'external-test.view', () => null) } })
      assert.equal(ctx.pluginPages.getSnapshot().pages.length, 2)
      await builtinFiber.dispose()
      assert.deepEqual(ctx.pluginPages.getSnapshot().pages.map(page => page.owner), [external.id])
      await externalFiber.dispose()
      assert.equal(ctx.pluginPages.getSnapshot().pages.length, 0)
      await assert.rejects(async () => { await ctx.plugin({ name: 'invalid', inject: ['pluginPages'], apply(ctx) { ctx.pluginPages.register({ ...external, scope: 99 }, 'view', 'external-test.view', () => null) } }) }, /作用域/)
      // A dependency declared through inject activates only when its service exists.
      const pending = ctx.plugin({ name: 'dependent', inject: ['pageDependency', 'pluginPages'], apply(ctx) { ctx.pluginPages.register(external, 'view', 'external-test.view', () => null) } })
      assert.equal(ctx.pluginPages.getSnapshot().pages.length, 0)
      await ctx.plugin({ name: 'dependency', apply(ctx) { ctx.provide('pageDependency', {}) } })
      const dependencyFiber = await pending
      assert.equal(ctx.pluginPages.getSnapshot().pages.length, 1)
      await dependencyFiber.dispose()
      assert.equal(ctx.pluginPages.getSnapshot().pages.length, 0)
    } finally { await ctx.fiber.dispose() }
  } finally { await server.close() }
})
