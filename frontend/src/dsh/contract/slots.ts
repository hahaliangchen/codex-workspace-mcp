/**
 * Stand-in for ui-chat's contract types used by copied leaf components.
 * The actual ui-slots and ui-renderer runtimes are copied alongside this file.
 */

import type { SnapshotSelectorHook } from '@deepseek-ai/dsh-client-store'

export type TranslateParams = Readonly<Record<string, string | number>>

export type Translate = (key: string, params?: TranslateParams) => string

export interface ChatViewSlotProps {
  readonly t: Translate
}

/** Mirrors ui-chat `ChatPresentationPolicy` in the `standard` work-details mode. */
export interface ChatPresentationPolicy {
  readonly foldCompletedTurns: boolean
  readonly stepGrouping: 'collapsed' | 'history' | 'none'
  readonly liveProcessDetail: boolean
  readonly settledReasoningPreview: boolean
}

export type UsePresentation = SnapshotSelectorHook<ChatPresentationPolicy>

export type UseDisclosure = () => {
  readonly expanded: boolean
  readonly setExpanded: (open: boolean) => void
  readonly toggle: () => void
}
