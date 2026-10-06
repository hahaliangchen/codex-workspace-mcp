import { memo, useMemo } from 'react'
import clsx from 'clsx'
import {
  IconCheckOutlineRegular, IconCopyOutlineRegular, MarkdownText, Tooltip,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { useDisclosure } from '../disclosure.ts'
import { ReasoningRow } from '../dsh/chat/ReasoningRow.tsx'
import css from '../dsh/chat/AssistantMarkdown.module.css'
import tailCss from '../dsh/chat/TurnTailNodeView.module.css'
import actionsCss from '../dsh/chat/MessageIconActions.module.css'
import type { ChatPresentationPolicy } from '../dsh/contract/slots.ts'
import { markdownLabels } from '../dsh/markdown-labels.ts'
import { useCopyFeedback } from '../dsh/ui-primitives/use-copy-feedback.ts'
import { t } from '../i18n.ts'
import { useSmoothStream } from '../useSmoothStream.ts'

const POLICY: ChatPresentationPolicy = {
  foldCompletedTurns: true,
  stepGrouping: 'collapsed',
  liveProcessDetail: true,
  settledReasoningPreview: true,
}

function usePresentation<S>(select: (policy: ChatPresentationPolicy) => S): S {
  return select(POLICY)
}

/** Assistant reply laid out like ui-chat's AssistantMarkdown + TurnTailNodeView copy actions. */
export const AssistantMessage = memo(function AssistantMessage({ text, reasoning, streaming = false, showActions = true }: {
  text: string
  reasoning: string
  streaming?: boolean | undefined
  showActions?: boolean | undefined
}) {
  const labels = useMemo(() => markdownLabels(t), [])
  const { copied, onCopy } = useCopyFeedback(text)
  const displayedText = useSmoothStream(text, streaming)
  const displayedReasoning = useSmoothStream(reasoning, streaming)

  return (
    <div className={tailCss.root} data-actions-reveal="hover">
      <div className={css.root}>
        <div className={css.body}>
          {reasoning !== '' && (
            <ReasoningRow
              text={displayedReasoning.text}
              running={displayedReasoning.streaming}
              usePresentation={usePresentation}
              useDisclosure={useDisclosure}
              t={t}
            />
          )}
          {text !== '' && <MarkdownText text={displayedText.text} labels={labels} streaming={displayedText.streaming} />}
        </div>
      </div>
      {showActions && !displayedText.streaming && text.trim() !== '' && (
        <div className={clsx(actionsCss.actions, tailCss.actions)} data-clock="end">
          <Tooltip label={copied ? t('markdown.copied') : t('markdown.copy')}>
            <button
              type="button"
              className={actionsCss.action}
              aria-label={copied ? t('markdown.copied') : t('markdown.copy')}
              onClick={onCopy}
            >
              {copied ? <IconCheckOutlineRegular size={16} /> : <IconCopyOutlineRegular size={16} />}
            </button>
          </Tooltip>
        </div>
      )}
    </div>
  )
})
