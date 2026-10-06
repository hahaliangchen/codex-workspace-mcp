import './dsh/web-base.css'
import './dsh/theme/brand-font.css'
import './dsh/theme/base.css'
import './dsh/theme/corner-shape.css'
import './dsh/theme/design-platform.css'
import './dsh/theme/scrollbar.css'
import './dsh/theme/gradient-shadow-text.css'
import './dsh/theme/shiki.css'
import './codex-theme.css'
import type { Context } from '@deepseek-ai/cordis'
import { bootBrowserCordis } from './cordis/runtime.ts'
import { applyStoredTheme } from './theme.ts'

applyStoredTheme()

function requireRoot(): HTMLElement {
  const element = document.getElementById('root')
  if (element === null) throw new Error('agent web: missing #root')
  return element
}

const root = requireRoot()
let context: Context | undefined
let unmount: (() => void) | undefined
let disposed = false

function showBootError(error: unknown): void {
  console.error('Browser Cordis failed to start', error)
  const message = error instanceof Error ? error.message : String(error)
  const alert = document.createElement('p')
  alert.setAttribute('role', 'alert')
  alert.textContent = `页面插件启动失败：${message}`
  root.replaceChildren(alert)
}

async function start(): Promise<void> {
  try {
    const ctx = await bootBrowserCordis()
    if (disposed) {
      await ctx.fiber.dispose()
      return
    }
    context = ctx
    unmount = ctx.uiRenderer.mount(root)
  } catch (error) {
    if (context !== undefined) await context.fiber.dispose()
    if (!disposed) showBootError(error)
  }
}

if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    disposed = true
    unmount?.()
    if (context !== undefined) void context.fiber.dispose()
  })
}

void start()
