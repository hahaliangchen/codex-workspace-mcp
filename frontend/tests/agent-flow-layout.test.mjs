import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

async function load(path) {
  const source = await readFile(new URL(path, import.meta.url), 'utf8')
  const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
  return import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
}
const { flowState } = await load('../src/model.ts')
const { layoutAgentFlow } = await load('../src/flowLayout.ts')
const event = (type, data) => ({ type, time: 0, data: { turn: 1, ...data } })
const node = (id, kind, parent, previous, agent_id) => event('flow/node_add', {
  node: { id, kind, parent_id: parent, agent_id, title: `${kind} · ${id}`, status: 'running' },
  edges: previous ? [{ source: previous, target: id, label: 'next request' }]
    : parent ? [{ source: parent, target: id, label: 'context subagent' }] : [],
})
const start = (id, name, parent) => event('subagent/start', { agent_id: id, name, parent_node_id: parent })
const pos = (positions, id) => positions.get(`turn_1:${id}`)
const layout = events => {
  const state = flowState(events)
  return { state, positions: layoutAgentFlow(state.nodes, state.edges) }
}

test('finish follows all host steps even when it is created first, including serial children', () => {
  const task = (id, parent, kind, ordinal) => event('task/node_planned', { node_id: id, parent_id: parent, kind, ordinal,
    executor: 'inline', status: 'pending' })
  const { positions } = layout([
    task('main', undefined, 'task', 0), task('finish', 'main', 'finish', 10000),
    task('step2', 'main', 'step', 1), task('step1', 'main', 'step', 0),
    task('repair', 'step1', 'task', 0), task('child-finish', 'repair', 'finish', 10000),
    task('child-step', 'repair', 'step', 0),
  ])
  assert.ok(positions.get('task:step1').x < positions.get('task:step2').x)
  assert.ok(positions.get('task:step2').x < positions.get('task:finish').x)
  assert.ok(positions.get('task:child-step').x < positions.get('task:child-finish').x)
  assert.ok(positions.get('task:repair').y > positions.get('task:step1').y)
})

test('old PPT records recover named independent branches and distinguish compression', () => {
  const { state, positions } = layout([
    node('m1', 'agent'), start('a', 'pages-1-2', 'm1'), node('a1', 'worker', 'm1'),
    node('m2', 'agent', undefined, 'm1'), start('b', 'pages-3-5', 'm2'), node('b1', 'worker', 'm2'),
    node('b2', 'worker', 'm2', 'b1'), node('a2', 'worker', 'm1', 'a1'),
    node('m3', 'agent', undefined, 'm2'), node('compact', 'context_agent', 'm3'),
  ])
  assert.equal(state.nodes.find(n => n.id === 'turn_1:a2').agentName, 'pages-1-2')
  assert.equal(state.nodes.find(n => n.id === 'turn_1:b2').agentName, 'pages-3-5')
  assert.equal(state.edges.find(e => e.target === 'turn_1:a1').label, '派发 · pages-1-2')
  assert.equal(state.edges.find(e => e.target === 'turn_1:compact').label, '上下文压缩')
  assert.equal(pos(positions, 'a1').x, pos(positions, 'a2').x)
  assert.equal(pos(positions, 'b1').x, pos(positions, 'b2').x)
  assert.ok(pos(positions, 'm1').x < pos(positions, 'a1').x)
  assert.ok(pos(positions, 'a1').x < pos(positions, 'b1').x)
  assert.ok(pos(positions, 'compact').x < pos(positions, 'm1').x)
  for (const edge of state.edges) assert.ok(positions.get(edge.target).y > positions.get(edge.source).y)
})

test('delayed first request, reverse completion and later repair never move earlier nodes', () => {
  const events = [node('main', 'agent'), start('a', 'First dispatched', 'main'), start('b', 'Second dispatched', 'main'),
    node('b1', 'worker', 'main', undefined, 'b')]
  const before = layout(events).positions
  events.push(node('a1', 'worker', 'main', undefined, 'a'), node('a2', 'worker', 'main', 'a1', 'a'),
    event('subagent/end', { agent_id: 'b', status: 'completed' }),
    node('main2', 'agent', undefined, 'main'), node('compact', 'context_agent', 'main2'),
    start('repair', 'Repair', 'main2'), node('repair1', 'worker', 'main2', undefined, 'repair'))
  const after = layout(JSON.parse(JSON.stringify(events))).positions
  for (const [id, position] of before) assert.deepEqual(after.get(id), position)
  assert.ok(pos(after, 'a1').x < pos(after, 'b1').x, 'dispatch order wins over first-request arrival')
  assert.ok(pos(after, 'b1').x < pos(after, 'repair1').x, 'completed columns are not reused')
})

test('compression of a task has its own column while the task continues in place', () => {
  const events = [node('m', 'agent'), start('a', 'A', 'm'), node('a1', 'worker', 'm'), node('c1', 'context_agent', 'a1'),
    node('c2', 'context_agent', 'a1', 'c1'), node('a2', 'worker', 'm', 'a1')]
  const { positions } = layout(events)
  assert.equal(pos(positions, 'a1').x, pos(positions, 'a2').x)
  assert.equal(pos(positions, 'c1').x, pos(positions, 'c2').x)
  assert.notEqual(pos(positions, 'a1').x, pos(positions, 'c1').x)
})

test('disconnected main roots and malformed legacy cycles keep unique finite positions', () => {
  const state = flowState([node('one', 'agent'), node('two', 'agent'), node('x', 'worker', 'one', 'y'), node('y', 'worker', 'one', 'x')])
  const positions = layoutAgentFlow(state.nodes, [...state.edges, { id: 'missing', source: 'missing', target: 'turn_1:one' }])
  assert.equal(positions.size, 4)
  assert.equal(new Set([...positions.values()].map(p => `${p.x},${p.y}`)).size, 4)
  assert.ok([...positions.values()].every(p => Number.isFinite(p.x) && Number.isFinite(p.y)))
  assert.equal(pos(positions, 'one').x, pos(positions, 'two').x)
})
