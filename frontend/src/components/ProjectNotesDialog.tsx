import { useEffect, useState } from 'react'
import { Button, Modal } from '@deepseek-ai/dsh-client-ui-primitives'
import { api } from '../api.ts'
import css from './NoteProposal.module.css'

const PLACEHOLDER = `# 项目备注

- 打包要打完整独立包：对方服务器是封闭环境，只有系统，依赖全部打进包里，拷过去就能直接运行
- 部署前先在本机跑一遍测试`

/** Edit the project's AGENTS.md, which every code session in that project follows. */
export function ProjectNotesDialog({ project, onClose }: { project: { path: string; title: string } | undefined; onClose: () => void }) {
  const [content, setContent] = useState('')
  const [loaded, setLoaded] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  useEffect(() => {
    if (!project) return
    let live = true
    setLoaded(false)
    setError('')
    api.notes(project.path).then(
      notes => { if (live) { setContent(notes.content); setLoaded(true) } },
      (cause: unknown) => { if (live) setError(cause instanceof Error ? cause.message : String(cause)) },
    )
    return () => { live = false }
  }, [project])
  const save = () => {
    if (!project) return
    setBusy(true)
    setError('')
    api.saveNotes(project.path, content).then(onClose, (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause))).finally(() => setBusy(false))
  }
  return <Modal className={css.wideDialog ?? ''} open={!!project} onClose={() => { if (!busy) onClose() }} title={`项目备注 · ${project?.title ?? ''}`} closeLabel="关闭"
    description="写给 AI 的长期说明，保存在项目的 AGENTS.md 里。这个项目里的每次对话都会带上它。"
    footer={<><Button variant="outline" disabled={busy} onClick={onClose}>取消</Button><Button variant="primary" disabled={busy || !loaded} onClick={save}>{busy ? '保存中…' : '保存'}</Button></>}>
    <textarea className={css.input} style={{ minHeight: 260, fontFamily: 'var(--codex-font-mono, ui-monospace, monospace)', fontSize: 13 }}
      aria-label="项目备注内容" placeholder={PLACEHOLDER} value={content} disabled={!loaded || busy} onChange={event => setContent(event.target.value)} />
    {error && <p className={css.error} role="alert">{error}</p>}
  </Modal>
}
