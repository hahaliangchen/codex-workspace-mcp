import { useState } from 'react'
import { useAgentApi } from '../cordis/react.tsx'
import css from './NoteProposal.module.css'

export const PREFILL_EVENT = 'composer:prefill'

let pending: { readonly text: string; readonly at: number } | undefined

/** Put text back into the composer, e.g. the message a rewind removed. */
export function prefillComposer(text: string): void {
  // Kept briefly so a composer that mounts right after (a rewind to turn 1
  // swaps in the welcome page's composer) still receives it.
  pending = { text, at: Date.now() }
  window.dispatchEvent(new CustomEvent<string>(PREFILL_EVENT, { detail: text }))
}

/** A prefill made in the last few seconds, if any. */
export function recentPrefill(): string | undefined {
  return pending && Date.now() - pending.at < 3000 ? pending.text : undefined
}

/** "Back to here": undo this turn and everything after, then offer the message again. */
export function RewindControl({ taskId, turn, onRewound }: { taskId: string; turn: number; onRewound: () => void }) {
  const api = useAgentApi()
  const [phase, setPhase] = useState<'idle' | 'confirm' | 'busy'>('idle')
  const [error, setError] = useState('')
  if (phase === 'idle') {
    return <button type="button" className={css.rewindLink} title="撤销这一轮及之后的改动和对话，回到发这条消息之前" onClick={() => { setError(''); setPhase('confirm') }}>
      ↶ 回到这里
    </button>
  }
  const confirm = () => {
    setPhase('busy')
    api.rewind(taskId, turn).then(result => {
      setPhase('idle')
      onRewound()
      if (result.prompt) prefillComposer(result.prompt)
    }, (cause: unknown) => {
      setPhase('confirm')
      setError(cause instanceof Error ? cause.message : String(cause))
    })
  }
  return <div className={css.rewindConfirm} role="alertdialog" aria-label="确认回退">
    <span>撤销这一轮及之后 agent 改过的文件，并删除这之后的对话？这条消息会放回输入框，可以改了再发。</span>
    {error && <span className={css.error} role="alert">{error}</span>}
    <div className={css.actions}>
      <button type="button" className={css.secondary} disabled={phase === 'busy'} onClick={() => setPhase('idle')}>取消</button>
      <button type="button" className={css.primary} disabled={phase === 'busy'} onClick={confirm}>{phase === 'busy' ? '回退中…' : '确认回退'}</button>
    </div>
  </div>
}
