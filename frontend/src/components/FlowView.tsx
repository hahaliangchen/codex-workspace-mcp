import { useEffect, useMemo, useState } from 'react'
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  type Node,
  type Edge,
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'

import type { AgentEvent, Task } from '../api.ts'
import {
  chatItems,
  flowState,
  formatDuration,
  reviewMatchesNode,
  type FlowNodeData,
  type FlowObserverNote,
} from '../model.ts'
import { useTaskDuration } from '../useTaskDuration.ts'
import { layoutAgentFlow } from '../flowLayout.ts'
import { TaskNode } from './TaskNode.tsx'
import { RequestContextPanel } from './RequestContextPanel.tsx'
import { VisualMaterials } from './VisualMaterials.tsx'
import { useAgentApi } from '../cordis/react.tsx'
import { SkillDraftDialog, skillDraftInput } from './SkillDraftDialog.tsx'
import type { SkillDraftInput } from '../api.ts'
import styles from './FlowView.module.css'

const nodeTypes = {
  taskNode: TaskNode,
}
const reviewStatusLabels: Record<string, string> = { pending: '待评估', reviewing: '观察中', completed: '已评估', unassessed: '未评估', timeout: '超时未评估', cancelled: '已取消', failed: '观察失败', merged: '已并入交付复盘', superseded: '原版本历史' }
const reviewReasonLabels: Record<string, string> = { completed: '执行已结束，观察未完成。', max_steps: '执行步数已耗尽，观察未完成。', cancelled: '请求已取消。', interrupted: '执行中断，观察未完成。', failed: '执行已结束，观察未完成。', 'observation stopped': '观察已停止，尚无完成的 AI 判断。', 'included in handoff': '派发观察已合并到交付复盘。' }

interface FlowViewProps {
  events: readonly AgentEvent[]
  task?: Task | undefined
  running?: boolean | undefined
  onInterruptNode?: ((nodeId: string) => void) | undefined
}

function isNoteMatchingNode(note: FlowObserverNote, node: FlowNodeData): boolean {
  if (note.turn !== node.turn) return false
  if (note.nodeId) {
    const rawId = node.id.includes(':') ? node.id.split(':').slice(1).join(':') : node.id
    return (
      note.nodeId === node.id ||
      note.nodeId === rawId ||
      node.id.endsWith(`:${note.nodeId}`)
    )
  }
  if (note.turn !== undefined && node.turn !== undefined) {
    return note.turn === node.turn
  }
  return false
}

export function FlowView({ events, task, running, onInterruptNode }: FlowViewProps) {
  const api = useAgentApi()
  const [debug, setDebug] = useState(false)
  const [debugBusy, setDebugBusy] = useState(true)
  const [debugError, setDebugError] = useState('')
  const [showRequests, setShowRequests] = useState(false)
  useEffect(() => {
    let alive = true
    setDebug(false); setDebugBusy(true); setShowRequests(false); setDebugError('')
    if (!task) { setDebugBusy(false); return }
    void api.contextDebug(task.task_id).then(result => { if (alive) setDebug(result.enabled) })
      .catch(reason => { if (alive) setDebugError(String(reason)) })
      .finally(() => { if (alive) setDebugBusy(false) })
    return () => { alive = false }
  }, [api, task?.task_id])

  async function toggleDebug() {
    if (!task || debugBusy) return
    setDebugBusy(true); setDebugError('')
    try {
      const result = await api.setContextDebug(task.task_id, !debug)
      setDebug(result.enabled)
      if (!result.enabled) setShowRequests(false)
    } catch (reason) { setDebugError(String(reason)) } finally { setDebugBusy(false) }
  }
  const flow = useMemo(() => flowState(events), [events])
  // Initially null so the user gets the full unobstructed Flow diagram.
  // Clicking any node opens the detail drawer on the right.
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null)
  // Picking mode: clicks collect steps to save as a project skill instead of opening details.
  const [picking, setPicking] = useState(false)
  const [picked, setPicked] = useState<ReadonlySet<string>>(new Set())
  const [skillInput, setSkillInput] = useState<SkillDraftInput>()
  useEffect(() => { setSelectedNodeId(null); setPicking(false); setPicked(new Set()) }, [task?.task_id, flow.organizedTurn])
  const canSaveSkill = !!task?.workspace && !running
  useEffect(() => {
    if (!selectedNodeId && !showRequests) return
    const close = (event: KeyboardEvent) => { if (event.key === 'Escape') { setSelectedNodeId(null); setShowRequests(false) } }
    window.addEventListener('keydown', close)
    return () => window.removeEventListener('keydown', close)
  }, [selectedNodeId, showRequests])

  // Stable execution-window columns, with dependency edges preserved.
  const { nodes, edges } = useMemo(() => {
    const currentNodes = flow.nodes.filter(node => node.turn === flow.organizedTurn)
    const nodeOrder = new Map(currentNodes.map((node, index) => [node.id, index]))
    const currentEdges = flow.edges.filter(edge => nodeOrder.has(edge.source) && nodeOrder.has(edge.target))
    const positions = layoutAgentFlow(currentNodes, currentEdges)
    const flowNodes: Node[] = currentNodes.map((n) => ({
      id: n.id,
      type: 'taskNode',
      position: positions.get(n.id) ?? { x: 60, y: 100 },
      data: n,
      selected: picking ? picked.has(n.id) : n.id === selectedNodeId,
    }))

    const flowEdges: Edge[] = currentEdges.map(e => ({
      id: e.id, source: e.source, target: e.target, type: 'smoothstep',
      sourceHandle: 'source-bottom', targetHandle: 'target-top',
      pathOptions: { stepPosition: positions.get(e.source)?.x === positions.get(e.target)?.x ? 0.5 : 0.25 },
      label: e.label, style: { stroke: '#64748b', strokeWidth: 2 },
    }))

    return { nodes: flowNodes, edges: flowEdges }
  }, [flow, selectedNodeId, picking, picked])
  const togglePicked = (id: string) => setPicked(current => {
    const next = new Set(current)
    if (!next.delete(id)) next.add(id)
    return next
  })
  const openSkillDraft = () => {
    const steps = flow.nodes.filter(node => picked.has(node.id))
    const goal = chatItems(events, task).flatMap(item => item.kind === 'user' ? [item.text] : []).slice(-3).join('\n---\n')
    setSkillInput(skillDraftInput(goal, steps))
  }

  const durationInfo = useTaskDuration(events, task, running)
  const selectedNode = useMemo(() => {
    if (!selectedNodeId) return null
    return flow.nodes.find((n) => n.id === selectedNodeId) ?? null
  }, [flow.nodes, selectedNodeId])

  const activeNode = useMemo(() => {
    const current = flow.nodes.filter(node => node.turn === flow.organizedTurn)
    return (
      current.find(node => node.id === flow.activeNodeId) ??
      [...current].reverse().find(node => node.status === 'running')
    )
  }, [flow.activeNodeId, flow.nodes, flow.organizedTurn])

  const nodeReviews = useMemo(() => selectedNode ? flow.nodeReviews.filter(review => reviewMatchesNode(review, selectedNode)) : [], [flow.nodeReviews, selectedNode])
  const currentReview = activeNode ? [...flow.nodeReviews].reverse().find(review => reviewMatchesNode(review, activeNode)) : undefined
  // Observer notes relevant to selectedNode:
  // 1. Direct match with selectedNode
  // 2. Or same turn / plan notes if no direct match
  const nodeObserverNotes = useMemo(() => {
    if (!selectedNode) return []
    const specific = flow.observerNotes.filter(n => isNoteMatchingNode(n, selectedNode))
    if (specific.length > 0) return specific
    return []
  }, [flow.observerNotes, selectedNode])

  // If nodeObserverNotes is empty, but there are global observer notes, we can also offer them
  const fallbackGlobalNotes = useMemo<FlowObserverNote[]>(() => {
    if (!selectedNode || nodeObserverNotes.length > 0) return []
    return []
  }, [selectedNode, nodeObserverNotes, flow.observerNotes])

  return (
    <div className={styles.container}>
      {currentReview && <div className={styles.reviewStrip}>观察对象：{currentReview.identity.work_id}@{currentReview.identity.revision} · {reviewStatusLabels[currentReview.status] ?? currentReview.status} · {String(currentReview.result.summary ?? '节点评估不阻塞执行')}</div>}
      {/* Sleek Top Status Bar (Low height ~40px, never pushes canvas) */}
      <div className={styles.topBar}>
        <div className={styles.flowSummary}>
          <span className={styles.flowIcon}>↗</span>
          <div className={styles.flowSummaryText}>
            <strong>Agent 请求流程</strong>
            <span className={styles.flowSubtext}>每列对应一个任务分支，每个节点对应一次模型请求；上下文压缩单独分支显示</span>
          </div>
        </div>

        <div className={styles.topBarActions}>
          <button type="button" className={styles.topBarPillBtn} disabled={debugBusy || !task} aria-pressed={debug} onClick={() => { void toggleDebug() }} title="显示或隐藏上下文详情；规划、执行和观察的完整请求始终记录并在任务结束后保留">{debug ? '上下文：显示' : '上下文：隐藏'}</button>
          {debug && <button type="button" className={styles.topBarPillBtn} onClick={() => setShowRequests(true)}>全部请求</button>}
          {debugError && <span role="alert" title={debugError}>上下文显示设置失败</span>}
          {canSaveSkill && nodes.length > 0 && <button type="button" className={`${styles.topBarPillBtn} ${picking ? styles.topBarPillActive : ''}`} aria-pressed={picking}
            title="选取一段步骤，提炼成项目技能，以后遇到类似任务按这个流程走"
            onClick={() => { setPicking(value => !value); setPicked(new Set()); setSelectedNodeId(null); setShowRequests(false) }}>
            {picking ? '退出选取' : '选取流程'}
          </button>}
          {activeNode && (
            <button
              type="button"
              className={`${styles.topBarPillBtn} ${selectedNodeId === activeNode.id ? styles.topBarPillActive : ''}`}
              onClick={() => { setSelectedNodeId(activeNode.id); setShowRequests(false) }}
              title="点击查看当前进行中节点详情"
            >
              <span className={styles.pulseDot} />
              <span>当前步骤: #{activeNode.id.split(':').pop()} {activeNode.title}</span>
            </button>
          )}

          {flow.observerEnabled && flow.observerNotes.length > 0 && (
            <button
              type="button"
              className={styles.observerPillBtn}
              onClick={() => {
                const target = activeNode ?? flow.nodes[0]
                if (target) setSelectedNodeId(target.id)
                setShowRequests(false)
              }}
              title="点击查看 Observer 全局观察与建议"
            >
              <span className={styles.observerEyeIcon}>👁</span>
              <span>Observer 建议 ({flow.observerNotes.length})</span>
            </button>
          )}

          {durationInfo.currentTurnDurationMs !== undefined && (
            <div
              className={styles.timeBadge}
              title={durationInfo.isRunning ? '任务正在执行中' : '本次任务执行耗时'}
            >
              <span className={styles.timeIcon}>⏱</span>
              <span>
                {durationInfo.isRunning
                  ? `进行中 ${formatDuration(durationInfo.currentTurnDurationMs)}`
                  : `耗时 ${formatDuration(durationInfo.currentTurnDurationMs)}`}
              </span>
            </div>
          )}

          <div className={styles.legend}>
            <span className={styles.legendItem}>
              <span className={styles.legendDotRunning}>⟳</span> 进行中
            </span>
            <span className={styles.legendItem}>
              <span className={styles.legendDotCompleted}>↩</span> 执行已结束
            </span>
            <span className={styles.legendItem}>
              <span className={styles.legendDotPending}>○</span> 待执行
            </span>
            <span className={styles.legendItem}>
              <span className={styles.legendDotPending}>○</span> 待请求
            </span>
          </div>
        </div>
      </div>


      {/* Main Body: Full-bleed ReactFlow Canvas */}
      <div className={styles.canvasWrapper}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodeClick={(_, node) => { if (picking) { togglePicked(node.id); return } setSelectedNodeId(node.id); setShowRequests(false) }}
          onPaneClick={() => { if (picking) return; setSelectedNodeId(null); setShowRequests(false) }}
          fitView
          fitViewOptions={{ padding: 0.2 }}
          minZoom={0.2}
          maxZoom={1.5}
        >
          <Background color="#cbd5e1" gap={18} size={1} />
          <Controls />
          <MiniMap
            nodeStrokeWidth={3}
            zoomable
            pannable
            nodeColor={(node) => {
              const d = node.data as unknown as FlowNodeData | undefined
              if (d?.status === 'deprecated') return '#9ca3af'
              if (d?.status === 'running') return '#3b82f6'
              if (d?.status === 'done' || d?.status === 'completed') return '#94a3b8'
              if (d?.status === 'failed') return '#ef4444'
              if (d?.status === 'interrupted') return '#f59e0b'
              return '#94a3b8'
            }}
          />
        </ReactFlow>

        {picking && <div className={styles.pickBar} role="status">
          <span>{picked.size === 0 ? '点击节点，选出要保存的步骤（失败的步骤也可以选，会被写成注意事项）' : `已选 ${picked.size} 个步骤`}</span>
          {picked.size > 0 && <button type="button" className={styles.pickBarLink} onClick={() => setPicked(new Set())}>清空</button>}
          <button type="button" className={styles.pickBarPrimary} disabled={picked.size === 0} onClick={openSkillDraft}>存为技能</button>
        </div>}
        {task && <SkillDraftDialog taskId={task.task_id} input={skillInput} onClose={() => setSkillInput(undefined)} onSaved={() => { setPicking(false); setPicked(new Set()) }} />}

        {/* Selected Node Detail Drawer (slides in from right on node click) */}
        {debug && showRequests && task && <div className={styles.drawer}>
          <div className={styles.drawerHeader}>
            <span className={styles.drawerTitle}>上下文 · 全部模型请求</span>
            <button type="button" className={styles.closeButton} onClick={() => { setShowRequests(false); setSelectedNodeId(null) }} title="关闭请求详情">✕</button>
          </div>
          <div className={styles.drawerContent}><RequestContextPanel taskId={task.task_id} events={events} /></div>
        </div>}
        {selectedNode && !showRequests && (
          <div className={styles.drawer}>
            <div className={styles.drawerHeader}>
              <div className={styles.drawerTitleGroup}>
                <span className={styles.drawerTitle}>
                  #{selectedNode.id} {selectedNode.title}
                </span>
              </div>
              <button
                type="button"
                className={styles.closeButton}
                onClick={() => setSelectedNodeId(null)}
                title="关闭详情"
              >
                ✕
              </button>
            </div>

            <div className={styles.drawerContent}>
              {/* Meta bar: Node status & turn */}
              <div className={styles.nodeMetaBar}>
                <span className={`${styles.nodeStatusBadge} ${styles['status_' + selectedNode.status] ?? ''}`}>
                  {selectedNode.status === 'running' && '⟳ 进行中'}
                  {(selectedNode.status === 'done' || selectedNode.status === 'completed') && '↩ 执行已结束'}
                  {(selectedNode.status === 'ready' || selectedNode.status === 'pending') && '○ 待执行'}
                  {selectedNode.status === 'paused' && '⏸ 待继续'}
                  {selectedNode.status === 'blocked' && '⚠ 受阻'}
                  {selectedNode.status === 'interrupted' && '⏸ 已暂停 / 中断'}
                  {selectedNode.status === 'failed' && '✕ 失败'}
                  {selectedNode.status === 'skipped' && '— 已跳过'}
                </span>
                {selectedNode.revision && selectedNode.revision > 1 && (
                  <span className={styles.revisionBadge}>版本 r{selectedNode.revision}</span>
                )}
                {selectedNode.planRevision && selectedNode.planRevision > 1 && (
                  <span className={styles.planRevisionBadge}>Plan Rev {selectedNode.planRevision}</span>
                )}
                {selectedNode.kind && (
                  <span className={styles.nodeKindBadge}>
                    类型: {selectedNode.kind}
                  </span>
                )}
                {selectedNode.turn !== undefined && (
                  <span className={styles.turnBadge}>Turn {selectedNode.turn}</span>
                )}
              </div>

              <VisualMaterials events={events} node={selectedNode} />
              {debug && task && <><RequestContextPanel key={selectedNode.id} taskId={task.task_id} events={events} nodeId={selectedNode.id} turn={selectedNode.turn} requestId={selectedNode.requestId} revision={selectedNode.revision} planRevision={selectedNode.planRevision} /></>}

              <div className={styles.section}>
                <div className={styles.sectionTitle}>决策与路径复盘 <span className={styles.badgeCount}>({nodeReviews.length})</span></div>
                {nodeReviews.length === 0 ? <div className={styles.emptyHint}>本实例尚无复盘记录；执行无需等待观察。</div> : nodeReviews.map(review => {
                  const assessments: Record<string, string> = { on_track: '路线符合目标', needs_adjustment: '建议调整组织安排', uncertain: '现有事实不足以判断' }
                  const recommendations = Array.isArray(review.result.recommendations) ? review.result.recommendations as Record<string, unknown>[] : []
                  return <div key={review.id} className={styles.observerNoteCard}>
                    <div className={styles.observerNoteHeader}><span className={styles.observerKindTag}>{review.stage === 'assignment' ? '派发观察' : review.stage === 'handoff' ? '交付复盘' : '异常进度观察'} · {reviewStatusLabels[review.status] ?? review.status}</span></div>
                    <p>组织理由：{String(review.decision.reason ?? '未记录')}</p>
                    {review.delivery.summary ? <p>交付：{String(review.delivery.summary)}</p> : null}
                    {review.status === 'completed' ? <><p>{assessments[String(review.result.assessment)] ?? '未确定判断'} · {String(review.result.summary ?? '')}</p>
                      <ul>{recommendations.map((item, index) => <li key={index}>{String(item.adjustment ?? '')}</li>)}</ul></> : <p>{String(review.result.message ?? reviewReasonLabels[String(review.result.reason)] ?? review.result.reason ?? '本记录尚无完成的 AI 判断。')}</p>}
                    {review.responses.map((response, index) => <p key={index}>Organizer 处理：{String(response.disposition)} · {String(response.reason)}<br />落实决策：{String(response.decision_id ?? '')} · {JSON.stringify(response.application ?? {})}</p>)}
                    {review.result.lessons_recorded ? <details><summary>经验记录</summary><pre>{JSON.stringify(review.result.lessons_recorded, null, 2)}</pre></details> : null}
                    <div className={styles.reviewMeta}>请求 {review.identity.request_id} · {review.identity.work_id}@{review.identity.revision} · 计划 {review.identity.plan_revision} · 来源 {review.sourceEventId}</div>
                  </div>
                })}
              </div>

              {/* 2. Observer 观察与建议 (Observer Notes) */}
              {nodeObserverNotes.length > 0 && <div className={styles.section}>
                <div className={styles.sectionTitle}>
                  <span>👁</span>
                  <span>历史 Observer 观察与建议</span>
                  {nodeObserverNotes.length > 0 && (
                    <span className={styles.badgeCount}>({nodeObserverNotes.length})</span>
                  )}
                </div>
                {nodeObserverNotes.length > 0 ? (
                  <div className={styles.observerNotesList}>
                    {nodeObserverNotes.map((note, idx) => (
                      <div key={idx} className={styles.observerNoteCard}>
                        <div className={styles.observerNoteHeader}>
                          <span className={styles.observerKindTag}>
                            {note.kind === 'plan' ? '计划观察' : '持续观察'}
                          </span>
                          {note.nodeId && (
                            <span className={styles.observerNodeTag}>#{note.nodeId}</span>
                          )}
                        </div>

                        {note.unavailable ? (
                          <div className={styles.observerUnavailable}>{note.unavailable}</div>
                        ) : (
                          <>
                            {note.summary && (
                              <p className={styles.observerSummaryText}>{note.summary}</p>
                            )}
                            {note.suggestions.length > 0 && (
                              <div className={styles.observerSuggestionsBox}>
                                <div className={styles.observerSuggestionsTitle}>建议与改进方向：</div>
                                <ul className={styles.observerSuggestionsList}>
                                  {note.suggestions.map((sug, sIdx) => (
                                    <li key={sIdx}>{sug}</li>
                                  ))}
                                </ul>
                              </div>
                            )}
                            {!note.summary && note.suggestions.length === 0 && (
                              <div className={styles.observerNeutralTip}>本记录没有实质建议，不代表完成了问题检查。</div>
                            )}
                          </>
                        )}
                      </div>
                    ))}
                  </div>
                ) : fallbackGlobalNotes.length > 0 ? (
                  <div className={styles.observerNotesList}>
                    <div className={styles.emptyHintSubtle}>暂无针对本节点的专属观察，以下为全局观察建议：</div>
                    {fallbackGlobalNotes.map((note, idx) => (
                      <div key={idx} className={styles.observerNoteCard}>
                        <div className={styles.observerNoteHeader}>
                          <span className={styles.observerKindTag}>
                            {note.kind === 'plan' ? '全局计划' : '持续观察'}
                          </span>
                        </div>
                        {note.summary && <p className={styles.observerSummaryText}>{note.summary}</p>}
                        {note.suggestions.length > 0 && (
                          <ul className={styles.observerSuggestionsList}>
                            {note.suggestions.map((sug, sIdx) => <li key={sIdx}>{sug}</li>)}
                          </ul>
                        )}
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className={styles.emptyHint}>
                    {flow.observerEnabled
                      ? '本节点没有旧格式观察记录。'
                      : '观察者模式未启用。'}
                  </div>
                )}
              </div>}

              {/* 3. Observer 工作路径复盘 (如果存在复盘) */}
              {flow.observerRetrospective && (
                <div className={styles.section}>
                  <div className={styles.sectionTitle}>
                    <span>🧭</span>
                    <span>Observer 工作路径复盘</span>
                  </div>
                  <div className={styles.retrospectiveCard}>
                    {flow.observerRetrospective.status === 'reviewing' && (
                      <div className={styles.retrospectiveStatusTip}>⏳ 任务已结束，正在复盘工作路径…</div>
                    )}
                    {flow.observerRetrospective.summary && (
                      <p className={styles.retrospectiveSummary}>{flow.observerRetrospective.summary}</p>
                    )}
                    {flow.observerRetrospective.pathReview && (
                      <p className={styles.retrospectivePath}>{flow.observerRetrospective.pathReview}</p>
                    )}

                    {flow.observerRetrospective.shorteningOpportunities.length > 0 && (
                      <div className={styles.retrospectiveBlock}>
                        <div className={styles.retrospectiveSubtitle}>下次可缩短</div>
                        <ul className={styles.retrospectiveList}>
                          {flow.observerRetrospective.shorteningOpportunities.map((item, idx) => (
                            <li key={idx}>{item}</li>
                          ))}
                        </ul>
                      </div>
                    )}

                    {flow.observerRetrospective.workFindings.length > 0 && (
                      <div className={styles.retrospectiveBlock}>
                        <div className={styles.retrospectiveSubtitle}>项目事实与待核对结论</div>
                        <ul className={styles.retrospectiveList}>
                          {flow.observerRetrospective.workFindings.map((item, idx) => (
                            <li key={idx}>
                              <strong>{item.certainty === 'confirmed' ? '已确认' : '待核对'}</strong> · {item.finding}
                              {item.relevance && ` (适用: ${item.relevance})`}
                              {item.sourceRefs.length > 0 && ` [来源: ${item.sourceRefs.join('、')}]`}
                              {item.watchFor && ` · 边界: ${item.watchFor}`}
                            </li>
                          ))}
                        </ul>
                      </div>
                    )}

                    {flow.observerRetrospective.routeShortcuts.length > 0 && (
                      <div className={styles.retrospectiveBlock}>
                        <div className={styles.retrospectiveSubtitle}>下次可复用的路径</div>
                        <ul className={styles.retrospectiveList}>
                          {flow.observerRetrospective.routeShortcuts.map((route, idx) => (
                            <li key={idx}>
                              <strong>{route.situation}</strong>：先看 {route.lookFirst}
                              {route.avoid && `；避免 ${route.avoid}`}
                            </li>
                          ))}
                        </ul>
                      </div>
                    )}

                    {flow.observerRetrospective.memoryRecorded && (
                      <div className={styles.memoryStatus}>
                        ✓ 已写入工作记忆：{flow.observerRetrospective.memorySummary}
                      </div>
                    )}
                    {flow.observerRetrospective.memoryError && (
                      <div className={styles.memoryError}>
                        工作记忆未能写入：{flow.observerRetrospective.memoryError}
                      </div>
                    )}
                  </div>
                </div>
              )}

              {/* 4. 步骤目标与说明 */}
              <div className={styles.section}>
                <div className={styles.sectionTitle}>
                  <span>📋</span>
                  <span>步骤说明</span>
                </div>
                <div className={styles.contextBox}>
                  {selectedNode.description || '本次模型请求未提供额外说明。'}
                </div>
              </div>

              {/* 5. 观察者问答 (Consults) */}
              {selectedNode.consults.length > 0 && (
                <div className={styles.section}>
                  <div className={styles.sectionTitle}>
                    <span>💬</span>
                    <span>观察者问答 ({selectedNode.consults.length})</span>
                  </div>
                  <div className={styles.consultList}>
                    {selectedNode.consults.map((consult, idx) => (
                      <div key={idx} className={styles.consultCard}>
                        <div className={styles.consultQuestion}>问：{consult.question}</div>
                        <div className={styles.consultAnswer}>答：{consult.answer || '等待回复…'}</div>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* 6. 涉及文件 (Files) */}
              {selectedNode.files && selectedNode.files.length > 0 && (
                <div className={styles.section}>
                  <div className={styles.sectionTitle}>
                    <span>📄</span>
                    <span>涉及文件 ({selectedNode.files.length})</span>
                  </div>
                  <div className={styles.fileTagsList}>
                    {selectedNode.files.map((file) => (
                      <span key={file} className={styles.fileTag} title={file}>
                        {file}
                      </span>
                    ))}
                  </div>
                </div>
              )}

              {/* 7. 工具执行明细 (Tools Execution) */}
              <div className={styles.section}>
                <div className={styles.sectionTitle}>
                  <span>⚡</span>
                  <span>工具执行明细 ({selectedNode.tools?.length || 0})</span>
                </div>
                {selectedNode.tools && selectedNode.tools.length > 0 ? (
                  <div className={styles.toolsList}>
                    {selectedNode.tools.map((t) => (
                      <div key={t.id} className={styles.toolCard}>
                        <div className={styles.toolCardHeader}>
                          <span
                            className={styles.toolName}
                            style={{ color: t.failed ? 'var(--dsw-alias-state-error-primary, #ec1313)' : undefined }}
                          >
                            {t.name}
                          </span>
                          {t.durationMs !== undefined && (
                            <span className={styles.toolDuration}>{t.durationMs}ms</span>
                          )}
                        </div>
                        {t.input && (
                          <div className={styles.toolParamBlock}>
                            <div className={styles.toolParamLabel}>参数:</div>
                            <div className={styles.toolInput}>{t.input}</div>
                          </div>
                        )}
                        {t.output && (
                          <div className={styles.toolParamBlock}>
                            <div className={styles.toolParamLabel}>结果:</div>
                            <div className={styles.toolOutput}>{t.output}</div>
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className={styles.emptyHint}>本节点暂无独立工具调用</div>
                )}
              </div>

              {/* 8. 中断当前节点 */}
              {running && selectedNode.status === 'running' && selectedNode.id.startsWith('turn_') && onInterruptNode && (
                <div className={styles.actionSection}>
                  <button
                    type="button"
                    className={styles.interruptBtn}
                    onClick={() => onInterruptNode(`turn_${selectedNode.turn}:${selectedNode.id.split(':').slice(1).join(':')}`)}
                  >
                    ⏸ 中断当前节点
                  </button>
                </div>
              )}

            </div>
          </div>
        )}
      </div>
    </div>
  )
}
