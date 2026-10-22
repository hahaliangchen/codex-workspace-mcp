import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { flowState } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const event = (type, data) => ({ type, time: 0, data: { turn: 1, ...data } })

test('task nodes persist across turns and finish only on an explicit return', () => {
  const state = flowState([
    event('task/node_planned', { node_id: 'n0', kind: 'root', executor: 'inline', status: 'running', title: 'Twenty pages' }),
    event('task/node_planned', { node_id: 'page7', parent_id: 'n0', kind: 'planned', executor: 'inline', status: 'pending', title: 'Page 7' }),
    event('task/node_suspended', { node_id: 'n0', kind: 'root', executor: 'inline', status: 'suspended' }),
    event('task/node_started', { node_id: 'page7', parent_id: 'n0', kind: 'planned', executor: 'inline', status: 'running' }),
    event('flow/node_add', { node: { id: 'request7', kind: 'agent', task_node_id: 'page7', status: 'running' } }),
    event('tool/call', { flowNodeId: 'request7', callId: 'check7', name: 'run_program', arguments: '{}' }),
    event('tool/result', { flowNodeId: 'request7', message: { role: 'tool', toolCallId: 'check7', content: [{ type: 'text', text: 'missing class' }] } }),
    event('turn/end', { reason: { kind: 'completed' } }),
    event('turn/start', { turn: 2 }),
    event('task/node_planned', { turn: 2, node_id: 'repair', parent_id: 'page7', kind: 'branch', executor: 'inline', status: 'running', title: 'Repair CSS' }),
    event('task/node_suspended', { turn: 2, node_id: 'page7', parent_id: 'n0', kind: 'planned', executor: 'inline', status: 'suspended' }),
    event('task/node_returned', { turn: 2, node_id: 'repair', parent_id: 'page7', kind: 'branch', executor: 'inline', status: 'done', result: { result: 'CSS verified' } }),
    event('task/node_resumed', { turn: 2, node_id: 'page7', parent_id: 'n0', kind: 'planned', executor: 'inline', status: 'running' }),
  ])
  assert.equal(state.nodes.filter(n => n.id.startsWith('task:')).length, 3)
  assert.equal(state.nodes.find(n => n.id === 'task:n0').status, 'suspended')
  const page = state.nodes.find(n => n.id === 'task:page7')
  assert.equal(page.status, 'running')
  assert.equal(page.tools[0].output, 'missing class')
  assert.equal(state.nodes.find(n => n.id === 'task:repair').description, 'CSS verified')
  assert.equal(state.activeNodeId, 'task:page7')
  assert.ok(state.edges.some(e => e.source === 'task:page7' && e.target === 'task:repair'))
})
test('each model request retains parent, tools and outcome after reload', () => {
  const events = [
    event('turn/start', { runtime: 'single_agent' }),
    event('flow/node_add', { node: { id: 'main', kind: 'agent', status: 'running', request_id: 1 } }),
    event('flow/node_state', { id: 'main', status: 'done' }),
    event('flow/node_add', { node: { id: 'child', kind: 'context_agent', status: 'running', parent_id: 'main', request_id: 1 }, edges: [{ source: 'main', target: 'child' }] }),
    event('context/tool_call', { flowNodeId: 'child', callId: 'read', name: 'read_context_material', arguments: '{}' }),
    event('context/tool_result', { message: { role: 'tool', toolCallId: 'read', content: [{ type: 'text', text: '完整原文' }] } }),
    event('flow/node_state', { id: 'child', status: 'done' }),
    event('turn/end', { reason: { kind: 'completed' } }),
  ]
  const state = flowState(JSON.parse(JSON.stringify(events)))
  assert.equal(state.nodes.length, 2)
  const child = state.nodes.find(n => n.id === 'turn_1:child')
  assert.equal(child.parentId, 'turn_1:main')
  assert.equal(child.kind, 'context_agent')
  assert.equal(child.status, 'done')
  assert.equal(child.tools[0].output, '完整原文')
  assert.ok(state.edges.some(e => e.source === 'turn_1:main' && e.target === 'turn_1:child'))
})

test('a new turn keeps prior request outcomes and cancellation closes only the current requests', () => {
  const events = [
    event('turn/start', {}),
    event('flow/node_add', { node: { id: 'main', kind: 'agent', status: 'running', request_id: 1 } }),
    event('flow/node_state', { id: 'main', status: 'failed' }),
    event('turn/end', { reason: { kind: 'error' } }),
    event('turn/start', { turn: 2 }),
    event('flow/node_add', { turn: 2, node: { id: 'main', kind: 'agent', status: 'running', request_id: 2 } }),
    event('flow/node_add', { turn: 2, node: { id: 'child', kind: 'context_agent', status: 'running', parent_id: 'main', request_id: 2 } }),
    event('turn/end', { turn: 2, reason: { kind: 'aborted' } }),
  ]
  const state = flowState(JSON.parse(JSON.stringify(events)))
  assert.equal(state.nodes.length, 3)
  assert.equal(state.nodes.find(n => n.id === 'turn_1:main').status, 'failed')
  assert.equal(state.nodes.find(n => n.id === 'turn_2:main').status, 'interrupted')
  assert.equal(state.nodes.find(n => n.id === 'turn_2:child').parentId, 'turn_2:main')
  assert.equal(state.nodes.find(n => n.id === 'turn_2:child').status, 'interrupted')
  assert.equal(state.activeNodeId, undefined)
})
