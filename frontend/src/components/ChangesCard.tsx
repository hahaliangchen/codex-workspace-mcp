import { useEffect, useMemo, useRef, useState } from 'react'
import clsx from 'clsx'
import { DiffBlock, type DiffBlockLabels } from '@deepseek-ai/dsh-client-ui-primitives'
import type { ChangesSummary, FileDiff } from '../api.ts'
import { useAgentApi } from '../cordis/react.tsx'
import css from './ChangesCard.module.css'

const INITIAL_VISIBLE_COUNT = 3

const labels: DiffBlockLabels = {
  copy: '复制补丁',
  copied: '已复制',
  codeLabel: '变更',
  wrapLabel: '自动换行',
  unwrapLabel: '取消换行',
  collapseAria: '收起变更',
  expandAria: hidden => `展开其余 ${hidden} 行`,
  collapse: '收起',
  expand: hidden => `展开其余 ${hidden} 行`,
}

export function ChangesCard({
  taskId,
  summary,
  running,
  onChangesUpdated,
}: {
  taskId: string
  summary: ChangesSummary
  running: boolean
  onChangesUpdated?: (() => void) | undefined
}) {
  const api = useAgentApi()
  const [local, setLocal] = useState<ChangesSummary | null>(null)
  const [expanded, setExpanded] = useState(false)
  const [selected, setSelected] = useState<string | null>(null)
  const [diff, setDiff] = useState<FileDiff | null>(null)
  const [loading, setLoading] = useState(false)
  const [busy, setBusy] = useState(false)
  const [confirmUndo, setConfirmUndo] = useState(false)
  const [error, setError] = useState('')
  const closeButton = useRef<HTMLButtonElement>(null)
  const review = useRef<HTMLElement>(null)
  const data = local ?? summary

  const rows = expanded ? data.files : data.files.slice(0, INITIAL_VISIBLE_COUNT)
  const remainingCount = data.files.length - INITIAL_VISIBLE_COUNT

  const totalAdded = useMemo(
    () => data.files.reduce((sum, f) => sum + (f.reverted ? 0 : f.added), 0),
    [data.files],
  )
  const totalDeleted = useMemo(
    () => data.files.reduce((sum, f) => sum + (f.reverted ? 0 : f.deleted), 0),
    [data.files],
  )

  // Discard a local undo response once the stream publishes the same/newer state.
  useEffect(() => {
    setLocal(null)
    setConfirmUndo(false)
  }, [summary])

  useEffect(() => {
    if (selected === null) return
    let cancelled = false
    setLoading(true)
    setDiff(null)
    setError('')
    api.changeDiff(taskId, data.turn, 0, selected).then(
      value => {
        if (!cancelled) {
          setDiff(value)
          setLoading(false)
        }
      },
      failure => {
        if (!cancelled) {
          setError(String(failure))
          setLoading(false)
        }
      },
    )
    return () => {
      cancelled = true
    }
  }, [api, taskId, data.turn, selected, data])

  useEffect(() => {
    if (selected === null) return
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null
    closeButton.current?.focus()
    const close = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setSelected(null)
        setConfirmUndo(false)
      }
      if (event.key === 'Tab') {
        const elements = review.current?.querySelectorAll<HTMLElement>(
          'button:not(:disabled), select, a[href], [tabindex="0"]',
        )
        const first = elements?.[0]
        const last = elements?.[elements.length - 1]
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault()
          last?.focus()
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault()
          first?.focus()
        }
      }
    }
    window.addEventListener('keydown', close)
    return () => {
      window.removeEventListener('keydown', close)
      previous?.focus()
    }
  }, [selected])

  const hunks = useMemo(
    () =>
      diff && !diff.binary && !diff.unavailable
        ? [{ path: diff.path, oldText: diff.oldText, newText: diff.newText ?? '' }]
        : [],
    [diff],
  )

  const undo = async () => {
    setBusy(true)
    setError('')
    try {
      setLocal(await api.undoChanges(taskId, data.turn))
      setConfirmUndo(false)
      onChangesUpdated?.()
    } catch (failure) {
      setError(String(failure))
    } finally {
      setBusy(false)
    }
  }

  if (data.total === 0) return null

  return (
    <>
      <section className={css.card} aria-label={`第 ${data.turn} 轮文件变更`}>
        {/* Header */}
        <header className={css.header}>
          <div className={css.headerLeft}>
            <div className={css.iconBox} title="文件变更">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
                <rect x="4" y="3" width="16" height="18" rx="3" />
                <line x1="8" y1="9.5" x2="16" y2="9.5" />
                <line x1="12" y1="5.5" x2="12" y2="13.5" />
                <line x1="8" y1="17" x2="16" y2="17" />
              </svg>
            </div>
            <div className={css.headerMeta}>
              <span className={css.headingTitle}>
                {data.reverted ? '已撤销' : '已编辑'} {data.total} 个文件
              </span>
              <div className={css.totals}>
                <span className={css.totalAdd}>+{totalAdded}</span>
                <span className={css.totalDel}>-{totalDeleted}</span>
              </div>
            </div>
          </div>

          <div className={css.headerRight}>
            <button
              type="button"
              className={css.undoBtn}
              disabled={busy || running || !data.undo_available}
              title={running ? '任务结束后可撤销' : data.undo_available ? '撤销本轮记录的文件改动' : '已撤销或快照不完整，无法安全撤销'}
              onClick={() => setConfirmUndo(true)}
            >
              <span>撤销</span>
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round">
                <polyline points="1 4 1 10 7 10" />
                <path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" />
              </svg>
            </button>
            <button
              type="button"
              className={css.reviewBtn}
              onClick={() => setSelected(data.files[0]?.path ?? null)}
            >
              查看变更
            </button>
          </div>
        </header>

        {/* File List */}
        <div className={css.fileList}>
          {rows.map(file => {
            const slashIdx = file.path.lastIndexOf('/')
            const dir = slashIdx >= 0 ? file.path.slice(0, slashIdx + 1) : ''
            const name = slashIdx >= 0 ? file.path.slice(slashIdx + 1) : file.path

            return (
              <button
                type="button"
                className={css.fileRow}
                key={file.path}
                onClick={() => setSelected(file.path)}
                title={file.path}
              >
                <div className={css.filePath}>
                  {dir && <span className={css.fileDir}>{dir}</span>}
                  <span className={css.fileName}>{name}</span>
                  {file.created && <span className={clsx(css.tag, css.tagCreated)}>新建</span>}
                  {file.removed && <span className={clsx(css.tag, css.tagRemoved)}>删除</span>}
                  {file.reverted && <span className={clsx(css.tag, css.tagReverted)}>已撤销</span>}
                </div>
                <div className={css.fileStats}>
                  {file.unavailable ? (
                    <span className={css.fileStatNotice}>快照过大</span>
                  ) : file.binary ? (
                    <span className={css.fileStatNotice}>二进制</span>
                  ) : (
                    <>
                      <span className={css.fileAdd}>+{file.added}</span>
                      <span className={css.fileDel}>-{file.deleted}</span>
                    </>
                  )}
                  {file.coarse && <small className={css.coarse}> 粗略</small>}
                </div>
              </button>
            )
          })}
        </div>

        {/* Expand / Collapse Footer */}
        {data.files.length > INITIAL_VISIBLE_COUNT && (
          <button
            type="button"
            className={css.foldRow}
            onClick={() => setExpanded(!expanded)}
          >
            <span>{expanded ? '收起' : `再显示 ${remainingCount} 个文件`}</span>
            <svg
              width="13"
              height="13"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className={clsx(css.foldChevron, expanded && css.foldChevronExpanded)}
            >
              <polyline points="6 9 12 15 18 9" />
            </svg>
          </button>
        )}

        {data.files.some(file => file.conflicted) && (
          <p className={css.notice}>文件在操作之间被其他编辑修改，已禁用整轮撤销。</p>
        )}
        {!data.complete && (
          <p className={css.notice}>本轮改动记录不完整，已禁用整轮撤销。</p>
        )}

        {confirmUndo && (
          <div className={css.confirm} role="alert">
            <span className={css.confirmText}>恢复这轮修改前的内容？本轮新建文件会删除；后续改动冲突会阻止撤销。</span>
            <div className={css.confirmActions}>
              <button
                type="button"
                className={css.confirmBtn}
                disabled={busy || running}
                onClick={() => { void undo() }}
              >
                {busy ? '正在撤销…' : '确认撤销'}
              </button>
              <button
                type="button"
                className={css.cancelBtn}
                disabled={busy}
                onClick={() => setConfirmUndo(false)}
              >
                取消
              </button>
            </div>
          </div>
        )}

        {error && selected === null && <p className={css.error} role="alert">{error}</p>}
      </section>

      {/* Review Diff Modal */}
      {selected !== null && (
        <div className={css.backdrop} onMouseDown={event => { if (event.target === event.currentTarget) setSelected(null) }}>
          <aside ref={review} className={css.review} role="dialog" aria-modal="true" aria-label={`第 ${data.turn} 轮改动对比`}>
            <header className={css.reviewHeader}>
              <strong>第 {data.turn} 轮改动{data.reverted ? ' · 已撤销' : ''}</strong>
              <button ref={closeButton} aria-label="关闭变更详情" onClick={() => setSelected(null)}>✕</button>
            </header>
            <select aria-label="选择变更文件" value={selected} onChange={event => setSelected(event.target.value)}>
              {data.files.map(file => <option key={file.path} value={file.path}>{file.path}</option>)}
            </select>
            <div className={css.body}>
              {loading && <p role="status">正在读取改动…</p>}
              {error && <p role="alert" className={css.error}>{error}</p>}
              {diff?.unavailable ? (
                <p>文件超出快照上限，无法显示内容或撤销。</p>
              ) : diff?.binary ? (
                <p>二进制文件，无法显示文本 Diff。</p>
              ) : (
                hunks.length > 0 && <DiffBlock diffs={hunks} labels={labels} maxLines={120} />
              )}
            </div>
          </aside>
        </div>
      )}
    </>
  )
}
