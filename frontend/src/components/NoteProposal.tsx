import { useEffect, useState } from 'react'
import { useAgentApi } from '../cordis/react.tsx'
import css from './NoteProposal.module.css'

type Phase = 'pending' | 'saving' | 'saved' | 'dismissed'

const dismissKey = (callId: string): string => `note-proposal-dismissed:${callId}`
function wasDismissed(callId: string): boolean {
  try { return localStorage.getItem(dismissKey(callId)) === '1' } catch { return false }
}

export const LEAF_ICON = <svg width="15" height="15" viewBox="0 0 64 64" aria-hidden="true">
  <path d="M14 50C10 28 26 12 52 12c0 26-16 42-38 38Z" fill="#5DCAA5"/>
  <path d="M14 50 40 24" stroke="#0F6E56" strokeWidth="4" strokeLinecap="round"/>
</svg>

/** The agent's suggestion to keep a rule in AGENTS.md; nothing is written until the user adds it. */
export function NoteProposal({ callId, rawArguments, workspace }: {
  callId: string
  rawArguments: string
  workspace: string | null | undefined
}) {
  const api = useAgentApi()
  const args = parse(rawArguments)
  // Arguments stream in; follow them until the user starts editing.
  const [edited, setDraft] = useState<string>()
  const draft = edited ?? args.note
  const [phase, setPhase] = useState<Phase>(() => wasDismissed(callId) ? 'dismissed' : 'pending')
  const [error, setError] = useState('')

  // A reopened conversation shows the card as added when the note is already in the file.
  useEffect(() => {
    if (!workspace || !args.note || phase !== 'pending') return
    let live = true
    api.notes(workspace).then(notes => { if (live && notes.content.includes(args.note.split('\n')[0] ?? '')) setPhase('saved') }, () => {})
    return () => { live = false }
  }, [api, workspace, args.note]) // eslint-disable-line react-hooks/exhaustive-deps

  if (!workspace || !args.note) return null
  const add = () => {
    setError('')
    setPhase('saving')
    api.addNote(workspace, draft).then(() => setPhase('saved'), (cause: unknown) => {
      setError(cause instanceof Error ? cause.message : String(cause))
      setPhase('pending')
    })
  }
  const dismiss = () => {
    try { localStorage.setItem(dismissKey(callId), '1') } catch { /* per-viewer convenience only */ }
    setPhase('dismissed')
  }

  if (phase === 'saved') return <div className={css.done}>{LEAF_ICON}<span className={css.doneText}>已记到项目备注：{draft.split('\n')[0]}</span></div>
  if (phase === 'dismissed') return <div className={css.done}>{LEAF_ICON}未记录备注</div>
  return <div className={css.card}>
    <div className={css.title}>{LEAF_ICON}要把这条记到项目备注里吗？</div>
    {args.reason && <p className={css.reason}>{args.reason}</p>}
    <textarea className={css.input} value={draft} rows={Math.min(6, Math.max(2, draft.split('\n').length))}
      aria-label="备注内容" disabled={phase === 'saving'} onChange={event => setDraft(event.target.value)} />
    <p className={css.hint}>保存到项目的 AGENTS.md，之后每次对话 AI 都会遵守。可以先改改措辞或补一句。</p>
    {error && <p className={css.error} role="alert">{error}</p>}
    <div className={css.actions}>
      <button type="button" className={css.secondary} disabled={phase === 'saving'} onClick={dismiss}>不用了</button>
      <button type="button" className={css.primary} disabled={phase === 'saving' || !draft.trim()} onClick={add}>{phase === 'saving' ? '保存中…' : '添加备注'}</button>
    </div>
  </div>
}

function parse(raw: string): { note: string; reason: string } {
  try {
    const value: unknown = JSON.parse(raw)
    if (typeof value === 'object' && value !== null) {
      const record = value as Record<string, unknown>
      return { note: typeof record.note === 'string' ? record.note.trim() : '', reason: typeof record.reason === 'string' ? record.reason : '' }
    }
  } catch { /* streaming arguments are incomplete until the call finishes */ }
  return { note: '', reason: '' }
}
