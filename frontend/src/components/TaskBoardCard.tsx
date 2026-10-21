import { useState } from 'react'
import { useAgentApi } from '../cordis/react.tsx'
import type { Task } from '../api.ts'

export function TaskBoardCard({task, reload}: {task: Task; reload: () => void}) {
  const api = useAgentApi()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const board = task.task_board
  const pending = board?.pending_definition
  if (!board?.definition && !pending) return null
  const decide = async (approve: boolean) => {
    if (!pending || busy) return
    setBusy(true); setError('')
    try { await api.decideTaskDefinition(task.task_id, pending.version, approve); if (approve) await api.prompt(task.task_id, '继续执行任务面板中已确认的主任务；读取生效版本，不重写目标。'); reload() }
    catch (e) { setError(e instanceof Error ? e.message : String(e)) }
    finally { setBusy(false) }
  }
  const fields = (definition: NonNullable<typeof board>['definition']) => definition && <>
    <p>{definition.objective}</p>
    <strong>任务约束</strong><ul>{definition.constraints.map((v,i) => <li key={i}>{v}</li>)}</ul>
    <strong>完成条件</strong><ul>{definition.completion_conditions.map((v,i) => <li key={i}>{v}</li>)}</ul>
  </>
  return <section style={{margin: '12px 0', padding: 16, border: '1px solid #9dcbbb', borderRadius: 12}}>
    {board.definition && <details open={!pending}><summary>当前主任务 · 版本 {board.definition.version}</summary>{fields(board.definition)}</details>}
    {pending && <div><strong>{board.definition ? '待确认的任务变更' : '请确认主任务'} · 版本 {pending.version}</strong>{fields(pending.definition)}
      <p>确认后此版本生效；普通对话不会改写当前任务。</p>
      <button disabled={busy || task.status === 'running'} onClick={() => void decide(true)}>确认任务</button>{' '}
      <button disabled={busy || task.status === 'running'} onClick={() => void decide(false)}>拒绝变更</button>
    </div>}
    {error && <p role="alert">{error}</p>}
  </section>
}
