export interface CodeFeature { id: string; parent: string | null; title: string; summary: string; node_count: number }
export interface CodeEvidence { file: string; line: number; snippet: string; code_hash: string }
export interface CodeNode {
  id: string; label: string; file: string; name: string; role: string; responsibility: string; inputs: string; outputs: string
  status: string; symbol_id?: string; signature?: string
  source?: { file: string; line: number; end_line: number; code_hash: string }
  test_entries?: { filter: string; argv: string[]; status: string; source_matches: unknown[] }[]
}
export interface CodeEdge { id: string; source: string; target: string; evidence: CodeEvidence }
export interface BoundaryCall { source: string; target_text: string; qualifier: string | null; resolution: string; evidence: CodeEvidence }
export interface ExternalCaller { target: string; symbol_id: string; name: string; file: string; line: number; evidence: CodeEvidence }
export interface CodeGraph {
  scope?: string
  workspace_root: string; manifest: string; manifest_hash: string; features: CodeFeature[]
  feature?: { id: string; title: string; summary: string; parent: string | null }
  nodes?: CodeNode[]; edges?: CodeEdge[]; boundary_calls?: BoundaryCall[]; external_callers?: ExternalCaller[]
  source_content?: string; selected_node?: CodeNode
}

/** Collapse cycles for layout only. Preserve every original node and edge. */
export function layoutCodeFlow(ids: readonly string[], edges: readonly { source: string; target: string }[]) {
  const included = new Set(ids)
  const children = new Map(ids.map(id => [id, new Set<string>()]))
  for (const edge of edges) if (included.has(edge.source) && included.has(edge.target)) children.get(edge.source)?.add(edge.target)
  const index = new Map<string, number>(), low = new Map<string, number>()
  const stack: string[] = [], onStack = new Set<string>(), components: string[][] = []
  let next = 0
  function visit(id: string) {
    index.set(id, next); low.set(id, next++); stack.push(id); onStack.add(id)
    for (const child of children.get(id) ?? []) {
      if (!index.has(child)) { visit(child); low.set(id, Math.min(low.get(id)!, low.get(child)!)) }
      else if (onStack.has(child)) low.set(id, Math.min(low.get(id)!, index.get(child)!))
    }
    if (low.get(id) === index.get(id)) {
      const component: string[] = []
      let member: string
      do { member = stack.pop()!; onStack.delete(member); component.push(member) } while (member !== id)
      components.push(component)
    }
  }
  for (const id of ids) if (!index.has(id)) visit(id)
  const owner = new Map<string, number>()
  components.forEach((members, i) => members.forEach(id => owner.set(id, i)))
  const links = components.map(() => new Set<number>()), indegree = components.map(() => 0), depth = components.map(() => 0)
  for (const edge of edges) {
    const a = owner.get(edge.source), b = owner.get(edge.target)
    if (a !== undefined && b !== undefined && a !== b && !links[a]!.has(b)) { links[a]!.add(b); indegree[b]!++ }
  }
  const queue = components.map((_, i) => i).filter(i => indegree[i] === 0)
  for (let cursor = 0; cursor < queue.length; cursor++) {
    const a = queue[cursor]!
    for (const b of links[a]!) { depth[b] = Math.max(depth[b]!, depth[a]! + 1); if (--indegree[b]! === 0) queue.push(b) }
  }
  const rows = new Map<number, string[]>()
  for (const id of ids) { const level = depth[owner.get(id)!]!; const row = rows.get(level) ?? []; row.push(id); rows.set(level, row) }
  const widest = Math.max(1, ...[...rows.values()].map(row => row.length))
  const positions = new Map<string, { x: number; y: number }>()
  for (const [level, row] of rows) row.forEach((id, i) => positions.set(id, { x: 40 + ((widest - row.length) / 2 + i) * 310, y: 50 + level * 190 }))
  return positions
}
