import { useEffect, useState } from 'react'
import { Button, Modal } from '@deepseek-ai/dsh-client-ui-primitives'
import type { SkillDraft, SkillDraftInput } from '../api.ts'
import { useAgentApi } from '../cordis/react.tsx'
import type { FlowNodeData } from '../model.ts'
import css from './NoteProposal.module.css'

type Phase = { kind: 'distilling' } | { kind: 'editing' } | { kind: 'saving' } | { kind: 'saved'; path: string } | { kind: 'failed'; message: string }

/** The selected steps, in flow order, as compact input for the distiller. */
export function skillDraftInput(goal: string, steps: readonly FlowNodeData[]): SkillDraftInput {
  return {
    goal,
    steps: steps.map(step => ({
      title: step.title,
      status: step.status,
      description: step.description ?? '',
      tools: step.tools.map(tool => ({ name: tool.name, input: tool.input ?? '', output: tool.output ?? '', failed: tool.failed === true })),
    })),
  }
}

/** Distil the picked flow steps into a project skill the user reviews before saving. */
export function SkillDraftDialog({ taskId, input, onClose, onSaved }: {
  taskId: string
  input: SkillDraftInput | undefined
  onClose: () => void
  onSaved: () => void
}) {
  const api = useAgentApi()
  const [phase, setPhase] = useState<Phase>({ kind: 'distilling' })
  const [draft, setDraft] = useState<SkillDraft>({ name: '', description: '', body: '' })
  const [exists, setExists] = useState(false)
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    if (!input) return
    let live = true
    setPhase({ kind: 'distilling' })
    setExists(false)
    api.skillDraft(taskId, input).then(
      result => { if (live) { setDraft(result); setPhase({ kind: 'editing' }) } },
      (cause: unknown) => { if (live) setPhase({ kind: 'failed', message: cause instanceof Error ? cause.message : String(cause) }) },
    )
    return () => { live = false }
  }, [api, taskId, input, attempt])

  const save = () => {
    setPhase({ kind: 'saving' })
    api.saveSkill(taskId, draft, exists).then(
      result => { setPhase({ kind: 'saved', path: result.path }); onSaved() },
      (cause: unknown) => {
        const message = cause instanceof Error ? cause.message : String(cause)
        if (message.includes('SKILL_EXISTS')) setExists(true)
        setPhase({ kind: 'failed', message: message.replace(/^SKILL_EXISTS:\s*/, '') + (message.includes('SKILL_EXISTS') ? '。再点一次「覆盖保存」会替换原技能。' : '') })
      },
    )
  }
  const busy = phase.kind === 'distilling' || phase.kind === 'saving'
  const editable = phase.kind === 'editing' || (phase.kind === 'failed' && draft.body !== '')
  const field = (key: keyof SkillDraft) => (event: { target: { value: string } }) => {
    setDraft(current => ({ ...current, [key]: event.target.value }))
    if (key === 'name') setExists(false)
  }

  return <Modal className={css.wideDialog ?? ''} open={!!input} onClose={() => { if (!busy) onClose() }} title="存为技能" closeLabel="关闭"
    description={`把选中的 ${input?.steps.length ?? 0} 个步骤提炼成可复用的流程。保存后，这个项目里遇到类似任务时，AI 会先读取它再照着做。`}
    footer={phase.kind === 'saved'
      ? <Button variant="primary" onClick={onClose}>完成</Button>
      : <>
          <Button variant="outline" disabled={busy} onClick={onClose}>取消</Button>
          {phase.kind === 'failed' && draft.body === '' && <Button variant="outline" onClick={() => setAttempt(value => value + 1)}>重试</Button>}
          <Button variant="primary" disabled={busy || !editable || !draft.name.trim() || !draft.description.trim() || !draft.body.trim()} onClick={save}>
            {phase.kind === 'saving' ? '保存中…' : exists ? '覆盖保存' : '保存技能'}
          </Button>
        </>}>
    {phase.kind === 'distilling' && <p className={css.hint}>正在提炼流程：抽出可变参数，把失败过的地方写成注意事项…</p>}
    {phase.kind === 'saved' && <p className={css.hint}>已保存到 {phase.path}。它会跟着项目一起提交到 git。</p>}
    {phase.kind === 'failed' && <p className={css.error} role="alert">{phase.message}</p>}
    {editable && <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
      <label className={css.hint}>技能名称（小写英文和连字符）
        <input className={css.input} value={draft.name} disabled={busy} onChange={field('name')} />
      </label>
      <label className={css.hint}>什么时候用
        <input className={css.input} value={draft.description} disabled={busy} onChange={field('description')} />
      </label>
      <label className={css.hint}>流程步骤
        <textarea className={css.input} style={{ minHeight: 280, fontFamily: 'var(--codex-font-mono, ui-monospace, monospace)', fontSize: 12.5 }}
          value={draft.body} disabled={busy} onChange={field('body')} />
      </label>
    </div>}
  </Modal>
}
