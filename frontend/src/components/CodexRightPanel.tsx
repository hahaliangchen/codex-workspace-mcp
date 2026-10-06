import { useMemo, useState } from 'react'
import clsx from 'clsx'
import type { AgentEvent } from '../api.ts'
import { changeSummaries } from '../changes.ts'
import { ChangesCard } from './ChangesCard.tsx'
import css from './CodexRightPanel.module.css'

interface CodexRightPanelProps {
  workspacePath: string
  taskId?: string | undefined
  events?: readonly AgentEvent[] | undefined
  running?: boolean | undefined
  onClose?: (() => void) | undefined
  onOpenSettings?: (() => void) | undefined
  onChangesUpdated?: (() => void) | undefined
  width?: number | undefined
}

export function CodexRightPanel({
  workspacePath,
  taskId,
  events = [],
  running = false,
  onClose,
  onOpenSettings,
  onChangesUpdated,
  width,
}: CodexRightPanelProps) {
  const [expanded, setExpanded] = useState(true)
  const projectName = workspacePath.split(/[\\/]/).filter(Boolean).at(-1) ?? '工作区'
  const summaries = useMemo(() => [...changeSummaries(events).values()].filter(summary => summary.total > 0), [events])
  const active = summaries.flatMap(summary => summary.files.filter(file => !file.reverted))
  const added = active.reduce((sum, file) => sum + file.added, 0)
  const deleted = active.reduce((sum, file) => sum + file.deleted, 0)

  return (
    <aside className={css.panel} style={width ? { width } : undefined} aria-label="会话文件变更">
      <div className={css.header}>
        <div className={css.titleRow}>
          <div className={css.headerTitleGroup}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={css.headerFolderIcon}>
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
            </svg>
            <span className={css.projectName} title={workspacePath}>{projectName}</span>
          </div>
          <div className={css.headerActions}>
            <button type="button" className={css.iconBtn} title="项目设置" onClick={onOpenSettings}>
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="1" /><circle cx="19" cy="12" r="1" /><circle cx="5" cy="12" r="1" />
              </svg>
            </button>
            <button type="button" className={css.iconBtn} title="关闭面板" onClick={onClose}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
              </svg>
            </button>
          </div>
        </div>
      </div>
      <div className={css.content}>
        <section className={css.section}>
          <button
            type="button"
            className={css.sectionHeader}
            onClick={() => setExpanded(!expanded)}
            aria-expanded={expanded}
          >
            <div className={css.sectionTitle}>
              <svg
                width="12"
                height="12"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2.2"
                strokeLinecap="round"
                strokeLinejoin="round"
                className={clsx(css.sectionChevron, expanded && css.sectionChevronExpanded)}
              >
                <polyline points="9 18 15 12 9 6" />
              </svg>
              <span>各轮改动累计</span>
            </div>
            <div className={css.diffSummary}>
              <span className={css.addedBadge}>+{added}</span>
              <span className={css.deletedBadge}>-{deleted}</span>
            </div>
          </button>
          {expanded && (
            <div className={css.turnList}>
              {summaries.length === 0 && (
                <div className={css.emptyNotice}>
                  <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" className={css.emptyIcon}>
                    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                    <polyline points="14 2 14 8 20 8" />
                  </svg>
                  <span>尚未记录到文件变更</span>
                </div>
              )}
              {taskId && summaries.map(summary => (
                <div key={summary.turn} className={css.turnGroup}>
                  <div className={css.turnHeader}>
                    <span className={css.turnLabel}>第 {summary.turn} 轮</span>
                    <span className={css.turnFileCount}>{summary.files.length} 个文件</span>
                  </div>
                  <ChangesCard
                    taskId={taskId}
                    summary={summary}
                    running={running}
                    onChangesUpdated={onChangesUpdated}
                  />
                </div>
              ))}
            </div>
          )}
        </section>
      </div>
    </aside>
  )
}
