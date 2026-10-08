import type { AgentEvent } from './api.ts'
import type { FlowNodeData } from './model.ts'

export type VisualRecord = Record<string, unknown>
export function visualMatchesNode(identity: VisualRecord, node: FlowNodeData): boolean {
  const raw = node.id.replace(/^turn_\d+:/, '').replace(/^request_\d+:/, '')
  return identity.work_id === (node.workUnit?.id ?? raw) &&
    identity.request_id === node.requestId && identity.revision === node.revision && identity.plan_revision === node.planRevision
}
export function visualRecords(events: readonly AgentEvent[], node?: FlowNodeData): { artifacts: VisualRecord[]; checks: VisualRecord[]; dispatches: VisualRecord[] } {
  const artifacts = new Map<string, VisualRecord>()
  const checks: VisualRecord[] = []
  const dispatches: VisualRecord[] = []
  const add = (value: unknown, consumedHere = false) => {
    if (typeof value !== 'object' || value === null) return
    const item = value as VisualRecord
    if (typeof item.artifact_id !== 'string') return
    if (node && !consumedHere && !visualMatchesNode((item.identity ?? {}) as VisualRecord, node)) return
    artifacts.set(item.artifact_id, item)
  }
  for (const event of events) {
    const data = event.data as VisualRecord
    if (event.type === 'tool/result') add(((data.meta as VisualRecord | undefined)?.result as VisualRecord | undefined)?.visual_artifact)
    if (event.type === 'observer/node_review') {
      for (const artifact of (((data.result as VisualRecord | undefined)?.visual_artifacts ?? []) as unknown[])) add(artifact)
    }
    if (event.type === 'visual/model_received' || event.type === 'visual/service_result' || event.type === 'visual/model_failed') {
      const original = data.manifest as VisualRecord | undefined
      const manifest = event.type === 'visual/model_failed' && original
        ? { ...original, dispatch_status: original.status, status: 'request_failed', error: data.error } : original
      if (!manifest || (node && !visualMatchesNode((manifest.identity ?? {}) as VisualRecord, node))) continue
      dispatches.push(manifest)
      for (const artifact of [...(manifest.images ?? []) as unknown[], ...(manifest.selected_images ?? []) as unknown[]]) add(artifact, true)
    }
    if (event.type === 'visual/check_result') {
      const check = data.result as VisualRecord | undefined
      if (!check || (node && !visualMatchesNode((check.identity ?? {}) as VisualRecord, node))) continue
      checks.push(check)
      for (const artifact of (check.artifacts ?? []) as unknown[]) add(artifact, true)
    }
  }
  return { artifacts: [...artifacts.values()], checks, dispatches }
}
/** The full request remains available through explicit copy/export. */
export function foldImageData(value: unknown): unknown {
  if (typeof value === 'string' && value.startsWith('data:image/')) return `[图片数据已折叠，${value.length} 字符；实际发送体可复制核对]`
  if (Array.isArray(value)) return value.map(foldImageData)
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, foldImageData(item)]))
  return value
}
