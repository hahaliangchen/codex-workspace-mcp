import { Service, type Context } from '@deepseek-ai/cordis'
import type { ComponentType } from 'react'
import type { SkillPackage } from '../api.ts'
import type { PageContribution, PluginPageProps } from './protocol.ts'

export interface PagesSnapshot {
  packages: readonly SkillPackage[]
  pages: readonly PageContribution[]
  errors: Readonly<Record<string, string>>
}

/** Registrations belong to the caller's Cordis fiber, like DSH slots.register. */
export class PluginPages extends Service {
  private snapshot: PagesSnapshot = { packages: [], pages: [], errors: {} }
  private listeners = new Set<() => void>()
  constructor(ctx: Context) { super(ctx, 'pluginPages') }
  getSnapshot = (): PagesSnapshot => this.snapshot
  subscribe = (listener: () => void): (() => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener) } }
  private publish(next: PagesSnapshot): void { this.snapshot = next; for (const listener of this.listeners) listener() }
  setPackages(packages: readonly SkillPackage[]): void { this.publish({ ...this.snapshot, packages }) }
  setError(owner: string, message: string): void {
    const errors = { ...this.snapshot.errors }
    if (message) errors[owner] = message; else delete errors[owner]
    this.publish({ ...this.snapshot, errors })
  }
  register(owner: SkillPackage, id: string, entry: string, component: ComponentType<PluginPageProps>): () => void {
    const installed = this.snapshot.packages.find(item => item.id === owner.id && item.scope === owner.scope && item.enabled)
    if (!installed || owner.scope === null || !installed.pages.some(page => page.id === id && page.entry === entry)) throw new Error('页面贡献必须属于当前插件声明和作用域。')
    const contribution: PageContribution = { owner: owner.id, scope: owner.scope, id, entry, component }
    const dispose = this.ctx.effect(() => {
      if (this.snapshot.pages.some(page => page.owner === owner.id && page.id === id)) throw new Error('插件页面重复注册。')
      this.publish({ ...this.snapshot, pages: [...this.snapshot.pages, contribution] })
      return () => { this.publish({ ...this.snapshot, pages: this.snapshot.pages.filter(page => page !== contribution) }) }
    }, `pluginPages.register(${owner.id}/${id})`)
    return () => { void dispose() }
  }
}
declare module '@deepseek-ai/cordis' { interface Context { pluginPages: PluginPages } }
