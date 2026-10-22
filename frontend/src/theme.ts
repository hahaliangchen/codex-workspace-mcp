import sproutIcon from './assets/sprout.svg'
import ginkgoIcon from './assets/ginkgo.svg'

const KEY = 'agent-theme'
const PALETTE_KEY = 'agent-palette'

export type ThemeMode = 'light' | 'dark' | 'system'
/** Colour theme, independent of light/dark: 新芽 (green) or 银杏 (autumn). */
export type ThemePalette = 'sprout' | 'ginkgo'

export function getThemeMode(): ThemeMode {
  const stored = localStorage.getItem(KEY)
  if (stored === 'light' || stored === 'dark' || stored === 'system') return stored
  return 'system'
}

function prefersDark(mode: ThemeMode = getThemeMode()): boolean {
  if (mode === 'dark' || mode === 'light') return mode === 'dark'
  return matchMedia('(prefers-color-scheme: dark)').matches
}

export function getPalette(): ThemePalette {
  return localStorage.getItem(PALETTE_KEY) === 'ginkgo' ? 'ginkgo' : 'sprout'
}

export function brandIcon(palette: ThemePalette = getPalette()): string {
  return palette === 'ginkgo' ? ginkgoIcon : sproutIcon
}

const paletteListeners = new Set<() => void>()

export function subscribePalette(listener: () => void): () => void {
  paletteListeners.add(listener)
  return () => { paletteListeners.delete(listener) }
}

function applyPalette(palette: ThemePalette): void {
  document.body.dataset.theme = palette
  const favicon = document.querySelector<HTMLLinkElement>('link[rel="icon"]')
  if (favicon) favicon.href = brandIcon(palette)
}

export function setPalette(palette: ThemePalette): void {
  localStorage.setItem(PALETTE_KEY, palette)
  applyPalette(palette)
  paletteListeners.forEach(listener => { listener() })
}

export function applyStoredTheme(): void {
  document.body.toggleAttribute('data-ds-dark-theme', prefersDark())
  applyPalette(getPalette())
}

export function setThemeMode(mode: ThemeMode): boolean {
  localStorage.setItem(KEY, mode)
  const dark = prefersDark(mode)
  document.body.toggleAttribute('data-ds-dark-theme', dark)
  return dark
}

export function toggleTheme(): boolean {
  const dark = !document.body.hasAttribute('data-ds-dark-theme')
  return setThemeMode(dark ? 'dark' : 'light')
}
