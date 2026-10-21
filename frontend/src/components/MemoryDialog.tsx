import { useEffect, useState } from 'react'
import { Button, Modal } from '@deepseek-ai/dsh-client-ui-primitives'
import { api, type MemoryKind, type Memories } from '../api.ts'
import css from './NoteProposal.module.css'

const date = (seconds: number) => new Date(seconds * 1000).toLocaleDateString()

interface Row {
  readonly kind: MemoryKind
  readonly id: number
  readonly title: string
  readonly summary: string
  readonly details: readonly string[]
  readonly time: number
}

function rows(memories: Memories): Row[] {
  return [
    ...memories.work.map(item => ({
      kind: 'work' as const, id: item.id, time: item.time, title: item.kind === 'observer' ? '复盘经验' : '工作记录', summary: item.summary,
      details: [item.applies_to && `适用：${item.applies_to}`, item.risks && `风险：${item.risks}`, item.files.length ? `文件：${item.files.join('、')}` : ''].filter(Boolean),
    })),
    ...memories.architecture.map(item => ({
      kind: 'architecture' as const, id: item.id, time: item.time, title: `架构 · ${item.area}`, summary: item.summary,
      details: [item.boundaries && `边界：${item.boundaries}`, item.risks && `风险：${item.risks}`].filter(Boolean),
    })),
  ].sort((a, b) => b.time - a.time)
}

/** Everything the agent recalls by search, so the user can correct or forget it. */
export function MemoryDialog({ project, onClose }: { project: { path: string; title: string } | undefined; onClose: () => void }) {
  const open = !!project
  const [memories, setMemories] = useState<Memories>()
  const [editing, setEditing] = useState<{ kind: MemoryKind; id: number; text: string }>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  useEffect(() => {
    if (!project) return
    setError('')
    setEditing(undefined)
    setMemories(undefined)
    api.memories(project.path).then(setMemories, (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)))
  }, [project])
  const run = (action: Promise<Memories>) => {
    setBusy(true)
    setError('')
    action.then(next => { setMemories(next); setEditing(undefined) }, (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)))
      .finally(() => setBusy(false))
  }
  const list = memories ? rows(memories) : []
  return <Modal className={css.wideDialog ?? ''} open={open} onClose={() => { if (!busy) onClose() }} title={`AI 的记忆 · ${project?.title ?? ''}`} closeLabel="关闭"
    description="agent 在这个项目里做完任务后记下的经验和对代码结构的理解，之后在这个项目里遇到相关工作会搜出来参考。记错的可以改，过时的可以删。要它每次都遵守的规则，请写在项目备注里。"
    footer={<Button variant="outline" onClick={onClose}>关闭</Button>}>
    {error && <p className={css.error} role="alert">{error}</p>}
    {!memories && !error && <p className={css.hint}>正在读取…</p>}
    {memories && list.length === 0 && <p className={css.hint}>这个项目还没有记忆。任务结束后，观察者会把值得记住的经验记在这里。</p>}
    <div className={css.memoryList}>
      {list.map(row => {
        const isEditing = editing?.kind === row.kind && editing.id === row.id
        return <div key={`${row.kind}:${row.id}`} className={css.memoryItem}>
          <div className={css.memoryHead}>
            <span className={css.memoryTag}>{row.title}</span>
            <span className={css.hint}>{date(row.time)}</span>
            {!isEditing && <span className={css.memoryActions}>
              <button type="button" className={css.rewindLink} disabled={busy} onClick={() => setEditing({ kind: row.kind, id: row.id, text: row.summary })}>修改</button>
              <button type="button" className={css.rewindLink} disabled={busy} onClick={() => run(api.forgetMemory(project!.path, row.kind, row.id))}>忘掉</button>
            </span>}
          </div>
          {isEditing
            ? <>
                <textarea className={css.input} rows={3} value={editing.text} disabled={busy} aria-label="记忆内容" onChange={event => setEditing({ ...editing, text: event.target.value })} />
                <div className={css.actions}>
                  <button type="button" className={css.secondary} disabled={busy} onClick={() => setEditing(undefined)}>取消</button>
                  <button type="button" className={css.primary} disabled={busy || !editing.text.trim()} onClick={() => run(api.updateMemory(project!.path, row.kind, row.id, editing.text))}>保存</button>
                </div>
              </>
            : <p className={css.memorySummary}>{row.summary}</p>}
          {!isEditing && row.details.map((detail, index) => <p key={index} className={css.hint}>{detail}</p>)}
        </div>
      })}
    </div>
  </Modal>
}
