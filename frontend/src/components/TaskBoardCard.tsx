import { useState } from 'react'
import {
  IconCheckCircleOutlineMedium, IconChevronDownOutlineMedium, IconGoalOutlineMedium,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { useAgentApi } from '../cordis/react.tsx'
import type { Task, TaskDefinition } from '../api.ts'
import css from './TaskBoardCard.module.css'

function Fields({ definition }: { definition: TaskDefinition }) {
  return <div className={css.body}>
    {definition.constraints.length > 0 && <div className={css.section}>
      <div className={css.sectionLabel}>任务约束</div>
      <ul className={css.constraints}>{definition.constraints.map((value, i) => <li key={i}>{value}</li>)}</ul>
    </div>}
    {definition.completion_conditions.length > 0 && <div className={css.section}>
      <div className={css.sectionLabel}>完成条件</div>
      <ul className={css.conditions}>{definition.completion_conditions.map((value, i) =>
        <li key={i}><IconCheckCircleOutlineMedium className={css.conditionIcon} /><span>{value}</span></li>)}</ul>
    </div>}
  </div>
}

export function TaskBoardCard({task, reload}: {task: Task; reload: () => void}) {
  const api = useAgentApi()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  // A confirmed task stays out of the way; a proposal opens so it can be decided.
  const [userOpen, setUserOpen] = useState<boolean | undefined>(undefined)
  const board = task.task_board
  const pending = board?.pending_definition
  const definition = pending?.definition ?? board?.definition
  if (!board || !definition) return null
  const open = userOpen ?? Boolean(pending)
  const version = pending ? pending.version : board.definition?.version
  const decide = async (approve: boolean) => {
    if (!pending || busy) return
    setBusy(true); setError('')
    try { await api.decideTaskDefinition(task.task_id, pending.version, approve); if (approve) await api.prompt(task.task_id, '继续执行任务面板中已确认的主任务；读取生效版本，不重写目标。'); reload() }
    catch (e) { setError(e instanceof Error ? e.message : String(e)) }
    finally { setBusy(false) }
  }
  const counts = [
    definition.constraints.length > 0 ? `${definition.constraints.length} 条约束` : '',
    definition.completion_conditions.length > 0 ? `${definition.completion_conditions.length} 项完成条件` : '',
  ].filter(Boolean).join(' · ')
  return <section className={css.card} data-pending={pending ? '' : undefined} data-open={open ? '' : undefined}>
    <button type="button" className={css.header} aria-expanded={open} onClick={() => setUserOpen(!open)}>
      <span className={css.badge}><IconGoalOutlineMedium /></span>
      <span className={css.heading}>
        <span className={css.eyebrow}>
          {pending ? (board.definition ? '待确认的任务变更' : '请确认主任务') : '主任务'}
          {version !== undefined && <span className={css.version}>v{version}</span>}
        </span>
        <span className={css.objective}>{definition.objective}</span>
      </span>
      {!open && counts && <span className={css.meta}>{counts}</span>}
      <IconChevronDownOutlineMedium className={css.chevron} />
    </button>
    {open && <Fields definition={definition} />}
    {open && pending && board.definition && <details className={css.previous}>
      <summary>对照当前生效的 v{board.definition.version}</summary>
      <p className={css.previousObjective}>{board.definition.objective}</p>
      <Fields definition={board.definition} />
    </details>}
    {pending && <div className={css.decision}>
      <p className={css.hint}>{board.definition ? `确认后替换当前生效的 v${board.definition.version}；` : '确认后生效；'}普通对话不会改写任务。</p>
      <div className={css.actions}>
        <button type="button" className={css.secondary} disabled={busy || task.status === 'running'} onClick={() => void decide(false)}>拒绝</button>
        <button type="button" className={css.primary} disabled={busy || task.status === 'running'} onClick={() => void decide(true)}>确认任务</button>
      </div>
    </div>}
    {error && <p role="alert" className={css.error}>{error}</p>}
  </section>
}
