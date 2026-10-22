import { useSyncExternalStore } from 'react'
import { brandIcon, getPalette, subscribePalette, type ThemePalette } from './theme.ts'

/** The current colour theme; re-renders when the user switches it. */
export function usePalette(): ThemePalette {
  return useSyncExternalStore(subscribePalette, getPalette)
}

/** The brand mark for the current theme: a sprout or a ginkgo leaf. */
export function useBrandIcon(): string {
  return brandIcon(usePalette())
}
