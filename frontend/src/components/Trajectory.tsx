import { useMemo, useState, type CSSProperties } from 'react'
import clsx from 'clsx'
import { IconSearchOutlineRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import type { AgentEvent } from '../api.ts'
import viewsCss from '../dsh/trajectory/views.module.css'
import toolbarCss from '../dsh/trajectory/TrajectoryToolbar.module.css'
import timelineCss from '../dsh/trajectory/TrajectoryTimeline.module.css'
import tableCss from '../dsh/trajectory/TrajectoryTable.module.css'
import { t } from '../i18n.ts'
import { trajectoryRows, type TrajectoryRow } from '../model.ts'

function clock(time: number): string {
  return new Date(time).toLocaleTimeString('zh-CN', { hour12: false })
}

type SpanKind = 'user' | 'message' | 'tool' | 'subtool' | 'system'

function spanKindOf(row: TrajectoryRow): SpanKind {
  if (row.type === 'user/message') return 'user'
  if (row.type === 'assistant/message') return 'message'
  if (row.type.startsWith('subagent/')) return 'subtool'
  if (row.type.startsWith('tool/')) return 'tool'
  return 'system'
}

function laneOf(kind: SpanKind): number {
  if (kind === 'user' || kind === 'system') return 0
  if (kind === 'message') return 1
  return 2
}

function kindBadgeOf(row: TrajectoryRow): { label: string; className: string } {
  switch (row.type) {
    case 'user/message':
      return { label: 'USER', className: tableCss.user ?? '' }
    case 'assistant/message':
      return { label: 'MSG', className: tableCss.assistantVioletBright ?? '' }
    case 'tool/call':
    case 'tool/result':
      return { label: 'TOOL', className: tableCss.toolAmber ?? '' }
    case 'observer/advice_delivered':
    case 'observer/advice_response':
    case 'observer/advice_unanswered':
    case 'observer/inbox_state':
    case 'observer/node_review':
    case 'observer/brief':
    case 'observer/review':
    case 'observer/plan_review':
    case 'observer/progress_review':
    case 'observer/retrospective_start':
    case 'observer/retrospective':
    case 'observer/error':
    case 'observer/cancelled':
      return { label: 'OBS', className: tableCss.contextGreen ?? '' }
    case 'subagent/start':
    case 'subagent/end':
      return { label: 'SUB', className: tableCss.subtoolAmber ?? '' }
    case 'turn/start':
    case 'turn/end':
      return { label: 'TURN', className: tableCss.contextGreen ?? '' }
    default:
      return { label: 'STEP', className: tableCss.systemNeutral ?? '' }
  }
}

function titleOf(row: TrajectoryRow): string {
  const turn = row.turn ?? 1
  switch (row.type) {
    case 'turn/start': return t('trajectory.turnStart', { turn })
    case 'user/message': return t('trajectory.userMessage')
    case 'step/start': return t('trajectory.stepStart', { step: row.step ?? '' })
    case 'assistant/message': return t('trajectory.assistant')
    case 'tool/call': return row.name ?? 'tool'
    case 'tool/result': return row.name ?? t('trajectory.toolResult')
    case 'observer/advice_delivered': return 'Observer · 意见已送达'
    case 'observer/advice_response': return row.name === 'organizer' ? 'Organizer · 落实观察建议' : 'Worker · 历史观察回执'
    case 'observer/node_review': return 'Observer · 节点决策与路径复盘'
    case 'observer/advice_unanswered': return 'Observer · 尚未回应'
    case 'observer/inbox_state': return 'Observer · 意见状态'
    case 'worker/work_state': return 'Worker · 工作记录'
    case 'observer/brief': return 'Observer · 任务范围'
    case 'observer/review': return `Observer · ${row.name ?? 'review'}`
    case 'observer/plan_review': return 'Observer · 计划建议'
    case 'observer/progress_review': return 'Observer · 进度观察'
    case 'observer/retrospective_start': return 'Observer · 开始复盘'
    case 'observer/retrospective': return 'Observer · 工作路径复盘'
    case 'observer/error': return 'Observer 不可用'
    case 'observer/cancelled': return 'Observer 检查已取消'
    case 'step/end': return t('trajectory.stepEnd', { step: row.step ?? '' })
    case 'turn/end': return t('trajectory.turnEnd', { turn })
    case 'subagent/start': return t('trajectory.subagentStart')
    case 'subagent/end': return t('trajectory.subagentEnd')
    default: return row.type
  }
}

/** Authentic DSH Trajectory view: sticky Toolbar + 3-lane Timeline plot + split Ledger Table & Details Inspector. */
export function Trajectory({ events, onOpenTask }: {
  events: readonly AgentEvent[]
  onOpenTask: (taskId: string) => void
}) {
  const allRows = useMemo(() => trajectoryRows(events), [events])
  const [showTimeline, setShowTimeline] = useState(true)
  const [onlyTools, setOnlyTools] = useState(false)
  const [query, setQuery] = useState('')
  const [selectedKey, setSelectedKey] = useState<string | undefined>(undefined)
  const [detailTab, setDetailTab] = useState<'overview' | 'input' | 'output'>('overview')

  const filteredRows = useMemo(() => {
    const q = query.trim().toLowerCase()
    return allRows.filter((row) => {
      if (onlyTools && !row.type.startsWith('tool/') && !row.type.startsWith('subagent/') && !row.type.startsWith('observer/')) {
        return false
      }
      if (q === '') return true
      return (
        row.type.toLowerCase().includes(q)
        || (row.name ?? '').toLowerCase().includes(q)
        || row.detail.toLowerCase().includes(q)
        || (row.input ?? '').toLowerCase().includes(q)
      )
    })
  }, [allRows, onlyTools, query])

  const selectedRow = useMemo(
    () => filteredRows.find(r => r.key === selectedKey) ?? allRows.find(r => r.key === selectedKey),
    [filteredRows, allRows, selectedKey],
  )

  return (
    <div className={viewsCss.root}>
      {/* DSH Trajectory Toolbar */}
      <div className={toolbarCss.root}>
        <div className={toolbarCss.inner}>
          <div className={toolbarCss.actions}>
            <button
              type="button"
              className={toolbarCss.toggle}
              aria-pressed={showTimeline}
              onClick={() => { setShowTimeline(v => !v) }}
            >
              <svg viewBox="0 0 12 12" fill="none" className={toolbarCss.toggleIcon}>
                <path d="M1.5 3h4M6.5 6h4M3 9h5.5" />
              </svg>
              <span>时间轴</span>
            </button>
            <button
              type="button"
              className={toolbarCss.toggle}
              aria-pressed={onlyTools}
              onClick={() => { setOnlyTools(v => !v) }}
            >
              <span>仅看工具</span>
            </button>
          </div>

          <div className={toolbarCss.search}>
            <IconSearchOutlineRegular size={12} className={toolbarCss.searchIcon} />
            <input
              type="search"
              className={toolbarCss.searchInput}
              placeholder="过滤事件、工具或内容..."
              value={query}
              onChange={(e) => { setQuery(e.target.value) }}
            />
          </div>
        </div>
      </div>

      {/* DSH 3-Lane Timeline Waterfall */}
      {showTimeline && (
        <div className={timelineCss.root}>
          <div className={timelineCss.plot}>
            <div className={timelineCss.labels}>
              <span>TURN</span>
              <span>LLM</span>
              <span>TOOL</span>
            </div>
            <div className={timelineCss.track}>
              {allRows.length === 0
                ? <div className={timelineCss.empty}>{t('app.waiting')}</div>
                : (
                    <div
                      className={timelineCss.lanes}
                      style={{
                        '--trajectory-domain-left': '8px',
                        '--trajectory-domain-width': 'calc(100% - 16px)',
                      } as CSSProperties}
                    >
                      {allRows.map((row, idx) => {
                        const kind = spanKindOf(row)
                        const lane = laneOf(kind)
                        const total = Math.max(allRows.length, 1)
                        const leftPct = (idx / total) * 100
                        const widthPct = Math.max(100 / total, 1.2)
                        const isSelected = selectedKey === row.key
                        return (
                          <div
                            key={row.key}
                            className={timelineCss.span}
                            data-timeline-span={kind === 'system' ? 'context' : kind}
                            data-error={row.failed ? 'true' : undefined}
                            data-current={isSelected ? 'true' : undefined}
                            title={`${titleOf(row)} (${clock(row.time)})`}
                            onClick={() => { setSelectedKey(row.key) }}
                            style={{
                              '--trajectory-span-lane': lane,
                              '--trajectory-span-left': `${leftPct}%`,
                              '--trajectory-span-width': `${widthPct}%`,
                              '--trajectory-span-gap': '1px',
                              cursor: 'pointer',
                            } as CSSProperties}
                          />
                        )
                      })}
                    </div>
                  )}
            </div>
          </div>
        </div>
      )}

      {/* DSH Trajectory Ledger Split: Table + Right Details Inspector */}
      <div className={viewsCss.ledger}>
        <div className={tableCss.split}>
          <div className={tableCss.tablePane}>
            <table className={tableCss.table} data-scroll-ready="true">
              <thead>
                <tr>
                  <th className={clsx(tableCss.eventColumn, tableCss.eventHeader)}>事件</th>
                  <th className={tableCss.contentColumn}>内容</th>
                </tr>
              </thead>
              <tbody>
                {filteredRows.length === 0 && (
                  <tr>
                    <td colSpan={2} style={{ textAlign: 'center', color: 'var(--dsw-alias-label-caption)', padding: '24px 0' }}>
                      {t('app.waiting')}
                    </td>
                  </tr>
                )}
                {filteredRows.map((row, index) => {
                  const prevTurn = index > 0 ? filteredRows[index - 1]?.turn : undefined
                  const isTurnStart = row.type === 'turn/start' || (row.turn !== undefined && row.turn !== prevTurn)
                  const badge = kindBadgeOf(row)
                  const isSelected = selectedKey === row.key
                  const spanKind = spanKindOf(row)
                  const singleLineDetail = row.detail.replace(/\s+/g, ' ').trim()

                  return (
                    <tr
                      key={row.key}
                      data-selected={isSelected ? 'true' : undefined}
                      data-turn-start={isTurnStart ? 'true' : undefined}
                      data-error={row.failed ? 'true' : undefined}
                      data-kind={spanKind}
                      onClick={() => { setSelectedKey(prev => (prev === row.key ? undefined : row.key)) }}
                    >
                      <td className={tableCss.event}>
                        <span className={tableCss.turnRail} />
                        {isSelected && <span className={tableCss.selectionRail} />}
                        {isTurnStart && row.turn !== undefined && (
                          <span className={clsx(tableCss.turnLabel, tableCss.turnLabelActive)}>
                            <span className={tableCss.turnLabelFull}>T{row.turn}</span>
                            <span className={tableCss.turnLabelCompact}>T{row.turn}</span>
                          </span>
                        )}
                        <div className={tableCss.eventInner}>
                          <div className={tableCss.kindSlot}>
                            <span className={clsx(tableCss.kindTag, badge.className)}>
                              <span className={tableCss.kindTagLabel}>{badge.label}</span>
                            </span>
                          </div>
                        </div>
                      </td>
                      <td className={tableCss.content}>
                        {spanKind === 'tool' || spanKind === 'subtool'
                          ? (
                              <div className={tableCss.resultPreview}>
                                <span className={tableCss.resultRequest}>
                                  <span className={tableCss.toolCallNameTypeface}>{titleOf(row)}</span>
                                  {row.input !== undefined && (
                                    <span className={tableCss.toolCallPayload}>
                                      {row.input.replace(/\s+/g, ' ').slice(0, 120)}
                                    </span>
                                  )}
                                </span>
                                <span className={clsx(tableCss.inlineResult, row.failed && tableCss.error)}>
                                  <span className={tableCss.arrow}>→</span>
                                  <span className={tableCss.inlineResultText}>
                                    {singleLineDetail || (row.durationMs !== undefined ? `${row.durationMs} ms` : '完成')}
                                  </span>
                                </span>
                              </div>
                            )
                          : (
                              <span className={clsx(tableCss.contentText, row.failed && tableCss.error)}>
                                <strong style={{ fontWeight: 500, marginRight: 8 }}>{titleOf(row)}</strong>
                                <span style={{ color: 'var(--dsw-alias-label-secondary)' }}>{singleLineDetail}</span>
                              </span>
                            )}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>

          {/* Right-side DSH Inspector Panel when a row is selected */}
          {selectedRow !== undefined && (
            <aside className={tableCss.details}>
              <div className={tableCss.detailsHeader}>
                <div className={tableCss.detailsTitle}>
                  <span className={tableCss.requestDetailsDot} />
                  <span className={tableCss.requestDetailsName}>{titleOf(selectedRow)}</span>
                  <span className={tableCss.detailsLocation}>{clock(selectedRow.time)}</span>
                </div>
                <button
                  type="button"
                  className={tableCss.close}
                  aria-label="关闭详情"
                  onClick={() => { setSelectedKey(undefined) }}
                >
                  ×
                </button>
              </div>

              <div className={tableCss.detailTabs} role="tablist">
                <button
                  type="button"
                  role="tab"
                  aria-selected={detailTab === 'overview'}
                  className={clsx(tableCss.detailTab, detailTab === 'overview' && tableCss.detailTabActive)}
                  onClick={() => { setDetailTab('overview') }}
                >
                  概览
                </button>
                {selectedRow.input !== undefined && (
                  <button
                    type="button"
                    role="tab"
                    aria-selected={detailTab === 'input'}
                    className={clsx(tableCss.detailTab, detailTab === 'input' && tableCss.detailTabActive)}
                    onClick={() => { setDetailTab('input') }}
                  >
                    {t('row.input')}
                  </button>
                )}
                {selectedRow.detail !== '' && (
                  <button
                    type="button"
                    role="tab"
                    aria-selected={detailTab === 'output'}
                    className={clsx(tableCss.detailTab, detailTab === 'output' && tableCss.detailTabActive)}
                    onClick={() => { setDetailTab('output') }}
                  >
                    {t('row.output')}
                  </button>
                )}
              </div>

              <div className={tableCss.detailBody}>
                {detailTab === 'overview' && (
                  <div className={tableCss.detailBodySummary}>
                    <dl className={tableCss.overview}>
                      <div>
                        <dt>事件类型</dt>
                        <dd>{selectedRow.type}</dd>
                      </div>
                      {selectedRow.turn !== undefined && (
                        <div>
                          <dt>对话轮次</dt>
                          <dd>Turn {selectedRow.turn}</dd>
                        </div>
                      )}
                      {selectedRow.name !== undefined && (
                        <div>
                          <dt>工具名称</dt>
                          <dd>{selectedRow.name}</dd>
                        </div>
                      )}
                      <div>
                        <dt>记录时间</dt>
                        <dd>{clock(selectedRow.time)}</dd>
                      </div>
                      {selectedRow.durationMs !== undefined && (
                        <div>
                          <dt>执行耗时</dt>
                          <dd>{selectedRow.durationMs} ms</dd>
                        </div>
                      )}
                      <div>
                        <dt>执行状态</dt>
                        <dd className={selectedRow.failed ? tableCss.error : undefined}>
                          {selectedRow.failed ? '失败 (Error)' : '正常 (OK)'}
                        </dd>
                      </div>
                      {selectedRow.childTaskId !== undefined && (
                        <div>
                          <dt>子代理会话</dt>
                          <dd>
                            <button
                              type="button"
                              className={tableCss.overviewHierarchyNavLink}
                              onClick={() => { onOpenTask(selectedRow.childTaskId as string) }}
                            >
                              <span>{t('app.viewSubagent')} →</span>
                            </button>
                          </dd>
                        </div>
                      )}
                    </dl>
                    {selectedRow.input !== undefined && (
                      <div className={tableCss.promptDiffSections}>
                        <div className={tableCss.promptDiffSection}>
                          <h4 className={tableCss.promptDiffTitle}>{t('row.input')}</h4>
                          <pre className={tableCss.promptDiff}>{selectedRow.input}</pre>
                        </div>
                      </div>
                    )}
                    {selectedRow.detail !== '' && (
                      <div className={tableCss.promptDiffSections}>
                        <div className={tableCss.promptDiffSection}>
                          <h4 className={tableCss.promptDiffTitle}>{t('row.output')}</h4>
                          <pre className={clsx(tableCss.promptDiff, selectedRow.failed && tableCss.error)}>
                            {selectedRow.detail}
                          </pre>
                        </div>
                      </div>
                    )}
                  </div>
                )}

                {detailTab === 'input' && selectedRow.input !== undefined && (
                  <div className={tableCss.promptDiffSections}>
                    <pre className={tableCss.promptDiff}>{selectedRow.input}</pre>
                  </div>
                )}

                {detailTab === 'output' && (
                  <div className={tableCss.promptDiffSections}>
                    <pre className={clsx(tableCss.promptDiff, selectedRow.failed && tableCss.error)}>
                      {selectedRow.detail}
                    </pre>
                  </div>
                )}
              </div>
            </aside>
          )}
        </div>
      </div>
    </div>
  )
}
