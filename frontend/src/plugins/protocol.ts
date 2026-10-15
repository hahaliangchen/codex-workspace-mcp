import type { Context } from '@deepseek-ai/cordis'
import type { ComponentType } from 'react'
import type { SkillPackage } from '../api.ts'

export interface PluginPageProps { workspaceRoot: string; owner: SkillPackage; pageId: string }
export interface FrontendConfig { owner: SkillPackage }
/** Same Cordis registration contract for project modules and installed modules. */
export interface FrontendPlugin {
  name: string
  inject: string[]
  apply(ctx: Context, config: FrontendConfig): void | Promise<void>
}
export interface PageContribution {
  owner: string
  scope: number
  id: string
  entry: string
  component: ComponentType<PluginPageProps>
}
