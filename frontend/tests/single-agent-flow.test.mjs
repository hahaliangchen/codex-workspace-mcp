import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { flowState } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const event = (type, data) => ({ type, time: 0, data: { turn: 1, ...data } })
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
