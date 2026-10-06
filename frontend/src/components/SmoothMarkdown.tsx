import { memo, type ComponentProps } from 'react'
import { MarkdownText } from '@deepseek-ai/dsh-client-ui-primitives'
import { useSmoothStream } from '../useSmoothStream.ts'

export const SmoothMarkdown = memo(function SmoothMarkdown(props: ComponentProps<typeof MarkdownText>) {
  const display = useSmoothStream(props.text, props.streaming ?? false)
  return <MarkdownText {...props} text={display.text} streaming={display.streaming} />
})
