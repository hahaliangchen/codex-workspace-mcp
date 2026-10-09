import type { AgentEvent, ResumeTarget } from '../api.ts'
import { taskProgress } from '../taskProgress.ts'
import styles from './TaskProgressCard.module.css'

export function TaskProgressCard({ events, running, disabled, onResume, onContinue, onViewFlow }: {
  readonly events: readonly AgentEvent[]
  readonly running: boolean
  readonly disabled: boolean
  readonly onResume: (target: ResumeTarget, title: string) => Promise<boolean>
  readonly onContinue: () => Promise<boolean>
  readonly onViewFlow: () => void
}) {
  const progress = taskProgress(events)
  if (!progress?.goal) return null
  return <section className={styles.card} aria-label="主任务进展">
    <div className={styles.heading}><strong>主任务</strong><span>{running ? '进行中' : progress.completed ? '目标已达成' : '执行已结束 · 目标未完全达成'}</span></div>
    <p>{progress.goal}</p>
    {progress.summary && <details><summary>主任务结论</summary><p className={styles.summary}>{progress.summary}</p></details>}
    {progress.unresolved.length > 0 && <ul>{progress.unresolved.map((item, index) => <li key={index}>{item}</li>)}</ul>}
    {progress.resumeTarget && !running && <div className={styles.node}>
      <strong>暂停节点：{progress.nodeTitle}</strong>
      <span>继续将在原节点恢复执行。</span>
      <button type="button" disabled={disabled} onClick={() => { void onResume(progress.resumeTarget!, progress.nodeTitle) }}>继续此任务</button>
    </div>}
    {!progress.resumeTarget && progress.nodeSummary && <p className={styles.summary}>上一节点：{progress.nodeSummary}</p>}
    <div className={styles.actions}>
      {!running && !progress.completed && <button type="button" disabled={disabled} onClick={() => { void onContinue() }}>继续主任务</button>}
      <button type="button" onClick={onViewFlow}>查看任务流</button>
    </div>
  </section>
}
