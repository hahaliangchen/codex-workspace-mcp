import * as React from 'react'
import { type Context, type Fiber } from '@deepseek-ai/cordis'
import { builtinFrontends } from '../builtin_plugins/index.ts'
import { PluginPages } from './pages.ts'
import { HtmlPluginPage } from './html-page.tsx'
import { usePluginTools } from './react.tsx'
import type { FrontendPlugin } from './protocol.ts'
import type { SkillPackage } from '../api.ts'
import { createPluginHost } from './host.ts'

const pluginUi = { React, usePluginTools }
declare module '@deepseek-ai/cordis' { interface Context { pluginUi: typeof pluginUi } }

export const PluginFrontendRuntime = {
  name: 'plugin-frontend-runtime',
  inject: ['agentApi'],
  apply(ctx: Context): void {
    const pages = new PluginPages(ctx)
    ctx.provide('pluginUi', pluginUi)
    const mounted = new Map<string, { scope: number; fiber: Fiber }>()
    let alive = true, syncing = false, previous = ''
    async function modules(owner: SkillPackage): Promise<FrontendPlugin[]> {
      const result: FrontendPlugin[] = []
      if (owner.origin === 'builtin' && owner.pages.some(page => page.renderer === 'react')) {
        const load = builtinFrontends[owner.id]
        if (!load) throw new Error('项目未注册此插件的前端模块。')
        result.push((await load()).default)
      }
      const seen = new Set<string>()
      for (const page of owner.pages.filter(page => page.renderer === 'react' && owner.origin === 'external')) {
        if (seen.has(page.entry)) continue
        seen.add(page.entry)
        const url = `/agent/skill-packages/${encodeURIComponent(owner.id)}/pages/${encodeURIComponent(page.id)}/index?scope=${owner.scope}`
        const loaded: { default: FrontendPlugin } = await import(/* @vite-ignore */ url)
        result.push(loaded.default)
      }
      const html = owner.pages.filter(page => page.renderer === 'html')
      if (html.length) result.push({ name: owner.id, inject: ['pluginPages'], apply(ctx, { owner }) {
        for (const page of html) ctx.pluginPages.register(owner, page.id, page.entry, HtmlPluginPage)
      } })
      return result
    }
    async function sync() {
      if (!alive || syncing) return
      syncing = true
      try {
        const { packages } = await ctx.agentApi.skillPackages()
        if (!alive) return
        const serialized = JSON.stringify(packages)
        if (serialized !== previous) { pages.setPackages(packages); previous = serialized }
        for (const [id, old] of mounted) {
          if (!packages.some(pkg => pkg.id === id && pkg.enabled && pkg.scope === old.scope)) {
            await old.fiber.dispose(); mounted.delete(id); pages.setError(id, '')
          }
        }
        for (const owner of packages.filter(pkg => pkg.enabled && pkg.scope !== null && pkg.pages.length)) {
          if (mounted.has(owner.id)) continue
          try {
            const plugins = await modules(owner)
            if (!alive) return
            for (const plugin of plugins) {
              if (plugin.name !== owner.id || !Array.isArray(plugin.inject) || typeof plugin.apply !== 'function') throw new Error('前端模块必须按插件协议导出 name、inject 和 apply。')
            }
            pages.setError(owner.id, '')
            const parent = ctx.isolate('pluginHost').plugin({ name: `frontend:${owner.id}`, inject: ['pluginPages', 'agentApi'], apply(ctx: Context) {
              ctx.provide('pluginHost', createPluginHost(ctx.agentApi, owner))
              for (const plugin of plugins) {
                const child = ctx.plugin(plugin, { owner })
                void child.then(() => {
                  if (!alive) return
                  const registered = pages.getSnapshot().pages
                  const declared = owner.pages.filter(page => page.renderer === 'react' && owner.origin === 'builtin')
                  if (declared.some(page => !registered.some(item => item.owner === owner.id && item.id === page.id))) pages.setError(owner.id, '前端插件未注册其声明的页面。')
                }, reason => { if (alive) pages.setError(owner.id, String(reason)) })
              }
            } })
            const fiber = await parent
            if (!alive) { await fiber.dispose(); return }
            mounted.set(owner.id, { scope: owner.scope!, fiber })
          } catch (reason) { if (alive) pages.setError(owner.id, String(reason)) }
        }
      } catch (reason) { if (alive) pages.setError('host', String(reason)) }
      finally { syncing = false }
    }
    const timer = setInterval(() => { void sync() }, 1000)
    ctx.effect(() => () => { alive = false; clearInterval(timer) })
    void sync()
  },
}
