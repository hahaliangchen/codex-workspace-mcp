import { useMemo } from 'react'
import type { AgentEvent } from '../api.ts'
import type { FlowNodeData } from '../model.ts'
import { visualRecords, type VisualRecord } from '../visual.ts'

export function VisualArtifactPreview({ artifact }: { artifact: VisualRecord }) {
  const task = String(artifact.task_id ?? '')
  const id = String(artifact.artifact_id ?? '')
  if (!task || !id) return null
  const url = `/agent/tasks/${encodeURIComponent(task)}/visual-artifacts/${encodeURIComponent(id)}/image`
  return <figure style={{ margin: '8px 0' }}>
    <a href={url} target="_blank" rel="noreferrer"><img src={url} alt={`截图 ${id}`} loading="lazy" style={{ maxWidth: '100%', maxHeight: 240, objectFit: 'contain', borderRadius: 6 }} /></a>
    <figcaption style={{ fontSize: 12, overflowWrap: 'anywhere' }}>{String(artifact.url ?? '')}<br />
      {String(artifact.width)} × {String(artifact.height)} · {Number(artifact.byte_size ?? 0).toLocaleString()} B · {new Date(Number(artifact.captured_at ?? 0)).toLocaleString()} · 页面版本 {String(artifact.page_epoch ?? '?')}<br />
      {id} · SHA256 {String(artifact.content_hash ?? '')}
    </figcaption>
  </figure>
}
export function VisualMaterials({ events, node }: { events: readonly AgentEvent[]; node: FlowNodeData }) {
  const records = useMemo(() => visualRecords(events, node), [events, node])
  if (!records.artifacts.length && !records.dispatches.length && !records.checks.length) return null
  return <section><h4>视觉材料与判断</h4>
    {records.artifacts.map(artifact => <VisualArtifactPreview key={String(artifact.artifact_id)} artifact={artifact} />)}
    {records.checks.map((check, index) => <details key={String(check.check_id ?? index)}><summary>{String(check.actor)} · {String(check.assessment)} · {String(check.checked_goal ?? '')}</summary><pre style={{ whiteSpace: 'pre-wrap' }}>{JSON.stringify(check, null, 2)}</pre></details>)}
    {records.dispatches.map((dispatch, index) => <details key={index}><summary>图片请求 · {String(dispatch.actor)} · {String(dispatch.status)} · {JSON.stringify(dispatch.model_route)}</summary><pre style={{ whiteSpace: 'pre-wrap' }}>{JSON.stringify(dispatch, null, 2)}</pre></details>)}
  </section>
}
