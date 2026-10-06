import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

// Exercise the actual event reducer without introducing another test runtime.
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 },
})
const { flowState } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))

const event = (type, turn, data = {}) => ({ type, time: 0, data: { turn, ...data } })
const priorGoal = (turn, status) => [
  event('turn/start', turn),
  event('flow/plan', turn, { mode: 'tree', nodes: [
    { id: 'goal', objective: `Goal ${turn}`, status },
    { id: 'task_a', node_id: 'work_task_a', parent_id: 'goal', status },
  ], edges: [{ id: 'contains_goal_task_a', source: 'goal', target: 'task_a' }] }),
]
const history = [
  ...priorGoal(1, 'completed'),
  ...priorGoal(2, 'blocked'),
  event('turn/start', 3),
  event('flow/request_archived', 3, { started_turn: 2, plan_revision: 2, node_ids: ['goal', 'task_a'] }),
]
const archivedNodes = [
  { id: 'request_1:goal', objective: 'Cancelled goal', status: 'deprecated', invalidated_by_plan_revision: 2 },
  { id: 'request_1:task_a', node_id: 'request_1:work_task_a', parent_id: 'request_1:goal', status: 'deprecated', invalidated_by_plan_revision: 2 },
]
const archivedEdges = [{ id: 'request_1:edge', source: 'request_1:goal', target: 'request_1:task_a', deprecated: true }]
const scheduler = event('scheduler/state', 3, { state: { current: 'task_a', frames: {
  task_a: { status: 'running', sequence: 1, invalidated_by_plan_revision: null,
    order: { id: 'task_a', node_id: 'work_task_a', goal: 'New work', revision: 1, plan_revision: 2 } },
} } })

test('replacement greys only the cancelled request and keeps a reused work ID active', () => {
  const state = flowState([...history,
    event('flow/plan', 3, { mode: 'direct', replaceCurrentTurn: true,
      nodes: [{ id: 'task_a', node_id: 'work_task_a', objective: 'New answer', status: 'running' }, ...archivedNodes],
      edges: archivedEdges, active_node_id: 'task_a' }),
    scheduler,
  ])
  const node = id => state.nodes.find(node => node.id === id)
  assert.equal(node('turn_1:goal').status, 'completed')
  assert.equal(node('turn_1:task_a').status, 'completed')
  assert.equal(node('turn_2:goal').status, 'deprecated')
  assert.equal(node('turn_2:task_a').status, 'deprecated')
  assert.equal(node('turn_3:task_a').status, 'running')
  assert.equal(node('turn_3:task_a').invalidatedByPlanRevision, undefined)
  assert.equal(node('turn_3:task_a').parentId, undefined)
  assert.equal(node('turn_3:request_1:task_a').status, 'deprecated')
  assert.equal(node('turn_3:request_1:task_a').parentId, 'turn_3:request_1:goal')
  assert.equal(state.activeNodeId, 'turn_3:task_a')
  assert.equal(state.organizedMode, 'direct')
  assert.equal(state.edges.find(edge => edge.id === 'turn_2:contains_goal_task_a').deprecated, true)
})

test('archived roots do not become parents or block completion of the new complex goal', () => {
  const state = flowState([...history,
    event('flow/plan', 3, { mode: 'tree', replaceCurrentTurn: true, nodes: [
      { id: 'goal', objective: 'New export goal', status: 'waiting_children' },
      { id: 'task_a', node_id: 'work_task_a', parent_id: 'goal', status: 'running' }, ...archivedNodes,
    ], edges: [{ id: 'new_edge', source: 'goal', target: 'task_a' }, ...archivedEdges] }),
    scheduler,
    event('flow/tree_state', 3, { state: { nodes: {
      goal: { id: 'goal', objective: 'New export goal', status: 'completed' },
      work_task_a: { id: 'work_task_a', parent_id: 'goal', status: 'completed' },
    } }, active_node_id: '', active_path: [] }),
    event('turn/end', 3, { reason: { kind: 'completed' } }),
  ])
  assert.equal(state.nodes.find(node => node.id === 'turn_3:goal').objective, 'New export goal')
  assert.equal(state.nodes.find(node => node.id === 'turn_3:task_a').parentId, 'turn_3:goal')
  assert.equal(state.nodes.find(node => node.id === 'turn_3:request_1:task_a').status, 'deprecated')
  assert.equal(state.unfinishedTree, false)
  assert.equal(state.activeNodeId, undefined)
})
