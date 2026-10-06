import { memo, useEffect, useMemo, useState } from 'react'
import type { AgentEvent, RequestContext, RequestContextMeta } from '../api.ts'
import { useAgentApi } from '../cordis/react.tsx'
import css from './RequestContextPanel.module.css'
import { foldImageData } from '../visual.ts'

function text(value: unknown): string { return typeof value === 'string' ? value : JSON.stringify(value, null, 2) ?? '' }
function messageLabel(message: Record<string, unknown>): string {
  const content = text(message.content)
  if (content.startsWith('Current work packet')) return '当前工作入参 · 目标、上游结果、完成条件'
  if (content.startsWith('Current task-tree node')) return '当前节点 · 祖先目标与子问题结果'
  if (content.startsWith('Previous unfinished task-tree summary')) return '可恢复的任务树摘要'
  if (content.startsWith('Current task and historical')) return '当前任务与历史摘要'
  if (content.startsWith('Recent work and tool outcomes')) return '工具执行记录摘要'
  if (content.startsWith('Stable Worker work state')) return '工作者持久工作记录'
  if (content.startsWith('Task notebook conclusions')) return '任务记事本：相关结论与当前工作'
  if (content.startsWith('Task notebook material index')) return '任务记事本：材料索引'
  if (content.startsWith('Exact source retrieved')) return '当前节点取回的源码材料'
  if (content.startsWith('Exact source working set')) return '当前节点的源码工作集'
  if (content.startsWith('Recovery from repeated')) return '执行恢复 · 暂停重复汇报与咨询'
  if (content.startsWith('Recorded Observer decisions')) return '已处理的观察者决定'
  if (content.startsWith('Observer advice to read')) return '待处理的观察者建议'
  if (content.startsWith('Already-read Observer advice')) return '已处理的观察者建议'
  if (content.startsWith('Source ranges already returned')) return '已读取源码范围'
  if (content.startsWith('Runtime permissions')) return '权限与执行统计'
  if (content.startsWith('Current live Flow plan')) return '当前任务流与进度'
  return String(message.role ?? 'message') + (message.tool_call_id ? ` · ${String(message.tool_call_id)}` : '')
}
const statuses: Record<string, string> = { pending: '请求中', completed: '已收到完整响应', interrupted: '请求中断或超时', failed: '失败', http_error: 'HTTP 错误', transport_error: '连接失败', stream_error: '流响应失败' }

const JsonDetails = memo(function JsonDetails({ title, value }: { title: string; value: unknown }) {
  const [open, setOpen] = useState(false)
  const content = useMemo(() => open ? text(foldImageData(value)) : '', [open, value])
  return <details onToggle={event => setOpen(event.currentTarget.open)}><summary>{title}</summary>{open && <pre>{content}</pre>}</details>
})

const ContextMessages = memo(function ContextMessages({ messages, added }: { messages: readonly Record<string, unknown>[]; added?: readonly boolean[] | undefined }) {
  return <div className={css.messages}>{messages.map((message, index) => <JsonDetails key={index}
    title={`#${index + 1} · ${messageLabel(message)} · ${text(message).length} 字符${added?.[index] ? ' · 新增/变更' : added ? ' · 与上次相同' : ''}`}
    value={message} />)}</div>
})

/** Full bodies are fetched only after selecting a request. SSE carries IDs only. */
export function RequestContextPanel({ taskId, events, nodeId, turn, workId, requestId, revision, planRevision }: { taskId: string; events: readonly AgentEvent[]; nodeId?: string | undefined; turn?: number | undefined; workId?: string | undefined; requestId?: number | undefined; revision?: number | undefined; planRevision?: number | undefined }) {
  const api = useAgentApi()
  const [items, setItems] = useState<readonly RequestContextMeta[]>([])
  const [hasMore, setHasMore] = useState(false)
  const [selected, setSelected] = useState<number | null>(null)
  const [context, setContext] = useState<RequestContext | null>(null)
  const [previous, setPrevious] = useState<RequestContext | null>(null)
  const [compare, setCompare] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [copied, setCopied] = useState(false)
  const contextEventVersion = [...events].reverse().find(event => event.type.startsWith('debug/context_'))?.seq ?? -1

  useEffect(() => {
    let alive = true
    // Coalesce the start/end notifications without refetching on token deltas.
    const timer = setTimeout(() => {
      void api.requestContexts(taskId).then(result => {
        if (!alive) return
        setItems(current => [...result.items, ...current.filter(item => !result.items.some(newer => newer.id === item.id))].sort((a, b) => b.id - a.id))
        setHasMore(result.has_more)
        setError('')
      }).catch(reason => { if (alive) setError(String(reason)) })
    }, 150)
    return () => { alive = false; clearTimeout(timer) }
  }, [api, taskId, contextEventVersion])

  const visible = useMemo(() => items.filter(item => {
    if (nodeId === undefined) return true
    if (item.request_id !== undefined && requestId !== undefined && item.request_id !== requestId) return false
    if (item.revision !== undefined && revision !== undefined && item.revision !== revision) return false
    if (item.plan_revision !== undefined && planRevision !== undefined && item.plan_revision !== planRevision) return false
    if (item.workId !== undefined && workId !== undefined) return item.workId === workId
    const rawNodeId = nodeId.replace(/^turn_\d+:/, '').replace(/^request_\d+:/, '')
    if (item.request_id !== undefined && requestId !== undefined && item.workId !== undefined) return item.workId === rawNodeId
    if (item.turn !== turn) return false
    const itemWorkId = (item as unknown as { workId?: string }).workId
    return item.nodeId === nodeId || `turn_${item.turn}:${item.nodeId}` === nodeId ||
           item.nodeId === rawNodeId ||
           (Boolean(itemWorkId) && (itemWorkId === nodeId || `turn_${item.turn}:${itemWorkId}` === nodeId || itemWorkId === rawNodeId))
  }), [items, nodeId, turn, workId, requestId, revision, planRevision])
  const active = visible.find(item => item.id === selected)
  const predecessor = active ? items.find(item => item.id < active.id && item.actor === active.actor) : undefined

  useEffect(() => {
    setSelected(null); setContext(null); setPrevious(null); setCompare(false)
  }, [taskId, nodeId, turn])

  useEffect(() => {
    if (!active) return
    let alive = true
    setContext(null); setPrevious(null); setLoading(true); setError(''); setCopied(false)
    void api.requestContext(taskId, active.id).then(result => { if (alive) setContext(result) })
      .catch(reason => { if (alive) setError(String(reason)) })
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
  }, [api, taskId, active?.id, active?.status])

  useEffect(() => {
    setPrevious(null)
    if (!compare || !predecessor) return
    let alive = true
    void api.requestContext(taskId, predecessor.id).then(result => { if (alive) setPrevious(result) })
      .catch(reason => { if (alive) setError(String(reason)) })
    return () => { alive = false }
  }, [api, taskId, compare, predecessor?.id])

  const changes = useMemo(() => {
    if (!context || !previous) return null
    const old = new Map<string, number>()
    for (const message of previous.body.messages) { const key = JSON.stringify(message); old.set(key, (old.get(key) ?? 0) + 1) }
    const added = context.body.messages.map(message => { const key = JSON.stringify(message); const count = old.get(key) ?? 0; if (count) old.set(key, count - 1); return count === 0 })
    const removed = [...old.values()].reduce((sum, count) => sum + count, 0)
    return { added, removed }
  }, [context, previous])

  async function more() {
    const oldest = items.at(-1)
    if (!oldest) return
    setLoading(true)
    try {
      const result = await api.requestContexts(taskId, oldest.id)
      setItems(current => [...current, ...result.items.filter(item => !current.some(existing => existing.id === item.id))])
      setHasMore(result.has_more)
    } catch (reason) { setError(String(reason)) } finally { setLoading(false) }
  }

  return <section className={css.panel}>
    <h3>实际模型请求上下文</h3>
    <p className={css.hint}>开启调试后，从下一次请求开始记录。这里按请求发起时的节点归属展示；规划前请求及复盘可在「全部请求」查看。字节和字符数不是 token 数。</p>
    {error && <p role="alert" className={css.error}>{error}</p>}
    <select aria-label="选择模型请求" value={selected ?? ''} onChange={event => { setSelected(Number(event.target.value) || null); setCompare(false) }}>
      <option value="">选择请求（{visible.length} 条）</option>
      {visible.map(item => {
        const rev = (item as unknown as { revision?: number }).revision
        const revLabel = rev && rev > 1 ? ` (r${rev})` : ''
        const displayId = (item as unknown as { workId?: string }).workId || item.nodeId || '未分配节点'
        return <option key={item.id} value={item.id}>{item.actor === 'worker' ? 'Worker' : item.actor === 'organizer' ? 'Organizer' : 'Observer'} · 第 {item.turn} 轮 / 步骤 {item.step} · {item.stage} · {displayId}{revLabel}{item.review_id ? ` · 复盘 ${item.review_id}` : ''} · {statuses[item.status] ?? item.status}</option>
      })}
    </select>
    {!visible.length && <p className={css.hint}>尚无已记录的请求。旧请求无法补回；节点切换后下一次请求才使用新节点上下文。</p>}
    {hasMore && <button type="button" disabled={loading} onClick={() => { void more() }}>加载更早请求</button>}
    {loading && <p>正在读取…</p>}
    {context && active && <>
      <div className={css.stats}>
        <span>{active.model}</span><span>{active.message_count} 条消息</span><span>{(active.request_bytes / 1024).toFixed(1)} KiB</span>
        <span>{statuses[active.status] ?? active.status}{active.elapsed_ms === undefined ? '' : ` · ${(active.elapsed_ms / 1000).toFixed(1)} 秒`}</span>
        <span>{active.tools.length} 个可用工具</span>
      </div>
      <p className={css.hint}>耗时包括调试存储、连接及完整模型响应，不包括后续工具执行。请求正文未再次裁剪，不包含鉴权请求头。</p>
      {active.compaction?.request_sizes && <p className={css.hint}>
        消息正文 {active.compaction.request_sizes.message_chars.toLocaleString()} 字符
        {' · '}其中系统消息 {active.compaction.request_sizes.system_chars.toLocaleString()}
        {' · '}工作记录 {active.compaction.request_sizes.work_state_chars.toLocaleString()}
        {' · '}工具定义另计 {active.compaction.request_sizes.tool_schema_chars.toLocaleString()} 字符（非 token 数）
      </p>}
      {active.compaction?.work_projection && <p className={css.hint}>结论按本轮任务与相关文件筛选；完整历史仍保存在记事本。</p>}
      {active.work && <p>此前工具调用 {active.work.tool_calls} 次 · 写入成功 {active.work.successful_file_writes} 次 · 重复读取 {active.work.repeated_read_calls} 次</p>}
      {active.permission_mode && <p>本轮权限：{active.permission_mode}{active.final_step ? ' · 步数上限已到，工具因预算被移除' : ''}</p>}
      {active.compaction && <JsonDetails title={`历史裁剪：移除 ${active.compaction.omitted_raw_message_count} 条原始消息，截短 ${active.compaction.shortened_messages.length} 条`} value={active.compaction} />}
      {predecessor && <label className={css.compare}><input type="checkbox" checked={compare} onChange={event => setCompare(event.target.checked)} />与上一条同角色请求比较（轮 {predecessor.turn} / 步骤 {predecessor.step}）</label>}
      {changes && <p>新增或变更消息 {changes.added.filter(Boolean).length} 条 · 上一请求中移除或被替换的消息 {changes.removed} 条。按完整消息匹配，摘要更新也算变更。</p>}
      <div className={css.actions}>
        <button type="button" onClick={() => { void navigator.clipboard.writeText(JSON.stringify(context, null, 2)).then(() => setCopied(true)).catch(reason => setError(String(reason))) }}>{copied ? '已复制' : '复制请求及调试信息'}</button>
      </div>
      <ContextMessages key={active.id} messages={context.body.messages} added={changes?.added} />
      <JsonDetails title="实际图片派发 · 角色、路由、尺寸、哈希、原图／缩放／降级" value={(context.metadata as unknown as Record<string, unknown>).visual_dispatch} />
      <JsonDetails title="完整请求 JSON（含工具定义）" value={context.body} />
      <JsonDetails title="本次请求结果摘要（连接、首个输出、流响应耗时及可用 usage）" value={context.metadata.outcome} />
      {previous && <JsonDetails title="上一请求的完整上下文" value={previous.body} />}
    </>}
  </section>
}
