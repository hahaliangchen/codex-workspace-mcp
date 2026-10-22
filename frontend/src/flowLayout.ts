import type { FlowEdgeData, FlowNodeData } from './model.ts'

/** Keep each execution window in a fixed column, in dispatch order.
 * Context-compression branches live to the left, task branches to the right.
 * New requests and completions never move an existing branch sideways.
 */
export function layoutAgentFlow(nodes: readonly FlowNodeData[], edges: readonly FlowEdgeData[]) {
  if (nodes.length > 0 && nodes.every(node => node.id.startsWith('task:'))) {
    const positions = new Map<string, { x: number; y: number }>()
    const children = new Map<string, FlowNodeData[]>()
    for (const node of nodes) if (node.parentId) children.set(node.parentId, [...children.get(node.parentId) ?? [], node])
    let column = 0
    const visiting = new Set<string>()
    const place = (node: FlowNodeData, depth: number): number => {
      if (visiting.has(node.id)) return column++
      visiting.add(node.id)
      const columns = (children.get(node.id) ?? []).map(child => place(child, depth + 1))
      const x = columns.length ? (columns[0]! + columns.at(-1)!) / 2 : column++
      positions.set(node.id, { x: 60 + x * 380, y: 50 + depth * 250 })
      return x
    }
    for (const root of nodes.filter(node => !node.parentId || !nodes.some(parent => parent.id === node.parentId))) place(root, 0)
    for (const node of nodes) if (!positions.has(node.id)) place(node, 0)
    return positions
  }
  const byId = new Map(nodes.map(node => [node.id, node]))
  const order = new Map(nodes.map((node, index) => [node.id, index]))
  const incoming = new Map<string, string[]>()
  const outgoing = new Map<string, string[]>()
  const indegree = new Map(nodes.map(node => [node.id, 0]))
  for (const edge of edges) {
    if (!byId.has(edge.source) || !byId.has(edge.target)) continue
    incoming.set(edge.target, [...incoming.get(edge.target) ?? [], edge.source])
    outgoing.set(edge.source, [...outgoing.get(edge.source) ?? [], edge.target])
    indegree.set(edge.target, (indegree.get(edge.target) ?? 0) + 1)
  }
  const branches = new Map<string, string>()
  const columns = new Map<string, number>([['main', 0]])
  let nextTaskColumn = Math.max(0, ...nodes.map(node => (node.agentOrder ?? -1) + 1)) + 1
  let nextContextColumn = -1
  const positions = new Map<string, { x: number; y: number }>()
  const lastRow = new Map<number, number>()

  function place(node: FlowNodeData) {
    const previous = (incoming.get(node.id) ?? []).find(id => {
      const source = byId.get(id)
      return source?.kind === node.kind && source.parentId === node.parentId
    })
    const branch = node.kind === 'agent' ? 'main'
      : node.agentId ? `delegate:${node.agentId}`
        : (previous && branches.get(previous))
          || (node.kind === 'worker' ? `worker:${node.parentId ?? node.id}` : `${node.kind}:${node.id}`)
    branches.set(node.id, branch)
    if (!columns.has(branch)) {
      columns.set(branch, node.kind === 'worker'
        ? (node.agentOrder === undefined ? nextTaskColumn++ : node.agentOrder + 1)
        : nextContextColumn--)
    }
    const column = columns.get(branch) ?? 0
    const parentRows = (incoming.get(node.id) ?? []).map(id => {
      const position = positions.get(id)
      return position ? (position.y - 50) / 250 : -1
    })
    const row = Math.max(lastRow.get(column) ?? -1, ...parentRows) + 1
    lastRow.set(column, row)
    positions.set(node.id, { x: 60 + column * 380, y: 50 + row * 250 })
  }

  const queue = nodes.filter(node => indegree.get(node.id) === 0).map(node => node.id)
  while (queue.length) {
    // Prefer original event order even when multiple roots are ready.
    queue.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0))
    const id = queue.shift()!
    place(byId.get(id)!)
    for (const target of outgoing.get(id) ?? []) {
      const count = (indegree.get(target) ?? 1) - 1
      indegree.set(target, count)
      if (count === 0) queue.push(target)
    }
  }
  // Retain malformed/cyclic legacy records without overlapping cards.
  for (const node of nodes) if (!positions.has(node.id)) place(node)
  return positions
}
